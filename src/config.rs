use std::{env, path::PathBuf};

/// Application settings, loaded from `.env` / environment variables.
#[derive(Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub security_token: String,
    pub scripts_dir: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        dotenvy::dotenv().ok();

        let security_token = required("SECURITY_TOKEN")?;
        if security_token.trim().is_empty() {
            return Err("SECURITY_TOKEN must not be empty".into());
        }

        Ok(Self {
            host: env::var("HOST").unwrap_or_else(|_| "0.0.0.0".into()),
            port: required("PORT")?
                .parse()
                .map_err(|_| "PORT must be a valid number".to_string())?,
            security_token,
            scripts_dir: env::var("SCRIPTS_DIR")
                .unwrap_or_else(|_| "scripts".into())
                .into(),
        })
    }
}

fn required(key: &str) -> Result<String, String> {
    env::var(key).map_err(|_| format!("missing required env var: {key}"))
}
