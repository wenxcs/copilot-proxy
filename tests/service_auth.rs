#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("proxy-service-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir(root.join("home")).unwrap();
        let systemctl = root.join("bin/systemctl");
        fs::write(
            &systemctl,
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$SERVICE_TEST_LOG"
[ "$1" = "--user" ] || exit 90
case "$2" in
    status) exit "${SERVICE_TEST_STATUS:-4}" ;;
    stop|enable|start) exit 0 ;;
    *) exit 91 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(systemctl, fs::Permissions::from_mode(0o700)).unwrap();
        Self { root }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_copilot-api-proxy"));
        command
            .env_clear()
            .current_dir(&self.root)
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            // Only the fake manager is reachable; this cannot affect a real service.
            .env("PATH", self.root.join("bin"))
            .env("SERVICE_TEST_LOG", self.root.join("commands"));
        command
    }

    fn key_path(&self) -> PathBuf {
        self.root
            .join("home/.local/share/copilot-api-proxy/proxy_api_key")
    }

    fn policy_path(&self) -> PathBuf {
        self.root
            .join("home/.local/share/copilot-api-proxy/model_policy_config.json")
    }

    fn unit(&self) -> String {
        fs::read_to_string(
            self.root
                .join("config/systemd/user/wxplus-copilot-api-proxy.service"),
        )
        .unwrap()
    }

    fn commands(&self) -> String {
        fs::read_to_string(self.root.join("commands")).unwrap_or_default()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn service_stores_key_privately_and_only_exposes_its_path() {
    let sandbox = Sandbox::new();
    let key = "sk-service-private-key";
    let output = sandbox
        .command()
        .args(["service", "install"])
        .env("COPILOT_PROXY_API_KEY", key)
        .output()
        .unwrap();
    assert_success(&output);

    let key_path = sandbox.key_path();
    assert_eq!(fs::read_to_string(&key_path).unwrap(), key);
    assert_eq!(
        fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(key_path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let unit = sandbox.unit();
    assert!(unit.contains("--api-key-file"));
    assert!(unit.contains(key_path.to_str().unwrap()));
    for text in [
        unit,
        sandbox.commands(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ] {
        assert!(!text.contains(key));
    }
}

#[test]
fn reinstalling_running_service_restarts_it_with_cli_key_over_environment() {
    let sandbox = Sandbox::new();
    let first = sandbox
        .command()
        .args(["service", "install"])
        .env("COPILOT_PROXY_API_KEY", "sk-original-key")
        .output()
        .unwrap();
    assert_success(&first);

    let second = sandbox
        .command()
        .args(["service", "install", "--api-key", "sk-cli-replacement"])
        .env("COPILOT_PROXY_API_KEY", "sk-env-must-not-win")
        .env("SERVICE_TEST_STATUS", "0")
        .output()
        .unwrap();
    assert_success(&second);
    assert_eq!(
        fs::read_to_string(sandbox.key_path()).unwrap(),
        "sk-cli-replacement"
    );
    let commands = sandbox.commands();
    assert_eq!(commands.matches("--user start ").count(), 2);
    assert_eq!(commands.matches("--user stop ").count(), 1);
    assert!(commands.find("--user stop ").unwrap() < commands.rfind("--user start ").unwrap());
    assert!(!sandbox.unit().contains("sk-cli-replacement"));
}

#[test]
fn invalid_service_key_fails_before_any_service_action() {
    for key in ["", "bad key", "bad\"key", "bad%key"] {
        let sandbox = Sandbox::new();
        let output = sandbox
            .command()
            .args(["service", "install"])
            .env("COPILOT_PROXY_API_KEY", key)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("API key"));
        assert!(sandbox.commands().is_empty());
        assert!(!sandbox.key_path().exists());
    }
}

#[test]
fn missing_or_empty_key_file_fails_server_startup() {
    let sandbox = Sandbox::new();
    let missing = sandbox.root.join("missing-key");
    let empty = sandbox.root.join("empty-key");
    fs::write(&empty, "").unwrap();
    for path in [missing, empty] {
        let output = sandbox
            .command()
            .args(["server", "--api-key-file"])
            .arg(path)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.to_lowercase().contains("api key"), "{stderr}");
        assert!(!stderr.contains("Token not found"), "{stderr}");
    }
    assert!(sandbox.commands().is_empty());
}

#[test]
fn service_captures_policy_and_admin_secret_only_in_private_configuration() {
    let sandbox = Sandbox::new();
    let rules = "-all,+gpt-6-astra,5h:gpt-6-astra:1024,7d:gpt-6-astra:2048";
    let password = "admin!$%\"secret\\value";
    let state_path = sandbox.root.join("private-state/usage.json");
    let output = sandbox
        .command()
        .args(["service", "install"])
        .env("COPILOT_PROXY_MODEL_RULES", rules)
        .env("COPILOT_PROXY_SUPER_PASSWORD", password)
        .env("COPILOT_PROXY_LIMIT_STATE_PATH", &state_path)
        .output()
        .unwrap();
    assert_success(&output);

    let policy_path = sandbox.policy_path();
    let stored: serde_json::Value =
        serde_json::from_slice(&fs::read(&policy_path).unwrap()).unwrap();
    assert_eq!(stored["rules"], rules);
    assert_eq!(stored["super_password"], password);
    assert_eq!(stored["state_path"], state_path.to_str().unwrap());
    assert_eq!(
        fs::metadata(&policy_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(policy_path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let unit = sandbox.unit();
    assert!(unit.contains("--model-policy-file"));
    assert!(unit.contains(policy_path.to_str().unwrap()));
    for text in [
        unit,
        sandbox.commands(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ] {
        assert!(!text.contains(password));
        assert!(!text.contains(rules));
    }
}

#[test]
fn invalid_policy_settings_fail_before_stopping_or_installing_a_service() {
    for (name, value) in [
        ("COPILOT_PROXY_MODEL_RULES", "5h:gpt-6-astra:-1"),
        ("COPILOT_PROXY_MODEL_RULES", "8h:gpt-6-astra:1024"),
        ("COPILOT_PROXY_MODEL_RULES", "gpt-6-astra"),
        ("COPILOT_PROXY_SUPER_PASSWORD", ""),
        ("COPILOT_PROXY_SUPER_PASSWORD", "secret with space"),
        ("COPILOT_PROXY_SUPER_PASSWORD", "秘密"),
        ("COPILOT_PROXY_LIMIT_STATE_PATH", "relative.json"),
    ] {
        let sandbox = Sandbox::new();
        let output = sandbox
            .command()
            .args(["service", "install"])
            .env(name, value)
            .env("SERVICE_TEST_STATUS", "0")
            .output()
            .unwrap();
        assert!(!output.status.success(), "{name}");
        assert!(sandbox.commands().is_empty(), "{name}");
        assert!(!sandbox.policy_path().exists());
        if name == "COPILOT_PROXY_SUPER_PASSWORD" && !value.is_empty() {
            assert!(!String::from_utf8_lossy(&output.stderr).contains(value));
        }
    }
}

#[test]
fn missing_or_invalid_policy_file_fails_before_github_authentication() {
    let sandbox = Sandbox::new();
    let file = sandbox.root.join("policy.json");
    for contents in [
        None,
        Some("{"),
        Some(r#"{"super_password":""}"#),
        Some(r#"{"rules":"5h:gpt-6-astra:nope"}"#),
        Some(r#"{"unknown_field":"secret-value"}"#),
    ] {
        if let Some(contents) = contents {
            fs::write(&file, contents).unwrap();
        }
        let output = sandbox
            .command()
            .args(["server", "--model-policy-file"])
            .arg(&file)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("Configuration error"), "{stderr}");
        assert!(!stderr.contains("Token not found"), "{stderr}");
        assert!(!stderr.contains("secret-value"));
    }
    assert!(sandbox.commands().is_empty());
}

#[test]
fn explicit_policy_file_overrides_invalid_environment_without_using_network() {
    let sandbox = Sandbox::new();
    let file = sandbox.root.join("policy.json");
    fs::write(&file, "{}").unwrap();
    let output = sandbox
        .command()
        .args(["server", "--model-policy-file"])
        .arg(&file)
        .env("COPILOT_PROXY_MODEL_RULES", "invalid-rule")
        .env("COPILOT_PROXY_SUPER_PASSWORD", "")
        .env("COPILOT_PROXY_LIMIT_STATE_PATH", "relative.json")
        .output()
        .unwrap();
    assert!(!output.status.success());
    // Cleared environment and the isolated empty home ensure GitHub setup stops locally.
    assert!(String::from_utf8_lossy(&output.stderr).contains("Token not found"));
    assert!(sandbox.commands().is_empty());
}
