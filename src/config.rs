use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub bank_url: String,
    pub bank_secret: String,
    pub export_dir: String,
    pub admin_user: String,
    pub admin_password: String,
    pub port: u16,
    pub db_max_connections: u32,
    /// When true, notify_url targets that resolve to loopback/private addresses are refused.
    pub notify_block_private: bool,
    /// Re-hash legacy MD5 passwords with argon2id at login. Turn off to keep the legacy service able to log users in.
    pub upgrade_legacy_hashes: bool,
}

fn var(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

impl Config {
    pub fn from_env() -> Config {
        Config {
            database_url: var("DATABASE_URL", "postgresql://shop:shop@localhost:5432/shop"),
            bank_url: var("BANK_URL", "http://localhost:5001")
                .trim_end_matches('/')
                .to_string(),
            bank_secret: var("BANK_SECRET", "bank-shared-secret-2019"),
            export_dir: var("EXPORT_DIR", "/tmp/exports"),
            admin_user: var("ADMIN_USER", "admin"),
            admin_password: var("ADMIN_PASSWORD", "admin123"),
            port: var("PORT", "8000").parse().unwrap_or(8000),
            db_max_connections: var("DB_MAX_CONNECTIONS", "20").parse().unwrap_or(20),
            notify_block_private: matches!(
                var("NOTIFY_BLOCK_PRIVATE", "false").to_lowercase().as_str(),
                "1" | "true" | "yes"
            ),
            upgrade_legacy_hashes: !matches!(
                var("UPGRADE_LEGACY_HASHES", "true").to_lowercase().as_str(),
                "0" | "false" | "no"
            ),
        }
    }
}
