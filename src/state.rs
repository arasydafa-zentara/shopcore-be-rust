use std::sync::Arc;

use sqlx::PgPool;

use crate::{config::Config, jobs::Notifier};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub cfg: Arc<Config>,
    pub http: reqwest::Client,
    pub notifier: Notifier,
    /// Bounds concurrent password hashing: every argon2 run takes ~19 MiB, so without a ceiling a burst of logins or
    /// registrations (no account needed) could exhaust memory. Excess requests queue instead.
    pub hash_gate: Arc<tokio::sync::Semaphore>,
}
