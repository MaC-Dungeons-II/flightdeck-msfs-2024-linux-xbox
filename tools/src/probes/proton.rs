// SPDX-License-Identifier: MIT
//! The Flightdeck runner, or a selected Proton, driven by the launcher's own
//! loader: synthetic programs handed over as memfd images, a DLL loaded from
//! the working and the module directory next to the Store libraries, then
//! Direct3D 11 and 12 rendering. Everything happens in a fresh prefix that
//! only copies the installed Store DLLs of an existing runtime, which is read
//! and never written. With `--runner` the profile also moves to that Proton
//! and back and must keep its identity and add-ons. No game, account or
//! licensed Fenix program is started.
use super::{
    Site,
    host::{self, Api, Environment},
    lab,
};
use crate::{
    ensure,
    util::{Failure, Result, digest, fail, require},
};
use rustix::io::FdFlags;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::{
        fd::{AsFd, AsRawFd},
        unix::fs::{MetadataExt, PermissionsExt, symlink},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[derive(clap::Args)]
#[command(group(clap::ArgGroup::new("mode").required(true).args(["runner", "default_runner"])))]
pub struct Arguments {
    /// Read only: source of the installed Store DLLs and base runner
    #[arg(long)]
    runtime: PathBuf,
    /// Exercise selection and return from this installed Proton
    #[arg(long)]
    runner: Option<PathBuf>,
    /// Exercise the default Flightdeck runner and native memfd mapping
    #[arg(long)]
    default_runner: bool,
    #[arg(long)]
    output: PathBuf,
    /// Test matching Fenix modules with synthetic apps; no licensed Fenix application
    #[arg(long)]
    fenix_bundle: Option<PathBuf>,
    /// Rust launcher binary (built by cargo build) [default: target/debug/flightdeck-rust]
    #[arg(long)]
    native_launcher: Option<PathBuf>,
}

/// The synthetic game files: the main program and a DLL in a subdirectory.
const NAMES: [&str; 2] = ["FlightSimulator2024.exe", "nested/probe.dll"];
const PLACEHOLDER: &[u8] = b"Synthetic encrypted placeholder; must remain unchanged.\n";
const SCOPE: &str = "Synthetic memfd EXE/DLL, working-directory and module-directory loading, Store library loading and D3D11/D3D12 rendering; profile round trip only with --runner; no game/account";

#[derive(serde::Serialize)]
struct Report {
    implementation: &'static str,
    mode: &'static str,
    version: Value,
    loader: &'static str,
    loader_passed: bool,
    fastlaunch_argument_passed: bool,
    working_directory_dll_passed: bool,
    rendering_passed: bool,
    original_files_unchanged: bool,
    return_to_default_passed: Option<bool>,
    profile_identity_preserved: bool,
    addons_retained: bool,
    fenix_overlay: bool,
    scope: &'static str,
}

/// Every check has passed when this is written; a failure ends the probe
/// earlier. The return to the default runner is only exercised with Proton.
fn report(selected: bool, version: Value, fenix_overlay: bool) -> Report {
    Report {
        implementation: "rust",
        mode: if selected { "proton" } else { "flightdeck" },
        version,
        loader: if selected { "portable" } else { "native" },
        loader_passed: true,
        fastlaunch_argument_passed: true,
        working_directory_dll_passed: true,
        rendering_passed: true,
        original_files_unchanged: true,
        return_to_default_passed: selected.then_some(true),
        profile_identity_preserved: true,
        addons_retained: true,
        fenix_overlay,
        scope: SCOPE,
    }
}

/// What a real import records about the Store libraries copied into the
/// prefix, so the launcher accepts the synthetic runtime as prepared.
fn import_manifest(hashes: &BTreeMap<String, String>) -> Result<Value> {
    let hash = |name: &str| {
        hashes
            .get(name)
            .ok_or_else(|| Failure(format!("The Store bridge does not include {name}")))
    };
    Ok(
        json!({"original_runtime_sha256": hash("xgameruntime_original.dll")?, "artifacts": {"files": {
            "runtime/xgameruntime.dll": hash("xgameruntime.dll")?,
            "builtin/x86_64-windows/xodus_store_test.dll": hash("xodus_store_test.dll")?}}}),
    )
}

/// Adds what the launcher's game start would set for the loader under test.
fn loader_environment(environment: &mut Environment, runtime: &Path, wine: &str, selected: bool) {
    host::set(
        environment,
        [
            (
                "FLIGHTDECK_PROTON_LOADER",
                if selected { "portable" } else { "native" },
            ),
            (
                "WINEDLLPATH",
                &host::text(&runtime.join("local/store-runtime")),
            ),
            ("FLIGHTDECK_FAST_LAUNCH", "1"),
            ("XODUS_WINE_RUNNER", wine),
            ("WINELOADER", wine),
            (
                "WINESERVER",
                &host::text(&runtime.join("runner/files/bin/wineserver")),
            ),
            (
                "WINEDLLOVERRIDES",
                "xgameruntime=n;xgameruntime_original=n,b;xodus_store_test=b;winemenubuilder.exe=d",
            ),
            ("DXVK_LOG_LEVEL", "info"),
            ("VKD3D_DEBUG", "warn"),
        ],
    );
}

/// `WINE_DLL_FILE_MAP`: each inherited descriptor with the NT path of the
/// placeholder file it stands for.
fn file_map(images: &[(i32, PathBuf)]) -> String {
    images
        .iter()
        .map(|(descriptor, path)| {
            format!(
                "{descriptor}:\\??\\Z:{}",
                host::text(path).replace('/', "\\")
            )
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// An anonymous memory file with the content of a built program.
fn image(path: &Path) -> Result<fs::File> {
    let descriptor = rustix::fs::memfd_create(
        "flightdeck-synthetic-probe",
        rustix::fs::MemfdFlags::CLOEXEC,
    )
    .map_err(std::io::Error::from)?;
    let mut image = fs::File::from(descriptor);
    image.write_all(&fs::read(path)?)?;
    Ok(image)
}

/// Runs `start` with the descriptors inheritable, like `pass_fds`. They are
/// close-on-exec before and after, so no other child ever receives them;
/// `pre_exec` would be the usual way and is unsafe.
fn inherited<T>(descriptors: &[impl AsFd], start: impl FnOnce() -> T) -> Result<T> {
    let flag = |flags: FdFlags| -> Result<()> {
        for descriptor in descriptors {
            rustix::io::fcntl_setfd(descriptor, flags).map_err(std::io::Error::from)?;
        }
        Ok(())
    };
    flag(FdFlags::empty())?;
    let result = start();
    flag(FdFlags::CLOEXEC)?;
    Ok(result)
}

fn identity(path: &Path) -> Result<(u64, u64)> {
    let info =
        fs::metadata(path).map_err(|error| Failure(format!("{}: {error}", path.display())))?;
    Ok((info.dev(), info.ino()))
}

fn announced_port(line: &str) -> Option<u16> {
    line.strip_prefix("Flightdeck: http://127.0.0.1:")?
        .strip_suffix('\n')
        .filter(|port| !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()))?
        .parse()
        .ok()
}

/// Whether a Proton job has ended, and how.
fn settled(job: &Value) -> Option<Result<()>> {
    let state = job["state"].as_str().unwrap_or_default();
    (host::truthy(job) && ["complete", "failed", "cancelled"].contains(&state))
        .then(|| require(state == "complete", job.to_string()))
}

/// The launcher service of the probe runtime. Dropping stops it, so no
/// failure in between leaves a service behind.
struct Launcher {
    child: Child,
    api: Api,
}
impl Launcher {
    fn start(binary: &Path, output: &Path, runtime: &Path) -> Result<Self> {
        let binary = host::canonical(binary)?;
        let state = output.join("native-state");
        let log = fs::File::create(output.join("native-service.log"))?;
        let child = Command::new(&binary)
            .arg("--state-dir")
            .arg(&state)
            .arg("--runtime")
            .arg(runtime)
            .arg("--no-browser")
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .map_err(|error| Failure(format!("{}: {error}", binary.display())))?;
        let mut launcher = Self {
            child,
            api: Api {
                port: 0,
                token: String::new(),
                seconds: 15,
            },
        };
        let line = host::first_line(&mut launcher.child, 15, "Native service did not start")?;
        launcher.api.port = announced_port(&line)
            .ok_or_else(|| Failure("Unexpected native service output".into()))?;
        launcher.api.token = host::read_json(&state.join("desktop-service.json"))?["token"]
            .as_str()
            .ok_or_else(|| Failure("The service record has no token".into()))?
            .to_string();
        Ok(launcher)
    }

    /// Selects a runner and waits for the preparation job.
    fn switch(&self, runtime: &Path, mode: &str, path: Option<&Path>) -> Result<()> {
        self.api.request(
            "/api/proton/select",
            Some(&json!({"runtime_path": host::text(runtime), "mode": mode,
                "path": path.map(host::text).unwrap_or_default()})),
            None,
        )?;
        let deadline = Instant::now() + Duration::from_secs(600);
        while Instant::now() < deadline {
            let job = self.api.request("/api/proton", None, None)?["job"].clone();
            if let Some(ended) = settled(&job) {
                return ended;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        fail("Native Proton preparation timed out")
    }

    fn close(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            host::interrupt(&self.child);
            if !host::exited_within(&mut self.child, 15) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
}
impl Drop for Launcher {
    fn drop(&mut self) {
        self.close();
    }
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| Failure(format!("The Rust probe answered without {key}")))
}

pub fn run(site: &Site, arguments: &Arguments) -> Result<u8> {
    let (root, lab) = (site.root, &site.lab);
    let native_launcher = arguments
        .native_launcher
        .clone()
        .unwrap_or_else(|| root.join("target/debug/flightdeck-rust"));
    let original = host::canonical(&arguments.runtime)?;
    let base = lab.call("proton-info", json!({"root": host::text(&original)}))?;
    let runner = host::canonical(Path::new(field(&base, "base_runner")?))?;
    let output = host::resolve(&arguments.output)?;
    host::new_dirs(&output, host::DEFAULT)?;
    let runtime = output.join("runtime");
    host::new_dirs(&runtime.join("private"), host::PRIVATE)?;
    host::new_dir(&runtime.join("local"), host::DEFAULT)?;
    symlink(&runner, runtime.join("runner"))?;
    host::copytree(&root.join("scripts/runtime"), &runtime.join("tools"), false)?;
    for tool in fs::read_dir(runtime.join("tools"))? {
        fs::set_permissions(tool?.path(), fs::Permissions::from_mode(0o700))?;
    }
    host::write(
        &runtime.join("private/runtime.json"),
        "{\"game_id\":\"msfs2024\",\"market\":\"AT\"}\n",
    )?;
    let prefix = runtime.join("local/msfs-prefix");
    lab::mark(&runtime)?;
    let probe_runtime = json!({"root": host::text(&runtime)});
    lab.call("prefix", probe_runtime.clone())?;
    let source = original.join("local/msfs-prefix/drive_c/windows/system32");
    let system = prefix.join("drive_c/windows/system32");
    let mut hashes = BTreeMap::new();
    let bridge = lab.call("proton-info", probe_runtime.clone())?;
    for name in bridge["bridge"]
        .as_array()
        .ok_or_else(|| Failure("The Rust probe answered without bridge".into()))?
    {
        let name = name
            .as_str()
            .ok_or_else(|| Failure("Unexpected Store bridge name".into()))?;
        // A fresh prefix may hold a link to the runner's own file here.
        match fs::remove_file(system.join(name)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        host::copyfile(&source.join(name), &system.join(name))?;
        hashes.insert(name.to_string(), digest(&fs::read(system.join(name))?));
    }
    host::copytree(
        &original.join("local/store-runtime"),
        &runtime.join("local/store-runtime"),
        true,
    )?;
    host::write(
        &runtime.join("private/import-manifest.json"),
        import_manifest(&hashes)?.to_string(),
    )?;
    host::write(&prefix.join("original-marker"), "preserve this profile")?;
    let (exe, dll, dll_source) = (
        output.join("probe.exe"),
        output.join("probe.dll"),
        output.join("dll.c"),
    );
    host::write(
        &dll_source,
        "__declspec(dllexport) int probe(void) { return 42; }\n",
    )?;
    host::checked(
        Command::new(&site.compiler)
            .args(["-O2", "-Wall", "-Wextra"])
            .arg(root.join("tests/graphics/proton-loader.c"))
            .arg("-o")
            .arg(&exe),
    )?;
    host::checked(
        Command::new(&site.compiler)
            .arg("-shared")
            .arg(&dll_source)
            .arg("-o")
            .arg(&dll),
    )?;
    let graphics = output.join("multiwindow.exe");
    host::checked(
        Command::new(&site.compiler)
            .args(["-O2", "-Wall", "-Wextra", "-Werror"])
            .arg(root.join("tests/graphics/multiwindow.c"))
            .arg("-o")
            .arg(&graphics)
            .args(["-ld3d12", "-ld3d11", "-ldxgi", "-ldxguid", "-lgdi32"]),
    )?;
    let game = runtime.join("games/MSFS2024");
    host::new_dirs(&game.join("nested"), host::DEFAULT)?;
    for name in NAMES {
        host::write(&game.join(name), PLACEHOLDER)?;
    }
    let unchanged = digest(PLACEHOLDER);
    let mut launcher = Launcher::start(&native_launcher, &output, &runtime)?;
    if let Some(bundle) = &arguments.fenix_bundle {
        let fixture = lab.call(
            "fenix",
            json!({"root": host::text(&runtime), "bundle": host::text(&host::canonical(bundle)?)}),
        )?;
        let cache = output.join("native-state/fenix-bundles");
        host::new_dirs(&cache, host::DEFAULT)?;
        symlink(
            field(&fixture, "bundle")?,
            cache.join(field(&fixture, "version")?),
        )?;
    }
    let before = identity(&prefix)?;
    if let Some(proton) = &arguments.runner {
        launcher.switch(&runtime, "proton", Some(&host::canonical(proton)?))?;
    }
    let info = lab.call("proton-info", probe_runtime.clone())?;
    let selection = &info["selection"];
    let selected = host::truthy(selection);
    ensure!(selected == arguments.runner.is_some());
    let wine = field(&info, "wine")?;
    let mut env = lab.isolated_environment(&runtime)?;
    loader_environment(&mut env, &runtime, wine, selected);
    let ran = (|| {
        let images = [image(&exe)?, image(&dll)?];
        let mapped: Vec<_> = images
            .iter()
            .zip(NAMES)
            .map(|(image, name)| (image.as_raw_fd(), game.join(name)))
            .collect();
        env.insert("WINE_DLL_FILE_MAP".into(), file_map(&mapped));
        let mut launch = Command::new(host::resolve(&native_launcher)?);
        launch
            .arg("wine-launch")
            .arg("--runtime")
            .arg(&runtime)
            .arg("--")
            .arg(game.join(NAMES[0]))
            .env_clear()
            .envs(&env);
        let loader = inherited(&images, || {
            host::logged(&mut launch, &output.join("loader.log"), 60)
        })??;
        env.remove("WINE_DLL_FILE_MAP");
        let rendering = host::logged(
            Command::new(wine)
                .arg(&graphics)
                .current_dir(&output)
                .env_clear()
                .envs(&env),
            &output.join("render.log"),
            90,
        )?;
        Ok((loader, rendering))
    })();
    let stopped = host::stop_wine(&runtime.join("runner/files/bin/wineserver"), &env, |_| {});
    let (loader, rendering) = host::finally(ran, stopped)?;
    for (code, log) in [(loader, "loader.log"), (rendering, "render.log")] {
        if code != 0 {
            let text = fs::read(output.join(log)).unwrap_or_default();
            return fail(format!(
                "{log} ended with exit status {code}:\n{}",
                String::from_utf8_lossy(&text)
            ));
        }
    }
    for name in NAMES {
        ensure!(digest(&fs::read(game.join(name))?) == unchanged, "{name}");
    }
    for entry in fs::read_dir(&game)? {
        let name = entry?.file_name();
        ensure!(
            !name.to_string_lossy().starts_with(".xodus-launch-"),
            "{}",
            name.to_string_lossy()
        );
    }
    host::write(
        &prefix.join("installed-on-proton"),
        "retain add-ons and settings",
    )?;
    if selected {
        launcher.switch(&runtime, "default", None)?;
    }
    launcher.close();
    let profile = if selected {
        runtime.join(field(selection, "base_prefix")?)
    } else {
        prefix.clone()
    };
    ensure!(identity(&profile)? == before);
    ensure!(fs::read_to_string(prefix.join("original-marker"))? == "preserve this profile");
    ensure!(
        fs::read_to_string(prefix.join("installed-on-proton"))? == "retain add-ons and settings"
    );
    let last = lab.call("proton-info", probe_runtime.clone())?;
    ensure!(last["selection"].is_null());
    let runner_version = fs::read_to_string(runner.join("version"))?
        .trim()
        .to_string();
    ensure!(last["version"] == runner_version);
    if arguments.fenix_bundle.is_some() {
        lab.call("fenix-check", probe_runtime)?;
    }
    let version = if selected {
        selection["version"].clone()
    } else {
        runner_version.into()
    };
    let report = host::pretty(&report(selected, version, arguments.fenix_bundle.is_some()))?;
    host::write(&output.join("result.json"), &report)?;
    print!("{report}");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{http::fixtures::Service, probes::fixtures::script};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn the_import_manifest_binds_the_copied_store_libraries() {
        let mut hashes: BTreeMap<String, String> = [
            ("xgameruntime.dll", "aa"),
            ("xgameruntime_original.dll", "bb"),
            ("xodus_store_test.dll", "cc"),
        ]
        .map(|(name, hash)| (name.to_string(), hash.to_string()))
        .into();
        assert_eq!(
            import_manifest(&hashes).expect("manifest"),
            json!({"original_runtime_sha256": "bb", "artifacts": {"files": {
                "runtime/xgameruntime.dll": "aa",
                "builtin/x86_64-windows/xodus_store_test.dll": "cc"}}})
        );
        hashes.remove("xodus_store_test.dll");
        assert_eq!(
            import_manifest(&hashes).expect_err("missing").0,
            "The Store bridge does not include xodus_store_test.dll"
        );
    }

    #[test]
    fn the_loader_environment_follows_the_selected_runner() {
        let mut native = Environment::from([("WINEPREFIX".to_string(), "/p".to_string())]);
        loader_environment(
            &mut native,
            Path::new("/out/runtime"),
            "/runner/bin/wine",
            false,
        );
        assert_eq!(
            native,
            [
                ("DXVK_LOG_LEVEL", "info"),
                ("FLIGHTDECK_FAST_LAUNCH", "1"),
                ("FLIGHTDECK_PROTON_LOADER", "native"),
                ("VKD3D_DEBUG", "warn"),
                (
                    "WINEDLLOVERRIDES",
                    "xgameruntime=n;xgameruntime_original=n,b;xodus_store_test=b;winemenubuilder.exe=d"
                ),
                ("WINEDLLPATH", "/out/runtime/local/store-runtime"),
                ("WINELOADER", "/runner/bin/wine"),
                ("WINEPREFIX", "/p"),
                ("WINESERVER", "/out/runtime/runner/files/bin/wineserver"),
                ("XODUS_WINE_RUNNER", "/runner/bin/wine"),
            ]
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .into()
        );
        let mut portable = Environment::new();
        loader_environment(&mut portable, Path::new("/r"), "wine", true);
        assert_eq!(portable["FLIGHTDECK_PROTON_LOADER"], "portable");
    }

    #[test]
    fn the_file_map_names_descriptors_with_nt_paths() {
        assert_eq!(
            file_map(&[
                (
                    7,
                    "/out/runtime/games/MSFS2024/FlightSimulator2024.exe".into()
                ),
                (8, "/out/runtime/games/MSFS2024/nested/probe.dll".into()),
            ]),
            r"7:\??\Z:\out\runtime\games\MSFS2024\FlightSimulator2024.exe|8:\??\Z:\out\runtime\games\MSFS2024\nested\probe.dll"
        );
        assert_eq!(file_map(&[]), "");
    }

    #[test]
    fn memory_images_reach_only_the_child_started_for_them() {
        let temp = tempfile::tempdir().expect("temp");
        let program = temp.path().join("probe.exe");
        fs::write(&program, b"MZ synthetic").expect("program");
        let images = [image(&program).expect("image")];
        let read = || {
            Command::new("cat")
                .arg(format!("/proc/self/fd/{}", images[0].as_raw_fd()))
                .stderr(Stdio::null())
                .output()
                .expect("cat")
        };
        let during = inherited(&images, read).expect("flags");
        assert!(during.status.success());
        assert_eq!(during.stdout, b"MZ synthetic");
        assert!(!read().status.success());
    }

    #[test]
    fn only_the_announced_loopback_port_is_accepted() {
        assert_eq!(
            announced_port("Flightdeck: http://127.0.0.1:43127\n"),
            Some(43127)
        );
        for line in [
            "",
            "Flightdeck: http://127.0.0.1:43127",
            "Flightdeck: http://127.0.0.1:\n",
            "Flightdeck: http://127.0.0.1:+80\n",
            "Flightdeck: http://127.0.0.1:99999\n",
            "Flightdeck: http://localhost:80\n",
            " Flightdeck: http://127.0.0.1:80\n",
            "Flightdeck: http://127.0.0.1:80/\n",
        ] {
            assert_eq!(announced_port(line), None, "{line:?}");
        }
    }

    #[test]
    fn a_job_is_awaited_until_it_ends_and_only_completion_passes() {
        for pending in [
            json!(null),
            json!({}),
            json!({"state": "running"}),
            json!({"state": "queued", "progress": 0.5}),
        ] {
            assert!(settled(&pending).is_none(), "{pending}");
        }
        assert!(
            settled(&json!({"state": "complete"}))
                .expect("ended")
                .is_ok()
        );
        for state in ["failed", "cancelled"] {
            let job = json!({"state": state, "error": "No space left"});
            assert_eq!(
                settled(&job).expect("ended").expect_err(state).0,
                job.to_string()
            );
        }
    }

    #[test]
    fn the_report_distinguishes_the_default_runner_from_proton() {
        let default =
            serde_json::to_value(report(false, "GE-Proton10-1".into(), false)).expect("report");
        assert_eq!(
            default,
            json!({"implementation": "rust", "mode": "flightdeck", "version": "GE-Proton10-1",
                "loader": "native", "loader_passed": true, "fastlaunch_argument_passed": true,
                "working_directory_dll_passed": true, "rendering_passed": true,
                "original_files_unchanged": true, "return_to_default_passed": null,
                "profile_identity_preserved": true, "addons_retained": true,
                "fenix_overlay": false, "scope": SCOPE})
        );
        let proton = serde_json::to_value(report(true, "Proton 10".into(), true)).expect("report");
        assert_eq!(proton["mode"], "proton");
        assert_eq!(proton["loader"], "portable");
        assert_eq!(proton["return_to_default_passed"], true);
        assert_eq!(proton["fenix_overlay"], true);
    }

    #[test]
    fn the_service_is_started_asked_to_switch_and_interrupted() {
        let polls = Arc::new(AtomicUsize::new(0));
        let selections = Arc::new(Mutex::new(Vec::new()));
        let (counted, recorded) = (polls.clone(), selections.clone());
        let service = Service::start(move |method, path, headers, body| {
            let authenticated =
                headers.get("x-flightdeck-token") == Some(&vec!["session".to_string()]);
            match (method, path) {
                _ if !authenticated => (403, json!({})),
                ("POST", "/api/proton/select") => {
                    let selection: Value = serde_json::from_slice(body).expect("selection");
                    recorded.lock().expect("selections").push(selection);
                    (200, json!({}))
                }
                ("GET", "/api/proton") => {
                    let job = match counted.fetch_add(1, Ordering::SeqCst) {
                        0 => json!(null),
                        1 => json!({"state": "running"}),
                        2 => json!({"state": "complete"}),
                        _ => json!({"state": "failed", "error": "Unsupported Proton"}),
                    };
                    (200, json!({"job": job}))
                }
                _ => (404, json!({})),
            }
        });
        let temp = tempfile::tempdir().expect("temp");
        let binary = temp.path().join("flightdeck-rust");
        script(
            &binary,
            &format!(
                "mkdir -p \"$2\"\nprintf '%s' '{{\"token\":\"session\"}}' > \"$2/desktop-service.json\"\n\
                 printf '%s\\n' \"$@\" > \"$2/arguments\"\necho 'Flightdeck: http://127.0.0.1:{}'\nexec sleep 60",
                service.port
            ),
        );
        let runtime = temp.path().join("runtime");
        let mut launcher = Launcher::start(&binary, temp.path(), &runtime).expect("service");
        assert_eq!(
            fs::read_to_string(temp.path().join("native-state/arguments")).expect("arguments"),
            format!(
                "--state-dir\n{}\n--runtime\n{}\n--no-browser\n",
                temp.path().join("native-state").display(),
                runtime.display()
            )
        );
        assert!(temp.path().join("native-service.log").is_file());
        launcher
            .switch(&runtime, "proton", Some(Path::new("/proton")))
            .expect("complete");
        assert_eq!(polls.load(Ordering::SeqCst), 3);
        let failed = launcher
            .switch(&runtime, "default", None)
            .expect_err("failed");
        assert_eq!(
            failed.0,
            json!({"state": "failed", "error": "Unsupported Proton"}).to_string()
        );
        assert_eq!(
            *selections.lock().expect("selections"),
            [
                json!({"runtime_path": host::text(&runtime), "mode": "proton", "path": "/proton"}),
                json!({"runtime_path": host::text(&runtime), "mode": "default", "path": ""}),
            ]
        );
        launcher.close();
        assert!(matches!(launcher.child.try_wait(), Ok(Some(_))));
    }

    #[test]
    fn a_service_without_the_expected_announcement_is_stopped() {
        let temp = tempfile::tempdir().expect("temp");
        let binary = temp.path().join("flightdeck-rust");
        script(&binary, "echo 'Listening elsewhere'\nexec sleep 60");
        let error = Launcher::start(&binary, temp.path(), temp.path())
            .err()
            .expect("refused");
        assert_eq!(error.0, "Unexpected native service output");
        assert!(Launcher::start(&temp.path().join("missing"), temp.path(), temp.path()).is_err());
    }
}
