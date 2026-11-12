use std::time::Duration;

use sqlx::{PgPool, postgres::PgPoolOptions};

use crate::{auth, config::Config, util::py_float};

const SCHEMA: &str = include_str!("../migrations/001_schema.sql");
const INDEXES: &str = include_str!("../migrations/002_indexes.sql");
const SEED_PRODUCTS: &str = include_str!("../seed/products.csv");

/// Connect with retries so the service also survives starting before Postgres is ready.
pub async fn connect(cfg: &Config) -> Result<PgPool, sqlx::Error> {
    let mut last = None;
    for attempt in 1..=40 {
        let res = PgPoolOptions::new()
            .max_connections(cfg.db_max_connections)
            .acquire_timeout(Duration::from_secs(15))
            .after_connect(|conn, _| {
                Box::pin(async move {
                    // Every statement gets a ceiling so one bad query cannot hold a connection forever.
                    sqlx::query("SET statement_timeout = '30s'").execute(&mut *conn).await?;
                    Ok(())
                })
            })
            .connect(&cfg.database_url)
            .await;
        match res {
            Ok(p) => return Ok(p),
            Err(e) => {
                tracing::warn!(attempt, error = %e, "database not ready, retrying");
                last = Some(e);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    Err(last.expect("at least one attempt"))
}

/// Create/upgrade tables. Safe to run repeatedly and concurrently (advisory lock).
pub async fn init_db(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut conn = pool.acquire().await?;
    sqlx::query("SELECT pg_advisory_lock(727274)")
        .execute(&mut *conn)
        .await?;
    let res = run_migrations(&mut conn).await;
    sqlx::query("SELECT pg_advisory_unlock(727274)")
        .execute(&mut *conn)
        .await?;
    res
}

async fn run_migrations(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    // Required part: a failure here is fatal.
    sqlx::raw_sql(SCHEMA).execute(&mut *conn).await?;
    // Optional part: indexes/unique constraints, one statement at a time.
    for stmt in INDEXES.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let stmt: String = stmt
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        if stmt.trim().is_empty() {
            continue;
        }
        if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(stmt.clone())).execute(&mut *conn).await {
            tracing::warn!(statement = %stmt, error = %e, "optional index/constraint skipped");
        }
    }
    Ok(())
}

pub async fn seed(pool: &PgPool, cfg: &Config) -> Result<(), sqlx::Error> {
    let admin: Option<(i32,)> = sqlx::query_as("SELECT id FROM users WHERE username = $1")
        .bind(&cfg.admin_user)
        .fetch_optional(pool)
        .await?;
    if admin.is_none() {
        let pw = cfg.admin_password.clone();
        let hash = tokio::task::spawn_blocking(move || auth::hash_password(&pw))
            .await
            .unwrap_or_default();
        sqlx::query(
            "INSERT INTO users (username, email, name, password_hash, role, wallet_balance) \
             VALUES ($1, 'admin@shopcore.local', 'Administrator', $2, 'admin', 0) ON CONFLICT DO NOTHING",
        )
        .bind(&cfg.admin_user)
        .bind(hash)
        .execute(pool)
        .await?;
    }
    let any: Option<(i32,)> = sqlx::query_as("SELECT id FROM coupons LIMIT 1")
        .fetch_optional(pool)
        .await?;
    if any.is_none() {
        sqlx::query(
            "INSERT INTO coupons (code, percent, expires, single_use) VALUES \
             ('SAVE10', 10, '2099-12-31', FALSE), ('ONCE50', 50, '2099-12-31', TRUE)",
        )
        .execute(pool)
        .await?;
    }
    let any: Option<(i32,)> = sqlx::query_as("SELECT id FROM products LIMIT 1")
        .fetch_optional(pool)
        .await?;
    if any.is_none() {
        import_products_text(pool, SEED_PRODUCTS).await?;
    }
    Ok(())
}

/// Import `sku,name,price,stock` CSV text. Rows are parameterised, quoted CSV is understood,
/// duplicate SKUs (in the file or already in the database) and malformed rows are skipped.
pub async fn import_products_text(pool: &PgPool, text: &str) -> Result<usize, sqlx::Error> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(text.as_bytes());
    let mut seen = std::collections::HashSet::new();
    let mut n = 0;
    for rec in rdr.records() {
        let Ok(rec) = rec else { continue };
        let (Some(sku), Some(name), Some(price), Some(stock)) = (rec.get(0), rec.get(1), rec.get(2), rec.get(3)) else {
            tracing::warn!("skipping short product row");
            continue;
        };
        let (Ok(price), Ok(stock)) = (price.trim().parse::<f64>(), stock.trim().parse::<i32>()) else {
            tracing::warn!(sku, "skipping product row with bad number");
            continue;
        };
        if !seen.insert(sku.to_string()) {
            continue;
        }
        let res = sqlx::query(
            "INSERT INTO products (sku, name, price, stock) \
             SELECT $1, $2, $3, $4 WHERE NOT EXISTS (SELECT 1 FROM products WHERE sku = $1)",
        )
        .bind(sku)
        .bind(name)
        .bind(price)
        .bind(stock)
        .execute(pool)
        .await?;
        n += res.rows_affected() as usize;
    }
    Ok(n)
}

/// `manage export-sales <file>`: plain CSV of every order (legacy format).
pub async fn export_sales_plain(pool: &PgPool, out: &str) -> Result<usize, Box<dyn std::error::Error>> {
    let rows: Vec<(Option<i32>, Option<String>, Option<f64>, Option<String>)> =
        sqlx::query_as("SELECT order_no, status, total, created_at::text FROM orders ORDER BY id")
            .fetch_all(pool)
            .await?;
    let none = || "None".to_string();
    let mut text = String::from("order_no,status,total,created_at\n");
    for (no, status, total, created) in &rows {
        text.push_str(&format!(
            "{},{},{},{}\n",
            no.map(|v| v.to_string()).unwrap_or_else(none),
            status.clone().unwrap_or_else(none),
            total.map(py_float).unwrap_or_else(none),
            created.clone().unwrap_or_else(none),
        ));
    }
    std::fs::write(out, text)?;
    Ok(rows.len())
}
