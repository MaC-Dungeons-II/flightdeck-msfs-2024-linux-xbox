// SPDX-License-Identifier: MIT
#![allow(clippy::unwrap_used)]
use flightdeck::{backend::Launcher, fenix, fenix_bundle, files, proton, transaction as tx};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
};
fn write(path: &Path, data: &[u8]) {
    files::private_dir(path.parent().unwrap()).unwrap();
    files::atomic(path, data).unwrap();
}
/// Small AMD64 PE with a real export directory. Its private copy must keep
/// every byte except the module's internal export name.
fn json_dll(version: &str, name: &str) -> Vec<u8> {
    let mut data = vec![0; 0x800];
    data[..2].copy_from_slice(b"MZ");
    for (offset, value) in [
        (60, 0x80_u32),
        (0x80, 0x4550),
        (0x98 + 108, 16),
        (0x98 + 112, 0x1000),
        (0x98 + 116, 0x100),
        (0x188 + 12, 0x1000),
        (0x188 + 16, 0x600),
        (0x188 + 20, 0x200),
        (0x200 + 12, 0x1040),
        (0x200 + 20, 1),
        (0x200 + 24, 1),
        (0x200 + 28, 0x1028),
        (0x200 + 32, 0x102c),
        (0x200 + 36, 0x1030),
        (0x228, 0x1100),
        (0x22c, 0x1060),
    ] {
        data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    for (offset, value) in [(0x84, 0x8664_u16), (0x86, 1), (0x94, 240), (0x98, 0x20b)] {
        data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    data[0x240..0x240 + name.len()].copy_from_slice(name.as_bytes());
    let factory = b"DllGetActivationFactory\0";
    data[0x260..0x260 + factory.len()].copy_from_slice(factory);
    data[0x300] = 0xc3;
    data[0x400..0x400 + version.len()].copy_from_slice(version.as_bytes());
    data
}
fn runner(path: &Path, version: &str) {
    for name in proton::JSON_LIBRARIES {
        write(
            &path.join("files/lib/wine/x86_64-windows").join(name),
            &json_dll(version, name),
        );
    }
    for name in ["wine", "wineserver"] {
        let target = path.join("files/bin").join(name);
        write(&target, b"#!/bin/sh\nexit 0\n");
        fs::set_permissions(target, fs::Permissions::from_mode(0o700)).unwrap();
    }
    write(&path.join("version"), version.as_bytes());
    for arch in ["x86_64-windows", "i386-windows"] {
        for (lib, names) in [
            ("dxvk", &["dxgi", "d3d11", "d3d10core"][..]),
            ("vkd3d-proton", &["d3d12", "d3d12core"][..]),
        ] {
            for name in names {
                write(
                    &path.join(format!("files/lib/wine/{lib}/{arch}/{name}.dll")),
                    format!("{version}/{name}").as_bytes(),
                );
            }
        }
    }
}
fn fixture(base: &Path) -> (PathBuf, std::sync::Arc<Launcher>) {
    let root = base.join("runtime");
    let original = base.join("original-runner");
    runner(&original, "synthetic-original");
    write(&root.join("tools/play-msfs.sh"), b"#!/bin/sh\n");
    write(
        &root.join("tools/launch-msfs.sh"),
        flightdeck::resources::asset("scripts/runtime/launch-msfs.sh").unwrap(),
    );
    for name in ["system.reg", "user.reg"] {
        write(
            &root.join("local/msfs-prefix").join(name),
            b"original registry",
        );
    }
    files::private_dir(&root.join("local/msfs-prefix/drive_c/windows/syswow64")).unwrap();
    files::private_dir(&root.join("private")).unwrap();
    let system = root.join("local/msfs-prefix/drive_c/windows/system32");
    for name in proton::BRIDGE {
        write(&system.join(name), name.as_bytes());
    }
    let hashes: Value = proton::BRIDGE
        .into_iter()
        .map(|v| (v.to_string(), json!(files::sha256(v.as_bytes()))))
        .collect();
    files::atomic_json(&root.join("private/import-manifest.json"),&json!({"artifacts":{"files":{"runtime/xgameruntime.dll":hashes["xgameruntime.dll"],"builtin/x86_64-windows/xodus_store_test.dll":hashes["xodus_store_test.dll"]}},"original_runtime_sha256":hashes["xgameruntime_original.dll"]})).unwrap();
    symlink(&original, root.join("runner")).unwrap();
    let app = Launcher::new(base.join("state"), Some(root.to_str().unwrap())).unwrap();
    (root, app)
}
#[test]
fn current_native_wrapper_allows_the_public_proton_switch_without_fenix() {
    let temp = tempfile::tempdir().unwrap();
    let (root, app) = fixture(temp.path());
    let candidate = temp.path().join("alternative");
    runner(&candidate, "synthetic-selected");
    assert!(proton::supports_switch(&root).unwrap());
    proton::start(
        &app,
        &json!({"runtime_path":root,"mode":"proton","path":candidate}),
    )
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let job = app.job("proton");
        if job["state"] == "complete" {
            break;
        }
        assert_ne!(job["state"], "failed", "{job}");
        assert!(std::time::Instant::now() < deadline, "{job}");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(proton::snapshot(&app)["selected"], "synthetic-selected");
    for (source, target) in proton::JSON_LIBRARIES
        .into_iter()
        .zip(["fd-json.dll", "fd-types.dll"])
    {
        assert_eq!(
            fs::read(
                root.join("local/msfs-prefix/drive_c/windows/system32")
                    .join(target)
            )
            .unwrap(),
            json_dll("synthetic-original", target)
        );
        assert_eq!(
            fs::read(
                root.join("runner/files/lib/wine/x86_64-windows")
                    .join(source)
            )
            .unwrap(),
            json_dll("synthetic-selected", source)
        );
        assert_eq!(
            fs::read(candidate.join("files/lib/wine/x86_64-windows").join(source)).unwrap(),
            json_dll("synthetic-selected", source)
        );
    }
    write(
        &root.join("tools/launch-msfs.sh"),
        b"#!/bin/sh\necho unknown legacy launcher\n",
    );
    assert!(!proton::supports_switch(&root).unwrap());
    write(
        &root.join("tools/launch-msfs.sh"),
        b"#!/bin/sh\nexport FLIGHTDECK_PROTON_LOADER=portable\n",
    );
    assert!(proton::supports_switch(&root).unwrap());
}
#[test]
fn existing_selected_profiles_gain_json_support_without_replacing_runner_dlls() {
    let temp = tempfile::tempdir().unwrap();
    let (root, app) = fixture(temp.path());
    let candidate = temp.path().join("alternative");
    runner(&candidate, "synthetic-selected");
    let ctx = app.reserve("proton", "select", true).unwrap();
    let (fresh, selected) = proton::prepare(
        &ctx,
        &proton::inspect(candidate.to_str().unwrap()).unwrap(),
        None,
        false,
    )
    .unwrap();
    proton::switch(&ctx, &fresh, &selected, false).unwrap();
    app.finish(&ctx, Ok(json!({"state":"complete"})), false);
    let prefix = root.join("local/msfs-prefix");
    let json = prefix.join("drive_c/windows/system32/fd-json.dll");
    fs::remove_file(prefix.join(".flightdeck-json.json")).unwrap();
    fs::write(&json, b"old private copy").unwrap();
    proton::prepare_game_runtime(&root).unwrap();
    assert_eq!(
        fs::read(&json).unwrap(),
        json_dll("synthetic-original", "fd-json.dll")
    );
    assert_eq!(
        fs::read(root.join("runner/files/lib/wine/x86_64-windows/windows.web.dll")).unwrap(),
        json_dll("synthetic-selected", "windows.web.dll")
    );
    // A subsequent Wine update may restore stock WinRT registrations. The
    // marker alone must not make those stub factories appear repaired.
    fs::write(prefix.join("system.reg"), b"stock registrations").unwrap();
    proton::prepare_game_runtime(&root).unwrap();
    assert!(prefix.join(".flightdeck-json.json").is_file());
    let original_json = temp
        .path()
        .join("original-runner/files/lib/wine/x86_64-windows/windows.web.dll");
    for offset in [60, 0x84, 0x188 + 20, 0x200 + 12, 0x228, 0x240, 0x260] {
        let mut invalid = json_dll("synthetic-original", "windows.web.dll");
        invalid[offset..offset + 4].fill(0xff);
        fs::write(&original_json, invalid).unwrap();
        fs::write(&json, b"must remain on validation failure").unwrap();
        assert!(
            proton::prepare_game_runtime(&root).is_err(),
            "offset {offset}"
        );
        assert_eq!(
            fs::read(&json).unwrap(),
            b"must remain on validation failure"
        );
    }
    fs::write(
        &original_json,
        json_dll("synthetic-original", "windows.web.dll"),
    )
    .unwrap();
    fs::remove_file(
        temp.path()
            .join("original-runner/files/lib/wine/x86_64-windows/wintypes.dll"),
    )
    .unwrap();
    fs::write(&json, b"must remain on validation failure").unwrap();
    assert!(proton::prepare_game_runtime(&root).is_err());
    assert_eq!(
        fs::read(&json).unwrap(),
        b"must remain on validation failure"
    );
}
#[test]
fn discovery_returns_displayable_labels_for_steam_and_custom_runners() {
    let temp = tempfile::tempdir().unwrap();
    let common = temp.path().join(".local/share/Steam/steamapps/common");
    let custom = temp.path().join(".local/share/Steam/compatibilitytools.d");
    runner(
        &common.join("Proton - Experimental"),
        "experimental-11.0-test",
    );
    runner(
        &custom.join("proton-cachyos-10.0-sunset"),
        "cachyos-10.0-sunset",
    );
    runner(&custom.join("Proton für Tests"), "synthetic-unicode");
    let choices = proton::discover(temp.path());
    for name in [
        "Proton - Experimental",
        "proton-cachyos-10.0-sunset",
        "Proton für Tests",
    ] {
        let item = choices
            .iter()
            .find(|item| item["label"] == name)
            .expect("runner label is a JSON string");
        assert!(item["path"].as_str().unwrap().ends_with(name));
        assert!(item["version"].is_string());
        assert!(item["fenix"].is_boolean());
    }
}
#[test]
fn repeated_switch_and_default_restore_carry_forward_new_addons() {
    let temp = tempfile::tempdir().unwrap();
    let (root, app) = fixture(temp.path());
    let candidate = temp.path().join("new-runner");
    runner(&candidate, "synthetic-alternative");
    let baseline = proton::inspect(
        root.join("runner")
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let ctx = app.reserve("proton", "select", true).unwrap();
    let (fresh, selected) = proton::prepare(
        &ctx,
        &proton::inspect(candidate.to_str().unwrap()).unwrap(),
        None,
        false,
    )
    .unwrap();
    proton::switch(&ctx, &fresh, &selected, false).unwrap();
    app.finish(&ctx, Ok(json!({"state":"complete"})), false);
    let backup = root.join(selected["base_prefix"].as_str().unwrap());
    assert!(backup.join("system.reg").is_file());
    assert!(proton::selection(&root).unwrap().is_some());
    assert_eq!(proton::snapshot(&app)["selected_path"], json!(candidate));
    let addon = "local/msfs-prefix/drive_c/Program Files/SyntheticAddon/after-switch.dat";
    write(&root.join(addon), b"user add-on after first switch");
    let ctx = app.reserve("proton", "select", true).unwrap();
    let (fresh, selected2) = proton::prepare(
        &ctx,
        &proton::inspect(candidate.to_str().unwrap()).unwrap(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(selected2["base_prefix"], selected["base_prefix"]);
    proton::switch(&ctx, &fresh, &selected2, false).unwrap();
    app.finish(&ctx, Ok(json!({"state":"complete"})), false);
    assert_eq!(
        fs::read(root.join(addon)).unwrap(),
        b"user add-on after first switch"
    );
    let ctx = app.reserve("proton", "restore", true).unwrap();
    let (fresh, prepared) = proton::prepare(&ctx, &baseline, None, true).unwrap();
    proton::switch(&ctx, &fresh, &prepared, true).unwrap();
    assert!(proton::selection(&root).unwrap().is_none());
    assert_eq!(
        fs::read(root.join(addon)).unwrap(),
        b"user add-on after first switch"
    );
    for name in proton::BRIDGE {
        assert_eq!(
            tx::digest(
                &root
                    .join("local/msfs-prefix/drive_c/windows/system32")
                    .join(name)
            )
            .unwrap(),
            files::sha256(name.as_bytes())
        );
    }
    assert!(backup.exists());
}
fn journal(root: &Path) -> (PathBuf, Value) {
    let work = root.join("local/proton-tests/0123456789abcdef0123456789abcdef");
    let prefix = work.join("previous-prefix");
    files::private_dir(&prefix).unwrap();
    write(&prefix.join("probe"), b"new profile");
    runner(&work.join("runner"), "synthetic-new");
    let selected = json!({"schema":1,"version":"synthetic-new","base_runner":root.join("runner").canonicalize().unwrap(),"runner":"local/proton-tests/0123456789abcdef0123456789abcdef/runner","base_prefix":"local/proton-tests/0123456789abcdef0123456789abcdef/previous-prefix","base_hashes":proton::bridge_hashes(root).unwrap()});
    let data = json!({"schema":1,"backup":selected["base_prefix"],"runner":work.join("runner"),"previous_runner":root.join("runner").canonicalize().unwrap(),"before":fenix::identity(&root.join("local/msfs-prefix")).unwrap(),"after":fenix::identity(&prefix).unwrap(),"selection":selected,"fenix_state":null,"scripts":{}});
    files::atomic_json(&root.join("private/proton-switch.json"), &data).unwrap();
    (prefix, data)
}
#[test]
fn interrupted_exchange_recovers_idempotently_before_or_after_directory_swap() {
    for exchanged in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (root, _) = fixture(temp.path());
        let (backup, data) = journal(&root);
        if exchanged {
            tx::exchange(&root.join("local/msfs-prefix"), &backup).unwrap();
        }
        proton::recover(&root).unwrap();
        proton::recover(&root).unwrap();
        assert_eq!(
            fenix::identity(&root.join("local/msfs-prefix")).unwrap(),
            data["after"]
        );
        assert_eq!(fenix::identity(&backup).unwrap(), data["before"]);
        assert!(proton::selection(&root).unwrap().is_some());
        assert!(!root.join("private/proton-switch.json").exists());
    }
}
#[test]
fn changed_prefix_or_launch_script_stops_recovery_before_publication() {
    let temp = tempfile::tempdir().unwrap();
    let (root, _) = fixture(temp.path());
    let (backup, mut data) = journal(&root);
    let before = fenix::identity(&root.join("local/msfs-prefix")).unwrap();
    data["before"] = json!([0, 0]);
    files::atomic_json(&root.join("private/proton-switch.json"), &data).unwrap();
    assert!(proton::recover(&root).is_err());
    assert_eq!(
        fenix::identity(&root.join("local/msfs-prefix")).unwrap(),
        before
    );
    data["before"] = before.clone();
    let source = backup.parent().unwrap().join("launch-msfs.sh");
    write(&source, fenix_bundle::script("launch-msfs.sh").unwrap());
    // The native wrapper is now the normal fixture; replace it to simulate
    // an external edit that matches neither side of the recovery journal.
    write(
        &root.join("tools/launch-msfs.sh"),
        b"#!/bin/sh\necho externally modified\n",
    );
    data["scripts"] = json!({"launch-msfs.sh":{"before":"0".repeat(64),"after":tx::digest(&source).unwrap(),"source":source.strip_prefix(&root).unwrap()}});
    files::atomic_json(&root.join("private/proton-switch.json"), &data).unwrap();
    assert!(proton::recover(&root).is_err());
    assert_eq!(
        fenix::identity(&root.join("local/msfs-prefix")).unwrap(),
        before
    );
}
#[test]
fn current_loader_is_independent_of_the_older_fenix_archive() {
    let bytes = fenix_bundle::script("xodus-wine-launch").unwrap();
    assert!(String::from_utf8_lossy(bytes).contains("flightdeck-helper\" wine-launch"));
    assert!(!String::from_utf8_lossy(bytes).contains("python"));
    assert_ne!(
        files::sha256(bytes),
        fenix_bundle::manifest(None).unwrap()["integration"]["xodus-wine-launch"]
    );
}
#[test]
fn runner_escapes_and_ambiguous_selection_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("runner");
    runner(&path, "test");
    let file = path.join("files/bin/wine");
    fs::remove_file(&file).unwrap();
    symlink("/bin/sh", &file).unwrap();
    assert!(proton::inspect(path.to_str().unwrap()).is_err());
    assert!(proton::validate_selection(&json!({"schema":true})).is_err());
}
