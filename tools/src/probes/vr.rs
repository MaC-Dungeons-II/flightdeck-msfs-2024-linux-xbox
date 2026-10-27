// SPDX-License-Identifier: MIT
//! A real OpenXR stereo session under Wine, in a new disposable prefix: the
//! launcher's own VR preparation, then swapchains, tracking and frame
//! submission through Direct3D 11 and 12, for the runner's renderer and
//! optionally the packaged graphics bundle. With `--simulate` Monado's
//! simulated headset and null compositor are used. No physical headset,
//! tracking quality or simulator compatibility is established.
use super::{
    Site,
    host::{self, Environment},
    lab,
};
use crate::util::{Result, require};
use serde_json::json;
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
    runtime_json: PathBuf,
    /// Also test the verified packaged graphics bundle
    #[arg(long)]
    bundle: Option<PathBuf>,
    #[arg(long, default_value = "/usr/include/openxr")]
    headers: PathBuf,
    /// Enable Monado's simulated headset and null compositor
    #[arg(long)]
    simulate: bool,
    /// New directory; never an existing game runtime
    #[arg(long)]
    output: PathBuf,
}

#[derive(serde::Serialize)]
struct Outcome {
    renderer: &'static str,
    api: &'static str,
    passed: bool,
    exit_code: i32,
}

#[derive(serde::Serialize)]
struct Report<'a> {
    scope: &'static str,
    simulated: bool,
    physical_headset_validated: bool,
    cases: &'a [Outcome],
}

/// The scrubbed host environment with the prefix, the native OpenXR runtime
/// manifest and, when simulating, Monado's headset without a compositor.
fn environment(
    mut host: Environment,
    prefix: &Path,
    manifest: &Path,
    simulate: bool,
) -> Environment {
    host::set(
        &mut host,
        [
            ("WINEPREFIX", &host::text(prefix)),
            ("WINEDEBUG", "-all"),
            ("WINEESYNC", "0"),
            ("WINEFSYNC", "0"),
            ("XR_RUNTIME_JSON", &host::text(manifest)),
            ("DXVK_LOG_LEVEL", "info"),
            ("VKD3D_DEBUG", "warn"),
        ],
    );
    if simulate {
        host::set(
            &mut host,
            [("SIMULATED_ENABLE", "1"), ("XRT_COMPOSITOR_NULL", "1")],
        );
    }
    host
}

/// The renderer each case must report and the bundle that selects it. A
/// bundle path that does not exist leaves the runner's own libraries.
fn cases(output: &Path, bundle: Option<PathBuf>) -> Vec<(&'static str, PathBuf)> {
    let mut cases = vec![("runner", output.join("no-bundle"))];
    cases.extend(bundle.map(|bundle| ("backport", bundle)));
    cases
}

/// A clean exit alone is not enough: the program must report its own pass.
fn passed(exit_code: i32, log: &str) -> bool {
    exit_code == 0 && log.contains("PASS:")
}

pub fn run(site: &Site, arguments: &Arguments) -> Result<u8> {
    let (root, lab) = (site.root, &site.lab);
    let runner = host::canonical(&arguments.runner)?;
    let manifest = host::canonical(&arguments.runtime_json)?;
    if !host::proton_style(&runner) {
        return super::refused("--runner must name a Proton-style runner");
    }
    let output = host::resolve(&arguments.output)?;
    host::new_dirs(&output, host::DEFAULT)?;
    let runtime = output.join("runtime");
    host::new_dirs(&runtime.join("private"), host::PRIVATE)?;
    host::new_dir(&runtime.join("local"), host::DEFAULT)?;
    std::os::unix::fs::symlink(&runner, runtime.join("runner"))?;
    host::write(
        &runtime.join("private/vr-settings.json"),
        r#"{"schema":1,"mode":"auto"}"#,
    )?;
    let include = output.join("include");
    host::copytree(&arguments.headers, &include.join("openxr"), false)?;
    let binary = output.join("openxr-stereo.exe");
    let mut include_flag = std::ffi::OsString::from("-I");
    include_flag.push(&include);
    host::checked(
        Command::new(&site.compiler)
            .args([
                "-O2",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-Wno-missing-field-initializers",
            ])
            .arg(include_flag)
            .arg(root.join("tests/graphics/openxr-stereo.c"))
            .arg("-o")
            .arg(&binary)
            .args(["-ld3d11", "-ld3d12", "-ldxgi", "-ldxguid"]),
    )?;
    let prefix = runtime.join("local/msfs-prefix");
    lab::mark(&runtime)?;
    let root_input = host::text(&runtime);
    lab.call("prefix", json!({"root": root_input}))?;
    let env = environment(
        host::scrubbed(std::env::vars_os()),
        &prefix,
        &manifest,
        arguments.simulate,
    );
    let bundle = arguments
        .bundle
        .as_deref()
        .map(host::canonical)
        .transpose()?;
    let wineserver = runner.join("files/bin/wineserver");
    let mut outcomes = Vec::new();
    for (name, bundle) in cases(&output, bundle) {
        let renderer = lab.call(
            "renderer",
            json!({"root": root_input, "bundle": host::text(&bundle)}),
        )?;
        require(
            renderer == name,
            "The renderer bundle does not match the runner",
        )?;
        let mut prepared: Environment = serde_json::from_value(
            lab.call("vr", json!({"root": root_input, "environment": env}))?,
        )?;
        for (key, part) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_STATE_HOME", "state"),
        ] {
            let directory = runtime.join("private/xdg").join(part);
            host::dirs(&directory, host::DEFAULT)?;
            prepared.insert(key.into(), host::text(&directory));
        }
        for api in ["d3d11", "d3d12"] {
            let path = output.join(format!("{name}-{api}.log"));
            let ran = host::logged(
                Command::new(runner.join("files/bin/wine"))
                    .arg(&binary)
                    .arg(api)
                    .current_dir(&output)
                    .env_clear()
                    .envs(&prepared),
                &path,
                60,
            )
            .and_then(|exit_code| {
                let log = fs::read(&path)?;
                Ok(Outcome {
                    renderer: name,
                    api,
                    passed: passed(exit_code, &String::from_utf8_lossy(&log)),
                    exit_code,
                })
            });
            let stopped = host::stop_wine(&wineserver, &prepared, |command| {
                command.stdout(Stdio::null()).stderr(Stdio::null());
            });
            outcomes.push(host::finally(ran, stopped)?);
        }
    }
    let report = host::pretty(&Report {
        scope: "Wine OpenXR stereo swapchains, tracking and frame submission",
        simulated: arguments.simulate,
        physical_headset_validated: false,
        cases: &outcomes,
    })?;
    host::write(&output.join("result.json"), &report)?;
    print!("{report}");
    Ok((!outcomes.iter().all(|row| row.passed)).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn the_environment_names_the_runtime_and_simulates_only_on_request() {
        let host = || Environment::from([("DISPLAY".to_string(), ":0".to_string())]);
        let real = environment(
            host(),
            Path::new("/out/runtime/local/msfs-prefix"),
            Path::new("/usr/share/openxr/1/openxr_monado.json"),
            false,
        );
        assert_eq!(
            real,
            [
                ("DISPLAY", ":0"),
                ("DXVK_LOG_LEVEL", "info"),
                ("VKD3D_DEBUG", "warn"),
                ("WINEDEBUG", "-all"),
                ("WINEESYNC", "0"),
                ("WINEFSYNC", "0"),
                ("WINEPREFIX", "/out/runtime/local/msfs-prefix"),
                ("XR_RUNTIME_JSON", "/usr/share/openxr/1/openxr_monado.json"),
            ]
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .into()
        );
        let simulated = environment(host(), Path::new("/p"), Path::new("/m.json"), true);
        assert_eq!(simulated["SIMULATED_ENABLE"], "1");
        assert_eq!(simulated["XRT_COMPOSITOR_NULL"], "1");
        assert_eq!(simulated.len(), real.len() + 2);
    }

    #[test]
    fn the_runner_is_always_tested_and_a_bundle_adds_the_backport() {
        let output = Path::new("/out");
        assert_eq!(
            cases(output, None),
            [("runner", PathBuf::from("/out/no-bundle"))]
        );
        assert_eq!(
            cases(output, Some("/graphics".into())),
            [
                ("runner", PathBuf::from("/out/no-bundle")),
                ("backport", PathBuf::from("/graphics"))
            ]
        );
    }

    #[test]
    fn a_case_passes_only_with_a_clean_exit_and_the_programs_own_verdict() {
        assert!(passed(0, "frames=90\nPASS: d3d12 stereo\n"));
        assert!(!passed(0, "frames=90\n"));
        assert!(!passed(1, "PASS: d3d12 stereo\n"));
        assert!(!passed(-6, "PASS:"));
        assert!(!passed(0, "pass: lower case"));
    }

    #[test]
    fn the_result_keeps_the_keys_of_the_python_report() {
        let outcomes = [Outcome {
            renderer: "runner",
            api: "d3d11",
            passed: true,
            exit_code: 0,
        }];
        let report = host::pretty(&Report {
            scope: "Wine OpenXR stereo swapchains, tracking and frame submission",
            simulated: true,
            physical_headset_validated: false,
            cases: &outcomes,
        })
        .expect("report");
        assert_eq!(
            serde_json::from_str::<Value>(&report).expect("JSON"),
            json!({"scope": "Wine OpenXR stereo swapchains, tracking and frame submission",
                "simulated": true, "physical_headset_validated": false,
                "cases": [{"renderer": "runner", "api": "d3d11", "passed": true, "exit_code": 0}]})
        );
    }
}
