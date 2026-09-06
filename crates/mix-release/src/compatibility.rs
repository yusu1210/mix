use clap::Subcommand;
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use uuid::Uuid;

const MAX_REQUEST_HEADER_BYTES: usize = 64 * 1024;
const MAX_CLAUDE_STATUS_BYTES: u64 = 64 * 1024;

#[derive(Subcommand)]
pub enum Command {
    /// Verify custom-Provider file auth with a disposable home and loopback server.
    CodexProviderRoute {
        #[arg(long)]
        codex: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..=60))]
        timeout_seconds: u64,
    },
    /// Verify that Claude Code honors CLAUDE_CONFIG_DIR without using real login state.
    ClaudeConfigDir {
        #[arg(long)]
        claude: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..=60))]
        timeout_seconds: u64,
    },
}

pub fn run(command: Command) -> Result<(), String> {
    match command {
        Command::CodexProviderRoute {
            codex,
            output,
            timeout_seconds,
        } => {
            let report = codex_provider_route(&codex, Duration::from_secs(timeout_seconds))?;
            if let Some(path) = output {
                super::atomic_json(&path, &report, 0o644)?;
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
            );
        }
        Command::ClaudeConfigDir {
            claude,
            output,
            timeout_seconds,
        } => {
            let report = claude_config_dir(&claude, Duration::from_secs(timeout_seconds))?;
            if let Some(path) = output {
                super::atomic_json(&path, &report, 0o644)?;
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
            );
        }
    }
    Ok(())
}

fn claude_config_dir(claude: &Path, timeout: Duration) -> Result<Value, String> {
    let claude = resolved_executable(claude, "Claude Code")?;
    let version = client_version(&claude, "Claude Code")?;
    let digest = super::sha256(&claude)?;
    let temporary =
        tempfile::tempdir().map_err(|error| format!("cannot create Claude probe home: {error}"))?;
    let home = temporary.path().join("home");
    let config = temporary.path().join("config");
    fs::create_dir_all(&home)
        .and_then(|_| fs::create_dir_all(&config))
        .map_err(|error| format!("cannot create Claude probe directories: {error}"))?;
    let sentinel = br#"{"mix_probe_sentinel":true}"#;
    let home_state = home.join(".claude.json");
    fs::write(&home_state, sentinel)
        .map_err(|error| format!("cannot write Claude probe sentinel: {error}"))?;

    let stdout = temporary.path().join("stdout");
    let status = run_claude_status(&claude, &home, &config, &stdout, timeout)?;
    let output = read_bounded(&stdout, MAX_CLAUDE_STATUS_BYTES, "Claude status output")?;
    let status_json = serde_json::from_slice::<Value>(&output)
        .map_err(|error| format!("Claude status output is not JSON: {error}"))?;
    if !status_json.is_object() {
        return Err("Claude status output must be a JSON object".into());
    }
    let isolated_state = config.join(".claude.json");
    let isolated_metadata = fs::symlink_metadata(&isolated_state)
        .map_err(|error| format!("Claude did not create isolated state: {error}"))?;
    if isolated_metadata.file_type().is_symlink() || !isolated_metadata.is_file() {
        return Err("Claude isolated state must be a regular file".into());
    }
    if fs::read(&home_state).map_err(|error| format!("cannot verify Claude probe home: {error}"))?
        != sentinel
    {
        return Err("Claude changed HOME state instead of isolating it".into());
    }

    Ok(json!({
        "schema_version": 1,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "result": "pass",
        "platform": {
            "os": std::env::consts::OS,
            "architecture": std::env::consts::ARCH
        },
        "claude": {
            "version": version,
            "sha256": digest
        },
        "isolation": {
            "disposable_home": true,
            "disposable_config_dir": true,
            "real_credential_used": false,
            "home_state_unchanged": true
        },
        "observation": {
            "status_json": true,
            "isolated_state_created": true,
            "exit_code": status.code()
        }
    }))
}

fn run_claude_status(
    claude: &Path,
    home: &Path,
    config: &Path,
    stdout: &Path,
    timeout: Duration,
) -> Result<std::process::ExitStatus, String> {
    let stdout_file = fs::File::create(stdout)
        .map_err(|error| format!("cannot create Claude probe output: {error}"))?;
    let mut child = ProcessCommand::new(claude)
        .args(["auth", "status", "--json"])
        .current_dir(home)
        .env("HOME", home)
        .env("CLAUDE_CONFIG_DIR", config)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .env_remove("CLAUDE_CODE_USE_BEDROCK")
        .env_remove("CLAUDE_CODE_USE_VERTEX")
        .env_remove("CLAUDE_CODE_USE_FOUNDRY")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot start isolated Claude status probe: {error}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("cannot inspect Claude status probe: {error}"))?
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Claude status probe timed out".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn read_bounded(path: &Path, limit: u64, label: &str) -> Result<Vec<u8>, String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("cannot inspect {label}: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > limit {
        return Err(format!("{label} is not a bounded regular file"));
    }
    fs::read(path).map_err(|error| format!("cannot read {label}: {error}"))
}

fn resolved_executable(path: &Path, client: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{client} executable path must be absolute"));
    }
    let resolved = fs::canonicalize(path)
        .map_err(|error| format!("cannot resolve {}: {error}", path.display()))?;
    let metadata = fs::symlink_metadata(&resolved)
        .map_err(|error| format!("cannot inspect {}: {error}", resolved.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{client} executable must resolve to a regular file"
        ));
    }
    Ok(resolved)
}

fn client_version(executable: &Path, client: &str) -> Result<String, String> {
    let output = ProcessCommand::new(executable)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("cannot run {client} version probe: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{client} version probe failed with {}",
            output.status
        ));
    }
    let version = String::from_utf8(output.stdout)
        .map_err(|_| format!("{client} version is not UTF-8"))?
        .trim()
        .to_owned();
    if version.is_empty() || version.len() > 200 {
        return Err(format!("{client} returned an invalid version"));
    }
    Ok(version)
}

fn codex_provider_route(codex: &Path, timeout: Duration) -> Result<Value, String> {
    let codex = executable(codex)?;
    let version = codex_version(&codex)?;
    let digest = super::sha256(&codex)?;
    let temporary =
        tempfile::tempdir().map_err(|error| format!("cannot create probe home: {error}"))?;
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .map_err(|error| format!("cannot bind probe server: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("cannot configure probe server: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("cannot read probe address: {error}"))?;
    let credential = format!("mix-probe-{}", Uuid::new_v4());
    write_probe_home(&temporary, address.port(), &credential)?;
    let mut child = start_codex(&codex, &temporary)?;
    let request = wait_for_request(&listener, &mut child, timeout);
    stop_child(&mut child);
    let request = request?;
    let observed = parse_request(&request)?;
    let expected_authorization = format!("Bearer {credential}");
    let passed = observed.method == "POST"
        && observed.path == "/v1/responses"
        && observed.host == address.to_string()
        && observed.authorization == expected_authorization;
    let report = json!({
        "schema_version": 1,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "result": if passed { "pass" } else { "fail" },
        "platform": {
            "os": std::env::consts::OS,
            "architecture": std::env::consts::ARCH
        },
        "codex": {
            "version": version,
            "sha256": digest
        },
        "isolation": {
            "disposable_home": true,
            "loopback_endpoint": true,
            "real_credential_used": false
        },
        "observation": {
            "request_method": observed.method,
            "request_path": observed.path,
            "selected_provider_used": observed.host == address.to_string(),
            "synthetic_auth_json_api_key_used": observed.authorization == expected_authorization
        }
    });
    if passed {
        Ok(report)
    } else {
        Err(
            "Codex did not send the synthetic file credential to the selected custom Provider"
                .into(),
        )
    }
}

fn executable(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("Codex executable path must be absolute".into());
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Codex executable must be a regular file, not a symbolic link".into());
    }
    Ok(path.to_path_buf())
}

fn codex_version(codex: &Path) -> Result<String, String> {
    client_version(codex, "Codex")
}

fn write_probe_home(home: &TempDir, port: u16, credential: &str) -> Result<(), String> {
    let config = format!(
        "check_for_update_on_startup = false\nmodel = \"mix-probe\"\nmodel_provider = \"mix-probe\"\n\n[otel]\nexporter = \"none\"\ntrace_exporter = \"none\"\nmetrics_exporter = \"none\"\n\n[model_providers.mix-probe]\nname = \"Mix isolated probe\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true\n"
    );
    fs::write(home.path().join("config.toml"), config)
        .map_err(|error| format!("cannot write probe config: {error}"))?;
    let auth = serde_json::to_vec(&json!({"OPENAI_API_KEY": credential}))
        .map_err(|error| error.to_string())?;
    fs::write(home.path().join("auth.json"), auth)
        .map_err(|error| format!("cannot write probe credential: {error}"))
}

fn start_codex(codex: &Path, home: &TempDir) -> Result<Child, String> {
    ProcessCommand::new(codex)
        .args([
            "exec",
            "--strict-config",
            "--ephemeral",
            "--ignore-rules",
            "--skip-git-repo-check",
            "--color",
            "never",
            "probe",
        ])
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("CODEX_HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot start isolated Codex probe: {error}"))
}

fn wait_for_request(
    listener: &TcpListener,
    child: &mut Child,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((mut stream, peer)) => {
                if !peer.ip().is_loopback() {
                    return Err("probe server accepted a non-loopback peer".into());
                }
                return read_request(&mut stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(format!("cannot accept probe request: {error}")),
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("cannot inspect Codex probe: {error}"))?
        {
            return Err(format!(
                "Codex exited before contacting the custom Provider: {status}"
            ));
        }
        if Instant::now() >= deadline {
            return Err("Codex did not contact the custom Provider before timeout".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn read_request(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| format!("cannot configure probe connection: {error}"))?;
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    while !request.windows(4).any(|value| value == b"\r\n\r\n") {
        let read = stream
            .read(&mut chunk)
            .map_err(|error| format!("cannot read probe request: {error}"))?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
        if request.len() > MAX_REQUEST_HEADER_BYTES {
            return Err("Codex probe request headers are too large".into());
        }
    }
    let body = b"{\"error\":{\"message\":\"isolated probe stop\",\"type\":\"probe\"}}";
    write!(
        stream,
        "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .and_then(|_| stream.write_all(body))
        .map_err(|error| format!("cannot finish probe request: {error}"))?;
    Ok(request)
}

fn stop_child(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

struct ObservedRequest {
    method: String,
    path: String,
    host: String,
    authorization: String,
}

fn parse_request(request: &[u8]) -> Result<ObservedRequest, String> {
    let header_end = request
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .ok_or_else(|| "probe request headers are incomplete".to_string())?;
    let text = std::str::from_utf8(&request[..header_end])
        .map_err(|_| "probe request headers are not UTF-8")?;
    let mut lines = text.split("\r\n");
    let first = lines
        .next()
        .ok_or_else(|| "probe request has no request line".to_string())?;
    let mut request_line = first.split_whitespace();
    let method = request_line.next().unwrap_or_default().to_owned();
    let path = request_line.next().unwrap_or_default().to_owned();
    if request_line.next().is_none() || request_line.next().is_some() {
        return Err("probe request line is invalid".into());
    }
    let mut host = None;
    let mut authorization = None;
    for line in lines.take_while(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err("probe request contains an invalid header".into());
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "host" if host.is_some() => {
                return Err("probe request has duplicate Host headers".into())
            }
            "host" => host = Some(value.trim().to_owned()),
            "authorization" if authorization.is_some() => {
                return Err("probe request has duplicate Authorization headers".into());
            }
            "authorization" => authorization = Some(value.trim().to_owned()),
            _ => {}
        }
    }
    Ok(ObservedRequest {
        method,
        path,
        host: host.ok_or_else(|| "probe request has no Host header".to_string())?,
        authorization: authorization
            .ok_or_else(|| "probe request has no Authorization header".to_string())?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn claude_fixture(script: &str) -> (TempDir, PathBuf) {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("claude-fixture");
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let link = directory.path().join("claude");
        symlink(&executable, &link).unwrap();
        (directory, link)
    }

    #[cfg(unix)]
    #[test]
    fn claude_probe_accepts_logged_out_status_and_redacts_output() {
        let (_directory, claude) = claude_fixture(
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf '%s\n' 'fixture 1.0.0'
  exit 0
fi
mkdir -p "$CLAUDE_CONFIG_DIR"
printf '%s' '{"token":"isolated-secret"}' > "$CLAUDE_CONFIG_DIR/.claude.json"
printf '%s\n' '{"loggedIn":false,"token":"status-secret"}'
exit 1
"#,
        );

        let report = claude_config_dir(&claude, Duration::from_secs(2)).unwrap();
        let serialized = serde_json::to_string(&report).unwrap();

        assert_eq!(report["result"], "pass");
        assert_eq!(report["observation"]["exit_code"], 1);
        assert!(!serialized.contains("isolated-secret"));
        assert!(!serialized.contains("status-secret"));
    }

    #[cfg(unix)]
    #[test]
    fn claude_probe_rejects_a_client_that_changes_home_state() {
        let (_directory, claude) = claude_fixture(
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf '%s\n' 'fixture 1.0.0'
  exit 0
fi
mkdir -p "$CLAUDE_CONFIG_DIR"
printf '%s' '{}' > "$CLAUDE_CONFIG_DIR/.claude.json"
printf '%s' '{}' > "$HOME/.claude.json"
printf '%s\n' '{"loggedIn":false}'
exit 1
"#,
        );

        let error = claude_config_dir(&claude, Duration::from_secs(2)).unwrap_err();

        assert!(error.contains("changed HOME state"));
    }

    #[test]
    fn request_parser_extracts_only_routing_evidence() {
        let request = b"POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nAuthorization: Bearer private\r\nContent-Length: 0\r\n\r\n";
        let observed = parse_request(request).unwrap();
        assert_eq!(observed.method, "POST");
        assert_eq!(observed.path, "/v1/responses");
        assert_eq!(observed.host, "127.0.0.1:1234");
        assert_eq!(observed.authorization, "Bearer private");
    }

    #[test]
    fn request_parser_rejects_an_ambiguous_request_line() {
        let request = b"POST /v1/responses HTTP/1.1 extra\r\nHost: localhost\r\n\r\n";
        assert!(parse_request(request).is_err());
    }

    #[test]
    fn request_parser_rejects_duplicate_routing_headers() {
        let request = b"POST /v1/responses HTTP/1.1\r\nHost: first\r\nHost: second\r\nAuthorization: Bearer private\r\n\r\n";
        assert!(parse_request(request).is_err());
    }

    #[test]
    fn request_parser_ignores_a_binary_body() {
        let request = b"POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer private\r\nContent-Length: 2\r\n\r\n\xff\xfe";

        let observed = parse_request(request).unwrap();

        assert_eq!(observed.method, "POST");
        assert_eq!(observed.authorization, "Bearer private");
    }
}
