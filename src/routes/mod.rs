use axum::{
    Json, Router,
    http::{StatusCode, Uri},
    routing::{get, post},
};
use serde_json::{Value, json};

use crate::{error::AppError, state::AppState};

pub mod admin;
pub mod orders;
pub mod payments;
pub mod products;
pub mod reports;
pub mod users;
pub mod wallet;
pub mod webhooks;

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn not_found(_uri: Uri) -> AppError {
    AppError::not_found()
}

async fn method_not_allowed() -> AppError {
    AppError::with(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/register", post(users::register))
        .route("/api/login", post(users::login))
        .route("/api/logout", post(users::logout))
        .route("/api/users/me", get(users::me).patch(users::update_me))
        .route("/api/products", get(products::list).post(products::create))
        .route("/api/products/{id}", get(products::get_one).put(products::update))
        .route("/api/orders", post(orders::create).get(orders::list))
        .route("/api/orders/{id}", get(orders::get_one))
        .route("/api/orders/{id}/invoice", get(orders::invoice))
        .route("/api/orders/{id}/cancel", post(orders::cancel))
        .route("/api/orders/{id}/refund", post(orders::refund))
        .route("/api/orders/{id}/pay", post(payments::pay))
        .route("/api/wallet", get(wallet::get_wallet))
        .route("/api/wallet/topup", post(wallet::topup))
        .route("/api/wallet/transfer", post(wallet::transfer))
        .route("/api/reports/sales", get(reports::sales))
        .route("/api/reports/export", post(reports::export))
        .route("/api/reports/download", get(reports::download))
        .route("/api/admin/users", get(admin::list_users))
        .route("/api/admin/users/{id}/role", post(admin::set_role))
        .route("/api/admin/orders/{id}/status", post(admin::set_order_status))
        .route("/api/webhooks/bank", post(webhooks::bank))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}
