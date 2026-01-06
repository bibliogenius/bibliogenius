use std::env;

#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub port: u16,
    pub hub_url: Option<String>,
    pub cors_allowed_origins: Vec<String>,
    pub profile: String,
}

impl Config {
    pub fn from_env() -> Self {
        let profile = env::var("PROFILE").unwrap_or_else(|_| "default".to_string());

        let database_url = env::var("DATABASE_URL").unwrap_or_else(|_| {
            if profile == "default" {
                "sqlite://bibliogenius.db?mode=rwc".to_string()
            } else {
                format!("sqlite://bibliogenius_{}.db?mode=rwc", profile)
            }
        });

        Self {
            database_url,
            port: env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(8000),
            hub_url: env::var("HUB_URL").ok(),
            cors_allowed_origins: env::var("CORS_ALLOWED_ORIGINS")
                .ok()
                .map(|s| s.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_else(Vec::new),
            profile,
        }
    }
}
