use std::{
    collections::HashMap,
    io::{BufWriter, Write},
    path::Path as FsPath,
};

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use flate2::{Compression, write::GzEncoder};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;

use crate::{
    auth::AdminUser,
    error::{AppError, AppResult},
    state::AppState,
    util::{JsonBody, py_float, py_str},
};

pub async fn sales(
    State(s): State<AppState>,
    _a: AdminUser,
    Query(q): Query<HashMap<String, String>>,
) -> AppResult<Json<Value>> {
    let d_from = q.get("from").cloned().unwrap_or_else(|| "1970-01-01".into());
    let d_to = q.get("to").cloned().unwrap_or_else(|| "2999-12-31".into());
    let group_by = q.get("group_by").cloned().unwrap_or_else(|| "day".into());
    // The grouping column is an identifier: allowlist only, never user text.
    let expr = match group_by.as_str() {
        "day" => "date(created_at)",
        "status" => "status",
        "user" => "user_id",
        _ => return Err(AppError::bad_request("invalid group_by")),
    };
    let buckets: Vec<(String, i64, Option<f64>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT COALESCE(b::text, 'None') AS bucket, orders, revenue FROM ( \
           SELECT {expr} AS b, COUNT(*)::bigint AS orders, SUM(total) AS revenue FROM orders \
           WHERE created_at >= $1::timestamp AND created_at < $2::timestamp GROUP BY 1) t ORDER BY b"
    )))
    .bind(&d_from)
    .bind(&d_to)
    .fetch_all(&s.pool)
    .await?;
    // One aggregate query instead of the legacy per-order, per-item, per-product loops.
    let top: Vec<(Option<String>, Option<f64>)> = sqlx::query_as(
        "SELECT p.name, SUM(i.price * i.quantity) AS revenue FROM orders o \
         JOIN order_items i ON i.order_id = o.id JOIN products p ON p.id = i.product_id \
         WHERE o.created_at >= $1::timestamp AND o.created_at < $2::timestamp AND o.status IN ('paid', 'shipped') \
         GROUP BY p.name ORDER BY revenue DESC, p.name LIMIT 10",
    )
    .bind(&d_from)
    .bind(&d_to)
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(json!({
        "from": d_from,
        "to": d_to,
        "group_by": group_by,
        "buckets": buckets.iter().map(|(b, n, r)| json!({"bucket": b, "orders": n, "revenue": r})).collect::<Vec<_>>(),
        "top_products": top.iter().map(|(n, r)| json!({"name": n, "revenue": r})).collect::<Vec<_>>(),
    })))
}

/// A file name that stays inside the export directory.
fn safe_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 200 && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

pub async fn export(State(s): State<AppState>, _a: AdminUser, JsonBody(data): JsonBody) -> AppResult<Json<Value>> {
    let filename = py_str(data.get("filename"))
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| "sales.csv".into());
    if !safe_name(&filename) {
        return Err(AppError::bad_request("invalid filename"));
    }
    let d_from = py_str(data.get("from")).unwrap_or_else(|| "1970-01-01".into());
    let d_to = py_str(data.get("to")).unwrap_or_else(|| "2999-12-31".into());

    let dir = FsPath::new(&s.cfg.export_dir);
    tokio::fs::create_dir_all(dir).await?;
    let final_path = dir.join(format!("{filename}.gz"));
    let tmp_path = dir.join(format!(".{filename}.gz.tmp"));

    // Rows are streamed from Postgres to a blocking writer thread that gzips as it goes (no shell, no
    // whole-report string in memory).
    let (ctx, mut crx) = tokio::sync::mpsc::channel::<String>(8);
    let tmp2 = tmp_path.clone();
    let writer = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let mut enc = GzEncoder::new(BufWriter::new(std::fs::File::create(&tmp2)?), Compression::default());
        while let Some(chunk) = crx.blocking_recv() {
            enc.write_all(chunk.as_bytes())?;
        }
        enc.finish()?.flush()
    });

    let mut buf = String::from("order_no,date,status,user,product,qty,price\n");
    let mut rows = sqlx::query(
        "SELECT o.order_no, o.created_at::text, o.status, u.username, p.name, i.quantity, i.price FROM orders o \
         JOIN users u ON u.id = o.user_id JOIN order_items i ON i.order_id = o.id JOIN products p ON p.id = i.product_id \
         WHERE o.created_at >= $1::timestamp AND o.created_at < $2::timestamp ORDER BY o.id, i.id",
    )
    .bind(&d_from)
    .bind(&d_to)
    .fetch(&s.pool);
    let none = || "None".to_string();
    let mut failure: Option<sqlx::Error> = None;
    while let Some(row) = rows.next().await {
        let row = match row {
            Ok(r) => r,
            Err(e) => {
                failure = Some(e);
                break;
            }
        };
        let g = |i| row.try_get::<Option<String>, _>(i).ok().flatten();
        let line = [
            row.try_get::<Option<i32>, _>(0)
                .ok()
                .flatten()
                .map(|v| v.to_string())
                .unwrap_or_else(none),
            g(1).unwrap_or_else(none),
            g(2).unwrap_or_else(none),
            g(3).unwrap_or_else(none),
            g(4).unwrap_or_else(none),
            row.try_get::<Option<i32>, _>(5)
                .ok()
                .flatten()
                .map(|v| v.to_string())
                .unwrap_or_else(none),
            row.try_get::<Option<f64>, _>(6)
                .ok()
                .flatten()
                .map(py_float)
                .unwrap_or_else(none),
        ]
        .iter()
        .map(|f| csv_field(f))
        .collect::<Vec<_>>()
        .join(",");
        buf.push_str(&line);
        buf.push('\n');
        if buf.len() > 64 * 1024 && ctx.send(std::mem::take(&mut buf)).await.is_err() {
            break;
        }
    }
    drop(rows);
    if failure.is_none() && !buf.is_empty() {
        let _ = ctx.send(buf).await;
    }
    drop(ctx);
    let written = writer
        .await
        .map_err(|_| AppError::with(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
    if let Some(e) = failure {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(e.into());
    }
    written?;
    tokio::fs::rename(&tmp_path, &final_path).await?;
    Ok(Json(json!({ "file": format!("{filename}.gz") })))
}

pub async fn download(
    State(s): State<AppState>,
    _a: AdminUser,
    Query(q): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    let name = q.get("file").cloned().unwrap_or_default();
    // Anything that is not a plain file name (../, absolute paths, ...) simply "does not exist".
    if !safe_name(&name) {
        return Err(AppError::not_found());
    }
    let bytes = tokio::fs::read(FsPath::new(&s.cfg.export_dir).join(&name))
        .await
        .map_err(|_| AppError::not_found())?;
    Ok((
        [(header::CONTENT_TYPE, HeaderValue::from_static("application/gzip"))],
        bytes,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_cannot_escape() {
        assert!(safe_name("sales.csv.gz"));
        assert!(!safe_name("../etc/passwd"));
        assert!(!safe_name("/etc/passwd"));
        assert!(!safe_name(".."));
        assert!(!safe_name("a\\b"));
        assert!(!safe_name(""));
    }

    #[test]
    fn csv_quoting() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    }
}
