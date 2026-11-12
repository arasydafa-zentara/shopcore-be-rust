use axum::{Json, extract::State};
use serde_json::{Value, json};

use crate::{
    auth::{self, AuthUser, PublicUser},
    error::{AppError, AppResult},
    state::AppState,
    util::{JsonBody, clean, py_str},
};

pub async fn register(
    State(s): State<AppState>,
    JsonBody(data): JsonBody,
) -> AppResult<(axum::http::StatusCode, Json<Value>)> {
    let username = clean(data.get("username"));
    let email = clean(data.get("email"));
    let password = py_str(data.get("password")).unwrap_or_default();
    if username.is_empty() || email.is_empty() || password.is_empty() {
        return Err(AppError::bad_request("username, email and password are required"));
    }
    let exists: Option<(i32,)> = sqlx::query_as("SELECT id FROM users WHERE username = $1 LIMIT 1")
        .bind(&username)
        .fetch_optional(&s.pool)
        .await?;
    if exists.is_some() {
        return Err(AppError::conflict("username taken"));
    }
    let exists: Option<(i32,)> = sqlx::query_as("SELECT id FROM users WHERE email = $1 LIMIT 1")
        .bind(&email)
        .fetch_optional(&s.pool)
        .await?;
    if exists.is_some() {
        return Err(AppError::conflict("email taken"));
    }
    let name = py_str(data.get("name"))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| username.clone());
    let hash = tokio::task::spawn_blocking(move || auth::hash_password(&password))
        .await
        .map_err(|_| AppError::with(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
    let (id,): (i32,) = sqlx::query_as(
        "INSERT INTO users (username, email, name, password_hash, role, wallet_balance) \
         VALUES ($1, $2, $3, $4, 'user', 0) RETURNING id",
    )
    .bind(&username)
    .bind(&email)
    .bind(&name)
    .bind(&hash)
    .fetch_one(&s.pool)
    .await?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({ "id": id, "username": username })),
    ))
}

pub async fn login(State(s): State<AppState>, JsonBody(data): JsonBody) -> AppResult<Json<Value>> {
    let username = py_str(data.get("username"));
    let password = py_str(data.get("password")).unwrap_or_default();
    // Brute-force guard: too many recent failures for this name -> 429 until the window passes.
    let throttle_key: String = username.clone().unwrap_or_default().chars().take(100).collect();
    let (recent,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM login_failures WHERE username = $1 AND failed_at > now() - interval '60 seconds'",
    )
    .bind(&throttle_key)
    .fetch_one(&s.pool)
    .await?;
    if recent >= MAX_FAILED_LOGINS {
        return Err(AppError::with(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "too many attempts, try again later",
        ));
    }
    let row: Option<(i32, Option<String>)> = match &username {
        Some(u) => {
            sqlx::query_as("SELECT id, password_hash FROM users WHERE username = $1 ORDER BY id LIMIT 1")
                .bind(u)
                .fetch_optional(&s.pool)
                .await?
        }
        None => None,
    };
    let Some((uid, stored)) = row else {
        let _ = tokio::task::spawn_blocking(move || auth::dummy_verify(&password)).await;
        return Err(login_failed(&s, &throttle_key).await);
    };
    let stored = stored.unwrap_or_default();
    let pw = password.clone();
    let (ok, upgrade) = tokio::task::spawn_blocking(move || auth::verify_password(&pw, &stored))
        .await
        .unwrap_or((false, false));
    if !ok {
        return Err(login_failed(&s, &throttle_key).await);
    }
    let _ = sqlx::query("DELETE FROM login_failures WHERE username = $1")
        .bind(&throttle_key)
        .execute(&s.pool)
        .await;
    if upgrade && s.cfg.upgrade_legacy_hashes {
        // Legacy MD5 row: transparently move it to argon2id now that we know the password.
        if let Ok(new_hash) = tokio::task::spawn_blocking(move || auth::hash_password(&password)).await {
            let _ = sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
                .bind(new_hash)
                .bind(uid)
                .execute(&s.pool)
                .await;
        }
    }
    let token = auth::new_token();
    sqlx::query("INSERT INTO sessions (token_hash, user_id) VALUES ($1, $2)")
        .bind(auth::hash_token(&token))
        .bind(uid)
        .execute(&s.pool)
        .await?;
    Ok(Json(json!({ "token": token })))
}

const MAX_FAILED_LOGINS: i64 = 10;

/// Record a failed attempt (and drop old records) and produce the 401.
async fn login_failed(s: &AppState, key: &str) -> AppError {
    let _ = sqlx::query("INSERT INTO login_failures (username) VALUES ($1)")
        .bind(key)
        .execute(&s.pool)
        .await;
    let _ = sqlx::query("DELETE FROM login_failures WHERE failed_at < now() - interval '10 minutes'")
        .execute(&s.pool)
        .await;
    AppError::with(axum::http::StatusCode::UNAUTHORIZED, "invalid credentials")
}

pub async fn logout(State(s): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
        .bind(&user.token_hash)
        .execute(&s.pool)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn me(user: AuthUser) -> Json<PublicUser> {
    Json(user.public())
}

/// Only `name` and `email` can be changed here. (The legacy handler built SQL from the JSON keys,
/// which allowed injection and self-promotion to admin.)
pub async fn update_me(
    State(s): State<AppState>,
    user: AuthUser,
    JsonBody(data): JsonBody,
) -> AppResult<Json<PublicUser>> {
    if let Some(v) = data.get("email").filter(|v| !v.is_null()) {
        let email = py_str(Some(v)).map(|e| e.trim().to_string()).unwrap_or_default();
        if email.is_empty() {
            return Err(AppError::bad_request("email must not be empty"));
        }
        let taken: Option<(i32,)> = sqlx::query_as("SELECT id FROM users WHERE email = $1 AND id <> $2 LIMIT 1")
            .bind(&email)
            .bind(user.id)
            .fetch_optional(&s.pool)
            .await?;
        if taken.is_some() {
            return Err(AppError::conflict("email taken"));
        }
        sqlx::query("UPDATE users SET email = $1 WHERE id = $2")
            .bind(&email)
            .bind(user.id)
            .execute(&s.pool)
            .await?;
    }
    if let Some(name) = data.get("name").filter(|v| !v.is_null()).and_then(|v| py_str(Some(v))) {
        sqlx::query("UPDATE users SET name = $1 WHERE id = $2")
            .bind(&name)
            .bind(user.id)
            .execute(&s.pool)
            .await?;
    }
    let u = sqlx::query_as::<_, AuthUser>(
        "SELECT id, username, email, name, role, wallet_balance, '' AS token_hash FROM users WHERE id = $1",
    )
    .bind(user.id)
    .fetch_one(&s.pool)
    .await?;
    Ok(Json(u.public()))
}
