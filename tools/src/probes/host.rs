// SPDX-License-Identifier: MIT
//! What the probes kept from their Python originals: pathlib's directory and
//! path rules, shutil's copies, subprocess's deadline and `try/finally`. It
//! starts only what a probe hands it and knows nothing about Wine itself.
use crate::{
    http,
    util::{Failure, Result, fail, require},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::DirBuilderExt, process::ExitStatusExt},
    path::{Component, Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

/// A complete child environment. It crosses the probe driver as JSON, so
/// names and values are text.
pub type Environment = BTreeMap<String, String>;

/// pathlib's default directory mode, before the umask.
pub const DEFAULT: u32 = 0o777;
pub const PRIVATE: u32 = 0o700;

pub fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn at(path: &Path) -> impl Fn(std::io::Error) -> Failure + '_ {
    move |error| Failure(format!("{}: {error}", path.display()))
}

/// The host environment without the variables that select a Wine build or
/// steer its renderers. An entry that is not UTF-8 cannot be sent to the
/// driver and is left out.
pub fn scrubbed(variables: impl IntoIterator<Item = (OsString, OsString)>) -> Environment {
    const SELECTING: [&str; 5] = ["WINE", "DXVK", "VKD3D", "PROTON", "NVIDIA_WINE"];
    variables
        .into_iter()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(name, _)| !SELECTING.iter().any(|prefix| name.starts_with(prefix)))
        .collect()
}

/// Adds or replaces entries, like `dict.update`.
pub fn set<const N: usize>(environment: &mut Environment, entries: [(&str, &str); N]) {
    environment.extend(entries.map(|(name, value)| (name.to_string(), value.to_string())));
}

/// Python's truth value of a JSON answer.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

/// Indented JSON with a final newline, fields in their declared order.
pub fn pretty(value: &impl serde::Serialize) -> Result<String> {
    Ok(serde_json::to_string_pretty(value)? + "\n")
}

pub fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_slice(&fs::read(path).map_err(at(path))?)
        .map_err(|error| Failure(format!("{}: {error}", path.display())))
}

pub fn write(path: &Path, content: impl AsRef<[u8]>) -> Result<()> {
    fs::write(path, content).map_err(at(path))
}

/// The strict `resolve`: the path must exist.
pub fn canonical(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(at(path))
}

/// The lenient `resolve`: absolute, links followed as far as the path exists
/// and the remainder taken literally.
pub fn resolve(path: &Path) -> Result<PathBuf> {
    let mut resolved = PathBuf::new();
    for part in std::path::absolute(path).map_err(at(path))?.components() {
        match part {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            other => resolved.push(other),
        }
        if let Ok(real) = fs::canonicalize(&resolved) {
            resolved = real;
        }
    }
    Ok(resolved)
}

fn mkdir(path: &Path, mode: u32, parents: bool, exist_ok: bool) -> Result<()> {
    // Only the last directory receives the mode; pathlib creates missing
    // parents with the default one.
    if parents && let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(at(parent))?;
    }
    match fs::DirBuilder::new().mode(mode).create(path) {
        Err(error)
            if !(exist_ok
                && error.kind() == std::io::ErrorKind::AlreadyExists
                && path.is_dir()) =>
        {
            Err(at(path)(error))
        }
        _ => Ok(()),
    }
}
/// `mkdir()`: the parent exists, the directory does not.
pub fn new_dir(path: &Path, mode: u32) -> Result<()> {
    mkdir(path, mode, false, false)
}
/// `mkdir(parents=True)`: the directory itself must still be new.
pub fn new_dirs(path: &Path, mode: u32) -> Result<()> {
    mkdir(path, mode, true, false)
}
/// `mkdir(parents=True, exist_ok=True)`.
pub fn dirs(path: &Path, mode: u32) -> Result<()> {
    mkdir(path, mode, true, true)
}

/// `shutil.copyfile`: the content only, through links, with a default mode.
pub fn copyfile(source: &Path, destination: &Path) -> Result<()> {
    let mut input = fs::File::open(source).map_err(at(source))?;
    let mut output = fs::File::create(destination).map_err(at(destination))?;
    std::io::copy(&mut input, &mut output).map_err(at(destination))?;
    Ok(())
}

fn copy_times(source: &Path, destination: &Path) -> Result<()> {
    let info = fs::metadata(source).map_err(at(source))?;
    let times = fs::FileTimes::new()
        .set_accessed(info.accessed()?)
        .set_modified(info.modified()?);
    fs::File::open(destination)
        .and_then(|file| file.set_times(times))
        .map_err(at(destination))
}

/// `shutil.copy2`: content, permission bits and timestamps. Extended
/// attributes are not carried over.
pub fn copy2(source: &Path, destination: &Path) -> Result<()> {
    fs::copy(source, destination).map_err(at(source))?;
    copy_times(source, destination)
}

/// `shutil.copytree` into a new directory. Links are followed unless
/// `symlinks` recreates them. The first error ends the copy.
pub fn copytree(source: &Path, destination: &Path, symlinks: bool) -> Result<()> {
    new_dirs(destination, DEFAULT)?;
    for entry in fs::read_dir(source).map_err(at(source))? {
        let entry = entry?;
        let (from, to) = (entry.path(), destination.join(entry.file_name()));
        if symlinks && entry.file_type()?.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&from)?, &to).map_err(at(&to))?;
        } else if fs::metadata(&from).map_err(at(&from))?.is_dir() {
            copytree(&from, &to, symlinks)?;
        } else {
            copy2(&from, &to)?;
        }
    }
    let mode = fs::metadata(source).map_err(at(source))?.permissions();
    fs::set_permissions(destination, mode).map_err(at(destination))?;
    copy_times(source, destination)
}

/// A finished child: Python's `returncode`, which is the negative signal
/// number after a signal, and whatever was captured.
pub struct Finished {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub fn returncode(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| -signal))
        .unwrap_or(-1)
}

/// `subprocess.run(..., timeout=seconds)`: at the deadline the child itself
/// is killed and the run fails. Its descendants are the caller's to stop.
/// Piped output is collected; an exit status other than zero is no failure.
pub fn run(command: &mut Command, input: Option<&[u8]>, seconds: u64) -> Result<Finished> {
    let label = command.get_program().to_string_lossy().into_owned();
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .spawn()
        .map_err(|error| Failure(format!("{label}: {error}")))?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let timed_out = |mut child: Child| {
        let _ = child.kill();
        let _ = child.wait();
        fail(format!("{label} timed out after {seconds} seconds"))
    };
    if let (Some(mut pipe), Some(input)) = (child.stdin.take(), input.map(<[u8]>::to_vec)) {
        // A child that ends without reading its input is no failure.
        std::thread::spawn(move || {
            let _ = pipe.write_all(&input);
        });
    }
    let (sender, receiver) = mpsc::channel();
    let pipes: [Option<Box<dyn Read + Send>>; 2] = [
        child.stdout.take().map(|pipe| Box::new(pipe) as _),
        child.stderr.take().map(|pipe| Box::new(pipe) as _),
    ];
    let mut captured = [Vec::new(), Vec::new()];
    let mut pending = 0;
    for (index, pipe) in pipes.into_iter().enumerate() {
        let Some(mut pipe) = pipe else { continue };
        let sender = sender.clone();
        pending += 1;
        std::thread::spawn(move || {
            let mut data = Vec::new();
            let _ = pipe.read_to_end(&mut data);
            let _ = sender.send((index, data));
        });
    }
    // The pipes close when the child ends, so a captured run is noticed at
    // once. The benchmark measures such runs; polling would add to its times.
    while pending > 0 {
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((index, data)) => {
                captured[index] = data;
                pending -= 1;
            }
            Err(_) => return timed_out(child),
        }
    }
    let mut pause = Duration::from_micros(100);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            return timed_out(child);
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(20));
    };
    let [stdout, stderr] = captured;
    Ok(Finished {
        code: returncode(status),
        stdout,
        stderr,
    })
}

/// `subprocess.run(..., check=True)` without a deadline, output not captured.
pub fn checked(command: &mut Command) -> Result<()> {
    let label = command.get_program().to_string_lossy().into_owned();
    let status = command
        .status()
        .map_err(|error| Failure(format!("{label}: {error}")))?;
    require(
        status.success(),
        format!("{label} failed with exit status {}", returncode(status)),
    )
}

/// Runs with both output streams in one new log file and returns the exit
/// status.
pub fn logged(command: &mut Command, log: &Path, seconds: u64) -> Result<i32> {
    let file = fs::File::create(log).map_err(at(log))?;
    command.stdout(file.try_clone()?).stderr(file);
    Ok(run(command, None, seconds)?.code)
}

/// Whether both Wine programs of a Proton-style runner are present.
pub fn proton_style(runner: &Path) -> bool {
    ["wine", "wineserver"]
        .iter()
        .all(|name| runner.join("files/bin").join(name).is_file())
}

/// Ends the wineserver of a probe's environment and waits until it is gone.
/// A refusing wineserver is ignored, as there may be none; a hanging one is
/// an error. `configure` sets the standard streams.
pub fn stop_wine(
    wineserver: &Path,
    environment: &Environment,
    configure: impl Fn(&mut Command),
) -> Result<()> {
    for option in ["-k", "-w"] {
        let mut command = Command::new(wineserver);
        command.arg(option).env_clear().envs(environment);
        configure(&mut command);
        run(&mut command, None, 10)?;
    }
    Ok(())
}

/// `try/finally`: the cleanup has already run. Its own failure is reported,
/// together with the failure it followed.
pub fn finally<T>(body: Result<T>, cleanup: Result<()>) -> Result<T> {
    match (body, cleanup) {
        (body, Ok(())) => body,
        (Ok(_), Err(error)) => Err(error),
        (Err(first), Err(error)) => fail(format!("{error}\nwhile handling: {first}")),
    }
}

/// The first line a started service prints, or `missing` when none arrives
/// in time. Later output is drained, so the service never blocks on a full
/// pipe or fails on a closed one.
pub fn first_line(child: &mut Child, seconds: u64, missing: &str) -> Result<String> {
    let output = child
        .stdout
        .take()
        .ok_or_else(|| Failure("The service output is not captured".into()))?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(output);
        let mut line = Vec::new();
        let _ = reader.read_until(b'\n', &mut line);
        let _ = sender.send(line);
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    });
    let line = receiver
        .recv_timeout(Duration::from_secs(seconds))
        .map_err(|_| Failure(missing.into()))?;
    String::from_utf8(line).map_err(|_| Failure("The service output is not UTF-8".into()))
}

pub fn interrupt(child: &Child) {
    if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::INT);
    }
}

/// Whether the child ended within the time; it is reaped when it did.
pub fn exited_within(child: &mut Child, seconds: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if !matches!(child.try_wait(), Ok(None)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The authenticated JSON API of a started service.
pub struct Api {
    pub port: u16,
    pub token: String,
    pub seconds: u64,
}
impl Api {
    /// GET without a body and POST with one. `token` replaces the session
    /// token. Anything other than a JSON answer with status 200 fails.
    pub fn request(&self, path: &str, body: Option<&Value>, token: Option<&str>) -> Result<Value> {
        let body = body.map(|value| value.to_string().into_bytes());
        let method = if body.is_some() { "POST" } else { "GET" };
        let headers = [
            ("Content-Type", "application/json"),
            ("X-Flightdeck-Token", token.unwrap_or(&self.token)),
        ];
        let response = http::request(
            self.port,
            method,
            path,
            &headers,
            body.as_deref(),
            Duration::from_secs(self.seconds),
        )?;
        let value: Value = serde_json::from_slice(&response.body)
            .map_err(|error| Failure(format!("{method} {path}: {error}")))?;
        require(
            response.status == 200,
            format!("{method} {path} answered {}: {value}", response.status),
        )?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fixtures::Service;
    use serde_json::json;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).expect("metadata").mode() & 0o7777
    }

    #[test]
    fn renderer_and_wine_selection_is_removed_from_the_host_environment() {
        let host = [
            ("HOME", "/home/pilot"),
            ("WINEPREFIX", "/other"),
            ("WINE", "wine"),
            ("DXVK_HUD", "1"),
            ("VKD3D_CONFIG", "x"),
            ("PROTON_LOG", "1"),
            ("NVIDIA_WINE_DLSS", "1"),
            ("NVIDIA_VISIBLE", "kept"),
            ("XWINE", "kept"),
        ]
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));
        let invalid = {
            use std::os::unix::ffi::OsStringExt;
            (OsString::from("BROKEN"), OsString::from_vec(vec![0xff]))
        };
        let mut environment = scrubbed(host.into_iter().chain([invalid]));
        assert_eq!(
            environment.keys().collect::<Vec<_>>(),
            ["HOME", "NVIDIA_VISIBLE", "XWINE"]
        );
        set(&mut environment, [("HOME", "/new"), ("WINEDEBUG", "-all")]);
        assert_eq!(environment["HOME"], "/new");
        assert_eq!(environment["WINEDEBUG"], "-all");
    }

    #[test]
    fn truth_values_follow_python() {
        for value in [
            json!(null),
            json!(false),
            json!(0),
            json!(""),
            json!([]),
            json!({}),
        ] {
            assert!(!truthy(&value), "{value}");
        }
        for value in [
            json!(true),
            json!(1),
            json!("0"),
            json!([0]),
            json!({"a": null}),
        ] {
            assert!(truthy(&value), "{value}");
        }
    }

    #[test]
    fn only_the_last_directory_receives_the_mode() {
        let temp = tempfile::tempdir().expect("temp");
        let leaf = temp.path().join("a/b/private");
        new_dirs(&leaf, PRIVATE).expect("create");
        assert_eq!(mode(&leaf), 0o700);
        // The parents keep the default mode, reduced only by the umask.
        assert_ne!(mode(&temp.path().join("a/b")) & 0o100, 0);
        assert_eq!(mode(&temp.path().join("a/b")), mode(&temp.path().join("a")));
        assert!(new_dirs(&leaf, PRIVATE).is_err());
        assert!(new_dir(&leaf, DEFAULT).is_err());
        dirs(&leaf, PRIVATE).expect("existing");
        assert!(new_dir(&temp.path().join("missing/leaf"), DEFAULT).is_err());
        new_dir(&temp.path().join("a/local"), DEFAULT).expect("plain");
        fs::write(temp.path().join("file"), "").expect("file");
        assert!(dirs(&temp.path().join("file"), DEFAULT).is_err());
    }

    #[test]
    fn lenient_resolution_follows_links_and_keeps_a_missing_tail() {
        let temp = tempfile::tempdir().expect("temp");
        let root = fs::canonicalize(temp.path()).expect("root");
        fs::create_dir_all(root.join("real/inner")).expect("directories");
        symlink(root.join("real/inner"), root.join("link")).expect("link");
        assert_eq!(
            resolve(&root.join("link/new/output")).expect("resolved"),
            root.join("real/inner/new/output")
        );
        // `..` leaves the link's target, not the directory holding the link.
        assert_eq!(
            resolve(&root.join("link/../sibling")).expect("resolved"),
            root.join("real/sibling")
        );
        assert_eq!(
            resolve(&root.join("missing/./a/../b")).expect("resolved"),
            root.join("missing/b")
        );
        assert!(
            resolve(Path::new("relative"))
                .expect("absolute")
                .is_absolute()
        );
        assert!(canonical(&root.join("missing")).is_err());
        assert_eq!(
            canonical(&root.join("link")).expect("strict"),
            root.join("real/inner")
        );
    }

    #[test]
    fn copies_keep_what_their_python_originals_kept() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("nested")).expect("source");
        fs::write(source.join("tool.sh"), "#!/bin/sh\n").expect("tool");
        fs::set_permissions(source.join("tool.sh"), fs::Permissions::from_mode(0o750))
            .expect("mode");
        fs::write(source.join("nested/data"), "data").expect("data");
        symlink("tool.sh", source.join("alias")).expect("alias");
        let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        fs::File::open(source.join("tool.sh"))
            .expect("open")
            .set_times(fs::FileTimes::new().set_modified(old))
            .expect("times");

        let followed = temp.path().join("deep/followed");
        copytree(&source, &followed, false).expect("copy");
        assert_eq!(mode(&followed.join("tool.sh")), 0o750);
        assert_eq!(
            fs::metadata(followed.join("tool.sh"))
                .expect("info")
                .modified()
                .expect("time"),
            old
        );
        assert!(
            fs::symlink_metadata(followed.join("alias"))
                .expect("alias")
                .is_file()
        );
        assert_eq!(
            fs::read(followed.join("nested/data")).expect("data"),
            b"data"
        );
        assert!(copytree(&source, &followed, false).is_err());

        let linked = temp.path().join("linked");
        copytree(&source, &linked, true).expect("copy");
        assert_eq!(
            fs::read_link(linked.join("alias")).expect("link"),
            Path::new("tool.sh")
        );

        let plain = temp.path().join("plain");
        copyfile(&source.join("alias"), &plain).expect("content");
        assert_eq!(fs::read(&plain).expect("plain"), b"#!/bin/sh\n");
        assert_eq!(mode(&plain) & 0o111, 0);
    }

    #[test]
    fn a_run_captures_feeds_and_reports_signals_and_deadlines() {
        let mut echo = Command::new("sh");
        echo.args(["-c", "cat; echo problem >&2; exit 3"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let finished = run(&mut echo, Some(b"request"), 30).expect("run");
        assert_eq!(
            (finished.code, &finished.stdout[..], &finished.stderr[..]),
            (3, &b"request"[..], &b"problem\n"[..])
        );
        let killed = run(Command::new("sh").args(["-c", "kill -TERM $$"]), None, 30).expect("run");
        assert_eq!(killed.code, -15);
        let started = Instant::now();
        let error = run(Command::new("sleep").arg("30"), None, 1)
            .err()
            .expect("deadline");
        assert_eq!(error.0, "sleep timed out after 1 seconds");
        assert!(started.elapsed() < Duration::from_secs(10));
        // A captured run that outlives the deadline is ended in the same way.
        let mut silent = Command::new("sleep");
        silent.arg("30").stdout(Stdio::piped());
        assert!(run(&mut silent, None, 1).is_err());
        assert!(run(&mut Command::new("/nonexistent/program"), None, 1).is_err());
        assert!(checked(&mut Command::new("true")).is_ok());
        assert_eq!(
            checked(Command::new("sh").args(["-c", "exit 4"]))
                .expect_err("status")
                .0,
            "sh failed with exit status 4"
        );
    }

    #[test]
    fn logs_receive_both_streams_and_wine_is_stopped_then_awaited() {
        let temp = tempfile::tempdir().expect("temp");
        let log = temp.path().join("case.log");
        let code = logged(
            Command::new("sh").args(["-c", "echo out; echo err >&2; exit 7"]),
            &log,
            30,
        )
        .expect("run");
        assert_eq!(code, 7);
        assert_eq!(fs::read_to_string(&log).expect("log"), "out\nerr\n");

        let runner = temp.path().join("runner");
        fs::create_dir_all(runner.join("files/bin")).expect("runner");
        assert!(!proton_style(&runner));
        let wineserver = runner.join("files/bin/wineserver");
        crate::probes::fixtures::script(
            &wineserver,
            "printf '%s %s\\n' \"$1\" \"$WINEPREFIX\" >> \"$CALLS\"; exit 1",
        );
        fs::write(runner.join("files/bin/wine"), "").expect("wine");
        assert!(proton_style(&runner));
        let calls = temp.path().join("calls");
        let environment = Environment::from([
            ("CALLS".to_string(), text(&calls)),
            ("WINEPREFIX".to_string(), "/prefix".to_string()),
            (
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            ),
        ]);
        stop_wine(&wineserver, &environment, |command| {
            command.stdout(Stdio::null());
        })
        .expect("stopped");
        assert_eq!(
            fs::read_to_string(&calls).expect("calls"),
            "-k /prefix\n-w /prefix\n"
        );
    }

    #[test]
    fn a_failing_cleanup_is_reported_with_the_failure_it_followed() {
        assert_eq!(finally(Ok(1), Ok(())).expect("both"), 1);
        assert_eq!(
            finally(Ok(1), fail("cleanup")).expect_err("cleanup").0,
            "cleanup"
        );
        assert_eq!(
            finally(fail::<u8>("body"), Ok(())).expect_err("body").0,
            "body"
        );
        assert_eq!(
            finally(fail::<u8>("body"), fail("cleanup"))
                .expect_err("both")
                .0,
            "cleanup\nwhile handling: body"
        );
    }

    #[test]
    fn the_first_line_is_awaited_and_an_interrupt_ends_the_service() {
        let mut child = Command::new("sh")
            .args(["-c", "echo ready; echo more; exec sleep 30"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("service");
        assert_eq!(first_line(&mut child, 30, "none").expect("line"), "ready\n");
        assert!(first_line(&mut child, 30, "none").is_err());
        assert!(!exited_within(&mut child, 0));
        interrupt(&child);
        assert!(exited_within(&mut child, 30));

        let mut silent = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::piped())
            .spawn()
            .expect("service");
        assert_eq!(
            first_line(&mut silent, 1, "Service did not start")
                .expect_err("silent")
                .0,
            "Service did not start"
        );
        let _ = silent.kill();
        let _ = silent.wait();
    }

    #[test]
    fn the_api_client_posts_bodies_and_fails_on_refusals() {
        let service = Service::start(|method, path, headers, body| {
            let status = if path == "/api/refused" { 409 } else { 200 };
            (
                status,
                json!({"method": method, "token": headers.get("x-flightdeck-token"),
                    "kind": headers.get("content-type"), "body": String::from_utf8_lossy(body)}),
            )
        });
        let api = Api {
            port: service.port,
            token: "session".into(),
            seconds: 5,
        };
        let read = api.request("/api/status", None, None).expect("read");
        assert_eq!(read["method"], "GET");
        assert_eq!(read["token"], json!(["session"]));
        assert_eq!(read["kind"], json!(["application/json"]));
        let sent = api
            .request("/api/select", Some(&json!({"a": 1})), Some("csrf"))
            .expect("sent");
        assert_eq!(sent["method"], "POST");
        assert_eq!(sent["token"], json!(["csrf"]));
        assert_eq!(sent["body"], r#"{"a":1}"#);
        let refused = api
            .request("/api/refused", None, None)
            .expect_err("refused");
        assert!(
            refused.0.starts_with("GET /api/refused answered 409: "),
            "{}",
            refused.0
        );
    }

    #[test]
    fn reports_keep_their_field_order_and_end_with_a_newline() {
        #[derive(serde::Serialize)]
        struct Report {
            status: &'static str,
            cases: Vec<u8>,
        }
        let report = Report {
            status: "PASS",
            cases: vec![1],
        };
        assert_eq!(
            pretty(&report).expect("text"),
            "{\n  \"status\": \"PASS\",\n  \"cases\": [\n    1\n  ]\n}\n"
        );
        let temp = tempfile::tempdir().expect("temp");
        write(&temp.path().join("value.json"), "{\"a\": [1]}").expect("write");
        assert_eq!(
            read_json(&temp.path().join("value.json")).expect("read")["a"][0],
            1
        );
        assert!(read_json(&temp.path().join("missing.json")).is_err());
    }
}
