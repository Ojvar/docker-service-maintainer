//! Application configuration loaded from `.env` and the process environment.

use std::{env, path::PathBuf};

/// Runtime settings for the server.
#[derive(Clone, Debug)]
pub struct Config {
    /// Interface to bind to (`HOST`, default `0.0.0.0`).
    pub host: String,
    /// TCP port to listen on (`PORT`, required).
    pub port: u16,
    /// Shared secret every request must send in `x-security-token` (`SECURITY_TOKEN`, required).
    pub security_token: String,
    /// Directory holding the `<service>.sh` scripts (`SCRIPTS_DIR`, default `scripts`).
    pub scripts_dir: PathBuf,
}

impl Config {
    /// Loads `.env` (if present) and builds the config from environment variables.
    ///
    /// Returns a human-readable error when a required value is missing or invalid.
    pub fn from_env() -> Result<Self, String> {
        dotenvy::dotenv().ok();
        Self::from_lookup(|key| env::var(key).ok())
    }

    /// Builds the config from any key lookup. Kept separate from [`Config::from_env`]
    /// so it can be tested without touching the real process environment.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let required = |key: &str| get(key).ok_or_else(|| format!("missing required env var: {key}"));

        let security_token = required("SECURITY_TOKEN")?;
        if security_token.trim().is_empty() {
            return Err("SECURITY_TOKEN must not be empty".into());
        }

        Ok(Self {
            host: get("HOST").unwrap_or_else(|| "0.0.0.0".into()),
            port: required("PORT")?
                .parse()
                .map_err(|_| "PORT must be a valid number".to_string())?,
            security_token,
            scripts_dir: get("SCRIPTS_DIR").unwrap_or_else(|| "scripts".into()).into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn load(vars: &[(&str, &str)]) -> Result<Config, String> {
        let map: HashMap<String, String> =
            vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn loads_all_values() {
        let config = load(&[
            ("HOST", "127.0.0.1"),
            ("PORT", "9000"),
            ("SECURITY_TOKEN", "secret"),
            ("SCRIPTS_DIR", "/opt/scripts"),
        ])
        .unwrap();

        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 9000);
        assert_eq!(config.security_token, "secret");
        assert_eq!(config.scripts_dir, PathBuf::from("/opt/scripts"));
    }

    #[test]
    fn applies_defaults() {
        let config = load(&[("PORT", "8080"), ("SECURITY_TOKEN", "secret")]).unwrap();
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.scripts_dir, PathBuf::from("scripts"));
    }

    #[test]
    fn rejects_missing_token() {
        assert!(load(&[("PORT", "8080")]).unwrap_err().contains("SECURITY_TOKEN"));
    }

    #[test]
    fn rejects_blank_token() {
        assert!(load(&[("PORT", "8080"), ("SECURITY_TOKEN", "  ")]).is_err());
    }

    #[test]
    fn rejects_missing_or_invalid_port() {
        assert!(load(&[("SECURITY_TOKEN", "s")]).unwrap_err().contains("PORT"));
        assert!(load(&[("PORT", "abc"), ("SECURITY_TOKEN", "s")]).is_err());
        assert!(load(&[("PORT", "70000"), ("SECURITY_TOKEN", "s")]).is_err());
    }
}
