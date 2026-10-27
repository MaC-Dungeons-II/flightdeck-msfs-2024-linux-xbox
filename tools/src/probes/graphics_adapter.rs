// SPDX-License-Identifier: MIT
//! DXGI adapter selection and Direct3D 12 device creation through a supplied
//! runner, in a new isolated Wine prefix on the one discrete GPU. It compares
//! the old Linux-name filter with UUID selection, vendor hiding, a rejected
//! UUID and, on NVIDIA, both Flightdeck graphics modes as the launcher
//! prepares them. It establishes that the adapter is handed over and a device
//! is created; it does not validate simulator rendering or flight stability.
use super::{
    Site,
    host::{self, Environment},
    lab::{self, Gpu},
};
use crate::util::{Failure, Result};
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
    /// New directory for the isolated prefix and results
    #[arg(long)]
    output: Option<PathBuf>,
}

/// One start of the probe program. `expected` is unset for the old name
/// filter, whose outcome is recorded but decides nothing.
#[derive(Debug, PartialEq)]
struct Case {
    name: String,
    overrides: Environment,
    expected: Option<bool>,
}

#[derive(serde::Serialize)]
struct Outcome {
    case: String,
    exit_code: i32,
    expected_success: Option<bool>,
    success: bool,
}

#[derive(serde::Serialize)]
struct Report<'a> {
    scope: &'static str,
    gpu: &'a str,
    cases: &'a [Outcome],
}

const HIDE: [(u64, &str); 3] = [
    (lab::NVIDIA, "WINE_HIDE_NVIDIA_GPU"),
    (0x1002, "WINE_HIDE_AMD_GPU"),
    (0x8086, "WINE_HIDE_INTEL_GPU"),
];

/// The scrubbed host environment with the prefix and quiet, informative logs.
fn environment(mut host: Environment, prefix: &Path) -> Environment {
    host::set(
        &mut host,
        [
            ("WINEPREFIX", &host::text(prefix)),
            ("WINEDEBUG", "-all"),
            ("WINEESYNC", "0"),
            ("WINEFSYNC", "0"),
            ("DXVK_LOG_LEVEL", "info"),
            ("VKD3D_DEBUG", "info"),
        ],
    );
    host
}

/// The cases that need no launcher code: name filter, UUID, a UUID that
/// matches nothing and, for a known vendor, the UUID with that vendor hidden.
fn cases(gpu: &Gpu) -> Vec<Case> {
    let case = |name: &str, overrides: &[(&str, &str)], expected| Case {
        name: name.into(),
        overrides: overrides
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        expected,
    };
    let selection = ("DXVK_FILTER_DEVICE_UUID", gpu.uuid.as_str());
    let mut cases = vec![
        case(
            "legacy-host-name",
            &[
                ("DXVK_FILTER_DEVICE_NAME", &gpu.name),
                ("VKD3D_FILTER_DEVICE_NAME", &gpu.name),
            ],
            None,
        ),
        case("uuid", &[selection], Some(true)),
        case(
            "invalid-uuid",
            &[("DXVK_FILTER_DEVICE_UUID", &"0".repeat(32))],
            Some(false),
        ),
    ];
    if let Some((_, hide)) = HIDE.iter().find(|(vendor, _)| *vendor == gpu.vendor) {
        cases.push(case(
            "uuid-hidden-vendor",
            &[selection, (hide, "1")],
            Some(true),
        ));
    }
    cases
}

/// Whether any case with an expectation ended differently.
fn failed(outcomes: &[Outcome]) -> bool {
    outcomes.iter().any(|row| {
        row.expected_success
            .is_some_and(|expected| row.success != expected)
    })
}

pub fn run(site: &Site, arguments: &Arguments) -> Result<u8> {
    let (root, lab) = (site.root, &site.lab);
    let runner = host::canonical(&arguments.runner)?;
    if !host::proton_style(&runner) {
        return super::refused("--runner must name a Proton-style runner");
    }
    let Some(gpu) = lab::discrete(&lab.call("probe", Value::Null)?)? else {
        return super::refused(
            "This regression check requires one discrete Vulkan GPU with a device UUID",
        );
    };
    let output = match &arguments.output {
        Some(output) => {
            let output = host::resolve(output)?;
            host::new_dirs(&output, host::DEFAULT)?;
            output
        }
        None => tempfile::Builder::new()
            .prefix("flightdeck-graphics-")
            .tempdir()?
            .keep(),
    };
    let runtime = output.join("runtime");
    host::new_dirs(&runtime.join("private"), host::PRIVATE)?;
    host::new_dir(&runtime.join("local"), host::DEFAULT)?;
    std::os::unix::fs::symlink(&runner, runtime.join("runner"))?;
    let prefix = runtime.join("local/msfs-prefix");
    let binary = output.join("adapter-probe.exe");
    host::checked(
        Command::new(&site.compiler)
            .args(["-O2", "-Wall", "-Wextra", "-Werror"])
            .arg(root.join("tests/graphics/adapter-probe.c"))
            .arg("-o")
            .arg(&binary)
            .args(["-ld3d12", "-ldxgi", "-ldxguid"]),
    )?;
    lab::mark(&runtime)?;
    let root_input = host::text(&runtime);
    lab.call("prefix", json!({"root": root_input}))?;
    let environment = environment(host::scrubbed(std::env::vars_os()), &prefix);
    let mut cases = cases(&gpu);
    if gpu.vendor == lab::NVIDIA {
        for mode in ["auto", "compatibility"] {
            fs::write(
                runtime.join("private/graphics-settings.json"),
                format!(r#"{{"schema": 1, "nvidia_mode": "{mode}"}}"#),
            )?;
            let prepared = lab.call(
                "graphics",
                json!({"root": root_input, "environment": environment}),
            )?;
            // The driver answers with the environment and its report.
            let overrides = match prepared.as_array().map(Vec::as_slice) {
                Some([environment, _]) => serde_json::from_value(environment.clone())?,
                _ => return Err(Failure("Unexpected graphics preparation answer".into())),
            };
            cases.push(Case {
                name: format!("flightdeck-{mode}"),
                overrides,
                expected: Some(true),
            });
        }
    }
    let wineserver = runner.join("files/bin/wineserver");
    let mut outcomes = Vec::new();
    for case in cases {
        let mut env = environment.clone();
        env.extend(case.overrides);
        let ran = host::logged(
            Command::new(runner.join("files/bin/wine"))
                .arg(&binary)
                .env_clear()
                .envs(&env)
                .current_dir(&output),
            &output.join(format!("{}.log", case.name)),
            45,
        );
        let stopped = host::stop_wine(&wineserver, &env, |command| {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
        });
        let exit_code = host::finally(ran, stopped)?;
        outcomes.push(Outcome {
            case: case.name,
            exit_code,
            expected_success: case.expected,
            success: exit_code == 0,
        });
    }
    let report = host::pretty(&Report {
        scope: "adapter_selection_and_device_creation",
        gpu: &gpu.name,
        cases: &outcomes,
    })?;
    host::write(&output.join("result.json"), &report)?;
    println!("{report}Results: {}", output.display());
    Ok(failed(&outcomes).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(vendor: u64) -> Gpu {
        Gpu {
            name: "Radeon RX 6900 XT (RADV NAVI21)".into(),
            uuid: "00112233445566778899aabbccddeeff".into(),
            vendor,
        }
    }

    #[test]
    fn the_environment_points_wine_at_the_new_prefix() {
        let host = Environment::from([("HOME".to_string(), "/home/pilot".to_string())]);
        let environment = environment(host, Path::new("/out/runtime/local/msfs-prefix"));
        assert_eq!(
            environment,
            [
                ("DXVK_LOG_LEVEL", "info"),
                ("HOME", "/home/pilot"),
                ("VKD3D_DEBUG", "info"),
                ("WINEDEBUG", "-all"),
                ("WINEESYNC", "0"),
                ("WINEFSYNC", "0"),
                ("WINEPREFIX", "/out/runtime/local/msfs-prefix"),
            ]
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .into()
        );
    }

    #[test]
    fn a_known_vendor_adds_the_hidden_vendor_case() {
        let amd = cases(&gpu(0x1002));
        assert_eq!(
            amd.iter()
                .map(|case| case.name.as_str())
                .collect::<Vec<_>>(),
            [
                "legacy-host-name",
                "uuid",
                "invalid-uuid",
                "uuid-hidden-vendor"
            ]
        );
        assert_eq!(
            amd.iter().map(|case| case.expected).collect::<Vec<_>>(),
            [None, Some(true), Some(false), Some(true)]
        );
        let name = "Radeon RX 6900 XT (RADV NAVI21)";
        assert_eq!(amd[0].overrides["DXVK_FILTER_DEVICE_NAME"], name);
        assert_eq!(amd[0].overrides["VKD3D_FILTER_DEVICE_NAME"], name);
        assert_eq!(
            amd[1].overrides,
            Environment::from([(
                "DXVK_FILTER_DEVICE_UUID".to_string(),
                "00112233445566778899aabbccddeeff".to_string()
            )])
        );
        assert_eq!(amd[2].overrides["DXVK_FILTER_DEVICE_UUID"], "0".repeat(32));
        assert_eq!(amd[3].overrides["WINE_HIDE_AMD_GPU"], "1");
        assert_eq!(
            amd[3].overrides["DXVK_FILTER_DEVICE_UUID"],
            "00112233445566778899aabbccddeeff"
        );
        assert!(
            cases(&gpu(0x10de))[3]
                .overrides
                .contains_key("WINE_HIDE_NVIDIA_GPU")
        );
        assert!(
            cases(&gpu(0x8086))[3]
                .overrides
                .contains_key("WINE_HIDE_INTEL_GPU")
        );
        assert_eq!(cases(&gpu(0x5143)).len(), 3);
    }

    #[test]
    fn only_cases_with_an_expectation_decide_the_result() {
        let row = |expected_success, exit_code| Outcome {
            case: "case".into(),
            exit_code,
            expected_success,
            success: exit_code == 0,
        };
        assert!(!failed(&[
            row(None, 1),
            row(Some(true), 0),
            row(Some(false), 1)
        ]));
        assert!(!failed(&[row(None, 0)]));
        assert!(failed(&[row(Some(true), 0), row(Some(true), 1)]));
        assert!(failed(&[row(Some(false), 0)]));
        assert!(failed(&[row(Some(true), -9)]));
        assert!(!failed(&[]));
    }

    #[test]
    fn the_result_keeps_the_keys_of_the_python_report() {
        let outcomes = [Outcome {
            case: "legacy-host-name".into(),
            exit_code: 1,
            expected_success: None,
            success: false,
        }];
        let report = host::pretty(&Report {
            scope: "adapter_selection_and_device_creation",
            gpu: "GPU",
            cases: &outcomes,
        })
        .expect("report");
        assert_eq!(
            serde_json::from_str::<Value>(&report).expect("JSON"),
            json!({"scope": "adapter_selection_and_device_creation", "gpu": "GPU",
                "cases": [{"case": "legacy-host-name", "exit_code": 1,
                    "expected_success": null, "success": false}]})
        );
    }
}
