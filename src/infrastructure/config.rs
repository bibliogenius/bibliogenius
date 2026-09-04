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
        Self::from_env_with_profile(None)
    }

    /// Build the config from the environment, with `profile` (the `--profile`
    /// CLI flag) taking precedence over the `PROFILE` variable. The database
    /// URL follows the profile unless `DATABASE_URL` is set explicitly.
    pub fn from_env_with_profile(profile: Option<&str>) -> Self {
        let profile = profile
            .map(str::to_owned)
            .or_else(|| env::var("PROFILE").ok())
            .unwrap_or_else(|| "default".to_string());

        let database_url =
            env::var("DATABASE_URL").unwrap_or_else(|_| Self::default_database_url(&profile));

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
                .unwrap_or_default(),
            profile,
        }
    }

    fn default_database_url(profile: &str) -> String {
        if profile == "default" {
            "sqlite://bibliogenius.db?mode=rwc".to_string()
        } else {
            format!("sqlite://bibliogenius_{}.db?mode=rwc", profile)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_argument_overrides_the_environment() {
        let config = Config::from_env_with_profile(Some("qa"));
        assert_eq!(config.profile, "qa");
    }

    #[test]
    fn database_url_follows_the_profile() {
        assert_eq!(
            Config::default_database_url("default"),
            "sqlite://bibliogenius.db?mode=rwc"
        );
        assert_eq!(
            Config::default_database_url("qa"),
            "sqlite://bibliogenius_qa.db?mode=rwc"
        );
    }
}
