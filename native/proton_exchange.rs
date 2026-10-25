// SPDX-License-Identifier: MIT
//! Restartable profile exchange under the already durable Proton journal.
use crate::{Result, error::require, fenix, files, proton, transaction, wine_processes};
use serde_json::Value;
use std::path::Path;

const CHANGED: &str = "Proton prefix identity changed.";

fn identity(path: &Path) -> Result<Option<Value>> {
    if files::exists(path) {
        fenix::identity(path).map(Some)
    } else {
        Ok(None)
    }
}

pub(crate) fn exchange(root: &Path, backup: &Path, before: &Value, after: &Value) -> Result<()> {
    exchange_with(root, backup, before, after, transaction::exchange)
}

fn exchange_with(
    root: &Path,
    backup: &Path,
    before: &Value,
    after: &Value,
    atomic: impl FnOnce(&Path, &Path) -> Result<()>,
) -> Result<()> {
    let prefix = root.join("local/msfs-prefix");
    let holding = backup
        .parent()
        .ok_or(crate::Error::Invalid(CHANGED))?
        .join("exchanging-prefix");
    for path in [&prefix, backup, &holding] {
        proton::managed(root, path.parent().ok_or(crate::Error::Invalid(CHANGED))?)?;
        if files::exists(path) {
            proton::managed(root, path)?;
            wine_processes::idle(path)?;
        }
    }
    require(before != after, CHANGED)?;
    let state = (identity(&prefix)?, identity(backup)?, identity(&holding)?);
    let original = (Some(before.clone()), Some(after.clone()), None);
    let first = (None, Some(after.clone()), Some(before.clone()));
    let second = (Some(after.clone()), None, Some(before.clone()));
    let complete = (Some(after.clone()), Some(before.clone()), None);
    require(
        [&original, &first, &second, &complete].contains(&&state),
        CHANGED,
    )?;
    if state == complete {
        return Ok(());
    }
    if state == original {
        match atomic(&prefix, backup) {
            Ok(()) => return Ok(()),
            Err(crate::Error::Io(error)) if matches!(error.raw_os_error(), Some(22 | 38 | 95)) => {}
            Err(error) => return Err(error),
        }
        // ntfs3 supports ordinary no-replace renames but not RENAME_EXCHANGE.
        // Each step is recoverable from the existing before/after inode IDs;
        // no journal phase update can get ahead of the directory operation.
        transaction::publish(&prefix, &holding)?;
    }
    if !files::exists(&prefix) {
        require(
            identity(backup)? == Some(after.clone()) && identity(&holding)? == Some(before.clone()),
            CHANGED,
        )?;
        transaction::publish(backup, &prefix)?;
    }
    if !files::exists(backup) {
        require(
            identity(&prefix)? == Some(after.clone())
                && identity(&holding)? == Some(before.clone()),
            CHANGED,
        )?;
        transaction::publish(&holding, backup)?;
    }
    require(
        identity(&prefix)? == Some(after.clone()) && identity(backup)? == Some(before.clone()),
        CHANGED,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn unsupported_exchange_and_each_interrupted_rename_resume_without_losing_profiles() {
        for phase in 0..=3 {
            let temp = tempfile::tempdir().expect("runtime");
            let root = temp.path();
            let prefix = root.join("local/msfs-prefix");
            let backup =
                root.join("local/proton-tests/0123456789abcdef0123456789abcdef/previous-prefix");
            files::private_dir(&prefix).expect("prefix");
            files::private_dir(&backup).expect("backup");
            fs::write(prefix.join("old-addon"), b"current user data").expect("old");
            fs::write(backup.join("new-addon"), b"prepared user data").expect("new");
            let before = fenix::identity(&prefix).expect("before");
            let after = fenix::identity(&backup).expect("after");
            let holding = backup.parent().expect("parent").join("exchanging-prefix");
            if phase >= 1 {
                transaction::publish(&prefix, &holding).expect("first");
            }
            if phase >= 2 {
                transaction::publish(&backup, &prefix).expect("second");
            }
            if phase >= 3 {
                transaction::publish(&holding, &backup).expect("third");
            }
            exchange_with(root, &backup, &before, &after, |_, _| {
                Err(std::io::Error::from_raw_os_error(22).into())
            })
            .expect("recover");
            exchange(root, &backup, &before, &after).expect("idempotent");
            assert_eq!(
                fs::read(prefix.join("new-addon")).expect("new preserved"),
                b"prepared user data"
            );
            assert_eq!(
                fs::read(backup.join("old-addon")).expect("old preserved"),
                b"current user data"
            );
            assert!(!holding.exists());
        }
    }

    #[test]
    fn unrelated_holding_directory_and_real_io_errors_never_trigger_fallback() {
        let temp = tempfile::tempdir().expect("runtime");
        let root = temp.path();
        let prefix = root.join("local/msfs-prefix");
        let backup =
            root.join("local/proton-tests/0123456789abcdef0123456789abcdef/previous-prefix");
        files::private_dir(&prefix).expect("prefix");
        files::private_dir(&backup).expect("backup");
        let before = fenix::identity(&prefix).expect("before");
        let after = fenix::identity(&backup).expect("after");
        assert!(
            exchange_with(root, &backup, &before, &after, |_, _| Err(
                std::io::Error::from_raw_os_error(13).into()
            ))
            .is_err()
        );
        files::private_dir(&backup.parent().expect("parent").join("exchanging-prefix"))
            .expect("unrelated");
        assert!(exchange(root, &backup, &before, &after).is_err());
        assert_eq!(fenix::identity(&prefix).expect("unchanged"), before);
    }
}
