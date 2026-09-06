use chrono::{SecondsFormat, Utc};
use clap::Subcommand;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

const ARCHITECTURES: [&str; 2] = ["aarch64", "x86_64"];
const MAX_EVIDENCE_BYTES: u64 = 2 * 1024 * 1024;
const EVIDENCE_MARKER: &str = "<!-- mix-release-decision:v1 -->";
const CHECKS: [&str; 8] = [
    "codex_oauth_api_key_provider_matrix",
    "codex_account_switch_history_rollback",
    "claude_auth_and_native_resume",
    "macos_aarch64_clean_install_uninstall",
    "macos_x86_64_clean_install_uninstall",
    "signed_update_and_failure_recovery",
    "voiceover_keyboard_responsive_ui",
    "privacy_security_support_signoff",
];
const RELEASE_ENV: [&str; 16] = [
    "APPLE_CERTIFICATE",
    "APPLE_CERTIFICATE_PASSWORD",
    "APPLE_INSTALLER_CERTIFICATE",
    "APPLE_INSTALLER_CERTIFICATE_PASSWORD",
    "APPLE_KEYCHAIN_PASSWORD",
    "APPLE_SIGNING_IDENTITY",
    "APPLE_INSTALLER_SIGNING_IDENTITY",
    "APPLE_ID",
    "APPLE_PASSWORD",
    "APPLE_TEAM_ID",
    "TAURI_SIGNING_PRIVATE_KEY",
    "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
    "MIX_UPDATER_PUBKEY",
    "MIX_UPDATER_SIGNING_PUBLIC_KEY",
    "MIX_UPDATE_ENDPOINT",
    "MIX_RELEASE_PAGE_URL",
];

#[derive(Subcommand)]
pub enum Command {
    Verify {
        #[arg(long)]
        version: String,
        #[arg(long)]
        artifacts: PathBuf,
        #[arg(long)]
        accepted: bool,
    },
    Record {
        #[arg(long)]
        version: String,
        #[arg(long)]
        repository: String,
        #[arg(long)]
        tag: String,
        #[arg(long)]
        artifacts: PathBuf,
        #[arg(long)]
        approver: String,
        #[arg(long)]
        workflow_run: String,
        #[arg(long)]
        evidence_reference: String,
        #[arg(long)]
        evidence_sha256: String,
        #[arg(long)]
        evidence_file: PathBuf,
        #[arg(long)]
        draft_release_url: String,
        #[arg(long = "check")]
        checks: Vec<String>,
    },
    EvidenceHash {
        path: PathBuf,
    },
    EvidenceLocation {
        #[arg(long)]
        repository: String,
        reference: String,
    },
    EvidenceVerify {
        #[arg(long)]
        version: String,
        #[arg(long)]
        repository: String,
        #[arg(long)]
        tag: String,
        #[arg(long)]
        draft_release_url: String,
        #[arg(long)]
        sha256sums_sha256: String,
        path: PathBuf,
    },
    Audit {
        #[arg(long)]
        artifacts: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        markdown_output: Option<PathBuf>,
    },
}

pub fn run(command: Command, root: &Path) -> Result<bool, String> {
    match command {
        Command::Verify {
            version,
            artifacts,
            accepted,
        } => {
            verify_artifacts(&artifacts, &version, accepted)?;
            println!(
                "verified {}release assets: version={version}",
                if accepted { "accepted " } else { "" }
            );
        }
        Command::Record {
            version,
            repository,
            tag,
            artifacts,
            approver,
            workflow_run,
            evidence_reference,
            evidence_sha256,
            evidence_file,
            draft_release_url,
            checks,
        } => record(
            &artifacts,
            &version,
            &repository,
            &tag,
            &approver,
            &workflow_run,
            &evidence_reference,
            &evidence_sha256,
            &evidence_file,
            &draft_release_url,
            &parse_checks(&checks)?,
        )?,
        Command::EvidenceHash { path } => {
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.len() == 0
                || metadata.len() > MAX_EVIDENCE_BYTES
            {
                return Err(
                    "acceptance evidence must be a non-empty regular file no larger than 2 MiB"
                        .into(),
                );
            }
            println!("{}", super::sha256(&path)?);
        }
        Command::EvidenceLocation {
            repository,
            reference,
        } => {
            let (commit, path) = evidence_location(&reference, &repository)?;
            println!(
                "{}",
                serde_json::to_string(&json!({"commit":commit,"path":path}))
                    .map_err(|error| error.to_string())?
            );
        }
        Command::EvidenceVerify {
            version,
            repository,
            tag,
            draft_release_url,
            sha256sums_sha256,
            path,
        } => {
            verify_evidence(
                &path,
                &version,
                &repository,
                &tag,
                &draft_release_url,
                &sha256sums_sha256,
                None,
            )?;
            println!("verified release acceptance evidence: {}", path.display());
        }
        Command::Audit {
            artifacts,
            output,
            markdown_output,
        } => {
            let version = super::release_version(root)?;
            let report = audit(&artifacts, &version)?;
            if let Some(path) = output {
                super::atomic_json(&path, &report, 0o644)?;
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
                );
            }
            if let Some(path) = markdown_output {
                super::atomic_write(&path, markdown(&report)?.as_bytes(), 0o644)?;
            }
            return Ok(report.get("status").and_then(Value::as_str) == Some("ready"));
        }
    }
    Ok(true)
}

fn payload_names(version: &str) -> Result<BTreeSet<String>, String> {
    super::validate_version(version)?;
    let mut names = BTreeSet::from(["latest.json".into(), "THIRD-PARTY-NOTICES.md".into()]);
    for architecture in ARCHITECTURES {
        let archive = format!("mix_{version}_{architecture}.app.tar.gz");
        names.extend([
            format!("mix_{version}_{architecture}.dmg"),
            archive.clone(),
            format!("{archive}.sig"),
            format!("mix_{version}_macos_{architecture}.cdx.json"),
            format!("mix_{version}_cli_{architecture}.pkg"),
            format!("mix_{version}_cli_{architecture}.cdx.json"),
            format!("mix_{version}_macos_{architecture}.licenses.html"),
        ]);
    }
    Ok(names)
}

fn expected_names(version: &str, accepted: bool) -> Result<BTreeSet<String>, String> {
    let mut names = payload_names(version)?;
    names.insert("SHA256SUMS".into());
    if accepted {
        names.insert("RELEASE-ACCEPTANCE.json".into());
    }
    Ok(names)
}

fn regular_names(directory: &Path) -> Result<BTreeSet<String>, String> {
    let mut names = BTreeSet::new();
    for entry in std::fs::read_dir(directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?
    {
        let entry = entry.map_err(|error| error.to_string())?;
        let metadata = entry
            .path()
            .symlink_metadata()
            .map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "release directory contains a non-regular entry: {}",
                entry.file_name().to_string_lossy()
            ));
        }
        names.insert(entry.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

fn verify_checksums(directory: &Path, payloads: &BTreeSet<String>) -> Result<(), String> {
    let text = std::fs::read_to_string(directory.join("SHA256SUMS"))
        .map_err(|error| format!("cannot read SHA256SUMS: {error}"))?;
    let mut entries = BTreeMap::new();
    let mut ordered = Vec::new();
    for line in text.lines() {
        let (digest, name) = line
            .split_once("  ")
            .ok_or_else(|| format!("invalid SHA256SUMS line: {line}"))?;
        validate_digest(digest)?;
        if !safe_asset_name(name) || entries.insert(name.to_string(), digest).is_some() {
            return Err(format!("invalid or duplicate checksum entry: {name}"));
        }
        ordered.push(name.to_string());
    }
    if ordered.windows(2).any(|pair| pair[0] >= pair[1])
        || entries.keys().cloned().collect::<BTreeSet<_>>() != *payloads
    {
        return Err("SHA256SUMS entries must exactly match sorted release payloads".into());
    }
    for (name, digest) in entries {
        let path = directory.join(&name);
        let metadata = path
            .symlink_metadata()
            .map_err(|error| format!("cannot inspect {name}: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
            return Err(format!("release artifact is empty or unsafe: {name}"));
        }
        if super::sha256(&path)? != digest {
            return Err(format!("release artifact checksum mismatch: {name}"));
        }
    }
    Ok(())
}

fn verify_artifacts(directory: &Path, version: &str, accepted: bool) -> Result<(), String> {
    let expected = expected_names(version, accepted)?;
    let actual = regular_names(directory)?;
    if actual != expected {
        let missing = expected.difference(&actual).collect::<Vec<_>>();
        let extra = actual.difference(&expected).collect::<Vec<_>>();
        return Err(format!(
            "release artifact set mismatch: missing={missing:?}, extra={extra:?}"
        ));
    }
    verify_checksums(directory, &payload_names(version)?)?;
    if accepted {
        validate_acceptance(directory, version)?;
    }
    Ok(())
}

fn safe_asset_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
}

fn validate_digest(value: &str) -> Result<(), String> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("SHA-256 must be 64 lowercase hexadecimal characters".into());
    }
    Ok(())
}

fn validate_repository(repository: &str) -> Result<(), String> {
    if repository.split('/').count() != 2
        || repository.split('/').any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
        })
    {
        return Err("invalid release repository".into());
    }
    Ok(())
}

fn validate_identity(version: &str, repository: &str, tag: &str) -> Result<(), String> {
    super::validate_version(version)?;
    validate_repository(repository)?;
    if tag != format!("v{version}") {
        return Err("release tag does not match the version".into());
    }
    Ok(())
}

fn validate_https(value: &str) -> Result<(), String> {
    let url = url::Url::parse(value).map_err(|_| "invalid HTTPS reference")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || ["example.com", "example.test", "placeholder", "changeme"]
            .iter()
            .any(|marker| value.to_ascii_lowercase().contains(marker))
    {
        return Err("release reference must be a non-placeholder HTTPS URL".into());
    }
    Ok(())
}

fn evidence_location(value: &str, repository: &str) -> Result<(String, String), String> {
    validate_repository(repository)?;
    validate_https(value)?;
    let url = url::Url::parse(value).map_err(|_| "invalid acceptance evidence URL")?;
    let mut segments = url
        .path_segments()
        .ok_or("acceptance evidence URL must contain a repository path")?;
    let owner = segments.next().unwrap_or("");
    let name = segments.next().unwrap_or("");
    let revision = segments.next().unwrap_or("");
    let file = segments.collect::<Vec<_>>();
    let revision_is_commit = matches!(revision.len(), 40 | 64)
        && revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
    let file_is_safe = !file.is_empty()
        && file.iter().all(|part| {
            !part.is_empty()
                && !matches!(*part, "." | "..")
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        });
    if url.host_str() != Some("raw.githubusercontent.com")
        || url.port().is_some()
        || url.query().is_some()
        || format!("{owner}/{name}") != repository
        || !revision_is_commit
        || !file_is_safe
    {
        return Err(
            "acceptance evidence must be a commit-addressed raw GitHub file in the release repository"
                .into(),
        );
    }
    Ok((revision.into(), file.join("/")))
}

fn validate_workflow_run(value: &str, repository: &str) -> Result<(), String> {
    validate_https(value)?;
    let prefix = format!("https://github.com/{repository}/actions/runs/");
    let run_id = value.strip_prefix(&prefix).unwrap_or("");
    if run_id.is_empty() || !run_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("workflow run URL does not match the release repository".into());
    }
    Ok(())
}

fn validate_draft_release_url(value: &str, repository: &str, tag: &str) -> Result<(), String> {
    validate_https(value)?;
    if value != format!("https://github.com/{repository}/releases/tag/{tag}") {
        return Err("draft release URL does not match the exact repository and tag".into());
    }
    Ok(())
}

fn parse_checks(values: &[String]) -> Result<BTreeMap<String, bool>, String> {
    let required = CHECKS.into_iter().collect::<BTreeSet<_>>();
    let mut result = BTreeMap::new();
    for value in values {
        let (name, raw) = value
            .split_once('=')
            .ok_or_else(|| format!("invalid acceptance check: {value}"))?;
        if !required.contains(name)
            || result.insert(name.into(), raw == "true").is_some()
            || !matches!(raw, "true" | "false")
        {
            return Err(format!("invalid or duplicate acceptance check: {value}"));
        }
    }
    if result.len() != CHECKS.len() || result.values().any(|value| !value) {
        return Err("every required release acceptance check must be true".into());
    }
    Ok(result)
}

fn exact_fields(value: &Value, expected: &[&str], context: &str) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{context} must be an object"))?;
    let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(format!("{context} has an unexpected schema"));
    }
    Ok(())
}

fn validate_utc(value: &str, context: &str) -> Result<(), String> {
    let timestamp = chrono::DateTime::parse_from_rfc3339(value)
        .map_err(|_| format!("{context} must be an RFC 3339 timestamp"))?;
    if timestamp.offset().local_minus_utc() != 0 {
        return Err(format!("{context} must be UTC"));
    }
    Ok(())
}

fn read_evidence_decision(path: &Path) -> Result<Value, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_EVIDENCE_BYTES
    {
        return Err(
            "acceptance evidence must be a non-empty regular file no larger than 2 MiB".into(),
        );
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("acceptance evidence must be valid UTF-8: {error}"))?;
    if text.contains("- [ ]") {
        return Err("acceptance evidence contains an incomplete checklist item".into());
    }
    for placeholder in [
        "`X.Y.Z`",
        "`vX.Y.Z`",
        "<40/64-hex>",
        "<64-lowercase-hex>",
        "`PASS/FAIL`",
        "`ACCEPT / REJECT`",
        "https://…",
        "<name>",
        "<time>",
        "<value>",
        "<digest>",
        "<tester",
        "<reviewer",
        "<device",
        "<start>",
        "<end>",
        "<name/team>",
        "<RFC3339 UTC>",
        "<none or linked list>",
    ] {
        if text.contains(placeholder) {
            return Err(format!(
                "acceptance evidence still contains placeholder: {placeholder}"
            ));
        }
    }
    let mut sections = text.split(EVIDENCE_MARKER);
    let _human = sections.next();
    let encoded = sections
        .next()
        .ok_or("acceptance evidence has no sealed decision block")?;
    if sections.next().is_some() {
        return Err("acceptance evidence has more than one sealed decision block".into());
    }
    let encoded = encoded.trim();
    let encoded = encoded
        .strip_prefix("```json\n")
        .and_then(|value| value.strip_suffix("\n```"))
        .ok_or("sealed decision block must be the final fenced JSON block")?;
    serde_json::from_str(encoded).map_err(|error| format!("invalid sealed decision JSON: {error}"))
}

fn verify_evidence(
    path: &Path,
    version: &str,
    repository: &str,
    tag: &str,
    draft_release_url: &str,
    sha256sums_sha256: &str,
    expected_sha256: Option<&str>,
) -> Result<BTreeMap<String, bool>, String> {
    validate_identity(version, repository, tag)?;
    validate_draft_release_url(draft_release_url, repository, tag)?;
    validate_digest(sha256sums_sha256)?;
    if let Some(expected) = expected_sha256 {
        validate_digest(expected)?;
        if super::sha256(path)? != expected {
            return Err(
                "downloaded acceptance evidence SHA-256 does not match the reviewed digest".into(),
            );
        }
    }
    let decision = read_evidence_decision(path)?;
    exact_fields(
        &decision,
        &[
            "schema_version",
            "product",
            "version",
            "tag",
            "repository",
            "draft_release_url",
            "sha256sums_sha256",
            "decision",
            "release_manager",
            "decision_at",
            "known_risk_links",
            "checks",
        ],
        "sealed decision",
    )?;
    if decision["schema_version"].as_u64() != Some(1)
        || decision["product"].as_str() != Some("Mix")
        || decision["version"].as_str() != Some(version)
        || decision["tag"].as_str() != Some(tag)
        || decision["repository"].as_str() != Some(repository)
        || decision["draft_release_url"].as_str() != Some(draft_release_url)
        || decision["sha256sums_sha256"].as_str() != Some(sha256sums_sha256)
        || decision["decision"].as_str() != Some("ACCEPT")
    {
        return Err("sealed decision does not match the exact accepted release".into());
    }
    let manager = decision["release_manager"]
        .as_str()
        .map(str::trim)
        .unwrap_or("");
    if manager.is_empty() || manager.len() > 100 {
        return Err("sealed decision requires a valid release manager".into());
    }
    validate_utc(
        decision["decision_at"].as_str().unwrap_or(""),
        "sealed decision_at",
    )?;
    let risks = decision["known_risk_links"]
        .as_array()
        .ok_or("known_risk_links must be an array")?;
    if risks.len() > 100 {
        return Err("known_risk_links contains too many entries".into());
    }
    for risk in risks {
        validate_https(
            risk.as_str()
                .ok_or("known_risk_links entries must be HTTPS URLs")?,
        )?;
    }
    let checks = decision["checks"]
        .as_object()
        .ok_or("sealed decision checks must be an object")?;
    if checks.keys().map(String::as_str).collect::<BTreeSet<_>>()
        != CHECKS.into_iter().collect::<BTreeSet<_>>()
    {
        return Err("sealed decision must contain exactly the eight required checks".into());
    }
    let mut accepted = BTreeMap::new();
    for name in CHECKS {
        let check = checks
            .get(name)
            .ok_or_else(|| format!("sealed decision is missing check: {name}"))?;
        exact_fields(
            check,
            &["result", "reviewer", "tested_at", "evidence"],
            &format!("sealed check {name}"),
        )?;
        if check["result"].as_str() != Some("PASS") {
            return Err(format!("sealed check did not pass: {name}"));
        }
        let reviewer = check["reviewer"].as_str().map(str::trim).unwrap_or("");
        if reviewer.is_empty() || reviewer.len() > 100 {
            return Err(format!("sealed check requires a valid reviewer: {name}"));
        }
        validate_utc(
            check["tested_at"].as_str().unwrap_or(""),
            &format!("sealed check tested_at: {name}"),
        )?;
        let links = check["evidence"]
            .as_array()
            .filter(|links| !links.is_empty() && links.len() <= 100)
            .ok_or_else(|| format!("sealed check requires evidence links: {name}"))?;
        for link in links {
            validate_https(
                link.as_str()
                    .ok_or_else(|| format!("sealed check evidence must be an HTTPS URL: {name}"))?,
            )?;
        }
        accepted.insert(name.into(), true);
    }
    Ok(accepted)
}

#[allow(clippy::too_many_arguments)]
fn record(
    directory: &Path,
    version: &str,
    repository: &str,
    tag: &str,
    approver: &str,
    workflow_run: &str,
    evidence_reference: &str,
    evidence_sha256: &str,
    evidence_file: &Path,
    draft_release_url: &str,
    checks: &BTreeMap<String, bool>,
) -> Result<(), String> {
    if directory.join("RELEASE-ACCEPTANCE.json").is_file() {
        verify_artifacts(directory, version, true)?;
    } else {
        verify_artifacts(directory, version, false)?;
    }
    validate_identity(version, repository, tag)?;
    validate_workflow_run(workflow_run, repository)?;
    evidence_location(evidence_reference, repository)?;
    validate_digest(evidence_sha256)?;
    let sha256sums_sha256 = super::sha256(&directory.join("SHA256SUMS"))?;
    let evidence_checks = verify_evidence(
        evidence_file,
        version,
        repository,
        tag,
        draft_release_url,
        &sha256sums_sha256,
        Some(evidence_sha256),
    )?;
    if &evidence_checks != checks {
        return Err("workflow acceptance checks do not match the sealed evidence decision".into());
    }
    let approver = approver.trim();
    if approver.is_empty() || approver.len() > 100 {
        return Err("approver must be between 1 and 100 characters".into());
    }
    let artifact_sha256 = payload_names(version)?
        .into_iter()
        .map(|name| Ok((name.clone(), super::sha256(&directory.join(name))?)))
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let payload = json!({
        "schema_version":3,
        "product":"Mix",
        "version":version,
        "tag":tag,
        "repository":repository,
        "completed_at":Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "approver":approver,
        "workflow_run":workflow_run,
        "evidence_reference":evidence_reference,
        "evidence_sha256":evidence_sha256,
        "draft_release_url":draft_release_url,
        "sha256sums_sha256":sha256sums_sha256,
        "checks":checks,
        "artifact_sha256":artifact_sha256,
    });
    super::atomic_json(&directory.join("RELEASE-ACCEPTANCE.json"), &payload, 0o644)?;
    verify_artifacts(directory, version, true)
}

fn validate_acceptance(directory: &Path, version: &str) -> Result<(), String> {
    let payload = super::read_json(&directory.join("RELEASE-ACCEPTANCE.json"))?;
    let expected_fields = BTreeSet::from([
        "schema_version",
        "product",
        "version",
        "tag",
        "repository",
        "completed_at",
        "approver",
        "workflow_run",
        "evidence_reference",
        "evidence_sha256",
        "draft_release_url",
        "sha256sums_sha256",
        "checks",
        "artifact_sha256",
    ]);
    let actual_fields = payload
        .as_object()
        .ok_or("release acceptance must be an object")?
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if actual_fields != expected_fields {
        return Err("release acceptance has an unexpected schema".into());
    }
    if payload.get("schema_version").and_then(Value::as_u64) != Some(3)
        || payload.get("product").and_then(Value::as_str) != Some("Mix")
        || payload.get("version").and_then(Value::as_str) != Some(version)
    {
        return Err("release acceptance identity is invalid".into());
    }
    validate_identity(
        version,
        payload
            .get("repository")
            .and_then(Value::as_str)
            .unwrap_or(""),
        payload.get("tag").and_then(Value::as_str).unwrap_or(""),
    )?;
    validate_workflow_run(
        payload
            .get("workflow_run")
            .and_then(Value::as_str)
            .unwrap_or(""),
        payload
            .get("repository")
            .and_then(Value::as_str)
            .unwrap_or(""),
    )?;
    evidence_location(
        payload
            .get("evidence_reference")
            .and_then(Value::as_str)
            .unwrap_or(""),
        payload
            .get("repository")
            .and_then(Value::as_str)
            .unwrap_or(""),
    )?;
    validate_digest(
        payload
            .get("evidence_sha256")
            .and_then(Value::as_str)
            .unwrap_or(""),
    )?;
    validate_draft_release_url(
        payload
            .get("draft_release_url")
            .and_then(Value::as_str)
            .unwrap_or(""),
        payload
            .get("repository")
            .and_then(Value::as_str)
            .unwrap_or(""),
        payload.get("tag").and_then(Value::as_str).unwrap_or(""),
    )?;
    let sha256sums_sha256 = payload
        .get("sha256sums_sha256")
        .and_then(Value::as_str)
        .unwrap_or("");
    validate_digest(sha256sums_sha256)?;
    if super::sha256(&directory.join("SHA256SUMS"))? != sha256sums_sha256 {
        return Err("release acceptance SHA256SUMS digest mismatch".into());
    }
    let approver = payload
        .get("approver")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if approver.is_empty() || approver.len() > 100 {
        return Err("release acceptance requires a valid approver".into());
    }
    let completed_at = payload
        .get("completed_at")
        .and_then(Value::as_str)
        .ok_or("release acceptance has no completed_at")?;
    validate_utc(completed_at, "release acceptance completed_at")?;
    let checks = payload
        .get("checks")
        .and_then(Value::as_object)
        .ok_or("missing checks")?;
    if checks.len() != CHECKS.len()
        || CHECKS
            .iter()
            .any(|name| checks.get(*name).and_then(Value::as_bool) != Some(true))
    {
        return Err("release acceptance checks are incomplete".into());
    }
    let hashes = payload
        .get("artifact_sha256")
        .and_then(Value::as_object)
        .ok_or("missing artifact hashes")?;
    let names = payload_names(version)?;
    if hashes.keys().map(String::as_str).collect::<BTreeSet<_>>()
        != names.iter().map(String::as_str).collect::<BTreeSet<_>>()
    {
        return Err("acceptance artifact set mismatch".into());
    }
    for name in names {
        let digest = hashes.get(&name).and_then(Value::as_str).unwrap_or("");
        validate_digest(digest)?;
        if super::sha256(&directory.join(&name))? != digest {
            return Err(format!("acceptance digest mismatch: {name}"));
        }
    }
    Ok(())
}

fn command_status(program: &str, args: &[&str]) -> Value {
    match ProcessCommand::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) => json!({"passed":status.success(),"exit_code":status.code()}),
        Err(_) => json!({"passed":false,"exit_code":Value::Null}),
    }
}

fn codesigning_identity_count() -> usize {
    if !cfg!(target_os = "macos") {
        return 0;
    }
    let output = match ProcessCommand::new("/usr/bin/security")
        .args(["find-identity", "-v", "-p", "codesigning"])
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(_) => return 0,
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let suffix = " valid identities found";
            line.trim()
                .strip_suffix(suffix)
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(0)
}

fn assess_dmg(path: &Path) -> Value {
    if !cfg!(target_os = "macos") || !path.is_file() {
        return json!({
            "gatekeeper":{"passed":false,"exit_code":Value::Null},
            "stapled_ticket":{"passed":false,"exit_code":Value::Null},
        });
    }
    let path = path.to_string_lossy();
    json!({
        "gatekeeper":command_status("/usr/sbin/spctl", &["--assess","--type","open","--context","context:primary-signature","-vv",&path]),
        "stapled_ticket":command_status("/usr/bin/xcrun", &["stapler","validate",&path]),
    })
}

fn audit(artifacts: &Path, version: &str) -> Result<Value, String> {
    let expected = expected_names(version, true)?;
    let actual = regular_names(artifacts).unwrap_or_default();
    let verification = verify_artifacts(artifacts, version, true);
    let mut architectures = Map::new();
    for architecture in ARCHITECTURES {
        architectures.insert(
            architecture.into(),
            assess_dmg(&artifacts.join(format!("mix_{version}_{architecture}.dmg"))),
        );
    }
    let signed = architectures.values().all(|value| {
        ["gatekeeper", "stapled_ticket"].iter().all(|check| {
            value
                .get(*check)
                .and_then(|item| item.get("passed"))
                .and_then(Value::as_bool)
                == Some(true)
        })
    });
    let configured = RELEASE_ENV
        .into_iter()
        .map(|name| {
            (
                name.into(),
                Value::Bool(std::env::var(name).is_ok_and(|value| !value.trim().is_empty())),
            )
        })
        .collect::<Map<_, _>>();
    Ok(json!({
        "schema_version":1,
        "product":"Mix",
        "version":version,
        "generated_at":Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "host":{"system":std::env::consts::OS,"machine":std::env::consts::ARCH},
        "status":if verification.is_ok() && signed {"ready"} else {"not_ready"},
        "checks":{
            "accepted_exact_release_assets":{
                "passed":verification.is_ok(),
                "expected_files":expected.len(),
                "missing":expected.difference(&actual).collect::<Vec<_>>(),
                "extra":actual.difference(&expected).collect::<Vec<_>>(),
                "error":verification.err().unwrap_or_default(),
            },
            "dual_architecture_gatekeeper_and_notarization":{"passed":signed,"architectures":architectures},
        },
        "local_build_inputs":{
            "codesigning_identity_count":codesigning_identity_count(),
            "configured":configured,
            "note":"Configuration presence is diagnostic only and never proves a signed or accepted release.",
        }
    }))
}

fn markdown(report: &Value) -> Result<String, String> {
    let assets = &report["checks"]["accepted_exact_release_assets"];
    let notarization = &report["checks"]["dual_architecture_gatekeeper_and_notarization"];
    let mut rows = vec![
        "# Mix release readiness audit".into(),
        "".into(),
        format!(
            "Generated: `{}`",
            report["generated_at"].as_str().unwrap_or("")
        ),
        format!("Version: `{}`", report["version"].as_str().unwrap_or("")),
        format!(
            "Status: **{}**",
            report["status"].as_str().unwrap_or("not_ready")
        ),
        "".into(),
        "| Gate | Result |".into(),
        "| --- | --- |".into(),
        format!(
            "| Exact accepted release assets | {} |",
            if assets["passed"] == true {
                "pass"
            } else {
                "fail"
            }
        ),
        format!(
            "| Dual-architecture Gatekeeper + notarization | {} |",
            if notarization["passed"] == true {
                "pass"
            } else {
                "fail"
            }
        ),
        "".into(),
    ];
    if assets["missing"]
        .as_array()
        .is_some_and(|items| !items.is_empty())
    {
        rows.extend(["## Missing release assets".into(), "".into()]);
        for name in assets["missing"].as_array().into_iter().flatten() {
            rows.push(format!("- `{}`", name.as_str().unwrap_or("")));
        }
        rows.push(String::new());
    }
    rows.extend([
        "## Architecture verification".into(),
        "".into(),
        "| Architecture | Gatekeeper | Stapled ticket |".into(),
        "| --- | --- | --- |".into(),
    ]);
    for architecture in ARCHITECTURES {
        let checks = &notarization["architectures"][architecture];
        rows.push(format!(
            "| `{architecture}` | {} | {} |",
            check_text(&checks["gatekeeper"]),
            check_text(&checks["stapled_ticket"])
        ));
    }
    rows.extend([
        "".into(),
        "## Local build inputs".into(),
        "".into(),
        format!(
            "- Valid code-signing identities: `{}`",
            report["local_build_inputs"]["codesigning_identity_count"]
                .as_u64()
                .unwrap_or(0)
        ),
        "- Environment values are reported only as configured/not configured; secret values are never included.".into(),
        "- Input presence does not count as release acceptance.".into(),
        "".into(),
    ]);
    Ok(rows.join("\n"))
}

fn check_text(value: &Value) -> String {
    if value["passed"].as_bool() == Some(true) {
        "pass".into()
    } else if value["exit_code"].is_null() {
        "not available".into()
    } else {
        format!("fail (exit {})", value["exit_code"].as_i64().unwrap_or(-1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealed_decision() -> Value {
        let checks = CHECKS
            .into_iter()
            .map(|name| {
                (
                    name.into(),
                    json!({
                        "result":"PASS",
                        "reviewer":"release-reviewer",
                        "tested_at":"2026-09-03T00:00:00Z",
                        "evidence":[format!("https://github.com/owner/repository/issues/{name}")],
                    }),
                )
            })
            .collect::<Map<_, _>>();
        json!({
            "schema_version":1,
            "product":"Mix",
            "version":"1.2.3",
            "tag":"v1.2.3",
            "repository":"owner/repository",
            "draft_release_url":"https://github.com/owner/repository/releases/tag/v1.2.3",
            "sha256sums_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "decision":"ACCEPT",
            "release_manager":"release-manager",
            "decision_at":"2026-09-03T01:00:00Z",
            "known_risk_links":[],
            "checks":checks,
        })
    }

    fn evidence_text(decision: &Value) -> String {
        format!(
            "# Completed acceptance\n\n- [x] All required scenarios passed.\n\n{EVIDENCE_MARKER}\n```json\n{}\n```\n",
            serde_json::to_string_pretty(decision).unwrap()
        )
    }

    #[test]
    fn checks_require_the_exact_complete_true_set() {
        let valid = CHECKS
            .iter()
            .map(|name| format!("{name}=true"))
            .collect::<Vec<_>>();
        assert_eq!(
            parse_checks(&valid).expect("valid checks").len(),
            CHECKS.len()
        );
        assert!(parse_checks(&valid[..7]).is_err());
        let mut false_check = valid.clone();
        false_check[0] = format!("{}=false", CHECKS[0]);
        assert!(parse_checks(&false_check).is_err());
    }

    #[test]
    fn references_reject_credentials_fragments_and_placeholders() {
        assert!(validate_https("https://github.com/org/repo/actions/runs/1").is_ok());
        for invalid in [
            "http://github.com/org/repo",
            "https://user@github.com/org/repo",
            "https://example.com/evidence",
            "https://github.com/org/repo#fragment",
        ] {
            assert!(validate_https(invalid).is_err(), "{invalid}");
        }
        assert!(
            validate_workflow_run("https://github.com/org/repo/actions/runs/123", "org/repo")
                .is_ok()
        );
        assert!(validate_workflow_run(
            "https://github.com/other/repo/actions/runs/123",
            "org/repo"
        )
        .is_err());
        assert!(evidence_location(
            "https://raw.githubusercontent.com/org/repo/0123456789abcdef0123456789abcdef01234567/release/acceptance.md",
            "org/repo"
        )
        .is_ok());
        for invalid in [
            "https://github.com/org/repo/blob/main/release/acceptance.md",
            "https://raw.githubusercontent.com/org/repo/main/release/acceptance.md",
            "https://raw.githubusercontent.com/other/repo/0123456789abcdef0123456789abcdef01234567/release/acceptance.md",
            "https://raw.githubusercontent.com/org/repo/0123456789abcdef0123456789abcdef01234567/release/acceptance.md?download=1",
            "https://raw.githubusercontent.com/org/repo/0123456789abcdef0123456789abcdef01234567/release/my%20acceptance.md",
        ] {
            assert!(
                evidence_location(invalid, "org/repo").is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn asset_names_are_exact_and_safe() {
        let payloads = payload_names("1.2.3").expect("payloads");
        assert_eq!(payloads.len(), 16);
        assert!(payloads.iter().all(|name| safe_asset_name(name)));
        assert!(payloads.contains("mix_1.2.3_cli_aarch64.pkg"));
        assert!(!payloads.contains("mix_1.2.3_cli_aarch64.app.tar.gz"));
        assert_eq!(expected_names("1.2.3", true).expect("accepted").len(), 18);
    }

    #[test]
    fn sealed_evidence_binds_every_release_identity_and_check() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("acceptance.md");
        std::fs::write(&path, evidence_text(&sealed_decision())).unwrap();
        let digest = super::super::sha256(&path).unwrap();

        let checks = verify_evidence(
            &path,
            "1.2.3",
            "owner/repository",
            "v1.2.3",
            "https://github.com/owner/repository/releases/tag/v1.2.3",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Some(&digest),
        )
        .expect("complete sealed evidence");
        assert_eq!(checks.len(), CHECKS.len());
        assert!(checks.values().all(|value| *value));
    }

    #[test]
    fn sealed_evidence_rejects_digest_or_release_mismatch() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("acceptance.md");
        std::fs::write(&path, evidence_text(&sealed_decision())).unwrap();

        assert!(verify_evidence(
            &path,
            "1.2.3",
            "owner/repository",
            "v1.2.3",
            "https://github.com/owner/repository/releases/tag/v1.2.3",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        )
        .is_err());
        assert!(verify_evidence(
            &path,
            "1.2.4",
            "owner/repository",
            "v1.2.4",
            "https://github.com/owner/repository/releases/tag/v1.2.4",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            None,
        )
        .is_err());
    }

    #[test]
    fn sealed_evidence_rejects_unfinished_or_incomplete_reviews() {
        let root = tempfile::tempdir().unwrap();
        let unchecked = root.path().join("unchecked.md");
        std::fs::write(
            &unchecked,
            evidence_text(&sealed_decision()).replace("- [x]", "- [ ]"),
        )
        .unwrap();
        assert!(read_evidence_decision(&unchecked).is_err());

        let mut incomplete = sealed_decision();
        incomplete["checks"]
            .as_object_mut()
            .unwrap()
            .remove(CHECKS[0]);
        let missing = root.path().join("missing.md");
        std::fs::write(&missing, evidence_text(&incomplete)).unwrap();
        assert!(verify_evidence(
            &missing,
            "1.2.3",
            "owner/repository",
            "v1.2.3",
            "https://github.com/owner/repository/releases/tag/v1.2.3",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            None,
        )
        .is_err());
    }
}
