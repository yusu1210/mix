use clap::{Parser, Subcommand};
use mix_core::{AdapterKind, ConfigStore, Error, ErrorCode, MixService, ProfileCategory};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "mix",
    version,
    about = "Switch AI coding accounts without losing native sessions"
)]
struct Cli {
    /// Use an alternate Mix config file.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize Mix state and run safe pending cleanup.
    Init,
    /// Show connected clients, saved accounts, and recovery state.
    Status,
    /// Find supported AI coding clients installed on this machine.
    Discover,
    /// Connect one discovered client to Mix.
    Connect {
        /// Client adapter to connect, such as codex or claude.
        #[arg(value_name = "CLIENT")]
        client: AdapterKind,
    },
    /// Add the current Codex account, or create a Claude environment.
    Add {
        /// Connected client name; defaults to codex.
        #[arg(value_name = "CLIENT", default_value = "codex")]
        client: String,
        /// Editable account label shown in Mix.
        #[arg(long)]
        label: Option<String>,
    },
    /// Use a saved Codex account or select a default client environment.
    #[command(
        long_about = "Use a saved Codex account, or select the default isolated environment for another client.\n\nCodex switching is verified and reversible. Environment selection changes only Mix's default and never projects files into the client's native home. Run `mix use <ACCOUNT>` for Codex, or `mix use <CLIENT> <ENVIRONMENT>` for another connected client."
    )]
    Use {
        /// Codex account, or client when ENVIRONMENT is also provided.
        #[arg(value_name = "ACCOUNT|CLIENT")]
        target: String,
        /// Environment when CLIENT is provided first.
        #[arg(value_name = "ENVIRONMENT")]
        profile: Option<String>,
    },
    /// Launch one account or environment in an isolated project runtime.
    Run {
        /// Connected client name.
        #[arg(value_name = "CLIENT")]
        client: String,
        /// Saved account or isolated environment.
        #[arg(value_name = "ACCOUNT|ENVIRONMENT")]
        profile: String,
        /// Project directory to open.
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Sync the active account's latest login and Provider overlay.
    Sync {
        /// Connected client name.
        #[arg(default_value = "codex")]
        client: String,
    },
    /// List bounded, read-only native session metadata.
    Sessions {
        /// Connected client name, or all.
        #[arg(default_value = "all")]
        client: String,
        /// Search session titles and paths.
        #[arg(long)]
        query: Option<String>,
        /// Maximum number of results.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Resume a verified native session through the client's official CLI.
    Resume {
        /// Connected client name.
        #[arg(value_name = "CLIENT")]
        client: String,
        /// Native resume identifier returned by `mix sessions`.
        #[arg(value_name = "RESUME_ID")]
        resume_id: String,
    },
    /// Finish cleanup or rollback for an interrupted account switch.
    Recover,
    /// Print redacted local diagnostics.
    Diagnostics,
    /// Open the shared Mix interface on authenticated loopback HTTP.
    Web {
        /// Loopback port; zero chooses a free random port.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Development UI directory; packaged releases discover this automatically.
        #[arg(long)]
        ui: Option<PathBuf>,
        /// Print the authenticated URL without opening the default browser.
        #[arg(long)]
        no_open: bool,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(error) = run(cli).await {
        eprintln!("{}: {}", error.code.as_str(), error);
        std::process::exit(2);
    }
}

async fn run(cli: Cli) -> mix_core::Result<()> {
    let config = cli.config.unwrap_or(ConfigStore::default_path()?);
    let service = Arc::new(MixService::new(config)?);
    let initialized = service.initialize()?;
    match cli.command {
        Command::Init => emit(
            cli.json,
            serde_json::to_value(initialized)?,
            HumanOutput::Summary("Mix is ready"),
        ),
        Command::Status => emit(
            cli.json,
            serde_json::to_value(service.state()?)?,
            HumanOutput::Status,
        ),
        Command::Discover => emit(
            cli.json,
            serde_json::to_value(service.discovery()?)?,
            HumanOutput::Discovery,
        ),
        Command::Connect { client } => {
            let discovery = service
                .discovery()?
                .into_iter()
                .find(|item| item.adapter == client)
                .ok_or_else(|| mix_core::Error::not_found("client", client.as_str()))?;
            emit(
                cli.json,
                service.register_client(&discovery.name, client, discovery.live_dir)?,
                HumanOutput::Summary("Client connected"),
            )
        }
        Command::Add { client, label } => match service.profile_category(&client)? {
            ProfileCategory::Account => emit(
                cli.json,
                service.capture_current_account(&client, None, label.as_deref())?,
                HumanOutput::Summary("Account added"),
            ),
            ProfileCategory::Environment => emit(
                cli.json,
                service.add_profile(
                    &client,
                    None,
                    mix_core::ProfileInput {
                        label,
                        auth_strategy: Some(mix_core::AuthStrategy::Interactive),
                        ..Default::default()
                    },
                )?,
                HumanOutput::Summary("Environment added"),
            ),
        },
        Command::Use { target, profile } => {
            let (client, selector) = if let Some(profile) = profile {
                (target, profile)
            } else {
                ("codex".into(), target)
            };
            let category = service.profile_category(&client)?;
            emit(
                cli.json,
                serde_json::to_value(service.switch(&client, &selector)?)?,
                HumanOutput::Use(category),
            )
        }
        Command::Run {
            client,
            profile,
            workspace,
        } => emit(
            cli.json,
            service.run(&client, &profile, workspace.as_deref())?,
            HumanOutput::Summary("Client launched"),
        ),
        Command::Sync { client } => emit(
            cli.json,
            service.sync_active_account(&client)?,
            HumanOutput::Summary("Account synchronized"),
        ),
        Command::Sessions {
            client,
            query,
            limit,
        } => emit(
            cli.json,
            serde_json::to_value(service.sessions(
                Some(&client),
                query.as_deref(),
                None,
                None,
                limit,
                0,
            )?)?,
            HumanOutput::Sessions,
        ),
        Command::Resume { client, resume_id } => emit(
            cli.json,
            service.resume_session(&client, &resume_id)?,
            HumanOutput::Summary("Session launched"),
        ),
        Command::Recover => emit(
            cli.json,
            service.recover_interrupted_switch()?,
            HumanOutput::Summary("Interrupted switch recovered"),
        ),
        Command::Diagnostics => emit(cli.json, service.diagnostics()?, HumanOutput::Diagnostics),
        Command::Web { port, ui, no_open } => {
            let ui = Some(resolve_ui_root(ui)?);
            let token = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            let server = mix_server::start(
                service,
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                port,
                token.clone(),
                ui,
            )
            .await?;
            let url = format!("http://{}/#token={token}", server.address);
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({"status":"ready","url":url,"address":server.address})
                );
            } else {
                println!("Mix Local Web: {url}");
            }
            use std::io::Write;
            std::io::stdout()
                .flush()
                .map_err(|error| Error::io("cannot publish the Local Web URL", error))?;
            if should_open_browser(no_open, cli.json) {
                if let Err(error) = open_browser(&url) {
                    eprintln!("Local Web is ready, but the browser could not be opened: {error}");
                }
            }
            tokio::signal::ctrl_c()
                .await
                .map_err(|error| mix_core::Error::io("cannot wait for shutdown signal", error))?;
            server.shutdown().await;
        }
    }
    Ok(())
}

fn open_browser(url: &str) -> mix_core::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = ProcessCommand::new("/usr/bin/open");
    #[cfg(target_os = "linux")]
    let mut command = ProcessCommand::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = ProcessCommand::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return Err(Error::new(
        ErrorCode::MixUnsupported,
        "opening a browser is unsupported on this platform",
    ));

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        let mut child = command
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| Error::io("cannot open the default browser", error))?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

fn should_open_browser(no_open: bool, json: bool) -> bool {
    !no_open && !json
}

fn resolve_ui_root(explicit: Option<PathBuf>) -> mix_core::Result<PathBuf> {
    if let Some(root) = explicit {
        return validated_ui_root(&root);
    }
    let executable = std::env::current_exe()
        .map_err(|error| Error::io("cannot locate the Mix executable", error))?;
    packaged_ui_candidates(&executable)
        .into_iter()
        .find_map(|candidate| validated_ui_root(&candidate).ok())
        .ok_or_else(|| {
            Error::new(
                ErrorCode::MixNotFound,
                "Mix visual interface is not installed; reinstall Mix or pass --ui <dist>",
            )
        })
}

fn packaged_ui_candidates(executable: &Path) -> Vec<PathBuf> {
    let Some(executable_dir) = executable.parent() else {
        return Vec::new();
    };
    let mut candidates = Vec::with_capacity(2);
    if executable_dir.file_name().and_then(|name| name.to_str()) == Some("MacOS") {
        if let Some(contents) = executable_dir.parent() {
            candidates.push(contents.join("Resources/ui"));
        }
    }
    candidates.push(executable_dir.join("../share/mix/ui"));
    candidates
}

fn validated_ui_root(root: &Path) -> mix_core::Result<PathBuf> {
    let metadata = std::fs::symlink_metadata(root).map_err(|_| {
        Error::new(
            ErrorCode::MixNotFound,
            format!("Mix visual interface is missing: {}", root.display()),
        )
    })?;
    let index = root.join("index.html");
    let index_metadata = std::fs::symlink_metadata(&index).map_err(|_| {
        Error::new(
            ErrorCode::MixNotFound,
            format!("Mix visual interface has no index.html: {}", root.display()),
        )
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || index_metadata.file_type().is_symlink()
        || !index_metadata.is_file()
    {
        return Err(Error::invalid(format!(
            "Mix visual interface path is unsafe: {}",
            root.display()
        )));
    }
    root.canonicalize()
        .map_err(|error| Error::io("cannot resolve the Mix visual interface", error))
}

enum HumanOutput {
    Summary(&'static str),
    Use(ProfileCategory),
    Status,
    Discovery,
    Sessions,
    Diagnostics,
}

fn emit(json: bool, value: serde_json::Value, human: HumanOutput) {
    if json {
        println!("{value:#}");
    } else {
        println!("{}", render_human(&value, human));
    }
}

fn render_human(value: &serde_json::Value, output: HumanOutput) -> String {
    match output {
        HumanOutput::Summary(heading) => render_summary(value, heading),
        HumanOutput::Use(ProfileCategory::Account) => render_summary(
            value,
            if value["status"].as_str() == Some("already_active") {
                "Current account activated"
            } else {
                "Account switched"
            },
        ),
        HumanOutput::Use(ProfileCategory::Environment) => {
            render_summary(value, "Default environment selected")
        }
        HumanOutput::Status => {
            let health = value
                .pointer("/health/status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let mut lines = vec![format!("Mix status · {health}")];
            for app in value["apps"].as_array().into_iter().flatten() {
                let name = string(app, "name", "unknown");
                let status = string(app, "status", "unknown");
                let active = app
                    .get("active")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("none");
                lines.push(format!("  {name} · {status} · active: {active}"));
                for profile in app["profiles"].as_array().into_iter().flatten() {
                    lines.push(format!(
                        "    {} ({})",
                        string(profile, "display_label", "unnamed"),
                        string(profile, "name", "unknown")
                    ));
                }
            }
            if value["apps"].as_array().is_none_or(Vec::is_empty) {
                lines.push("  No clients connected".into());
            }
            lines.join("\n")
        }
        HumanOutput::Discovery => {
            let mut lines = vec!["Detected clients".into()];
            for client in value.as_array().into_iter().flatten() {
                lines.push(format!(
                    "  {} · {} · {}",
                    string(client, "name", "unknown"),
                    if client["installed"].as_bool() == Some(true) {
                        "installed"
                    } else {
                        "not installed"
                    },
                    if client["configured"].as_bool() == Some(true) {
                        "connected"
                    } else {
                        "not connected"
                    }
                ));
                if let Some(path) = client["live_dir"].as_str() {
                    lines.push(format!("    {path}"));
                }
            }
            lines.join("\n")
        }
        HumanOutput::Sessions => {
            let rows = value.as_array().map_or(&[][..], Vec::as_slice);
            let mut lines = vec![format!("Sessions · {}", rows.len())];
            for session in rows {
                lines.push(format!(
                    "  [{}] {} · {}",
                    string(session, "recoverability", "?"),
                    string(session, "app", "unknown"),
                    string(session, "title", "Untitled session")
                ));
                if let Some(cwd) = session["cwd"].as_str() {
                    lines.push(format!("    {cwd}"));
                }
                if let Some(resume_id) = session["resume_id"].as_str() {
                    lines.push(format!("    resume id: {resume_id}"));
                }
            }
            lines.join("\n")
        }
        HumanOutput::Diagnostics => format!("{value:#}"),
    }
}

fn render_summary(value: &serde_json::Value, heading: &str) -> String {
    let mut lines = vec![heading.to_string()];
    for key in [
        "status",
        "app",
        "profile",
        "from_profile",
        "to_profile",
        "warning",
        "activity_warning",
    ] {
        if let Some(value) = value.get(key).and_then(serde_json::Value::as_str) {
            lines.push(format!("  {}: {value}", key.replace('_', " ")));
        }
    }
    lines.join("\n")
}

fn string<'a>(value: &'a serde_json::Value, key: &str, fallback: &'a str) -> &'a str {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn add_defaults_to_codex_without_exposing_an_internal_name() {
        let parsed = Cli::try_parse_from(["mix", "add"]).expect("default add command");
        let Command::Add { client, label } = parsed.command else {
            panic!("expected add command");
        };
        assert_eq!(client, "codex");
        assert_eq!(label, None);
        assert!(Cli::try_parse_from(["mix", "add", "--name", "account-7"]).is_err());
    }

    #[test]
    fn local_web_opens_for_people_and_stays_headless_for_automation() {
        let default = Cli::try_parse_from(["mix", "web"]).expect("default web command");
        let Command::Web { no_open, port, .. } = default.command else {
            panic!("expected web command");
        };
        assert_eq!(port, 0);
        assert!(!no_open);
        assert!(should_open_browser(no_open, default.json));

        let automated =
            Cli::try_parse_from(["mix", "--json", "web", "--no-open", "--port", "41821"])
                .expect("headless web command");
        let Command::Web { no_open, port, .. } = automated.command else {
            panic!("expected web command");
        };
        assert_eq!(port, 41821);
        assert!(no_open);
        assert!(!should_open_browser(no_open, automated.json));
        assert!(!should_open_browser(false, true));
    }

    #[test]
    fn human_status_exposes_clients_accounts_and_active_profile() {
        let output = render_human(
            &json!({
                "health":{"status":"healthy"},
                "apps":[{"name":"codex","status":"ready","active":"account-1","profiles":[{"name":"account-1","display_label":"Personal"}]}]
            }),
            HumanOutput::Status,
        );
        assert!(output.contains("codex · ready · active: account-1"));
        assert!(output.contains("Personal (account-1)"));
    }

    #[test]
    fn human_switch_output_distinguishes_activation_from_a_state_change() {
        let activated = render_human(
            &json!({"status":"already_active","app":"codex","to_profile":"personal"}),
            HumanOutput::Use(ProfileCategory::Account),
        );
        let switched = render_human(
            &json!({"status":"switched","app":"codex","to_profile":"team"}),
            HumanOutput::Use(ProfileCategory::Account),
        );

        assert!(activated.starts_with("Current account activated\n"));
        assert!(switched.starts_with("Account switched\n"));
    }

    #[test]
    fn human_environment_selection_is_not_described_as_an_account_switch() {
        let output = render_human(
            &json!({"status":"selected","app":"claude","to_profile":"work"}),
            HumanOutput::Use(ProfileCategory::Environment),
        );

        assert!(output.starts_with("Default environment selected\n"));
        assert!(!output.contains("Account switched"));
    }

    #[test]
    fn human_sessions_are_compact_and_actionable() {
        let output = render_human(
            &json!([{"app":"codex","recoverability":"A","title":"Fix checkout","cwd":"/work/mix","resume_id":"session-1"}]),
            HumanOutput::Sessions,
        );
        assert!(output.contains("Sessions · 1"));
        assert!(output.contains("[A] codex · Fix checkout"));
        assert!(output.contains("/work/mix"));
        assert!(output.contains("resume id: session-1"));
    }

    #[test]
    fn packaged_macos_cli_finds_its_visual_interface() {
        let root = tempdir().expect("temporary directory");
        let contents = root.path().join("mix-cli.app/Contents");
        let executable = contents.join("MacOS/mix");
        let ui = contents.join("Resources/ui");
        std::fs::create_dir_all(executable.parent().expect("executable directory"))
            .expect("create executable directory");
        std::fs::create_dir_all(&ui).expect("create UI directory");
        std::fs::write(ui.join("index.html"), "Mix").expect("write UI index");

        let resolved = packaged_ui_candidates(&executable)
            .into_iter()
            .find_map(|candidate| validated_ui_root(&candidate).ok())
            .expect("packaged UI");
        assert_eq!(resolved, ui.canonicalize().expect("canonical UI"));
    }

    #[test]
    fn explicit_visual_interface_rejects_a_symlink_root() {
        let root = tempdir().expect("temporary directory");
        let ui = root.path().join("ui");
        let link = root.path().join("linked-ui");
        std::fs::create_dir(&ui).expect("create UI directory");
        std::fs::write(ui.join("index.html"), "Mix").expect("write UI index");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&ui, &link).expect("create UI link");

        #[cfg(unix)]
        assert_eq!(
            validated_ui_root(&link)
                .expect_err("symlink must fail")
                .code,
            ErrorCode::MixValidationError
        );
    }
}
