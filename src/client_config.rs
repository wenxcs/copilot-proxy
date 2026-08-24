//! User-level Claude Code and Codex configuration patching.

use crate::error::Error;
use serde_json::{Map, Value as JsonValue};
use std::fs::{self, OpenOptions, Permissions};
use std::io::Write;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike, Value as TomlValue, value};

const DEFAULT_PROXY_URL: &str = "http://localhost:9876";
const DEFAULT_CLAUDE_AUTH_TOKEN: &str = "copilot-api-proxy";
const CODEX_PROVIDER_ID: &str = "copilot_api_proxy";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatchStatus {
    Updated,
    Unchanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProxyUrls {
    pub claude: String,
    pub codex: String,
}

pub fn default_proxy_url() -> &'static str {
    DEFAULT_PROXY_URL
}

/// Resolve Claude Code's user settings path, honoring `CLAUDE_CONFIG_DIR`.
pub fn claude_settings_path() -> Result<PathBuf, Error> {
    match config_dir_from_env("CLAUDE_CONFIG_DIR") {
        Some(path) => Ok(path.join("settings.json")),
        None => Ok(home_dir()?.join(".claude/settings.json")),
    }
}

/// Resolve Codex's user configuration path, honoring `CODEX_HOME`.
pub fn codex_config_path() -> Result<PathBuf, Error> {
    match config_dir_from_env("CODEX_HOME") {
        Some(path) => Ok(path.join("config.toml")),
        None => Ok(home_dir()?.join(".codex/config.toml")),
    }
}

/// Validate a proxy URL and derive the protocol-specific client base URLs.
///
/// The proxy itself is rooted at the supplied origin. Claude Code appends
/// `/v1/messages`, while Codex needs an OpenAI-compatible `/v1` base URL.
pub fn proxy_urls(proxy_url: &str) -> Result<ProxyUrls, Error> {
    let mut url = reqwest::Url::parse(proxy_url.trim())
        .map_err(|error| Error::Config(format!("Invalid proxy URL '{proxy_url}': {error}")))?;

    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(Error::Config(
            "Proxy URL must be an absolute http:// or https:// URL".to_string(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::Config(
            "Proxy URL must not contain embedded credentials".to_string(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::Config(
            "Proxy URL must not contain a query string or fragment".to_string(),
        ));
    }

    let path = url.path().trim_end_matches('/');
    if !path.is_empty() && path != "/v1" {
        return Err(Error::Config(
            "Proxy URL path must be empty or /v1".to_string(),
        ));
    }

    url.set_path("");
    let claude = url.as_str().trim_end_matches('/').to_string();
    Ok(ProxyUrls {
        codex: format!("{claude}/v1"),
        claude,
    })
}

/// Patch Claude Code's user settings while preserving unrelated JSON values.
///
/// Credential precedence is `auth_token`, an existing `ANTHROPIC_AUTH_TOKEN`,
/// `fallback_auth_token`, and finally a non-secret local placeholder.
pub fn patch_claude_settings(
    path: &Path,
    proxy_url: &str,
    auth_token: Option<&str>,
    fallback_auth_token: Option<&str>,
    model: Option<&str>,
) -> Result<PatchStatus, Error> {
    let urls = proxy_urls(proxy_url)?;
    let auth_token = nonempty_optional("Claude API key", auth_token)?;
    let fallback_auth_token = nonempty_optional("fallback Claude API key", fallback_auth_token)?;
    let model = nonempty_optional("Claude model", model)?;
    if let Some(model) = model
        && !crate::claude::is_native_claude_model(model)
    {
        return Err(Error::Config(format!(
            "Claude model '{model}' is not a native Claude-family model"
        )));
    }
    let config = read_config(path)?;

    let mut document: JsonValue = if config.contents.trim().is_empty() {
        JsonValue::Object(Map::new())
    } else {
        serde_json::from_str(&config.contents).map_err(|error| {
            Error::Config(format!(
                "Could not parse Claude settings at {}: {error}",
                path.display()
            ))
        })?
    };
    let root = document.as_object_mut().ok_or_else(|| {
        Error::Config(format!(
            "Claude settings at {} must contain a JSON object",
            path.display()
        ))
    })?;

    if !root.contains_key("env") {
        root.insert("env".to_string(), JsonValue::Object(Map::new()));
    }
    let env = root
        .get_mut("env")
        .and_then(JsonValue::as_object_mut)
        .ok_or_else(|| {
            Error::Config(format!(
                "Claude setting 'env' at {} must be a JSON object",
                path.display()
            ))
        })?;

    let existing_auth_token = env
        .get("ANTHROPIC_AUTH_TOKEN")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let resolved_auth_token = auth_token
        .or(existing_auth_token)
        .or(fallback_auth_token)
        .unwrap_or(DEFAULT_CLAUDE_AUTH_TOKEN)
        .to_string();

    env.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        JsonValue::String(urls.claude),
    );
    env.insert(
        "ANTHROPIC_AUTH_TOKEN".to_string(),
        JsonValue::String(resolved_auth_token),
    );
    if let Some(model) = model {
        root.insert("model".to_string(), JsonValue::String(model.to_string()));
    }

    let mut output = serde_json::to_string_pretty(&document)
        .map_err(|error| Error::Config(format!("Could not serialize Claude settings: {error}")))?;
    output.push('\n');
    finish_patch(config, output.as_bytes(), true)
}

/// Patch Codex's user config with a custom Responses API provider.
pub fn patch_codex_config(
    path: &Path,
    proxy_url: &str,
    model: Option<&str>,
) -> Result<PatchStatus, Error> {
    let urls = proxy_urls(proxy_url)?;
    let model = nonempty_optional("Codex model", model)?;
    let config = read_config(path)?;

    let mut document = config.contents.parse::<DocumentMut>().map_err(|error| {
        Error::Config(format!(
            "Could not parse Codex config at {}: {error}",
            path.display()
        ))
    })?;

    let root = document.as_table_mut();
    set_toml_string(root, "model_provider", CODEX_PROVIDER_ID);
    if let Some(model) = model {
        set_toml_string(root, "model", model);
    }

    if !root.contains_key("model_providers") {
        let mut providers = Table::new();
        providers.set_implicit(true);
        root.insert("model_providers", Item::Table(providers));
    }
    let providers_item = root.get_mut("model_providers").expect("inserted above");
    let providers_are_inline = providers_item.is_inline_table();
    let providers = providers_item.as_table_like_mut().ok_or_else(|| {
        Error::Config(format!(
            "Codex setting 'model_providers' at {} must be a TOML table",
            path.display()
        ))
    })?;

    if !providers.contains_key(CODEX_PROVIDER_ID) {
        let provider = if providers_are_inline {
            Item::Value(TomlValue::InlineTable(InlineTable::new()))
        } else {
            Item::Table(Table::new())
        };
        providers.insert(CODEX_PROVIDER_ID, provider);
    }
    let provider = providers
        .get_mut(CODEX_PROVIDER_ID)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| {
            Error::Config(format!(
                "Codex provider '{CODEX_PROVIDER_ID}' at {} must be a TOML table",
                path.display()
            ))
        })?;

    set_toml_string(provider, "name", "GitHub Copilot Proxy");
    set_toml_string(provider, "base_url", &urls.codex);
    set_toml_string(provider, "wire_api", "responses");
    for key in [
        "env_key",
        "env_key_instructions",
        "experimental_bearer_token",
        "requires_openai_auth",
        "auth",
    ] {
        provider.remove(key);
    }

    let output = document.to_string();
    finish_patch(config, output.as_bytes(), false)
}

fn home_dir() -> Result<PathBuf, Error> {
    dirs::home_dir().ok_or_else(|| Error::Config("Could not determine home directory".to_string()))
}

fn config_dir_from_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn nonempty_optional<'a>(label: &str, value: Option<&'a str>) -> Result<Option<&'a str>, Error> {
    match value {
        Some(value) if value.trim().is_empty() => {
            Err(Error::Config(format!("{label} must not be empty")))
        }
        Some(value) => Ok(Some(value.trim())),
        None => Ok(None),
    }
}

fn set_toml_string(table: &mut dyn TableLike, key: &str, expected: &str) {
    if table.get(key).and_then(Item::as_str) != Some(expected) {
        table.insert(key, value(expected));
    }
}

struct ConfigFile {
    target: PathBuf,
    contents: String,
    permissions: Option<Permissions>,
}

fn read_config(path: &Path) -> Result<ConfigFile, Error> {
    let target = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::canonicalize(path).map_err(|error| {
                Error::Config(format!(
                    "Could not resolve config symlink {}: {error}",
                    path.display()
                ))
            })?
        }
        Ok(_) => path.to_path_buf(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(error) => return Err(Error::Io(error)),
    };

    match fs::metadata(&target) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(Error::Config(format!(
                    "Config path {} is not a regular file",
                    path.display()
                )));
            }
            Ok(ConfigFile {
                contents: fs::read_to_string(&target)?,
                target,
                permissions: Some(metadata.permissions()),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ConfigFile {
            target,
            contents: String::new(),
            permissions: None,
        }),
        Err(error) => Err(Error::Io(error)),
    }
}

fn finish_patch(
    config: ConfigFile,
    output: &[u8],
    make_private: bool,
) -> Result<PatchStatus, Error> {
    let ConfigFile {
        target,
        contents,
        permissions,
    } = config;
    let existed = permissions.is_some();
    let (permissions, permissions_changed) = desired_permissions(permissions, make_private);
    let expected_contents = existed.then_some(contents.as_bytes());

    if contents.as_bytes() == output {
        if !permissions_changed {
            return Ok(PatchStatus::Unchanged);
        }
        if !config_is_unchanged(&target, expected_contents)? {
            return Err(config_changed_error(&target));
        }
        fs::set_permissions(&target, permissions.expect("existing file permissions"))?;
        return Ok(PatchStatus::Updated);
    }

    atomic_write(&target, output, permissions, expected_contents)?;
    Ok(PatchStatus::Updated)
}

fn atomic_write(
    path: &Path,
    contents: &[u8],
    permissions: Option<Permissions>,
    expected_contents: Option<&[u8]>,
) -> Result<(), Error> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let create_private_parent = !parent.exists();
    fs::create_dir_all(parent)?;

    #[cfg(unix)]
    if create_private_parent {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, Permissions::from_mode(0o700))?;
    }

    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("config");
    let temp_path = parent.join(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));

    let result = (|| -> std::io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        let mut file = options.open(&temp_path)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.write_all(contents)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(Error::Io(error));
    }

    if !config_is_unchanged(path, expected_contents)? {
        let _ = fs::remove_file(&temp_path);
        return Err(config_changed_error(path));
    }
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(Error::Io(error));
    }
    Ok(())
}

fn config_is_unchanged(path: &Path, expected_contents: Option<&[u8]>) -> Result<bool, Error> {
    match expected_contents {
        Some(expected) => Ok(fs::read(path)? == expected),
        None => match fs::symlink_metadata(path) {
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(Error::Io(error)),
        },
    }
}

fn config_changed_error(path: &Path) -> Error {
    Error::Config(format!(
        "Config {} changed while it was being patched; no changes were written",
        path.display()
    ))
}

fn desired_permissions(
    mut permissions: Option<Permissions>,
    make_private: bool,
) -> (Option<Permissions>, bool) {
    #[cfg(unix)]
    if make_private
        && let Some(permissions) = permissions.as_mut()
    {
        use std::os::unix::fs::PermissionsExt;
        let changed = permissions.mode() & 0o777 != 0o600;
        permissions.set_mode(0o600);
        return (Some(permissions.clone()), changed);
    }

    let _ = make_private;
    (permissions, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "copilot-api-proxy-client-config-{}",
                uuid::Uuid::new_v4()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn join(&self, path: &str) -> PathBuf {
            self.0.join(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn derives_client_urls_from_proxy_origin_or_v1_url() {
        let expected = ProxyUrls {
            claude: "http://localhost:9876".to_string(),
            codex: "http://localhost:9876/v1".to_string(),
        };
        assert_eq!(proxy_urls("http://localhost:9876/").unwrap(), expected);
        assert_eq!(proxy_urls("http://localhost:9876/v1/").unwrap(), expected);

        assert!(proxy_urls("localhost:9876").is_err());
        assert!(proxy_urls("http://localhost:9876/other").is_err());
        assert!(proxy_urls("http://user:secret@localhost:9876").is_err());
    }

    #[test]
    fn creates_claude_settings_with_private_permissions() {
        let temp = TempDir::new();
        let path = temp.join("nested/settings.json");

        assert_eq!(
            patch_claude_settings(&path, DEFAULT_PROXY_URL, None, Some("claude-sonnet-4.6"))
                .unwrap(),
            PatchStatus::Updated
        );
        let document: JsonValue =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            document["env"]["ANTHROPIC_BASE_URL"],
            "http://localhost:9876"
        );
        assert_eq!(
            document["env"]["ANTHROPIC_AUTH_TOKEN"],
            DEFAULT_CLAUDE_AUTH_TOKEN
        );
        assert_eq!(document["model"], "claude-sonnet-4.6");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn merges_claude_settings_and_is_idempotent() {
        let temp = TempDir::new();
        let path = temp.join("settings.json");
        fs::write(
            &path,
            r#"{
  "env": {
    "KEEP_ME": "yes",
    "ANTHROPIC_AUTH_TOKEN": "existing-secret"
  },
  "permissions": {"allow": ["Bash(git status)"]}
}
"#,
        )
        .unwrap();

        assert_eq!(
            patch_claude_settings(&path, "http://127.0.0.1:8080/v1", None, None).unwrap(),
            PatchStatus::Updated
        );
        let first = fs::read(&path).unwrap();
        let document: JsonValue = serde_json::from_slice(&first).unwrap();
        assert_eq!(document["env"]["KEEP_ME"], "yes");
        assert_eq!(document["env"]["ANTHROPIC_AUTH_TOKEN"], "existing-secret");
        assert_eq!(
            document["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:8080"
        );
        assert_eq!(document["permissions"]["allow"][0], "Bash(git status)");
        assert!(document.get("model").is_none());

        assert_eq!(
            patch_claude_settings(&path, "http://127.0.0.1:8080", None, None).unwrap(),
            PatchStatus::Unchanged
        );
        assert_eq!(fs::read(&path).unwrap(), first);
    }

    #[test]
    fn explicit_claude_token_and_model_take_precedence() {
        let temp = TempDir::new();
        let path = temp.join("settings.json");
        fs::write(
            &path,
            r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"old"},"model":"old-model"}"#,
        )
        .unwrap();

        patch_claude_settings(
            &path,
            DEFAULT_PROXY_URL,
            Some("new-secret"),
            Some("claude-sonnet-4.6"),
        )
        .unwrap();
        let document: JsonValue = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(document["env"]["ANTHROPIC_AUTH_TOKEN"], "new-secret");
        assert_eq!(document["model"], "claude-sonnet-4.6");
    }

    #[test]
    fn invalid_claude_settings_are_not_overwritten() {
        let temp = TempDir::new();
        for (name, original) in [
            ("invalid.json", "{not-json"),
            ("wrong-root.json", "[]"),
            ("wrong-env.json", r#"{"env":"not-an-object"}"#),
        ] {
            let path = temp.join(name);
            fs::write(&path, original).unwrap();
            assert!(patch_claude_settings(&path, DEFAULT_PROXY_URL, None, None).is_err());
            assert_eq!(fs::read_to_string(path).unwrap(), original);
        }
    }

    #[test]
    fn creates_codex_provider_config() {
        let temp = TempDir::new();
        let path = temp.join("nested/config.toml");

        assert_eq!(
            patch_codex_config(&path, DEFAULT_PROXY_URL, Some("gpt-5.3-codex")).unwrap(),
            PatchStatus::Updated
        );
        let document = fs::read_to_string(path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(document["model_provider"].as_str(), Some(CODEX_PROVIDER_ID));
        assert_eq!(document["model"].as_str(), Some("gpt-5.3-codex"));
        assert_eq!(
            document["model_providers"][CODEX_PROVIDER_ID]["base_url"].as_str(),
            Some("http://localhost:9876/v1")
        );
        assert_eq!(
            document["model_providers"][CODEX_PROVIDER_ID]["wire_api"].as_str(),
            Some("responses")
        );
        assert_eq!(
            document["model_providers"][CODEX_PROVIDER_ID]["requires_openai_auth"].as_bool(),
            Some(false)
        );
    }

    #[test]
    fn preserves_codex_comments_and_other_providers_and_is_idempotent() {
        let temp = TempDir::new();
        let path = temp.join("config.toml");
        fs::write(
            &path,
            r#"# keep this comment
model = "existing-model"

[model_providers.other]
name = "Other provider"
base_url = "https://example.com/v1"
"#,
        )
        .unwrap();

        assert_eq!(
            patch_codex_config(&path, "http://127.0.0.1:9999/", None).unwrap(),
            PatchStatus::Updated
        );
        let first = fs::read_to_string(&path).unwrap();
        assert!(first.contains("# keep this comment"));
        assert!(first.contains("[model_providers.other]"));
        assert!(first.contains("name = \"Other provider\""));
        let document = first.parse::<DocumentMut>().unwrap();
        assert_eq!(document["model"].as_str(), Some("existing-model"));
        assert_eq!(
            document["model_providers"][CODEX_PROVIDER_ID]["base_url"].as_str(),
            Some("http://127.0.0.1:9999/v1")
        );

        assert_eq!(
            patch_codex_config(&path, "http://127.0.0.1:9999", None).unwrap(),
            PatchStatus::Unchanged
        );
        assert_eq!(fs::read_to_string(path).unwrap(), first);
    }

    #[test]
    fn patches_inline_codex_provider_tables() {
        let temp = TempDir::new();
        let path = temp.join("config.toml");
        fs::write(
            &path,
            r#"model_providers = { other = { name = "Other", base_url = "https://example.com" } }
"#,
        )
        .unwrap();

        patch_codex_config(&path, DEFAULT_PROXY_URL, None).unwrap();
        let document = fs::read_to_string(path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            document["model_providers"][CODEX_PROVIDER_ID]["base_url"].as_str(),
            Some("http://localhost:9876/v1")
        );
        assert_eq!(
            document["model_providers"]["other"]["name"].as_str(),
            Some("Other")
        );
    }

    #[test]
    fn invalid_codex_config_is_not_overwritten() {
        let temp = TempDir::new();
        for (name, original) in [
            ("invalid.toml", "not = [valid"),
            ("wrong-provider.toml", "model_providers = 42\n"),
            (
                "wrong-owned-provider.toml",
                "[model_providers]\ncopilot_api_proxy = 42\n",
            ),
        ] {
            let path = temp.join(name);
            fs::write(&path, original).unwrap();
            assert!(patch_codex_config(&path, DEFAULT_PROXY_URL, None).is_err());
            assert_eq!(fs::read_to_string(path).unwrap(), original);
        }
    }

    #[cfg(unix)]
    #[test]
    fn follows_config_symlinks_instead_of_replacing_them() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let target = temp.join("actual-settings.json");
        let link = temp.join("settings.json");
        fs::write(&target, "{}\n").unwrap();
        symlink(&target, &link).unwrap();

        patch_claude_settings(&link, DEFAULT_PROXY_URL, None, None).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let document: JsonValue =
            serde_json::from_str(&fs::read_to_string(target).unwrap()).unwrap();
        assert_eq!(
            document["env"]["ANTHROPIC_BASE_URL"],
            "http://localhost:9876"
        );
    }
}
