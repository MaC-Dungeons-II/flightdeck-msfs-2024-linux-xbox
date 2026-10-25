// SPDX-License-Identifier: MIT
#![allow(clippy::unwrap_used)]
use flightdeck::{files, process};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

fn compare(source: &Path, destination: &Path, links: &mut BTreeMap<(u64, u64), (u64, u64)>) {
    let a = fs::symlink_metadata(source).expect("source metadata");
    let b = fs::symlink_metadata(destination).expect("copy metadata");
    assert_eq!(a.file_type(), b.file_type(), "{}", source.display());
    if a.is_symlink() {
        assert_eq!(
            fs::read_link(source).unwrap(),
            fs::read_link(destination).unwrap()
        );
    } else if a.is_file() {
        assert_eq!(a.len(), b.len());
        // NTFS stores timestamps at 100-ns precision; tolerate only rounding.
        let a_time = a.modified().unwrap();
        let b_time = b.modified().unwrap();
        let delta = a_time
            .duration_since(b_time)
            .unwrap_or_else(|e| e.duration());
        assert!(delta < std::time::Duration::from_micros(1));
        assert_eq!(
            files::digest(fs::File::open(source).unwrap()).unwrap(),
            files::digest(fs::File::open(destination).unwrap()).unwrap(),
            "{}",
            source.display()
        );
        if a.mode() & 0o100 != 0 {
            assert_ne!(b.mode() & 0o100, 0, "executable lost: {}", source.display());
        }
        if a.nlink() > 1 {
            let target = (b.dev(), b.ino());
            assert_eq!(*links.entry((a.dev(), a.ino())).or_insert(target), target);
        }
    } else if a.is_dir() {
        let names = |p: &Path| {
            let mut names = fs::read_dir(p)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        assert_eq!(names(source), names(destination));
        for name in names(source) {
            compare(&source.join(&name), &destination.join(name), links);
        }
    } else {
        panic!("unexpected profile entry: {}", source.display());
    }
}

#[test]
fn profile_copy_preserves_contents_links_and_executables_without_following_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("wine"), b"executable fixture").unwrap();
    fs::File::options()
        .write(true)
        .open(source.join("wine"))
        .unwrap()
        .set_times(
            fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000),
            ),
        )
        .unwrap();
    fs::set_permissions(source.join("wine"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::hard_link(source.join("wine"), source.join("wine64")).unwrap();
    fs::create_dir(source.join("nested")).unwrap();
    fs::write(source.join("nested/registry"), b"registry fixture").unwrap();
    symlink("../wine", source.join("nested/relative")).unwrap();
    symlink("missing-drive", source.join("broken")).unwrap();
    symlink("/", source.join("z:")).unwrap();
    let destination = temp.path().join("clone");
    let cancel = AtomicBool::new(false);
    process::copy_tree(&source, &destination, &cancel).unwrap();
    compare(&source, &destination, &mut BTreeMap::new());
    assert!(process::copy_tree(&source, &destination, &cancel).is_err());
    assert!(
        process::copy_tree(
            &source,
            &temp.path().join("cancelled"),
            &AtomicBool::new(true)
        )
        .is_err()
    );
    assert_eq!(
        fs::read(source.join("wine")).unwrap(),
        b"executable fixture"
    );
}

#[test]
#[ignore = "requires explicit synthetic profile/runner sources and a new destination on the test filesystem"]
fn profile_and_runner_copy_on_explicit_filesystem() {
    let input = |key| PathBuf::from(std::env::var_os(key).expect("explicit copy probe input"));
    let output = std::path::absolute(input("FLIGHTDECK_TEST_COPY_OUTPUT")).unwrap();
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .unwrap();
    assert!(output.starts_with(repo.join("build")));
    assert!(!files::exists(&output));
    fs::create_dir(&output).unwrap();
    for (name, key) in [
        ("prefix", "FLIGHTDECK_TEST_COPY_PREFIX"),
        ("runner", "FLIGHTDECK_TEST_COPY_RUNNER"),
    ] {
        let source = input(key);
        let destination = output.join(name);
        process::copy_tree(&source, &destination, &AtomicBool::new(false)).unwrap();
        compare(&source, &destination, &mut BTreeMap::new());
        println!("{name}: all contents, symlink targets, hard links and executable files verified");
    }
}
