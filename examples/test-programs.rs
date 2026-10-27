// SPDX-License-Identifier: MIT
//! Disposable peers for the integration tests: programs the launcher starts,
//! supervises or finds running, which answer or misbehave in one known way.
//! None is part of the product. A copy named like the program it stands in for
//! (`xodus-service`, `xodus-cli`, `fake-wine`) acts as that program; under any
//! other name the first argument selects the peer. A failed expectation ends
//! the peer with exit code 1 and a message, like a failed assertion.
use std::{
    env, fs,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::Duration,
};
use tokio::signal::unix::{SignalKind, signal};

type Outcome = Result<u8, String>;

fn check(holds: bool, message: &str) -> Result<(), String> {
    if holds { Ok(()) } else { Err(message.into()) }
}

fn failed<E: std::fmt::Display>(what: &'static str) -> impl Fn(E) -> String {
    move |error| format!("{what}: {error}")
}

fn variable(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("{name} is not set"))
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(failed("runtime"))
}

/// The play lease belongs to the supervisor alone: no inherited descriptor
/// may refer to it, and it must still be held.
fn lease(private: &Path) -> Result<(), String> {
    let lock = private.join("play.lock");
    let expected = fs::metadata(&lock).map_err(failed("play.lock"))?;
    for entry in fs::read_dir("/proc/self/fd").map_err(failed("descriptors"))? {
        let Ok(info) = entry.map_err(failed("descriptor"))?.path().metadata() else {
            continue;
        };
        check(
            (info.dev(), info.ino()) != (expected.dev(), expected.ino()),
            "lease leaked into child",
        )?;
    }
    let probe = fs::File::open(&lock).map_err(failed("play.lock"))?;
    match rustix::fs::flock(&probe, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(()),
        Ok(()) => Err("supervisor released the lease".into()),
        Err(error) => Err(format!("play.lock: {error}")),
    }
}

/// The Xodus service: answers every request on its socket with status 2 and
/// leaves a mark when it was stopped by a signal instead of being killed.
fn xodus_service(private: &Path) -> Outcome {
    let path = PathBuf::from(variable("XDG_RUNTIME_DIR")?).join("xodus.sock");
    let served: Result<(), String> = runtime()?.block_on(async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut interrupt = signal(SignalKind::interrupt()).map_err(failed("signal"))?;
        let mut terminate = signal(SignalKind::terminate()).map_err(failed("signal"))?;
        let listener = tokio::net::UnixListener::bind(&path).map_err(failed("socket"))?;
        loop {
            let (mut connection, _) = tokio::select! {
                _ = interrupt.recv() => return Ok(()),
                _ = terminate.recv() => return Ok(()),
                accepted = listener.accept() => accepted.map_err(failed("accept"))?,
            };
            let mut data = [0; 1024];
            let read = tokio::time::timeout(Duration::from_secs(1), connection.read(&mut data));
            let length = read
                .await
                .map_err(failed("request"))?
                .map_err(failed("request"))?;
            let data = &data[..length];
            let mut answer = data[..length.min(4)].to_vec();
            answer.extend(2u16.to_le_bytes());
            answer.extend(data.get(6..).unwrap_or_default());
            connection
                .write_all(&answer)
                .await
                .map_err(failed("answer"))?;
        }
    });
    let _ = fs::remove_file(&path);
    fs::write(private.join("service-stopped"), "yes").map_err(failed("mark"))?;
    served.map(|()| 0)
}

/// The game started through the Xodus command line, in the game directory and
/// with the configured market. It ends with exit code 7 or, when the test
/// wants to stop it, runs until it is killed.
fn xodus_cli(root: &Path, arguments: &[String]) -> Outcome {
    check(arguments.first().is_some_and(|word| word == "run"), "run")?;
    let directory = env::current_dir().map_err(failed("directory"))?;
    check(directory == root.join("games/MSFS2024"), "game directory")?;
    check(variable("XODUS_STORE_MARKET")? == "AT", "market")?;
    check(
        Path::new(&variable("FLIGHTDECK_HELPER_RUNTIME")?) == root,
        "helper runtime",
    )?;
    let launcher = arguments.get(2).map(Path::new).and_then(Path::file_name);
    check(
        launcher.is_some_and(|name| name == "xodus-wine-launch"),
        "launcher",
    )?;
    fs::write(root.join("private/game-started"), "yes").map_err(failed("mark"))?;
    if env::var("FLIGHTDECK_TEST_WAIT").is_ok_and(|value| value == "1") {
        loop {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    Ok(7)
}

fn xodus(name: &str, arguments: &[String]) -> Outcome {
    let root = PathBuf::from(variable("MSFS_LINUX_ROOT")?);
    lease(&root.join("private"))?;
    if name == "xodus-service" {
        xodus_service(&root.join("private"))
    } else {
        xodus_cli(&root, arguments)
    }
}

/// Wine as the loader starts it: the image is in place, mapped from memory in
/// the native mode and linked in the portable one, and the game's own files
/// are readable next to it. Ends with exit code 7.
fn wine(arguments: &[String]) -> Outcome {
    let image = Path::new(arguments.first().ok_or("no image")?);
    check(image.exists(), "image exists")?;
    let expected: Vec<String> = serde_json::from_str(&variable("FLIGHTDECK_EXPECT_ARGUMENTS")?)
        .map_err(failed("expected"))?;
    check(
        arguments[1..] == expected,
        &format!("arguments {arguments:?}"),
    )?;
    check(
        variable("XODUS_STORE_PACKAGE_SCOPE")? == "FlightdeckBaseGameOnlyV1",
        "package scope",
    )?;
    let folder = image.parent().ok_or("no folder")?;
    let directory = env::current_dir().map_err(failed("directory"))?;
    check(directory == folder, "working directory")?;
    let read =
        |path: PathBuf| fs::read(&path).map_err(|error| format!("{}: {error}", path.display()));
    check(
        read(directory.join("nested/probe.dll"))? == b"memory-backed DLL",
        "library from memory",
    )?;
    check(
        read(folder.join("asset.dat"))? == b"ordinary resource",
        "ordinary resource",
    )?;
    let linked = image
        .symlink_metadata()
        .map_err(failed("image"))?
        .is_symlink();
    let content = read(image.to_path_buf())?;
    if env::var("FLIGHTDECK_PROTON_LOADER").is_ok_and(|mode| mode == "portable") {
        check(env::var_os("WINE_DLL_FILE_MAP").is_none(), "no file map")?;
        check(linked, "image is a link")?;
    } else {
        let map = variable("WINE_DLL_FILE_MAP")?;
        let entries: Vec<&str> = map.split('|').collect();
        check(entries.len() == 4, "four mapped files")?;
        check(
            !linked && content.get(256..) == Some(&[0; 256][..]),
            "placeholder image",
        )?;
        let descriptor = entries[0].split(':').next().unwrap_or_default();
        check(descriptor.parse::<u32>().is_ok(), "descriptor number")?;
        let mut start = [0; 2];
        fs::File::open(format!("/proc/self/fd/{descriptor}"))
            .and_then(|mut mapped| mapped.read_exact(&mut start))
            .map_err(failed("mapped image"))?;
        check(&start == b"MZ", "mapped image header")?;
    }
    check(content.starts_with(b"MZ"), "image header")?;
    Ok(7)
}

/// A cloud helper on its pipe: length-prefixed JSON requests on standard
/// input, answered byte by byte, or deliberately wrong as `mode` says.
fn pipe(mode: &str) -> Outcome {
    let (mut input, mut output) = (std::io::stdin().lock(), std::io::stdout().lock());
    for _ in 0..3 {
        let mut header = [0; 4];
        if input.read_exact(&mut header).is_err() {
            break;
        }
        let mut request = vec![0; u32::from_le_bytes(header) as usize];
        input.read_exact(&mut request).map_err(failed("request"))?;
        serde_json::from_slice::<serde_json::Value>(&request).map_err(failed("request"))?;
        match mode {
            "timeout" => {
                std::thread::sleep(Duration::from_millis(300));
                break;
            }
            "oversize" => {
                output
                    .write_all(&300_000u32.to_le_bytes())
                    .and_then(|()| output.flush())
                    .map_err(failed("answer"))?;
                break;
            }
            _ => {}
        }
        let raw: &[u8] = if mode == "duplicate" {
            br#"{"ok":true,"ok":true}"#
        } else {
            br#"{"ok": true, "body_bytes": 3}"#
        };
        let mut data = (raw.len() as u32).to_le_bytes().to_vec();
        data.extend(raw);
        data.extend(b"abc");
        for byte in data {
            output
                .write_all(&[byte])
                .and_then(|()| output.flush())
                .map_err(failed("answer"))?;
            if mode == "fragmented" {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    Ok(0)
}

/// Stays for `seconds` after marking `ready`, whatever INT and TERM say.
fn unstoppable(ready: &Path, seconds: u64) -> Outcome {
    runtime()?.block_on(async {
        let _interrupt = signal(SignalKind::interrupt()).map_err(failed("signal"))?;
        let _terminate = signal(SignalKind::terminate()).map_err(failed("signal"))?;
        fs::write(ready, "ready").map_err(failed("ready"))?;
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        Ok(0)
    })
}

/// A process whose environment and files other processes of the same user
/// cannot inspect, as a session service that protects itself.
fn undumpable(ready: &Path) -> Outcome {
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        .map_err(failed("prctl"))?;
    fs::write(ready, "").map_err(failed("ready"))?;
    std::thread::sleep(Duration::from_secs(30));
    Ok(0)
}

/// A leader that exits once its descendant runs. The descendant marks in the
/// working directory that it was asked to stop instead of being killed.
fn leader() -> Outcome {
    let program = env::current_exe().map_err(failed("program"))?;
    Command::new(program)
        .arg("descendant")
        .spawn()
        .map_err(failed("descendant"))?;
    while !Path::new("private/ready").exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(0)
}

fn descendant() -> Outcome {
    runtime()?.block_on(async {
        let mut terminate = signal(SignalKind::terminate()).map_err(failed("signal"))?;
        fs::write("private/ready", "yes").map_err(failed("ready"))?;
        terminate.recv().await;
        fs::write("private/stopped", "yes").map_err(failed("stopped"))?;
        Ok(0)
    })
}

fn run(name: &str, arguments: &[String]) -> Outcome {
    let word = |index: usize| {
        arguments
            .get(index)
            .map(String::as_str)
            .ok_or("argument missing")
    };
    match name {
        "xodus-service" | "xodus-cli" => xodus(name, arguments),
        "fake-wine" => wine(arguments),
        _ => match word(0)? {
            "pipe" => pipe(word(1)?),
            "unstoppable" => unstoppable(Path::new(word(1)?), 60),
            "undumpable" => undumpable(Path::new(word(1)?)),
            "leader" => leader(),
            "descendant" => descendant(),
            other => Err(format!("unknown test program {other}")),
        },
    }
}

fn main() -> ExitCode {
    let mut arguments = env::args();
    let name = arguments
        .next()
        .and_then(|path| Some(Path::new(&path).file_name()?.to_str()?.to_string()))
        .unwrap_or_default();
    match run(&name, &arguments.collect::<Vec<_>>()) {
        Ok(code) => code.into(),
        Err(message) => {
            eprintln!("{name}: {message}");
            1.into()
        }
    }
}
