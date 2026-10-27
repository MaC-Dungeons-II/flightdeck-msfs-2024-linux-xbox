// SPDX-License-Identifier: MIT
//! Renderer capability and settings migrations without Wine or a simulator.
use flightdeck::{
    fenix_bundle,
    fenix_setup::{self, DisplayRenderer},
    files, xml,
};
use serde_json::{Value, json};
use std::{fs, path::Path, sync::atomic::AtomicBool};

fn write(path: &Path, bytes: &[u8]) {
    files::private_dir(path.parent().expect("parent")).expect("directory");
    files::atomic(path, bytes).expect("write");
}

fn gpu_manifest() -> Value {
    let mut manifest = json!({"display_renderer":"GPU", "dxvk":{"revision":"fixture"}, "files":{}});
    for name in [
        "files/lib/wine/x86_64-windows/ntdll.dll",
        "files/lib/wine/x86_64-unix/ntdll.so",
        "files/bin/wineserver",
        "files/lib/wine/x86_64-windows/gdi32.dll",
        "files/lib/wine/x86_64-windows/win32u.dll",
        "files/lib/wine/x86_64-unix/win32u.so",
        "files/lib/wine/x86_64-windows/wow64win.dll",
        fenix_setup::DXGI_FILE,
    ] {
        manifest["files"][name] = json!("0".repeat(64));
    }
    manifest
}

fn prefix(root: &Path) {
    write(&root.join(fenix_setup::PROGRAM).join("Fenix.exe"), b"MZ");
    write(
        &root.join(fenix_setup::CONFIG).join("fenixConfig.xml"),
        b"<Settings><displayMode>CPU</displayMode><custom>keep</custom></Settings>",
    );
    write(
        &root.join(fenix_setup::CONFIG).join("persistancy.xml"),
        b"<Settings><fcuReadoutsType>1</fcuReadoutsType></Settings>",
    );
    files::private_dir(
        &root.join("drive_c/users/pilot/AppData/Roaming/Microsoft Flight Simulator 2024"),
    )
    .expect("simulator settings");
}

#[test]
fn adopting_autostart_retains_existing_renderer_and_fcu_preferences_byte_for_byte() {
    let temp = tempfile::tempdir().expect("fixture");
    let root = temp.path().join("prefix");
    prefix(&root);
    let config = root.join(fenix_setup::CONFIG).join("fenixConfig.xml");
    let persisted = root.join(fenix_setup::CONFIG).join("persistancy.xml");
    let before = fs::read(&config).expect("CPU preference");
    let fcu = fs::read(&persisted).expect("FCU preference");
    let autostart =
        root.join("drive_c/users/pilot/AppData/Roaming/Microsoft Flight Simulator 2024/exe.xml");
    write(&autostart,b"<SimBase.Document><Launch.Addon><Name>Other companion</Name><Path>other.exe</Path></Launch.Addon></SimBase.Document>");
    fenix_setup::configure_autostart(&root).expect("Fenix auto-start entry");
    assert_eq!(fs::read(config).expect("retained CPU preference"), before);
    assert_eq!(fs::read(persisted).expect("retained FCU preference"), fcu);
    let xml = fs::read_to_string(autostart).expect("auto-start XML");
    assert!(xml.contains("other.exe"));
    assert!(xml.contains("FenixBootstrapper.exe"));
}

#[test]
fn gpu_capability_is_complete_and_specific_to_each_variant() {
    assert_eq!(
        DisplayRenderer::from_manifest(&json!({})).expect("legacy"),
        DisplayRenderer::Cpu
    );
    assert!(
        DisplayRenderer::from_manifest(&json!({"display_renderer":"GPU", "files":{}})).is_err()
    );
    let mut lock = gpu_manifest();
    assert_eq!(
        DisplayRenderer::from_manifest(&lock).expect("GPU"),
        DisplayRenderer::Gpu
    );
    lock["variants"] = json!({
        "legacy":{"files":{}},
        "unpinned":{"display_renderer":"GPU", "files":lock["files"]},
        "new-dxvk":{"dxvk":lock["dxvk"], "files":lock["files"]},
        "gpu":{"display_renderer":"GPU", "dxvk":lock["dxvk"], "files":lock["files"]},
    });
    let legacy = fenix_bundle::select_variant(lock.clone(), Some("legacy")).expect("legacy");
    assert!(legacy.get("dxvk").is_none());
    assert!(legacy.get("display_renderer").is_none());
    assert_eq!(
        DisplayRenderer::from_manifest(&legacy).expect("CPU"),
        DisplayRenderer::Cpu
    );
    let unpinned = fenix_bundle::select_variant(lock.clone(), Some("unpinned")).expect("variant");
    assert!(DisplayRenderer::from_manifest(&unpinned).is_err());
    let unverified = fenix_bundle::select_variant(lock.clone(), Some("new-dxvk")).expect("variant");
    assert_eq!(
        DisplayRenderer::from_manifest(&unverified).expect("CPU"),
        DisplayRenderer::Cpu
    );
    let gpu = fenix_bundle::select_variant(lock, Some("gpu")).expect("variant");
    assert_eq!(
        DisplayRenderer::from_manifest(&gpu).expect("GPU"),
        DisplayRenderer::Gpu
    );
    assert!(
        DisplayRenderer::for_state(&legacy, &json!({"display_renderer_override":"GPU"})).is_err()
    );
}

#[test]
fn preview_seven_cpu_settings_migrate_to_gpu_with_a_backup_and_legacy_readouts() {
    let temp = tempfile::tempdir().expect("fixture");
    let root = temp.path().join("prefix");
    prefix(&root);
    let settings = root.join(fenix_setup::CONFIG).join("fenixConfig.xml");
    let original = fs::read(&settings).expect("original settings");
    let mut state = json!({"version":"0.1.0-preview.7", "configured":true, "account_data":"keep"});
    let renderer = DisplayRenderer::for_state(&gpu_manifest(), &state).expect("new renderer");
    assert_eq!(renderer, DisplayRenderer::Gpu);
    let backup = temp.path().join("settings-before-update");
    assert!(
        fenix_setup::migrate_renderer(
            &mut state,
            &root,
            &backup,
            renderer,
            &AtomicBool::new(false)
        )
        .expect("migration")
    );
    assert_eq!(
        fs::read(backup.join("fenixConfig.xml")).expect("backup"),
        original
    );
    assert_eq!(state["display_renderer"], "GPU");
    assert_eq!(state["configured"], true);
    assert_eq!(state["readouts"], true);
    assert_eq!(state["account_data"], "keep");
    let config = xml::parse(&fs::read(&settings).expect("settings")).expect("XML");
    for (tag, value) in [
        ("displayMode", "GPU"),
        ("newRender", "true"),
        ("preferCPU", "false"),
        ("multithread", "true"),
        ("custom", "keep"),
    ] {
        assert_eq!(config.child(tag).expect("setting").text(), value);
    }
    let persisted = xml::parse(
        &fs::read(root.join(fenix_setup::CONFIG).join("persistancy.xml")).expect("readouts"),
    )
    .expect("XML");
    assert_eq!(
        persisted
            .child("fcuReadoutsType")
            .expect("readout setting")
            .text(),
        "0"
    );
    assert!(
        !fenix_setup::migrate_renderer(
            &mut state,
            &root,
            &temp.path().join("unused-backup"),
            renderer,
            &AtomicBool::new(false)
        )
        .expect("already current")
    );
    assert!(!temp.path().join("unused-backup").exists());
}

#[test]
fn gpu_displays_can_be_ready_before_fenix_creates_its_readout_file() {
    let temp = tempfile::tempdir().expect("fixture");
    prefix(temp.path());
    let persisted = temp
        .path()
        .join(fenix_setup::CONFIG)
        .join("persistancy.xml");
    fs::remove_file(&persisted).expect("delayed readout file");
    let mut state = json!({});
    fenix_setup::apply_settings(&mut state, temp.path(), DisplayRenderer::Gpu)
        .expect("GPU settings");
    assert_eq!(state["configured"], true);
    assert_eq!(state["display_renderer"], "GPU");
    assert_eq!(state["readouts"], false);
    assert!(!persisted.exists());
    write(&persisted, b"<Settings/>");
    fenix_setup::apply_settings(&mut state, temp.path(), DisplayRenderer::Gpu)
        .expect("readout setup");
    assert_eq!(state["readouts"], true);
}

#[test]
fn a_saved_cpu_fallback_survives_reconfiguration_and_future_gpu_updates() {
    let temp = tempfile::tempdir().expect("fixture");
    prefix(temp.path());
    let mut state =
        json!({"configured":true, "display_renderer":"GPU", "display_renderer_override":"CPU"});
    let lock = gpu_manifest();
    let renderer = DisplayRenderer::for_state(&lock, &state).expect("CPU fallback");
    assert_eq!(renderer, DisplayRenderer::Cpu);
    fenix_setup::apply_settings(&mut state, temp.path(), renderer).expect("apply fallback");
    let saved = state.clone();
    let renderer = DisplayRenderer::for_state(&lock, &saved).expect("saved preference");
    fenix_setup::apply_settings(&mut state, temp.path(), renderer).expect("configure again");
    assert_eq!(state["display_renderer"], "CPU");
    assert_eq!(state["display_renderer_override"], "CPU");
    assert_eq!(state["readouts"], true);
    let backup = temp.path().join("unused-backup");
    assert!(
        !fenix_setup::migrate_renderer(
            &mut state,
            temp.path(),
            &backup,
            renderer,
            &AtomicBool::new(false)
        )
        .expect("keep CPU on update")
    );
    assert!(!backup.exists());
    let config = xml::parse(
        &fs::read(
            temp.path()
                .join(fenix_setup::CONFIG)
                .join("fenixConfig.xml"),
        )
        .expect("settings"),
    )
    .expect("XML");
    for (tag, value) in [
        ("displayMode", "CPU"),
        ("newRender", "false"),
        ("preferCPU", "true"),
        ("multithread", "true"),
    ] {
        assert_eq!(config.child(tag).expect("setting").text(), value);
    }
}
