use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};

use crate::{
    auth::AuthUser,
    error::{AppError, AppResult},
    jobs,
    state::AppState,
    util::{JsonBody, finite_positive, paging, parse_id, parse_money, py_str, round2, to_int, ts},
};

#[derive(Serialize, sqlx::FromRow)]
pub struct OrderItem {
    #[serde(skip)]
    pub order_id: Option<i32>,
    pub product_id: Option<i32>,
    pub sku: Option<String>,
    pub name: Option<String>,
    pub quantity: Option<i32>,
    pub price: Option<f64>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Order {
    pub id: i32,
    pub order_no: Option<i32>,
    pub user_id: Option<i32>,
    pub status: Option<String>,
    pub total: Option<f64>,
    pub paid_amount: Option<f64>,
    pub refunded: Option<f64>,
    pub coupon: Option<String>,
    pub created_at: Option<String>,
    #[sqlx(skip)]
    pub items: Vec<OrderItem>,
}

pub fn order_select() -> String {
    format!(
        "SELECT id, order_no, user_id, status, total, paid_amount, refunded, coupon, {} AS created_at FROM orders",
        ts("created_at")
    )
}

/// Load the items of many orders with one query (the legacy code did one query per item).
pub async fn attach_items(pool: &PgPool, orders: &mut [Order]) -> Result<(), sqlx::Error> {
    if orders.is_empty() {
        return Ok(());
    }
    let ids: Vec<i32> = orders.iter().map(|o| o.id).collect();
    let items: Vec<OrderItem> = sqlx::query_as(
        "SELECT i.order_id, i.product_id, p.sku, p.name, i.quantity, i.price \
         FROM order_items i LEFT JOIN products p ON p.id = i.product_id \
         WHERE i.order_id = ANY($1) ORDER BY i.id",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    let mut by_order: HashMap<i32, Vec<OrderItem>> = HashMap::new();
    for it in items {
        by_order.entry(it.order_id.unwrap_or_default()).or_default().push(it);
    }
    for o in orders.iter_mut() {
        o.items = by_order.remove(&o.id).unwrap_or_default();
    }
    Ok(())
}

pub async fn fetch_order(pool: &PgPool, id: i32) -> Result<Option<Order>, sqlx::Error> {
    let o = sqlx::query_as::<_, Order>(sqlx::AssertSqlSafe(format!("{} WHERE id = $1", order_select())))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    match o {
        Some(o) => {
            let mut v = vec![o];
            attach_items(pool, &mut v).await?;
            Ok(v.pop())
        }
        None => Ok(None),
    }
}

/// Visible order for the caller, or 404 (also for somebody else's order).
async fn visible_order(s: &AppState, user: &AuthUser, id: i32) -> AppResult<Order> {
    match fetch_order(&s.pool, id).await? {
        Some(o) if user.can_access(o.user_id) => Ok(o),
        _ => Err(AppError::not_found()),
    }
}

/// True while a bank charge for this order is being processed (a `pending` payment row younger than two
/// minutes; the bank call itself is capped well below that). While it is true the order must not be
/// cancelled or otherwise moved, so a customer is never charged for an order that no longer exists.
pub async fn payment_in_flight(conn: &mut PgConnection, order_id: i32) -> Result<bool, sqlx::Error> {
    let (busy,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM payments WHERE order_id = $1 AND status = 'pending' \
         AND created_at > now()::timestamp - interval '2 minutes')",
    )
    .bind(order_id)
    .fetch_one(conn)
    .await?;
    Ok(busy)
}

/// Money actually received for the order. `paid_amount` is authoritative; legacy rows that were settled through the
/// webhook never stored it, so a captured/settled payment row stands in for "paid in full". An admin merely setting
/// a status never creates money, so refunds and cancellations can never pay out more than was received.
const RECEIVED: &str = "(CASE WHEN COALESCE(paid_amount, 0) > 0 THEN paid_amount \
    WHEN EXISTS (SELECT 1 FROM payments p WHERE p.order_id = orders.id AND p.status IN ('captured', 'settled')) \
    THEN COALESCE(total, 0) ELSE 0 END)";

pub async fn amount_received(conn: &mut PgConnection, order_id: i32) -> Result<f64, sqlx::Error> {
    let (v,): (f64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {RECEIVED}::float8 FROM orders WHERE id = $1"
    )))
    .bind(order_id)
    .fetch_one(conn)
    .await?;
    Ok(v)
}

/// Cancel an order whose row is already locked (`FOR UPDATE`): give the stock back and refund whatever was received
/// and not yet refunded. Used by the customer endpoint and by the admin status endpoint.
pub async fn cancel_locked(conn: &mut PgConnection, id: i32, owner: Option<i32>) -> Result<(), sqlx::Error> {
    // Lock the affected products in id order, the same order order-creation uses, so the two can never deadlock.
    sqlx::query(
        "SELECT id FROM products WHERE id IN (SELECT product_id FROM order_items WHERE order_id = $1) ORDER BY id FOR UPDATE",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE products p SET stock = COALESCE(p.stock, 0) + r.q FROM \
         (SELECT product_id, SUM(quantity)::int AS q FROM order_items WHERE order_id = $1 GROUP BY product_id) r \
         WHERE p.id = r.product_id",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    let (refunded,): (Option<f64>,) = sqlx::query_as("SELECT refunded FROM orders WHERE id = $1")
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
    let back = round2(amount_received(conn, id).await? - refunded.unwrap_or(0.0));
    if back > 0.0 {
        credit_wallet(conn, owner, back).await?;
    }
    sqlx::query("UPDATE orders SET status = 'cancelled' WHERE id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub async fn credit_wallet(conn: &mut PgConnection, user_id: Option<i32>, amount: f64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE users SET wallet_balance = ROUND((COALESCE(wallet_balance, 0) + $1)::numeric, 2)::float8 WHERE id = $2",
    )
    .bind(round2(amount))
    .bind(user_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Same arithmetic as the legacy `calc_total`: subtotal * (1 - pct/100), rounded to 2 places.
pub fn calc_total(subtotal: f64, percent: i32) -> f64 {
    let mut total = subtotal;
    if percent != 0 {
        total = subtotal * (1.0 - percent as f64 / 100.0);
    }
    round2(total)
}

struct Line {
    idx: usize,
    pid: i32,
    qty: i32,
}

pub async fn create(
    State(s): State<AppState>,
    user: AuthUser,
    JsonBody(data): JsonBody,
) -> AppResult<(StatusCode, Json<Order>)> {
    let raw = match data.get("items") {
        Some(Value::Array(a)) if !a.is_empty() => a,
        _ => return Err(AppError::bad_request("items required")),
    };
    if raw.len() > 100 {
        return Err(AppError::bad_request("too many items (max 100)"));
    }
    let mut lines = Vec::with_capacity(raw.len());
    for (idx, it) in raw.iter().enumerate() {
        let Value::Object(o) = it else {
            return Err(AppError::bad_request("invalid item"));
        };
        let qty = to_int(o.get("quantity"), 1);
        if !(1..=i32::MAX as i64).contains(&qty) {
            return Err(AppError::bad_request("quantity must be a positive integer"));
        }
        lines.push(Line {
            idx,
            // An id outside the i32 range cannot exist (it must not wrap around onto a real product).
            pid: i32::try_from(to_int(o.get("product_id"), 0)).unwrap_or(0),
            qty: qty as i32,
        });
    }
    let coupon_code = py_str(data.get("coupon")).filter(|c| !c.is_empty());
    let notify_url = py_str(data.get("notify_url")).filter(|u| !u.is_empty());

    let mut tx = s.pool.begin().await?;

    let mut pct = 0;
    if let Some(code) = &coupon_code {
        let c: Option<(Option<i32>, Option<String>, Option<bool>, Option<i32>)> = sqlx::query_as(
            "SELECT percent, expires, single_use, used_count FROM coupons WHERE code = $1 ORDER BY id LIMIT 1",
        )
        .bind(code)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((percent, expires, single_use, used)) = c else {
            return Err(AppError::bad_request("invalid coupon"));
        };
        let (today,): (String,) = sqlx::query_as("SELECT ((now() AT TIME ZONE 'Asia/Jakarta')::date)::text")
            .fetch_one(&mut *tx)
            .await?;
        if expires.map(|e| e < today).unwrap_or(false) {
            return Err(AppError::bad_request("coupon expired"));
        }
        if single_use.unwrap_or(false) && used.unwrap_or(0) > 0 {
            return Err(AppError::conflict("coupon already used"));
        }
        pct = percent.unwrap_or(0);
    }

    // Reserve stock with one guarded UPDATE per product, in product-id order so concurrent orders
    // always take row locks in the same order. Any failure rolls the whole order back.
    let mut order: Vec<&Line> = lines.iter().collect();
    order.sort_by_key(|l| (l.pid, l.idx));
    let mut prices = vec![0.0f64; lines.len()];
    for l in order {
        let row: Option<(Option<f64>,)> =
            sqlx::query_as("UPDATE products SET stock = stock - $2 WHERE id = $1 AND stock >= $2 RETURNING price")
                .bind(l.pid)
                .bind(l.qty)
                .fetch_optional(&mut *tx)
                .await?;
        match row {
            Some((price,)) => prices[l.idx] = price.unwrap_or(0.0),
            None => {
                let sku: Option<(Option<String>,)> = sqlx::query_as("SELECT sku FROM products WHERE id = $1")
                    .bind(l.pid)
                    .fetch_optional(&mut *tx)
                    .await?;
                return Err(match sku {
                    None => AppError::not_found_with(format!("product {} not found", l.pid)),
                    Some((sku,)) => AppError::conflict(format!("insufficient stock for {}", sku.unwrap_or_default())),
                });
            }
        }
    }
    let mut subtotal = 0.0f64;
    for l in &lines {
        subtotal += prices[l.idx] * l.qty as f64;
    }
    let total = calc_total(subtotal, pct);

    let (oid, order_no): (i32, i32) = sqlx::query_as(
        "INSERT INTO orders (order_no, user_id, status, total, coupon, notify_url, created_at) \
         VALUES (nextval('order_no_seq'), $1, 'pending', $2, $3, $4, now() AT TIME ZONE 'Asia/Jakarta') \
         RETURNING id, order_no",
    )
    .bind(user.id)
    .bind(total)
    .bind(&coupon_code)
    .bind(&notify_url)
    .fetch_one(&mut *tx)
    .await?;
    for l in &lines {
        sqlx::query("INSERT INTO order_items (order_id, product_id, quantity, price) VALUES ($1, $2, $3, $4)")
            .bind(oid)
            .bind(l.pid)
            .bind(l.qty)
            .bind(prices[l.idx])
            .execute(&mut *tx)
            .await?;
    }
    if let Some(code) = &coupon_code {
        // Atomic check-and-increment: two requests can never both consume a single-use coupon.
        let r = sqlx::query(
            "UPDATE coupons SET used_count = COALESCE(used_count, 0) + 1 \
             WHERE code = $1 AND (NOT COALESCE(single_use, FALSE) OR COALESCE(used_count, 0) = 0)",
        )
        .bind(code)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 0 {
            return Err(AppError::conflict("coupon already used"));
        }
    }
    tx.commit().await?;

    jobs::email(&format!("Order {order_no} received"));
    s.notifier
        .order_event(oid, Some(order_no), "pending", notify_url.as_deref());
    let o = fetch_order(&s.pool, oid).await?.ok_or_else(AppError::not_found)?;
    Ok((StatusCode::CREATED, Json(o)))
}

pub async fn list(
    State(s): State<AppState>,
    user: AuthUser,
    Query(q): Query<HashMap<String, String>>,
) -> AppResult<Json<Vec<Order>>> {
    let (offset, limit) = paging(q.get("page").map(String::as_str), q.get("limit").map(String::as_str));
    let mut sql = format!("{} WHERE user_id = $1 ORDER BY id DESC", order_select());
    if limit.is_some() {
        sql.push_str(" LIMIT $2 OFFSET $3");
    }
    let mut query = sqlx::query_as::<_, Order>(sqlx::AssertSqlSafe(sql)).bind(user.id);
    if let Some(l) = limit {
        query = query.bind(l).bind(offset);
    }
    let mut orders = query.fetch_all(&s.pool).await?;
    attach_items(&s.pool, &mut orders).await?;
    Ok(Json(orders))
}

pub async fn get_one(State(s): State<AppState>, user: AuthUser, Path(id): Path<String>) -> AppResult<Json<Order>> {
    let id = parse_id(&id)?;
    Ok(Json(visible_order(&s, &user, id).await?))
}

pub async fn invoice(State(s): State<AppState>, user: AuthUser, Path(id): Path<String>) -> AppResult<Json<Value>> {
    let id = parse_id(&id)?;
    let o = visible_order(&s, &user, id).await?;
    let cust: Option<(Option<String>, Option<String>)> = sqlx::query_as("SELECT name, email FROM users WHERE id = $1")
        .bind(o.user_id)
        .fetch_optional(&s.pool)
        .await?;
    let (name, email) = cust.unwrap_or((None, None));
    let lines: Vec<Value> = o
        .items
        .iter()
        .map(|it| {
            json!({
                "sku": it.sku, "name": it.name, "quantity": it.quantity, "price": it.price,
                "line_total": round2(it.price.unwrap_or(0.0) * it.quantity.unwrap_or(0) as f64),
            })
        })
        .collect();
    Ok(Json(json!({
        "order_no": o.order_no,
        "customer": { "name": name, "email": email },
        "lines": lines,
        "total": o.total,
        "status": o.status,
    })))
}

pub async fn cancel(State(s): State<AppState>, user: AuthUser, Path(id): Path<String>) -> AppResult<Json<Order>> {
    let id = parse_id(&id)?;
    let mut tx = s.pool.begin().await?;
    let row: Option<(Option<i32>, Option<String>, Option<i32>)> =
        sqlx::query_as("SELECT user_id, status, order_no FROM orders WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((owner, status, order_no)) = row.filter(|r| user.can_access(r.0)) else {
        return Err(AppError::not_found());
    };
    if status.as_deref() == Some("cancelled") {
        return Err(AppError::conflict("already cancelled"));
    }
    if payment_in_flight(&mut tx, id).await? {
        return Err(AppError::conflict("payment in progress"));
    }
    cancel_locked(&mut tx, id, owner).await?;
    tx.commit().await?;
    jobs::email(&format!("Order {} cancelled", order_no.unwrap_or_default()));
    Ok(Json(fetch_order(&s.pool, id).await?.ok_or_else(AppError::not_found)?))
}

pub async fn refund(
    State(s): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    JsonBody(data): JsonBody,
) -> AppResult<Json<Order>> {
    let id = parse_id(&id)?;
    let mut tx = s.pool.begin().await?;
    let row: Option<(Option<i32>, Option<String>, Option<f64>)> =
        sqlx::query_as("SELECT user_id, status, refunded FROM orders WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((owner, status, refunded)) = row.filter(|r| user.can_access(r.0)) else {
        return Err(AppError::not_found());
    };
    match status.as_deref() {
        Some("pending") | None => return Err(AppError::conflict("order not paid")),
        Some("cancelled") => return Err(AppError::conflict("order cancelled")),
        _ => {}
    }
    let remaining = round2(amount_received(&mut tx, id).await? - refunded.unwrap_or(0.0));
    if remaining <= 0.0 {
        return Err(AppError::conflict("order already fully refunded"));
    }
    let amount = match data.get("amount") {
        None | Some(Value::Null) => remaining,
        Some(v) => parse_money(Some(v)).ok_or_else(|| AppError::bad_request("invalid amount"))?,
    };
    let amount = round2(amount);
    if !finite_positive(amount) || amount > remaining {
        return Err(AppError::bad_request("invalid refund amount"));
    }
    credit_wallet(&mut tx, owner, amount).await?;
    sqlx::query(
        "UPDATE orders SET status = 'refunded', refunded = ROUND((COALESCE(refunded, 0) + $1)::numeric, 2)::float8 WHERE id = $2",
    )
    .bind(amount)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(fetch_order(&s.pool, id).await?.ok_or_else(AppError::not_found)?))
}

#[cfg(test)]
mod tests {
    use super::calc_total;

    #[test]
    fn totals_match_python() {
        assert_eq!(calc_total(179.98, 10), 161.98);
        assert_eq!(calc_total(89.99 * 2.0, 0), 179.98);
        assert_eq!(calc_total(100.0, 50), 50.0);
    }
}
