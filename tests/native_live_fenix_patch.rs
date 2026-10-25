// SPDX-License-Identifier: MIT
//! Explicit account-free patch setup using a pinned runner and a new runtime.
use flightdeck::{backend::Launcher, bootstrap, fenix, files, resources};
use serde_json::json;
use std::{
    fs,
    path::{Component, PathBuf},
    sync::atomic::AtomicBool,
};

#[test]
#[ignore = "requires the pinned original runner archive and public Fenix payload; creates a new prefix"]
fn fresh_fenix_patch_setup() {
    let input = |key| PathBuf::from(std::env::var_os(key).expect("explicit test input"));
    let root = std::path::absolute(input("FLIGHTDECK_TEST_PATCH_ROOT")).expect("root");
    let payload = input("FLIGHTDECK_TEST_PATCH_BUNDLE");
    let archive = input("FLIGHTDECK_TEST_PATCH_ARCHIVE");
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .expect("repo");
    assert!(root.starts_with(repo.join("build")));
    assert!(
        root.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
    );
    assert!(!files::exists(&root), "must use a new runtime directory");
    let lock = resources::json("compat/bootstrap.lock.json").expect("bootstrap lock");
    assert_eq!(
        files::digest(fs::File::open(&archive).expect("archive")).expect("archive hash"),
        lock["runner"]["archive_sha256"]
    );
    files::private_dir(&root.join("private")).expect("private");
    files::private_dir(&root.join("local")).expect("local");
    fs::write(
        root.join("private/synthetic-probe"),
        b"Flightdeck isolated test\n",
    )
    .expect("marker");
    fs::write(
        root.join("private/runtime.json"),
        b"{\"game_id\":\"msfs2024\"}\n",
    )
    .expect("metadata");
    if let Some(cache) = std::env::var_os("FLIGHTDECK_TEST_PATCH_CACHE") {
        let cache = PathBuf::from(cache);
        let destination = root.join("private/fenix-downloads");
        files::private_dir(&destination).expect("private download cache");
        for name in [
            "dotNetFx40_Full_x86_x64.exe",
            "NDP48-x86-x64-AllOS-ENU.exe",
            "Windows6.1-KB2670838-x64.msu",
            "msdelta.dll",
            "windowsdesktop-runtime-10.0.13-win-x64.exe",
        ] {
            fs::copy(cache.join(name), destination.join(name)).expect("cached public prerequisite");
        }
    }
    let unpacked = root.join("local/original-runner");
    files::private_dir(&unpacked).expect("runner extraction directory");
    bootstrap::extract(&archive, &unpacked, &AtomicBool::new(false)).expect("original archive");
    let runner = unpacked.join(
        lock["runner"]["directory"]
            .as_str()
            .expect("runner directory"),
    );
    std::os::unix::fs::symlink(&runner, root.join("runner")).expect("runner link");
    bootstrap::prepare_prefix(
        &runner,
        &root.join("local/msfs-prefix"),
        &AtomicBool::new(false),
    )
    .expect("fresh Wine prefix");
    resources::install_scripts(&root.join("tools")).expect("original launchers");
    let app = Launcher::new(root.join("private/patch-probe-state"), None).expect("isolated state");
    app.lock().runtime = Some(root.clone());
    let ctx = app.reserve("fenix", "install", true).expect("reservation");
    let result = fenix::install(&ctx, &payload);
    let marker = files::json::<serde_json::Value>(&root.join(fenix::MARKER), 1024 * 1024)
        .unwrap_or_default();
    let report = json!({"passed":result.is_ok(),"error":result.as_ref().err().map(ToString::to_string),
        "patch_state":marker["state"],"version":marker["version"],
        "previous_prefix_exists":marker["previous_prefix"].as_str().is_some_and(|p|root.join(p).is_dir())});
    fs::write(
        root.join("private/patch-probe-result.json"),
        serde_json::to_vec_pretty(&report).expect("report"),
    )
    .expect("save result");
    assert!(result.is_ok(), "{report}");
    assert_eq!(marker["state"], "installed");
    assert_eq!(report["previous_prefix_exists"], true);
}
