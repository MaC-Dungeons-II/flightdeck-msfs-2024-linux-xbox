// SPDX-License-Identifier: MIT
//! What the drivers share: commands with deadlines, guards that stop a child,
//! a wineserver or remove a generated probe on every way out, the staged
//! Xodus workspace, window screenshots and a loopback HTTP fixture.
use crate::util::{Failure, Result, fail, require};
use serde_json::Value;
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{BufRead, BufReader, ErrorKind, Read, Write},
    net::{Ipv4Addr, Shutdown, TcpListener, TcpStream},
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const BACKENDS: [&str; 2] = ["x11", "wayland"];

/// A poisoned lock still holds everything a failed thread recorded.
pub fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The exit code as the reports record it: a fatal signal is negative.
pub fn exit_code(status: ExitStatus) -> i64 {
    status.code().map_or_else(
        || -i64::from(status.signal().unwrap_or_default()),
        i64::from,
    )
}

/// Whether a value a test program reported counts as set: not null, false,
/// zero or empty.
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

/// The lines of a test's output as the reports list them. Wine programs end
/// lines with CR LF, and every other Unicode line boundary counts as well; a
/// final boundary adds no empty line.
pub fn split_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                characters.next_if_eq(&'\n');
                lines.push(std::mem::take(&mut line));
            }
            '\n' | '\u{b}' | '\u{c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}'
            | '\u{2029}' => lines.push(std::mem::take(&mut line)),
            other => line.push(other),
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn label(command: &Command) -> String {
    let program = Path::new(command.get_program());
    program
        .file_name()
        .unwrap_or(program.as_os_str())
        .to_string_lossy()
        .into_owned()
}

pub fn spawn(command: &mut Command) -> Result<Child> {
    command
        .spawn()
        .map_err(|error| Failure(format!("{}: {error}", label(command))))
}

/// Runs a command with inherited streams and no deadline; anything but
/// success fails.
pub fn checked(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .map_err(|error| Failure(format!("{}: {error}", label(command))))?;
    require(
        status.success(),
        format!(
            "{} failed with exit code {}",
            label(command),
            exit_code(status)
        ),
    )
}

/// Waits for a child until the deadline. `None` leaves it running.
pub fn wait_until(child: &mut Child, deadline: Instant) -> Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A child this driver started. Dropping it stops a child that still runs, so
/// no failure between spawning and judging can leave a window or a test
/// program behind: the signal first, a kill once the grace period is over.
pub struct Owned {
    pub child: Child,
    signal: rustix::process::Signal,
    grace: Duration,
}
impl Owned {
    /// Asked to end with SIGTERM, so that a window can close itself.
    pub fn terminated(child: Child) -> Self {
        Self {
            child,
            signal: rustix::process::Signal::TERM,
            grace: Duration::from_secs(5),
        }
    }
    pub fn killed(child: Child) -> Self {
        Self {
            child,
            signal: rustix::process::Signal::KILL,
            grace: Duration::from_secs(5),
        }
    }
    pub fn stop(&mut self) -> Option<ExitStatus> {
        if matches!(self.child.try_wait(), Ok(None))
            && let Some(pid) = rustix::process::Pid::from_raw(self.child.id() as i32)
        {
            let _ = rustix::process::kill_process(pid, self.signal);
        }
        let _ = wait_until(&mut self.child, Instant::now() + self.grace);
        let _ = self.child.kill();
        self.child.wait().ok()
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Runs a command whose streams the caller has set up. `None` means that the
/// deadline passed and the command was killed.
pub fn finished(command: &mut Command, timeout: Duration) -> Result<Option<ExitStatus>> {
    let mut owned = Owned::killed(spawn(command)?);
    wait_until(&mut owned.child, Instant::now() + timeout)
}

pub struct Finished {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

fn collected<R: Read + Send + 'static>(stream: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut data = Vec::new();
        if let Some(mut stream) = stream {
            let _ = stream.read_to_end(&mut data);
        }
        let _ = sender.send(data);
    });
    receiver
}

/// Runs a command and collects both of its streams while it runs, so that no
/// amount of output can block it. The deadline covers the command and every
/// descendant that keeps its streams open, which is how a Wine session that
/// does not end shows up. `None` means that the deadline passed and the
/// command was killed.
pub fn captured(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Option<Finished>> {
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut owned = Owned::killed(spawn(command)?);
    let deadline = Instant::now() + timeout;
    let stdout = collected(owned.child.stdout.take());
    let stderr = collected(owned.child.stderr.take());
    if let (Some(input), Some(mut stdin)) = (input, owned.child.stdin.take()) {
        // A command that exits without reading is judged by its status.
        let _ = stdin.write_all(input);
    }
    let within = |stream: &mpsc::Receiver<Vec<u8>>| {
        stream
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .ok()
    };
    let (Some(stdout), Some(stderr)) = (within(&stdout), within(&stderr)) else {
        return Ok(None);
    };
    Ok(
        wait_until(&mut owned.child, deadline)?.map(|status| Finished {
            status,
            stdout,
            stderr,
        }),
    )
}

/// The variables of one isolated Wine prefix. Everything that would let the
/// host session or another runtime in is removed from the inherited ones.
pub struct Environment {
    pub removed: &'static [&'static str],
    pub set: Vec<(&'static str, OsString)>,
}
impl Environment {
    pub fn apply<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        for name in self.removed {
            command.env_remove(name);
        }
        for (name, value) in &self.set {
            command.env(name, value);
        }
        command
    }
}

/// A Wine prefix in use. Its wineserver, and only that one, is stopped when
/// the work on the prefix ends, fails or panics.
pub struct Prefix<'a> {
    wine: &'a Path,
    environment: &'a Environment,
    stopped: bool,
}
impl<'a> Prefix<'a> {
    pub fn new(wine: &'a Path, environment: &'a Environment) -> Self {
        Self {
            wine,
            environment,
            stopped: false,
        }
    }
    /// Creates or updates the prefix within a minute.
    pub fn boot(&self) -> Result<()> {
        let booted = self.run(&[OsStr::new("wineboot"), OsStr::new("-u")], None, 60)?;
        require(
            booted.status.success(),
            format!(
                "wineboot failed with exit code {}",
                exit_code(booted.status)
            ),
        )
    }
    /// Runs Wine with these arguments; the exit status is left to the caller.
    /// `runtime` replaces the runtime directory, where the broker socket is.
    pub fn run(
        &self,
        arguments: &[&OsStr],
        runtime: Option<&Path>,
        seconds: u64,
    ) -> Result<Finished> {
        let mut command = Command::new(self.wine);
        self.environment.apply(command.args(arguments));
        if let Some(runtime) = runtime {
            command.env("XDG_RUNTIME_DIR", runtime);
        }
        captured(&mut command, None, Duration::from_secs(seconds))?
            .ok_or_else(|| Failure(format!("{} timed out", label(&command))))
    }
    pub fn stop(&mut self) -> Result<()> {
        self.stopped = true;
        let mut command = Command::new(self.wine.with_file_name("wineserver"));
        self.environment.apply(command.arg("-k"));
        captured(&mut command, None, Duration::from_secs(10))?
            .map(|_| ())
            .ok_or_else(|| Failure("wineserver -k timed out".into()))
    }
}
impl Drop for Prefix<'_> {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.stop();
        }
    }
}

/// The resolved stage directory.
pub fn stage(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|error| Failure(format!("{}: {error}", path.display())))
}

/// A test program placed among the staged Xodus command sources. It exists
/// only for the build and is removed whatever that build does. A file that is
/// already there belongs to someone else: it is neither replaced nor removed.
pub struct Probe(PathBuf);
impl Probe {
    pub fn write(stage: &Path, name: &str, program: &str, refusal: &str) -> Result<Self> {
        let sources = stage.join("xodus-src/crates/xodus-cli/src");
        require(
            sources.is_dir(),
            format!(
                "The stage has no Xodus command sources: {}",
                sources.display()
            ),
        )?;
        let directory = sources.join("bin");
        match fs::create_dir(&directory) {
            Err(error) if error.kind() != ErrorKind::AlreadyExists => return Err(error.into()),
            _ => {}
        }
        let path = directory.join(format!("{name}.rs"));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| match error.kind() {
                ErrorKind::AlreadyExists => Failure(refusal.into()),
                _ => Failure(format!("{}: {error}", path.display())),
            })?;
        let probe = Self(path);
        file.write_all(program.as_bytes())?;
        Ok(probe)
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn cargo(stage: &Path, name: &str, offline: bool) -> Command {
    let mut command = Command::new("cargo");
    command
        .arg("build")
        .args(offline.then_some("--offline"))
        .args(["--locked", "--manifest-path"])
        .arg(stage.join("xodus-src/Cargo.toml"))
        .args(["-p", "xodus-cli", "--bin", name]);
    command
}

/// Where Cargo leaves the test program: below the caller's target directory
/// if one is set, as for the native build, else below the staged workspace.
pub fn binary(stage: &Path, name: &str, target: Option<OsString>) -> PathBuf {
    target
        .map_or_else(|| stage.join("xodus-src/target"), PathBuf::from)
        .join("debug")
        .join(name)
}

/// Builds a test program inside the staged Xodus command crate, with Cargo's
/// output in `log` if one is given, and returns the executable.
pub fn built(
    stage: &Path,
    name: &str,
    program: &str,
    refusal: &str,
    offline: bool,
    log: Option<&Path>,
) -> Result<PathBuf> {
    let _probe = Probe::write(stage, name, program, refusal)?;
    let mut command = cargo(stage, name, offline);
    if let Some(log) = log {
        let log = fs::File::create(log)?;
        command.stdout(log.try_clone()?).stderr(log);
    }
    checked(&mut command)?;
    Ok(binary(stage, name, std::env::var_os("CARGO_TARGET_DIR")))
}

/// One line a window test program printed: always a JSON object.
pub fn event(line: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(line)
        .map_err(|error| Failure(format!("The test program printed no JSON: {error}")))?;
    require(value.is_object(), "The test program printed no JSON object")?;
    Ok(value)
}

pub fn hyprctl(list: &str) -> Result<Value> {
    let output = Command::new("hyprctl")
        .args(["-j", list])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| Failure(format!("hyprctl: {error}")))?;
    require(output.status.success(), format!("hyprctl -j {list} failed"))?;
    Ok(serde_json::from_slice(&output.stdout)?)
}

/// The command that captures the test window into `target`. An X11 window is
/// captured by the id the program announced. A native Wayland window has
/// none; it is found by its process among Hyprland's clients and captured by
/// its geometry, which `visible` first requires to be on the workspace its
/// monitor shows, because grim captures whatever is on the screen there.
pub fn screenshot(
    window: Option<&Value>,
    pid: u32,
    target: &Path,
    visible: bool,
    desktop: &mut dyn FnMut(&str) -> Result<Value>,
) -> Result<Vec<OsString>> {
    let window = window
        .ok_or_else(|| Failure("The test program rendered before naming its window".into()))?;
    if !window.is_null() {
        let id = window
            .as_str()
            .map_or_else(|| window.to_string(), str::to_string);
        return Ok(vec![
            "import".into(),
            "-window".into(),
            id.into(),
            target.into(),
        ]);
    }
    let clients = desktop("clients")?;
    let client = clients
        .as_array()
        .and_then(|clients| {
            clients
                .iter()
                .find(|client| client["pid"].as_u64() == Some(pid.into()))
        })
        .ok_or_else(|| Failure("Hyprland lists no window of the test program".into()))?;
    if visible {
        let monitors = desktop("monitors")?;
        let monitor = monitors
            .as_array()
            .and_then(|monitors| {
                monitors
                    .iter()
                    .find(|monitor| !monitor["id"].is_null() && monitor["id"] == client["monitor"])
            })
            .ok_or_else(|| Failure("Hyprland lists no monitor of the test window".into()))?;
        let shown = &monitor["activeWorkspace"]["id"];
        require(
            !shown.is_null() && *shown == client["workspace"]["id"],
            "Test window is not visible",
        )?;
    }
    let pair = |value: &Value| match value.as_array().map(Vec::as_slice) {
        Some([first, second]) if first.is_number() && second.is_number() => {
            Ok((first.clone(), second.clone()))
        }
        _ => fail("Hyprland reported no geometry of the test window"),
    };
    let ((x, y), (width, height)) = (pair(&client["at"])?, pair(&client["size"])?);
    Ok(vec![
        "grim".into(),
        "-g".into(),
        format!("{x},{y} {width}x{height}").into(),
        target.into(),
    ])
}

/// Takes the screenshot, which must be there within three seconds.
pub fn capture(command: &[OsString]) -> Result<()> {
    let mut process = Command::new(&command[0]);
    process.args(&command[1..]).stdin(Stdio::null());
    match finished(&mut process, Duration::from_secs(3))? {
        Some(status) if status.success() => Ok(()),
        Some(status) => fail(format!(
            "{} failed with exit code {}",
            label(&process),
            exit_code(status)
        )),
        None => fail(format!("{} timed out", label(&process))),
    }
}

pub struct Request {
    pub method: String,
    /// The request target as sent, including a query.
    pub path: String,
    headers: Vec<(String, String)>,
    /// Present when the request declared a length.
    pub body: Option<Vec<u8>>,
}
impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(&'static str, &'static str)>,
    pub body: Vec<u8>,
}
impl Reply {
    pub fn unsupported() -> Self {
        Self {
            status: 501,
            headers: vec![("Content-Length", "0")],
            body: Vec::new(),
        }
    }
}

const LINE_MAX: usize = 65536;
const BODY_MAX: usize = 1024 * 1024;

fn line(stream: &mut impl BufRead) -> Option<String> {
    let mut raw = Vec::new();
    stream
        .take(LINE_MAX as u64 + 1)
        .read_until(b'\n', &mut raw)
        .ok()?;
    // Header bytes are not text in any particular encoding; each is kept.
    (raw.len() <= LINE_MAX).then(|| raw.iter().map(|&byte| char::from(byte)).collect())
}

fn request(stream: &mut impl BufRead) -> Option<Request> {
    let first = line(stream)?;
    let mut words = first.split_whitespace();
    let (method, path, version) = (words.next()?, words.next()?, words.next()?);
    if !version.starts_with("HTTP/") || words.next().is_some() {
        return None;
    }
    let mut request = Request {
        method: method.into(),
        path: path.into(),
        headers: Vec::new(),
        body: None,
    };
    loop {
        let text = line(stream)?;
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() {
            break;
        }
        if request.headers.len() >= 100 {
            return None;
        }
        if let Some((name, value)) = text.split_once(':') {
            request
                .headers
                .push((name.trim().into(), value.trim().into()));
        }
    }
    if let Some(length) = request
        .header("Content-Length")
        .and_then(|value| value.parse::<usize>().ok())
    {
        let mut body = vec![0; length.min(BODY_MAX + 1)];
        if length > BODY_MAX || stream.read_exact(&mut body).is_err() {
            return None;
        }
        request.body = Some(body);
    }
    Some(request)
}

/// A timestamp in the fixed-length format of HTTP dates.
pub fn http_date(seconds: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (days, time) = (seconds / 86400, seconds % 86400);
    // Days since 1970 to a civil date, counted in 400-year eras from March.
    let shifted = days + 719_468;
    let (era, day_of_era) = (shifted / 146_097, shifted % 146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!(
        "{}, {day:02} {} {year} {:02}:{:02}:{:02} GMT",
        DAYS[(days % 7) as usize],
        MONTHS[(month - 1) as usize],
        time / 3600,
        time % 3600 / 60,
        time % 60
    )
}

type Handler = dyn Fn(&Request, &str) -> Option<Reply> + Send + Sync;

fn answer(mut stream: TcpStream, origin: &str, handler: &Handler) {
    let timeout = Some(Duration::from_secs(30));
    let prepared = stream
        .set_nonblocking(false)
        .and_then(|()| stream.set_read_timeout(timeout))
        .and_then(|()| stream.set_write_timeout(timeout))
        .and_then(|()| stream.try_clone());
    let Ok(reader) = prepared else { return };
    // No reply to a request that cannot be read or that the fixture rejects:
    // the connection just ends.
    let Some(reply) =
        request(&mut BufReader::new(reader)).and_then(|request| handler(&request, origin))
    else {
        return;
    };
    let reason = match reply.status {
        200 => "OK",
        403 => "Forbidden",
        _ => "Not Implemented",
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    // HTTP/1.0 without a length: the end of the connection ends the body,
    // and a refused form submission is an entirely empty document.
    let mut head = format!(
        "HTTP/1.0 {} {reason}\r\nServer: flightdeck-tools\r\nDate: {}\r\n",
        reply.status,
        http_date(now)
    );
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(&reply.body));
    let _ = stream.shutdown(Shutdown::Write);
}

/// A loopback-only HTTP server with one thread per connection. The handler
/// receives each request and this server's origin; `None` drops the
/// connection unanswered. Dropping the fixture stops accepting connections.
pub struct Fixture {
    pub origin: String,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Fixture {
    pub fn start(
        handler: impl Fn(&Request, &str) -> Option<Reply> + Send + Sync + 'static,
    ) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let origin = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let (stopped, served) = (stop.clone(), origin.clone());
        let handler: Arc<Handler> = Arc::new(handler);
        let worker = std::thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let Ok((stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let (origin, handler) = (served.clone(), handler.clone());
                std::thread::spawn(move || answer(stream, &origin, &*handler));
            }
        });
        Ok(Self {
            origin,
            stop,
            worker: Some(worker),
        })
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
pub mod fixtures {
    use std::{
        fs,
        io::Write,
        path::Path,
        process::{Command, Stdio},
    };

    /// A stand-in for a tool the drivers start. It is written by a child
    /// process: were it written here, a process another test thread starts at
    /// that moment would hold it open for writing for an instant, and
    /// executing it then fails with "Text file busy".
    pub fn script(path: &Path, text: &str) {
        let mut writer = Command::new("sh")
            .args(["-c", "umask 077 && cat > \"$0\" && chmod 700 \"$0\""])
            .arg(path)
            .stdin(Stdio::piped())
            .spawn()
            .expect("writer");
        writer
            .stdin
            .take()
            .expect("input")
            .write_all(format!("#!/bin/sh\n{text}\n").as_bytes())
            .expect("script");
        assert!(writer.wait().expect("writer").success());
    }

    /// A directory that looks like a stage to the drivers that build a probe.
    pub fn staged() -> tempfile::TempDir {
        let stage = tempfile::tempdir().expect("stage");
        fs::create_dir_all(stage.path().join("xodus-src/crates/xodus-cli/src")).expect("sources");
        stage
    }

    /// A request as the fixture hands it to a handler.
    pub fn request(
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> super::Request {
        super::Request {
            method: method.into(),
            path: path.into(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            body: body.map(<[u8]>::to_vec),
        }
    }

    pub fn port(origin: &str) -> u16 {
        origin
            .rsplit(':')
            .next()
            .and_then(|port| port.parse().ok())
            .expect("port")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        fixtures::{port, script, staged},
        *,
    };
    use serde_json::json;

    #[test]
    fn lines_end_at_every_boundary_without_a_final_empty_line() {
        assert_eq!(
            split_lines("PASS a\r\nPASS b\n\nSUMMARY failures=0\r"),
            ["PASS a", "PASS b", "", "SUMMARY failures=0"]
        );
        assert_eq!(split_lines("a\u{2028}b\u{c}c"), ["a", "b", "c"]);
        assert_eq!(split_lines("a\r\rb"), ["a", "", "b"]);
        assert!(split_lines("").is_empty());
        assert_eq!(split_lines("\n"), [""]);
    }

    #[test]
    fn exit_codes_are_negative_for_signals_and_values_have_truth() {
        let status = |script: &str| {
            Command::new("sh")
                .args(["-c", script])
                .status()
                .expect("status")
        };
        assert_eq!(exit_code(status("exit 0")), 0);
        assert_eq!(exit_code(status("exit 3")), 3);
        assert_eq!(exit_code(status("kill -TERM $$")), -15);
        for value in [json!(true), json!(1), json!(0.5), json!("x"), json!([0])] {
            assert!(truthy(&value), "{value}");
        }
        for value in [
            json!(null),
            json!(false),
            json!(0),
            json!(0.0),
            json!(""),
            json!([]),
            json!({}),
        ] {
            assert!(!truthy(&value), "{value}");
        }
    }

    #[test]
    fn captured_commands_return_both_streams_or_are_killed_at_the_deadline() {
        let mut command = Command::new("sh");
        // More than a pipe holds: the streams are read while the command runs.
        command.args([
            "-c",
            "cat; head -c 200000 /dev/zero; echo problem >&2; exit 4",
        ]);
        let result = captured(&mut command, Some(b"input"), Duration::from_secs(30))
            .expect("run")
            .expect("in time");
        assert_eq!(exit_code(result.status), 4);
        assert!(result.stdout.starts_with(b"input") && result.stdout.len() == 200_005);
        assert_eq!(result.stderr, b"problem\n");
        let started = Instant::now();
        let mut slow = Command::new("sleep");
        slow.arg("30");
        assert!(
            captured(&mut slow, None, Duration::from_millis(100))
                .expect("run")
                .is_none()
        );
        let mut plain = Command::new("sleep");
        plain.arg("30");
        assert!(
            finished(&mut plain, Duration::from_millis(100))
                .expect("run")
                .is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(captured(&mut Command::new("/nonexistent/tool"), None, Duration::ZERO).is_err());
        assert!(checked(Command::new("sh").args(["-c", "exit 0"])).is_ok());
        assert_eq!(
            checked(Command::new("sh").args(["-c", "exit 2"]))
                .expect_err("failure")
                .0,
            "sh failed with exit code 2"
        );
    }

    #[test]
    fn owned_children_are_stopped_on_drop_and_killed_after_the_grace_period() {
        let sleeper = || Command::new("sleep").arg("30").spawn().expect("process");
        let mut gentle = Owned::terminated(sleeper());
        assert_eq!(gentle.stop().and_then(|status| status.signal()), Some(15));
        let mut hard = Owned::killed(sleeper());
        assert_eq!(hard.stop().and_then(|status| status.signal()), Some(9));
        // A child that already exited is only collected.
        let mut done = Owned::terminated(Command::new("true").spawn().expect("process"));
        assert_eq!(
            wait_until(&mut done.child, Instant::now() + Duration::from_secs(10))
                .expect("wait")
                .map(exit_code),
            Some(0)
        );
        assert_eq!(done.stop().map(exit_code), Some(0));
        let mut deaf = Command::new("sh");
        deaf.args(["-c", "trap '' TERM; echo ready; while :; do :; done"])
            .stdout(Stdio::piped());
        let mut deaf = Owned::terminated(deaf.spawn().expect("process"));
        deaf.grace = Duration::from_millis(200);
        let mut ready = String::new();
        BufReader::new(deaf.child.stdout.take().expect("stdout"))
            .read_line(&mut ready)
            .expect("ready");
        assert_eq!(deaf.stop().and_then(|status| status.signal()), Some(9));
        // Dropping is stopping: the failing step returns at once, not after
        // the half minute its child would have run.
        let started = Instant::now();
        let failing = || -> Result<()> {
            let _owned = Owned::terminated(sleeper());
            fail("A later step failed")
        };
        assert!(failing().is_err());
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn wine_prefix_runs_with_its_environment_and_always_stops_its_wineserver() {
        let temp = tempfile::tempdir().expect("temp");
        let wine = temp.path().join("wine");
        script(
            &wine,
            "echo \"$WINEPREFIX|${DISPLAY-unset}|${XDG_RUNTIME_DIR-unset}|$*\"; [ \"$1\" != broken ]",
        );
        script(
            &temp.path().join("wineserver"),
            "echo \"$WINEPREFIX $*\" >> \"$0.log\"",
        );
        let environment = Environment {
            removed: &["DISPLAY", "XDG_RUNTIME_DIR"],
            set: vec![("WINEPREFIX", "/synthetic/prefix".into())],
        };
        let log = temp.path().join("wineserver.log");
        let stops = || fs::read_to_string(&log).unwrap_or_default();
        let mut prefix = Prefix::new(&wine, &environment);
        prefix.boot().expect("boot");
        let run = prefix
            .run(
                &[OsStr::new("test.exe"), OsStr::new("case")],
                Some(Path::new("/synthetic/runtime")),
                30,
            )
            .expect("run");
        assert_eq!(
            String::from_utf8_lossy(&run.stdout),
            "/synthetic/prefix|unset|/synthetic/runtime|test.exe case\n"
        );
        let plain = prefix.run(&[OsStr::new("broken")], None, 30).expect("run");
        assert_eq!(exit_code(plain.status), 1);
        assert_eq!(
            String::from_utf8_lossy(&plain.stdout),
            "/synthetic/prefix|unset|unset|broken\n"
        );
        assert_eq!(stops(), "");
        prefix.stop().expect("stop");
        drop(prefix);
        assert_eq!(stops(), "/synthetic/prefix -k\n");
        // A failure on the way leaves through the same stop.
        let failing = || -> Result<()> {
            let prefix = Prefix::new(&wine, &environment);
            prefix.run(&[OsStr::new("test.exe")], None, 30)?;
            fail("The test binary reported nothing")
        };
        assert!(failing().is_err());
        assert_eq!(stops(), "/synthetic/prefix -k\n/synthetic/prefix -k\n");
        script(&wine, "exit 1");
        assert_eq!(
            Prefix::new(&wine, &environment).boot().expect_err("boot").0,
            "wineboot failed with exit code 1"
        );
        // A runner without a wineserver cannot be stopped, which is reported.
        let alone = tempfile::tempdir().expect("temp");
        let wine = alone.path().join("wine");
        assert!(Prefix::new(&wine, &environment).stop().is_err());
    }

    #[test]
    fn probe_is_written_only_into_a_stage_and_removed_again() {
        let stage = staged();
        let path = stage
            .path()
            .join("xodus-src/crates/xodus-cli/src/bin/flightdeck-synthetic-test.rs");
        {
            let _probe = Probe::write(
                stage.path(),
                "flightdeck-synthetic-test",
                "fn main() {}\n",
                "x",
            )
            .expect("probe");
            assert_eq!(fs::read_to_string(&path).expect("probe"), "fn main() {}\n");
            // A second one is refused and leaves the first alone.
            let error = Probe::write(
                stage.path(),
                "flightdeck-synthetic-test",
                "other",
                "Refusing to replace an existing probe",
            )
            .err()
            .expect("refusal");
            assert_eq!(error.0, "Refusing to replace an existing probe");
            assert_eq!(fs::read_to_string(&path).expect("probe"), "fn main() {}\n");
        }
        assert!(!path.exists() && path.parent().expect("bin").is_dir());
        // The build fails here, because the stage has no workspace; the
        // probe is gone all the same.
        let failed = built(
            stage.path(),
            "flightdeck-synthetic-test",
            "fn main() {}\n",
            "x",
            true,
            Some(&stage.path().join("build.log")),
        );
        assert!(failed.is_err() && !path.exists());
        assert!(stage.path().join("build.log").is_file());
        let empty = tempfile::tempdir().expect("temp");
        let error = Probe::write(empty.path(), "flightdeck-synthetic-test", "", "x")
            .err()
            .expect("no sources");
        assert!(
            error
                .0
                .starts_with("The stage has no Xodus command sources: "),
            "{}",
            error.0
        );
        assert!(!empty.path().join("xodus-src").exists());
        assert!(super::stage(&empty.path().join("missing")).is_err());
        assert_eq!(
            super::stage(&stage.path().join("xodus-src/../xodus-src")).expect("stage"),
            fs::canonicalize(stage.path())
                .expect("path")
                .join("xodus-src")
        );
    }

    #[test]
    fn cargo_builds_the_probe_in_the_staged_workspace() {
        let stage = Path::new("/synthetic/stage");
        let words = |command: &Command| {
            command
                .get_args()
                .map(|word| word.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        let online = cargo(stage, "flightdeck-store-window-test", false);
        assert_eq!(online.get_program(), "cargo");
        assert_eq!(
            words(&online),
            [
                "build",
                "--locked",
                "--manifest-path",
                "/synthetic/stage/xodus-src/Cargo.toml",
                "-p",
                "xodus-cli",
                "--bin",
                "flightdeck-store-window-test"
            ]
        );
        // The caller's environment is Cargo's: nothing is set or removed.
        assert_eq!(online.get_envs().count(), 0);
        assert_eq!(
            words(&cargo(stage, "flightdeck-login-flow-test", true))[..3],
            ["build", "--offline", "--locked"]
        );
        assert_eq!(
            binary(stage, "flightdeck-login-flow-test", None),
            Path::new("/synthetic/stage/xodus-src/target/debug/flightdeck-login-flow-test")
        );
        assert_eq!(
            binary(stage, "probe", Some("/synthetic/target".into())),
            Path::new("/synthetic/target/debug/probe")
        );
    }

    #[test]
    fn events_are_json_objects() {
        assert_eq!(
            event("{\"window\":null,\"pid\":7}\n").expect("event")["pid"],
            7
        );
        for line in ["", "\n", "warning: text", "[1]", "7", "{\"a\":1} trailing"] {
            assert!(event(line).is_err(), "{line:?}");
        }
    }

    #[test]
    fn screenshots_use_the_x11_window_or_the_visible_hyprland_client() {
        let target = Path::new("/synthetic/window.png");
        let words = |command: Result<Vec<OsString>>| {
            command.map(|words| {
                words
                    .iter()
                    .map(|word| word.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
        };
        let mut unused = |_: &str| -> Result<Value> { fail("Hyprland is not asked on X11") };
        assert_eq!(
            words(screenshot(
                Some(&json!(73400323_u64)),
                7,
                target,
                true,
                &mut unused
            ))
            .expect("x11"),
            ["import", "-window", "73400323", "/synthetic/window.png"]
        );
        assert!(screenshot(None, 7, target, true, &mut unused).is_err());
        let desktop = |workspace: u64| {
            move |list: &str| -> Result<Value> {
                Ok(match list {
                    "clients" => json!([
                        {"pid": 6, "monitor": 0, "workspace": {"id": 1}, "at": [0, 0], "size": [1, 1]},
                        {"pid": 7, "monitor": 1, "workspace": {"id": workspace}, "at": [1930, 40],
                            "size": [720, 820]}]),
                    "monitors" => json!([{"id": 0, "activeWorkspace": {"id": 1}},
                        {"id": 1, "activeWorkspace": {"id": 3}}]),
                    _ => return fail("Unexpected list"),
                })
            }
        };
        let grim = ["grim", "-g", "1930,40 720x820", "/synthetic/window.png"];
        let wayland = json!(null);
        assert_eq!(
            words(screenshot(Some(&wayland), 7, target, true, &mut desktop(3))).expect("wayland"),
            grim
        );
        assert_eq!(
            words(screenshot(Some(&wayland), 7, target, true, &mut desktop(2)))
                .expect_err("hidden")
                .0,
            "Test window is not visible"
        );
        // The navigation test captures without the visibility requirement.
        let mut asked = Vec::new();
        let mut recorded = |list: &str| {
            asked.push(list.to_string());
            desktop(2)(list)
        };
        assert_eq!(
            words(screenshot(Some(&wayland), 7, target, false, &mut recorded)).expect("wayland"),
            grim
        );
        assert_eq!(asked, ["clients"]);
        assert!(screenshot(Some(&wayland), 8, target, true, &mut desktop(3)).is_err());
        assert!(capture(&["true".into()]).is_ok());
        assert_eq!(
            capture(&["false".into()]).expect_err("failure").0,
            "false failed with exit code 1"
        );
    }

    #[test]
    fn http_dates_have_the_fixed_format() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(951_782_399), "Mon, 28 Feb 2000 23:59:59 GMT");
        assert_eq!(http_date(951_782_400), "Tue, 29 Feb 2000 00:00:00 GMT");
        assert_eq!(http_date(1_791_835_530), "Mon, 12 Oct 2026 20:05:30 GMT");
        assert_eq!(http_date(4_102_444_800), "Fri, 01 Jan 2100 00:00:00 GMT");
    }

    #[test]
    fn fixture_answers_over_loopback_and_drops_what_the_handler_rejects() {
        let server = Fixture::start(|request, origin| {
            if request.path == "/reject" {
                return None;
            }
            if request.method == "PUT" {
                return Some(Reply::unsupported());
            }
            let text = format!(
                "{} {} {} {:?} {origin}",
                request.method,
                request.path,
                request.header("x-synthetic").unwrap_or("none"),
                request
                    .body
                    .as_deref()
                    .map(|body| String::from_utf8_lossy(body).into_owned()),
            );
            Some(Reply {
                status: if request.method == "POST" { 403 } else { 200 },
                headers: vec![("Content-Type", "text/html")],
                body: text.into_bytes(),
            })
        })
        .expect("fixture");
        let port = port(&server.origin);
        let timeout = Duration::from_secs(10);
        let get = crate::http::request(
            port,
            "GET",
            "/page?mode",
            &[("X-Synthetic", "yes")],
            None,
            timeout,
        )
        .expect("response");
        assert_eq!(get.status, 200);
        assert_eq!(get.headers["content-type"], "text/html");
        assert_eq!(get.headers["server"], "flightdeck-tools");
        assert!(get.headers["date"].ends_with(" GMT"));
        assert_eq!(
            String::from_utf8_lossy(&get.body),
            format!("GET /page?mode yes None {}", server.origin)
        );
        let post = crate::http::request(port, "POST", "/form", &[], Some(b"a=1&b=2"), timeout)
            .expect("response");
        assert_eq!(post.status, 403);
        assert_eq!(
            String::from_utf8_lossy(&post.body),
            format!("POST /form none Some(\"a=1&b=2\") {}", server.origin)
        );
        assert_eq!(
            crate::http::request(port, "PUT", "/", &[], None, timeout)
                .expect("response")
                .status,
            501
        );
        assert!(crate::http::request(port, "GET", "/reject", &[], None, timeout).is_err());
        // A declared body that never arrives in full is no request.
        assert!(
            crate::http::request(
                port,
                "POST",
                "/form",
                &[("Content-Length", "9")],
                Some(b"a=1"),
                Duration::from_millis(300)
            )
            .is_err()
        );
        drop(server);
        assert!(
            crate::http::request(port, "GET", "/", &[], None, Duration::from_millis(300)).is_err()
        );
    }
}
