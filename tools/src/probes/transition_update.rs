// SPDX-License-Identifier: MIT
//! The update path of the Python releases, with real packages: the published
//! 0.1.21 is installed and its service is asked to update to the 0.1.22 bridge
//! and on to a native candidate, back to the bridge and forward again; then
//! the bridge's own installer and the terminal update take the same step,
//! and the service of the installed native package must answer.
//! Everything lives in a new private directory, without display or network.
//!
//! The releases under test are Python programs, so an interpreter runs them,
//! and their GitHub reads are replaced by a hook in that interpreter which
//! exists only in the test directory. Product code has no test routes, custom
//! update URLs or bypass switches. This checks the historical two-step
//! transition, not the current public Latest routing.
use super::host;
use crate::{
    ensure,
    package_check::{Poll, record_at, request, wait_for},
    util::{Failure, Result, digest, fail, read_regular, require},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

#[derive(clap::Args)]
pub struct Arguments {
    /// The extracted, unchanged 0.1.21 package
    #[arg(long)]
    legacy_package: PathBuf,
    /// The published 0.1.22 archive
    #[arg(long)]
    bridge_archive: PathBuf,
    /// A native archive with a stable version
    #[arg(long)]
    native_archive: PathBuf,
    /// New directory for the isolated installations and the result
    #[arg(long)]
    output: PathBuf,
    /// Interpreter that runs the Python releases under test
    #[arg(long, default_value = "python3")]
    python: PathBuf,
}

const ARCHIVE_MAX: u64 = 2 * 1024 * 1024 * 1024;
const PROJECT: &str = "https://github.com/marselnenaj/flightdeck-msfs-2024-linux-xbox";

/// Loaded by the interpreters of the releases under test as `sitecustomize`:
/// their update module reads the offer and the archives of this check instead
/// of GitHub. Any other address fails the check.
const HOOK: &str = r#"import io,json,os,sys
from pathlib import Path
fixture=Path(os.environ["FLIGHTDECK_TRANSITION_FIXTURE"])
def local_url(url):
    values=json.loads((fixture/"offer.json").read_text())
    if url.endswith("/releases/latest"):
        return io.BytesIO(json.dumps(values["bridge"]).encode())
    if url.endswith("/releases?per_page=50"):
        return io.BytesIO(json.dumps([values["bridge"],values["native"]]).encode())
    for name in ("bridge","native"):
        if url==values[name]["assets"][0]["browser_download_url"]:
            return Path(values[name+"_archive"]).open("rb")
    raise AssertionError("Unexpected network request in isolated transition check")
if (Path.cwd()/"flightdeck/__init__.py").is_file():
    sys.path.insert(0,str(Path.cwd()))
    from flightdeck import launcher_update as updates
    updates.open_url=local_url
"#;

type Environment = BTreeMap<OsString, OsString>;

/// The result, with the keys in the order the Python check wrote them.
#[derive(serde::Serialize)]
struct Report<'a> {
    status: &'static str,
    initial_version: &'static str,
    hops: &'a [String],
    same_browser_origin: bool,
    settings_preserved: bool,
    no_additional_browser: bool,
    final_wrapper_requires_python: bool,
    terminal_update: &'static str,
    native_window: &'static str,
    tampered_native_binary_rejected: bool,
    bridge_bootstrap: &'static str,
    bridge_archive_sha256: String,
    native_archive_sha256: String,
    network: &'static str,
    published: bool,
}

/// The release as GitHub would describe it; nothing of it is published.
fn metadata(version: &str, archive: &[u8]) -> Value {
    json!({"draft": false, "prerelease": false, "tag_name": format!("v{version}"),
        "body": "Local fixture; not published",
        "assets": [{"name": "Flightdeck-Linux-x86_64.tar.gz", "state": "uploaded",
            "size": archive.len(), "digest": format!("sha256:{}", digest(archive)),
            "browser_download_url": format!(
                "{PROJECT}/releases/download/v{version}/Flightdeck-Linux-x86_64.tar.gz")}]})
}

fn member(archive: &[u8], name: &str) -> Result<Vec<u8>> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.as_ref() == Path::new(name) {
            let mut content = Vec::new();
            entry.by_ref().take(1024 * 1024).read_to_end(&mut content)?;
            return Ok(content);
        }
    }
    fail(format!("The archive has no {name}"))
}

/// The version a package states about itself. The stable channel offers only
/// plain versions, so a candidate with any other version cannot be tested.
fn version(archive: &[u8], native: bool) -> Result<String> {
    let found = if native {
        let manifest: Value = serde_json::from_slice(&member(
            archive,
            "flightdeck-linux/FLIGHTDECK-PACKAGE.json",
        )?)?;
        manifest["version"].as_str().map(str::to_string)
    } else {
        let text = member(archive, "flightdeck-linux/flightdeck/__init__.py")?;
        regex::Regex::new(r#"(?m)^__version__\s*=\s*"([^"]+)""#)
            .expect("pattern")
            .captures(&String::from_utf8_lossy(&text))
            .map(|found| found[1].to_string())
    };
    let stable = regex::Regex::new(r"^[0-9]+\.[0-9]+\.[0-9]+$").expect("pattern");
    match found {
        Some(version) if stable.is_match(&version) => Ok(version),
        _ => fail("Use an isolated stable-version native candidate for the stable-channel test"),
    }
}

/// Files, directories and links below `destination`; nothing else and
/// nothing that would leave it.
fn extract(archive: &[u8], destination: &Path) -> Result<()> {
    fs::create_dir(destination)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        require(
            kind.is_file() || kind.is_dir() || kind.is_symlink() || kind.is_hard_link(),
            "Unexpected archive member",
        )?;
        require(
            entry.unpack_in(destination)?,
            "Archive member leaves its directory",
        )?;
    }
    Ok(())
}

/// The host environment with a private home, no display, no reachable network
/// and a browser that only records that it was started.
fn environment(host: Environment, output: &Path, hook: &Path, browsers: &Path) -> Environment {
    let mut environment = host;
    environment.remove(OsStr::new("PYTHONHOME"));
    let mut path = OsString::from(browsers);
    path.push(":");
    path.push(
        environment
            .get(OsStr::new("PATH"))
            .cloned()
            .unwrap_or_default(),
    );
    let mut set = |name: &str, value: OsString| environment.insert(name.into(), value);
    set("PYTHONPATH", hook.into());
    set("FLIGHTDECK_TRANSITION_FIXTURE", output.into());
    set("HTTPS_PROXY", "http://127.0.0.1:1".into());
    set("HTTP_PROXY", "http://127.0.0.1:1".into());
    set("NO_PROXY", "127.0.0.1,localhost".into());
    set("HOME", output.into());
    set("DISPLAY", "".into());
    set("WAYLAND_DISPLAY", "".into());
    set(
        "FLIGHTDECK_TEST_BROWSER_LOG",
        output.join("unexpected-browser").into(),
    );
    set("PATH", path);
    homes(&mut environment, output);
    environment
}

fn homes(environment: &mut Environment, root: &Path) {
    for name in ["DATA", "STATE", "CONFIG", "CACHE"] {
        environment.insert(
            format!("XDG_{name}_HOME").into(),
            root.join(name.to_lowercase()).into(),
        );
    }
}

fn without(environment: &Environment, names: &[&str]) -> Environment {
    let mut environment = environment.clone();
    for name in names {
        environment.remove(OsStr::new(name));
    }
    environment
}

/// A request outside of polling: any failure to get an answer ends the check.
fn call(record: &Value, path: &str, data: Option<&Value>) -> Result<Value> {
    match request(record, path, data) {
        Ok(value) => Ok(value),
        Err(Poll::Retry(reason)) => fail(format!("{path}: {reason}")),
        Err(Poll::Fatal(error)) => Err(error),
    }
}

fn post(record: &Value, path: &str) -> Result<Value> {
    call(record, path, Some(&json!({})))
}

/// The confirmed service record; after a restart the one of the replacement,
/// which must keep the port and, where given, report the wanted version.
fn current(state: &Path, previous: Option<&Value>, wanted: Option<&str>) -> Result<Value> {
    wait_for(
        || {
            let Some(record) = record_at(state)? else {
                return Ok(None);
            };
            if let Some(previous) = previous {
                if record["token"] == previous["token"] {
                    return Ok(None);
                }
                if record["port"] != previous["port"] {
                    return Err(Poll::Fatal(Failure(
                        "The restart must preserve the local service endpoint".into(),
                    )));
                }
            }
            if let Some(wanted) = wanted {
                let version = request(&record, "/api/status", None)?["app"]["version"].clone();
                if version != wanted {
                    return Err(Poll::Fatal(Failure(format!(
                        "The service reports version {version}, not {wanted}"
                    ))));
                }
            }
            Ok(Some(record))
        },
        Duration::from_secs(60),
    )
}

/// The update status once the running job is complete.
fn finished(record: &Value) -> Result<Value> {
    wait_for(
        || {
            let status = request(record, "/api/launcher-update", None)?;
            if status["job"]["state"] == "failed" {
                return Err(Poll::Fatal(Failure(format!(
                    "The update job failed: {}",
                    status["job"]
                ))));
            }
            Ok((status["job"]["state"] == "complete").then_some(status))
        },
        Duration::from_secs(60),
    )
}

fn restart(state: &Path, record: &Value, wanted: &str) -> Result<Value> {
    post(record, "/api/launcher-update/restart")?;
    current(state, Some(record), Some(wanted))
}

fn installed(data: &Path) -> Result<Value> {
    host::read_json(&data.join("installation.json"))
}

fn shell_wrapper(path: &Path) -> Result<bool> {
    Ok(fs::read(path)?.starts_with(b"#!/bin/sh\n"))
}

fn command(program: &Path, directory: &Path, environment: &Environment) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(directory)
        .env_clear()
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn succeeded(what: &str, run: &host::Finished) -> Result<()> {
    require(
        run.code == 0,
        format!(
            "{what} failed with exit status {}: {}",
            run.code,
            String::from_utf8_lossy(&run.stderr)
        ),
    )
}

struct Inputs<'a> {
    python: &'a Path,
    legacy: &'a Path,
    bridge: &'a [u8],
    native: &'a [u8],
    bridge_version: &'a str,
    native_version: &'a str,
}

/// What the check starts and must stop again, whatever happens.
#[derive(Default)]
struct Started {
    children: Vec<Child>,
    states: Vec<PathBuf>,
}
impl Started {
    fn stop(&mut self) {
        for state in &self.states {
            if let Ok(record) = wait_for(|| record_at(state), Duration::from_secs(2)) {
                let _ = post(&record, "/api/desktop/refresh");
            }
        }
        for child in &mut self.children {
            if matches!(child.try_wait(), Ok(None)) {
                host::terminate(child);
            }
            if !host::exited_within(child, 20) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

/// 0.1.21 → bridge → native → bridge → native through the running service.
fn service_updates(
    inputs: &Inputs,
    output: &Path,
    env: &Environment,
    started: &mut Started,
) -> Result<Vec<String>> {
    let (state, data, bin) = (
        output.join("state/flightdeck"),
        output.join("installed"),
        output.join("bin"),
    );
    started.states.push(state.clone());
    // The initial installation is the real old package, not the current
    // source with an artificially changed version string.
    let install = host::run(
        command(inputs.python, output, &without(env, &["PYTHONPATH"]))
            .arg(inputs.legacy.join("scripts/install-launcher.py"))
            .arg("--source")
            .arg(inputs.legacy)
            .arg("--data-dir")
            .arg(&data)
            .arg("--bin-dir")
            .arg(&bin)
            .arg("--applications-dir")
            .arg(output.join("applications"))
            .args(["--no-desktop", "--no-launch", "--language", "en"]),
        None,
        300,
    )?;
    succeeded("The legacy installation", &install)?;
    host::dirs(&state, host::PRIVATE)?;
    let marker = state.join("retained-preference.txt");
    fs::write(&marker, "synthetic browser preference")?;
    let release = data
        .join("releases")
        .join(installed(&data)?["current"].as_str().unwrap_or_default());
    let log = fs::File::create(output.join("service.log"))?;
    started.children.push(
        command(inputs.python, &release, env)
            .args(["-B", "-m", "flightdeck", "--desktop-service", "--state-dir"])
            .arg(&state)
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?,
    );
    let mut record = current(&state, None, Some("0.1.21"))?;
    let mut hops = Vec::new();
    for target in [inputs.bridge_version, inputs.native_version] {
        let before = installed(&data)?["current"].clone();
        post(&record, "/api/launcher-update/check")?;
        let status = finished(&record)?;
        require(
            status["latest_version"] == target && host::truthy(&status["can_install"]),
            format!("The update to {target} is not offered: {status}"),
        )?;
        call(
            &record,
            "/api/launcher-update/install",
            Some(&json!({"check_id": status["check_id"]})),
        )?;
        let status = finished(&record)?;
        ensure!(host::truthy(&status["pending_restart"]) && host::truthy(&status["can_restart"]));
        ensure!(installed(&data)?["previous"] == before);
        record = restart(&state, &record, target)?;
        hops.push(target.to_string());
    }
    // Crossing back to Python and then forward to native must retain the
    // native manager and a shell wrapper that no longer needs Python.
    for target in [inputs.bridge_version, inputs.native_version] {
        post(&record, "/api/launcher-update/rollback")?;
        let status = finished(&record)?;
        ensure!(host::truthy(&status["pending_restart"]) && host::truthy(&status["can_restart"]));
        record = restart(&state, &record, target)?;
        hops.push(target.to_string());
    }
    ensure!(shell_wrapper(&bin.join("flightdeck"))?);
    ensure!(fs::read(&marker)? == b"synthetic browser preference");
    ensure!(!output.join("unexpected-browser").exists());
    post(&record, "/api/desktop/refresh")?;
    Ok(hops)
}

/// The downloaded bridge's own installer and the terminal update. The first
/// command is Python; a successful update must leave a shell wrapper and
/// start the native desktop service.
fn terminal_update(
    inputs: &Inputs,
    output: &Path,
    env: &Environment,
    started: &mut Started,
) -> Result<()> {
    let cli = output.join("cli");
    fs::create_dir(&cli)?;
    extract(inputs.bridge, &cli.join("bridge"))?;
    extract(inputs.native, &cli.join("native"))?;
    let bridge_source = cli.join("bridge/flightdeck-linux");
    let native_source = cli.join("native/flightdeck-linux");
    let mut env = without(env, &["PYTHONPATH", "FLIGHTDECK_TRANSITION_FIXTURE"]);
    homes(&mut env, &cli);
    env.insert(
        "FLIGHTDECK_TEST_BROWSER_LOG".into(),
        cli.join("browser-opened").into(),
    );
    let (state, data, bin) = (
        cli.join("state/flightdeck"),
        cli.join("installed"),
        cli.join("bin"),
    );
    started.states.push(state.clone());
    let install = host::run(
        command(&bridge_source.join("install.sh"), &cli, &env)
            .arg("--data-dir")
            .arg(&data)
            .arg("--bin-dir")
            .arg(&bin)
            .arg("--applications-dir")
            .arg(cli.join("applications"))
            .args(["--no-desktop", "--no-launch", "--language", "en"]),
        None,
        90,
    )?;
    succeeded("The bridge installation", &install)?;
    let before = fs::read(data.join("installation.json"))?;
    let binary = native_source.join("bin/flightdeck");
    let original = fs::read(&binary)?;
    let mut modified = original.clone();
    *modified
        .last_mut()
        .ok_or(Failure("Empty native executable".into()))? ^= 1;
    fs::write(&binary, &modified)?;
    let update = || {
        host::run(
            command(&bin.join("flightdeck"), &cli, &env)
                .arg("--update")
                .arg(&native_source),
            None,
            90,
        )
    };
    let rejected = update()?;
    require(
        rejected.code != 0,
        "Reject the modified native executable before execution",
    )?;
    ensure!(fs::read(data.join("installation.json"))? == before);
    ensure!(!cli.join("browser-opened").exists());
    fs::write(&binary, &original)?;
    let updated = update()?;
    // The native window cannot open on a headless host. The terminal update
    // must still install the checked package and start its verified service.
    let errors = String::from_utf8_lossy(&updated.stderr);
    require(
        updated.code == 0
            || (updated.code == 1 && errors.contains("X11") && errors.contains("Wayland")),
        format!("The terminal update failed: {errors}"),
    )?;
    // Without a session to open its window in, a current launcher ends before
    // it starts its service; native releases before 0.2.8 left it running.
    // Either way the installed package must serve.
    let record = match wait_for(|| record_at(&state), Duration::from_secs(10)) {
        Ok(record) => record,
        Err(_) => {
            let log = fs::File::create(cli.join("service.log"))?;
            started.children.push(
                command(&bin.join("flightdeck"), &cli, &env)
                    .args(["--desktop-service", "--state-dir"])
                    .arg(&state)
                    .stdout(log.try_clone()?)
                    .stderr(log)
                    .spawn()?,
            );
            wait_for(|| record_at(&state), Duration::from_secs(60))?
        }
    };
    ensure!(call(&record, "/api/status", None)?["app"]["version"] == inputs.native_version);
    let previous: Value = serde_json::from_slice(&before)?;
    ensure!(installed(&data)?["previous"] == previous["current"]);
    ensure!(shell_wrapper(&bin.join("flightdeck"))?);
    require(
        !cli.join("browser-opened").exists(),
        "Native startup must not launch a browser",
    )?;
    post(&record, "/api/desktop/refresh")?;
    Ok(())
}

pub fn run(arguments: &Arguments) -> Result<u8> {
    let output = host::resolve(&arguments.output)?;
    host::new_dirs(&output, host::PRIVATE)?;
    let legacy = host::canonical(&arguments.legacy_package)?;
    let bridge = read_regular(&host::canonical(&arguments.bridge_archive)?, ARCHIVE_MAX)?;
    let native = read_regular(&host::canonical(&arguments.native_archive)?, ARCHIVE_MAX)?;
    let (bridge_version, native_version) = (version(&bridge, false)?, version(&native, true)?);
    require(
        bridge_version == "0.1.22",
        "The bridge archive is not the published 0.1.22",
    )?;
    host::write(
        &output.join("offer.json"),
        json!({"bridge": metadata(&bridge_version, &bridge),
            "native": metadata(&native_version, &native),
            "bridge_archive": host::canonical(&arguments.bridge_archive)?,
            "native_archive": host::canonical(&arguments.native_archive)?})
        .to_string(),
    )?;
    let hook = output.join("hook");
    fs::create_dir(&hook)?;
    host::write(&hook.join("sitecustomize.py"), HOOK)?;
    let browsers = output.join("fake-browser");
    fs::create_dir(&browsers)?;
    let browser = browsers.join("chromium");
    host::write(
        &browser,
        "#!/bin/sh\nprintf opened > \"$FLIGHTDECK_TEST_BROWSER_LOG\"\n",
    )?;
    fs::set_permissions(&browser, fs::Permissions::from_mode(0o700))?;
    let env = environment(std::env::vars_os().collect(), &output, &hook, &browsers);
    let inputs = Inputs {
        python: &arguments.python,
        legacy: &legacy,
        bridge: &bridge,
        native: &native,
        bridge_version: &bridge_version,
        native_version: &native_version,
    };
    let mut started = Started::default();
    let checked = service_updates(&inputs, &output, &env, &mut started).and_then(|hops| {
        terminal_update(&inputs, &output, &env, &mut started)?;
        Ok(hops)
    });
    started.stop();
    let report = Report {
        status: "PASS",
        initial_version: "0.1.21",
        hops: &checked?,
        same_browser_origin: true,
        settings_preserved: true,
        no_additional_browser: true,
        final_wrapper_requires_python: false,
        terminal_update: "PASS",
        native_window: "headless; checked separately",
        tampered_native_binary_rejected: true,
        bridge_bootstrap: "PASS",
        bridge_archive_sha256: digest(&bridge),
        native_archive_sha256: digest(&native),
        network: "Private GitHub fixtures; no account or game operations",
        published: false,
    };
    host::write(&output.join("result.json"), host::pretty(&report)?)?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::fixtures;

    #[test]
    fn a_package_states_a_stable_version_or_cannot_be_tested() {
        let python = |text: &str| {
            fixtures::files(&[("flightdeck-linux/flightdeck/__init__.py", text.as_bytes())])
        };
        assert_eq!(
            version(
                &python("\"\"\"Launcher.\"\"\"\n__version__ = \"0.1.22\"\n"),
                false
            )
            .expect("version"),
            "0.1.22"
        );
        let native = |text: &str| {
            fixtures::files(&[("flightdeck-linux/FLIGHTDECK-PACKAGE.json", text.as_bytes())])
        };
        assert_eq!(
            version(&native(r#"{"version": "0.2.11"}"#), true).expect("version"),
            "0.2.11"
        );
        // The stable channel never offers a candidate with a suffix.
        for archive in [
            native(r#"{"version": "0.3.0-rc.1"}"#),
            native(r#"{"schema": 1}"#),
            python("__version__ = \"0.1.22\"\n"),
        ] {
            assert!(version(&archive, true).is_err());
        }
        assert!(version(&python("version = 1\n"), false).is_err());
    }

    #[test]
    fn the_offer_describes_the_archive_as_a_published_release_would() {
        let offer = metadata("0.1.22", b"archive");
        assert_eq!(offer["tag_name"], "v0.1.22");
        assert_eq!(
            (offer["draft"].clone(), offer["prerelease"].clone()),
            (json!(false), json!(false))
        );
        let asset = &offer["assets"][0];
        assert_eq!(asset["size"], 7);
        assert_eq!(asset["digest"], format!("sha256:{}", digest(b"archive")));
        assert_eq!(
            asset["browser_download_url"],
            format!("{PROJECT}/releases/download/v0.1.22/Flightdeck-Linux-x86_64.tar.gz")
        );
        // The hook of the releases under test reads exactly these addresses.
        assert!(HOOK.contains(r#"values[name]["assets"][0]["browser_download_url"]"#));
    }

    #[test]
    fn the_releases_run_in_a_private_home_without_display_or_network() {
        let host = Environment::from([
            ("PATH".into(), "/usr/bin".into()),
            ("PYTHONHOME".into(), "/elsewhere".into()),
            ("DISPLAY".into(), ":0".into()),
            ("XDG_DATA_HOME".into(), "/data/of/the/host".into()),
        ]);
        let env = environment(
            host,
            Path::new("/out"),
            Path::new("/out/hook"),
            Path::new("/out/fake-browser"),
        );
        let value = |name: &str| env.get(OsStr::new(name)).and_then(|value| value.to_str());
        assert_eq!(value("PATH"), Some("/out/fake-browser:/usr/bin"));
        assert_eq!(value("PYTHONHOME"), None);
        assert_eq!(value("PYTHONPATH"), Some("/out/hook"));
        assert_eq!(
            (value("DISPLAY"), value("WAYLAND_DISPLAY")),
            (Some(""), Some(""))
        );
        assert_eq!(
            (value("HOME"), value("XDG_DATA_HOME")),
            (Some("/out"), Some("/out/data"))
        );
        assert_eq!(value("HTTPS_PROXY"), Some("http://127.0.0.1:1"));
        assert_eq!(
            value("FLIGHTDECK_TEST_BROWSER_LOG"),
            Some("/out/unexpected-browser")
        );
        // The installers run without the hook; the terminal update also without the offer.
        let plain = without(&env, &["PYTHONPATH", "FLIGHTDECK_TRANSITION_FIXTURE"]);
        assert!(!plain.contains_key(OsStr::new("PYTHONPATH")));
        assert!(!plain.contains_key(OsStr::new("FLIGHTDECK_TRANSITION_FIXTURE")));
        assert_eq!(plain.len(), env.len() - 2);
    }

    #[test]
    fn extraction_stays_below_its_directory() {
        let temp = tempfile::tempdir().expect("temp");
        let archive = fixtures::files(&[
            ("flightdeck-linux/install.sh", b"#!/bin/sh\n"),
            ("flightdeck-linux/flightdeck/__init__.py", b""),
        ]);
        let destination = temp.path().join("bridge");
        extract(&archive, &destination).expect("extract");
        assert_eq!(
            fs::read(destination.join("flightdeck-linux/install.sh")).expect("member"),
            b"#!/bin/sh\n"
        );
        // An earlier extraction is never mixed with a new one.
        assert!(extract(&archive, &destination).is_err());
        let escaping = fixtures::files(&[("../outside", b"x")]);
        assert!(extract(&escaping, &temp.path().join("other")).is_err());
        assert!(!temp.path().join("outside").exists());
    }

    #[test]
    fn the_result_keeps_the_keys_of_the_python_report() {
        let report = Report {
            status: "PASS",
            initial_version: "0.1.21",
            hops: &["0.1.22".to_string()],
            same_browser_origin: true,
            settings_preserved: true,
            no_additional_browser: true,
            final_wrapper_requires_python: false,
            terminal_update: "PASS",
            native_window: "headless; checked separately",
            tampered_native_binary_rejected: true,
            bridge_bootstrap: "PASS",
            bridge_archive_sha256: "b".into(),
            native_archive_sha256: "n".into(),
            network: "Private GitHub fixtures; no account or game operations",
            published: false,
        };
        let text = serde_json::to_string(&report).expect("report");
        assert!(text.starts_with(r#"{"status":"PASS","initial_version":"0.1.21","hops":["0.1.22"],"same_browser_origin":true,"#));
        assert!(text.ends_with(r#""network":"Private GitHub fixtures; no account or game operations","published":false}"#));
    }
}
