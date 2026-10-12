// SPDX-License-Identifier: MIT
//! Real rendering through a supplied runner in a new isolated Wine prefix, on
//! the one discrete GPU: a mixed Direct3D 11/12 program clears, reads back and
//! presents two windows with the runner's own renderer, with the supplied
//! bundle, and with the bundle in automatic mode. No game is launched and no
//! account is used. On other than NVIDIA hardware the automatic case checks
//! the explicit extension opt-out, not the NVIDIA driver or vendor hiding.
use super::{
    Site,
    host::{self, Environment},
    lab,
};
use crate::util::{Result, require};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(clap::Args)]
pub struct Arguments {
    #[arg(long)]
    runner: PathBuf,
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long)]
    output: PathBuf,
    /// Keep both colored windows visible longer
    #[arg(long)]
    visible: bool,
}

#[derive(serde::Serialize)]
struct Outcome {
    case: &'static str,
    exit_code: i32,
    passed: bool,
    blank_present_warning: bool,
}

#[derive(serde::Serialize)]
struct Report<'a> {
    scope: &'static str,
    gpu: &'a str,
    nvidia_hardware: bool,
    cases: &'a [Outcome],
}

/// Name, whether the bundle is installed, and the NVIDIA mode of each case.
const CASES: [(&str, bool, &str); 3] = [
    ("runner", false, "features"),
    ("backport", true, "features"),
    ("backport-automatic", true, "auto"),
];

/// The scrubbed host environment with the prefix, the selected GPU and logs
/// that show the renderer's own warnings.
fn environment(mut host: Environment, prefix: &Path, uuid: &str) -> Environment {
    host::set(
        &mut host,
        [
            ("WINEPREFIX", &host::text(prefix)),
            ("WINEESYNC", "0"),
            ("WINEFSYNC", "0"),
            ("WINEDEBUG", "-all"),
            ("DXVK_LOG_LEVEL", "info"),
            ("VKD3D_DEBUG", "warn"),
            ("DXVK_FILTER_DEVICE_UUID", uuid),
        ],
    );
    host
}

/// A clean exit alone is not enough: the program must report its own pass. A
/// present of a surface that was never rendered to is recorded separately.
fn outcome(case: &'static str, exit_code: i32, log: &str) -> Outcome {
    Outcome {
        case,
        exit_code,
        passed: exit_code == 0 && log.contains("PASS:"),
        blank_present_warning: log.contains("never been rendered to"),
    }
}

pub fn run(site: &Site, arguments: &Arguments) -> Result<u8> {
    let (root, lab) = (site.root, &site.lab);
    let runner = host::canonical(&arguments.runner)?;
    let bundle = host::canonical(&arguments.bundle)?;
    let output = host::resolve(&arguments.output)?;
    host::new_dirs(&output, host::DEFAULT)?;
    let runtime = output.join("runtime");
    host::new_dirs(&runtime.join("private"), host::DEFAULT)?;
    host::new_dir(&runtime.join("local"), host::DEFAULT)?;
    std::os::unix::fs::symlink(&runner, runtime.join("runner"))?;
    let prefix = runtime.join("local/msfs-prefix");
    let Some(gpu) = lab::discrete(&lab.call("probe", Value::Null)?)? else {
        return super::refused("Requires one discrete Vulkan GPU with a device UUID");
    };
    let binary = output.join("multiwindow.exe");
    host::checked(
        Command::new(&site.compiler)
            .args(["-O2", "-Wall", "-Wextra", "-Werror"])
            .arg(root.join("tests/graphics/multiwindow.c"))
            .arg("-o")
            .arg(&binary)
            .args(["-ld3d12", "-ld3d11", "-ldxgi", "-ldxguid", "-lgdi32"]),
    )?;
    lab::mark(&runtime)?;
    let root_input = host::text(&runtime);
    lab.call("prefix", json!({"root": root_input}))?;
    let environment = environment(host::scrubbed(std::env::vars_os()), &prefix, &gpu.uuid);
    let wineserver = runner.join("files/bin/wineserver");
    let mut outcomes = Vec::new();
    for (name, patched, mode) in CASES {
        fs::write(
            runtime.join("private/graphics-settings.json"),
            format!(r#"{{"schema": 1, "nvidia_mode": "{mode}"}}"#),
        )?;
        let prepared = lab.call(
            "graphics",
            json!({"root": root_input, "environment": environment}),
        )?;
        // The driver answers with the environment and its report.
        let mut env: Environment = match prepared.as_array().map(Vec::as_slice) {
            Some([environment, _]) => serde_json::from_value(environment.clone())?,
            _ => return crate::util::fail("Unexpected graphics preparation answer"),
        };
        // A packaged launcher can install its default bundle while preparing.
        // The runner's own pair is restored explicitly for the baseline.
        let selected = patched.then(|| host::text(&bundle));
        let state = lab.call("renderer", json!({"root": root_input, "bundle": selected}))?;
        require(
            state == if patched { "backport" } else { "runner" },
            "The supplied bundle does not match the runner",
        )?;
        if mode == "auto" && gpu.vendor != lab::NVIDIA {
            env = serde_json::from_value(
                lab.call("mode", json!({"environment": env, "mode": mode}))?,
            )?;
        }
        let path = output.join(format!("{name}.log"));
        let mut wine = Command::new(runner.join("files/bin/wine"));
        wine.arg(&binary)
            .current_dir(&output)
            .env_clear()
            .envs(&env);
        if arguments.visible && name == "backport" {
            wine.arg("--visible");
        }
        let ran = host::logged(&mut wine, &path, 90).and_then(|exit_code| {
            let log = fs::read(&path)?;
            Ok(outcome(name, exit_code, &String::from_utf8_lossy(&log)))
        });
        let stopped = host::stop_wine(&wineserver, &env, |command| {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        });
        outcomes.push(host::finally(ran, stopped)?);
    }
    let report = host::pretty(&Report {
        scope: "Mixed D3D11/D3D12 clear/readback/present and swapchain lifetime",
        gpu: &gpu.name,
        nvidia_hardware: gpu.vendor == lab::NVIDIA,
        cases: &outcomes,
    })?;
    host::write(&output.join("result.json"), &report)?;
    print!("{report}");
    Ok((!outcomes.iter().all(|row| row.passed)).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pass_needs_a_clean_exit_and_the_programs_own_verdict() {
        assert!(outcome("runner", 0, "info\nPASS: 2 windows\n").passed);
        assert!(!outcome("runner", 0, "info\n").passed);
        assert!(!outcome("runner", 1, "PASS: 2 windows\n").passed);
        let warned = outcome(
            "backport",
            0,
            "PASS: ok\nwarn: image has never been rendered to\n",
        );
        assert!(warned.passed && warned.blank_present_warning);
        assert!(!outcome("backport", 0, "PASS: ok\n").blank_present_warning);
    }

    #[test]
    fn the_environment_selects_the_gpu_in_the_new_prefix() {
        let host = Environment::from([("DISPLAY".to_string(), ":0".to_string())]);
        let env = environment(host, Path::new("/out/runtime/local/msfs-prefix"), "aabb");
        assert_eq!(env["WINEPREFIX"], "/out/runtime/local/msfs-prefix");
        assert_eq!(env["DXVK_FILTER_DEVICE_UUID"], "aabb");
        assert_eq!(env["VKD3D_DEBUG"], "warn");
        assert_eq!(env["DISPLAY"], ":0");
        assert_eq!(
            (env["WINEESYNC"].as_str(), env["WINEFSYNC"].as_str()),
            ("0", "0")
        );
    }
}
