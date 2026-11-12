use axum::{
    Json,
    extract::{Path, State},
};
use serde_json::{Value, json};

use crate::{
    auth::AuthUser,
    error::{AppError, AppResult},
    jobs,
    services::bank,
    state::AppState,
    util::{JsonBody, parse_id, py_str},
};

type OrderRow = (Option<i32>, Option<String>, Option<f64>, Option<String>, Option<String>);

pub async fn pay(State(s): State<AppState>, user: AuthUser, Path(id): Path<String>, JsonBody(data): JsonBody) -> AppResult<Json<Value>> {
    let oid = parse_id(&id)?;
    let method = match data.get("method") {
        None => "wallet".to_string(),
        Some(v) => py_str(Some(v)).unwrap_or_default(),
    };
    let row: Option<OrderRow> = sqlx::query_as(
        "SELECT order_no, status, total, notify_url, created_at::text FROM orders WHERE id = $1 AND user_id = $2",
    )
    .bind(oid)
    .bind(user.id)
    .fetch_optional(&s.pool)
    .await?;
    let Some((order_no, status, total, notify_url, created)) = row else {
        return Err(AppError::not_found());
    };
    if status.as_deref() != Some("pending") {
        return Err(AppError::conflict("order not payable"));
    }
    let total = total.unwrap_or(0.0);

    let bank_ref = match method.as_str() {
        "wallet" => {
            pay_wallet(&s, &user, oid, total).await?;
            None
        }
        "bank" => {
            // Same key for every attempt and every concurrent request on this order: the bank records one
            // charge. The row's number and creation time keep the key unique even if ids are ever reused.
            let key = format!("shopcore-{}-{}-{}", oid, order_no.unwrap_or_default(), created.unwrap_or_default());
            let r = bank::charge(&s, &s.cfg, oid, total, &key)
                .await
                .ok_or_else(|| AppError::with(axum::http::StatusCode::BAD_GATEWAY, "payment failed"))?;
            record_bank_payment(&s, oid, total, &r).await?;
            Some(r)
        }
        _ => return Err(AppError::bad_request("unknown method")),
    };

    jobs::email(&format!("Payment received for order {}", order_no.unwrap_or_default()));
    s.notifier.order_event(oid, order_no, "paid", notify_url.as_deref());
    Ok(Json(json!({ "order_id": oid, "status": "paid", "method": method, "bank_ref": bank_ref })))
}

async fn pay_wallet(s: &AppState, user: &AuthUser, oid: i32, total: f64) -> AppResult<()> {
    let mut tx = s.pool.begin().await?;
    let st: Option<(Option<String>,)> =
        sqlx::query_as("SELECT status FROM orders WHERE id = $1 FOR UPDATE").bind(oid).fetch_optional(&mut *tx).await?;
    if st.and_then(|r| r.0).as_deref() != Some("pending") {
        return Err(AppError::conflict("order not payable"));
    }
    // Check and debit in one statement so two concurrent payments cannot spend the same balance.
    let debited = sqlx::query(
        "UPDATE users SET wallet_balance = ROUND((wallet_balance - $1)::numeric, 2)::float8 \
         WHERE id = $2 AND COALESCE(wallet_balance, 0) >= $1",
    )
    .bind(total)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    if debited.rows_affected() == 0 {
        return Err(AppError::payment_required("insufficient wallet balance"));
    }
    sqlx::query("INSERT INTO payments (order_id, amount, method, bank_ref, status) VALUES ($1, $2, 'wallet', NULL, 'captured')")
        .bind(oid)
        .bind(total)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE orders SET status = 'paid', paid_amount = $1 WHERE id = $2").bind(total).bind(oid).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Record the captured bank payment. The settlement webhook can arrive before this commit (the bank
/// settles ~0.2s after the charge), in which case the order is already `paid` without a payment row:
/// that still counts as success and the payment is stored as settled.
async fn record_bank_payment(s: &AppState, oid: i32, total: f64, bank_ref: &str) -> AppResult<()> {
    let mut tx = s.pool.begin().await?;
    let st: Option<(Option<String>,)> =
        sqlx::query_as("SELECT status FROM orders WHERE id = $1 FOR UPDATE").bind(oid).fetch_optional(&mut *tx).await?;
    let status = st.and_then(|r| r.0);
    let payment_status = match status.as_deref() {
        Some("pending") => "captured",
        Some("paid") => {
            let (exists,): (bool,) =
                sqlx::query_as("SELECT EXISTS (SELECT 1 FROM payments WHERE order_id = $1)").bind(oid).fetch_one(&mut *tx).await?;
            if exists {
                return Err(AppError::conflict("order not payable"));
            }
            "settled"
        }
        _ => return Err(AppError::conflict("order not payable")),
    };
    sqlx::query("INSERT INTO payments (order_id, amount, method, bank_ref, status) VALUES ($1, $2, 'bank', $3, $4)")
        .bind(oid)
        .bind(total)
        .bind(bank_ref)
        .bind(payment_status)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE orders SET status = 'paid', paid_amount = $1 WHERE id = $2").bind(total).bind(oid).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
