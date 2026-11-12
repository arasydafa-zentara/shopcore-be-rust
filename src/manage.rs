//! Maintenance commands (the legacy manage.py): init-db, seed, import-products, export-sales, purge-sessions.

use crate::{config::Config, db};

const USAGE: &str = "usage: manage <init-db|seed|import-products|export-sales|purge-sessions> [args]";

pub async fn run(cfg: &Config, cmd: Option<&str>, args: &[String]) -> i32 {
    let known = ["init-db", "seed", "import-products", "export-sales", "purge-sessions"];
    let Some(cmd) = cmd.filter(|c| known.contains(c)) else {
        println!("{USAGE}");
        return 1;
    };
    if matches!(cmd, "import-products" | "export-sales") && args.is_empty() {
        println!("{USAGE}");
        return 1;
    }
    match execute(cfg, cmd, args).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

async fn execute(cfg: &Config, cmd: &str, args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let pool = db::connect(cfg).await?;
    // Idempotent, so every command works even on a database that was never initialised.
    db::init_db(&pool).await?;
    match cmd {
        "init-db" => println!("tables ready"),
        "seed" => {
            db::seed(&pool, cfg).await?;
            println!("seeded");
        }
        "import-products" => {
            let text = std::fs::read_to_string(&args[0])?;
            let n = db::import_products_text(&pool, &text).await?;
            println!("imported {n} products");
        }
        "export-sales" => {
            let n = db::export_sales_plain(&pool, &args[0]).await?;
            println!("wrote {} ({} rows)", args[0], n);
        }
        "purge-sessions" => {
            sqlx::query("DELETE FROM sessions").execute(&pool).await?;
            println!("purged sessions");
        }
        _ => unreachable!(),
    }
    Ok(())
}
