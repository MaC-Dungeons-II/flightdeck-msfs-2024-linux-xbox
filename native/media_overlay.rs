// SPDX-License-Identifier: MIT
//! A small game-only Wine view, leaving the selected runner and prefix intact.
//! The separately licensed GStreamer module is built from the pinned LGPL
//! sources. Its PE partner and loader must match before it can be selected.
use crate::{Result, error::require, files, media::Environment, owned_tree, transaction as tx};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
};

#[cfg(feature = "bundled-media")]
const MODULE: &[u8] = include_bytes!("../build/media/winegstreamer.so");
#[cfg(not(feature = "bundled-media"))]
const MODULE: &[u8] = &[];
const COPIES: [&str; 4] = [
    "files/bin/wine",
    "files/lib/wine/x86_64-unix/ntdll.so",
    "files/lib/wine/x86_64-windows/ntdll.dll",
    "files/lib/wine/x86_64-windows/winegstreamer.dll",
];
const LINKS: [&str; 4] = [
    "files/bin/wineserver",
    "files/share",
    "files/lib/wine/i386-windows",
    "files/lib/wine/i386-unix",
];
const PATCHED: &str = "files/lib/wine/x86_64-unix/winegstreamer.so";
const INVALID: &str = "The private media runner could not be verified.";

pub fn payload_status() -> Value {
    let lock: Value = serde_json::from_str(include_str!("../compat/media.lock.json"))
        .expect("embedded media lock");
    json!({"schema":1,"bundled":!MODULE.is_empty(),"sha256":if MODULE.is_empty(){Value::Null}else{json!(files::sha256(MODULE))},"expected_sha256":lock["files"]["winegstreamer.so"]["sha256"]})
}

struct Draft(PathBuf, [u64; 2]);
impl Drop for Draft {
    fn drop(&mut self) {
        let _ = owned_tree::remove(&self.0, self.1);
    }
}

fn contents(path: &Path) -> Result<Vec<u8>> {
    let file = files::open_at(rustix::fs::CWD, path, false, false)?;
    require(
        file.metadata()?.uid() == rustix::process::geteuid().as_raw(),
        INVALID,
    )?;
    files::read_file(file, 32 * 1024 * 1024)
}

// The view is a DLL search root. An extra DLL would change what Wine loads,
// so checking only our five expected hashes would not be sufficient.
fn valid(view: &Path, runner: &Path, hashes: &BTreeMap<String, String>) -> bool {
    if files::directory(view, false).is_err() {
        return false;
    }
    let mut allowed: BTreeSet<PathBuf> = hashes.keys().map(PathBuf::from).collect();
    allowed.insert("view.json".into());
    allowed.extend(LINKS.map(PathBuf::from));
    for path in allowed.clone() {
        allowed.extend(
            path.ancestors()
                .skip(1)
                .filter(|p| !p.as_os_str().is_empty())
                .map(Path::to_path_buf),
        );
    }
    let check = || -> Result<bool> {
        let mut pending = vec![PathBuf::new()];
        let mut seen = BTreeSet::new();
        while let Some(relative) = pending.pop() {
            for entry in fs::read_dir(view.join(&relative))? {
                let entry = entry?;
                let name = relative.join(entry.file_name());
                if !allowed.contains(&name) {
                    return Ok(false);
                }
                let info = fs::symlink_metadata(entry.path())?;
                if info.uid() != rustix::process::geteuid().as_raw() {
                    return Ok(false);
                }
                if LINKS.iter().any(|p| Path::new(p) == name) {
                    if !info.is_symlink() || fs::read_link(entry.path())? != runner.join(&name) {
                        return Ok(false);
                    }
                } else if info.is_dir() {
                    pending.push(name.clone());
                } else if !info.is_file() || info.nlink() != 1 {
                    return Ok(false);
                } else if let Some(expected) = name.to_str().and_then(|n| hashes.get(n))
                    && tx::digest(&entry.path())? != *expected
                {
                    return Ok(false);
                }
                seen.insert(name);
            }
        }
        let record = json!({"schema":1,"source_runner":runner,"files":hashes});
        let saved: Value = serde_json::from_slice(&contents(&view.join("view.json"))?)?;
        Ok(seen == allowed && saved == record)
    };
    check().unwrap_or(false)
}

/// `runner` is the real selected runner, including a managed Fenix overlay.
/// Only the game gets the returned loader and additional DLL search path.
/// Call with a fresh child environment; fallbacks leave it unchanged.
pub fn prepare(
    root: &Path,
    runner: &Path,
    wine: &Path,
    env: &mut Environment,
) -> Result<(PathBuf, &'static str)> {
    let original = wine.to_path_buf();
    if MODULE.is_empty() {
        return Ok((original, "not_bundled"));
    }
    if env
        .get(OsStr::new("FLIGHTDECK_MEDIA_SYSTEM_MEMORY"))
        .is_some_and(|v| v == "0")
    {
        return Ok((original, "disabled"));
    }
    let lock: Value = serde_json::from_str(include_str!("../compat/media.lock.json"))?;
    if runner.join("files/bin/wine").canonicalize().ok().as_deref() != Some(wine) {
        return Ok((original, "unsupported"));
    }
    prepare_with(root, runner, env, &lock, MODULE)
}

fn prepare_with(
    root: &Path,
    runner: &Path,
    env: &mut Environment,
    lock: &Value,
    module: &[u8],
) -> Result<(PathBuf, &'static str)> {
    let runner = runner.canonicalize()?;
    let original = runner.join("files/bin/wine");
    let inputs: BTreeMap<String, String> = serde_json::from_value(lock["runner_inputs"].clone())?;
    // Unknown/custom Proton builds keep their own ABI-compatible module.
    let mut verified = BTreeMap::new();
    for (name, expected) in &inputs {
        let Ok(bytes) = contents(&runner.join(name)) else {
            return Ok((original, "unsupported"));
        };
        if files::sha256(&bytes) != *expected {
            return Ok((original, "unsupported"));
        }
        verified.insert(name.clone(), bytes);
    }
    require(
        lock["schema"] == 1
            && lock["files"]["winegstreamer.so"]["sha256"] == files::sha256(module)
            && lock["files"]["winegstreamer.so"]["bytes"] == module.len(),
        INVALID,
    )?;
    let mut data = BTreeMap::new();
    for name in COPIES {
        let bytes = if let Some(bytes) = verified.remove(name) {
            bytes
        } else {
            contents(&runner.join(name))?
        };
        data.insert(name.to_string(), bytes);
    }
    data.insert(PATCHED.into(), module.to_vec());
    let hashes: BTreeMap<String, String> = data
        .iter()
        .map(|(n, d)| (n.clone(), files::sha256(d)))
        .collect();
    let record = json!({"schema":1,"source_runner":runner,"files":hashes});
    let id = files::sha256(&serde_json::to_vec(&record)?);
    let parent = root.join("local/media-runners");
    files::private_dir(&root.join("local"))?;
    files::private_dir(&parent)?;
    let view = parent.join(id);
    if files::exists(&view) {
        if !valid(&view, &runner, &hashes) {
            return Ok((original, "cache_changed"));
        }
    } else {
        let path = tx::new_directory(&parent, ".media-")?;
        let draft = Draft(path.clone(), owned_tree::identity(&path)?);
        for (name, bytes) in &data {
            let target = path.join(name);
            fs::create_dir_all(target.parent().expect("view parent"))?;
            files::atomic(&target, bytes)?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o700))?;
        }
        for name in LINKS {
            let target = path.join(name);
            fs::create_dir_all(target.parent().expect("view parent"))?;
            symlink(runner.join(name), target)?;
        }
        files::atomic_json(&path.join("view.json"), &record)?;
        require(valid(&path, &runner, &hashes), INVALID)?;
        if let Err(error) = tx::publish(&path, &view)
            && !valid(&view, &runner, &hashes)
        {
            return Err(error);
        }
        drop(draft);
    }
    let mut dlls = vec![runner.join("files/lib/wine")];
    if let Some(existing) = env.get(OsStr::new("WINEDLLPATH")) {
        for path in std::env::split_paths(existing).filter(|p| !p.as_os_str().is_empty()) {
            if !dlls.contains(&path) {
                dlls.push(path);
            }
        }
    }
    env.insert(
        "WINEDLLPATH".into(),
        std::env::join_paths(dlls).map_err(|_| crate::Error::Invalid(INVALID))?,
    );
    env.insert(
        "WINESERVER".into(),
        runner.join("files/bin/wineserver").into_os_string(),
    );
    let wine = view.join("files/bin/wine");
    env.insert("WINELOADER".into(), wine.as_os_str().to_owned());
    env.insert("WINE".into(), wine.as_os_str().to_owned());
    Ok((wine, "system_memory"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, PathBuf, Value) {
        let root = tempfile::tempdir().expect("root");
        let runner = root.path().join("runner");
        for name in COPIES.into_iter().chain([PATCHED]) {
            let path = runner.join(name);
            fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            fs::write(path, name).expect("file");
        }
        let inputs: BTreeMap<_, _> = [COPIES[0], COPIES[3], PATCHED]
            .map(|n| (n.to_string(), files::sha256(n.as_bytes())))
            .into();
        let lock = json!({"schema":1,"runner_inputs":inputs,"files":{"winegstreamer.so":{"sha256":files::sha256(b"patched"),"bytes":7}}});
        (root, runner, lock)
    }
    #[test]
    fn view_is_reused_without_mutating_runner_and_extra_dlls_are_never_loaded() {
        let (root, runner, lock) = fixture();
        let mut env = Environment::new();
        let (wine, state) =
            prepare_with(root.path(), &runner, &mut env, &lock, b"patched").expect("view");
        assert_eq!(state, "system_memory");
        assert_eq!(
            fs::read(runner.join(PATCHED)).expect("source"),
            PATCHED.as_bytes()
        );
        assert_eq!(
            prepare_with(root.path(), &runner, &mut env, &lock, b"patched")
                .expect("reuse")
                .0,
            wine
        );
        fs::write(wine.parent().expect("bin").join("foreign.dll"), b"unknown").expect("extra");
        assert_eq!(
            prepare_with(root.path(), &runner, &mut env, &lock, b"patched")
                .expect("protected")
                .1,
            "cache_changed"
        );
        fs::write(runner.join(PATCHED), b"different ABI").expect("custom");
        assert_eq!(
            prepare_with(root.path(), &runner, &mut env, &lock, b"patched")
                .expect("custom retained")
                .1,
            "unsupported"
        );
    }

    #[test]
    fn current_fenix_ntdll_pair_is_copied_and_changed_provenance_is_rejected() {
        let (root, runner, lock) = fixture();
        let first = prepare_with(
            root.path(),
            &runner,
            &mut Environment::new(),
            &lock,
            b"patched",
        )
        .expect("initial view")
        .0;
        for name in [COPIES[1], COPIES[2]] {
            fs::write(runner.join(name), b"current Fenix loader").expect("updated ntdll");
        }
        let second = prepare_with(
            root.path(),
            &runner,
            &mut Environment::new(),
            &lock,
            b"patched",
        )
        .expect("current view")
        .0;
        assert_ne!(first, second);
        let view = second
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("view");
        for name in [COPIES[1], COPIES[2]] {
            assert_eq!(
                fs::read(view.join(name)).expect("copied"),
                b"current Fenix loader"
            );
            assert_eq!(
                fs::read(runner.join(name)).expect("original"),
                b"current Fenix loader"
            );
        }
        fs::write(view.join("view.json"), b"{}").expect("changed provenance");
        let mut env = Environment::new();
        let (wine, state) =
            prepare_with(root.path(), &runner, &mut env, &lock, b"patched").expect("fallback");
        assert_eq!(state, "cache_changed");
        assert_eq!(wine, runner.join("files/bin/wine"));
        assert!(env.is_empty());
    }
}
