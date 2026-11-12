use axum::{Json, body::Bytes, extract::State, http::HeaderMap};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::{
    error::{AppError, AppResult},
    state::AppState,
    util::{py_str, to_int},
};

fn bad_signature() -> AppError {
    AppError::with(axum::http::StatusCode::UNAUTHORIZED, "bad signature")
}

pub async fn bank(State(s): State<AppState>, headers: HeaderMap, body: Bytes) -> AppResult<Json<Value>> {
    // The signature is mandatory (legacy accepted unsigned calls) and compared in constant time.
    let sig = headers.get("x-signature").and_then(|v| v.to_str().ok()).ok_or_else(bad_signature)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(s.cfg.bank_secret.as_bytes()).map_err(|_| bad_signature())?;
    mac.update(&body);
    let expected = hex::encode(mac.finalize().into_bytes());
    if !bool::from(sig.as_bytes().ct_eq(expected.as_bytes())) {
        return Err(bad_signature());
    }

    let data: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    if data.get("status").and_then(Value::as_str) == Some("settled") {
        let order_id = to_int(data.get("order_id"), 0) as i32;
        let bank_ref = py_str(data.get("bank_ref"));
        let mut tx = s.pool.begin().await?;
        // Taking the order lock first makes this wait for an in-flight payment to commit.
        let o: Option<(Option<i32>, Option<String>)> =
            sqlx::query_as("SELECT order_no, notify_url FROM orders WHERE id = $1 FOR UPDATE").bind(order_id).fetch_optional(&mut *tx).await?;
        sqlx::query("UPDATE payments SET status = 'settled' WHERE order_id = $1 AND bank_ref = $2")
            .bind(order_id)
            .bind(&bank_ref)
            .execute(&mut *tx)
            .await?;
        let moved = sqlx::query("UPDATE orders SET status = 'paid', paid_amount = COALESCE(total, 0) WHERE id = $1 AND status = 'pending'")
            .bind(order_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        if let (Some((order_no, url)), true) = (o, moved.rows_affected() > 0) {
            s.notifier.order_event(order_id, order_no, "paid", url.as_deref());
        }
    }
    Ok(Json(json!({ "ok": true })))
}
