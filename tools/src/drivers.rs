// SPDX-License-Identifier: MIT
//! Drivers of the compatibility component tests. Each takes a source tree
//! staged by `compat-stage`, builds a test program against it, runs that and
//! judges what it reports. All of them use synthetic data only: no account,
//! no Microsoft endpoint, no payment. The Wine drivers need a runner, the
//! window drivers a desktop session; none of this runs in CI.
mod common;
mod ipc_timeout;
mod login_flow;
mod run_native;
mod store_navigation;
mod store_window;

use crate::util::Result;
use std::path::{Path, PathBuf};

#[derive(clap::Subcommand)]
pub enum Driver {
    /// Compile and run account-free GameSave, Store and catalog tests.
    RunNative {
        #[arg(long)]
        stage: PathBuf,
        #[arg(long)]
        wine: PathBuf,
        /// Test family to run.
        #[arg(long, value_parser = run_native::SUITES, default_value = "all")]
        suite: String,
    },
    /// Exercise the built Wine IPC layer with delayed, account-free broker replies.
    IpcTimeout {
        #[arg(long)]
        stage: PathBuf,
        #[arg(long)]
        wine: PathBuf,
        /// A scenario to run instead of all; may be given several times.
        #[arg(long, value_parser = ipc_timeout::CASES)]
        case: Vec<String>,
    },
    /// Exercise the actual Linux Store webview with offline content, without accounts.
    StoreWindow {
        #[arg(long)]
        stage: PathBuf,
        /// Demonstrate the previous foreign-window attachment for comparison.
        #[arg(long)]
        legacy: bool,
        /// Render the window check page instead of the purchase host.
        #[arg(long)]
        health: bool,
        #[arg(long, value_parser = common::BACKENDS, default_value = "x11")]
        backend: String,
    },
    /// Regress the native Store form's origin using a loopback-only HTTP fixture.
    StoreNavigation {
        #[arg(long)]
        stage: PathBuf,
        #[arg(long, value_parser = common::BACKENDS, default_value = "x11")]
        backend: String,
    },
    /// Exercise login challenge handoff using loopback pages and synthetic tokens.
    LoginFlow {
        #[arg(long)]
        stage: PathBuf,
        /// A new directory for the build log, the logs of each mode and the result.
        #[arg(long)]
        output: PathBuf,
        #[arg(long, value_parser = common::BACKENDS, default_value = "x11")]
        backend: String,
    },
}

/// Runs a driver and returns its exit code: 1 when a test did not pass. A
/// driver that could not do its work at all fails instead.
pub fn run(root: &Path, command: Driver) -> Result<u8> {
    match command {
        Driver::RunNative { stage, wine, suite } => run_native::run(root, &stage, &wine, &suite),
        Driver::IpcTimeout { stage, wine, case } => ipc_timeout::run(root, &stage, &wine, &case),
        Driver::StoreWindow {
            stage,
            legacy,
            health,
            backend,
        } => store_window::run(&stage, legacy, health, &backend),
        Driver::StoreNavigation { stage, backend } => store_navigation::run(&stage, &backend),
        Driver::LoginFlow {
            stage,
            output,
            backend,
        } => login_flow::run(&stage, &output, &backend),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Line {
        #[command(subcommand)]
        driver: Driver,
    }
    fn parsed(words: &[&str]) -> std::result::Result<Driver, clap::Error> {
        Line::try_parse_from([&["flightdeck-tools"], words].concat()).map(|line| line.driver)
    }

    #[test]
    fn every_driver_keeps_the_flags_of_its_script() {
        let Ok(Driver::RunNative { stage, wine, suite }) = parsed(&[
            "run-native",
            "--stage",
            "build/compat",
            "--wine",
            "runner/files/bin/wine",
        ]) else {
            panic!("run-native");
        };
        assert_eq!(
            (stage.as_path(), wine.as_path(), suite.as_str()),
            (
                Path::new("build/compat"),
                Path::new("runner/files/bin/wine"),
                "all"
            )
        );
        for suite in run_native::SUITES {
            assert!(
                parsed(&[
                    "run-native",
                    "--stage",
                    "s",
                    "--wine",
                    "w",
                    "--suite",
                    suite
                ])
                .is_ok()
            );
        }
        assert!(
            parsed(&[
                "run-native",
                "--stage",
                "s",
                "--wine",
                "w",
                "--suite",
                "other"
            ])
            .is_err()
        );
        assert!(parsed(&["run-native", "--stage", "s"]).is_err());
        assert!(parsed(&["run-native", "--wine", "w"]).is_err());

        let Ok(Driver::IpcTimeout { case, .. }) =
            parsed(&["ipc-timeout", "--stage", "s", "--wine", "w"])
        else {
            panic!("ipc-timeout");
        };
        assert!(case.is_empty());
        let Ok(Driver::IpcTimeout { case, .. }) = parsed(&[
            "ipc-timeout",
            "--stage",
            "s",
            "--wine",
            "w",
            "--case",
            "auth",
            "--case",
            "account-timeout",
        ]) else {
            panic!("ipc-timeout");
        };
        assert_eq!(case, ["auth", "account-timeout"]);
        assert!(parsed(&["ipc-timeout", "--stage", "s", "--wine", "w", "--case", "x"]).is_err());
        assert!(parsed(&["ipc-timeout", "--stage", "s"]).is_err());

        let Ok(Driver::StoreWindow {
            legacy,
            health,
            backend,
            ..
        }) = parsed(&["store-window", "--stage", "s"])
        else {
            panic!("store-window");
        };
        assert_eq!((legacy, health, backend.as_str()), (false, false, "x11"));
        let Ok(Driver::StoreWindow {
            legacy,
            health,
            backend,
            ..
        }) = parsed(&[
            "store-window",
            "--stage",
            "s",
            "--legacy",
            "--health",
            "--backend",
            "wayland",
        ])
        else {
            panic!("store-window");
        };
        assert_eq!((legacy, health, backend.as_str()), (true, true, "wayland"));
        assert!(parsed(&["store-window"]).is_err());
        assert!(parsed(&["store-window", "--stage", "s", "--backend", "mir"]).is_err());

        let Ok(Driver::StoreNavigation { backend, .. }) =
            parsed(&["store-navigation", "--stage", "s"])
        else {
            panic!("store-navigation");
        };
        assert_eq!(backend, "x11");
        assert!(parsed(&["store-navigation", "--stage", "s", "--backend", "wayland"]).is_ok());
        assert!(parsed(&["store-navigation", "--stage", "s", "--legacy"]).is_err());

        let Ok(Driver::LoginFlow {
            output, backend, ..
        }) = parsed(&["login-flow", "--stage", "s", "--output", "build/check"])
        else {
            panic!("login-flow");
        };
        assert_eq!(
            (output.as_path(), backend.as_str()),
            (Path::new("build/check"), "x11")
        );
        assert!(parsed(&["login-flow", "--stage", "s"]).is_err());
        assert!(parsed(&["login-flow", "--output", "o"]).is_err());
        assert!(parsed(&["loader-test"]).is_err());
    }
}
