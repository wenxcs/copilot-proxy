//! Environment and private-file configuration for model access and token limits.

use crate::config;
use crate::error::Error;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

// Deliberately omit Debug: the service configuration contains an admin secret.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    pub rules: String,
    pub super_password: Option<String>,
    pub state_path: Option<PathBuf>,
}

impl PolicyConfig {
    /// An explicit file supplies the complete configuration, overriding the environment.
    pub fn load(file: Option<&Path>) -> Result<Self, Error> {
        let configuration = match file {
            Some(path) => {
                let contents = fs::read_to_string(path).map_err(|error| {
                    Error::Config(format!(
                        "Could not read model policy configuration file {}: {error}",
                        path.display()
                    ))
                })?;
                serde_json::from_str(&contents).map_err(|_| {
                    Error::Config(format!(
                        "Invalid model policy configuration file {}",
                        path.display()
                    ))
                })?
            }
            None => Self::from_values(
                environment_value("COPILOT_PROXY_MODEL_RULES")?,
                environment_value("COPILOT_PROXY_SUPER_PASSWORD")?,
                environment_value("COPILOT_PROXY_LIMIT_STATE_PATH")?,
            )?,
        };
        Self::validate(configuration)
    }

    fn from_values(
        rules: Option<String>,
        super_password: Option<String>,
        state_path: Option<String>,
    ) -> Result<Self, Error> {
        Self::validate(Self {
            rules: rules.unwrap_or_default(),
            super_password,
            state_path: state_path.map(PathBuf::from),
        })
    }

    pub fn validate(self) -> Result<Self, Error> {
        if let Some(password) = &self.super_password
            && (password.is_empty() || !password.bytes().all(|byte| byte.is_ascii_graphic()))
        {
            return Err(Error::Config(
                "COPILOT_PROXY_SUPER_PASSWORD must be nonempty and contain only printable ASCII without whitespace".to_string(),
            ));
        }
        if let Some(path) = &self.state_path
            && !path.is_absolute()
        {
            return Err(Error::Config(
                "COPILOT_PROXY_LIMIT_STATE_PATH must be an absolute file path".to_string(),
            ));
        }
        // Validate rules without opening the live usage ledger (including during service install).
        crate::model_policy::ModelPolicy::new(&self.rules, None)?;
        Ok(self)
    }

    pub fn state_path(&self) -> PathBuf {
        self.state_path
            .clone()
            .unwrap_or_else(|| config::token_path().with_file_name("model_usage.json"))
    }

    pub fn is_configured(&self) -> bool {
        !self.rules.trim().is_empty() || self.super_password.is_some() || self.state_path.is_some()
    }

    /// Keep the admin password out of public service definitions and process arguments.
    pub fn save_for_service(&self) -> Result<PathBuf, Error> {
        config::ensure_token_dir()?;
        let path = config::token_path().with_file_name("model_policy_config.json");
        self.save_to(&path)?;
        Ok(path)
    }

    fn save_to(&self, path: &Path) -> Result<(), Error> {
        let contents = serde_json::to_string(self).map_err(|_| {
            Error::Config("Could not serialize model policy configuration".to_string())
        })?;
        config::write_token(path, &contents)
    }
}

fn environment_value(name: &str) -> Result<Option<String>, Error> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(Error::Config(format!("{name} must contain valid Unicode")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "copilot-policy-config-test-{}",
                uuid::Uuid::new_v4()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn file(&self) -> PathBuf {
            self.0.join("policy.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn unconfigured_values_preserve_default_behavior() {
        let config = PolicyConfig::from_values(None, None, None).unwrap();
        assert!(config.rules.is_empty());
        assert!(config.super_password.is_none());
        assert!(!config.is_configured());
    }

    #[test]
    fn validates_secrets_without_echoing_them() {
        for password in ["", " ", "with space", "new\nline", "秘密", "tab\tsecret"] {
            let error = PolicyConfig::from_values(None, Some(password.to_string()), None)
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("COPILOT_PROXY_SUPER_PASSWORD must be nonempty"));
            if !password.trim().is_empty() {
                assert!(!error.contains(password));
            }
        }
        let config =
            PolicyConfig::from_values(None, Some("admin!$%+secret".to_string()), None).unwrap();
        assert!(config.is_configured());
    }

    #[test]
    fn requires_absolute_state_file_paths() {
        for path in ["", "relative.json", "../usage.json", "~/usage.json"] {
            assert!(PolicyConfig::from_values(None, None, Some(path.to_string())).is_err());
        }
        let temp = TempDir::new();
        let path = temp.0.join("usage.json");
        let config =
            PolicyConfig::from_values(None, None, Some(path.to_str().unwrap().to_string()))
                .unwrap();
        assert_eq!(config.state_path(), path);
    }

    #[test]
    fn loads_explicit_policy_file_and_persists_private_service_configuration() {
        let temp = TempDir::new();
        let file = temp.file();
        let config = PolicyConfig::from_values(
            Some("-all,+gpt-6-astra,5h:gpt-6-astra:1024".to_string()),
            Some("admin-secret".to_string()),
            Some(temp.0.join("usage.json").to_str().unwrap().to_string()),
        )
        .unwrap();
        config.save_to(&file).unwrap();
        let loaded = PolicyConfig::load(Some(&file)).unwrap();
        assert_eq!(loaded.rules, config.rules);
        assert_eq!(loaded.super_password, config.super_password);
        assert_eq!(loaded.state_path, config.state_path);
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::write(&file, "{}").unwrap();
        let empty = PolicyConfig::load(Some(&file)).unwrap();
        assert!(!empty.is_configured());
    }

    #[test]
    fn rejects_missing_malformed_unknown_and_invalid_file_values() {
        let temp = TempDir::new();
        let file = temp.file();
        assert!(PolicyConfig::load(Some(&file)).is_err());
        for contents in [
            r#"{"super_password":"admin-secret""#,
            r#"{"super_password":"secret with space"}"#,
            r#"{"unknown_field":"secret-value"}"#,
            r#"{"state_path":"relative.json"}"#,
        ] {
            fs::write(&file, contents).unwrap();
            let error = PolicyConfig::load(Some(&file)).err().unwrap().to_string();
            assert!(!error.contains("admin-secret"));
            assert!(!error.contains("secret with space"));
            assert!(!error.contains("secret-value"));
        }
    }
}
