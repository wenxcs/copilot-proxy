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
