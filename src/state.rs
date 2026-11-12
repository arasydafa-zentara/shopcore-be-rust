use std::sync::Arc;

use sqlx::PgPool;

use crate::{config::Config, jobs::Notifier};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub cfg: Arc<Config>,
    pub http: reqwest::Client,
    pub notifier: Notifier,
}
