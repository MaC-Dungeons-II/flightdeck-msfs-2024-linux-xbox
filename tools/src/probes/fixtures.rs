// SPDX-License-Identifier: MIT
//! Shell scripts in place of what the probes start: the probe driver, the
//! MinGW compiler and a Proton-style runner. With them a whole probe runs in
//! a temporary directory, without Wine, a GPU or a cross compiler. They show
//! that a probe's steps fit together, not that the real programs pass.
use super::{Probe, Site, lab::Lab};
use serde_json::Value;
use std::{
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

/// The command line of the probes, without the tool around it.
#[derive(clap::Parser)]
pub struct Line {
    #[command(subcommand)]
    pub probe: Probe,
}

/// The probe a command line selects; the words follow `probe`.
pub fn parsed(words: impl IntoIterator<Item = OsString>) -> Probe {
    use clap::Parser;
    match Line::try_parse_from([OsString::from("probe")].into_iter().chain(words)) {
        Ok(line) => line.probe,
        Err(error) => panic!("{error}"),
    }
}

/// An executable shell script. `cp` creates the file, so this process never
/// holds it open for writing: a concurrent test's fork cannot inherit such a
/// descriptor and make the exec fail as "text file busy".
pub fn script(path: &Path, body: &str) {
    let draft = path.with_extension("draft");
    fs::write(&draft, format!("#!/bin/sh\n{body}\n")).expect("draft");
    let copied = Command::new("cp")
        .arg(&draft)
        .arg(path)
        .status()
        .expect("cp");
    assert!(copied.success());
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("mode");
}

/// A driver that records every request and answers with the first matching
/// arm of a shell `case` over `$request`. Requests are compact JSON with
/// sorted keys, so `*'"operation":"prefix"'*` selects one operation.
pub fn lab(directory: &Path, arms: &str) -> Lab {
    let binary = directory.join("runtime-lab");
    script(
        &binary,
        &format!(
            "request=$(cat)\nprintf '%s\\n' \"$request\" >> \"$0.requests\"\ncase \"$request\" in\n{arms}\n\
             *) echo 'Unknown probe operation.' >&2; exit 1 ;;\nesac"
        ),
    );
    Lab::at(binary)
}

/// The requests the driver of `lab` has received so far.
pub fn requests(directory: &Path) -> Vec<Value> {
    fs::read_to_string(directory.join("runtime-lab.requests"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("request"))
        .collect()
}

/// A compiler that writes `built <name>` to its output file.
pub fn compiler(directory: &Path) -> PathBuf {
    let binary = directory.join("mingw-gcc");
    script(
        &binary,
        "printf '%s\\n' \"$*\" >> \"$0.calls\"\nwhile [ $# -gt 1 ]; do\n  \
         if [ \"$1\" = -o ]; then printf 'built %s' \"${2##*/}\" > \"$2\"; fi\n  shift\ndone",
    );
    binary
}

/// A runner whose `wine` is the given script and whose `wineserver` records
/// its option. Wine scripts get a cleared environment and use only builtins.
pub fn runner(directory: &Path, wine: &str) -> PathBuf {
    let runner = directory.join("runner");
    fs::create_dir_all(runner.join("files/bin")).expect("runner");
    script(&runner.join("files/bin/wine"), wine);
    script(
        &runner.join("files/bin/wineserver"),
        "printf '%s\\n' \"$1\" >> \"$0.calls\"",
    );
    fs::write(runner.join("version"), "Flightdeck-Runner-1\n").expect("version");
    runner
}

/// How often the runner's wineserver was asked to stop and to wait.
pub fn wineserver_calls(runner: &Path) -> String {
    fs::read_to_string(runner.join("files/bin/wineserver.calls")).unwrap_or_default()
}

/// The real checkout for sources, and scripts for the driver and compiler.
pub fn site(directory: &Path, arms: &str) -> Site<'static> {
    static ROOT: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(crate::util::root);
    Site {
        root: &ROOT,
        lab: lab(directory, arms),
        compiler: compiler(directory),
    }
}
