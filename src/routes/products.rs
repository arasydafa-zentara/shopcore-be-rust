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
    util::{JsonBody, paging, parse_id, py_str, to_int, to_money, ts},
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
        sql.push_str(" WHERE name ILIKE $1");
    }
    sql.push_str(&format!(" ORDER BY {col}, id"));
    let base = if search.is_some() { 2 } else { 1 };
    if limit.is_some() {
        sql.push_str(&format!(" LIMIT ${} OFFSET ${}", base, base + 1));
    }
    let mut query = sqlx::query_as::<_, Product>(sqlx::AssertSqlSafe(sql));
    if let Some(term) = search {
        query = query.bind(format!("%{term}%"));
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
    let (id,): (i32,) =
        sqlx::query_as("INSERT INTO products (sku, name, price, stock) VALUES ($1, $2, $3, $4) RETURNING id")
            .bind(py_str(data.get("sku")))
            .bind(py_str(data.get("name")))
            .bind(to_money(data.get("price"), 0.0))
            .bind(to_int(data.get("stock"), 0) as i32)
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
    let cur = fetch(&s, id).await?.ok_or_else(AppError::not_found)?;
    let sku = if data.contains_key("sku") {
        py_str(data.get("sku"))
    } else {
        cur.sku
    };
    let name = if data.contains_key("name") {
        py_str(data.get("name"))
    } else {
        cur.name
    };
    let price = if data.contains_key("price") {
        to_money(data.get("price"), 0.0)
    } else {
        cur.price.unwrap_or(0.0)
    };
    let stock = if data.contains_key("stock") {
        to_int(data.get("stock"), 0) as i32
    } else {
        cur.stock.unwrap_or(0)
    };
    sqlx::query("UPDATE products SET sku = $1, name = $2, price = $3, stock = $4 WHERE id = $5")
        .bind(sku)
        .bind(name)
        .bind(price)
        .bind(stock)
        .bind(id)
        .execute(&s.pool)
        .await?;
    fetch(&s, id).await?.map(Json).ok_or_else(AppError::not_found)
}
