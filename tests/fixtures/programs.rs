// SPDX-License-Identifier: MIT
//! Where the test peers of `examples/test-programs.rs` are and how a test
//! places one. Tests include this file as a module.
#![allow(dead_code)]
use std::{
    path::{Path, PathBuf},
    process::Command,
};

/// The built test peers. Cargo builds the examples together with the whole
/// test suite; before a run of a single test target they are built once.
pub fn program() -> PathBuf {
    let path = Path::new(env!("CARGO_BIN_EXE_flightdeck-rust"))
        .parent()
        .expect("target directory")
        .join("examples/test-programs");
    const BUILD: &str = "cargo build --locked --example test-programs";
    assert!(path.is_file(), "Build the test programs first: {BUILD}");
    // A run of a single test target does not rebuild them after a change.
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/test-programs.rs");
    let changed = |path: &Path| path.metadata().and_then(|info| info.modified()).ok();
    assert!(
        changed(&source) <= changed(&path),
        "The test programs are older than their source: {BUILD}"
    );
    path
}

/// A copy under the name of the program it stands in for. `cp` writes it, so
/// this process never holds the copy open for writing: a concurrent test's
/// fork cannot inherit such a descriptor and make the exec fail as "text file
/// busy".
pub fn place(path: &Path) {
    let copied = Command::new("cp")
        .arg("--reflink=auto")
        .arg(program())
        .arg(path)
        .status()
        .expect("cp");
    assert!(copied.success(), "copy to {}", path.display());
}

/// A test peer selected by its first argument.
pub fn peer(mode: &str) -> Command {
    let mut command = Command::new(program());
    command.arg(mode);
    command
}
