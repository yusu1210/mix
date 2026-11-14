#![cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

#[test]
fn script_switches_accounts_through_the_real_cli_and_preserves_history() {
    use mix_core::{AdapterKind, MixService};
    use serde_json::{json, Value};

    let root = tempfile::tempdir().unwrap();
    let live = root.path().join("native");
    let config = root.path().join("mix/config.json");
    std::fs::create_dir_all(live.join("sessions")).unwrap();
    let service = MixService::new(&config).unwrap();
    service.initialize().unwrap();
    service
        .register_client("codex", AdapterKind::Codex, live.clone())
        .unwrap();
    for name in ["First", "Second"] {
        std::fs::write(live.join("config.toml"), "model = \"test-model\"\n").unwrap();
        std::fs::write(
            live.join("auth.json"),
            json!({"OPENAI_API_KEY": format!("fixture-{name}")}).to_string(),
        )
        .unwrap();
        service
            .capture_current_account("codex", None, Some(name))
            .unwrap();
    }
    let history = live.join("sessions/rollout-example.jsonl");
    let transcript = concat!(
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"example\",\"model_provider\":\"openai\"}}\n",
        "{\"type\":\"event_msg\",\"payload\":{\"message\":\"keep this conversation\"}}\n"
    );
    std::fs::write(&history, transcript).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/switch-codex.sh");
    let desktop = live.join(".codex-global-state.json");
    let backup = live.join(".codex-global-state.json.bak");
    let checkpoint = live.join(".mix-desktop-workspace.json");
    let shared = json!({
        "electron-renderer-content-owner":"example-owner",
        "local-projects":{"project-1":{"id":"project-1","name":"Example","rootPaths":["/example/repo"]}},
        "thread-project-assignments":{"example":{"projectId":"project-1","projectKind":"local"}},
        "electron-owner-atoms:example-owner":{"private-preference":true}
    }).to_string();
    // Desktop schemas are not part of the account-switch contract. Include
    // damaged and unrecognized state so a future client upgrade cannot revive
    // the old dependency on parsing these files.
    for native_state in [
        None,
        Some(shared.as_str()),
        Some("not json"),
        Some("{\"future\":3}"),
    ] {
        if let Some(state) = native_state {
            std::fs::write(&desktop, state).unwrap();
        }
        std::fs::write(&backup, b"native backup").unwrap();
        std::fs::write(&checkpoint, b"obsolete checkpoint").unwrap();
        for (name, status) in [
            ("First", "switched"),
            ("Second", "switched"),
            ("Second", "already_active"),
        ] {
            let output = Command::new("/bin/sh")
                .arg(&script)
                .arg(name)
                .arg("--config")
                .arg(&config)
                .arg("--json")
                .env("MIX_BIN", env!("CARGO_BIN_EXE_mix"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let result: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(result["status"], status);
            assert_eq!(result["stopped_pids"], json!([]));
            assert_eq!(result["restarted"], false);
            let auth: Value =
                serde_json::from_slice(&std::fs::read(live.join("auth.json")).unwrap()).unwrap();
            assert_eq!(auth["OPENAI_API_KEY"], format!("fixture-{name}"));
            assert_eq!(std::fs::read_to_string(&history).unwrap(), transcript);
            assert_eq!(
                std::fs::read_to_string(&desktop).ok().as_deref(),
                native_state
            );
            assert_eq!(std::fs::read(&backup).unwrap(), b"native backup");
            assert_eq!(std::fs::read(&checkpoint).unwrap(), b"obsolete checkpoint");
            assert!(!root
                .path()
                .join("mix/transactions/active-switch.json")
                .exists());
        }
        if desktop.exists() {
            std::fs::remove_file(&desktop).unwrap();
        }
    }
}

#[test]
fn cli_json_failure_has_a_stable_error_and_nonzero_exit() {
    let root = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(["--json", "--config"])
        .arg(root.path().join("mix/config.json"))
        .args(["use", "codex", "Missing"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["code"], "MIX_NOT_FOUND");
    assert!(error["error"].is_string());
    assert!(error["details"].is_object());
}

#[test]
fn script_uses_only_mix_and_preserves_arguments_output_and_exit_status() {
    let root = tempfile::tempdir().unwrap();
    let binary = root.path().join("mix fixture");
    std::fs::write(&binary, "#!/bin/sh\nprintf '%s\\n' \"$@\"\nexit 42\n").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/switch-codex.sh");
    let output = Command::new("/bin/sh")
        .arg(&script)
        .args([
            "工作 account; not-a-command",
            "--config",
            "/tmp/config with spaces.json",
            "--json",
        ])
        .env("MIX_BIN", &binary)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(42));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "use\ncodex\n工作 account; not-a-command\n--config\n/tmp/config with spaces.json\n--json\n"
    );
    let empty = Command::new("/bin/sh")
        .arg(&script)
        .env("MIX_BIN", &binary)
        .output()
        .unwrap();
    assert_eq!(empty.status.code(), Some(2));
    assert!(empty.stdout.is_empty());
}
