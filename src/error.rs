use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

/// Every error leaves the service as `{"error": "<message>"}` (see openapi.yaml conventions).
#[derive(Debug)]
pub struct AppError {
    pub status: StatusCode,
    pub message: String,
}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    fn new(status: StatusCode, message: impl Into<String>) -> AppError {
        AppError { status, message: message.into() }
    }
    pub fn bad_request(m: impl Into<String>) -> AppError {
        AppError::new(StatusCode::BAD_REQUEST, m)
    }
    pub fn unauthorized() -> AppError {
        AppError::new(StatusCode::UNAUTHORIZED, "unauthorized")
    }
    pub fn forbidden() -> AppError {
        AppError::new(StatusCode::FORBIDDEN, "forbidden")
    }
    pub fn not_found() -> AppError {
        AppError::new(StatusCode::NOT_FOUND, "not found")
    }
    pub fn not_found_with(m: impl Into<String>) -> AppError {
        AppError::new(StatusCode::NOT_FOUND, m)
    }
    pub fn conflict(m: impl Into<String>) -> AppError {
        AppError::new(StatusCode::CONFLICT, m)
    }
    pub fn payment_required(m: impl Into<String>) -> AppError {
        AppError::new(StatusCode::PAYMENT_REQUIRED, m)
    }
    pub fn with(status: StatusCode, m: impl Into<String>) -> AppError {
        AppError::new(status, m)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> AppError {
        if let Some(db) = e.as_database_error() {
            match db.code().as_deref() {
                // unique_violation
                Some("23505") => return AppError::conflict("already exists"),
                // data_exception class (bad timestamp text, numeric overflow, ...)
                Some(c) if c.starts_with("22") => return AppError::bad_request("invalid parameter"),
                _ => {}
            }
        }
        // Never leak SQL or stack details to the client (legacy returned a traceback).
        tracing::error!(error = %e, "database error");
        AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> AppError {
        tracing::error!(error = %e, "io error");
        AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}
