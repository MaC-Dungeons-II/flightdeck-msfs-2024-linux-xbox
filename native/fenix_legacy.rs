// SPDX-License-Identifier: MIT
//! Recognize a known local patch before copying it into a managed transaction.
//! The legacy JSON is evidence of an old setup, never authority for its paths.
use crate::{
    Result,
    error::require,
    fenix, fenix_bundle as bundle,
    fenix_setup::{self as setup, DisplayRenderer},
    files, runtime, transaction as tx, xml,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub const MARKER: &str = "private/fenix-compat.json";

#[derive(Debug, PartialEq)]
pub struct Plan {
    pub variant: Option<String>,
    pub renderer: DisplayRenderer,
    pub renderer_saved: bool,
    pub configured: bool,
    pub readouts: bool,
    pub readouts_saved: bool,
    runner: PathBuf,
    prefix_id: Value,
    scripts: BTreeMap<String, String>,
    settings: BTreeMap<String, Option<String>>,
}

fn script_allowed(name: &str, hash: &str, lock: &Value) -> Result<bool> {
    // Reviewed pre-native FD loader: bounded PE headers, runtime/game/prefix
    // checks, inherited licensed descriptors and scoped Fenix helpers. Only
    // these exact bytes are accepted; a locally edited loader stays protected.
    const LOCAL_FD_LOADER: &str =
        "9df6574b6a3d806a184afe77723302456c5a94a3a5557300dc8dad17b928cf76";
    Ok(bundle::accepted_script(name, hash, lock, None)?
        || lock["integration"][name] == hash
        || name == "xodus-wine-launch" && hash == LOCAL_FD_LOADER
        || lock["previous_releases"]
            .as_object()
            .into_iter()
            .flatten()
            .any(|(_, snapshot)| snapshot["integration"][name] == hash))
}

pub fn inspect(root: &Path, manifest: &Value) -> Result<Plan> {
    require(
        !files::exists(&root.join(fenix::MARKER)),
        "A managed Fenix transaction already exists.",
    )?;
    require(
        runtime::value(&root.join(MARKER))?.is_object(),
        "Invalid local Fenix installation record.",
    )?;
    let runner = root.join("runner").canonicalize()?;
    let variant = bundle::runner_variant_with(&runner, true, manifest)?;
    let lock = bundle::select_variant(manifest.clone(), variant.as_deref())?;
    let mut scripts = BTreeMap::new();
    for name in bundle::LAUNCH_FILES {
        let hash = tx::digest(&root.join("tools").join(name))?;
        require(
            script_allowed(name, &hash, &lock)?,
            "The local Fenix launch script was customized. It has not been replaced.",
        )?;
        scripts.insert(name.to_owned(), hash);
    }
    let prefix = root.join("local/msfs-prefix");
    let config = bundle::contained(&prefix, &format!("{}/fenixConfig.xml", setup::CONFIG))?;
    let config_bytes = if files::exists(&config) {
        Some(files::read(&config, 2 * 1024 * 1024)?)
    } else {
        None
    };
    let configured =
        config_bytes.is_some() && prefix.join(setup::PROGRAM).join("Fenix.exe").is_file();
    let saved = config_bytes
        .as_ref()
        .map(|bytes| xml::parse(bytes))
        .transpose()?
        .and_then(|settings| settings.child("displayMode").map(|v| v.text()));
    let renderer = if let Some(saved) = &saved {
        DisplayRenderer::for_state(
            &lock,
            &serde_json::json!({"display_renderer_override":saved}),
        )?
    } else {
        DisplayRenderer::from_manifest(&lock)?
    };
    let persisted = bundle::contained(&prefix, &format!("{}/persistancy.xml", setup::CONFIG))?;
    let persisted_bytes = if files::exists(&persisted) {
        Some(files::read(&persisted, 2 * 1024 * 1024)?)
    } else {
        None
    };
    let readouts_saved = persisted_bytes.is_some();
    let readouts = persisted_bytes
        .as_ref()
        .map(|bytes| xml::parse(bytes))
        .transpose()?
        .is_some_and(|settings| {
            settings
                .child("fcuReadoutsType")
                .is_some_and(|v| v.text() == "0")
        });
    let settings = BTreeMap::from([
        (
            "fenixConfig.xml".to_owned(),
            config_bytes.as_ref().map(|bytes| files::sha256(bytes)),
        ),
        (
            "persistancy.xml".to_owned(),
            persisted_bytes.as_ref().map(|bytes| files::sha256(bytes)),
        ),
    ]);
    Ok(Plan {
        variant,
        renderer,
        renderer_saved: saved.is_some(),
        configured,
        readouts,
        readouts_saved,
        runner,
        prefix_id: fenix::identity(&prefix)?,
        scripts,
        settings,
    })
}

impl Plan {
    pub fn revalidate(&self, root: &Path, manifest: &Value) -> Result<()> {
        // The managed preparation marker is already durable at this point.
        // Compare the original runner, scripts and profile without trusting it.
        require(
            root.join("runner").canonicalize()? == self.runner
                && fenix::identity(&root.join("local/msfs-prefix"))? == self.prefix_id,
            "The local Fenix installation changed during preparation.",
        )?;
        let lock = bundle::select_variant(manifest.clone(), self.variant.as_deref())?;
        require(
            bundle::runner_variant_with(&self.runner, true, manifest)? == self.variant,
            "The local Fenix runner changed during preparation.",
        )?;
        for (name, hash) in &self.scripts {
            require(
                tx::digest(&root.join("tools").join(name))? == *hash
                    && script_allowed(name, hash, &lock)?,
                "The local Fenix launch script changed during preparation.",
            )?;
        }
        for (name, expected) in &self.settings {
            let file = bundle::contained(
                &root.join("local/msfs-prefix"),
                &format!("{}/{name}", setup::CONFIG),
            )?;
            let actual = if files::exists(&file) {
                Some(files::sha256(&files::read(&file, 2 * 1024 * 1024)?))
            } else {
                None
            };
            require(
                actual == *expected,
                "The saved Fenix preferences changed during preparation.",
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{fs, os::unix::fs::symlink};

    fn put(path: &Path, bytes: &[u8]) {
        files::private_dir(path.parent().expect("parent")).expect("directory");
        files::atomic(path, bytes).expect("fixture bytes");
    }

    fn fixture(root: &Path) -> Value {
        let runner = root.join("local/old-runner");
        put(&runner.join("version"), b"fixture runner");
        put(&runner.join("files/bin/wine"), b"stock loader");
        put(&runner.join("files/lib/module.dll"), b"known old patch");
        symlink(&runner, root.join("runner")).expect("runner link");
        // These historical paths must never become copy/restore authority.
        put(
            &root.join(MARKER),
            br#"{"runner":"/unrelated/path","prefix":"/outside/profile"}"#,
        );
        for name in bundle::LAUNCH_FILES {
            put(
                &root.join("tools").join(name),
                bundle::script(name).expect("current script"),
            );
        }
        let prefix = root.join("local/msfs-prefix");
        put(
            &prefix.join(setup::PROGRAM).join("Fenix.exe"),
            b"fixture aircraft app",
        );
        put(&prefix.join(setup::CONFIG).join("fenixConfig.xml"),
            b"<Settings><displayMode>CPU</displayMode><preferCPU>true</preferCPU><HomeCockpitMode>true</HomeCockpitMode></Settings>");
        put(&prefix.join(setup::CONFIG).join("persistancy.xml"),
            b"<Settings><fcuReadoutsType>1</fcuReadoutsType><keep>user preference</keep></Settings>");
        json!({"version":"0.1.0","runner_version":"fixture runner","display_renderer":"CPU", "variants":{},
            "runner_files":{"files/bin/wine":files::sha256(b"stock loader"),"files/lib/module.dll":files::sha256(b"stock module")},
            "files":{"files/lib/module.dll":files::sha256(b"new patch")},
            "previous_releases":{"0.1.0-preview.3":{"files":{"files/lib/module.dll":files::sha256(b"known old patch")},"integration":{},"prefix_files":{}}}})
    }

    #[test]
    fn known_patch_is_recognized_without_rewriting_preferences_or_trusting_legacy_paths() {
        let temp = tempfile::tempdir().expect("fixture");
        let lock = fixture(temp.path());
        let config = temp.path().join("local/msfs-prefix").join(setup::CONFIG);
        let before = fs::read(config.join("fenixConfig.xml")).expect("settings");
        let persisted = fs::read(config.join("persistancy.xml")).expect("FCU preference");
        let plan = inspect(temp.path(), &lock).expect("known local installation");
        assert_eq!(plan.variant, None);
        assert_eq!(plan.renderer, DisplayRenderer::Cpu);
        assert!(plan.renderer_saved && plan.configured && plan.readouts_saved);
        assert!(!plan.readouts, "the user's modern FCU choice is retained");
        assert_eq!(
            fs::read(config.join("fenixConfig.xml")).expect("unchanged settings"),
            before
        );
        assert_eq!(
            fs::read(config.join("persistancy.xml")).expect("unchanged FCU"),
            persisted
        );
        assert!(!temp.path().join(fenix::MARKER).exists());
    }

    #[test]
    fn a_verified_current_patch_wrapper_is_supported_but_its_local_edits_are_protected() {
        let temp = tempfile::tempdir().expect("fixture");
        let mut lock = fixture(temp.path());
        let path = temp.path().join("tools/xodus-wine-launch");
        put(&path, b"verified standalone runtime wrapper");
        lock["integration"] =
            json!({"xodus-wine-launch":files::sha256(b"verified standalone runtime wrapper")});
        inspect(temp.path(), &lock).expect("current verified wrapper");
        put(&path, b"edited standalone runtime wrapper");
        assert!(inspect(temp.path(), &lock).is_err());
    }

    #[test]
    fn changed_native_payload_and_custom_scripts_cannot_be_adopted() {
        for changed in [
            "local/old-runner/files/lib/module.dll",
            "tools/xodus-wine-launch",
        ] {
            let temp = tempfile::tempdir().expect("fixture");
            let lock = fixture(temp.path());
            let path = temp.path().join(changed);
            put(&path, b"local customization");
            assert!(inspect(temp.path(), &lock).is_err(), "{changed}");
            assert_eq!(
                fs::read(path).expect("custom file retained"),
                b"local customization"
            );
            assert!(!temp.path().join(fenix::MARKER).exists());
        }
    }

    #[test]
    fn preparation_rechecks_the_actual_source_before_publication() {
        for changed in [
            "tools/launch-msfs.sh",
            "local/msfs-prefix/drive_c/ProgramData/Fenix/FenixSim A320/persistancy.xml",
        ] {
            let temp = tempfile::tempdir().expect("fixture");
            let lock = fixture(temp.path());
            let plan = inspect(temp.path(), &lock).expect("known patch");
            put(
                &temp.path().join(fenix::MARKER),
                br#"{"state":"preparing"}"#,
            );
            plan.revalidate(temp.path(), &lock)
                .expect("unchanged source despite new preparation marker");
            put(&temp.path().join(changed), b"changed after the copy");
            assert!(plan.revalidate(temp.path(), &lock).is_err());
        }
    }

    #[test]
    fn linked_legacy_metadata_and_invalid_saved_renderer_do_not_enable_adoption() {
        let temp = tempfile::tempdir().expect("fixture");
        let lock = fixture(temp.path());
        put(
            &temp
                .path()
                .join("local/msfs-prefix")
                .join(setup::CONFIG)
                .join("fenixConfig.xml"),
            b"<Settings><displayMode>unknown</displayMode></Settings>",
        );
        assert!(inspect(temp.path(), &lock).is_err());
        fs::remove_file(temp.path().join(MARKER)).expect("replace metadata");
        put(&temp.path().join("untrusted.json"), b"{}");
        symlink(temp.path().join("untrusted.json"), temp.path().join(MARKER))
            .expect("linked metadata");
        assert!(inspect(temp.path(), &lock).is_err());
    }
}
