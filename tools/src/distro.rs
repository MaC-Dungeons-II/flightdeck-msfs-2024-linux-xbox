// SPDX-License-Identifier: MIT
//! A packaged launcher on a pinned distribution, without host desktop access.
//! Container tests establish installation, HTTP and software desktop startup
//! only; not GPU, Microsoft sign-in, Wine or simulator compatibility.
use crate::{
    contract, ensure, http,
    util::{Failure, Result, digest, fail, pretty, read_regular, require},
};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, FileTypeExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::LazyLock,
    time::{Duration, Instant},
};

pub const DISTRIBUTIONS: [&str; 5] = ["ubuntu", "debian", "fedora", "arch", "opensuse"];

/// Whether the titled toplevel has committed a real buffer. Decorations use
/// separate surfaces and can paint before the content, so the toplevel's role
/// chain is followed to its actual main surface.
pub fn wayland_painted(text: &str) -> bool {
    static SURFACE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\.get_xdg_surface\(new id xdg_surface[@#](\d+), wl_surface[@#](\d+)\)")
            .expect("pattern")
    });
    static TOPLEVEL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"xdg_surface[@#](\d+)\.get_toplevel\(new id xdg_toplevel[@#](\d+)\)")
            .expect("pattern")
    });
    static TITLE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"xdg_toplevel[@#](\d+)\.set_title\("([^"\n]*)"\)"#).expect("pattern")
    });
    static CONFIGURE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"xdg_surface[@#](\d+)\.ack_configure\(").expect("pattern"));
    static ATTACH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"wl_surface[@#](\d+)\.attach\(([^,]*)").expect("pattern"));
    static BUFFER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^wl_buffer[@#]\d+$").expect("pattern"));
    static COMMIT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"wl_surface[@#](\d+)\.commit\(").expect("pattern"));
    let (mut surfaces, mut toplevels, mut titles) =
        (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    let (mut configured, mut buffered) = (BTreeSet::new(), BTreeSet::new());
    for line in text.lines() {
        if let Some(event) = SURFACE.captures(line) {
            surfaces.insert(event[1].to_string(), event[2].to_string());
            configured.remove(&event[1]);
            buffered.remove(&event[2]);
        } else if let Some(event) = TOPLEVEL.captures(line) {
            toplevels.insert(event[2].to_string(), event[1].to_string());
            titles.remove(&event[2]);
        } else if let Some(event) = TITLE.captures(line) {
            titles.insert(event[1].to_string(), event[2].to_string());
        } else if let Some(event) = CONFIGURE.captures(line) {
            configured.insert(event[1].to_string());
        } else if let Some(event) = ATTACH.captures(line) {
            if BUFFER.is_match(&event[2]) {
                buffered.insert(event[1].to_string());
            } else {
                buffered.remove(&event[1]);
            }
        } else if let Some(event) = COMMIT.captures(line)
            && buffered.contains(&event[1])
            && titles.iter().any(|(top, title)| {
                title == "Flightdeck"
                    && toplevels.get(top).is_some_and(|surface| {
                        configured.contains(surface)
                            && surfaces.get(surface).map(String::as_str) == Some(&event[1])
                    })
            })
        {
            return true;
        }
    }
    false
}

fn captured(command: &mut Command, seconds: u64) -> Result<String> {
    let label = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Failure(format!("{label}: {error}")))?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return fail(format!("{label} timed out"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let result = child.wait_with_output()?;
    require(
        result.status.success(),
        format!(
            "{label} failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        ),
    )?;
    Ok(String::from_utf8_lossy(&result.stdout).into_owned())
}

/// The processes of one window test. Dropping stops them, newest first, so no
/// failure between spawning and cleanup can leave a server or window behind.
pub struct Owned(pub Vec<Child>);
impl Owned {
    fn alive(&mut self) -> Result<()> {
        for child in &mut self.0 {
            require(
                child.try_wait()?.is_none(),
                "A desktop process exited before readiness",
            )?;
        }
        Ok(())
    }
    fn wait_for(&mut self, mut check: impl FnMut() -> Result<bool>, seconds: u64) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            self.alive()?;
            if check()? {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        fail("Desktop readiness timed out")
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        for child in self.0.iter_mut().rev() {
            if matches!(child.try_wait(), Ok(None))
                && let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32)
            {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Startup evidence without credentials: only these three fields leave the test.
pub fn evidence(record: &[u8]) -> Option<Value> {
    let record: Value = serde_json::from_slice(record).ok()?;
    record.is_object().then(
        || json!({"pid": record["pid"], "port": record["port"], "release": record["release"]}),
    )
}

fn window(binary: &Path, backend: &str, work: &Path, output: &Path) -> Result<()> {
    let runtime = work.join(format!("{backend}-runtime"));
    fs::DirBuilder::new().mode(0o700).create(&runtime)?;
    let environment = |command: &mut Command| {
        for name in [
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "WAYLAND_SOCKET",
            "WAYLAND_DEBUG",
        ] {
            command.env_remove(name);
        }
        command.env("XDG_RUNTIME_DIR", &runtime);
        if backend == "x11" {
            command.env("DISPLAY", ":99");
        } else {
            command.env("WAYLAND_DISPLAY", "flightdeck-test");
        }
    };
    let mut server = if backend == "x11" {
        let mut server = Command::new("Xvfb");
        server.args([
            ":99",
            "-screen",
            "0",
            "1280x900x24",
            "-nolisten",
            "tcp",
            "-noreset",
        ]);
        server
    } else {
        let mut help = Command::new("weston");
        help.arg("--help");
        environment(&mut help);
        let renderer = if captured(&mut help, 60)?.contains("--renderer") {
            "--renderer=pixman"
        } else {
            "--use-pixman"
        };
        let mut server = Command::new("weston");
        server.args([
            "--backend=headless-backend.so",
            renderer,
            "--socket=flightdeck-test",
            "--idle-time=0",
            "--no-config",
        ]);
        server
    };
    environment(&mut server);
    let state = work.join(format!("{backend}-state"));
    let server_log = fs::File::create(output.join(format!("{backend}-server.log")))?;
    let window_log = fs::File::create(output.join(format!("{backend}-window.log")))?;
    let mut owned = Owned(Vec::new());
    let result = (|| {
        owned.0.push(
            server
                .stdin(Stdio::null())
                .stdout(server_log.try_clone()?)
                .stderr(server_log)
                .spawn()
                .map_err(|error| Failure(format!("{backend} server: {error}")))?,
        );
        let socket = runtime.join("flightdeck-test");
        owned.wait_for(
            || {
                Ok(if backend == "x11" {
                    Path::new("/tmp/.X11-unix/X99").exists()
                } else {
                    fs::metadata(&socket).is_ok_and(|info| info.file_type().is_socket())
                })
            },
            75,
        )?;
        // --desktop creates only an isolated, unconfigured service. Outbound
        // networking is disabled by the container; there is no host display.
        let mut gui = Command::new(binary);
        gui.arg("--desktop")
            .arg("--state-dir")
            .arg(&state)
            .args(["--language", "en"]);
        environment(&mut gui);
        if backend == "wayland" {
            gui.env("WAYLAND_DEBUG", "client");
        }
        owned.0.push(
            gui.stdin(Stdio::null())
                .stdout(window_log.try_clone()?)
                .stderr(window_log)
                .spawn()
                .map_err(|error| Failure(format!("{}: {error}", binary.display())))?,
        );
        owned.wait_for(
            || {
                if backend == "x11" {
                    let mut tree = Command::new("xwininfo");
                    tree.args(["-root", "-tree"]);
                    environment(&mut tree);
                    let text = captured(&mut tree, 5)?;
                    fs::write(output.join("x11-windows.txt"), &text)?;
                    Ok(text.contains("\"Flightdeck\""))
                } else {
                    let log = fs::read(output.join("wayland-window.log"))?;
                    Ok(wayland_painted(&String::from_utf8_lossy(&log)))
                }
            },
            75,
        )?;
        let record: Value = serde_json::from_slice(&fs::read(state.join("desktop-service.json"))?)?;
        let port = record["port"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .unwrap_or(0);
        let token = record["token"].as_str().unwrap_or_default();
        let response = http::request(
            port,
            "GET",
            "/api/status",
            &[("X-Flightdeck-Token", token)],
            None,
            Duration::from_secs(10),
        )?;
        ensure!(response.status == 200 && response.json()["runtime"]["configured"] == false);
        Ok(())
    })();
    // Startup evidence is retained without credentials, even on failed startup.
    let record = fs::read(state.join("desktop-service.json"))
        .ok()
        .and_then(|raw| evidence(&raw));
    if let Some(record) = &record {
        fs::write(
            output.join(format!("{backend}-service.json")),
            pretty(record),
        )?;
    }
    drop(owned);
    // The service deliberately outlives the window. This PID belongs to this
    // test's private container and state, never to a host session.
    if let Some(pid) = record
        .and_then(|record| record["pid"].as_i64())
        .filter(|pid| *pid > 1)
        .and_then(|pid| rustix::process::Pid::from_raw(pid as i32))
    {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::INT);
    }
    result
}

/// The checks that run inside the container, as an unprivileged user.
pub fn inside(package: &Path, output: &Path) -> Result<()> {
    require(
        !rustix::process::geteuid().is_root(),
        "The launcher must be tested as an unprivileged user",
    )?;
    let binary = package.join("bin/flightdeck");
    let manifest: Value =
        serde_json::from_slice(&fs::read(package.join("FLIGHTDECK-PACKAGE.json"))?)?;
    let hash = digest(&read_regular(&binary, 512 * 1024 * 1024)?);
    ensure!(manifest["files"]["bin/flightdeck"] == hash);
    let expected = format!(
        "flightdeck {}",
        manifest["version"].as_str().unwrap_or_default()
    );
    ensure!(captured(Command::new(&binary).arg("--version"), 60)?.trim() == expected);
    let temporary = tempfile::Builder::new()
        .prefix("flightdeck-linux-")
        .tempdir()?;
    let work = temporary.path();
    let (root, commands) = (work.join("installed launcher ä"), work.join("commands"));
    captured(
        Command::new("bash")
            .arg(package.join("install.sh"))
            .arg("--data-dir")
            .arg(&root)
            .arg("--bin-dir")
            .arg(&commands)
            .arg("--applications-dir")
            .arg(work.join("applications"))
            .args(["--no-desktop", "--no-launch", "--language", "en"]),
        60,
    )?;
    let installed = commands.join("flightdeck");
    ensure!(captured(Command::new(&installed).arg("--version"), 60)?.trim() == expected);
    let checks = contract::run(&installed)?;
    fs::write(
        output.join("http.log"),
        format!(
            "Rust HTTP: {checks} service, origin, asset, configuration and backup checks passed.\n"
        ),
    )?;
    for backend in ["x11", "wayland"] {
        window(&installed, backend, work, output)?;
    }
    captured(
        Command::new(&installed)
            .args(["install", "--uninstall", "--data-dir"])
            .arg(&root)
            .args(["--no-launch", "--language", "en"]),
        60,
    )?;
    require(
        fs::symlink_metadata(&installed).is_err(),
        "Uninstall retained the managed command",
    )?;
    let report = json!({"status": "PASS", "version": manifest["version"], "binary_sha256": hash,
        "system": fs::read_to_string("/etc/os-release")?,
        "glibc": captured(Command::new("getconf").arg("GNU_LIBC_VERSION"), 60)?.trim(),
        "checks": ["native_version", "install_unicode_path", "installed_command", "http_contract",
            "x11_window", "wayland_buffer_commit", "uninstall"],
        "scope": "unprivileged, isolated container; no GPU, login, Wine or simulator tests"});
    fs::write(output.join("result.json"), pretty(&report))?;
    println!("{report}");
    Ok(())
}

/// Docker as this check uses it: a command with optional input, an optional
/// log for its output and a deadline.
pub trait Docker {
    fn run(
        &mut self,
        command: &[String],
        input: Option<&str>,
        log: Option<&Path>,
        seconds: u64,
    ) -> Result<String>;
}

pub struct Daemon;
impl Docker for Daemon {
    fn run(
        &mut self,
        command: &[String],
        input: Option<&str>,
        log: Option<&Path>,
        seconds: u64,
    ) -> Result<String> {
        let mut process = Command::new(&command[0]);
        process.args(&command[1..]);
        process.stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        match log {
            Some(path) => {
                let log = fs::File::create(path)?;
                process.stdout(log.try_clone()?).stderr(log);
            }
            None => {
                process.stdout(Stdio::piped()).stderr(Stdio::inherit());
            }
        }
        let mut child = process
            .spawn()
            .map_err(|error| Failure(format!("{}: {error}", command[0])))?;
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            stdin.write_all(input.as_bytes())?;
        }
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while child.try_wait()?.is_none() {
            if Instant::now() >= deadline {
                // Killing the client does not stop the daemon's container.
                let _ = child.kill();
                let _ = child.wait();
                return fail(format!("{} timed out", command[..2].join(" ")));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let result = child.wait_with_output()?;
        require(
            result.status.success(),
            format!("{} failed", command[..2].join(" ")),
        )?;
        Ok(String::from_utf8_lossy(&result.stdout).into_owned())
    }
}

pub struct Outside<'a> {
    pub images: &'a Path,
    pub distribution: &'a str,
    pub package: &'a Path,
    pub output: &'a Path,
    /// This tool, as it runs inside the container.
    pub tool: &'a Path,
    pub user: (u32, u32),
    pub container: &'a str,
}

/// Builds the pinned test image and runs the inside checks in it. No sockets,
/// host PID namespace, host networking, devices, credentials or writable
/// source mounts; the package and this tool are mounted read-only.
pub fn outside(docker: &mut dyn Docker, job: &Outside<'_>) -> Result<()> {
    static IMAGE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[a-z0-9./:_-]+@sha256:[0-9a-f]{64}$").expect("pattern"));
    let matrix: Value = serde_json::from_slice(&fs::read(job.images)?)?;
    let spec = &matrix[job.distribution];
    let (image, install) = (
        spec["image"].as_str().unwrap_or_default(),
        spec["install"].as_str().unwrap_or_default(),
    );
    require(
        IMAGE.is_match(image) && !install.is_empty(),
        "The test image must be pinned by digest",
    )?;
    let package = fs::canonicalize(job.package)?;
    require(
        package.join("FLIGHTDECK-PACKAGE.json").is_file(),
        "The package directory has no manifest",
    )?;
    let tool = fs::canonicalize(job.tool)?;
    let output = std::path::absolute(job.output)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::DirBuilder::new().mode(0o700).create(&output)?;
    let dockerfile =
        format!("FROM {image}\nRUN {install}\nRUN mkdir -p /checks /results /package\n");
    let tag = format!("flightdeck-linux-check:{}", job.distribution);
    let words = |words: &[&str]| {
        words
            .iter()
            .map(|word| word.to_string())
            .collect::<Vec<_>>()
    };
    docker.run(
        &words(&["docker", "build", "--tag", &tag, "-"]),
        Some(&dockerfile),
        Some(&output.join("build.log")),
        1200,
    )?;
    let built = docker
        .run(
            &words(&["docker", "image", "inspect", "--format", "{{.Id}}", &tag]),
            None,
            None,
            60,
        )?
        .trim()
        .to_string();
    let mut described = spec.clone();
    described["test_image"] = built.clone().into();
    fs::write(output.join("image.json"), pretty(&described))?;
    let mut command = words(&[
        "docker",
        "run",
        "--name",
        job.container,
        "--rm",
        "--init",
        "--network=none",
        "--read-only",
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        "--user",
    ]);
    command.push(format!("{}:{}", job.user.0, job.user.1));
    command.extend(words(&[
        "--tmpfs",
        "/tmp:rw,exec,mode=1777,size=768m",
        "--shm-size=128m",
        "--env",
        "HOME=/tmp",
        "--env",
        "XDG_CONFIG_HOME=/tmp/config",
        "--env",
        "XDG_DATA_HOME=/tmp/data",
        "--env",
        "XDG_STATE_HOME=/tmp/state",
        "--env",
        "XDG_CACHE_HOME=/tmp/cache",
    ]));
    for mount in [
        format!(
            "type=bind,source={},target=/package,readonly",
            package.display()
        ),
        format!("type=bind,source={},target=/results", output.display()),
        format!(
            "type=bind,source={},target=/checks/flightdeck-tools,readonly",
            tool.display()
        ),
    ] {
        command.extend(["--mount".to_string(), mount]);
    }
    command.push(built);
    command.extend(words(&[
        "/checks/flightdeck-tools",
        "distro-check",
        "--inside",
        "--package",
        "/package",
        "--output",
        "/results",
    ]));
    let result = docker.run(&command, None, None, 420);
    // A docker-client timeout does not stop the daemon-owned container.
    let _ = docker.run(
        &words(&["docker", "rm", "--force", job.container]),
        None,
        Some(Path::new("/dev/null")),
        30,
    );
    result.map(|_| ())
}

pub fn container_name() -> String {
    let mut random = [0u8; 16];
    let unique = fs::File::open("/dev/urandom")
        .and_then(|mut source| std::io::Read::read_exact(&mut source, &mut random))
        .map(|()| hex::encode(random))
        .unwrap_or_else(|_| format!("{}", std::process::id()));
    format!("flightdeck-linux-check-{unique}")
}

pub fn tool() -> Result<PathBuf> {
    Ok(std::env::current_exe()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        outcome: &'static str,
        containers: BTreeSet<String>,
        invocations: Vec<(Vec<String>, u64)>,
    }
    impl Docker for Fake {
        fn run(
            &mut self,
            command: &[String],
            _: Option<&str>,
            _: Option<&Path>,
            seconds: u64,
        ) -> Result<String> {
            self.invocations.push((command.to_vec(), seconds));
            match (command[1].as_str(), command[2].as_str()) {
                ("image", "inspect") => return Ok(format!("sha256:{}\n", "a".repeat(64))),
                ("run", _) => {
                    let name = command
                        .iter()
                        .position(|word| word == "--name")
                        .expect("name");
                    self.containers.insert(command[name + 1].clone());
                    // A killed Docker client deliberately leaves the daemon's
                    // container alive, just as a timeout does.
                    match self.outcome {
                        "timeout" => return fail("docker run timed out"),
                        "failed" => return fail("docker run failed"),
                        _ => {}
                    }
                }
                ("rm", "--force") => {
                    assert!(self.containers.remove(&command[3]));
                }
                _ => {}
            }
            Ok(String::new())
        }
    }

    #[test]
    fn daemon_owned_container_is_removed_after_timeout_failure_and_success() {
        for outcome in ["timeout", "failed", "success"] {
            let temp = tempfile::tempdir().expect("temp");
            let root = temp.path();
            let package = root.join("package ä");
            fs::create_dir(&package).expect("package");
            fs::write(package.join("FLIGHTDECK-PACKAGE.json"), "{}").expect("manifest");
            fs::write(root.join("tool"), "tool").expect("tool");
            let images = root.join("images.json");
            let image = format!("ubuntu:24.04@sha256:{}", "5".repeat(64));
            fs::write(
                &images,
                json!({"ubuntu": {"name": "Ubuntu", "image": image, "install": "true"}})
                    .to_string(),
            )
            .expect("images");
            let output = root.join("results");
            let mut docker = Fake {
                outcome,
                containers: BTreeSet::from(["unrelated-user-session".to_string()]),
                invocations: Vec::new(),
            };
            let job = Outside {
                images: &images,
                distribution: "ubuntu",
                package: &package,
                output: &output,
                tool: &root.join("tool"),
                user: (1001, 1002),
                container: "flightdeck-linux-check-fixture",
            };
            assert_eq!(
                outside(&mut docker, &job).is_ok(),
                outcome == "success",
                "{outcome}"
            );
            assert_eq!(
                docker.containers,
                BTreeSet::from(["unrelated-user-session".to_string()])
            );
            let (command, seconds) = docker
                .invocations
                .iter()
                .find(|(command, _)| command[1] == "run")
                .expect("run");
            assert!(*seconds <= 420);
            for flag in [
                "--network=none",
                "--read-only",
                "--cap-drop=ALL",
                "--security-opt=no-new-privileges",
            ] {
                assert!(command.iter().any(|word| word == flag), "{flag}");
            }
            let position = |word: &str| command.iter().position(|part| part == word).expect(word);
            assert_eq!(command[position("--user") + 1], "1001:1002");
            assert!(!command.iter().any(|word| {
                ["--privileged", "--pid", "--device", "--ipc", "--cap-add"]
                    .iter()
                    .any(|prefix| word.starts_with(prefix))
            }));
            let mounts: Vec<&String> = command
                .iter()
                .enumerate()
                .filter(|(_, word)| *word == "--mount")
                .map(|(index, _)| &command[index + 1])
                .collect();
            assert_eq!(mounts.len(), 3);
            let writable: Vec<_> = mounts
                .iter()
                .filter(|mount| !mount.ends_with(",readonly"))
                .collect();
            assert_eq!(
                writable,
                [&&format!(
                    "type=bind,source={},target=/results",
                    output.display()
                )]
            );
            let package = fs::canonicalize(&package).expect("package");
            assert!(mounts.contains(&&format!(
                "type=bind,source={},target=/package,readonly",
                package.display()
            )));
            assert!(
                !command
                    .iter()
                    .any(|word| word.contains("docker.sock") || word.contains("/dev/dri"))
            );
            assert_eq!(
                command[command.len() - 7..],
                [
                    "/checks/flightdeck-tools",
                    "distro-check",
                    "--inside",
                    "--package",
                    "/package",
                    "--output",
                    "/results"
                ]
            );
            let described: Value =
                serde_json::from_slice(&fs::read(output.join("image.json")).expect("image"))
                    .expect("JSON");
            assert_eq!(
                described["test_image"],
                format!("sha256:{}", "a".repeat(64))
            );
            assert!(
                !output.join("result.json").exists(),
                "only the inside checks may claim PASS"
            );
        }
    }

    #[test]
    fn unpinned_images_and_incomplete_packages_are_refused_before_docker_runs() {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path();
        fs::create_dir(root.join("package")).expect("package");
        fs::write(root.join("tool"), "tool").expect("tool");
        let attempt = |image: &str, manifest: bool| {
            let images = root.join("images.json");
            fs::write(
                &images,
                json!({"ubuntu": {"image": image, "install": "true"}}).to_string(),
            )
            .expect("images");
            if manifest {
                fs::write(root.join("package/FLIGHTDECK-PACKAGE.json"), "{}").expect("manifest");
            }
            let mut docker = Fake {
                outcome: "success",
                containers: BTreeSet::new(),
                invocations: Vec::new(),
            };
            let job = Outside {
                images: &images,
                distribution: "ubuntu",
                package: &root.join("package"),
                output: &root.join("results"),
                tool: &root.join("tool"),
                user: (1001, 1001),
                container: "fixture",
            };
            let result = outside(&mut docker, &job);
            (result, docker.invocations.len())
        };
        let pinned = format!("ubuntu:24.04@sha256:{}", "5".repeat(64));
        assert_eq!(attempt("ubuntu:24.04", true).1, 0);
        assert_eq!(attempt("ubuntu@sha256:abc", true).1, 0);
        fs::remove_file(root.join("package/FLIGHTDECK-PACKAGE.json")).expect("remove");
        let (result, calls) = attempt(&pinned, false);
        assert!(result.is_err() && calls == 0);
    }

    #[test]
    fn wayland_requires_a_real_buffer_commit_on_the_same_surface() {
        for separator in ["@", "#"] {
            let s = separator;
            let initial = format!(
                "xdg_wm_base{s}1.get_xdg_surface(new id xdg_surface{s}9, wl_surface{s}7)\n\
                 xdg_surface{s}9.get_toplevel(new id xdg_toplevel{s}8)\n\
                 xdg_toplevel{s}8.set_title(\"Flightdeck\")\n\
                 wl_surface{s}7.commit()\nxdg_surface{s}9.ack_configure(1)\n"
            );
            let attach = format!("wl_surface{s}7.attach(wl_buffer{s}10, 0, 0)\n");
            let decoration =
                format!("wl_surface{s}17.attach(wl_buffer{s}20, 0, 0)\nwl_surface{s}17.commit()\n");
            assert!(!wayland_painted(&initial));
            assert!(!wayland_painted(&format!("{initial}{decoration}")));
            assert!(!wayland_painted(&format!("{initial}{attach}")));
            assert!(!wayland_painted(&format!(
                "{initial}{attach}wl_surface{s}17.commit()\n"
            )));
            assert!(!wayland_painted(&format!(
                "{initial}wl_surface{s}7.attach(nil, 0, 0)\nwl_surface{s}7.commit()\n"
            )));
            let painted = format!("{initial}{decoration}{attach}wl_surface{s}7.commit()\n");
            assert!(wayland_painted(&painted));
            assert!(!wayland_painted(
                &painted.replace("set_title(\"Flightdeck\")", "set_title(\"Other\")")
            ));
            assert!(!wayland_painted(&painted.replace(
                &format!("xdg_surface{s}9.ack_configure"),
                &format!("xdg_surface{s}99.ack_configure")
            )));
        }
    }

    #[test]
    fn evidence_never_contains_credentials_and_corrupt_records_yield_none() {
        let record =
            br#"{"pid": 4242, "port": 8080, "release": "abc", "token": "synthetic-secret"}"#;
        let kept = evidence(record).expect("record");
        assert_eq!(kept, json!({"pid": 4242, "port": 8080, "release": "abc"}));
        assert!(!kept.to_string().contains("synthetic-secret"));
        assert_eq!(evidence(br#"{"token": "synthetic-secret", broken"#), None);
        assert_eq!(evidence(b"[1, 2]"), None);
    }

    #[test]
    fn owned_processes_are_stopped_newest_first_when_a_check_fails() {
        let spawn = || Command::new("sleep").arg("30").spawn().expect("process");
        let mut owned = Owned(vec![spawn(), spawn()]);
        let ids: Vec<u32> = owned.0.iter().map(Child::id).collect();
        let error = owned
            .wait_for(|| fail("paint failed"), 5)
            .expect_err("failed check");
        assert_eq!(error.0, "paint failed");
        drop(owned);
        for id in ids {
            assert!(!Path::new(&format!("/proc/{id}")).exists(), "{id} survived");
        }
        // A process that exits before readiness fails the wait immediately.
        let mut owned = Owned(vec![Command::new("true").spawn().expect("process")]);
        std::thread::sleep(Duration::from_millis(200));
        let error = owned.wait_for(|| Ok(false), 5).expect_err("exited");
        assert!(error.0.contains("exited before readiness"));
    }
}
