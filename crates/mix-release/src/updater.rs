use base64::Engine;
use clap::Subcommand;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const ARCHITECTURES: [&str; 2] = ["aarch64", "x86_64"];

#[derive(Subcommand)]
pub enum Command {
    Config {
        output: PathBuf,
    },
    Keys {
        #[arg(long)]
        version: String,
        #[arg(long)]
        embedded_public_key: String,
        #[arg(long)]
        signing_public_key: String,
        #[arg(long, default_value = "")]
        bridge_version: String,
    },
    Manifest {
        #[arg(long)]
        version: String,
        #[arg(long)]
        repository: String,
        #[arg(long)]
        tag: String,
        #[arg(long)]
        artifacts: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        public_key: String,
    },
    Verify {
        #[arg(long)]
        version: String,
        #[arg(long)]
        repository: String,
        #[arg(long)]
        tag: String,
        #[arg(long)]
        public_key: String,
        manifest: PathBuf,
    },
}

pub fn run(command: Command) -> Result<(), String> {
    match command {
        Command::Config { output } => {
            let public_key = std::env::var("MIX_UPDATER_PUBKEY").unwrap_or_default();
            let endpoint = std::env::var("MIX_UPDATE_ENDPOINT").unwrap_or_default();
            super::atomic_json(&output, &config(&public_key, &endpoint)?, 0o600)?;
        }
        Command::Keys {
            version,
            embedded_public_key,
            signing_public_key,
            bridge_version,
        } => {
            super::validate_version(&version)?;
            validate_public_key(&embedded_public_key)?;
            validate_public_key(&signing_public_key)?;
            if embedded_public_key.trim() == signing_public_key.trim() {
                if !bridge_version.trim().is_empty() {
                    return Err("updater bridge version must be empty when keys match".into());
                }
            } else if bridge_version.trim() != version {
                return Err(
                    "different updater keys require the current version as the bridge version"
                        .into(),
                );
            }
        }
        Command::Manifest {
            version,
            repository,
            tag,
            artifacts,
            output,
            public_key,
        } => {
            let manifest = manifest(&version, &repository, &tag, &artifacts, &public_key)?;
            super::atomic_json(&output, &manifest, 0o644)?;
        }
        Command::Verify {
            version,
            repository,
            tag,
            public_key,
            manifest,
        } => {
            let payload = super::read_json(&manifest)?;
            verify_manifest(&payload, &version, &repository, &tag, &public_key)?;
        }
    }
    Ok(())
}

fn decode(value: &str, label: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|_| format!("{label} must be valid base64"))
}

fn public_key_id(value: &str) -> Result<[u8; 8], String> {
    let key = value.trim();
    if key.is_empty() || has_placeholder(key) {
        return Err("updater public key must be a non-placeholder key".into());
    }
    let decoded = String::from_utf8(decode(key, "updater public key")?)
        .map_err(|_| "updater public key must encode UTF-8".to_string())?;
    let lines = decoded.lines().collect::<Vec<_>>();
    if lines.len() != 2 || !lines[0].starts_with("untrusted comment:") {
        return Err("updater public key must encode a complete minisign key".into());
    }
    let record = decode(lines[1], "minisign public key")?;
    if record.len() != 42 || !matches!(&record[..2], b"Ed" | b"ED") {
        return Err("updater public key has an invalid minisign record".into());
    }
    record[2..10]
        .try_into()
        .map_err(|_| "updater public key ID is invalid".into())
}

fn validate_public_key(value: &str) -> Result<(), String> {
    public_key_id(value).map(|_| ())
}

fn validate_signature(value: &str, expected_key_id: [u8; 8]) -> Result<String, String> {
    let encoded = value.trim();
    let decoded = String::from_utf8(decode(encoded, "updater signature")?)
        .map_err(|_| "updater signature must encode UTF-8".to_string())?;
    let lines = decoded.lines().collect::<Vec<_>>();
    if lines.len() != 4
        || !lines[0].starts_with("untrusted comment:")
        || !lines[2].starts_with("trusted comment:")
    {
        return Err("updater signature must encode a complete minisign signature".into());
    }
    let signature = decode(lines[1], "minisign signature")?;
    let global = decode(lines[3], "minisign global signature")?;
    if signature.len() != 74 || !matches!(&signature[..2], b"Ed" | b"ED") {
        return Err("updater signature record is invalid".into());
    }
    if global.len() != 64 || signature[2..10] != expected_key_id {
        return Err("updater signature key or global signature is invalid".into());
    }
    Ok(encoded.into())
}

fn has_placeholder(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    ["placeholder", "changeme", "replace-me", "example-key"]
        .iter()
        .any(|marker| value.contains(marker))
}

fn validate_endpoint(value: &str) -> Result<(), String> {
    let parsed = url::Url::parse(value.trim()).map_err(|_| "invalid update endpoint")?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
        || has_placeholder(value)
    {
        return Err("update endpoint must be a non-placeholder HTTPS URL".into());
    }
    Ok(())
}

fn config(public_key: &str, endpoint: &str) -> Result<Value, String> {
    validate_public_key(public_key)?;
    validate_endpoint(endpoint)?;
    Ok(json!({
        "bundle":{"createUpdaterArtifacts":true},
        "plugins":{"updater":{"endpoints":[endpoint.trim()],"pubkey":public_key.trim()}},
    }))
}

fn validate_identity(version: &str, repository: &str, tag: &str) -> Result<(), String> {
    super::validate_version(version)?;
    if repository.split('/').count() != 2
        || repository.split('/').any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
        })
    {
        return Err("invalid GitHub repository".into());
    }
    if tag != format!("v{version}") {
        return Err(format!("release tag {tag} does not match v{version}"));
    }
    Ok(())
}

fn archive_url(repository: &str, tag: &str, name: &str) -> String {
    let encode = |value: &str| {
        value
            .bytes()
            .map(|byte| {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                    (byte as char).to_string()
                } else {
                    format!("%{byte:02X}")
                }
            })
            .collect::<String>()
    };
    format!(
        "https://github.com/{repository}/releases/download/{}/{}",
        encode(tag),
        encode(name)
    )
}

fn manifest(
    version: &str,
    repository: &str,
    tag: &str,
    artifacts: &Path,
    public_key: &str,
) -> Result<Value, String> {
    validate_identity(version, repository, tag)?;
    let key_id = public_key_id(public_key)?;
    let mut platforms = BTreeMap::new();
    for architecture in ARCHITECTURES {
        let name = format!("mix_{version}_{architecture}.app.tar.gz");
        let archive = artifacts.join(&name);
        if !archive.is_file() || archive.metadata().map_err(|error| error.to_string())?.len() == 0 {
            return Err(format!("missing updater archive: {name}"));
        }
        let signature_path = artifacts.join(format!("{name}.sig"));
        let signature = std::fs::read_to_string(&signature_path)
            .map_err(|error| format!("cannot read {}: {error}", signature_path.display()))?;
        platforms.insert(
            format!("darwin-{architecture}"),
            json!({
                "signature":validate_signature(&signature, key_id)?,
                "url":archive_url(repository, tag, &name),
            }),
        );
    }
    Ok(json!({
        "version":version,
        "notes":format!("Mix {version}. See the signed release notes before installing."),
        "platforms":platforms,
    }))
}

fn verify_manifest(
    payload: &Value,
    version: &str,
    repository: &str,
    tag: &str,
    public_key: &str,
) -> Result<(), String> {
    validate_identity(version, repository, tag)?;
    let key_id = public_key_id(public_key)?;
    if payload.get("version").and_then(Value::as_str) != Some(version) {
        return Err("update manifest version mismatch".into());
    }
    let platforms = payload
        .get("platforms")
        .and_then(Value::as_object)
        .ok_or_else(|| "update manifest has no platforms".to_string())?;
    if platforms.len() != ARCHITECTURES.len() {
        return Err("update manifest must contain exactly two macOS platforms".into());
    }
    for architecture in ARCHITECTURES {
        let name = format!("mix_{version}_{architecture}.app.tar.gz");
        let entry = platforms
            .get(&format!("darwin-{architecture}"))
            .and_then(Value::as_object)
            .ok_or_else(|| format!("missing darwin-{architecture}"))?;
        validate_signature(
            entry.get("signature").and_then(Value::as_str).unwrap_or(""),
            key_id,
        )?;
        if entry.get("url").and_then(Value::as_str)
            != Some(archive_url(repository, tag, &name).as_str())
        {
            return Err(format!("invalid update URL for darwin-{architecture}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_lines(lines: &[String]) -> String {
        base64::engine::general_purpose::STANDARD.encode(lines.join("\n"))
    }

    fn public_key(key_id: [u8; 8]) -> String {
        let mut record = vec![0_u8; 42];
        record[..2].copy_from_slice(b"Ed");
        record[2..10].copy_from_slice(&key_id);
        encode_lines(&[
            "untrusted comment: mix test key".into(),
            base64::engine::general_purpose::STANDARD.encode(record),
        ])
    }

    fn signature(key_id: [u8; 8]) -> String {
        let mut record = vec![0_u8; 74];
        record[..2].copy_from_slice(b"Ed");
        record[2..10].copy_from_slice(&key_id);
        encode_lines(&[
            "untrusted comment: signature".into(),
            base64::engine::general_purpose::STANDARD.encode(record),
            "trusted comment: timestamp:0".into(),
            base64::engine::general_purpose::STANDARD.encode([0_u8; 64]),
        ])
    }

    #[test]
    fn minisign_structure_binds_signature_to_key_id() {
        let key_id = *b"12345678";
        let key = public_key(key_id);
        assert_eq!(public_key_id(&key).expect("valid key"), key_id);
        assert!(validate_signature(&signature(key_id), key_id).is_ok());
        assert!(validate_signature(&signature(*b"87654321"), key_id).is_err());
    }

    #[test]
    fn endpoint_rejects_unsafe_or_ambiguous_urls() {
        assert!(validate_endpoint(
            "https://github.com/owner/project/releases/latest/download/latest.json"
        )
        .is_ok());
        for invalid in [
            "http://github.com/latest.json",
            "https://user@github.com/latest.json",
            "https://github.com/latest.json#fragment",
            "https://placeholder.invalid/latest.json",
        ] {
            assert!(validate_endpoint(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn config_is_generated_by_the_validated_release_path() {
        let key = public_key(*b"12345678");
        let value = config(
            &key,
            "https://github.com/owner/project/releases/latest/download/latest.json",
        )
        .expect("validated updater config");
        assert_eq!(value["bundle"]["createUpdaterArtifacts"], true);
        assert_eq!(value["plugins"]["updater"]["pubkey"], key);
        assert_eq!(
            value["plugins"]["updater"]["endpoints"],
            json!(["https://github.com/owner/project/releases/latest/download/latest.json"])
        );
    }

    #[test]
    fn archive_urls_encode_tag_and_asset_segments() {
        assert_eq!(
            archive_url("owner/project", "v1.0.0+build", "mix 1.dmg"),
            "https://github.com/owner/project/releases/download/v1.0.0%2Bbuild/mix%201.dmg"
        );
    }
}
