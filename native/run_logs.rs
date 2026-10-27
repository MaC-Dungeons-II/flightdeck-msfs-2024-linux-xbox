// SPDX-License-Identifier: MIT
//! Keeps the private logs of game sessions from growing without limit: only
//! the newest sessions are kept, and a log that a running game keeps filling
//! gives its middle back to the file system.
use crate::{Result, cloud_prefix, files, log_reader::regex};
use rustix::fs::FallocateFlags;
use std::{fs::File, path::Path};

/// Sessions whose logs are kept, besides the one that is starting.
pub const KEPT: usize = 10;

/// Removes the directories of all but the newest `keep` sessions. Their names
/// start with the time they began, so their order is their age. Only complete
/// session names are touched; removal never follows a link and stops at
/// anything this user does not own. The caller holds the play lease, so no
/// other session writes there.
pub fn prune(private: &Path, keep: usize) -> Result<usize> {
    let folder = files::directory(private, false)?;
    let mut sessions = Vec::new();
    for entry in std::fs::read_dir(private)?.take(100_000) {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir() && regex(r"^run-\d{8}-\d{6}-[A-Za-z0-9]+$").is_match(&name) {
            sessions.push(name);
        }
    }
    sessions.sort();
    let outdated = sessions.len().saturating_sub(keep);
    let mut removed = 0;
    for name in &sessions[..outdated] {
        // What cannot be removed safely stays; the next start tries again.
        if cloud_prefix::remove_tree(&folder, name, &mut 10_000, 0).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// How much of a growing log stays on disk.
#[derive(Clone, Copy)]
pub struct Limit {
    /// The start of the session, which is kept for good.
    pub head: u64,
    /// The newest part, which is kept at any time.
    pub tail: u64,
    /// Growth beyond both before the middle is given back.
    pub slack: u64,
}

/// The start of a session explains most problems and its end the rest. The
/// launcher's own diagnostics read less than this of either.
pub const LIMIT: Limit = Limit {
    head: 16 * 1024 * 1024,
    tail: 16 * 1024 * 1024,
    slack: 16 * 1024 * 1024,
};

const BLOCK: u64 = 4096;

/// A log that another process appends to. The writer keeps its descriptor
/// and is never interrupted: what lies between the kept start and the kept
/// end is released as a hole. The file keeps its length and reads as zero
/// bytes there, but no longer occupies that space.
pub struct Bounded {
    file: File,
    limit: Limit,
    released: u64,
    supported: bool,
}

impl Bounded {
    pub fn new(log: &File, limit: Limit) -> Result<Self> {
        Ok(Self {
            file: log.try_clone()?,
            limit,
            released: 0,
            supported: true,
        })
    }

    /// Releases what has grown beyond the limit and returns the bytes given
    /// back. A file system without holes leaves the log as it is.
    pub fn enforce(&mut self) -> u64 {
        if !self.supported {
            return 0;
        }
        let Ok(length) = self.file.metadata().map(|info| info.len()) else {
            return 0;
        };
        let limit = self.limit;
        if length <= limit.head + limit.tail + limit.slack {
            return 0;
        }
        let start = self.released.max(limit.head).next_multiple_of(BLOCK);
        let end = (length - limit.tail) / BLOCK * BLOCK;
        if end <= start {
            return 0;
        }
        let hole = FallocateFlags::PUNCH_HOLE | FallocateFlags::KEEP_SIZE;
        match rustix::fs::fallocate(&self.file, hole, start, end - start) {
            Ok(()) => {
                self.released = end;
                end - start
            }
            Err(_) => {
                self.supported = false;
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::{
        fs,
        io::Write,
        os::unix::fs::{MetadataExt, symlink},
    };

    #[test]
    fn only_the_newest_sessions_keep_their_logs() {
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("private");
        files::private_dir(&private).unwrap();
        let sessions: Vec<String> = (10..25)
            .map(|second| format!("run-20261012-1200{second}-AbC{second}"))
            .collect();
        for name in &sessions {
            let run = private.join(name);
            files::private_dir(&run).unwrap();
            fs::write(run.join("game.log"), "log").unwrap();
            symlink("/usr/bin/true", run.join("xodus-wine-launch")).unwrap();
        }
        // Nothing that is not exactly a session directory is ever removed.
        let outside = temp.path().join("kept.txt");
        fs::write(&outside, "kept").unwrap();
        for name in ["run-backup", "run-20261012-120000", "local-saves"] {
            files::private_dir(&private.join(name)).unwrap();
        }
        fs::write(private.join("run-20260101-000000-File"), "a file").unwrap();
        symlink(temp.path(), private.join("run-20260101-000001-Link")).unwrap();
        assert_eq!(prune(&private, 10).unwrap(), 5);
        for (index, name) in sessions.iter().enumerate() {
            assert_eq!(private.join(name).exists(), index >= 5, "{name}");
        }
        for name in [
            "run-backup",
            "run-20261012-120000",
            "local-saves",
            "run-20260101-000000-File",
        ] {
            assert!(private.join(name).exists(), "{name}");
        }
        assert!(private.join("run-20260101-000001-Link").is_symlink());
        assert_eq!(fs::read(&outside).unwrap(), b"kept");
        assert_eq!(prune(&private, 10).unwrap(), 0);
    }

    #[test]
    fn a_session_with_foreign_content_is_left_alone() {
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("private");
        files::private_dir(&private).unwrap();
        let run = private.join("run-20261012-120000-Old");
        files::private_dir(&run).unwrap();
        // A directory others may write to is not one of ours.
        let open = run.join("shared");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();
        fs::write(open.join("data"), "data").unwrap();
        assert_eq!(prune(&private, 0).unwrap(), 0);
        assert_eq!(fs::read(open.join("data")).unwrap(), b"data");
    }

    #[test]
    fn a_log_that_runs_append_to_starts_anew_when_it_is_large() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("launcher.log");
        let previous = temp.path().join("launcher.log.1");
        crate::process::log(&path)
            .unwrap()
            .write_all(b"first run\n")
            .unwrap();
        crate::process::log(&path)
            .unwrap()
            .write_all(b"second run\n")
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first run\nsecond run\n");
        assert!(!previous.exists());
        let large = crate::process::LOG_MAXIMUM + 1;
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(large)
            .unwrap();
        crate::process::log(&path)
            .unwrap()
            .write_all(b"third run\n")
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"third run\n");
        assert_eq!(fs::metadata(&previous).unwrap().len(), large);
        // Only one earlier log is kept, and a link is never set aside.
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(large + 5)
            .unwrap();
        drop(crate::process::log(&path).unwrap());
        assert_eq!(fs::metadata(&previous).unwrap().len(), large + 5);
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        let target = temp.path().join("elsewhere.log");
        fs::File::create(&target).unwrap().set_len(large).unwrap();
        let link = temp.path().join("linked.log");
        symlink(&target, &link).unwrap();
        assert!(crate::process::log(&link).is_err());
        assert_eq!(fs::metadata(&target).unwrap().len(), large);
        assert!(!temp.path().join("linked.log.1").exists());
    }

    #[test]
    fn a_growing_log_keeps_its_start_and_end_and_gives_back_the_middle() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("game.log");
        let mut writer = crate::process::log(&path).unwrap();
        let limit = Limit {
            head: 64 * 1024,
            tail: 64 * 1024,
            slack: 64 * 1024,
        };
        let mut bounded = Bounded::new(&writer, limit).unwrap();
        let line = |number: usize| format!("{number:0>63}\n");
        let mut written = 0;
        let mut write = |writer: &mut File, lines: usize| {
            for _ in 0..lines {
                writer.write_all(line(written).as_bytes()).unwrap();
                written += 1;
            }
            writer.sync_all().unwrap();
        };
        // Up to the limit nothing is touched.
        write(&mut writer, 3 * 1024);
        assert_eq!(bounded.enforce(), 0);
        write(&mut writer, 13 * 1024);
        let before = fs::metadata(&path).unwrap();
        let released = bounded.enforce();
        if released == 0 {
            // This file system has no holes; the log is then left complete.
            assert_eq!(fs::read(&path).unwrap().len() as u64, before.len());
            return;
        }
        assert_eq!(released, 16 * 64 * 1024 - limit.head - limit.tail);
        let after = fs::metadata(&path).unwrap();
        assert_eq!(after.len(), before.len());
        assert!(after.blocks() * 512 <= limit.head + limit.tail + 16 * BLOCK);
        // The writer goes on appending, and the start and the end stay.
        write(&mut writer, 1);
        let content = fs::read(&path).unwrap();
        assert!(content.starts_with(line(0).as_bytes()));
        assert!(content.ends_with(line(16 * 1024).as_bytes()));
        let head = limit.head as usize;
        assert_eq!(&content[head - 64..head], line(1023).as_bytes());
        assert!(
            content[head..head + released as usize]
                .iter()
                .all(|byte| *byte == 0)
        );
        assert_eq!(
            &content[head + released as usize..][..64],
            line(15 * 1024).as_bytes()
        );
        // Only what was added since is released the next time.
        assert_eq!(bounded.enforce(), 0);
        write(&mut writer, 4 * 1024 - 1);
        assert_eq!(bounded.enforce(), 4 * 64 * 1024);
        // The launcher's own reader passes the hole without joining lines.
        let mut lines = Vec::new();
        crate::log_reader::scan(&path, |text| {
            lines.extend(text.lines().map(str::to_string));
        })
        .unwrap();
        assert_eq!(lines.first().unwrap(), line(0).trim_end());
        assert_eq!(lines.last().unwrap(), line(written - 1).trim_end());
        assert!(
            lines.iter().all(|text| text.len() == 63),
            "no joined or zero lines"
        );
    }
}
