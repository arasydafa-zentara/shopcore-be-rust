use axum::{
    Json,
    extract::{Path, State},
};
use serde_json::{Value, json};

use crate::{
    auth::AuthUser,
    error::{AppError, AppResult},
    jobs,
    routes::orders::payment_in_flight,
    services::bank,
    state::AppState,
    util::{JsonBody, parse_id, py_str},
};

type OrderRow = (Option<i32>, Option<String>, Option<f64>, Option<String>, Option<String>);

pub async fn pay(
    State(s): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    JsonBody(data): JsonBody,
) -> AppResult<Json<Value>> {
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
            // 1. Under the order lock, claim the order with a `pending` payment row (this also stops a
            //    cancel or a second payment while the bank call is running).
            let payment_id = begin_bank_payment(&s, oid, total).await?;
            // 2. Charge outside any transaction. Every attempt carries the same Idempotency-Key, so the bank
            //    records one charge. The row's number and creation time keep the key unique even if ids are reused.
            let key = format!(
                "shopcore-{}-{}-{}",
                oid,
                order_no.unwrap_or_default(),
                created.unwrap_or_default()
            );
            let Some(bank_ref) = bank::charge(&s, &s.cfg, oid, total, &key).await else {
                abort_bank_payment(&s, payment_id).await;
                return Err(AppError::with(axum::http::StatusCode::BAD_GATEWAY, "payment failed"));
            };
            // 3. Record the result and mark the order paid.
            finish_bank_payment(&s, oid, payment_id, total, &bank_ref).await?;
            Some(bank_ref)
        }
        _ => return Err(AppError::bad_request("unknown method")),
    };

    jobs::email(&format!("Payment received for order {}", order_no.unwrap_or_default()));
    s.notifier.order_event(oid, order_no, "paid", notify_url.as_deref());
    Ok(Json(
        json!({ "order_id": oid, "status": "paid", "method": method, "bank_ref": bank_ref }),
    ))
}

async fn pay_wallet(s: &AppState, user: &AuthUser, oid: i32, total: f64) -> AppResult<()> {
    let mut tx = s.pool.begin().await?;
    let st: Option<(Option<String>,)> = sqlx::query_as("SELECT status FROM orders WHERE id = $1 FOR UPDATE")
        .bind(oid)
        .fetch_optional(&mut *tx)
        .await?;
    if st.and_then(|r| r.0).as_deref() != Some("pending") {
        return Err(AppError::conflict("order not payable"));
    }
    if payment_in_flight(&mut tx, oid).await? {
        return Err(AppError::conflict("payment in progress"));
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
    sqlx::query(
        "INSERT INTO payments (order_id, amount, method, bank_ref, status) VALUES ($1, $2, 'wallet', NULL, 'captured')",
    )
    .bind(oid)
    .bind(total)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE orders SET status = 'paid', paid_amount = $1 WHERE id = $2")
        .bind(total)
        .bind(oid)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn begin_bank_payment(s: &AppState, oid: i32, total: f64) -> AppResult<i32> {
    let mut tx = s.pool.begin().await?;
    let st: Option<(Option<String>,)> = sqlx::query_as("SELECT status FROM orders WHERE id = $1 FOR UPDATE")
        .bind(oid)
        .fetch_optional(&mut *tx)
        .await?;
    if st.and_then(|r| r.0).as_deref() != Some("pending") {
        return Err(AppError::conflict("order not payable"));
    }
    if payment_in_flight(&mut tx, oid).await? {
        return Err(AppError::conflict("payment in progress"));
    }
    // A pending row older than the in-flight window belongs to a request that died: retire it.
    sqlx::query("UPDATE payments SET status = 'failed' WHERE order_id = $1 AND status = 'pending'")
        .bind(oid)
        .execute(&mut *tx)
        .await?;
    let (pid,): (i32,) = sqlx::query_as(
        "INSERT INTO payments (order_id, amount, method, bank_ref, status) VALUES ($1, $2, 'bank', NULL, 'pending') RETURNING id",
    )
    .bind(oid)
    .bind(total)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(pid)
}

async fn abort_bank_payment(s: &AppState, payment_id: i32) {
    if let Err(e) = sqlx::query("UPDATE payments SET status = 'failed' WHERE id = $1")
        .bind(payment_id)
        .execute(&s.pool)
        .await
    {
        tracing::error!(error = %e, payment_id, "could not mark failed payment");
    }
}

/// The settlement webhook can arrive before this commit (the bank settles ~0.2s after the charge). Then the
/// order is already `paid` and the payment is stored as settled straight away.
async fn finish_bank_payment(s: &AppState, oid: i32, payment_id: i32, total: f64, bank_ref: &str) -> AppResult<()> {
    let mut tx = s.pool.begin().await?;
    let st: Option<(Option<String>,)> = sqlx::query_as("SELECT status FROM orders WHERE id = $1 FOR UPDATE")
        .bind(oid)
        .fetch_optional(&mut *tx)
        .await?;
    let payment_status = match st.and_then(|r| r.0).as_deref() {
        Some("pending") => "captured",
        Some("paid") => "settled",
        other => {
            // Cannot happen while the in-flight guard is respected; keep the money trail if it ever does.
            tracing::error!(oid, status = ?other, bank_ref, "bank charge succeeded for an order that is not payable");
            sqlx::query("UPDATE payments SET status = 'orphaned', bank_ref = $1 WHERE id = $2")
                .bind(bank_ref)
                .bind(payment_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Err(AppError::conflict("order not payable"));
        }
    };
    sqlx::query("UPDATE payments SET status = $1, bank_ref = $2 WHERE id = $3")
        .bind(payment_status)
        .bind(bank_ref)
        .bind(payment_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE orders SET status = 'paid', paid_amount = $1 WHERE id = $2")
        .bind(total)
        .bind(oid)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
