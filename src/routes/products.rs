use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Serialize;

use crate::{
    auth::AdminUser,
    error::{AppError, AppResult},
    state::AppState,
    util::{JsonBody, paging, parse_id, parse_money, py_str, to_int, ts},
};

#[derive(Serialize, sqlx::FromRow)]
pub struct Product {
    pub id: i32,
    pub sku: Option<String>,
    pub name: Option<String>,
    pub price: Option<f64>,
    pub stock: Option<i32>,
    pub created_at: Option<String>,
}

fn select_cols() -> String {
    format!(
        "SELECT id, sku, name, price, stock, {} AS created_at FROM products",
        ts("created_at")
    )
}

pub async fn list(
    State(s): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> AppResult<Json<Vec<Product>>> {
    // `sort` is an identifier, so it is matched against an allowlist and never concatenated from input.
    let col = match q.get("sort").map(String::as_str).unwrap_or("id") {
        "id" => "id",
        "name" => "name",
        "price" => "price",
        "stock" => "stock",
        "sku" => "sku",
        _ => return Err(AppError::bad_request("invalid sort")),
    };
    let (offset, limit) = paging(q.get("page").map(String::as_str), q.get("limit").map(String::as_str));
    let mut sql = select_cols();
    let search = q.get("q").filter(|v| !v.is_empty());
    if search.is_some() {
        sql.push_str(" WHERE name ILIKE $1 ESCAPE '\\'");
    }
    sql.push_str(&format!(" ORDER BY {col}, id"));
    let base = if search.is_some() { 2 } else { 1 };
    if limit.is_some() {
        sql.push_str(&format!(" LIMIT ${} OFFSET ${}", base, base + 1));
    }
    let mut query = sqlx::query_as::<_, Product>(sqlx::AssertSqlSafe(sql));
    if let Some(term) = search {
        // `%` and `_` typed by the user are literal characters, not wildcards.
        let escaped = term.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        query = query.bind(format!("%{escaped}%"));
    }
    if let Some(l) = limit {
        query = query.bind(l).bind(offset);
    }
    Ok(Json(query.fetch_all(&s.pool).await?))
}

async fn fetch(s: &AppState, id: i32) -> AppResult<Option<Product>> {
    Ok(
        sqlx::query_as::<_, Product>(sqlx::AssertSqlSafe(format!("{} WHERE id = $1", select_cols())))
            .bind(id)
            .fetch_optional(&s.pool)
            .await?,
    )
}

pub async fn get_one(State(s): State<AppState>, Path(id): Path<String>) -> AppResult<Json<Product>> {
    let id = parse_id(&id)?;
    fetch(&s, id).await?.map(Json).ok_or_else(AppError::not_found)
}

pub async fn create(
    State(s): State<AppState>,
    _a: AdminUser,
    JsonBody(data): JsonBody,
) -> AppResult<(StatusCode, Json<Product>)> {
    let price = checked_price(&data)?.unwrap_or(0.0);
    let stock = checked_stock(&data)?.unwrap_or(0);
    let (id,): (i32,) =
        sqlx::query_as("INSERT INTO products (sku, name, price, stock) VALUES ($1, $2, $3, $4) RETURNING id")
            .bind(py_str(data.get("sku")))
            .bind(py_str(data.get("name")))
            .bind(price)
            .bind(stock)
            .fetch_one(&s.pool)
            .await?;
    let p = fetch(&s, id).await?.ok_or_else(AppError::not_found)?;
    Ok((StatusCode::CREATED, Json(p)))
}

pub async fn update(
    State(s): State<AppState>,
    _a: AdminUser,
    Path(id): Path<String>,
    JsonBody(data): JsonBody,
) -> AppResult<Json<Product>> {
    let id = parse_id(&id)?;
    let price = checked_price(&data)?;
    let stock = checked_stock(&data)?;
    // One statement: omitted (or null) fields keep their current value, and the stock is never overwritten with a
    // stale snapshot, so an order placed in between cannot be undone by an edit of the name.
    let updated = sqlx::query(
        "UPDATE products SET sku = COALESCE($1, sku), name = COALESCE($2, name), \
         price = COALESCE($3, price), stock = COALESCE($4, stock) WHERE id = $5",
    )
    .bind(py_str(data.get("sku")))
    .bind(py_str(data.get("name")))
    .bind(price)
    .bind(stock)
    .bind(id)
    .execute(&s.pool)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::not_found());
    }
    fetch(&s, id).await?.map(Json).ok_or_else(AppError::not_found)
}

/// `price`, when given, must be a finite, non-negative number (a NaN or negative price would break every payment
/// of that product, or pay the customer for buying it).
fn checked_price(data: &serde_json::Map<String, serde_json::Value>) -> AppResult<Option<f64>> {
    match data.get("price") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => match parse_money(Some(v)) {
            Some(p) if p.is_finite() && p >= 0.0 => Ok(Some(p)),
            _ => Err(AppError::bad_request("price must be a non-negative number")),
        },
    }
}

/// `stock`, when given, must be a non-negative integer that fits the column.
fn checked_stock(data: &serde_json::Map<String, serde_json::Value>) -> AppResult<Option<i32>> {
    match data.get("stock") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => i32::try_from(to_int(Some(v), -1))
            .ok()
            .filter(|n| *n >= 0)
            .map(Some)
            .ok_or_else(|| AppError::bad_request("stock must be a non-negative integer")),
    }
}
