use axum::{Json, extract::State};
use serde_json::{Value, json};

use crate::{
    auth::AuthUser,
    error::{AppError, AppResult},
    state::AppState,
    util::{JsonBody, finite_positive, parse_money, round2, to_int},
};

pub async fn get_wallet(State(s): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let (bal,): (Option<f64>,) = sqlx::query_as("SELECT wallet_balance FROM users WHERE id = $1")
        .bind(user.id)
        .fetch_one(&s.pool)
        .await?;
    Ok(Json(json!({ "balance": bal.unwrap_or(0.0) })))
}

pub async fn topup(State(s): State<AppState>, user: AuthUser, JsonBody(data): JsonBody) -> AppResult<Json<Value>> {
    let amount = parse_money(data.get("amount")).unwrap_or(0.0);
    if !finite_positive(amount) {
        return Err(AppError::bad_request("amount must be a positive number"));
    }
    // One atomic statement: no read-modify-write window.
    let (bal,): (f64,) = sqlx::query_as(
        "UPDATE users SET wallet_balance = ROUND((COALESCE(wallet_balance, 0) + $1)::numeric, 2)::float8 \
         WHERE id = $2 RETURNING wallet_balance",
    )
    .bind(round2(amount))
    .bind(user.id)
    .fetch_one(&s.pool)
    .await?;
    Ok(Json(json!({ "balance": bal })))
}

pub async fn transfer(State(s): State<AppState>, user: AuthUser, JsonBody(data): JsonBody) -> AppResult<Json<Value>> {
    let to_id = to_int(data.get("to_user_id"), 0) as i32;
    let amount = parse_money(data.get("amount")).unwrap_or(0.0);
    if !finite_positive(amount) {
        return Err(AppError::bad_request("amount must be a positive number"));
    }
    let amount = round2(amount);
    if to_id == user.id {
        return Err(AppError::bad_request("cannot transfer to yourself"));
    }
    let mut tx = s.pool.begin().await?;
    // Lock both wallets in id order so two opposite transfers cannot deadlock.
    let rows: Vec<(i32, Option<f64>)> =
        sqlx::query_as("SELECT id, wallet_balance FROM users WHERE id = ANY($1) ORDER BY id FOR UPDATE")
            .bind(vec![user.id, to_id])
            .fetch_all(&mut *tx)
            .await?;
    if !rows.iter().any(|(id, _)| *id == to_id) {
        return Err(AppError::not_found_with("recipient not found"));
    }
    let src = rows
        .iter()
        .find(|(id, _)| *id == user.id)
        .and_then(|(_, b)| *b)
        .unwrap_or(0.0);
    if src < amount {
        return Err(AppError::payment_required("insufficient balance"));
    }
    let (new_bal,): (f64,) = sqlx::query_as(
        "UPDATE users SET wallet_balance = ROUND((COALESCE(wallet_balance, 0) - $1)::numeric, 2)::float8 \
         WHERE id = $2 RETURNING wallet_balance",
    )
    .bind(amount)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE users SET wallet_balance = ROUND((COALESCE(wallet_balance, 0) + $1)::numeric, 2)::float8 WHERE id = $2",
    )
    .bind(amount)
    .bind(to_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "balance": new_bal })))
}
