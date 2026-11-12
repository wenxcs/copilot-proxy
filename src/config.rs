//! Configuration and file path management.

use crate::error::Error;
use std::fs::{self, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Get the path to the GitHub token file
pub fn token_path() -> PathBuf {
    dirs::home_dir()
        .expect("No home directory")
        .join(".local/share/copilot-api-proxy/github_token")
}

/// Get the private API key file used by installed services.
pub fn proxy_api_key_path() -> PathBuf {
    token_path().with_file_name("proxy_api_key")
}

/// Load GitHub token from environment or file
pub fn load_github_token() -> Result<String, Error> {
    std::env::var("GITHUB_TOKEN").or_else(|_| {
        let path = token_path();
        fs::read_to_string(&path)
            .map(|s| s.trim().to_string())
            .map_err(|_| Error::Config(format!("Token not found at {:?}", path)))
    })
}

/// Resolve the optional client API key, preferring the CLI over the environment.
pub fn resolve_proxy_api_key(api_key: Option<String>) -> Result<Option<String>, Error> {
    let api_key = match api_key {
        Some(key) => Some(key),
        None => match std::env::var("COPILOT_PROXY_API_KEY") {
            Ok(key) => Some(key),
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(Error::Config(
                    "COPILOT_PROXY_API_KEY must contain only ASCII letters, digits, or -._~+/="
                        .to_string(),
                ));
            }
        },
    };

    if let Some(key) = &api_key
        && (key.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._~+/=".contains(&byte)))
    {
        return Err(Error::Config(
            "Proxy API key must be nonempty and contain only ASCII letters, digits, or -._~+/="
                .to_string(),
        ));
    }

    Ok(api_key)
}

/// Load an explicitly configured key file without silently disabling authentication.
pub fn load_proxy_api_key_file(path: &Path) -> Result<String, Error> {
    let contents = fs::read_to_string(path).map_err(|error| {
        Error::Config(format!(
            "Could not read proxy API key file {}: {error}",
            path.display()
        ))
    })?;
    let key = contents.trim_end_matches(['\r', '\n']).to_string();
    resolve_proxy_api_key(Some(key))?
        .ok_or_else(|| Error::Config("Proxy API key file must contain a nonempty key".to_string()))
}

/// Ensure token directory exists with secure permissions
pub fn ensure_token_dir() -> Result<(), Error> {
    if let Some(parent) = token_path().parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write token with secure permissions (0600)
pub fn write_token(path: &Path, token: &str) -> Result<(), Error> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    // Restrict an existing file before truncating it or writing any secret.
    file.set_permissions(Permissions::from_mode(0o600))?;
    file.set_len(0)?;
    file.write_all(token.as_bytes())?;
    Ok(())
}

/// Load VSCode device ID from the system-specific path, or generate one.
pub fn load_vscode_device_id() -> String {
    let path = vscode_device_id_path();
    match fs::read_to_string(&path) {
        Ok(id) => {
            let id = id.trim().to_string();
            if !id.is_empty() {
                return id;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // File doesn't exist — generate and persist below
        }
        Err(_) => {
            // Other read error — return ephemeral ID
            return uuid::Uuid::new_v4().to_string();
        }
    }

    let id = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::write(&path, &id).is_err() {
        tracing::warn!("Failed to persist device ID to {:?}", path);
    }
    id
}

/// Load VSCode machine ID from the system-specific path, or generate one.
/// This is sent as the `vscode-machineid` header to the Copilot API.
pub fn load_vscode_machine_id() -> String {
    let path = vscode_machine_id_path();
    match fs::read_to_string(&path) {
        Ok(id) => {
            let id = id.trim().to_string();
            if !id.is_empty() {
                return id;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            return uuid::Uuid::new_v4().to_string();
        }
    }

    let id = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::write(&path, &id).is_err() {
        tracing::warn!("Failed to persist machine ID to {:?}", path);
    }
    id
}

fn vscode_machine_id_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        dirs::home_dir()
            .expect("No home directory")
            .join("Library/Application Support/Microsoft/DeveloperTools/machineid")
    } else {
        dirs::cache_dir()
            .expect("No cache directory")
            .join("Microsoft/DeveloperTools/machineid")
    }
}

fn vscode_device_id_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        dirs::home_dir()
            .expect("No home directory")
            .join("Library/Application Support/Microsoft/DeveloperTools/deviceid")
    } else {
        dirs::cache_dir()
            .expect("No cache directory")
            .join("Microsoft/DeveloperTools/deviceid")
    }
}

#[cfg(test)]
mod tests {
    use super::{load_proxy_api_key_file, resolve_proxy_api_key, write_token};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "copilot-proxy-config-test-{}",
                uuid::Uuid::new_v4()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn accepts_explicit_proxy_api_key() {
        let key = "sk-proxy_example-123.AZaz09~+/=";
        assert_eq!(
            resolve_proxy_api_key(Some(key.to_string())).unwrap(),
            Some(key.to_string())
        );
    }

    #[test]
    fn rejects_empty_or_unusable_proxy_api_keys_without_echoing_them() {
        for key in [
            "", " ", " secret", "secret ", "sec ret", "sec\tret", "sec\nret", "sec\0ret", "秘密",
            "sec%ret", "sec\"ret", "sec\\ret", "sec$ret", "sec,ret", "sec:ret",
        ] {
            let error = resolve_proxy_api_key(Some(key.to_string()))
                .unwrap_err()
                .to_string();
            assert!(error.contains("Proxy API key must be nonempty"));
            if !key.trim().is_empty() {
                assert!(!error.contains(key));
            }
        }
    }

    #[test]
    fn loads_key_files_with_optional_trailing_line_endings() {
        let temp = TempDir::new();
        let path = temp.file("proxy_api_key");
        let key = "sk-file_example-123~+/=";
        for ending in ["", "\n", "\r\n", "\r\n\r\n"] {
            fs::write(&path, format!("{key}{ending}")).unwrap();
            assert_eq!(load_proxy_api_key_file(&path).unwrap(), key);
        }
    }

    #[test]
    fn rejects_missing_empty_and_invalid_key_files_without_echoing_contents() {
        let temp = TempDir::new();
        let path = temp.file("proxy_api_key");
        let error = load_proxy_api_key_file(&path).unwrap_err().to_string();
        assert!(error.contains("Could not read proxy API key file"));

        for contents in [
            "",
            "\r\n",
            " sk-file-secret",
            "sk-file-secret \n",
            "sk-file\nsecret",
            "sk-file%secret",
        ] {
            fs::write(&path, contents).unwrap();
            let error = load_proxy_api_key_file(&path).unwrap_err().to_string();
            assert!(error.contains("Proxy API key must be nonempty"));
            assert!(!error.contains("sk-file"));
        }

        fs::write(&path, b"sk-file-secret\xff").unwrap();
        let error = load_proxy_api_key_file(&path).unwrap_err().to_string();
        assert!(error.contains("Could not read proxy API key file"));
        assert!(!error.contains("sk-file-secret"));
    }

    #[test]
    fn writes_new_token_files_with_private_permissions() {
        let temp = TempDir::new();
        let path = temp.file("new_token");
        write_token(&path, "sk-new-secret").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "sk-new-secret");
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn restricts_existing_token_files_and_replaces_their_contents() {
        let temp = TempDir::new();
        let path = temp.file("existing_token");
        fs::write(&path, "old-secret-with-longer-contents").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        write_token(&path, "sk-new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "sk-new");
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
