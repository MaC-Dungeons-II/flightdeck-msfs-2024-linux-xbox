// SPDX-License-Identifier: MIT
//! Explicit read-only recognition of an existing local installation. No Wine.
use flightdeck::{fenix_bundle, fenix_legacy};
use std::path::PathBuf;

#[test]
#[ignore = "requires an explicit existing local runtime; reads only known native/script/settings files"]
fn known_local_installation_passes_the_production_adoption_preflight() {
    let root = PathBuf::from(
        std::env::var_os("FLIGHTDECK_TEST_LEGACY_RUNTIME").expect("explicit runtime"),
    );
    let manifest = fenix_bundle::manifest(None).expect("embedded verified release");
    let plan = fenix_legacy::inspect(&root, &manifest).expect("known local installation");
    println!(
        "Known local installation recognized; renderer {}, saved preference {}. No files changed.",
        plan.renderer.as_str(),
        plan.renderer_saved
    );
}
