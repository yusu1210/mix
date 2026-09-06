use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Mutex;
use std::thread;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WindowEvent};
use url::Url;
use uuid::Uuid;

const TRAY_ID: &str = "mix-main-tray";
const TRAY_EVENT: &str = "mix-tray-action";
const TRAY_OPEN_ID: &str = "mix-tray-open";
const TRAY_SESSIONS_ID: &str = "mix-tray-sessions";
const TRAY_QUIT_ID: &str = "mix-tray-quit";
const TRAY_SWITCH_PREFIX: &str = "mix-tray-switch:";
const MAX_TRAY_CLIENTS: usize = 12;
const MAX_TRAY_PROFILES_PER_CLIENT: usize = 12;
const MAX_TRAY_LABEL_CHARS: usize = 64;

#[derive(Clone, Serialize)]
struct ControlPlaneInfo {
    base_url: String,
    token: String,
}

#[derive(Clone, Serialize)]
struct UpdaterCapability {
    enabled: bool,
    automatic_checks: bool,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TrayLanguage {
    Zh,
    En,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TrayHealth {
    Starting,
    Healthy,
    Attention,
}

#[derive(Deserialize)]
struct TrayProfileState {
    name: String,
    label: String,
    active: bool,
}

#[derive(Deserialize)]
struct TrayClientState {
    name: String,
    label: String,
    active_profile: Option<String>,
    profiles: Vec<TrayProfileState>,
    switchable: bool,
}

#[derive(Deserialize)]
struct TrayState {
    language: TrayLanguage,
    health_status: TrayHealth,
    clients: Vec<TrayClientState>,
}

#[derive(Clone, Serialize)]
struct TrayActionPayload {
    kind: &'static str,
    view: Option<&'static str>,
    app: Option<String>,
    profile: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum TrayMenuAction<'a> {
    Open,
    Sessions,
    Switch { app: &'a str, profile: &'a str },
    Quit,
    Ignore,
}

struct TrayCopy {
    starting: &'static str,
    ready: &'static str,
    attention: &'static str,
    current: &'static str,
    not_selected: &'static str,
    switch: &'static str,
    open_client: &'static str,
    sessions: &'static str,
    open: &'static str,
    quit: &'static str,
}

struct LocalControlPlane {
    connection: Mutex<Option<ControlPlaneInfo>>,
    server: Mutex<Option<mix_server::RunningServer>>,
    runtime: Mutex<Option<tokio::runtime::Runtime>>,
}

fn config_path(home_dir: &Path) -> PathBuf {
    home_dir.join(".mix/config.json")
}

fn config_path_with_override(
    home_dir: &Path,
    override_path: Option<&OsStr>,
) -> Result<PathBuf, String> {
    let Some(value) = override_path.filter(|value| !value.is_empty()) else {
        return Ok(config_path(home_dir));
    };
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err("MIX_CONFIG must be an absolute path".into());
    }
    Ok(path)
}

fn updater_enabled_from(value: Option<&str>) -> bool {
    matches!(value, Some("1"))
}
fn release_page_from(value: Option<&str>) -> Option<&str> {
    value.filter(|value| {
        if value.len() > 2048 || value.chars().any(char::is_whitespace) {
            return false;
        }
        if !value
            .get(.."https://".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
            || value["https://".len()..].starts_with('/')
        {
            return false;
        }
        let Ok(url) = Url::parse(value) else {
            return false;
        };
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
    })
}
fn release_page() -> Option<&'static str> {
    release_page_from(option_env!("MIX_RELEASE_PAGE_URL"))
}
fn updater_enabled() -> bool {
    updater_enabled_from(option_env!("MIX_UPDATER_ENABLED")) && release_page().is_some()
}

fn tray_copy(language: TrayLanguage) -> TrayCopy {
    match language {
        TrayLanguage::Zh => TrayCopy {
            starting: "正在启动…",
            ready: "运行正常",
            attention: "需要处理",
            current: "当前",
            not_selected: "未选择",
            switch: "切换",
            open_client: "打开",
            sessions: "最近会话…",
            open: "打开 mix",
            quit: "退出 mix",
        },
        TrayLanguage::En => TrayCopy {
            starting: "Starting…",
            ready: "Ready",
            attention: "Needs attention",
            current: "Current",
            not_selected: "Not selected",
            switch: "Switch",
            open_client: "Open",
            sessions: "Recent sessions…",
            open: "Open mix",
            quit: "Quit mix",
        },
    }
}

fn is_bidi_control(character: char) -> bool {
    matches!(character, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

fn bounded_menu_text(value: &str, fallback: &str) -> String {
    let mut normalized = String::new();
    let mut previous_was_space = false;
    for character in value.chars() {
        if character.is_whitespace() {
            if !previous_was_space && !normalized.is_empty() {
                normalized.push(' ');
            }
            previous_was_space = true;
        } else if !character.is_control() && !is_bidi_control(character) {
            normalized.push(character);
            previous_was_space = false;
        }
    }
    let source = if normalized.trim().is_empty() {
        fallback
    } else {
        normalized.trim()
    };
    let mut bounded: String = source.chars().take(MAX_TRAY_LABEL_CHARS).collect();
    if source.chars().count() > MAX_TRAY_LABEL_CHARS {
        bounded.push('…');
    }
    bounded.replace('&', "&&")
}

fn tray_profile_action_label(
    copy: &TrayCopy,
    client_label: &str,
    profile: &TrayProfileState,
) -> String {
    if profile.active {
        format!("{} {client_label}", copy.open_client)
    } else {
        format!(
            "{} {client_label} → {}",
            copy.switch,
            bounded_menu_text(&profile.label, &profile.name)
        )
    }
}

fn valid_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    value.len() <= 64
        && first.is_ascii_alphanumeric()
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
}

fn parse_tray_menu_action(id: &str) -> TrayMenuAction<'_> {
    match id {
        TRAY_OPEN_ID => TrayMenuAction::Open,
        TRAY_SESSIONS_ID => TrayMenuAction::Sessions,
        TRAY_QUIT_ID => TrayMenuAction::Quit,
        _ => id
            .strip_prefix(TRAY_SWITCH_PREFIX)
            .and_then(|target| target.split_once(':'))
            .filter(|(app, profile)| valid_identifier(app) && valid_identifier(profile))
            .map(|(app, profile)| TrayMenuAction::Switch { app, profile })
            .unwrap_or(TrayMenuAction::Ignore),
    }
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn append_menu_item(
    app: &AppHandle,
    menu: &Menu<tauri::Wry>,
    id: impl Into<tauri::menu::MenuId>,
    text: impl AsRef<str>,
    enabled: bool,
) -> Result<(), String> {
    let item = MenuItem::with_id(app, id, text, enabled, None::<&str>)
        .map_err(|error| error.to_string())?;
    menu.append(&item).map_err(|error| error.to_string())
}

fn append_separator(app: &AppHandle, menu: &Menu<tauri::Wry>) -> Result<(), String> {
    menu.append(&PredefinedMenuItem::separator(app).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())
}

fn validate_tray_clients(clients: &[TrayClientState]) -> Result<(), String> {
    if clients.len() > MAX_TRAY_CLIENTS {
        return Err(format!(
            "tray supports at most {MAX_TRAY_CLIENTS} configured clients"
        ));
    }
    let mut identifiers = std::collections::HashSet::new();
    if clients.iter().any(|client| {
        let mut profiles = std::collections::HashSet::new();
        !valid_identifier(&client.name)
            || !identifiers.insert(&client.name)
            || client.profiles.len() > MAX_TRAY_PROFILES_PER_CLIENT
            || client
                .profiles
                .iter()
                .any(|profile| !valid_identifier(&profile.name) || !profiles.insert(&profile.name))
    }) {
        return Err("tray client and profile identifiers must be bounded, unique, and safe".into());
    }
    Ok(())
}

fn build_tray_menu(
    app: &AppHandle,
    state: &TrayState,
    interactive: bool,
) -> Result<Menu<tauri::Wry>, String> {
    validate_tray_clients(&state.clients)?;
    let copy = tray_copy(state.language);
    let menu = Menu::new(app).map_err(|error| error.to_string())?;
    let health = match state.health_status {
        TrayHealth::Starting => copy.starting,
        TrayHealth::Healthy => copy.ready,
        TrayHealth::Attention => copy.attention,
    };
    append_menu_item(
        app,
        &menu,
        "mix-tray-status",
        format!("mix · {health}"),
        false,
    )?;
    if !state.clients.is_empty() {
        append_separator(app, &menu)?;
        for client in &state.clients {
            let label = bounded_menu_text(&client.label, &client.name);
            let active = client
                .active_profile
                .as_deref()
                .map(|value| bounded_menu_text(value, copy.not_selected))
                .unwrap_or_else(|| copy.not_selected.into());
            append_menu_item(
                app,
                &menu,
                format!("mix-tray-current:{}", client.name),
                format!("{label} · {}: {active}", copy.current),
                false,
            )?;
            for profile in &client.profiles {
                append_menu_item(
                    app,
                    &menu,
                    format!("{TRAY_SWITCH_PREFIX}{}:{}", client.name, profile.name),
                    tray_profile_action_label(&copy, &label, profile),
                    interactive && client.switchable,
                )?;
            }
        }
    }
    append_separator(app, &menu)?;
    append_menu_item(app, &menu, TRAY_SESSIONS_ID, copy.sessions, interactive)?;
    append_menu_item(app, &menu, TRAY_OPEN_ID, copy.open, true)?;
    append_separator(app, &menu)?;
    append_menu_item(app, &menu, TRAY_QUIT_ID, copy.quit, true)?;
    Ok(menu)
}

fn handle_tray_menu_event(app: &AppHandle, id: &str) {
    match parse_tray_menu_action(id) {
        TrayMenuAction::Open => show_main_window(app),
        TrayMenuAction::Sessions => {
            show_main_window(app);
            let _ = app.emit(
                TRAY_EVENT,
                TrayActionPayload {
                    kind: "view",
                    view: Some("sessions"),
                    app: None,
                    profile: None,
                },
            );
        }
        TrayMenuAction::Switch {
            app: client,
            profile,
        } => {
            show_main_window(app);
            let _ = app.emit(
                TRAY_EVENT,
                TrayActionPayload {
                    kind: "switch",
                    view: None,
                    app: Some(client.into()),
                    profile: Some(profile.into()),
                },
            );
        }
        TrayMenuAction::Quit => app.exit(0),
        TrayMenuAction::Ignore => {}
    }
}

fn tray_template_icon() -> Result<tauri::image::Image<'static>, String> {
    let image = tauri::image::Image::from_bytes(include_bytes!("../icons/tray-template.png"))
        .map_err(|error| format!("cannot decode the embedded Mix tray icon: {error}"))?;
    if image.width() != 44 || image.height() != 44 {
        return Err("the embedded Mix tray icon must be 44x44 pixels".into());
    }
    Ok(image)
}

fn setup_tray(app: &AppHandle) -> Result<(), String> {
    let state = TrayState {
        language: TrayLanguage::En,
        health_status: TrayHealth::Starting,
        clients: Vec::new(),
    };
    let menu = build_tray_menu(app, &state, false)?;
    TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .tooltip("mix · Starting…")
        .icon(tray_template_icon()?)
        .icon_as_template(true)
        .on_menu_event(|app, event| handle_tray_menu_event(app, &event.id().0))
        .build(app)
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
fn update_tray(app: AppHandle, state: TrayState) -> Result<(), String> {
    let copy = tray_copy(state.language);
    let tooltip = match state.health_status {
        TrayHealth::Starting => format!("mix · {}", copy.starting),
        TrayHealth::Healthy => format!("mix · {}", copy.ready),
        TrayHealth::Attention => format!("mix · {}", copy.attention),
    };
    let menu = build_tray_menu(&app, &state, true)?;
    let tray = app
        .tray_by_id(TRAY_ID)
        .ok_or_else(|| "Mix tray is unavailable".to_string())?;
    tray.set_menu(Some(menu))
        .map_err(|error| error.to_string())?;
    tray.set_tooltip(Some(tooltip))
        .map_err(|error| error.to_string())
}

impl Default for LocalControlPlane {
    fn default() -> Self {
        Self {
            connection: Mutex::new(None),
            server: Mutex::new(None),
            runtime: Mutex::new(None),
        }
    }
}

#[tauri::command]
fn control_plane(state: State<'_, LocalControlPlane>) -> Result<ControlPlaneInfo, String> {
    state
        .connection
        .lock()
        .map_err(|_| "local control-plane state is unavailable".to_string())?
        .clone()
        .ok_or_else(|| "local control plane is not ready".into())
}

#[tauri::command]
fn updater_capability() -> UpdaterCapability {
    let enabled = updater_enabled();
    UpdaterCapability {
        enabled,
        automatic_checks: enabled,
    }
}

#[tauri::command]
fn open_third_party_licenses(app: tauri::AppHandle) -> Result<(), String> {
    let path = app
        .path()
        .resource_dir()
        .map_err(|error| error.to_string())?
        .join("THIRD-PARTY-LICENSES.html");
    if !path.is_file() {
        return Err("bundled third-party license evidence is missing".into());
    }
    #[cfg(target_os = "macos")]
    {
        let mut child = ProcessCommand::new("/usr/bin/open")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("cannot open third-party licenses: {error}"))?;
        thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Err("third-party licenses can only be opened by the macOS app".into())
    }
}

#[tauri::command]
fn open_latest_release() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let page =
            release_page().ok_or_else(|| "Mix release page is not configured".to_string())?;
        let mut child = ProcessCommand::new("/usr/bin/open")
            .arg(page)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("cannot open the Mix release page: {error}"))?;
        thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("the Mix release page can only be opened by the macOS app".into())
    }
}

fn stop_control_plane(state: State<'_, LocalControlPlane>) {
    let server = state.server.lock().ok().and_then(|mut value| value.take());
    if let Some(server) = server {
        if let Ok(runtime) = state.runtime.lock() {
            if let Some(runtime) = runtime.as_ref() {
                runtime.block_on(server.shutdown());
            }
        }
    }
    if let Ok(mut connection) = state.connection.lock() {
        connection.take();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app)
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init());
    let builder = if updater_enabled() {
        builder.plugin(tauri_plugin_updater::Builder::new().build())
    } else {
        builder
    };
    let app =
        builder
            .manage(LocalControlPlane::default())
            .invoke_handler(tauri::generate_handler![
                control_plane,
                updater_capability,
                open_third_party_licenses,
                open_latest_release,
                update_tray
            ])
            .setup(|app| {
                setup_tray(app.handle()).map_err(std::io::Error::other)?;
                let home_dir = app.path().home_dir()?;
                let config_path =
                    config_path_with_override(&home_dir, std::env::var_os("MIX_CONFIG").as_deref())
                        .map_err(std::io::Error::other)?;
                let service = std::sync::Arc::new(
                    mix_core::MixService::new(config_path)
                        .map_err(|error| std::io::Error::other(error.to_string()))?,
                );
                service
                    .initialize()
                    .map_err(|error| std::io::Error::other(error.to_string()))?;
                let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
                let runtime = tokio::runtime::Runtime::new()?;
                let server = runtime
                    .block_on(mix_server::start(
                        service,
                        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                        0,
                        token.clone(),
                        None,
                    ))
                    .map_err(|error| std::io::Error::other(error.to_string()))?;
                let connection = ControlPlaneInfo {
                    base_url: format!("http://{}", server.address),
                    token,
                };
                let state = app.state::<LocalControlPlane>();
                *state.server.lock().map_err(|_| {
                    std::io::Error::other("control-plane server state is poisoned")
                })? = Some(server);
                *state.runtime.lock().map_err(|_| {
                    std::io::Error::other("control-plane runtime state is poisoned")
                })? = Some(runtime);
                *state.connection.lock().map_err(|_| {
                    std::io::Error::other("control-plane connection state is poisoned")
                })? = Some(connection);
                Ok(())
            })
            .on_window_event(|window, event| {
                if window.label() == "main" {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                }
            })
            .build(tauri::generate_context!());
    let app = match app {
        Ok(app) => app,
        Err(error) => {
            eprintln!("Mix could not start: {error}");
            std::process::exit(1);
        }
    };
    app.run(|app, event| match event {
        RunEvent::Exit => stop_control_plane(app.state::<LocalControlPlane>()),
        #[cfg(target_os = "macos")]
        RunEvent::Reopen {
            has_visible_windows: false,
            ..
        } => show_main_window(app),
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tray_identifiers_are_safe() {
        for value in ["codex", "claude-code", "team.codex_2", "A1"] {
            assert!(valid_identifier(value));
        }
        for value in ["", "-codex", "codex/team", "codex:team", "账户", "a b"] {
            assert!(!valid_identifier(value));
        }
        assert!(!valid_identifier(&"a".repeat(65)));
    }

    #[test]
    fn tray_distinguishes_current_activation_from_account_switching() {
        let copy = tray_copy(TrayLanguage::Zh);
        let current = TrayProfileState {
            name: "personal".into(),
            label: "Personal".into(),
            active: true,
        };
        let target = TrayProfileState {
            name: "team".into(),
            label: "Team".into(),
            active: false,
        };

        assert_eq!(
            tray_profile_action_label(&copy, "Codex", &current),
            "打开 Codex"
        );
        assert_eq!(
            tray_profile_action_label(&copy, "Codex", &target),
            "切换 Codex → Team"
        );
    }
    #[test]
    fn tray_state_rejects_ambiguous_or_unbounded_profile_targets() {
        let client = |profiles: Vec<&str>| TrayClientState {
            name: "codex".into(),
            label: "Codex".into(),
            active_profile: Some("account-1".into()),
            profiles: profiles
                .into_iter()
                .map(|name| TrayProfileState {
                    name: name.into(),
                    label: name.into(),
                    active: false,
                })
                .collect(),
            switchable: true,
        };

        assert!(validate_tray_clients(&[client(vec!["account-2"])]).is_ok());
        assert!(validate_tray_clients(&[client(vec!["account-2", "account-2"])]).is_err());
        assert!(validate_tray_clients(&[client(vec!["../../secret"])]).is_err());
        assert!(validate_tray_clients(&[client(vec![
            "account-0",
            "account-1",
            "account-2",
            "account-3",
            "account-4",
            "account-5",
            "account-6",
            "account-7",
            "account-8",
            "account-9",
            "account-10",
            "account-11",
            "account-12",
        ])])
        .is_err());
    }
    #[test]
    fn tray_action_rejects_path_traversal() {
        assert_eq!(
            parse_tray_menu_action("mix-tray-switch:codex:account-2"),
            TrayMenuAction::Switch {
                app: "codex",
                profile: "account-2"
            }
        );
        for value in [
            "mix-tray-switch:codex",
            "mix-tray-switch:../../secret:account-2",
            "mix-tray-switch:codex:../../secret",
            "mix-tray-switch:codex:account:extra",
        ] {
            assert_eq!(parse_tray_menu_action(value), TrayMenuAction::Ignore);
        }
    }
    #[test]
    fn config_override_must_be_absolute() {
        let home = PathBuf::from("/tmp/mix-home");
        assert_eq!(
            config_path_with_override(&home, Some(OsStr::new("/tmp/custom.json"))).unwrap(),
            PathBuf::from("/tmp/custom.json")
        );
        assert!(config_path_with_override(&home, Some(OsStr::new("relative.json"))).is_err());
        assert_eq!(
            config_path_with_override(&home, Some(OsStr::new(""))).unwrap(),
            home.join(".mix/config.json")
        );
    }
    #[test]
    fn updater_requires_explicit_release_flag() {
        assert!(updater_enabled_from(Some("1")));
        assert!(!updater_enabled_from(Some("true")));
        assert!(!updater_enabled_from(None));
    }

    #[test]
    fn release_page_requires_a_bounded_https_url() {
        assert_eq!(
            release_page_from(Some("https://example.com/mix/releases/latest")),
            Some("https://example.com/mix/releases/latest")
        );
        assert!(release_page_from(Some("http://example.com/releases")).is_none());
        assert!(release_page_from(Some("https://example.com/bad url")).is_none());
        assert!(release_page_from(Some("https://user@example.com/releases")).is_none());
        assert!(release_page_from(Some("https://example.com/releases#download")).is_none());
        assert!(release_page_from(Some("https:///missing-host")).is_none());
    }
    #[test]
    fn updater_capability_is_check_only() {
        let capability = include_str!("../capabilities/default.json");
        assert!(capability.contains("updater:allow-check"));
        for forbidden in [
            "updater:allow-download",
            "updater:allow-install",
            "process:allow-restart",
        ] {
            assert!(!capability.contains(forbidden));
        }
    }
}
