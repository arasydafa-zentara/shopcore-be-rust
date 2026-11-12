use axum::{
    Json,
    extract::{Path, State},
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    auth::AdminUser,
    error::{AppError, AppResult},
    routes::orders::{amount_received, cancel_locked, payment_in_flight},
    state::AppState,
    util::{JsonBody, parse_id, py_str, ts},
};

#[derive(Serialize, sqlx::FromRow)]
pub struct AdminUserRow {
    id: i32,
    username: Option<String>,
    email: Option<String>,
    name: Option<String>,
    role: Option<String>,
    wallet_balance: Option<f64>,
    created_at: Option<String>,
}

/// Password hashes are never part of the response (the legacy endpoint returned them).
pub async fn list_users(State(s): State<AppState>, _a: AdminUser) -> AppResult<Json<Vec<AdminUserRow>>> {
    let rows = sqlx::query_as::<_, AdminUserRow>(sqlx::AssertSqlSafe(format!(
        "SELECT id, username, email, name, role, wallet_balance, {} AS created_at FROM users ORDER BY id",
        ts("created_at")
    )))
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(rows))
}

pub async fn set_role(
    State(s): State<AppState>,
    _a: AdminUser,
    Path(id): Path<String>,
    JsonBody(data): JsonBody,
) -> AppResult<Json<Value>> {
    let id = parse_id(&id)?;
    // No silent default: a request without a role must never demote somebody.
    let role = py_str(data.get("role")).unwrap_or_default();
    if role != "user" && role != "admin" {
        return Err(AppError::bad_request("role must be user or admin"));
    }
    let row: Option<(i32, Option<String>, Option<String>)> =
        sqlx::query_as("UPDATE users SET role = $1 WHERE id = $2 RETURNING id, username, role")
            .bind(&role)
            .bind(id)
            .fetch_optional(&s.pool)
            .await?;
    let (id, username, role) = row.ok_or_else(AppError::not_found)?;
    Ok(Json(json!({ "id": id, "username": username, "role": role })))
}

const STATUSES: [&str; 5] = ["pending", "paid", "shipped", "cancelled", "refunded"];

pub async fn set_order_status(
    State(s): State<AppState>,
    _a: AdminUser,
    Path(id): Path<String>,
    JsonBody(data): JsonBody,
) -> AppResult<Json<Value>> {
    let id = parse_id(&id)?;
    let status = py_str(data.get("status")).unwrap_or_default();
    if !STATUSES.contains(&status.as_str()) {
        return Err(AppError::bad_request("invalid status"));
    }
    let mut tx = s.pool.begin().await?;
    let cur: Option<(Option<String>, Option<i32>)> =
        sqlx::query_as("SELECT status, user_id FROM orders WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((current, owner)) = cur else {
        return Err(AppError::not_found());
    };
    if payment_in_flight(&mut tx, id).await? {
        return Err(AppError::conflict("payment in progress"));
    }
    // Guard rails: a status change must never move money or stock behind the ledger's back.
    if current.as_deref() == Some("cancelled") {
        return Err(AppError::conflict("order is cancelled"));
    }
    if status == "pending" && amount_received(&mut tx, id).await? > 0.0 {
        return Err(AppError::conflict("order has already been paid"));
    }
    if status == "cancelled" {
        // Same bookkeeping as the customer's cancel: stock returns, received money goes back to the wallet.
        cancel_locked(&mut tx, id, owner).await?;
    }
    let (id, order_no, status): (i32, Option<i32>, Option<String>) =
        sqlx::query_as("UPDATE orders SET status = $1 WHERE id = $2 RETURNING id, order_no, status")
            .bind(&status)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(Json(json!({ "id": id, "order_no": order_no, "status": status })))
}
