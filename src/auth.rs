use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use axum::{extract::FromRequestParts, http::request::Parts};
use md5::{Digest as _, Md5};
use serde::Serialize;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::{error::AppError, state::AppState};

/// The authenticated caller, as loaded from the sessions table.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AuthUser {
    pub id: i32,
    pub username: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
    pub role: Option<String>,
    pub wallet_balance: Option<f64>,
    pub token_hash: String,
}

#[derive(Serialize)]
pub struct PublicUser {
    pub id: i32,
    pub username: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
    pub role: Option<String>,
    pub wallet_balance: Option<f64>,
}

impl AuthUser {
    pub fn is_admin(&self) -> bool {
        self.role.as_deref() == Some("admin")
    }
    /// Owners and admins may act on an order; everyone else gets a 404 (does not reveal existence).
    pub fn can_access(&self, owner: Option<i32>) -> bool {
        self.is_admin() || owner == Some(self.id)
    }
    pub fn public(&self) -> PublicUser {
        PublicUser {
            id: self.id,
            username: self.username.clone(),
            email: self.email.clone(),
            name: self.name.clone(),
            role: self.role.clone(),
            wallet_balance: Some(self.wallet_balance.unwrap_or(0.0)),
        }
    }
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn new_token() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}

fn md5_hex(p: &str) -> String {
    hex::encode(Md5::digest(p.as_bytes()))
}

/// Argon2id hash for new passwords (random salt, default OWASP-grade parameters).
/// CPU heavy: call through `spawn_blocking`.
pub fn hash_password(p: &str) -> String {
    let hash: Result<PasswordHash, _> = Argon2::default().hash_password(p.as_bytes());
    hash.map(|h| h.to_string()).unwrap_or_default()
}

/// Verify against either an argon2 hash or a legacy unsalted MD5 hex digest (existing production rows).
/// Returns (matches, needs_upgrade).
pub fn verify_password(p: &str, stored: &str) -> (bool, bool) {
    if stored.starts_with("$argon2") {
        (Argon2::default().verify_password(p.as_bytes(), stored).is_ok(), false)
    } else {
        let ok = md5_hex(p).as_bytes().ct_eq(stored.as_bytes()).into();
        (ok, ok)
    }
}

/// Verification against a throwaway hash so unknown usernames cost the same time as wrong passwords.
pub fn dummy_verify(p: &str) {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let h = DUMMY.get_or_init(|| hash_password("shopcore-dummy"));
    let _ = verify_password(p, h);
}

fn bearer(parts: &Parts) -> Option<String> {
    let h = parts.headers.get("authorization")?.to_str().ok()?;
    let (scheme, token) = h.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.trim().is_empty() {
        return None;
    }
    Some(token.trim().to_string())
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let token = bearer(parts).ok_or_else(AppError::unauthorized)?;
        let th = hash_token(&token);
        let user = sqlx::query_as::<_, AuthUser>(
            "SELECT u.id, u.username, u.email, u.name, u.role, u.wallet_balance, s.token_hash \
             FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.token_hash = $1 AND s.created_at > now() - interval '30 days'",
        )
        .bind(&th)
        .fetch_optional(&state.pool)
        .await?;
        user.ok_or_else(AppError::unauthorized)
    }
}

/// Admin-only routes: no/invalid token -> 401, valid non-admin -> 403 (legacy let anonymous callers through).
#[allow(dead_code)]
pub struct AdminUser(pub AuthUser);

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let u = AuthUser::from_request_parts(parts, state).await?;
        if !u.is_admin() {
            return Err(AppError::forbidden());
        }
        Ok(AdminUser(u))
    }
}
