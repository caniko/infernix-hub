use std::{fs, path::Path, process::Command};

use fleetix_agent_proxy::config::private_dir;
use serde_json::Value;
use uuid::Uuid;

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(Uuid::new_v4().simple().to_string());
        private_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn proxy(state: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fleetix-agent-proxy"));
    command.env_clear().env("HOME", state).env("PROXY_STATE_DIR", state);
    command
}

#[test]
fn register_uses_only_the_usage_key() {
    let dir = Scratch::new();
    let state = &dir.0;
    fs::write(state.join("management-key"), "admin-fixture").unwrap();
    fs::write(state.join("usage-key"), "state-usage").unwrap();
    let explicit = state.join("explicit");
    fs::write(&explicit, "file-usage\n").unwrap();
    for expected in ["state-usage", "env-usage", "file-usage"] {
        let mut command = proxy(state);
        command.arg("register-t3code").arg("--t3code-home").arg(state);
        if expected != "state-usage" {
            command.env("PROXY_USAGE_KEY", "env-usage");
        }
        if expected == "file-usage" {
            command.arg("--usage-key-file").arg(&explicit);
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let raw = fs::read_to_string(state.join("userdata/settings.json")).unwrap();
        let settings: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(settings["usageLimitSources"]["fleetix-proxy"]["managementKey"], expected);
        assert!(!raw.contains("admin-fixture"));
    }
}

#[test]
fn serve_refuses_shared_keys() {
    for (client, management, usage) in [("a", "b", "a"), ("a", "b", "b"), ("a", "a", "b")] {
        let dir = Scratch::new();
        let output = proxy(&dir.0)
            .args(["serve", "--bind", "127.0.0.1:0"])
            .env("PROXY_CLIENT_KEY", client)
            .env("PROXY_MANAGEMENT_KEY", management)
            .env("PROXY_USAGE_KEY", usage)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("keys must differ"));
    }
}
