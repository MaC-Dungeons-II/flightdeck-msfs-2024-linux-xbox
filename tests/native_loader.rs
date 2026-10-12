// SPDX-License-Identifier: MIT
//! The loader with a synthetic PE image and a stand-in for Wine: images that
//! arrive as inherited descriptors reach Wine, the owned game is left as it
//! was, and the startup arguments apply to both games in both loader modes.
#![allow(clippy::unwrap_used)]
#[path = "fixtures/programs.rs"]
mod programs;
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    os::{fd::AsRawFd, unix::fs::symlink},
    path::Path,
    process::Command,
};

struct Launch<'a> {
    game: &'a str,
    portable: bool,
    arguments: &'a [&'a str],
    /// `FLIGHTDECK_FAST_LAUNCH`, when the user has chosen.
    setting: Option<&'a str>,
    expected: &'a [&'a str],
}

impl Default for Launch<'_> {
    fn default() -> Self {
        Self {
            game: "msfs2024",
            portable: false,
            arguments: &[],
            setting: None,
            expected: &["-FastLaunch"],
        }
    }
}

/// An anonymous file the launched program inherits by its number.
fn memory(name: &str, content: &[u8]) -> fs::File {
    let mut file =
        fs::File::from(rustix::fs::memfd_create(name, rustix::fs::MemfdFlags::empty()).unwrap());
    file.write_all(content).unwrap();
    file
}

/// The smallest image the loader accepts: headers, and data past them that
/// exists only in memory.
fn image() -> Vec<u8> {
    let mut content = vec![0; 512];
    content[..2].copy_from_slice(b"MZ");
    content[0x3c..0x40].copy_from_slice(&128u32.to_le_bytes());
    content[128..132].copy_from_slice(b"PE\0\0");
    content[128 + 24 + 60..128 + 24 + 64].copy_from_slice(&256u32.to_le_bytes());
    content[500..].copy_from_slice(b"only-in-ram!");
    content
}

fn windows(path: &Path) -> String {
    format!(r"\??\Z:{}", path.to_str().unwrap().replace('/', r"\"))
}

fn launch(case: &Launch) {
    let temp = tempfile::tempdir().unwrap();
    let (root, runtime, game) = (
        temp.path(),
        temp.path().join("runtime"),
        temp.path().join("owned game"),
    );
    let (directory, executable) = match case.game {
        "msfs2020" => ("MSFS2020", "FlightSimulator.exe"),
        _ => ("MSFS2024", "FlightSimulator2024.exe"),
    };
    for folder in ["tools", "games", "private", "local/msfs-prefix"] {
        fs::create_dir_all(runtime.join(folder)).unwrap();
    }
    fs::create_dir_all(game.join("nested")).unwrap();
    symlink(&game, runtime.join("games").join(directory)).unwrap();
    fs::write(
        runtime.join("private/runtime.json"),
        format!(r#"{{"game_id": "{}"}}"#, case.game),
    )
    .unwrap();
    let (source, library) = (game.join(executable), game.join("nested/probe.dll"));
    fs::write(&source, "encrypted placeholder").unwrap();
    fs::write(&library, "encrypted DLL placeholder").unwrap();
    fs::write(game.join("asset.dat"), "ordinary resource").unwrap();
    let wine = root.join("fake-wine");
    programs::place(&wine);
    let loader = memory("synthetic-loader", &image());
    let dll = memory("synthetic-dll", b"memory-backed DLL");
    let target = windows(&source);
    let mut command = Command::new(env!("CARGO_BIN_EXE_flightdeck-rust"));
    command
        .args(["wine-launch", "--runtime"])
        .arg(&runtime)
        .args(["--", &target])
        .args(case.arguments)
        .env("WINEPREFIX", runtime.join("local/msfs-prefix"))
        .env("XODUS_WINE_RUNNER", &wine)
        .env(
            "WINE_DLL_FILE_MAP",
            format!(
                "{}:{target}|{}:{}",
                loader.as_raw_fd(),
                dll.as_raw_fd(),
                windows(&library)
            ),
        )
        .env(
            "FLIGHTDECK_EXPECT_ARGUMENTS",
            serde_json::to_string(case.expected).unwrap(),
        )
        .env(
            "FLIGHTDECK_PROTON_LOADER",
            if case.portable { "portable" } else { "native" },
        )
        .env_remove("FLIGHTDECK_FAST_LAUNCH");
    if let Some(setting) = case.setting {
        command.env("FLIGHTDECK_FAST_LAUNCH", setting);
    }
    let result = command.output().unwrap();
    let errors = String::from_utf8_lossy(&result.stderr);
    assert_eq!(result.status.code(), Some(7), "{errors}");
    assert!(errors.contains("exit_code=7"), "{errors}");
    let entries: BTreeSet<_> = fs::read_dir(&game)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(
        entries,
        BTreeSet::from([source.clone(), game.join("nested"), game.join("asset.dat")])
    );
    assert_eq!(fs::read(&source).unwrap(), b"encrypted placeholder");
    assert_eq!(fs::read(&library).unwrap(), b"encrypted DLL placeholder");
    assert_eq!(
        fs::read(game.join("asset.dat")).unwrap(),
        b"ordinary resource"
    );
}

#[test]
fn external_owned_game_symlink_and_exit_status() {
    launch(&Launch::default());
}

#[test]
fn fastlaunch_reaches_both_games_through_both_loader_modes() {
    let arguments = ["--fixture", "space ; $(not-a-command) ü"];
    for game in ["msfs2020", "msfs2024"] {
        for portable in [false, true] {
            launch(&Launch {
                game,
                portable,
                arguments: &arguments,
                expected: &["-FastLaunch", arguments[0], arguments[1]],
                ..Launch::default()
            });
        }
    }
}

#[test]
fn explicit_argument_is_not_duplicated_or_rewritten() {
    let arguments = ["--fixture", "-fAsTlAuNcH", "original argument"];
    for portable in [false, true] {
        launch(&Launch {
            portable,
            arguments: &arguments,
            expected: &arguments,
            ..Launch::default()
        });
    }
}

#[test]
fn full_intro_opt_out_preserves_explicit_game_arguments() {
    for portable in [false, true] {
        for arguments in [["--fixture"], ["-FastLaunch"]] {
            launch(&Launch {
                portable,
                arguments: &arguments,
                setting: Some("0"),
                expected: &arguments,
                ..Launch::default()
            });
        }
    }
}
