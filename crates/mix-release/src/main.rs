mod archive;
mod compatibility;
mod readiness;
mod sbom;
mod updater;

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "mix-release", about = "Private Mix release tooling")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Archive {
        #[command(subcommand)]
        command: archive::Command,
    },
    Compatibility {
        #[command(subcommand)]
        command: compatibility::Command,
    },
    Meta {
        #[command(subcommand)]
        command: MetaCommand,
    },
    Updater {
        #[command(subcommand)]
        command: updater::Command,
    },
    Sbom {
        #[command(subcommand)]
        command: sbom::Command,
    },
    Readiness {
        #[command(subcommand)]
        command: readiness::Command,
    },
}

#[derive(Subcommand)]
enum MetaCommand {
    Version,
    Check,
    CheckTag { tag: String },
}

fn main() {
    match run(Cli::parse()) {
        Ok(true) => {}
        Ok(false) => std::process::exit(3),
        Err(error) => {
            eprintln!("release tooling error: {error}");
            std::process::exit(2);
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let root = workspace_root()?;
    match cli.command {
        Command::Archive { command } => archive::run(command)?,
        Command::Compatibility { command } => compatibility::run(command)?,
        Command::Meta { command } => {
            let version = release_version(&root)?;
            match command {
                MetaCommand::Version => println!("{version}"),
                MetaCommand::Check => {
                    println!("Mix release metadata is consistent: version={version}")
                }
                MetaCommand::CheckTag { tag } => {
                    if tag.strip_prefix('v').unwrap_or(&tag) != version {
                        return Err(format!(
                            "tag {tag} does not match release version {version}"
                        ));
                    }
                    println!("Mix release metadata is consistent: version={version}");
                }
            }
        }
        Command::Updater { command } => updater::run(command)?,
        Command::Sbom { command } => sbom::run(command, &root)?,
        Command::Readiness { command } => return readiness::run(command, &root),
    }
    Ok(true)
}

fn workspace_root() -> Result<PathBuf, String> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .ok_or_else(|| "cannot resolve workspace root".into())
}

fn release_version(root: &Path) -> Result<String, String> {
    let package = read_json(&root.join("apps/macos/package.json"))?;
    let tauri = read_json(&root.join("apps/macos/src-tauri/tauri.conf.json"))?;
    let cargo = read_toml(&root.join("Cargo.toml"))?;
    let sources = [
        package.get("version").and_then(serde_json::Value::as_str),
        tauri.get("version").and_then(serde_json::Value::as_str),
        cargo
            .get("workspace")
            .and_then(|value| value.get("package"))
            .and_then(|value| value.get("version"))
            .and_then(toml::Value::as_str),
    ];
    let version = sources[0].ok_or_else(|| "package.json has no version".to_string())?;
    if sources.iter().any(|value| *value != Some(version)) {
        return Err("release versions do not match".into());
    }
    validate_version(version)?;
    Ok(version.into())
}

fn validate_version(value: &str) -> Result<(), String> {
    semver::Version::parse(value)
        .map(|_| ())
        .map_err(|_| format!("invalid SemVer release version: {value}"))
}

fn read_json(path: &Path) -> Result<serde_json::Value, String> {
    let data =
        std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let value: serde_json::Value = serde_json::from_slice(&data)
        .map_err(|error| format!("invalid {}: {error}", path.display()))?;
    value
        .is_object()
        .then_some(value)
        .ok_or_else(|| format!("{} must contain a JSON object", path.display()))
}

fn read_toml(path: &Path) -> Result<toml::Value, String> {
    let data = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    toml::from_str(&data).map_err(|error| format!("invalid {}: {error}", path.display()))
}

fn sha256(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest)
        .map_err(|error| format!("cannot hash {}: {error}", path.display()))?;
    Ok(format!("{:x}", digest.finalize()))
}

fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| "output has no parent".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("cannot create temporary output: {error}"))?;
    temporary
        .write_all(data)
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|error| format!("cannot protect {}: {error}", path.display()))?;
    }
    temporary
        .persist(path)
        .map_err(|error| format!("cannot commit {}: {}", path.display(), error.error))?;
    Ok(())
}

fn atomic_json(path: &Path, value: &serde_json::Value, mode: u32) -> Result<(), String> {
    let mut data = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    data.push(b'\n');
    atomic_write(path, &data, mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_validation_matches_release_contract() {
        for valid in ["0.1.0", "1.2.3-beta.1", "1.2.3+build.7"] {
            assert!(validate_version(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "", "1", "1.2", "1.2.x", "1.2.3/4", "v1.2.3", "01.2.3", "1.2.3-", "1.2.3+",
        ] {
            assert!(validate_version(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn toml_reader_accepts_workspace_documents() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("Cargo.toml");
        std::fs::write(&path, "[workspace.package]\nversion = \"1.2.3\"\n").expect("write TOML");
        assert_eq!(
            read_toml(&path)
                .expect("parse TOML")
                .get("workspace")
                .and_then(|value| value.get("package"))
                .and_then(|value| value.get("version"))
                .and_then(toml::Value::as_str),
            Some("1.2.3")
        );
    }
}
