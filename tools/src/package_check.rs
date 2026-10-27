// SPDX-License-Identifier: MIT
//! A real installation, service handoff and rollback of a package against the
//! previously published one, in an isolated home without display or network.
use crate::{
    ensure, http,
    util::{Failure, Result, digest, fail, pretty, read_regular, require},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// Why an observation produced nothing: the service is still changing hands,
/// or something is wrong and waiting cannot help.
#[derive(Debug)]
pub enum Poll {
    Retry(String),
    Fatal(Failure),
}
impl From<Failure> for Poll {
    fn from(error: Failure) -> Self {
        Self::Fatal(error)
    }
}
type Observed<T> = std::result::Result<Option<T>, Poll>;

/// An API answer other than 200.
#[derive(Debug)]
pub struct Refused {
    pub status: u16,
    pub path: String,
    pub value: Value,
}

pub fn request(
    record: &Value,
    path: &str,
    data: Option<&Value>,
) -> std::result::Result<Value, Poll> {
    let port = record["port"]
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .ok_or_else(|| Poll::Retry("The service record has no port".into()))?;
    let token = record["token"].as_str().unwrap_or_default();
    let body = data.map(|value| value.to_string().into_bytes());
    let headers = [
        ("X-Flightdeck-Token", token),
        ("Content-Type", "application/json"),
    ];
    let method = if body.is_some() { "POST" } else { "GET" };
    let response = http::request(
        port,
        method,
        path,
        &headers,
        body.as_deref(),
        Duration::from_secs(5),
    )
    .map_err(|error| Poll::Retry(error.0))?;
    let value: Value = serde_json::from_slice(&response.body)
        .map_err(|_| Poll::Retry("The service answered without JSON".into()))?;
    if response.status != 200 {
        return Err(Poll::Fatal(Failure(
            json!({"status": response.status, "path": path, "value": value}).to_string(),
        )));
    }
    Ok(value)
}

/// Like `request`, but reports a refusal instead of failing on it.
fn answered(
    record: &Value,
    path: &str,
) -> std::result::Result<std::result::Result<Value, Refused>, Poll> {
    match request(record, path, None) {
        Ok(value) => Ok(Ok(value)),
        Err(Poll::Fatal(error)) => {
            let refused: Value = serde_json::from_str(&error.0).unwrap_or_default();
            Ok(Err(Refused {
                status: refused["status"].as_u64().unwrap_or_default() as u16,
                path: path.into(),
                value: refused["value"].clone(),
            }))
        }
        Err(retry) => Err(retry),
    }
}

/// Repeats an observation until it yields a value. Only `Retry` is waited out.
pub fn wait_for<T>(mut check: impl FnMut() -> Observed<T>, timeout: Duration) -> Result<T> {
    let deadline = Instant::now() + timeout;
    let mut last = String::new();
    while Instant::now() < deadline {
        match check() {
            Ok(Some(value)) => return Ok(value),
            Ok(None) => {}
            Err(Poll::Retry(reason)) => last = reason,
            Err(Poll::Fatal(error)) => return Err(error),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    fail(format!(
        "Timed out waiting for the isolated service{}",
        if last.is_empty() {
            String::new()
        } else {
            format!(": {last}")
        }
    ))
}

/// The service record once the service itself confirms it.
pub fn record_at(state: &Path) -> Observed<Value> {
    let raw = fs::read(state.join("desktop-service.json"))
        .map_err(|error| Poll::Retry(error.to_string()))?;
    let record: Value =
        serde_json::from_slice(&raw).map_err(|error| Poll::Retry(error.to_string()))?;
    let status = match answered(&record, "/api/status")? {
        Ok(status) => status,
        // The replacement may rotate authentication after the record was read.
        // Only bounded discovery polling retries this; mutation errors stay fatal.
        Err(refused) if [401, 403].contains(&refused.status) => {
            return Err(Poll::Retry(
                "Service authentication changed during handoff".into(),
            ));
        }
        Err(refused) => {
            return Err(Poll::Fatal(Failure(format!(
                "{} answered {}: {}",
                refused.path, refused.status, refused.value
            ))));
        }
    };
    if status["csrf_token"] != record["token"] || status["service"]["release"] != record["release"]
    {
        // The replacement can publish its record between the file read and the
        // HTTP request. This observation is repeated; a stale token is never reused.
        return Err(Poll::Retry("Service record changed during handoff".into()));
    }
    if status["runtime"]["configured"] != false {
        return Err(Poll::Fatal(Failure(
            "The isolated service has a configured runtime".into(),
        )));
    }
    Ok(Some(record))
}

/// The replacement's record, once its coordinator has released the installation.
pub fn record_after_handoff(
    state: &Path,
    previous: &Value,
    installation: &Path,
    expected: Option<&str>,
) -> Observed<Value> {
    let Some(record) = record_at(state)? else {
        return Ok(None);
    };
    if record["token"] == previous["token"] {
        return Ok(None);
    }
    let fatal = |message: &str| Poll::Fatal(Failure(message.into()));
    if record["port"] != previous["port"] {
        return Err(fatal("Handoff must preserve the local service endpoint"));
    }
    if record["pid"] == previous["pid"] {
        return Err(fatal("Handoff must start a new service process"));
    }
    if expected.is_some_and(|expected| record["release"] != expected) {
        return Err(fatal("Handoff started an unexpected release"));
    }
    // The replacement publishes its service record before the coordinator has
    // finished verifying it. Wait for that coordinator's installation lease;
    // no other mutation starts and no failed rollback is retried in this window.
    use rustix::fs::{FlockOperation, Mode, OFlags};
    let inspect = || fatal("Cannot inspect the isolated installation lock");
    let lock = rustix::fs::open(
        installation.join(".install.lock"),
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|_| inspect())?;
    if !rustix::fs::fstat(&lock)
        .is_ok_and(|info| rustix::fs::FileType::from_raw_mode(info.st_mode).is_file())
    {
        return Err(fatal("Invalid installation lock"));
    }
    match rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => rustix::fs::flock(&lock, FlockOperation::Unlock).map_err(|_| inspect())?,
        Err(rustix::io::Errno::WOULDBLOCK) => return Ok(None),
        Err(_) => return Err(inspect()),
    }
    Ok(Some(record))
}

struct Sandbox {
    output: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    children: Vec<Child>,
    states: Vec<PathBuf>,
}
impl Sandbox {
    fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.output)
            .env_clear()
            .envs(&self.environment);
        command
    }
    /// Installs with nothing on PATH: the installer must need no interpreter.
    fn install(&self, source: &Path, root: &Path) -> Result<()> {
        let result = self
            .command(&source.join("bin/flightdeck"))
            .arg("install")
            .arg("--source")
            .arg(source)
            .arg("--data-dir")
            .arg(root.join("installed"))
            .arg("--bin-dir")
            .arg(root.join("bin"))
            .arg("--applications-dir")
            .arg(root.join("applications"))
            .args(["--no-desktop", "--no-launch", "--language", "en"])
            .env("PATH", "/no-programs-on-path")
            .stdin(Stdio::null())
            .output()?;
        require(
            result.status.success(),
            format!(
                "Installation failed: {}",
                String::from_utf8_lossy(&result.stderr)
            ),
        )
    }
    fn start(&mut self, root: &Path) -> Result<Value> {
        let state = root.join("state");
        self.states.push(state.clone());
        let log = fs::File::create(root.join("service.log"))?;
        let child = self
            .command(&root.join("bin/flightdeck"))
            .arg("--desktop-service")
            .arg("--state-dir")
            .arg(&state)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        self.children.push(child);
        wait_for(|| record_at(&state), Duration::from_secs(60))
    }
    fn stop(&mut self) {
        for state in &self.states {
            if let Ok(record) = wait_for(|| record_at(state), Duration::from_secs(3)) {
                let _ = request(&record, "/api/desktop/refresh", Some(&json!({})));
            }
        }
        for child in &mut self.children {
            if matches!(child.try_wait(), Ok(None))
                && let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32)
            {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::INT);
            }
            let deadline = Instant::now() + Duration::from_secs(30);
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn mutate(record: &Value, path: &str) -> Result<Value> {
    match request(record, path, Some(&json!({}))) {
        Ok(value) => Ok(value),
        Err(Poll::Retry(reason)) => fail(format!("{path}: {reason}")),
        Err(Poll::Fatal(error)) => Err(error),
    }
}
fn read(record: &Value, path: &str) -> Result<Value> {
    match request(record, path, None) {
        Ok(value) => Ok(value),
        Err(Poll::Retry(reason)) => fail(format!("{path}: {reason}")),
        Err(Poll::Fatal(error)) => Err(error),
    }
}

fn changed(state: &Path, previous: &Value, expected: Option<&str>) -> Result<Value> {
    let installation = state.parent().unwrap_or(state).join("installed");
    wait_for(
        || record_after_handoff(state, previous, &installation, expected),
        Duration::from_secs(60),
    )
}

fn rollback(record: &Value, state: &Path) -> Result<Value> {
    mutate(record, "/api/launcher-update/rollback")?;
    let status = wait_for(
        || {
            let status = request(record, "/api/launcher-update", None)?;
            if status["job"]["state"] == "failed" {
                return Err(Poll::Fatal(Failure(format!(
                    "Rollback failed: {}",
                    status["job"]
                ))));
            }
            Ok((status["job"]["state"] == "complete").then_some(status))
        },
        Duration::from_secs(60),
    )?;
    ensure!(status["pending_restart"] == true && status["can_restart"] == true);
    mutate(record, "/api/launcher-update/restart")?;
    changed(state, record, None)
}

fn exercise(sandbox: &mut Sandbox, package: &Path, previous: &Path, digest: &str) -> Result<Value> {
    let previous_digest = crate::util::digest(&read_regular(
        &previous.join("bin/flightdeck"),
        512 * 1024 * 1024,
    )?);
    require(
        digest != previous_digest,
        "Use different builds to exercise the actual handoff",
    )?;
    let root = sandbox.output.join("native");
    fs::create_dir(&root)?;
    sandbox.install(previous, &root)?;
    let before = sandbox.start(&root)?;
    ensure!(before["release"] == previous_digest);
    sandbox.install(package, &root)?;
    let status = read(&before, "/api/launcher-update")?;
    ensure!(status["pending_restart"] == true && status["can_restart"] == true);
    mutate(&before, "/api/launcher-update/restart")?;
    let state = root.join("state");
    let current = changed(&state, &before, Some(digest))?;
    let mut results = serde_json::Map::new();
    for language in ["en", "de"] {
        match request(
            &current,
            "/api/preferences",
            Some(&json!({"language": language})),
        ) {
            Ok(_) => {}
            Err(Poll::Retry(reason)) => return fail(reason),
            Err(Poll::Fatal(error)) => return Err(error),
        }
        let stored: Value = serde_json::from_slice(&fs::read(state.join("ui-preferences.json"))?)?;
        ensure!(stored["language"] == language);
    }
    require(
        !sandbox.output.join("browser-arguments.txt").exists(),
        "The native UI must not launch Chromium",
    )?;
    results.insert("native_language_preferences".into(), "PASS".into());
    let reverted = rollback(&current, &state)?;
    ensure!(reverted["release"] == previous_digest);
    ensure!(read(&reverted, "/api/launcher-update")?["can_rollback"] == true);
    results.insert("native_update_and_rollback".into(), "PASS".into());
    Ok(Value::Object(results))
}

pub fn check(package: &Path, previous: &Path, output: &Path) -> Result<()> {
    let package = fs::canonicalize(package)?;
    let previous = fs::canonicalize(previous)?;
    let digest = digest(&read_regular(
        &package.join("bin/flightdeck"),
        512 * 1024 * 1024,
    )?);
    let output = std::path::absolute(output)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::DirBuilder::new().mode(0o700).create(&output)?;
    let mut environment: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    for name in ["PYTHONPATH", "PYTHONHOME"] {
        environment.remove(std::ffi::OsStr::new(name));
    }
    let mut set = |name: &str, value: OsString| environment.insert(name.into(), value);
    set("HOME", output.clone().into());
    set("DISPLAY", "".into());
    set("WAYLAND_DISPLAY", "".into());
    for name in ["CONFIG", "CACHE", "DATA", "STATE"] {
        set(
            &format!("XDG_{name}_HOME"),
            output.join(name.to_lowercase()).into(),
        );
    }
    set("HTTPS_PROXY", "http://127.0.0.1:1".into());
    set("HTTP_PROXY", "http://127.0.0.1:1".into());
    set("NO_PROXY", "127.0.0.1,localhost".into());
    let browsers = output.join("fake-browser");
    fs::create_dir(&browsers)?;
    fs::write(
        browsers.join("chromium"),
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$FLIGHTDECK_TEST_BROWSER_LOG\"\n",
    )?;
    fs::set_permissions(browsers.join("chromium"), fs::Permissions::from_mode(0o700))?;
    set(
        "FLIGHTDECK_TEST_BROWSER_LOG",
        output.join("browser-arguments.txt").into(),
    );
    let mut path = OsString::from(&browsers);
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    set("PATH", path);
    let mut sandbox = Sandbox {
        output: output.clone(),
        environment,
        children: Vec::new(),
        states: Vec::new(),
    };
    let results = exercise(&mut sandbox, &package, &previous, &digest);
    sandbox.stop();
    let report = json!({"status": "PASS", "native_binary_sha256": digest, "checks": results?,
        "native_install_without_python": true, "same_service_endpoint": true,
        "native_window": "covered separately; this test is headless", "game_or_account_calls": false});
    fs::write(output.join("result.json"), pretty(&report))?;
    println!("{report}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fixtures::Service;
    use std::sync::{Arc, Mutex};

    fn healthy(release: &'static str) -> Service {
        Service::start(move |_, _, headers, _| {
            let token = headers
                .get("x-flightdeck-token")
                .map(|values| values[0].clone());
            (
                200,
                json!({"csrf_token": token, "service": {"release": release}, "runtime": {"configured": false}}),
            )
        })
    }
    fn write_record(state: &Path, record: &Value) {
        fs::write(state.join("desktop-service.json"), record.to_string()).expect("record");
    }
    fn short<T>(check: impl FnMut() -> Observed<T>) -> Result<T> {
        wait_for(check, Duration::from_millis(120))
    }

    #[test]
    fn reachable_replacement_waits_for_the_installation_coordinator() {
        let temp = tempfile::tempdir().expect("temp");
        let service = healthy("new");
        let state = temp.path();
        let installation = state.join("installed");
        fs::create_dir(&installation).expect("installation");
        let lock_path = installation.join(".install.lock");
        fs::write(&lock_path, "").expect("lock");
        let previous =
            json!({"port": service.port, "token": "previous", "pid": 123, "release": "old"});
        let current =
            json!({"port": service.port, "token": "current", "pid": 456, "release": "new"});
        write_record(state, &current);
        {
            // The coordinator holds the installation through its own descriptor.
            let coordinator = fs::File::open(&lock_path).expect("coordinator");
            rustix::fs::flock(&coordinator, rustix::fs::FlockOperation::LockExclusive)
                .expect("lease");
            assert_eq!(record_at(state).expect("record"), Some(current.clone()));
            let waiting = record_after_handoff(state, &previous, &installation, Some("new"))
                .expect("observed");
            assert_eq!(waiting, None);
            let started = Instant::now();
            let error =
                short(|| record_after_handoff(state, &previous, &installation, Some("new")))
                    .expect_err("still coordinated");
            assert!(error.0.contains("Timed out"));
            assert!(started.elapsed() < Duration::from_secs(2));
        }
        let found = wait_for(
            || record_after_handoff(state, &previous, &installation, Some("new")),
            Duration::from_secs(2),
        )
        .expect("released");
        assert_eq!(found, current);
        // The readiness probe releases its own lock immediately.
        let probe = fs::File::open(&lock_path).expect("probe");
        rustix::fs::flock(&probe, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .expect("free");
    }

    #[test]
    fn handoff_does_not_create_a_missing_installation_lock() {
        let temp = tempfile::tempdir().expect("temp");
        let service = healthy("new");
        let state = temp.path();
        let current =
            json!({"port": service.port, "token": "current", "pid": 456, "release": "new"});
        let previous =
            json!({"port": service.port, "token": "previous", "pid": 123, "release": "new"});
        write_record(state, &current);
        let error = wait_for(
            || record_after_handoff(state, &previous, state, Some("new")),
            Duration::from_secs(10),
        )
        .expect_err("no lock");
        assert!(error.0.contains("Cannot inspect"));
        assert!(!state.join(".install.lock").exists());
    }

    #[test]
    fn handoff_checks_endpoint_process_and_release() {
        let temp = tempfile::tempdir().expect("temp");
        let service = healthy("new");
        let state = temp.path();
        fs::write(state.join(".install.lock"), "").expect("lock");
        let current =
            json!({"port": service.port, "token": "current", "pid": 456, "release": "new"});
        write_record(state, &current);
        let unchanged = record_after_handoff(state, &current, state, None).expect("same token");
        assert_eq!(unchanged, None);
        for (previous, expected, message) in [
            (
                json!({"port": 1, "token": "previous", "pid": 123}),
                None,
                "endpoint",
            ),
            (
                json!({"port": service.port, "token": "previous", "pid": 456}),
                None,
                "new service process",
            ),
            (
                json!({"port": service.port, "token": "previous", "pid": 123}),
                Some("other"),
                "unexpected release",
            ),
        ] {
            match record_after_handoff(state, &previous, state, expected) {
                Err(Poll::Fatal(error)) => assert!(error.0.contains(message), "{}", error.0),
                other => panic!("{message}: {other:?}"),
            }
        }
    }

    #[test]
    fn handoff_reloads_the_record_after_the_old_token_is_rejected() {
        let temp = tempfile::tempdir().expect("temp");
        let state = temp.path().to_path_buf();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let (seen, record_path) = (observed.clone(), state.join("desktop-service.json"));
        let service = Service::start(move |_, path, headers, _| {
            let token = headers
                .get("x-flightdeck-token")
                .map(|values| values[0].clone())
                .unwrap_or_default();
            seen.lock()
                .expect("log")
                .push((path.to_string(), token.clone()));
            if token == "previous-token" {
                let mut replacement: Value =
                    serde_json::from_slice(&fs::read(&record_path).expect("record")).expect("JSON");
                replacement["token"] = "current-token".into();
                replacement["release"] = "new".into();
                fs::write(&record_path, replacement.to_string()).expect("replace");
                return (403, json!({"ok": false, "error": "Session expired"}));
            }
            (
                200,
                json!({"csrf_token": token, "service": {"release": "new"}, "runtime": {"configured": false}}),
            )
        });
        write_record(
            &state,
            &json!({"port": service.port, "token": "previous-token", "release": "old"}),
        );
        let current = wait_for(|| record_at(&state), Duration::from_secs(2)).expect("record");
        assert_eq!(current["token"], "current-token");
        assert_eq!(
            *observed.lock().expect("log"),
            [
                ("/api/status".to_string(), "previous-token".to_string()),
                ("/api/status".to_string(), "current-token".to_string())
            ]
        );
    }

    #[test]
    fn persistent_authentication_rejection_still_fails_the_bounded_poll() {
        for status in [401, 403] {
            let temp = tempfile::tempdir().expect("temp");
            let service = Service::start(move |_, _, _, _| (status, json!({"ok": false})));
            write_record(
                temp.path(),
                &json!({"port": service.port, "token": "invalid", "release": "new"}),
            );
            let error = short(|| record_at(temp.path())).expect_err("rejected");
            assert!(error.0.contains("Timed out"), "{status}");
        }
    }

    #[test]
    fn other_errors_and_mutations_remain_fatal() {
        let temp = tempfile::tempdir().expect("temp");
        let service = Service::start(|_, _, _, _| (500, json!({"ok": false})));
        let record = json!({"port": service.port, "token": "current", "release": "new"});
        write_record(temp.path(), &record);
        let started = Instant::now();
        let error =
            wait_for(|| record_at(temp.path()), Duration::from_secs(30)).expect_err("server error");
        assert!(error.0.contains("500") && started.elapsed() < Duration::from_secs(5));
        let forbidden = Service::start(|_, _, _, _| (403, json!({"ok": false})));
        let expired = json!({"port": forbidden.port, "token": "expired"});
        assert!(matches!(
            request(&expired, "/api/launcher-update", None),
            Err(Poll::Fatal(_))
        ));
        // A configured runtime means this is not the isolated service.
        let configured = Service::start(|_, _, headers, _| {
            let token = headers
                .get("x-flightdeck-token")
                .map(|values| values[0].clone());
            (
                200,
                json!({"csrf_token": token, "service": {"release": "new"}, "runtime": {"configured": true}}),
            )
        });
        write_record(
            temp.path(),
            &json!({"port": configured.port, "token": "current", "release": "new"}),
        );
        assert!(wait_for(|| record_at(temp.path()), Duration::from_secs(30)).is_err());
        // A missing or unreadable record is only ever waited for.
        let empty = tempfile::tempdir().expect("temp");
        assert!(
            short(|| record_at(empty.path()))
                .expect_err("no record")
                .0
                .contains("Timed out")
        );
    }
}
