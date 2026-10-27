// SPDX-License-Identifier: MIT
//! Automatic migration of an inactive edition: a synthetic MSFS 2020 runtime
//! built with the previous package's pinned components must become ready when
//! it is selected in a real service of the new package, with the new scripts,
//! native components and helper in place and its synthetic save retained.
//! Everything lives in a temporary home; no game is started and no account
//! or network is used. It does not establish that a migrated game runs.
use super::host::{self, Api};
use crate::{
    ensure,
    util::{Failure, Result, digest, fail},
};
use serde_json::{Map, Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

#[derive(clap::Args)]
pub struct Arguments {
    #[arg(long)]
    package: PathBuf,
    #[arg(long)]
    previous_package: PathBuf,
}

/// Each native component of a package and where an imported runtime keeps it.
const MAPPING: [(&str, &[&str]); 6] = [
    ("bin/xodus-cli", &["bin/xodus-cli"]),
    ("bin/xodus-service", &["bin/xodus-service"]),
    (
        "bin/flightdeck-connected-storage.exe",
        &["bin/flightdeck-connected-storage.exe"],
    ),
    (
        "runtime/xgameruntime.dll",
        &["local/msfs-prefix/drive_c/windows/system32/xgameruntime.dll"],
    ),
    (
        "builtin/x86_64-windows/xodus_store_test.dll",
        &[
            "local/store-runtime/x86_64-windows/xodus_store_test.dll",
            "local/msfs-prefix/drive_c/windows/system32/xodus_store_test.dll",
        ],
    ),
    (
        "builtin/x86_64-unix/xodus_store_test.so",
        &["local/store-runtime/x86_64-unix/xodus_store_test.so"],
    ),
];
const SAVE: &[u8] = b"keep synthetic save";

fn hash(path: &Path) -> Result<String> {
    Ok(digest(&fs::read(path).map_err(|error| {
        Failure(format!("{}: {error}", path.display()))
    })?))
}

fn private(path: &Path) -> Result<()> {
    Ok(fs::set_permissions(
        path,
        fs::Permissions::from_mode(0o700),
    )?)
}

/// An edition's runtime as the launcher expects to find one, without any
/// managed component: the play script would fail and the game is a text file.
fn synthetic_runtime(root: &Path, edition: &str, executable: &str) -> Result<()> {
    let game = format!("games/{}", edition.to_uppercase());
    let system = "local/msfs-prefix/drive_c/windows/system32";
    for name in ["private", "tools", &game, system] {
        host::new_dirs(&root.join(name), host::PRIVATE)?;
    }
    host::write(
        &root.join("private/runtime.json"),
        json!({"game_id": edition}).to_string(),
    )?;
    host::write(&root.join("tools/play-msfs.sh"), "#!/bin/sh\nexit 99\n")?;
    private(&root.join("tools/play-msfs.sh"))?;
    host::write(
        &root.join(game).join(executable),
        "synthetic game; never executed\n",
    )?;
    host::write(
        &root.join("local/msfs-prefix/system.reg"),
        "synthetic registry\n",
    )?;
    host::write(
        &root.join(system).join("xgameruntime.dll"),
        "synthetic unmanaged component\n",
    )
}

/// Installs the previous package's runtime scripts and native components and
/// records them as its import did. The scripts must be a pinned migration
/// source of the new package, and the components must match their manifest.
fn previous_installation(previous: &Path, lock: &Value, older: &Path) -> Result<()> {
    let mut old_scripts = Map::new();
    for name in files(&lock["runtime_scripts"]["files"])?.keys() {
        let source = previous.join("scripts/runtime").join(name);
        old_scripts.insert(name.clone(), hash(&source)?.into());
        host::copyfile(&source, &older.join("tools").join(name))?;
        private(&older.join("tools").join(name))?;
    }
    let old_scripts = Value::Object(old_scripts);
    ensure!(
        lock["runtime_scripts"]["upgrade_from"]
            .as_array()
            .is_some_and(|supported| supported.contains(&old_scripts)),
        "previous scripts must be a supported migration"
    );
    let old_native = previous.join("flightdeck/resources/native");
    let manifest = host::read_json(&old_native.join("manifest.json"))?;
    for (name, destinations) in MAPPING {
        ensure!(
            manifest["files"][name] == hash(&old_native.join(name))?,
            "{name}"
        );
        for destination in destinations {
            let target = older.join(destination);
            if let Some(parent) = target.parent() {
                host::dirs(parent, host::PRIVATE)?;
            }
            host::copyfile(&old_native.join(name), &target)?;
            private(&target)?;
        }
    }
    host::write(
        &older.join("private/import-manifest.json"),
        json!({"format": 1, "artifacts": manifest, "runtime_files": old_scripts}).to_string(),
    )?;
    host::write(&older.join("private/synthetic-save.bin"), SAVE)
}

fn files(value: &Value) -> Result<&Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| Failure("The bootstrap lock lists no files".into()))
}

/// The port at the end of the service's first line.
fn announced_port(line: &str) -> Result<u16> {
    line.trim()
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .ok_or_else(|| Failure("Unexpected service startup output".into()))
}

/// The migrated runtime carries exactly what the new package pins.
fn migrated(package: &Path, lock: &Value, older: &Path) -> Result<()> {
    for (name, expected) in files(&lock["runtime_scripts"]["files"])? {
        ensure!(
            *expected == hash(&older.join("tools").join(name))?,
            "{name}"
        );
    }
    for (name, destinations) in MAPPING {
        for destination in destinations {
            ensure!(
                lock["native"]["files"][name] == hash(&older.join(destination))?,
                "{destination}"
            );
        }
    }
    ensure!(
        hash(&older.join("tools/flightdeck-helper"))? == hash(&package.join("bin/flightdeck"))?
    );
    ensure!(fs::read(older.join("private/synthetic-save.bin"))? == SAVE);
    ensure!(!older.join("private/component-update.json").exists());
    Ok(())
}

fn exercise(api: &Api, package: &Path, lock: &Value, older: &Path) -> Result<Value> {
    let before = api.request("/api/status", None, None)?;
    ensure!(before["runtime"]["game_id"] == "msfs2024");
    ensure!(before["versions"]["msfs2020"]["installed"] == true);
    ensure!(before["versions"]["msfs2020"]["ready"] == false);
    let csrf = before["csrf_token"]
        .as_str()
        .filter(|csrf| !csrf.is_empty());
    let select =
        |edition: &str| api.request("/api/game/select", Some(&json!({"game_id": edition})), csrf);
    let started = Instant::now();
    select("msfs2020")?;
    let elapsed = started.elapsed().as_secs_f64();
    let after = api.request("/api/status", None, None)?;
    ensure!(after["runtime"]["game_id"] == "msfs2020");
    ensure!(
        after["runtime"]["ready"] == true,
        "{}",
        after["runtime"]["checks"]
    );
    migrated(package, lock, older)?;
    select("msfs2024")?;
    select("msfs2020")?;
    ensure!(api.request("/api/status", None, None)?["runtime"]["ready"] == true);
    Ok(
        json!({"status": "PASS", "select_and_migrate_seconds": elapsed,
        "migration": "inactive MSFS 2020 becomes ready on selection", "game_or_account_calls": false}),
    )
}

pub fn run(arguments: &Arguments) -> Result<u8> {
    let package = host::canonical(&arguments.package)?;
    let previous = host::canonical(&arguments.previous_package)?;
    let lock = host::read_json(&package.join("compat/bootstrap.lock.json"))?;
    let temporary = tempfile::Builder::new()
        .prefix("flightdeck-edition-update-")
        .tempdir()?;
    let work = temporary.path();
    let (older, active, state) = (
        work.join("msfs2020"),
        work.join("msfs2024"),
        work.join("state"),
    );
    synthetic_runtime(&older, "msfs2020", "FlightSimulator.exe")?;
    synthetic_runtime(&active, "msfs2024", "FlightSimulator2024.exe")?;
    previous_installation(&previous, &lock, &older)?;
    host::new_dir(&state, host::PRIVATE)?;
    host::write(
        &state.join("config.json"),
        json!({"schema": 1, "runtime_path": host::text(&active),
            "runtimes": {"msfs2020": host::text(&older), "msfs2024": host::text(&active)}})
        .to_string(),
    )?;
    let mut service = Command::new(package.join("bin/flightdeck"));
    service
        .arg("--state-dir")
        .arg(&state)
        .arg("--no-browser")
        .env("HOME", work)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(fs::File::create(work.join("service.log"))?);
    for name in ["CONFIG", "DATA", "CACHE", "STATE"] {
        service.env(format!("XDG_{name}_HOME"), work.join(name.to_lowercase()));
    }
    let mut child = service
        .spawn()
        .map_err(|error| Failure(format!("{}: {error}", package.display())))?;
    let result = (|| {
        let line = host::first_line(&mut child, 20, "service startup")?;
        let api = Api {
            port: announced_port(&line)?,
            token: host::read_json(&state.join("desktop-service.json"))?["token"]
                .as_str()
                .ok_or_else(|| Failure("The service record has no token".into()))?
                .to_string(),
            seconds: 30,
        };
        exercise(&api, &package, &lock, &older)
    })();
    host::interrupt(&child);
    let stopped = if host::exited_within(&mut child, 10) {
        Ok(())
    } else {
        // The Python original left such a service running. Its temporary home
        // is removed below, so nothing useful could come of it.
        let _ = child.kill();
        let _ = child.wait();
        fail("The service did not stop within 10 seconds")
    };
    println!("{}", host::finally(result, stopped)?);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fixtures::Service;
    use std::{
        os::unix::fs::MetadataExt,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").mode() & 0o777
    }

    /// A previous and a new package that differ in every managed file, and
    /// the lock of the new one. `supported` pins the previous scripts.
    fn packages(root: &Path, supported: bool) -> (PathBuf, PathBuf, Value) {
        let (previous, package) = (root.join("previous"), root.join("package"));
        let native = previous.join("flightdeck/resources/native");
        let (mut old_native, mut new_native) = (Map::new(), Map::new());
        for (name, _) in MAPPING {
            let path = native.join(name);
            fs::create_dir_all(path.parent().expect("parent")).expect("directory");
            fs::write(&path, format!("old {name}")).expect("component");
            old_native.insert(name.into(), digest(format!("old {name}").as_bytes()).into());
            new_native.insert(name.into(), digest(format!("new {name}").as_bytes()).into());
        }
        fs::write(
            native.join("manifest.json"),
            json!({"files": old_native}).to_string(),
        )
        .expect("manifest");
        let (mut old_scripts, mut new_scripts) = (Map::new(), Map::new());
        fs::create_dir_all(previous.join("scripts/runtime")).expect("scripts");
        for name in ["play-msfs.sh", "xodus.sh"] {
            fs::write(
                previous.join("scripts/runtime").join(name),
                format!("old {name}"),
            )
            .expect("script");
            old_scripts.insert(name.into(), digest(format!("old {name}").as_bytes()).into());
            new_scripts.insert(name.into(), digest(format!("new {name}").as_bytes()).into());
        }
        fs::create_dir_all(package.join("bin")).expect("package");
        fs::write(package.join("bin/flightdeck"), "new launcher").expect("launcher");
        let upgrade_from = if supported {
            json!([{"play-msfs.sh": "other"}, old_scripts])
        } else {
            json!([{"play-msfs.sh": "other"}])
        };
        let lock = json!({"runtime_scripts": {"files": new_scripts, "upgrade_from": upgrade_from},
            "native": {"files": new_native}});
        (previous, package, lock)
    }

    /// What the service's migration leaves in the older runtime.
    fn migrate(package: &Path, older: &Path) {
        for name in ["play-msfs.sh", "xodus.sh"] {
            fs::write(older.join("tools").join(name), format!("new {name}")).expect("script");
        }
        for (name, destinations) in MAPPING {
            for destination in destinations {
                fs::write(older.join(destination), format!("new {name}")).expect("component");
            }
        }
        fs::copy(
            package.join("bin/flightdeck"),
            older.join("tools/flightdeck-helper"),
        )
        .expect("helper");
    }

    #[test]
    fn a_synthetic_runtime_is_private_and_cannot_start_a_game() {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().join("msfs2020");
        synthetic_runtime(&root, "msfs2020", "FlightSimulator.exe").expect("runtime");
        assert_eq!(
            fs::read_to_string(root.join("private/runtime.json")).expect("identity"),
            r#"{"game_id":"msfs2020"}"#
        );
        assert_eq!(
            fs::read_to_string(root.join("tools/play-msfs.sh")).expect("script"),
            "#!/bin/sh\nexit 99\n"
        );
        assert_eq!(mode(&root.join("tools/play-msfs.sh")), 0o700);
        for directory in ["private", "tools", "games/MSFS2020"] {
            assert_eq!(mode(&root.join(directory)), 0o700, "{directory}");
        }
        assert_eq!(
            fs::read_to_string(root.join("games/MSFS2020/FlightSimulator.exe")).expect("game"),
            "synthetic game; never executed\n"
        );
        assert!(root.join("local/msfs-prefix/system.reg").is_file());
        assert!(
            root.join("local/msfs-prefix/drive_c/windows/system32/xgameruntime.dll")
                .is_file()
        );
        assert!(synthetic_runtime(&root, "msfs2020", "FlightSimulator.exe").is_err());
    }

    #[test]
    fn the_previous_components_are_installed_and_recorded_like_an_import() {
        let temp = tempfile::tempdir().expect("temp");
        let (previous, package, lock) = packages(temp.path(), true);
        let older = temp.path().join("msfs2020");
        synthetic_runtime(&older, "msfs2020", "FlightSimulator.exe").expect("runtime");
        previous_installation(&previous, &lock, &older).expect("installation");
        for (name, destinations) in MAPPING {
            for destination in destinations {
                assert_eq!(
                    fs::read_to_string(older.join(destination)).expect(destination),
                    format!("old {name}")
                );
                assert_eq!(mode(&older.join(destination)), 0o700, "{destination}");
            }
        }
        assert_eq!(
            fs::read_to_string(older.join("tools/play-msfs.sh")).expect("script"),
            "old play-msfs.sh"
        );
        let record = host::read_json(&older.join("private/import-manifest.json")).expect("record");
        assert_eq!(record["format"], 1);
        assert_eq!(
            record["artifacts"],
            host::read_json(&previous.join("flightdeck/resources/native/manifest.json"))
                .expect("manifest")
        );
        assert_eq!(
            record["runtime_files"],
            lock["runtime_scripts"]["upgrade_from"][1]
        );
        assert_eq!(
            fs::read(older.join("private/synthetic-save.bin")).expect("save"),
            SAVE
        );
        // Before the service has migrated it, the runtime does not match.
        assert!(migrated(&package, &lock, &older).is_err());
        migrate(&package, &older);
        migrated(&package, &lock, &older).expect("migrated");
        fs::write(older.join("private/component-update.json"), "{}").expect("pending");
        assert!(migrated(&package, &lock, &older).is_err());
        fs::remove_file(older.join("private/component-update.json")).expect("remove");
        fs::write(older.join("private/synthetic-save.bin"), "lost").expect("save");
        assert!(migrated(&package, &lock, &older).is_err());
    }

    #[test]
    fn unsupported_scripts_and_changed_components_are_refused() {
        let temp = tempfile::tempdir().expect("temp");
        let (previous, _, lock) = packages(temp.path(), false);
        let older = temp.path().join("msfs2020");
        synthetic_runtime(&older, "msfs2020", "FlightSimulator.exe").expect("runtime");
        let error = previous_installation(&previous, &lock, &older).expect_err("unsupported");
        assert!(
            error
                .0
                .ends_with("previous scripts must be a supported migration"),
            "{}",
            error.0
        );

        let (previous, _, lock) = packages(&temp.path().join("changed"), true);
        fs::write(
            previous.join("flightdeck/resources/native/bin/xodus-cli"),
            "replaced",
        )
        .expect("component");
        let older = temp.path().join("changed/msfs2020");
        synthetic_runtime(&older, "msfs2020", "FlightSimulator.exe").expect("runtime");
        let error = previous_installation(&previous, &lock, &older).expect_err("changed");
        assert!(error.0.ends_with("bin/xodus-cli"), "{}", error.0);
    }

    #[test]
    fn the_port_is_read_from_the_end_of_the_first_line() {
        assert_eq!(
            announced_port("Flightdeck: http://127.0.0.1:40123\n").expect("port"),
            40123
        );
        for line in ["", "Flightdeck\n", "Flightdeck: http://127.0.0.1:port\n"] {
            assert!(announced_port(line).is_err(), "{line:?}");
        }
    }

    #[test]
    fn selecting_the_inactive_edition_must_migrate_it_and_survive_switching() {
        let temp = tempfile::tempdir().expect("temp");
        let (previous, package, lock) = packages(temp.path(), true);
        let older = temp.path().join("msfs2020");
        synthetic_runtime(&older, "msfs2020", "FlightSimulator.exe").expect("runtime");
        previous_installation(&previous, &lock, &older).expect("installation");
        let run = |migrates: bool| {
            let selected = Arc::new(AtomicBool::new(false));
            let (new_package, runtime) = (package.clone(), older.clone());
            let service = Service::start(move |method, path, headers, body| {
                let token = headers
                    .get("x-flightdeck-token")
                    .map(|values| values[0].as_str());
                match (method, path, token) {
                    ("GET", "/api/status", Some("session")) => {
                        let active = selected.load(Ordering::SeqCst);
                        (
                            200,
                            json!({"csrf_token": "csrf",
                            "runtime": {"game_id": if active { "msfs2020" } else { "msfs2024" },
                                "ready": true, "checks": []},
                            "versions": {"msfs2020": {"installed": true, "ready": false}}}),
                        )
                    }
                    ("POST", "/api/game/select", Some("csrf")) => {
                        let edition: Value = serde_json::from_slice(body).expect("selection");
                        selected.store(edition["game_id"] == "msfs2020", Ordering::SeqCst);
                        if migrates {
                            migrate(&new_package, &runtime);
                        }
                        (200, json!({}))
                    }
                    _ => (403, json!({})),
                }
            });
            let api = Api {
                port: service.port,
                token: "session".into(),
                seconds: 5,
            };
            exercise(&api, &package, &lock, &older)
        };
        assert!(run(false).is_err());
        let result = run(true).expect("pass");
        assert_eq!(result["status"], "PASS");
        assert_eq!(result["game_or_account_calls"], false);
        assert_eq!(
            result["migration"],
            "inactive MSFS 2020 becomes ready on selection"
        );
        assert!(
            result["select_and_migrate_seconds"]
                .as_f64()
                .expect("seconds")
                >= 0.0
        );
    }

    #[test]
    fn the_packaged_service_runs_in_a_private_home_and_is_stopped() {
        let temp = tempfile::tempdir().expect("temp");
        let (previous, package, lock) = packages(temp.path(), true);
        fs::create_dir_all(package.join("compat")).expect("compat");
        fs::write(package.join("compat/bootstrap.lock.json"), lock.to_string()).expect("lock");
        let notes = temp.path().join("notes");
        fs::create_dir(&notes).expect("notes");
        let selected = Arc::new(AtomicBool::new(false));
        let (new_package, seen) = (package.clone(), notes.clone());
        let service = Service::start(move |method, path, headers, body| {
            let token = headers
                .get("x-flightdeck-token")
                .map(|values| values[0].as_str());
            match (method, path, token) {
                ("GET", "/api/status", Some("session")) => {
                    let active = selected.load(Ordering::SeqCst);
                    (
                        200,
                        json!({"csrf_token": "csrf",
                        "runtime": {"game_id": if active { "msfs2020" } else { "msfs2024" },
                            "ready": true, "checks": []},
                        "versions": {"msfs2020": {"installed": true, "ready": false}}}),
                    )
                }
                ("POST", "/api/game/select", Some("csrf")) => {
                    let edition: Value = serde_json::from_slice(body).expect("selection");
                    selected.store(edition["game_id"] == "msfs2020", Ordering::SeqCst);
                    // Like the real service, find the runtime in the configuration.
                    let state = fs::read_to_string(seen.join("state")).expect("state");
                    let config =
                        host::read_json(&Path::new(&state).join("config.json")).expect("config");
                    let older = config["runtimes"]["msfs2020"].as_str().expect("runtime");
                    migrate(&new_package, Path::new(older));
                    (200, json!({}))
                }
                _ => (403, json!({})),
            }
        });
        crate::probes::fixtures::script(
            &package.join("bin/flightdeck"),
            &format!(
                "printf '%s' \"$2\" > '{notes}/state'\n\
                 printf '%s\\n' \"$HOME\" \"$XDG_CONFIG_HOME\" \"$3\" \"$$\" > '{notes}/started'\n\
                 printf '%s' '{{\"token\":\"session\"}}' > \"$2/desktop-service.json\"\n\
                 echo 'Flightdeck: http://127.0.0.1:{port}'\nexec sleep 60",
                notes = notes.display(),
                port = service.port
            ),
        );
        let arguments = Arguments {
            package,
            previous_package: previous,
        };
        assert_eq!(super::run(&arguments).expect("probe"), 0);
        let started = fs::read_to_string(notes.join("started")).expect("started");
        let started: Vec<&str> = started.lines().collect();
        let work = Path::new(started[0]);
        let name = work.file_name().expect("name").to_string_lossy();
        assert!(name.starts_with("flightdeck-edition-update-"), "{name}");
        assert_eq!(Path::new(started[1]), work.join("config"));
        assert_eq!(started[2], "--no-browser");
        // The service was interrupted and its temporary home removed.
        assert!(!Path::new("/proc").join(started[3]).exists());
        assert!(!work.exists());
    }
}
