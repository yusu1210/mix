use crate::{Error, ErrorCode, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct ProcessSpec {
    pub executable: Option<PathBuf>,
    /// A worker that must be running before foreground/context activation.
    pub ready_executable: Option<PathBuf>,
    pub managed_executables: Vec<PathBuf>,
    pub grace_seconds: u64,
    /// Cold-start action for the client process itself.
    pub launch: Vec<String>,
    /// Foreground/context action used after the process is ready.
    pub activate: Vec<String>,
    pub ready_timeout: u64,
}

const fn default_grace() -> u64 {
    8
}

const fn default_ready_timeout() -> u64 {
    12
}

const STOP_QUIET_MILLIS: u64 = 500;

impl Default for ProcessSpec {
    fn default() -> Self {
        Self {
            executable: None,
            ready_executable: None,
            managed_executables: Vec::new(),
            grace_seconds: default_grace(),
            launch: Vec::new(),
            activate: Vec::new(),
            ready_timeout: default_ready_timeout(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ProcessController {
    spec: Option<ProcessSpec>,
    executables: Vec<PathBuf>,
    process_roots: Vec<PathBuf>,
}

impl ProcessController {
    pub fn new(spec: Option<ProcessSpec>) -> Self {
        let mut executables = spec
            .as_ref()
            .map(|value| value.managed_executables.clone())
            .unwrap_or_default();
        if let Some(executable) = spec.as_ref().and_then(|value| value.executable.clone()) {
            executables.push(executable);
        }
        executables.sort();
        executables.dedup();
        let process_roots = executables
            .iter()
            .filter_map(|path| normalized(path).ok())
            .filter_map(|path| application_contents_root(&path))
            .collect();
        Self {
            spec,
            executables,
            process_roots,
        }
    }

    pub fn matching_pids(&self) -> Result<Vec<u32>> {
        if self.executables.is_empty() {
            return Ok(Vec::new());
        }
        let expected = self
            .executables
            .iter()
            .map(|path| normalized(path))
            .collect::<Result<Vec<_>>>()?;
        matching_pids_for(&expected, &self.process_roots)
    }

    pub fn restart_configured(&self) -> bool {
        self.spec
            .as_ref()
            .is_some_and(|spec| spec.executable.is_some() && !spec.launch.is_empty())
    }

    pub fn stop(&self) -> Result<Vec<u32>> {
        let expected = self
            .executables
            .iter()
            .map(|path| normalized(path))
            .collect::<Result<Vec<_>>>()?;
        let mut stopped = self.matching_pids()?;
        if stopped.is_empty() {
            return Ok(stopped);
        }
        for pid in &stopped {
            terminate_if_matching(*pid, &expected, &self.process_roots, false)?;
        }
        let grace = Duration::from_secs(
            self.spec
                .as_ref()
                .map_or(default_grace(), |spec| spec.grace_seconds),
        );
        let deadline = Instant::now() + grace;
        let mut quiet_since = None;
        while Instant::now() < deadline {
            let running = self.matching_pids()?;
            if running.is_empty() {
                let quiet_since = quiet_since.get_or_insert_with(Instant::now);
                if quiet_since.elapsed() >= Duration::from_millis(STOP_QUIET_MILLIS) {
                    stopped.sort_unstable();
                    stopped.dedup();
                    return Ok(stopped);
                }
            } else {
                quiet_since = None;
                for pid in running {
                    if !stopped.contains(&pid) {
                        terminate_if_matching(pid, &expected, &self.process_roots, false)?;
                        stopped.push(pid);
                    }
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
        for pid in self.matching_pids()? {
            terminate_if_matching(pid, &expected, &self.process_roots, true)?;
            if !stopped.contains(&pid) {
                stopped.push(pid);
            }
        }
        let kill_deadline = Instant::now() + Duration::from_secs(2);
        quiet_since = None;
        while Instant::now() < kill_deadline {
            if self.matching_pids()?.is_empty() {
                let quiet_since = quiet_since.get_or_insert_with(Instant::now);
                if quiet_since.elapsed() >= Duration::from_millis(STOP_QUIET_MILLIS) {
                    stopped.sort_unstable();
                    stopped.dedup();
                    return Ok(stopped);
                }
            } else {
                quiet_since = None;
            }
            thread::sleep(Duration::from_millis(50));
        }
        Err(Error::new(
            ErrorCode::MixProcessStillRunning,
            "the client is still running after the shutdown timeout",
        ))
    }

    pub fn start(&self) -> Result<bool> {
        let Some(spec) = &self.spec else {
            return Ok(false);
        };
        if spec.launch.is_empty() {
            return Ok(false);
        }
        let readiness = self.readiness_executables()?;
        let mut command = Command::new(&spec.launch[0]);
        command
            .args(&spec.launch[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|error| Error::io("cannot restart client", error))?;
        let _ = thread::Builder::new()
            .name("mix-client-reaper".into())
            .spawn(move || {
                let _ = child.wait();
            });
        wait_for_executables(&readiness, spec.ready_timeout)?;
        Ok(true)
    }

    pub fn ensure_active(&self) -> Result<bool> {
        let Some(executable) = self.spec.as_ref().and_then(|spec| spec.executable.as_ref()) else {
            return self.start();
        };
        let primary = [normalized(executable)?];
        if matching_pids_for(&primary, &[])?.is_empty() {
            self.start()
        } else {
            let spec = self.spec.as_ref().ok_or_else(|| {
                Error::new(
                    ErrorCode::MixConfigInvalid,
                    "client process is not configured",
                )
            })?;
            wait_for_executables(&self.readiness_executables()?, spec.ready_timeout)?;
            self.activate()?;
            Ok(false)
        }
    }

    pub fn activate(&self) -> Result<bool> {
        let Some(spec) = &self.spec else {
            return Ok(false);
        };
        let Some((program, arguments)) = spec.activate.split_first() else {
            return Ok(false);
        };
        let status = Command::new(program)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| Error::io("cannot activate client", error))?;
        if status.success() {
            Ok(true)
        } else {
            Err(Error::new(
                ErrorCode::MixSwitchVerificationFailed,
                format!("the client activation command failed with {status}"),
            ))
        }
    }

    fn readiness_executables(&self) -> Result<Vec<PathBuf>> {
        let spec = self.spec.as_ref().ok_or_else(|| {
            Error::new(
                ErrorCode::MixConfigInvalid,
                "client process is not configured",
            )
        })?;
        let executable = spec.executable.as_ref().ok_or_else(|| {
            Error::new(
                ErrorCode::MixConfigInvalid,
                "restart requires an absolute process executable",
            )
        })?;
        let mut expected = vec![normalized(executable)?];
        if let Some(ready) = &spec.ready_executable {
            expected.push(normalized(ready)?);
        }
        Ok(expected)
    }
}

fn wait_for_executables(expected: &[PathBuf], timeout_seconds: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    while Instant::now() < deadline {
        let running = process_ids()?
            .into_iter()
            .filter_map(executable_path)
            .filter_map(|path| normalized(&path).ok())
            .collect::<Vec<_>>();
        if expected.iter().all(|path| running.contains(path)) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(150));
    }
    Err(Error::new(
        ErrorCode::MixSwitchVerificationFailed,
        "the client did not become ready after restart",
    ))
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn running_executable(candidates: &[PathBuf]) -> Result<Option<PathBuf>> {
    let expected = candidates
        .iter()
        .map(|path| normalized(path))
        .collect::<Result<Vec<_>>>()?;
    let mut running = Vec::new();
    for pid in process_ids()? {
        if let Some(path) = executable_path(pid).and_then(|path| normalized(&path).ok()) {
            running.push(path);
        }
    }
    Ok(expected
        .iter()
        .position(|path| running.contains(path))
        .map(|index| candidates[index].clone()))
}

fn terminate_if_matching(
    pid: u32,
    expected: &[PathBuf],
    process_roots: &[PathBuf],
    force: bool,
) -> Result<()> {
    let Some(executable) = executable_path(pid) else {
        return Ok(());
    };
    let Some(path) = normalized(&executable).ok() else {
        return Ok(());
    };
    if !matches_process_path(&path, expected, process_roots) {
        return Ok(());
    }
    terminate(pid, force)
}

fn matching_pids_for(expected: &[PathBuf], process_roots: &[PathBuf]) -> Result<Vec<u32>> {
    let current = std::process::id();
    let mut matches = Vec::new();
    for pid in process_ids()? {
        if pid == current {
            continue;
        }
        if let Some(path) = executable_path(pid) {
            if normalized(&path)
                .ok()
                .is_some_and(|path| matches_process_path(&path, expected, process_roots))
            {
                matches.push(pid);
            }
        }
    }
    matches.sort_unstable();
    matches.dedup();
    Ok(matches)
}

fn matches_process_path(path: &Path, expected: &[PathBuf], process_roots: &[PathBuf]) -> bool {
    expected.iter().any(|candidate| candidate == path)
        || process_roots
            .iter()
            .any(|root| path.starts_with(root.join("Contents")))
}

fn application_contents_root(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    loop {
        if current
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".app"))
        {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

fn normalized(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(Error::invalid(
            "process executable must be an absolute path",
        ));
    }
    std::fs::canonicalize(path).or_else(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(path.to_path_buf())
        } else {
            Err(Error::io("cannot resolve process executable", error))
        }
    })
}

#[cfg(target_os = "macos")]
fn process_ids() -> Result<Vec<u32>> {
    unsafe extern "C" {
        fn proc_listallpids(buffer: *mut libc::c_void, buffersize: libc::c_int) -> libc::c_int;
    }
    let count = unsafe { proc_listallpids(std::ptr::null_mut(), 0) };
    if count < 0 {
        return Err(Error::new(
            ErrorCode::MixLocalFailure,
            "cannot enumerate processes",
        ));
    }
    let mut values = vec![0i32; count as usize + 64];
    let bytes = (values.len() * std::mem::size_of::<i32>()) as i32;
    let actual = unsafe { proc_listallpids(values.as_mut_ptr().cast(), bytes) };
    if actual < 0 {
        return Err(Error::new(
            ErrorCode::MixLocalFailure,
            "cannot enumerate processes",
        ));
    }
    Ok(values
        .into_iter()
        .take(actual as usize)
        .filter_map(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 1)
        .collect())
}

#[cfg(target_os = "macos")]
fn executable_path(pid: u32) -> Option<PathBuf> {
    unsafe extern "C" {
        fn proc_pidpath(
            pid: libc::c_int,
            buffer: *mut libc::c_void,
            buffersize: u32,
        ) -> libc::c_int;
    }
    let mut buffer = vec![0u8; 4096];
    let length =
        unsafe { proc_pidpath(pid as i32, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if length <= 0 {
        return None;
    }
    buffer.truncate(length as usize);
    Some(PathBuf::from(String::from_utf8_lossy(&buffer).into_owned()))
}

#[cfg(target_os = "linux")]
fn process_ids() -> Result<Vec<u32>> {
    let entries = std::fs::read_dir("/proc")
        .map_err(|error| Error::io("cannot enumerate processes", error))?;
    Ok(entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u32>().ok())
        .collect())
}

#[cfg(target_os = "linux")]
fn executable_path(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_ids() -> Result<Vec<u32>> {
    Err(Error::new(
        ErrorCode::MixUnsupported,
        "process control is not implemented on this platform",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn executable_path(_: u32) -> Option<PathBuf> {
    None
}

#[cfg(unix)]
fn terminate(pid: u32, force: bool) -> Result<()> {
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    let result = unsafe { libc::kill(pid as i32, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(Error::io("cannot stop client process", error))
    }
}

#[cfg(not(unix))]
fn terminate(_: u32, _: bool) -> Result<()> {
    Err(Error::new(
        ErrorCode::MixUnsupported,
        "process stop is unsupported",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn unconfigured_controller_matches_nothing() {
        let controller = ProcessController::new(None);
        assert!(controller.matching_pids().unwrap().is_empty());
        assert!(!controller.start().unwrap());
        assert!(!controller.ensure_active().unwrap());
    }

    #[test]
    fn relative_process_identity_is_rejected() {
        let controller = ProcessController::new(Some(ProcessSpec {
            executable: Some(PathBuf::from("ChatGPT")),
            ..ProcessSpec::default()
        }));
        assert!(controller.matching_pids().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn invalid_start_configuration_has_no_launch_side_effect() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("started");
        let controller = ProcessController::new(Some(ProcessSpec {
            launch: vec!["/usr/bin/touch".into(), marker.display().to_string()],
            ..ProcessSpec::default()
        }));

        assert_eq!(
            controller.start().unwrap_err().code,
            ErrorCode::MixConfigInvalid
        );
        assert!(!marker.exists());
    }

    #[test]
    fn application_process_matching_includes_only_helpers_in_the_same_bundle() {
        let primary = PathBuf::from("/Applications/Client.app/Contents/MacOS/Client");
        let root = application_contents_root(&primary).unwrap();
        let expected = [primary];

        assert!(matches_process_path(
            Path::new("/Applications/Client.app/Contents/Frameworks/Helper"),
            &expected,
            std::slice::from_ref(&root),
        ));
        assert!(matches_process_path(
            Path::new("/Applications/Client.app/Contents/Resources/client-app-server"),
            &expected,
            std::slice::from_ref(&root),
        ));
        assert!(!matches_process_path(
            Path::new("/Applications/Other.app/Contents/MacOS/Client"),
            &expected,
            std::slice::from_ref(&root),
        ));
        assert!(!matches_process_path(
            Path::new("/Applications/Client.app/Library/Helper"),
            &expected,
            std::slice::from_ref(&root),
        ));
    }

    #[cfg(unix)]
    #[test]
    fn stop_terminates_the_primary_process_and_same_bundle_helpers() {
        let root = tempfile::tempdir().unwrap();
        let application = root.path().join("Client.app/Contents");
        let primary = application.join("MacOS/Client");
        let helper = application.join("Frameworks/Client Helper");
        fs::create_dir_all(primary.parent().unwrap()).unwrap();
        fs::create_dir_all(helper.parent().unwrap()).unwrap();
        let fixture = std::env::current_exe().unwrap();
        fs::copy(&fixture, &primary).unwrap();
        fs::copy(&fixture, &helper).unwrap();

        let spawn = |executable: &Path| {
            Command::new(executable)
                .args(["--exact", "process::tests::process_fixture", "--nocapture"])
                .env("MIX_PROCESS_FIXTURE", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut primary_child = spawn(&primary);
        let mut helper_child = spawn(&helper);
        let controller = ProcessController::new(Some(ProcessSpec {
            executable: Some(primary),
            grace_seconds: 1,
            ..ProcessSpec::default()
        }));
        let deadline = Instant::now() + Duration::from_secs(2);
        while controller.matching_pids().unwrap().len() < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        let stopped = controller.stop().unwrap();
        assert!(stopped.contains(&primary_child.id()));
        assert!(stopped.contains(&helper_child.id()));
        primary_child.wait().unwrap();
        helper_child.wait().unwrap();
        assert!(controller.matching_pids().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn stop_terminates_a_helper_spawned_during_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let application = root.path().join("Client.app/Contents");
        let primary = application.join("MacOS/Client");
        let helper = application.join("Frameworks/Client Helper");
        fs::create_dir_all(primary.parent().unwrap()).unwrap();
        fs::create_dir_all(helper.parent().unwrap()).unwrap();
        let fixture = std::env::current_exe().unwrap();
        fs::copy(&fixture, &primary).unwrap();
        fs::copy(&fixture, &helper).unwrap();

        let mut primary_child = Command::new(&primary)
            .args(["--exact", "process::tests::process_fixture", "--nocapture"])
            .env("MIX_PROCESS_FIXTURE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let controller = ProcessController::new(Some(ProcessSpec {
            executable: Some(primary),
            grace_seconds: 1,
            ..ProcessSpec::default()
        }));
        let deadline = Instant::now() + Duration::from_secs(2);
        while controller.matching_pids().unwrap().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        let respawned = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            let mut child = Command::new(helper)
                .args(["--exact", "process::tests::process_fixture", "--nocapture"])
                .env("MIX_PROCESS_FIXTURE", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let pid = child.id();
            child.wait().unwrap();
            pid
        });

        let stopped = controller.stop().unwrap();
        let helper_pid = respawned.join().unwrap();
        assert!(stopped.contains(&primary_child.id()));
        assert!(stopped.contains(&helper_pid));
        primary_child.wait().unwrap();
        assert!(controller.matching_pids().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn running_candidate_is_selected_and_all_installed_copies_are_managed() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("First.app/Contents/MacOS/FixtureClient");
        let second = root.path().join("Second.app/Contents/MacOS/FixtureClient");
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        let fixture = std::env::current_exe().unwrap();
        fs::copy(&fixture, &first).unwrap();
        fs::copy(&fixture, &second).unwrap();

        let mut child = Command::new(&second)
            .args(["--exact", "process::tests::process_fixture", "--nocapture"])
            .env("MIX_PROCESS_FIXTURE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let candidates = vec![first.clone(), second.clone()];
        let deadline = Instant::now() + Duration::from_secs(2);
        while running_executable(&candidates).unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(
            running_executable(&candidates).unwrap(),
            Some(second.clone())
        );
        let controller = ProcessController::new(Some(ProcessSpec {
            executable: Some(second),
            managed_executables: candidates,
            grace_seconds: 1,
            ..ProcessSpec::default()
        }));
        assert_eq!(controller.matching_pids().unwrap(), [child.id()]);
        assert_eq!(controller.stop().unwrap(), [child.id()]);
        child.wait().unwrap();
    }

    #[test]
    fn application_contents_root_is_not_inferred_for_non_app_executables() {
        assert_eq!(
            application_contents_root(Path::new("/usr/local/bin/codex")),
            None
        );
    }

    #[test]
    fn restart_configuration_does_not_depend_on_a_process_race() {
        let controller = ProcessController::new(Some(ProcessSpec {
            executable: Some(PathBuf::from(
                "/Applications/Client.app/Contents/MacOS/Client",
            )),
            launch: vec![
                "/usr/bin/open".into(),
                "-b".into(),
                "com.example.client".into(),
            ],
            ..ProcessSpec::default()
        }));
        assert!(controller.restart_configured());
    }

    #[cfg(unix)]
    #[test]
    fn ensure_active_starts_the_primary_when_only_a_managed_helper_remains() {
        let root = tempfile::tempdir().unwrap();
        let application = root.path().join("Client.app/Contents");
        let primary = application.join("MacOS/Client");
        let helper = application.join("Frameworks/Client Helper");
        fs::create_dir_all(primary.parent().unwrap()).unwrap();
        fs::create_dir_all(helper.parent().unwrap()).unwrap();
        let fixture = std::env::current_exe().unwrap();
        fs::copy(&fixture, &primary).unwrap();
        fs::copy(&fixture, &helper).unwrap();

        let mut helper_child = Command::new(&helper)
            .args(["--exact", "process::tests::process_fixture", "--nocapture"])
            .env("MIX_PROCESS_FIXTURE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let controller = ProcessController::new(Some(ProcessSpec {
            executable: Some(primary.clone()),
            managed_executables: vec![helper],
            launch: vec![
                "/usr/bin/env".into(),
                "MIX_PROCESS_FIXTURE=1".into(),
                primary.display().to_string(),
                "--exact".into(),
                "process::tests::process_fixture".into(),
                "--nocapture".into(),
            ],
            ready_timeout: 2,
            grace_seconds: 1,
            ..ProcessSpec::default()
        }));
        let deadline = Instant::now() + Duration::from_secs(2);
        while controller.matching_pids().unwrap().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        assert!(controller.ensure_active().unwrap());
        assert!(running_executable(&[primary]).unwrap().is_some());
        controller.stop().unwrap();
        helper_child.wait().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn post_restart_activation_must_succeed() {
        let successful = ProcessController::new(Some(ProcessSpec {
            activate: vec!["/usr/bin/true".into()],
            ..ProcessSpec::default()
        }));
        assert!(successful.activate().unwrap());

        let failed = ProcessController::new(Some(ProcessSpec {
            activate: vec!["/usr/bin/false".into()],
            ..ProcessSpec::default()
        }));
        assert_eq!(
            failed.activate().unwrap_err().code,
            ErrorCode::MixSwitchVerificationFailed
        );
    }

    #[cfg(unix)]
    #[test]
    fn start_waits_for_the_process_without_replaying_the_activation_action() {
        let root = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let activated = root.path().join("activated");
        let controller = ProcessController::new(Some(ProcessSpec {
            executable: Some(executable.clone()),
            launch: vec![
                "/usr/bin/env".into(),
                "MIX_PROCESS_FIXTURE=1".into(),
                executable.display().to_string(),
                "--exact".into(),
                "process::tests::process_fixture".into(),
                "--nocapture".into(),
            ],
            activate: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf activated > \"$1\"".into(),
                "mix-process-test".into(),
                activated.display().to_string(),
            ],
            ready_timeout: 2,
            ..ProcessSpec::default()
        }));

        assert!(controller.start().unwrap());
        assert!(!activated.exists());
        controller.stop().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn restart_waits_for_the_client_worker_not_only_the_main_process() {
        let root = tempfile::tempdir().unwrap();
        let application = root.path().join("Client.app/Contents");
        let primary = application.join("MacOS/Client");
        let worker = application.join("Resources/client-app-server");
        fs::create_dir_all(primary.parent().unwrap()).unwrap();
        fs::create_dir_all(worker.parent().unwrap()).unwrap();
        let fixture = std::env::current_exe().unwrap();
        fs::copy(&fixture, &primary).unwrap();
        fs::copy(&fixture, &worker).unwrap();
        let controller = ProcessController::new(Some(ProcessSpec {
            executable: Some(primary.clone()),
            ready_executable: Some(worker.clone()),
            launch: vec![
                "/bin/sh".into(),
                "-c".into(),
                "MIX_PROCESS_FIXTURE=1 \"$1\" --exact process::tests::process_fixture --nocapture & sleep 0.4; exec env MIX_PROCESS_FIXTURE=1 \"$2\" --exact process::tests::process_fixture --nocapture".into(),
                "mix-ready-test".into(),
                primary.display().to_string(),
                worker.display().to_string(),
            ],
            grace_seconds: 1,
            ready_timeout: 2,
            ..ProcessSpec::default()
        }));

        let started_at = Instant::now();
        assert!(controller.start().unwrap());
        assert!(started_at.elapsed() >= Duration::from_millis(300));
        controller.stop().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_process_is_activated_without_being_launched_again() {
        let root = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let activated = root.path().join("activated");
        let mut child = Command::new(&executable)
            .args(["--exact", "process::tests::process_fixture", "--nocapture"])
            .env("MIX_PROCESS_FIXTURE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let controller = ProcessController::new(Some(ProcessSpec {
            executable: Some(executable),
            launch: vec!["/usr/bin/false".into()],
            activate: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf activated > \"$1\"".into(),
                "mix-running-process-test".into(),
                activated.display().to_string(),
            ],
            ..ProcessSpec::default()
        }));
        let deadline = Instant::now() + Duration::from_secs(1);
        while controller.matching_pids().unwrap().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        assert!(!controller.ensure_active().unwrap());
        assert_eq!(std::fs::read_to_string(&activated).unwrap(), "activated");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn process_fixture() {
        if std::env::var_os("MIX_PROCESS_FIXTURE").is_some() {
            thread::sleep(Duration::from_secs(2));
        }
    }
}
