// SPDX-License-Identifier: MIT
//! The built Wine IPC layer against a broker whose replies are delayed. The
//! broker here is a local Unix socket that answers with synthetic documents:
//! inventory after six seconds, a server timeout, a late account context, an
//! unparsable ticket. This establishes how the transport waits, times out and
//! rejects late replies; it involves no account and no HTTP request. It needs
//! a staged tree with the built test builtin, MinGW and a Wine runner.
use super::common::{self, Environment, Prefix, checked, exit_code, locked, split_lines};
use crate::util::{Failure, Result, digest, pretty};
use serde_json::{Map, Value, json};
use std::{
    ffi::OsStr,
    fs,
    io::{ErrorKind, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const CASES: [&str; 6] = [
    "inventory-ok",
    "inventory-timeout",
    "auth",
    "updates",
    "account-refresh",
    "account-timeout",
];
const MAGIC: u32 = 0x5844_5358;
const BUILTIN: &str = "artifacts/builtin/x86_64-windows/xodus_store_test.dll";
const REMOVED: [&str; 5] = [
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "WINE_DLL_FILE_MAP",
    "XODUS_LOCAL_GAMESAVE",
    "XODUS_LOCAL_GAMESAVE_ROOT",
];

/// The broker's delays and timestamps, which the tests replace.
trait Clock: Sync {
    fn sleep(&self, seconds: u64);
    fn now(&self) -> u64;
}
struct System;
impl Clock for System {
    fn sleep(&self, seconds: u64) {
        std::thread::sleep(Duration::from_secs(seconds));
    }
    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }
}

/// The synthetic broker's answer to one request kind, after the delay the
/// scenario calls for. `None` is a request the tests never send.
fn respond(kind: u16, request: &[u8], scenario: &str, clock: &dyn Clock) -> Option<Vec<u8>> {
    let text = match kind {
        1 => return Some(request.to_vec()),
        5 => {
            if matches!(scenario, "account-refresh" | "account-timeout") {
                clock.sleep(if scenario == "account-timeout" { 66 } else { 6 });
            }
            format!(
                "<StoreAccountContextResponse><Status>Available</Status><Context>{}</Context></StoreAccountContextResponse>",
                "a".repeat(64)
            )
        }
        3 => {
            clock.sleep(6);
            "<InvalidSyntheticMsaResponse/>".into()
        }
        15 => {
            clock.sleep(6);
            if scenario == "inventory-timeout" {
                "<StoreCollectionsResponse><Status>Timeout</Status></StoreCollectionsResponse>"
                    .into()
            } else {
                let now = clock.now();
                format!(
                    "<StoreCollectionsResponse><Status>Available</Status><ContextBound>true</ContextBound>\
                     <Complete>true</Complete><DirectCoverage>true</DirectCoverage><SatisfyingCoverage>true</SatisfyingCoverage>\
                     <SharedCoverage>false</SharedCoverage><ParentControlActive>true</ParentControlActive>\
                     <ObservedAt>{now}</ObservedAt><ExpiresAt>{}</ExpiresAt>\
                     <Items/><AbsentIds/><UnknownIds/><Continuation/></StoreCollectionsResponse>",
                    now + 30
                )
            }
        }
        17 => {
            clock.sleep(6);
            "<StorePackageUpdatesResponse><Status>Current</Status></StorePackageUpdatesResponse>"
                .into()
        }
        7 => format!(
            "<StoreGameLicenseResponse><Status>Available</Status><SkuStoreId>GAME1234EFGH/0001</SkuStoreId>\
             <IsActive>true</IsActive><IsTrialOwnedByThisUser>false</IsTrialOwnedByThisUser>\
             <IsDiscLicense>false</IsDiscLicense><IsTrial>false</IsTrial><TrialTimeRemainingInSeconds>0</TrialTimeRemainingInSeconds>\
             <TrialUniqueId/><ExpirationDate>{}</ExpirationDate><Revision>{}</Revision></StoreGameLicenseResponse>",
            clock.now() + 60,
            "b".repeat(64)
        ),
        _ => return None,
    };
    Some(text.into_bytes())
}

/// Serves the one connection of a scenario: frames of magic, kind and length
/// in little-endian order, each answered with the kind after it. Returns what
/// went wrong, if anything did; the client closing the connection is how
/// every scenario ends.
fn serve(
    listener: &UnixListener,
    scenario: &str,
    seen: &Mutex<Vec<u16>>,
    clock: &dyn Clock,
    patience: Duration,
    over: &AtomicBool,
) -> Option<String> {
    let timed_out = || Some("TimeoutError: timed out".to_string());
    if let Err(error) = listener.set_nonblocking(true) {
        return Some(format!("OSError: {error}"));
    }
    let deadline = Instant::now() + patience;
    let mut connection = loop {
        match listener.accept() {
            Ok((connection, _)) => break connection,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                if over.load(Ordering::Acquire) {
                    return None;
                }
                if Instant::now() >= deadline {
                    return timed_out();
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Some(format!("OSError: {error}")),
        }
    };
    let exchange = |connection: &mut UnixStream| -> std::io::Result<Option<String>> {
        connection.set_nonblocking(false)?;
        connection.set_read_timeout(Some(Duration::from_secs(80)))?;
        connection.set_write_timeout(Some(Duration::from_secs(80)))?;
        loop {
            let mut header = [0; 8];
            connection.read_exact(&mut header)?;
            let magic = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
            let kind = u16::from_le_bytes([header[4], header[5]]);
            let length = u16::from_le_bytes([header[6], header[7]]);
            if magic != MAGIC {
                return Ok(Some("AssertionError: ".into()));
            }
            let mut request = vec![0; length.into()];
            connection.read_exact(&mut request)?;
            locked(seen).push(kind);
            let Some(response) = respond(kind, &request, scenario, clock) else {
                return Ok(Some(format!(
                    "AssertionError: Unexpected synthetic broker request: {kind}"
                )));
            };
            let Ok(length) = u16::try_from(response.len()) else {
                return Ok(Some("error: the response exceeds a frame".into()));
            };
            let mut frame = MAGIC.to_le_bytes().to_vec();
            frame.extend((kind + 1).to_le_bytes());
            frame.extend(length.to_le_bytes());
            frame.extend(response);
            connection.write_all(&frame)?;
        }
    };
    match exchange(&mut connection) {
        Ok(error) => error,
        Err(error) => match error.kind() {
            ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => None,
            ErrorKind::WouldBlock | ErrorKind::TimedOut => timed_out(),
            _ => Some(format!("OSError: {error}")),
        },
    }
}

fn compile(root: &Path, stage: &Path, binary: &Path) -> Command {
    let runtime = stage.join("runtime");
    let mut command = Command::new("x86_64-w64-mingw32-g++");
    command
        .args(["-std=c++17", "-O2", "-static", "-I"])
        .arg(runtime.join("include"))
        .arg("-I")
        .arg(runtime.join("src"))
        .arg("-I")
        .arg(stage.join("wine-src/dlls/xgameruntime/GDKComponent/Xodus"));
    for name in [
        "XAsync.cpp",
        "XTaskQueue.cpp",
        "ThreadPool.cpp",
        "WaitTimer.cpp",
        "XThreading.cpp",
    ] {
        command.arg(runtime.join("src").join(name));
    }
    command
        .arg(root.join("tests/compat/ipc-timeout-test.cpp"))
        .arg("-o")
        .arg(binary)
        .args(["-lbcrypt", "-lole32", "-luuid"]);
    command
}

fn environment(stage: &Path, work: &Path) -> Environment {
    Environment {
        removed: &REMOVED,
        set: vec![
            ("WINEPREFIX", work.join("prefix").into()),
            ("WINEARCH", "win64".into()),
            ("WINEDEBUG", "-all".into()),
            ("WINEESYNC", "0".into()),
            ("WINEFSYNC", "0".into()),
            (
                "WINEDLLOVERRIDES",
                "winemenubuilder.exe,mscoree,mshtml=d;xodus_store_test=b".into(),
            ),
            ("WINEDLLPATH", stage.join("artifacts/builtin").into()),
            ("XODUS_STORE_MARKET", "AT".into()),
            ("XODUS_USER_SOCKET_SUFFIX", "broker.sock".into()),
            (
                "XODUS_STORE_PACKAGE_SCOPE",
                "FlightdeckBaseGameOnlyV1".into(),
            ),
        ],
    }
}

/// Copies a file into a directory with its mode and timestamps.
fn install(source: &Path, directory: &Path) -> Result<()> {
    let name = source
        .file_name()
        .ok_or_else(|| Failure(format!("{} has no file name", source.display())))?;
    let target = directory.join(name);
    fs::copy(source, &target).map_err(|error| Failure(format!("{}: {error}", target.display())))?;
    let info = fs::metadata(source)?;
    let times = fs::FileTimes::new()
        .set_accessed(info.accessed()?)
        .set_modified(info.modified()?);
    fs::File::open(&target)?.set_times(times)?;
    Ok(())
}

/// A scenario passed if the test exited cleanly with its summary and the
/// broker finished its connection without an error.
fn passed(code: i64, output: &str, errors: &[String], serving: bool) -> bool {
    code == 0 && output.contains("SUMMARY failures=0") && errors.is_empty() && !serving
}

fn scenario(prefix: &Prefix, binary: &Path, work: &Path, scenario: &str) -> Result<Value> {
    // sockaddr_un is shorter than most full build-directory paths.
    let directory = tempfile::Builder::new()
        .prefix("flightdeck-ipc-")
        .tempdir()?;
    let listener = UnixListener::bind(directory.path().join("broker.sock"))?;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let over = Arc::new(AtomicBool::new(false));
    let broker = {
        let (name, seen, over) = (scenario.to_string(), seen.clone(), over.clone());
        std::thread::spawn(move || {
            serve(
                &listener,
                &name,
                &seen,
                &System,
                Duration::from_secs(30),
                &over,
            )
        })
    };
    let test = prefix.run(
        &[binary.as_os_str(), OsStr::new(scenario)],
        Some(directory.path()),
        90,
    );
    let deadline = Instant::now() + Duration::from_secs(if test.is_ok() { 10 } else { 0 });
    while !broker.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let serving = !broker.is_finished();
    // A broker nobody connected to stops waiting; one in a delay ends by itself.
    over.store(true, Ordering::Release);
    let test = test?;
    let errors: Vec<String> = if serving {
        Vec::new()
    } else {
        broker
            .join()
            .unwrap_or_else(|_| Some("The synthetic broker failed".into()))
            .into_iter()
            .collect()
    };
    let output = String::from_utf8_lossy(&test.stdout);
    fs::write(
        work.join(format!("{scenario}.log")),
        [test.stdout.as_slice(), test.stderr.as_slice()].concat(),
    )?;
    let code = exit_code(test.status);
    Ok(
        json!({"passed": passed(code, &output, &errors, serving), "exit_code": code,
        "requests": *locked(&seen), "checks": split_lines(&output), "errors": errors}),
    )
}

pub fn run(root: &Path, stage: &Path, wine: &Path, cases: &[String]) -> Result<u8> {
    let stage = common::stage(stage)?;
    let wine =
        fs::canonicalize(wine).map_err(|error| Failure(format!("{}: {error}", wine.display())))?;
    let work = tempfile::Builder::new()
        .prefix("ipc-timeout-tests-")
        .tempdir_in(&stage)?
        .keep();
    let binary = work.join("ipc-timeout-test.exe");
    checked(&mut compile(root, &stage, &binary))?;
    let builtin = stage.join(BUILTIN);
    let hashes = (digest(&fs::read(&binary)?), digest(&fs::read(&builtin)?));
    // One isolated prefix; each scenario gets a fresh process and broker socket.
    let environment = environment(&stage, &work);
    let mut prefix = Prefix::new(&wine, &environment);
    let mut results = Map::new();
    let outcome = (|| -> Result<()> {
        prefix.boot()?;
        install(&builtin, &work.join("prefix/drive_c/windows/system32"))?;
        let all = CASES.map(String::from);
        for name in if cases.is_empty() { &all[..] } else { cases } {
            let case = scenario(&prefix, &binary, &work, name)?;
            println!(
                "{name} {}",
                if case["passed"] == true {
                    "PASS"
                } else {
                    "FAIL"
                }
            );
            results.insert(name.clone(), case);
        }
        Ok(())
    })();
    let stopped = prefix.stop();
    outcome?;
    stopped?;
    let passed = results.values().all(|case| case["passed"] == true);
    let report = json!({"synthetic_only": true, "account_calls": false, "cases": results,
        "binary_sha256": hashes.0, "builtin_sha256": hashes.1, "passed": passed});
    let path = work.join("results.json");
    fs::write(&path, pretty(&report))?;
    println!(
        "{}",
        json!({"passed": passed, "report": path.display().to_string()})
    );
    Ok(u8::from(!passed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No delay passes; the requested ones are recorded.
    struct Fixed(Mutex<Vec<u64>>);
    impl Fixed {
        fn new() -> Self {
            Self(Mutex::new(Vec::new()))
        }
        fn slept(&self) -> Vec<u64> {
            std::mem::take(&mut *locked(&self.0))
        }
    }
    impl Clock for Fixed {
        fn sleep(&self, seconds: u64) {
            locked(&self.0).push(seconds);
        }
        fn now(&self) -> u64 {
            1_800_000_000
        }
    }

    fn frame(magic: u32, kind: u16, body: &[u8]) -> Vec<u8> {
        let mut frame = magic.to_le_bytes().to_vec();
        frame.extend(kind.to_le_bytes());
        frame.extend((body.len() as u16).to_le_bytes());
        frame.extend(body);
        frame
    }

    fn reply(client: &mut UnixStream) -> (u16, String) {
        let mut header = [0; 8];
        client.read_exact(&mut header).expect("header");
        assert_eq!(header[..4], MAGIC.to_le_bytes());
        let mut body = vec![0; u16::from_le_bytes([header[6], header[7]]).into()];
        client.read_exact(&mut body).expect("body");
        (
            u16::from_le_bytes([header[4], header[5]]),
            String::from_utf8(body).expect("text"),
        )
    }

    #[test]
    fn broker_answers_follow_the_scenario_and_its_delays() {
        let clock = Fixed::new();
        let text = |kind: u16, scenario: &str| {
            String::from_utf8(respond(kind, b"request", scenario, &clock).expect("answer"))
                .expect("text")
        };
        assert_eq!(text(1, "auth"), "request");
        assert!(clock.slept().is_empty());
        let context = format!(
            "<StoreAccountContextResponse><Status>Available</Status><Context>{}</Context></StoreAccountContextResponse>",
            "a".repeat(64)
        );
        assert_eq!(text(5, "inventory-ok"), context);
        assert!(clock.slept().is_empty());
        assert_eq!(text(5, "account-refresh"), context);
        assert_eq!(clock.slept(), [6]);
        assert_eq!(text(5, "account-timeout"), context);
        assert_eq!(clock.slept(), [66]);
        assert_eq!(text(3, "auth"), "<InvalidSyntheticMsaResponse/>");
        assert_eq!(clock.slept(), [6]);
        assert_eq!(
            text(15, "inventory-timeout"),
            "<StoreCollectionsResponse><Status>Timeout</Status></StoreCollectionsResponse>"
        );
        assert_eq!(
            text(15, "inventory-ok"),
            "<StoreCollectionsResponse><Status>Available</Status><ContextBound>true</ContextBound>\
             <Complete>true</Complete><DirectCoverage>true</DirectCoverage><SatisfyingCoverage>true</SatisfyingCoverage>\
             <SharedCoverage>false</SharedCoverage><ParentControlActive>true</ParentControlActive>\
             <ObservedAt>1800000000</ObservedAt><ExpiresAt>1800000030</ExpiresAt>\
             <Items/><AbsentIds/><UnknownIds/><Continuation/></StoreCollectionsResponse>"
        );
        assert_eq!(clock.slept(), [6, 6]);
        assert_eq!(
            text(17, "updates"),
            "<StorePackageUpdatesResponse><Status>Current</Status></StorePackageUpdatesResponse>"
        );
        assert_eq!(clock.slept(), [6]);
        assert_eq!(
            text(7, "inventory-ok"),
            format!(
                "<StoreGameLicenseResponse><Status>Available</Status><SkuStoreId>GAME1234EFGH/0001</SkuStoreId>\
                 <IsActive>true</IsActive><IsTrialOwnedByThisUser>false</IsTrialOwnedByThisUser>\
                 <IsDiscLicense>false</IsDiscLicense><IsTrial>false</IsTrial><TrialTimeRemainingInSeconds>0</TrialTimeRemainingInSeconds>\
                 <TrialUniqueId/><ExpirationDate>1800000060</ExpirationDate><Revision>{}</Revision></StoreGameLicenseResponse>",
                "b".repeat(64)
            )
        );
        assert!(clock.slept().is_empty());
        for kind in [0, 2, 4, 6, 8, 16, 18, u16::MAX] {
            assert!(respond(kind, b"", "auth", &clock).is_none(), "{kind}");
        }
    }

    /// Runs the broker against a client and returns its error and requests.
    fn served(client: impl FnOnce(&mut UnixStream)) -> (Option<String>, Vec<u16>) {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("broker.sock");
        let listener = UnixListener::bind(&path).expect("listener");
        let seen = Mutex::new(Vec::new());
        let (clock, over) = (Fixed::new(), AtomicBool::new(false));
        let error = std::thread::scope(|scope| {
            let broker = scope.spawn(|| {
                serve(
                    &listener,
                    "inventory-ok",
                    &seen,
                    &clock,
                    Duration::from_secs(30),
                    &over,
                )
            });
            let mut stream = UnixStream::connect(&path).expect("client");
            client(&mut stream);
            drop(stream);
            broker.join().expect("broker")
        });
        let requests = locked(&seen).clone();
        (error, requests)
    }

    #[test]
    fn broker_frames_each_answer_and_ends_when_the_client_leaves() {
        let (error, requests) = served(|client| {
            client
                .write_all(&frame(MAGIC, 1, b"echo me"))
                .expect("request");
            assert_eq!(reply(client), (2, "echo me".into()));
            // Two requests in one write are still two frames.
            let mut both = frame(MAGIC, 15, b"");
            both.extend(frame(MAGIC, 7, b"GAME1234EFGH"));
            client.write_all(&both).expect("requests");
            let (kind, inventory) = reply(client);
            assert_eq!(kind, 16);
            assert!(inventory.starts_with("<StoreCollectionsResponse><Status>Available</Status>"));
            let (kind, license) = reply(client);
            assert_eq!(kind, 8);
            assert!(license.contains("<ExpirationDate>1800000060</ExpirationDate>"));
        });
        assert_eq!(error, None);
        assert_eq!(requests, [1, 15, 7]);
        // A connection that ends inside a frame is the client leaving too.
        let (error, requests) = served(|client| {
            client
                .write_all(&frame(MAGIC, 1, b"whole")[..10])
                .expect("part");
        });
        assert_eq!((error, requests), (None, vec![]));
    }

    #[test]
    fn broker_reports_foreign_frames_unknown_requests_and_missing_clients() {
        let (error, requests) = served(|client| {
            client
                .write_all(&frame(0x1234_5678, 1, b""))
                .expect("request");
            assert_eq!(client.read(&mut [0; 8]).expect("closed"), 0);
        });
        assert_eq!(
            (error.as_deref(), requests),
            (Some("AssertionError: "), vec![])
        );
        let (error, requests) = served(|client| {
            client.write_all(&frame(MAGIC, 9, b"x")).expect("request");
            assert_eq!(client.read(&mut [0; 8]).expect("closed"), 0);
        });
        assert_eq!(
            (error.as_deref(), requests),
            (
                Some("AssertionError: Unexpected synthetic broker request: 9"),
                vec![9]
            )
        );
        let temp = tempfile::tempdir().expect("temp");
        let listener = UnixListener::bind(temp.path().join("broker.sock")).expect("listener");
        let (seen, clock) = (Mutex::new(Vec::new()), Fixed::new());
        let wait = |patience: u64, over: bool| {
            serve(
                &listener,
                "auth",
                &seen,
                &clock,
                Duration::from_millis(patience),
                &AtomicBool::new(over),
            )
        };
        assert_eq!(wait(50, false).as_deref(), Some("TimeoutError: timed out"));
        // Once the scenario is over, nobody is waited for and nothing reported.
        let started = Instant::now();
        assert_eq!(wait(30_000, true), None);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn test_binary_is_compiled_against_the_staged_runtime_and_wine_sources() {
        let command = compile(
            Path::new("/synthetic/checkout"),
            Path::new("/synthetic/stage"),
            Path::new("/synthetic/work/ipc-timeout-test.exe"),
        );
        assert_eq!(command.get_program(), "x86_64-w64-mingw32-g++");
        let words: Vec<_> = command
            .get_args()
            .map(|word| word.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            words,
            [
                "-std=c++17",
                "-O2",
                "-static",
                "-I",
                "/synthetic/stage/runtime/include",
                "-I",
                "/synthetic/stage/runtime/src",
                "-I",
                "/synthetic/stage/wine-src/dlls/xgameruntime/GDKComponent/Xodus",
                "/synthetic/stage/runtime/src/XAsync.cpp",
                "/synthetic/stage/runtime/src/XTaskQueue.cpp",
                "/synthetic/stage/runtime/src/ThreadPool.cpp",
                "/synthetic/stage/runtime/src/WaitTimer.cpp",
                "/synthetic/stage/runtime/src/XThreading.cpp",
                "/synthetic/checkout/tests/compat/ipc-timeout-test.cpp",
                "-o",
                "/synthetic/work/ipc-timeout-test.exe",
                "-lbcrypt",
                "-lole32",
                "-luuid"
            ]
        );
        assert!(
            crate::util::root()
                .join("tests/compat/ipc-timeout-test.cpp")
                .is_file()
        );
    }

    #[test]
    fn prefix_environment_loads_the_test_builtin_and_names_the_broker_socket() {
        let environment = environment(Path::new("/synthetic/stage"), Path::new("/synthetic/work"));
        assert_eq!(environment.removed, REMOVED);
        let set: Vec<_> = environment
            .set
            .iter()
            .map(|(name, value)| (*name, value.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(
            set,
            [
                ("WINEPREFIX", "/synthetic/work/prefix".to_string()),
                ("WINEARCH", "win64".into()),
                ("WINEDEBUG", "-all".into()),
                ("WINEESYNC", "0".into()),
                ("WINEFSYNC", "0".into()),
                (
                    "WINEDLLOVERRIDES",
                    "winemenubuilder.exe,mscoree,mshtml=d;xodus_store_test=b".into()
                ),
                ("WINEDLLPATH", "/synthetic/stage/artifacts/builtin".into()),
                ("XODUS_STORE_MARKET", "AT".into()),
                ("XODUS_USER_SOCKET_SUFFIX", "broker.sock".into()),
                (
                    "XODUS_STORE_PACKAGE_SCOPE",
                    "FlightdeckBaseGameOnlyV1".into()
                ),
            ]
        );
    }

    #[test]
    fn builtin_is_installed_with_its_timestamp_and_scenarios_are_judged() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("xodus_store_test.dll");
        fs::write(&source, b"synthetic builtin").expect("source");
        let old = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        fs::File::open(&source)
            .expect("source")
            .set_modified(old)
            .expect("time");
        let directory = temp.path().join("system32");
        // Without a booted prefix there is nowhere to install to.
        assert!(install(&source, &directory).is_err());
        fs::create_dir(&directory).expect("directory");
        install(&source, &directory).expect("install");
        let installed = directory.join("xodus_store_test.dll");
        assert_eq!(fs::read(&installed).expect("copy"), b"synthetic builtin");
        assert_eq!(
            fs::metadata(&installed)
                .expect("copy")
                .modified()
                .expect("time"),
            old
        );
        let summary = "PASS late reply rejected\r\nSUMMARY failures=0\r\n";
        assert!(passed(0, summary, &[], false));
        assert!(!passed(1, summary, &[], false));
        assert!(!passed(0, "SUMMARY failures=2", &[], false));
        assert!(!passed(
            0,
            summary,
            &["TimeoutError: timed out".into()],
            false
        ));
        assert!(!passed(0, summary, &[], true));
    }
}
