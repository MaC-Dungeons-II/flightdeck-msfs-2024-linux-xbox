// SPDX-License-Identifier: MIT
//! Optional developer probes. They need what CI does not have: a GPU, a Wine
//! runner, an installed runtime, an OpenXR runtime or two built packages.
//! Each one works in a new directory with synthetic programs and never starts
//! a game or uses an account. The exit status is the probe's verdict: 0 for a
//! pass, 1 for a failure and 2 for a refused host or an inconclusive run.
use crate::util::Result;
use std::path::{Path, PathBuf};

mod benchmark;
mod edition_update;
#[cfg(test)]
mod fixtures;
mod graphics_adapter;
mod graphics_rendering;
mod host;
pub(crate) mod lab;
mod media;
mod proton;
mod sync_fenix;
mod transition_update;
mod volume_layout;
mod vr;

#[derive(clap::Subcommand)]
pub enum Probe {
    /// Optional GPU integration check. Always create a new, isolated Wine prefix.
    GraphicsAdapter(graphics_adapter::Arguments),
    /// Exercise real rendering in new prefixes; never launch a game or use an account.
    GraphicsRendering(graphics_rendering::Arguments),
    /// Optional real OpenXR stereo test using an isolated, disposable Wine prefix.
    Vr(vr::Arguments),
    /// Reproduce the maintenance9 3D-copy layout bug in an isolated Wine prefix.
    VolumeLayout(volume_layout::Arguments),
    /// Test the Flightdeck runner or Proton switching with own PEs and fresh prefixes.
    Proton(proton::Arguments),
    /// Decode and rewind a local video through Wine Media Foundation.
    Media(media::Arguments),
    /// Verify automatic inactive-edition migration using an isolated synthetic game.
    EditionUpdate(edition_update::Arguments),
    /// Compare isolated launchers and identical save workloads; never start Wine/MSFS.
    BenchmarkLaunchers(benchmark::Arguments),
    /// Explicit maintainer import from the matching, locally built Fenix project.
    SyncFenix(sync_fenix::Arguments),
    /// Exercise legacy -> bridge -> native updates with real packages and local GitHub fixtures.
    TransitionUpdate(transition_update::Arguments),
}

/// A host or argument the probe cannot work with, reported like a usage
/// error: exit status 2 instead of a failed check.
fn refused(message: &str) -> Result<u8> {
    eprintln!("error: {message}");
    Ok(2)
}

/// What a Wine probe takes from the checkout and the host: the test sources,
/// the probe driver and the MinGW compiler.
struct Site<'a> {
    root: &'a Path,
    lab: lab::Lab,
    compiler: PathBuf,
}

pub fn run(root: &Path, command: Probe) -> Result<u8> {
    let site = Site {
        root,
        lab: lab::Lab::new(root),
        compiler: "x86_64-w64-mingw32-gcc".into(),
    };
    dispatch(&site, command)
}

fn dispatch(site: &Site, command: Probe) -> Result<u8> {
    match command {
        Probe::GraphicsAdapter(arguments) => graphics_adapter::run(site, &arguments),
        Probe::GraphicsRendering(arguments) => graphics_rendering::run(site, &arguments),
        Probe::Vr(arguments) => vr::run(site, &arguments),
        Probe::VolumeLayout(arguments) => volume_layout::run(site, &arguments),
        Probe::Proton(arguments) => proton::run(site, &arguments),
        Probe::Media(arguments) => media::run(site, &arguments),
        Probe::EditionUpdate(arguments) => edition_update::run(&arguments),
        Probe::BenchmarkLaunchers(arguments) => benchmark::run(&arguments),
        Probe::SyncFenix(arguments) => sync_fenix::run(site.root, &arguments),
        Probe::TransitionUpdate(arguments) => transition_update::run(&arguments),
    }
}

#[cfg(test)]
mod staged_tests;

#[cfg(test)]
mod tests {
    use super::{fixtures::Line, *};
    use clap::{CommandFactory, Parser};

    fn accepted(words: &[&str]) -> bool {
        Line::try_parse_from(["probe"].iter().chain(words)).is_ok()
    }

    /// The long flags of one probe, as its help lists them.
    fn flags(name: &str) -> Vec<String> {
        let command = Line::command();
        let probe = command.find_subcommand(name).expect(name);
        let mut flags: Vec<String> = probe
            .get_arguments()
            .filter_map(|argument| argument.get_long().map(str::to_string))
            .collect();
        flags.sort();
        flags
    }

    #[test]
    fn every_script_kept_its_flags() {
        for (name, expected) in [
            ("graphics-adapter", &["output", "runner"][..]),
            (
                "graphics-rendering",
                &["bundle", "output", "runner", "visible"],
            ),
            (
                "vr",
                &[
                    "bundle",
                    "headers",
                    "output",
                    "runner",
                    "runtime-json",
                    "simulate",
                ],
            ),
            (
                "volume-layout",
                &[
                    "baseline",
                    "bundle",
                    "output",
                    "runner",
                    "validation-layer-path",
                ],
            ),
            (
                "proton",
                &[
                    "default-runner",
                    "fenix-bundle",
                    "native-launcher",
                    "output",
                    "runner",
                    "runtime",
                ],
            ),
            ("edition-update", &["package", "previous-package"]),
            (
                "benchmark-launchers",
                &[
                    "binary",
                    "output",
                    "python",
                    "python-commit",
                    "python-launcher",
                    "python-source",
                    "repeats",
                    "requests",
                    "rust-launcher",
                ],
            ),
            ("sync-fenix", &[]),
            (
                "transition-update",
                &[
                    "bridge-archive",
                    "legacy-package",
                    "native-archive",
                    "output",
                    "python",
                ],
            ),
        ] {
            let found: Vec<String> = flags(name)
                .into_iter()
                .filter(|flag| flag != "help")
                .collect();
            assert_eq!(found, expected, "{name}");
        }
    }

    #[test]
    fn required_flags_and_the_runner_choice_are_enforced() {
        assert!(accepted(&["graphics-adapter", "--runner", "r"]));
        assert!(accepted(&[
            "graphics-adapter",
            "--runner",
            "r",
            "--output",
            "o"
        ]));
        assert!(!accepted(&["graphics-adapter", "--output", "o"]));

        let vr = [
            "vr",
            "--runner",
            "r",
            "--runtime-json",
            "j",
            "--output",
            "o",
        ];
        assert!(accepted(&vr));
        assert!(accepted(
            &[&vr[..], &["--simulate", "--bundle", "b", "--headers", "h"]].concat()
        ));
        assert!(!accepted(&vr[..5]));
        assert!(!accepted(&["vr", "--runner", "r", "--output", "o"]));

        let volume = [
            "volume-layout",
            "--runner",
            "r",
            "--baseline",
            "a",
            "--bundle",
            "b",
            "--output",
            "o",
        ];
        assert!(accepted(&volume));
        assert!(accepted(
            &[&volume[..], &["--validation-layer-path", "l"]].concat()
        ));
        assert!(!accepted(&volume[..7]));

        let proton = ["proton", "--runtime", "r", "--output", "o"];
        assert!(!accepted(&proton));
        assert!(accepted(&[&proton[..], &["--default-runner"]].concat()));
        assert!(accepted(&[&proton[..], &["--runner", "p"]].concat()));
        assert!(!accepted(
            &[&proton[..], &["--runner", "p", "--default-runner"]].concat()
        ));
        assert!(accepted(
            &[
                &proton[..],
                &[
                    "--runner",
                    "p",
                    "--fenix-bundle",
                    "f",
                    "--native-launcher",
                    "n"
                ]
            ]
            .concat()
        ));
        assert!(!accepted(&["proton", "--default-runner", "--output", "o"]));

        assert!(accepted(&[
            "edition-update",
            "--package",
            "p",
            "--previous-package",
            "q"
        ]));
        assert!(!accepted(&["edition-update", "--package", "p"]));

        let benchmark = [
            "benchmark-launchers",
            "--binary",
            "b",
            "--python-source",
            "s",
            "--python-commit",
            "c",
            "--output",
            "o",
        ];
        assert!(accepted(&benchmark));
        assert!(accepted(
            &[
                &benchmark[..],
                &[
                    "--rust-launcher",
                    "r",
                    "--python-launcher",
                    "p",
                    "--repeats",
                    "3",
                    "--requests",
                    "9"
                ]
            ]
            .concat()
        ));
        assert!(!accepted(
            &[&benchmark[..], &["--repeats", "many"]].concat()
        ));
        assert!(!accepted(&benchmark[..7]));

        assert!(accepted(&["sync-fenix", "/patch"]));
        assert!(!accepted(&["sync-fenix"]));
        assert!(!accepted(&["sync-fenix", "/patch", "/other"]));
        assert!(!accepted(&["check-graphics-rendering"]));
    }

    #[test]
    fn defaults_are_those_of_the_scripts() {
        let command = Line::command();
        let default = |probe: &str, flag: &str| {
            command
                .find_subcommand(probe)
                .expect(probe)
                .get_arguments()
                .find(|argument| argument.get_long() == Some(flag))
                .expect(flag)
                .get_default_values()
                .iter()
                .map(|value| value.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(default("vr", "headers"), ["/usr/include/openxr"]);
        assert_eq!(default("benchmark-launchers", "repeats"), ["7"]);
        assert_eq!(default("benchmark-launchers", "requests"), ["50"]);
        assert_eq!(default("benchmark-launchers", "python"), ["python3"]);
        assert!(default("graphics-adapter", "output").is_empty());
    }

    #[test]
    fn a_refused_host_is_a_usage_error_not_a_failed_check() {
        assert_eq!(
            refused("--runner must name a Proton-style runner").expect("code"),
            2
        );
    }
}
