//! Preserve the local workspace when Codex selects an account-scoped renderer.
//! These files are projected only by the stopped-client switch transaction.
use crate::adapter::decode_jwt_claims;
use crate::fs::{read_bounded, MAX_LOCAL_JSON_BYTES};
use crate::{Error, ErrorCode, Result};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub(crate) const STATE: &str = ".codex-global-state.json";
const STATE_BACKUP: &str = ".codex-global-state.json.bak";
pub(crate) const CHECKPOINT: &str = ".mix-desktop-workspace.json";
const ACCOUNTS: &str = "electron-account-renderer-state-v1";
const ATOMS: &str = "electron-persisted-atom-state";
const PROJECTS: &str = "local-projects";
const ASSIGNMENTS: &str = "thread-project-assignments";
const PROJECT_MAPS: &[&str] = &[
    "project-appearances",
    "project-writable-roots",
    "project-files",
    "sidebar-project-thread-orders",
];
const THREAD_MAPS: &[&str] = &[
    "thread-writable-roots",
    "thread-workspace-root-hints",
    "thread-project-membership-host-ids",
    "thread-projectless-output-directories",
];
// The native v1 account store moves these globals together with renderer atoms.
// Retain the complete account slots, but transfer only Workspace's local fields.
const ACCOUNT_GLOBALS: &[&str] = &[
    "open-in-target-preferences",
    "worktree-repository-preferences",
    "desktop-app-beta-preferences-by-account-id",
    "active-workspace-roots",
    "electron-saved-workspace-roots",
    "electron-workspace-root-labels",
    PROJECTS,
    "app-server-project-id-by-legacy-project-id-by-host",
    "app-server-projects-migration-by-host",
    "app-server-pending-project-deletions-by-host",
    "selected-project",
    "project-writable-roots",
    "project-appearances",
    "project-files",
    "pinned-thread-ids",
    "app-server-migrated-pinned-thread-ids-by-host",
    "pinned-project-ids",
    "sidebar-project-thread-orders",
    "sidebar-thread-metadata",
    ASSIGNMENTS,
    "thread-project-membership-host-ids",
    "thread-writable-roots",
    "thread-workspace-root-hints",
    "thread-projectless-output-directories",
    "projectless-thread-ids",
    "selected-remote-host-id",
    "remote-projects",
    "active-remote-project-id",
    "project-order",
    "connection-group-order",
    "remote-cwds-by-host-and-workspace",
    "host-id-remote-control-allowed",
    "added-remote-control-env-ids",
    "remote-project-connection-backfill-completed",
    "remote-connection-auto-connect-by-host-id",
    "queued-follow-ups",
];
const LOCAL_PREFERENCES: &[&str] = &["flat-project-sidebar-preferences-v1"];

#[derive(Default)]
pub(crate) struct Projection {
    pub files: BTreeMap<String, Vec<u8>>,
}

pub(crate) fn is_present(home: &Path) -> bool {
    [STATE, STATE_BACKUP, CHECKPOINT].iter().any(|name| {
        !matches!(std::fs::symlink_metadata(home.join(name)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    })
}

fn read_state(home: &Path) -> Result<Option<Map<String, Value>>> {
    // Both destinations are replaced by the transaction. Reject unsafe paths
    // during preflight even when the other file would be a readable fallback.
    for name in [STATE, STATE_BACKUP] {
        match std::fs::symlink_metadata(home.join(name)) {
            Ok(metadata) if !metadata.file_type().is_file() => return Err(invalid()),
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(Error::io("cannot inspect Codex desktop state", error));
            }
            _ => {}
        }
    }
    let primary = read_object(&home.join(STATE));
    if let Ok(Some(state)) = &primary {
        return Ok(Some(state.clone()));
    }
    match read_object(&home.join(STATE_BACKUP)) {
        Ok(Some(state)) => Ok(Some(state)),
        Ok(None) => primary,
        Err(backup_error) => match primary {
            Err(primary_error) => Err(primary_error),
            Ok(None) => Err(backup_error),
            Ok(Some(_)) => unreachable!("a present primary state was returned above"),
        },
    }
}

fn invalid() -> Error {
    Error::new(ErrorCode::MixConfigInvalid,
        "Codex desktop workspace state has an unsupported or invalid structure; no account was changed")
}

fn read_object(path: &Path) -> Result<Option<Map<String, Value>>> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::io("cannot inspect Codex desktop state", error)),
        Ok(_) => {}
    }
    let bytes = read_bounded(path, MAX_LOCAL_JSON_BYTES, "Codex desktop state")?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    value.as_object().cloned().map(Some).ok_or_else(invalid)
}

fn object(value: Option<&Value>) -> Result<Map<String, Value>> {
    match value {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(value)) => Ok(value.clone()),
        _ => Err(invalid()),
    }
}

fn local_project(id: &str, project: &Value) -> bool {
    !id.starts_with("g-p-")
        && [
            "chatGptBacking",
            "chatGptBackingCreation",
            "retiredChatGptBacking",
            "chatGptBackingRecoveryPending",
        ]
        .iter()
        .all(|key| project.get(*key).is_none_or(Value::is_null))
        && project
            .get("rootPaths")
            .and_then(Value::as_array)
            .is_some_and(|paths| paths.iter().all(Value::is_string))
}

fn ids(map: &Map<String, Value>) -> BTreeSet<String> {
    map.keys().cloned().collect()
}

fn filtered_map(value: Option<&Value>, allowed: &BTreeSet<String>) -> Result<Value> {
    Ok(Value::Object(
        object(value)?
            .into_iter()
            .filter(|(id, _)| allowed.contains(id))
            .collect(),
    ))
}

fn filtered_list(value: Option<&Value>, allowed: &BTreeSet<String>) -> Result<Value> {
    let values = match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(values)) => values.clone(),
        _ => return Err(invalid()),
    };
    if values.iter().any(|value| !value.is_string()) {
        return Err(invalid());
    }
    Ok(Value::Array(
        values
            .into_iter()
            .filter(|value| value.as_str().is_some_and(|id| allowed.contains(id)))
            .collect(),
    ))
}

fn string_list(value: Option<&Value>) -> Result<Vec<String>> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| value.as_str().map(str::to_owned).ok_or_else(invalid))
            .collect(),
        _ => Err(invalid()),
    }
}

fn validate_assignment(value: &Value) -> Result<()> {
    let assignment = value.as_object().ok_or_else(invalid)?;
    let project_id = assignment
        .get("projectId")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    if project_id.is_empty() {
        return Err(invalid());
    }
    match assignment.get("projectKind").and_then(Value::as_str) {
        Some("local") => {
            if let Some(origin) = assignment.get("projectOrigin") {
                if origin.as_str() != Some("chatgpt") {
                    return Err(invalid());
                }
            }
        }
        Some("remote") => {
            if let Some(host_id) = assignment.get("hostId") {
                if !host_id.is_string() {
                    return Err(invalid());
                }
            }
        }
        _ => return Err(invalid()),
    }
    Ok(())
}

fn same_assignment(left: &Value, right: &Value) -> bool {
    let left = left.as_object();
    let right = right.as_object();
    match (
        left.and_then(|value| value.get("projectKind"))
            .and_then(Value::as_str),
        right
            .and_then(|value| value.get("projectKind"))
            .and_then(Value::as_str),
    ) {
        (Some("local"), Some("local")) => {
            left.and_then(|value| value.get("projectId"))
                == right.and_then(|value| value.get("projectId"))
                && left
                    .and_then(|value| value.get("projectOrigin"))
                    .and_then(Value::as_str)
                    == right
                        .and_then(|value| value.get("projectOrigin"))
                        .and_then(Value::as_str)
        }
        (Some("remote"), Some("remote")) => {
            left.and_then(|value| value.get("projectId"))
                == right.and_then(|value| value.get("projectId"))
                && left
                    .and_then(|value| value.get("hostId"))
                    .and_then(Value::as_str)
                    == right
                        .and_then(|value| value.get("hostId"))
                        .and_then(Value::as_str)
        }
        _ => false,
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Workspace {
    globals: Map<String, Value>,
    preferences: Map<String, Value>,
}

impl Workspace {
    fn capture(
        globals: &Map<String, Value>,
        atoms: &Map<String, Value>,
        home: &Path,
    ) -> Result<Self> {
        let all_projects = object(globals.get(PROJECTS))?;
        for (id, project) in &all_projects {
            if !project.is_object()
                || project.get("id").and_then(Value::as_str) != Some(id)
                || project.get("name").and_then(Value::as_str).is_none()
                || !project
                    .get("rootPaths")
                    .and_then(Value::as_array)
                    .is_some_and(|paths| paths.iter().all(Value::is_string))
            {
                return Err(invalid());
            }
        }
        let projects: Map<String, Value> = all_projects
            .into_iter()
            .filter(|(id, project)| local_project(id, project))
            .collect();
        let project_ids = ids(&projects);
        let all_assignments = object(globals.get(ASSIGNMENTS))?;
        for assignment in all_assignments.values() {
            validate_assignment(assignment)?;
        }
        let assignments: Map<String, Value> = all_assignments
            .into_iter()
            .filter(|(_, value)| {
                value.get("projectKind").and_then(Value::as_str) == Some("local")
                    && value
                        .get("projectId")
                        .and_then(Value::as_str)
                        .is_some_and(|id| project_ids.contains(id))
            })
            .collect();
        let mut thread_ids = ids(&assignments);
        for id in string_list(globals.get("projectless-thread-ids"))? {
            if !id.is_empty() {
                thread_ids.insert(id);
            }
        }
        let mut result = Map::from_iter([
            (PROJECTS.into(), Value::Object(projects)),
            (ASSIGNMENTS.into(), Value::Object(assignments)),
        ]);
        for key in PROJECT_MAPS {
            result.insert(
                (*key).into(),
                filtered_map(globals.get(*key), &project_ids)?,
            );
        }
        for key in THREAD_MAPS {
            result.insert((*key).into(), filtered_map(globals.get(*key), &thread_ids)?);
        }
        for key in ["project-order", "pinned-project-ids"] {
            result.insert(key.into(), filtered_list(globals.get(key), &project_ids)?);
        }
        for key in ["pinned-thread-ids", "projectless-thread-ids"] {
            result.insert(key.into(), filtered_list(globals.get(key), &thread_ids)?);
        }
        let roots = result[PROJECTS]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|p| p["rootPaths"].as_array().unwrap())
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        for key in ["active-workspace-roots", "electron-saved-workspace-roots"] {
            result.insert(key.into(), filtered_list(globals.get(key), &roots)?);
        }
        result.insert(
            "electron-workspace-root-labels".into(),
            filtered_map(globals.get("electron-workspace-root-labels"), &roots)?,
        );
        if let Some(selected) = globals.get("selected-project") {
            if selected.get("projectKind").and_then(Value::as_str) == Some("local")
                && selected
                    .get("projectId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| project_ids.contains(id))
            {
                result.insert("selected-project".into(), selected.clone());
            }
        }
        let host = format!("local:{}", home.display());
        let mappings = object(globals.get("app-server-project-id-by-legacy-project-id-by-host"))?;
        result.insert(
            "app-server-project-id-by-legacy-project-id-by-host".into(),
            json!({host.clone(): filtered_map(mappings.get(&host), &project_ids)?}),
        );
        Ok(Self {
            globals: result,
            preferences: LOCAL_PREFERENCES
                .iter()
                .filter_map(|key| atoms.get(*key).map(|v| ((*key).into(), v.clone())))
                .collect(),
        })
    }

    fn apply(
        &self,
        globals: &mut Map<String, Value>,
        atoms: &mut Map<String, Value>,
        home: &Path,
    ) -> Result<()> {
        let before = Self::capture(globals, atoms, home)?;
        let old_projects = ids(before.globals[PROJECTS].as_object().unwrap());
        let old_threads: BTreeSet<String> = object(globals.get(ASSIGNMENTS))?
            .into_iter()
            .filter(|(_, v)| {
                v.get("projectKind").and_then(Value::as_str) == Some("local")
                    && v.get("projectId")
                        .and_then(Value::as_str)
                        .is_some_and(|id| {
                            globals
                                .get(PROJECTS)
                                .and_then(|p| p.get(id))
                                .is_none_or(|p| local_project(id, p))
                        })
            })
            .map(|(id, _)| id)
            .chain(string_list(before.globals.get("projectless-thread-ids"))?)
            .collect();
        for key in std::iter::once(PROJECTS).chain(PROJECT_MAPS.iter().copied()) {
            merge_map(globals, key, &old_projects, &self.globals[key])?;
        }
        // Local assignments are replaced as a set, including removals. Never
        // assign a cloud-backed project or a remote-host thread to a local one.
        merge_map(
            globals,
            ASSIGNMENTS,
            &old_threads,
            &self.globals[ASSIGNMENTS],
        )?;
        for key in THREAD_MAPS {
            merge_map(globals, key, &old_threads, &self.globals[*key])?;
        }
        for key in ["project-order", "pinned-project-ids"] {
            merge_list(globals, key, &old_projects, &self.globals[key])?;
        }
        for key in ["pinned-thread-ids", "projectless-thread-ids"] {
            merge_list(globals, key, &old_threads, &self.globals[key])?;
        }
        for key in [
            "active-workspace-roots",
            "electron-saved-workspace-roots",
            "electron-workspace-root-labels",
        ] {
            globals.insert(key.into(), self.globals[key].clone());
        }
        if let Some(selected) = self.globals.get("selected-project") {
            globals.insert("selected-project".into(), selected.clone());
        } else if globals
            .get("selected-project")
            .and_then(|p| p.get("projectId"))
            .and_then(Value::as_str)
            .is_some_and(|id| old_projects.contains(id))
        {
            globals.remove("selected-project");
        }
        let host = format!("local:{}", home.display());
        let key = "app-server-project-id-by-legacy-project-id-by-host";
        let mut hosts = object(globals.get(key))?;
        merge_map(&mut hosts, &host, &old_projects, &self.globals[key][&host])?;
        globals.insert(key.into(), Value::Object(hosts));
        if before.globals != self.globals {
            let key = "app-server-projects-migration-by-host";
            let mut hosts = object(globals.get(key))?;
            hosts.remove(&host); // Let native idempotent import reconcile memberships.
            globals.insert(key.into(), Value::Object(hosts));
        }
        atoms.extend(self.preferences.clone());
        Ok(())
    }
}

fn merge_map(
    target: &mut Map<String, Value>,
    key: &str,
    old: &BTreeSet<String>,
    new: &Value,
) -> Result<()> {
    let mut value = object(target.get(key))?;
    value.retain(|id, _| !old.contains(id));
    for (id, item) in new.as_object().ok_or_else(invalid)? {
        if value.contains_key(id) && value.get(id) != Some(item) {
            return Err(Error::new(
                ErrorCode::MixConflict,
                "a local workspace identifier conflicts with account-specific Codex state",
            ));
        }
        value.insert(id.clone(), item.clone());
    }
    target.insert(key.into(), Value::Object(value));
    Ok(())
}

fn merge_list(
    target: &mut Map<String, Value>,
    key: &str,
    old: &BTreeSet<String>,
    new: &Value,
) -> Result<()> {
    let mut value = match target.get(key) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(v)) => v.clone(),
        _ => return Err(invalid()),
    };
    if value.iter().any(|item| !item.is_string()) {
        return Err(invalid());
    }
    let new = new.as_array().ok_or_else(invalid)?;
    if new.iter().any(|item| !item.is_string()) {
        return Err(invalid());
    }
    value.retain(|v| !v.as_str().is_some_and(|id| old.contains(id)));
    for item in new {
        if !value.contains(item) {
            value.push(item.clone());
        }
    }
    target.insert(key.into(), Value::Array(value));
    Ok(())
}

fn owner(auth: &Value) -> Result<Option<String>> {
    // Older native logins omit auth_mode but still carry an OAuth account.
    if auth
        .pointer("/tokens/account_id")
        .and_then(Value::as_str)
        .is_none()
    {
        if auth
            .get("OPENAI_API_KEY")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(invalid());
        }
        return Ok(None);
    }
    let expected = auth.pointer("/tokens/account_id").and_then(Value::as_str);
    for key in ["access_token", "id_token"] {
        let Some(claims) = auth
            .pointer(&format!("/tokens/{key}"))
            .and_then(Value::as_str)
            .and_then(decode_jwt_claims)
        else {
            continue;
        };
        let Some(identity) = claims.get("https://api.openai.com/auth") else {
            continue;
        };
        let user = identity
            .get("chatgpt_user_id")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty());
        let account = identity
            .get("chatgpt_account_id")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty());
        if let (Some(user), Some(account)) = (user, account) {
            if Some(account) != expected {
                return Err(invalid());
            }
            return Ok(Some(format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&[user, account])?)
            )));
        }
    }
    Err(Error::new(
        ErrorCode::MixAccountReauthRequired,
        "the saved Codex login has no desktop user identity; sign in to this account again",
    ))
}

fn validate_accounts(accounts: &Map<String, Value>) -> Result<()> {
    if accounts.keys().any(|k| k != "owner" && k != "inactive") {
        return Err(invalid());
    }
    if !matches!(
        accounts.get("owner"),
        Some(Value::Null) | Some(Value::String(_))
    ) {
        return Err(invalid());
    }
    let inactive = accounts
        .get("inactive")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    for slot in inactive.values() {
        let slot = slot.as_object().ok_or_else(invalid)?;
        if slot.keys().any(|k| k != "atoms" && k != "globals")
            || !slot.get("atoms").is_some_and(Value::is_object)
            || !slot.get("globals").is_some_and(Value::is_object)
        {
            return Err(invalid());
        }
    }
    Ok(())
}

pub(crate) fn project(home: &Path, auth: &Value) -> Result<Projection> {
    let checkpoint = read_object(&home.join(CHECKPOINT))?;
    let Some(mut state) = read_state(home)? else {
        return if checkpoint.is_none() {
            Ok(Projection::default())
        } else {
            Err(invalid())
        };
    };
    if checkpoint
        .as_ref()
        .is_some_and(|v| v.get("version") != Some(&json!(1)))
    {
        return Err(invalid());
    }
    let mut accounts = object(
        state
            .get(ACCOUNTS)
            .or_else(|| checkpoint.as_ref().and_then(|c| c.get("accounts"))),
    )?;
    if accounts.is_empty() && checkpoint.is_none() {
        return Ok(Projection::default());
    } // Pre-scoping desktop or CLI only.
    validate_accounts(&accounts)?;
    let mut atoms = object(state.get(ATOMS))?;
    let workspace = if state.contains_key(PROJECTS) {
        Workspace::capture(&state, &atoms, home)?
    } else if let Some(value) = checkpoint.as_ref().and_then(|c| c.get("workspace")) {
        let saved: Workspace = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
        Workspace::capture(&saved.globals, &saved.preferences, home)?
    } else {
        let mut candidate = None;
        for slot in accounts["inactive"].as_object().unwrap().values() {
            let globals = slot["globals"].as_object().unwrap();
            if !globals.contains_key(PROJECTS) {
                continue;
            }
            let captured = Workspace::capture(globals, slot["atoms"].as_object().unwrap(), home)?;
            if captured.globals[PROJECTS].as_object().unwrap().is_empty() {
                continue;
            }
            if candidate.as_ref().is_some_and(|old| old != &captured) {
                return Err(Error::new(
                ErrorCode::MixConflict, "several Codex accounts have different local project catalogs; restore the intended catalog before switching"));
            }
            candidate = Some(captured);
        }
        candidate.unwrap_or(Workspace::capture(&state, &atoms, home)?)
    };
    let target = owner(auth)?;
    // Save native account state as-is before entering an accountless API-key
    // desktop. No source account atoms are exposed in that desktop.
    if let Some(current) = accounts
        .get("owner")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        if state.contains_key(ACCOUNTS) {
            let globals: Map<String, Value> = ACCOUNT_GLOBALS
                .iter()
                .filter_map(|key| state.get(*key).map(|v| ((*key).into(), v.clone())))
                .collect();
            accounts["inactive"]
                .as_object_mut()
                .unwrap()
                .insert(current, json!({"globals":globals,"atoms":atoms}));
        }
    }
    if let Some(target) = target {
        let inactive = accounts["inactive"].as_object_mut().unwrap();
        let slot = inactive
            .entry(target)
            .or_insert_with(|| json!({"atoms":{},"globals":{}}));
        let mut globals = object(slot.get("globals"))?;
        let mut target_atoms = object(slot.get("atoms"))?;
        workspace.apply(&mut globals, &mut target_atoms, home)?;
        *slot = json!({"globals":globals,"atoms":target_atoms});
        // If resuming from API-key mode, there is no current native owner to
        // archive on startup. The target slot is restored by Codex itself.
        if !state.contains_key(ACCOUNTS) {
            accounts.insert("owner".into(), Value::Null);
        }
        state.insert(ACCOUNTS.into(), Value::Object(accounts.clone()));
    } else {
        for key in ACCOUNT_GLOBALS {
            state.remove(*key);
        }
        atoms.clear();
        state.remove(ACCOUNTS);
        accounts.insert("owner".into(), Value::Null);
    }
    workspace.apply(&mut state, &mut atoms, home)?;
    state.insert(ATOMS.into(), Value::Object(atoms));
    let parked = if state.contains_key(ACCOUNTS) {
        Value::Null
    } else {
        Value::Object(accounts)
    };
    let checkpoint = json!({"version":1,"workspace":workspace,"accounts":parked});
    let payload = serde_json::to_vec(&state)?;
    let checkpoint = serde_json::to_vec(&checkpoint)?;
    if payload.len() as u64 > MAX_LOCAL_JSON_BYTES || checkpoint.len() as u64 > MAX_LOCAL_JSON_BYTES
    {
        return Err(invalid());
    }
    Ok(Projection {
        files: BTreeMap::from([
            (STATE.into(), payload.clone()),
            (format!("{STATE}.bak"), payload),
            (CHECKPOINT.into(), checkpoint),
        ]),
    })
}

pub(crate) fn needs_repair(home: &Path, auth: &Value) -> Result<bool> {
    let projection = project(home, auth)?;
    let Some(projected) = projection.files.get(STATE) else {
        return Ok(false);
    };
    let expected: Value = serde_json::from_slice(projected)?;
    let actual = read_state(home)?.ok_or_else(invalid)?;
    let expected =
        Workspace::capture(expected.as_object().ok_or_else(invalid)?, &Map::new(), home)?;
    let actual = Workspace::capture(&actual, &Map::new(), home)?;
    Ok(actual.globals[PROJECTS] != expected.globals[PROJECTS]
        || actual.globals[ASSIGNMENTS] != expected.globals[ASSIGNMENTS])
}

/// Read-only diagnosis. This never refreshes credentials or changes native state.
pub(crate) fn recovery_count(home: &Path) -> Result<Option<usize>> {
    if !is_present(home) {
        return Ok(None);
    }
    let auth = crate::CodexAdapter::parse_auth(&home.join("auth.json"))?;
    let projection = project(home, &auth)?;
    let Some(projected) = projection.files.get(STATE) else {
        return Ok(None);
    };
    let expected: Value = serde_json::from_slice(projected)?;
    let actual = read_state(home)?.ok_or_else(invalid)?;
    let expected =
        Workspace::capture(expected.as_object().ok_or_else(invalid)?, &Map::new(), home)?;
    let actual = Workspace::capture(&actual, &Map::new(), home)?;
    let project_count = expected.globals[PROJECTS]
        .as_object()
        .ok_or_else(invalid)?
        .len();
    if project_count == 0
        || (actual.globals[PROJECTS] == expected.globals[PROJECTS]
            && actual.globals[ASSIGNMENTS] == expected.globals[ASSIGNMENTS])
    {
        Ok(None)
    } else {
        Ok(Some(project_count))
    }
}

pub(crate) fn verify(home: &Path) -> Result<()> {
    let Some(checkpoint) = read_object(&home.join(CHECKPOINT))? else {
        return Ok(());
    };
    let workspace: Workspace =
        serde_json::from_value(checkpoint.get("workspace").cloned().ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
    let state = read_state(home)?.ok_or_else(invalid)?;
    let actual = Workspace::capture(&state, &object(state.get(ATOMS))?, home)?;
    let expected_projects = workspace.globals[PROJECTS]
        .as_object()
        .ok_or_else(invalid)?;
    let actual_projects = actual.globals[PROJECTS].as_object().unwrap();
    let projects_match = ids(expected_projects) == ids(actual_projects)
        && expected_projects.iter().all(|(id, expected)| {
            ["id", "name", "rootPaths"]
                .iter()
                .all(|key| actual_projects[id].get(*key) == expected.get(*key))
        });
    let expected_assignments = workspace.globals[ASSIGNMENTS]
        .as_object()
        .ok_or_else(invalid)?;
    let actual_assignments = actual.globals[ASSIGNMENTS]
        .as_object()
        .ok_or_else(invalid)?;
    let assignments_match = expected_assignments.len() == actual_assignments.len()
        && expected_assignments.iter().all(|(id, expected)| {
            actual_assignments
                .get(id)
                .is_some_and(|actual| same_assignment(expected, actual))
        });
    if !projects_match || !assignments_match {
        return Err(Error::new(
            ErrorCode::MixSwitchVerificationFailed,
            "Codex did not retain the local projects or conversation grouping after switching",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn oauth(account: &str) -> Value {
        let claims = json!({"https://api.openai.com/auth":{"chatgpt_account_id":account,"chatgpt_user_id":format!("user-{account}")}});
        let token = format!(
            "e30.{}.fixture",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        json!({"auth_mode":"chatgpt","tokens":{"account_id":account,"access_token":token,"refresh_token":"fixture"}})
    }

    fn initial() -> Value {
        json!({
            ACCOUNTS:{"owner":owner(&oauth("a")).unwrap(),"inactive":{}},
            ATOMS:{"cloud-cache":"account-a-private","prompt-history":["private"],
                "flat-project-sidebar-preferences-v1":{"mode":"project","chatSortMode":"updated_at"}},
            PROJECTS:{"local-1":{"id":"local-1","name":"Example","rootPaths":["/example/repo"],"updatedAt":1}},
            ASSIGNMENTS:{"thread-1":{"projectId":"local-1","projectKind":"local"}},
            "project-order":["local-1"],"selected-project":{"projectId":"local-1","projectKind":"local"},
            "electron-saved-workspace-roots":["/example/repo"],
            "queued-follow-ups":{"thread-1":"do not execute on another account"},
            "unrelated-native-setting":{"enabled":true}
        })
    }

    fn write(home: &Path, name: &str, value: &Value) {
        std::fs::write(home.join(name), serde_json::to_vec(value).unwrap()).unwrap();
    }
    fn read(home: &Path, name: &str) -> Value {
        serde_json::from_slice(&std::fs::read(home.join(name)).unwrap()).unwrap()
    }
    fn apply(home: &Path, auth: &Value) {
        for (name, bytes) in project(home, auth).unwrap().files {
            std::fs::write(home.join(name), bytes).unwrap();
        }
    }

    // Native v1 startup first selects no owner, then the authenticated user.
    // This independent transition exercises what the desktop actually reads,
    // rather than asserting only that Mix emitted the expected JSON fields.
    fn native_start(state: &mut Value, auth: &Value) {
        if state.get(ACCOUNTS).is_none() {
            return;
        }
        for next in [None, owner(auth).unwrap()] {
            let current = state[ACCOUNTS]["owner"].as_str().map(str::to_owned);
            if next.is_some() && current == next {
                continue;
            }
            let mut globals = Map::new();
            for key in ACCOUNT_GLOBALS {
                if let Some(value) = state.as_object_mut().unwrap().remove(*key) {
                    globals.insert((*key).into(), value);
                }
            }
            let atoms = state
                .as_object_mut()
                .unwrap()
                .remove(ATOMS)
                .unwrap_or(json!({}));
            if let Some(current) = current {
                state[ACCOUNTS]["inactive"][current] = json!({"atoms":atoms,"globals":globals});
            }
            let slot = next.as_ref().and_then(|next| {
                state[ACCOUNTS]["inactive"]
                    .as_object_mut()
                    .unwrap()
                    .remove(next)
            });
            state[ACCOUNTS]["owner"] = json!(next);
            state[ATOMS] = slot
                .as_ref()
                .map(|v| v["atoms"].clone())
                .unwrap_or(json!({}));
            if let Some(slot) = slot {
                state
                    .as_object_mut()
                    .unwrap()
                    .extend(slot["globals"].as_object().unwrap().clone());
            }
        }
    }

    fn switch(home: &Path, auth: &Value) -> Value {
        apply(home, auth);
        let mut state = read(home, STATE);
        native_start(&mut state, auth);
        write(home, STATE, &state);
        verify(home).unwrap();
        state
    }

    #[test]
    fn new_oauth_account_keeps_local_projects_without_inheriting_cloud_or_prompt_state() {
        let home = tempfile::tempdir().unwrap();
        let source = initial();
        write(home.path(), STATE, &source);
        let state = switch(home.path(), &oauth("b"));
        assert_eq!(state[PROJECTS], source[PROJECTS]);
        assert_eq!(state[ASSIGNMENTS], source[ASSIGNMENTS]);
        assert!(state[ATOMS].get("cloud-cache").is_none());
        assert!(state[ATOMS].get("prompt-history").is_none());
        assert!(state.get("queued-follow-ups").is_none());
        assert_eq!(
            state["unrelated-native-setting"],
            source["unrelated-native-setting"]
        );
        let state = switch(home.path(), &oauth("a"));
        assert_eq!(state[ATOMS]["cloud-cache"], "account-a-private");
    }

    #[test]
    fn local_project_assignments_keep_native_origin_and_non_uuid_projectless_threads() {
        let home = tempfile::tempdir().unwrap();
        let mut source = initial();
        source[ASSIGNMENTS]["local-chatgpt-origin"] = json!({
            "projectKind": "local",
            "projectOrigin": "chatgpt",
            "projectId": "local-1"
        });
        source["projectless-thread-ids"] = json!(["legacy-thread-id"]);
        write(home.path(), STATE, &source);
        let state = switch(home.path(), &oauth("b"));
        assert_eq!(
            state[ASSIGNMENTS]["local-chatgpt-origin"]["projectId"],
            "local-1"
        );
        assert_eq!(state["projectless-thread-ids"], json!(["legacy-thread-id"]));
    }

    #[test]
    fn rename_delete_and_empty_catalog_survive_round_trips_without_resurrection() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), STATE, &initial());
        let mut state = switch(home.path(), &oauth("b"));
        state[PROJECTS]["local-1"]["name"] = "Renamed".into();
        write(home.path(), STATE, &state);
        let mut state = switch(home.path(), &oauth("a"));
        assert_eq!(state[PROJECTS]["local-1"]["name"], "Renamed");
        state[PROJECTS] = json!({}); // Native deletion may leave stale assignment records.
        write(home.path(), STATE, &state);
        let state = switch(home.path(), &oauth("b"));
        assert_eq!(state[PROJECTS], json!({}));
        assert_eq!(state[ASSIGNMENTS], json!({}));
        assert!(state.get("selected-project").is_none());
    }

    #[test]
    fn existing_target_cloud_projects_and_private_ui_are_retained_only_for_that_account() {
        let home = tempfile::tempdir().unwrap();
        let mut state = initial();
        state[ACCOUNTS]["inactive"][owner(&oauth("b")).unwrap().unwrap()] = json!({
            "atoms":{"cloud-cache":"b-private"},"globals":{PROJECTS:{"cloud":{"id":"cloud","name":"Cloud","rootPaths":["/cloud"],"chatGptBacking":{"projectId":"g-p-example"}}},
                ASSIGNMENTS:{"cloud-thread":{"projectKind":"local","projectOrigin":"chatgpt","projectId":"cloud"}},"project-order":["cloud"]}});
        write(home.path(), STATE, &state);
        let state = switch(home.path(), &oauth("b"));
        assert_eq!(state[ATOMS]["cloud-cache"], "b-private");
        assert!(state[PROJECTS].get("cloud").is_some());
        let state = switch(home.path(), &oauth("a"));
        assert!(state[PROJECTS].get("cloud").is_none());
        assert!(state[ASSIGNMENTS].get("cloud-thread").is_none());
    }

    #[test]
    fn api_key_desktop_parks_account_scopes_and_preserves_local_edits_on_return() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), STATE, &initial());
        let key = json!({"OPENAI_API_KEY":"fixture-key"});
        let mut state = switch(home.path(), &key);
        assert!(state.get(ACCOUNTS).is_none());
        assert!(state[ATOMS].get("cloud-cache").is_none());
        assert_eq!(
            read(home.path(), CHECKPOINT)["accounts"]["inactive"]
                .as_object()
                .unwrap()
                .len(),
            1
        );
        state[PROJECTS]["local-1"]["name"] = "API edit".into();
        write(home.path(), STATE, &state);
        let mut restarted = state.clone();
        native_start(&mut restarted, &key);
        assert_eq!(restarted, state);
        let state = switch(home.path(), &oauth("a"));
        assert_eq!(state[PROJECTS]["local-1"]["name"], "API edit");
        assert_eq!(state[ATOMS]["cloud-cache"], "account-a-private");
        assert_eq!(read(home.path(), CHECKPOINT)["accounts"], Value::Null);
    }

    #[test]
    fn switched_away_catalog_is_recoverable_on_the_current_account() {
        let home = tempfile::tempdir().unwrap();
        let mut state = initial();
        native_start(&mut state, &oauth("b"));
        assert!(state.get(PROJECTS).is_none());
        write(home.path(), STATE, &state);
        write(home.path(), "auth.json", &oauth("b"));
        assert!(needs_repair(home.path(), &oauth("b")).unwrap());
        assert_eq!(recovery_count(home.path()).unwrap(), Some(1));
        let state = switch(home.path(), &oauth("b"));
        assert_eq!(state[PROJECTS], initial()[PROJECTS]);
        assert!(!needs_repair(home.path(), &oauth("b")).unwrap());
        assert_eq!(recovery_count(home.path()).unwrap(), None);
    }

    #[test]
    fn ambiguous_recovery_does_not_guess_or_write() {
        let home = tempfile::tempdir().unwrap();
        let mut state = initial();
        native_start(&mut state, &oauth("b"));
        let a = owner(&oauth("a")).unwrap().unwrap();
        let c = owner(&oauth("c")).unwrap().unwrap();
        state[ACCOUNTS]["inactive"][&c] = state[ACCOUNTS]["inactive"][&a].clone();
        state[ACCOUNTS]["inactive"][&c]["globals"][PROJECTS]["local-1"]["name"] = "Other".into();
        write(home.path(), STATE, &state);
        assert_eq!(
            project(home.path(), &oauth("b")).err().unwrap().code,
            ErrorCode::MixConflict
        );
        assert_eq!(read(home.path(), STATE), state);
        assert!(!home.path().join(CHECKPOINT).exists());
    }

    #[test]
    fn old_desktop_and_cli_only_installs_need_no_extra_files() {
        let home = tempfile::tempdir().unwrap();
        assert!(project(home.path(), &json!({})).unwrap().files.is_empty());
        let mut state = initial();
        state.as_object_mut().unwrap().remove(ACCOUNTS);
        write(home.path(), STATE, &state);
        assert!(project(home.path(), &json!({})).unwrap().files.is_empty());
    }

    #[test]
    fn native_backup_state_is_used_when_primary_is_missing_or_corrupt() {
        let home = tempfile::tempdir().unwrap();
        let state = initial();
        write(home.path(), STATE_BACKUP, &state);
        let projection = project(home.path(), &oauth("b")).unwrap();
        assert!(projection.files.contains_key(STATE));

        std::fs::write(home.path().join(STATE), b"not-json").unwrap();
        let projection = project(home.path(), &oauth("b")).unwrap();
        let projected: Value = serde_json::from_slice(&projection.files[STATE]).unwrap();
        assert!(projected.get(PROJECTS).is_some());
    }

    #[test]
    fn unknown_state_and_missing_oauth_principal_fail_without_mutation() {
        let home = tempfile::tempdir().unwrap();
        let mut state = initial();
        write(home.path(), STATE, &state);
        let auth =
            json!({"auth_mode":"chatgpt","tokens":{"account_id":"a","access_token":"opaque"}});
        assert_eq!(
            project(home.path(), &auth).err().unwrap().code,
            ErrorCode::MixAccountReauthRequired
        );
        state[ACCOUNTS]["future-field"] = json!(true);
        write(home.path(), STATE, &state);
        assert_eq!(
            project(home.path(), &oauth("b")).err().unwrap().code,
            ErrorCode::MixConfigInvalid
        );
        assert_eq!(read(home.path(), STATE), state);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_native_state_is_never_followed() {
        let outside = tempfile::NamedTempFile::new().unwrap();
        for (linked, regular) in [(STATE, STATE_BACKUP), (STATE_BACKUP, STATE)] {
            let home = tempfile::tempdir().unwrap();
            write(home.path(), regular, &initial());
            std::os::unix::fs::symlink(outside.path(), home.path().join(linked)).unwrap();
            assert!(project(home.path(), &oauth("b")).is_err());
            assert_eq!(read(home.path(), regular), initial());
        }
    }

    #[test]
    fn conflicting_cloud_project_ids_fail_before_projection() {
        let home = tempfile::tempdir().unwrap();
        let mut state = initial();
        state[ACCOUNTS]["inactive"][owner(&oauth("b")).unwrap().unwrap()] = json!({
            "atoms":{},"globals":{PROJECTS:{"local-1":{
                "id":"local-1","name":"Cloud","rootPaths":["/example/cloud"],
                "chatGptBacking":{"projectId":"g-p-example"}
            }}}
        });
        write(home.path(), STATE, &state);
        assert_eq!(
            project(home.path(), &oauth("b")).err().unwrap().code,
            ErrorCode::MixConflict
        );
        assert_eq!(read(home.path(), STATE), state);
    }

    #[test]
    fn target_remote_projects_and_assignments_survive_local_changes() {
        let home = tempfile::tempdir().unwrap();
        let mut state = initial();
        let remote = json!({"remote-1":{"name":"Remote"}});
        let assignment = json!({"projectKind":"remote","projectId":"remote-1","hostId":"host-1"});
        state[ACCOUNTS]["inactive"][owner(&oauth("b")).unwrap().unwrap()] = json!({
            "atoms":{},"globals":{
                "remote-projects":remote,
                ASSIGNMENTS:{"remote-thread":assignment},
                "pinned-thread-ids":["remote-thread"],
                "thread-workspace-root-hints":{"remote-thread":"/remote/repo"}
            }
        });
        write(home.path(), STATE, &state);
        let state = switch(home.path(), &oauth("b"));
        assert_eq!(state["remote-projects"], remote);
        assert_eq!(state[ASSIGNMENTS]["remote-thread"], assignment);
        assert_eq!(state["pinned-thread-ids"], json!(["remote-thread"]));
        assert_eq!(
            state["thread-workspace-root-hints"]["remote-thread"],
            "/remote/repo"
        );
        let state = switch(home.path(), &oauth("a"));
        assert!(state.get("remote-projects").is_none());
        assert!(state[ASSIGNMENTS].get("remote-thread").is_none());
    }

    #[test]
    fn journal_restores_native_state_fallback_and_checkpoint_byte_for_byte() {
        let home = tempfile::tempdir().unwrap();
        let mix = tempfile::tempdir().unwrap();
        write(home.path(), STATE, &initial());
        std::fs::write(
            home.path().join(format!("{STATE}.bak")),
            b"original fallback",
        )
        .unwrap();
        let original = std::fs::read(home.path().join(STATE)).unwrap();
        let profile = crate::Profile::default();
        let vault = crate::vault::MemoryVault::default();
        let journal = crate::transaction::SwitchJournal::prepare(
            mix.path(),
            crate::transaction::SwitchPlan {
                app: "codex",
                live_dir: home.path(),
                from_name: None,
                from: None,
                to_name: "fixture",
                to: &profile,
                file_overrides: project(home.path(), &oauth("b")).unwrap().files,
                file_removals: BTreeSet::new(),
                file_rewrites: vec![],
                restart_required: false,
            },
            &vault,
        )
        .unwrap();
        journal.apply(&vault).unwrap();
        assert!(home.path().join(CHECKPOINT).exists());
        journal.restore(mix.path(), &vault).unwrap();
        assert_eq!(std::fs::read(home.path().join(STATE)).unwrap(), original);
        assert_eq!(
            std::fs::read(home.path().join(format!("{STATE}.bak"))).unwrap(),
            b"original fallback"
        );
        assert!(!home.path().join(CHECKPOINT).exists());
    }
}
