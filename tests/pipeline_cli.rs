//! Black-box offline diagnostics: machine-readable stdout and no credential writes.
use serde_json::{Value, json};
use std::{fs, process::Command};

struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("kiro-pipeline-cli-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn run(&self, config: Value, request: Option<Value>) -> std::process::Output {
        let config_path = self.0.join("config.json");
        fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_kiro-rs"));
        command
            .arg("--config")
            .arg(&config_path)
            .arg("--credentials")
            .arg(self.0.join("must-not-be-created.json"))
            .env("RUST_LOG", "info");
        if let Some(request) = request {
            let request_path = self.0.join("request.json");
            fs::write(&request_path, serde_json::to_vec(&request).unwrap()).unwrap();
            command.arg("--inspect-request").arg(request_path);
        } else {
            command.arg("--check-config");
        }
        let output = command.output().unwrap();
        assert!(!self.0.join("must-not-be-created.json").exists());
        output
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn thinking_request_stdout_is_one_redacted_json_document() {
    let fixture = Fixture::new();
    let output = fixture.run(json!({}), Some(json!({
        "model":"claude-sonnet-4-thinking", "max_tokens":1000,
        "system":"PRIVATE_SYSTEM_TEXT", "messages":[{"role":"user","content":"PRIVATE_USER_TEXT"}]
    })));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["networkRequests"], 0);
    assert_eq!(result["nativeCacheEvidence"], Value::Null);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("PRIVATE_"));
}

#[test]
fn check_config_rejects_unknown_endpoint_and_misspelled_pipeline_option() {
    let fixture = Fixture::new();
    for config in [
        json!({"defaultEndpoint":"unknown"}),
        json!({"requestPipeline":{"cacheStratgey":"static-prefix"}}),
    ] {
        assert_eq!(fixture.run(config, None).status.code(), Some(2));
    }
    let output = fixture.run(json!({"requestPipeline":{"ingressMaxBytes":65536}}), None);
    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["requestPipeline"]["ingressMaxBytes"], 65536);
}

#[test]
fn inspection_rejects_invalid_request_and_outputs_configured_budget_failure() {
    let fixture = Fixture::new();
    let mut request = json!({"model":"claude-sonnet-4","max_tokens":0,"messages":[{"role":"user","content":"safe fixture"}]});
    assert_eq!(
        fixture.run(json!({}), Some(request.clone())).status.code(),
        Some(2)
    );
    request["max_tokens"] = json!(1000);
    let output = fixture.run(
        json!({"requestPipeline":{"limits":{"bodyBytes":10}}}),
        Some(request),
    );
    assert_eq!(output.status.code(), Some(2));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["localBudgetAccepted"], false);
    assert_eq!(result["networkRequests"], 0);
}
