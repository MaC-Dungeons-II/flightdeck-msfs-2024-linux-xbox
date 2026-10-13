// SPDX-License-Identifier: MIT
//! Bounds the history kept around saves: local backups before and after each
//! session, downloaded cloud snapshots and the local states saved before each
//! import. Every session adds to all three, and nothing removed them.
//!
//! An entry is kept while it is among the newest, while it is recent, or while
//! anything still refers to it: the snapshot cache, a pending session, the
//! comparison baseline of the local saves, or the running launcher. Only what
//! none of these keeps is removed.
use crate::{
    cloud::{self, Result},
    cloud_fs as fs, cloud_prefix, files,
};
use rustix::fs::{self as rfs, AtFlags, Mode};
use std::{
    collections::BTreeSet,
    fs::File,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// The newest entries of each kind that are kept whatever their age.
pub const KEPT: usize = 20;
/// Entries younger than this are kept whatever their number.
pub const RECENT: Duration = Duration::from_secs(30 * 24 * 3600);

/// What a run removed, by kind.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Removed {
    pub save_backups: usize,
    pub snapshots: usize,
    pub import_backups: usize,
}

/// `snapshot-` or `backup-` followed by 32 lowercase hexadecimal digits,
/// wherever they occur in the bytes of a record.
fn references(bytes: &[u8], found: &mut BTreeSet<String>) {
    for prefix in ["snapshot-", "backup-"] {
        let mut rest = bytes;
        while let Some(at) = rest
            .windows(prefix.len())
            .position(|part| part == prefix.as_bytes())
        {
            let end = at + prefix.len() + 32;
            if let Some(name) = rest
                .get(at..end)
                .and_then(|name| std::str::from_utf8(name).ok())
                && cloud::identifier(name, prefix)
            {
                found.insert(name.to_owned());
            }
            rest = &rest[at + prefix.len()..];
        }
    }
}

fn exists(parent: &File, name: &str) -> Result<bool> {
    match rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(rustix::io::Errno::NOENT) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Everything the stored records of this runtime still refer to. A record
/// that cannot be read ends the run: without it nothing is known to be free.
fn referenced(private: &File) -> Result<BTreeSet<String>> {
    const RECORD: usize = 1024 * 1024;
    let mut found = BTreeSet::new();
    for folder in ["cloud-cache", "cloud-sessions"] {
        if !exists(private, folder)? {
            continue;
        }
        let folder = fs::child(private, folder, false)?;
        for name in fs::names(&folder)? {
            references(&fs::read(&folder, &name, RECORD)?, &mut found);
        }
    }
    if exists(private, "local-saves")? {
        let saves = fs::child(private, "local-saves", false)?;
        for namespace in fs::names(&saves)? {
            let Ok(meta) = rfs::statat(&saves, &namespace, AtFlags::SYMLINK_NOFOLLOW) else {
                continue;
            };
            if rfs::FileType::from_raw_mode(meta.st_mode) != rfs::FileType::Directory {
                continue;
            }
            let folder = fs::child(&saves, &namespace, false)?;
            if let Some(pointer) = fs::optional(&folder, "cloud-baseline.json", RECORD)? {
                references(&pointer, &mut found);
            }
        }
    }
    if let Some(pending) = fs::optional(private, "cloud-offline.pending", RECORD)? {
        references(&pending, &mut found);
    }
    Ok(found)
}

struct Kind<'a> {
    folder: &'a str,
    /// Whether a name is an entry of this kind; anything else is left alone.
    entry: fn(&str) -> bool,
    /// Published read-only, with a receipt of the same name beside the folder.
    sealed: bool,
}

fn save_backup(name: &str) -> bool {
    // backup-YYYYMMDD-HHMMSS-NNNNNN
    let parts: Vec<&str> = name.split('-').collect();
    matches!(parts[..], ["backup", date, time, serial]
        if [(date, 8), (time, 6), (serial, 6)]
            .iter()
            .all(|(part, length)| part.len() == *length && part.bytes().all(|c| c.is_ascii_digit())))
}

/// Removes what the retention does not keep and returns how many entries
/// went. `held` names what the running launcher itself still refers to.
/// Requires the play lease, so that no session and no transfer runs meanwhile.
pub fn prune(runtime: &Path, lease: &File, now: SystemTime, held: &[String]) -> Result<Removed> {
    let root = fs::open(runtime, false)?;
    let private = fs::child(&root, "private", false)?;
    fs::lease(&private, lease)?;
    let mut kept = referenced(&private)?;
    kept.extend(held.iter().cloned());
    let mut removed = Removed::default();
    let kinds = [
        (
            Kind {
                folder: "save-backups",
                entry: save_backup,
                sealed: false,
            },
            &mut removed.save_backups,
        ),
        (
            Kind {
                folder: "cloud-saves",
                entry: |name| cloud::identifier(name, "snapshot-"),
                sealed: false,
            },
            &mut removed.snapshots,
        ),
        (
            Kind {
                folder: "cloud-import-backups",
                entry: |name| cloud::identifier(name, "backup-"),
                sealed: true,
            },
            &mut removed.import_backups,
        ),
    ];
    for (kind, count) in kinds {
        if !exists(&private, kind.folder)? {
            continue;
        }
        let folder = fs::child(&private, kind.folder, false)?;
        *count = prune_kind(&private, &folder, &kind, now, &kept)?;
        fs::linked(&private, kind.folder, &folder)?;
    }
    fs::lease(&private, lease)?;
    fs::same(runtime, &root, false)?;
    fs::linked(&root, "private", &private)?;
    Ok(removed)
}

fn prune_kind(
    private: &File,
    folder: &File,
    kind: &Kind,
    now: SystemTime,
    kept: &BTreeSet<String>,
) -> Result<usize> {
    let mut entries = Vec::new();
    for name in fs::names(folder)? {
        if !(kind.entry)(&name) {
            continue;
        }
        let meta = rfs::statat(folder, &name, AtFlags::SYMLINK_NOFOLLOW)?;
        if rfs::FileType::from_raw_mode(meta.st_mode) != rfs::FileType::Directory
            || meta.st_uid != files::uid()
        {
            continue;
        }
        let modified = UNIX_EPOCH + Duration::from_secs(u64::try_from(meta.st_mtime).unwrap_or(0));
        entries.push((modified, name));
    }
    // Newest first; among equal times the name decides, so a run is repeatable.
    entries.sort_by(|a, b| b.cmp(a));
    let mut removed = 0;
    for (index, (modified, name)) in entries.iter().enumerate() {
        // A time in the future is no proof of age.
        let recent = now
            .duration_since(*modified)
            .map_or(true, |age| age <= RECENT);
        if index < KEPT || recent || kept.contains(name) {
            continue;
        }
        // What cannot be removed safely stays; the next start tries again.
        if remove(private, folder, kind, name).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        folder.sync_all()?;
    }
    Ok(removed)
}

fn remove(private: &File, folder: &File, kind: &Kind, name: &str) -> Result<()> {
    if kind.sealed {
        let entry = fs::child(folder, name, false)?;
        rfs::fchmod(&entry, Mode::from_raw_mode(0o700))?;
    }
    cloud_prefix::remove_tree(folder, name, &mut 200_000, 0)?;
    if kind.sealed && exists(private, "cloud-import-receipts")? {
        let receipts = fs::child(private, "cloud-import-receipts", false)?;
        let receipt = format!("{name}.json");
        if exists(&receipts, &receipt)? {
            fs::unlink(&receipts, &receipt, false)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::{fs as stdfs, os::unix::fs::PermissionsExt, path::PathBuf};

    const DAY: u64 = 24 * 3600;

    struct Runtime {
        _temp: tempfile::TempDir,
        root: PathBuf,
        lease: files::Lease,
        now: SystemTime,
    }
    impl Runtime {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap().join("runtime");
            files::private_dir(&root.join("private")).unwrap();
            let lease = files::Lease::acquire(&root.join("private/play.lock"), true).unwrap();
            Self {
                _temp: temp,
                root,
                lease,
                now: SystemTime::now(),
            }
        }
        fn private(&self, name: &str) -> PathBuf {
            self.root.join("private").join(name)
        }
        /// An entry with one file, last changed `days` ago.
        fn entry(&self, folder: &str, name: &str, days: u64) -> PathBuf {
            let path = self.private(folder).join(name);
            stdfs::create_dir_all(&path).unwrap();
            stdfs::set_permissions(self.private(folder), stdfs::Permissions::from_mode(0o700))
                .unwrap();
            stdfs::set_permissions(&path, stdfs::Permissions::from_mode(0o700)).unwrap();
            stdfs::write(path.join("manifest.json"), "{}").unwrap();
            File::open(&path)
                .unwrap()
                .set_modified(self.now - Duration::from_secs(days * DAY))
                .unwrap();
            path
        }
        fn prune(&self, held: &[String]) -> Removed {
            prune(&self.root, &self.lease.0, self.now, held).unwrap()
        }
    }
    fn id(prefix: &str, number: usize) -> String {
        format!("{prefix}-{number:032x}")
    }

    #[test]
    fn the_newest_and_the_recent_stay_and_only_older_ones_go() {
        let runtime = Runtime::new();
        // Forty snapshots, one a day older than the one before.
        let snapshots: Vec<PathBuf> = (0..40)
            .map(|days| runtime.entry("cloud-saves", &id("snapshot", days), days as u64))
            .collect();
        // Twenty-five backups, all of them old: only their number counts.
        let backups: Vec<PathBuf> = (0..25)
            .map(|n| {
                let name = format!("backup-20260101-1200{n:02}-000001");
                runtime.entry("save-backups", &name, 100 + n as u64)
            })
            .collect();
        assert_eq!(
            runtime.prune(&[]),
            Removed {
                save_backups: 5,
                snapshots: 10,
                import_backups: 0
            }
        );
        // Beyond the twenty newest, those of the last thirty days stay as well.
        for (days, path) in snapshots.iter().enumerate() {
            assert_eq!(path.exists(), days < 30, "snapshot of {days} days ago");
        }
        for (n, path) in backups.iter().enumerate() {
            assert_eq!(path.exists(), n < 20, "backup {n}");
        }
        // A second run finds nothing more.
        assert_eq!(runtime.prune(&[]), Removed::default());
    }

    #[test]
    fn what_a_record_or_the_launcher_refers_to_is_never_removed() {
        let runtime = Runtime::new();
        let old = |folder: &str, prefix: &str| -> Vec<PathBuf> {
            (0..30)
                .map(|n| runtime.entry(folder, &id(prefix, n), 100 + n as u64))
                .collect()
        };
        let snapshots = old("cloud-saves", "snapshot");
        let backups = old("cloud-import-backups", "backup");
        // The oldest of each is what the stored records still point at.
        for (folder, name, content) in [
            (
                "cloud-cache",
                "binding.json",
                format!(r#"{{"snapshot_id":"{}"}}"#, id("snapshot", 29)),
            ),
            (
                "cloud-sessions",
                "binding.json",
                format!(
                    r#"{{"snapshot":{{"id":"{}"}},"target_backup_id":"{}"}}"#,
                    id("snapshot", 28),
                    id("backup", 29)
                ),
            ),
            (
                "local-saves/aa",
                "cloud-baseline.json",
                format!(r#"{{"receipt":"{}.json"}}"#, id("backup", 28)),
            ),
        ] {
            files::private_dir(&runtime.private(folder)).unwrap();
            files::atomic(&runtime.private(folder).join(name), content.as_bytes()).unwrap();
        }
        files::atomic(
            &runtime.private("cloud-offline.pending"),
            id("backup", 27).as_bytes(),
        )
        .unwrap();
        // And one more that only the running launcher knows about.
        let held = [id("backup", 26), id("snapshot", 27)];
        let removed = runtime.prune(&held);
        assert_eq!(
            removed,
            Removed {
                save_backups: 0,
                snapshots: 7,
                import_backups: 6
            }
        );
        for (n, path) in snapshots.iter().enumerate() {
            assert_eq!(path.exists(), !(20..27).contains(&n), "snapshot {n}");
        }
        for (n, path) in backups.iter().enumerate() {
            assert_eq!(path.exists(), !(20..26).contains(&n), "backup {n}");
        }
    }

    #[test]
    fn sealed_import_backups_go_with_their_receipts_and_nothing_else_is_touched() {
        let runtime = Runtime::new();
        let receipts = runtime.private("cloud-import-receipts");
        files::private_dir(&receipts).unwrap();
        for n in 0..25 {
            let path = runtime.entry("cloud-import-backups", &id("backup", n), 200 + n as u64);
            // As published: read-only files in a read-only folder.
            for file in ["state.bin", "imported.bin"] {
                stdfs::write(path.join(file), "state").unwrap();
                stdfs::set_permissions(path.join(file), stdfs::Permissions::from_mode(0o400))
                    .unwrap();
            }
            stdfs::set_permissions(&path, stdfs::Permissions::from_mode(0o500)).unwrap();
            File::open(&path)
                .unwrap()
                .set_modified(runtime.now - Duration::from_secs((200 + n as u64) * DAY))
                .unwrap();
            stdfs::write(receipts.join(format!("{}.json", id("backup", n))), "{}").unwrap();
        }
        // Not entries: a staging folder, a foreign name, a file, a link out of the runtime.
        let folder = runtime.private("cloud-import-backups");
        let outside = runtime.root.join("outside");
        stdfs::create_dir(&outside).unwrap();
        stdfs::write(outside.join("kept"), "kept").unwrap();
        stdfs::create_dir(folder.join(".backup-00000000000000000000000000000001")).unwrap();
        stdfs::create_dir(folder.join("notes")).unwrap();
        stdfs::write(folder.join(id("backup", 900)), "a file").unwrap();
        std::os::unix::fs::symlink(&outside, folder.join(id("backup", 901))).unwrap();
        for name in [".backup-00000000000000000000000000000001", "notes"] {
            File::open(folder.join(name))
                .unwrap()
                .set_modified(UNIX_EPOCH)
                .unwrap();
        }
        assert_eq!(runtime.prune(&[]).import_backups, 5);
        for n in 0..25 {
            let present = n < 20;
            assert_eq!(folder.join(id("backup", n)).exists(), present, "backup {n}");
            assert_eq!(
                receipts.join(format!("{}.json", id("backup", n))).exists(),
                present,
                "receipt {n}"
            );
        }
        for name in [".backup-00000000000000000000000000000001", "notes"] {
            assert!(folder.join(name).is_dir(), "{name}");
        }
        assert!(folder.join(id("backup", 900)).is_file());
        assert!(folder.join(id("backup", 901)).is_symlink());
        assert_eq!(stdfs::read(outside.join("kept")).unwrap(), b"kept");
    }

    #[test]
    fn an_unreadable_record_or_a_missing_lease_removes_nothing() {
        let runtime = Runtime::new();
        let snapshots: Vec<PathBuf> = (0..25)
            .map(|n| runtime.entry("cloud-saves", &id("snapshot", n), 100 + n as u64))
            .collect();
        // A record that is not a plain file: what it refers to is unknown.
        files::private_dir(&runtime.private("cloud-cache")).unwrap();
        stdfs::create_dir(runtime.private("cloud-cache/binding.json")).unwrap();
        assert!(prune(&runtime.root, &runtime.lease.0, runtime.now, &[]).is_err());
        assert!(snapshots.iter().all(|path| path.exists()));
        stdfs::remove_dir(runtime.private("cloud-cache/binding.json")).unwrap();
        // Any descriptor is not the lease of this runtime.
        let other = File::open(runtime.root.join("private")).unwrap();
        assert!(prune(&runtime.root, &other, runtime.now, &[]).is_err());
        assert!(snapshots.iter().all(|path| path.exists()));
        assert_eq!(runtime.prune(&[]).snapshots, 5);
    }

    #[test]
    fn references_are_found_wherever_they_stand_and_only_when_complete() {
        let mut found = BTreeSet::new();
        let text = format!(
            "x{}y \"{}.json\" snapshot-00ff backup-{} snapshot-{}Z",
            id("snapshot", 1),
            id("backup", 2),
            "g".repeat(32),
            "0".repeat(31)
        );
        references(text.as_bytes(), &mut found);
        assert_eq!(found, BTreeSet::from([id("backup", 2), id("snapshot", 1)]));
        assert!(save_backup("backup-20261013-120000-000001"));
        for name in [
            "backup-20261013-120000",
            "backup-2026101-120000-000001",
            ".backup-20261013-120000-000001",
            "backup-20261013-120000-00000x",
        ] {
            assert!(!save_backup(name), "{name}");
        }
    }
}
