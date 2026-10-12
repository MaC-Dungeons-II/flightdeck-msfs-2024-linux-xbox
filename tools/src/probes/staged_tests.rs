// SPDX-License-Identifier: MIT
//! Whole probes, from the command line to the result file, with scripts in
//! place of the probe driver, the compiler, Wine and the launchers. This
//! establishes that the steps, directories, environments, requests and
//! verdicts fit together. It cannot show that real Wine, a GPU, OpenXR or a
//! launcher passes; that needs the hardware these probes exist for.
use super::{
    Site, dispatch,
    fixtures::{self, script},
    host,
    lab::{self, Lab},
};
use crate::{
    http::fixtures::Service,
    util::{Result, digest},
};
use serde_json::{Value, json};
use std::{
    ffi::OsStr,
    fs,
    os::unix::fs::{MetadataExt, symlink},
    path::{Path, PathBuf},
};

const PREFIX: &str = r#"*'"operation":"prefix"'*) echo true ;;"#;
const NVIDIA: &str = r#"*'"operation":"probe"'*) echo '{"devices":[{"type":1,"name":"iGPU","vendor_id":32902,"device_uuid":"11"},{"type":2,"name":"RTX","vendor_id":4318,"device_uuid":"aabb"}]}' ;;"#;
const AMD: &str = r#"*'"operation":"probe"'*) echo '{"devices":[{"type":2,"name":"RX","vendor_id":4098,"device_uuid":"ccdd"}]}' ;;"#;
const TWO_GPUS: &str = r#"*'"operation":"probe"'*) echo '{"devices":[{"type":2,"name":"A","vendor_id":4098,"device_uuid":"aa"},{"type":2,"name":"B","vendor_id":4318,"device_uuid":"bb"}]}' ;;"#;

fn probe(site: &Site, words: &[&dyn AsRef<OsStr>]) -> Result<u8> {
    dispatch(
        site,
        fixtures::parsed(words.iter().map(|word| word.as_ref().to_owned())),
    )
}

/// A new working directory, by its real path as the probes resolve it.
fn work() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp");
    let path = fs::canonicalize(temp.path()).expect("work");
    (temp, path)
}

fn text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").mode() & 0o777
}

fn operations(directory: &Path) -> Vec<String> {
    fixtures::requests(directory)
        .iter()
        .map(|request| {
            request["operation"]
                .as_str()
                .expect("operation")
                .to_string()
        })
        .collect()
}

const ADAPTER_WINE: &str = r#"printf 'uuid=%s name=%s hide=%s prepared=%s prefix=%s cwd=%s program=%s\n' "$DXVK_FILTER_DEVICE_UUID" "$DXVK_FILTER_DEVICE_NAME" "$WINE_HIDE_NVIDIA_GPU" "$PREPARED" "$WINEPREFIX" "$PWD" "$1"
echo diagnostics >&2
case "$DXVK_FILTER_DEVICE_UUID" in 0000*) exit 1 ;; esac
if [ -n "$DXVK_FILTER_DEVICE_NAME" ]; then exit 3; fi"#;

#[test]
fn the_adapter_probe_runs_every_case_and_records_the_result() {
    let (_temp, work) = work();
    let graphics = r#"*'"operation":"graphics"'*) echo '[{"PREPARED":"yes","DXVK_FILTER_DEVICE_UUID":"aabb"},{"mode":"reported"}]' ;;"#;
    let site = fixtures::site(&work, &format!("{NVIDIA}\n{PREFIX}\n{graphics}"));
    let runner = fixtures::runner(&work, ADAPTER_WINE);
    let output = work.join("results/adapter");
    let words: [&dyn AsRef<OsStr>; 5] = [
        &"graphics-adapter",
        &"--runner",
        &runner,
        &"--output",
        &output,
    ];
    assert_eq!(probe(&site, &words).expect("probe"), 0);
    let case = |name: &str, exit_code: i32, expected: Value| {
        json!({"case": name, "exit_code": exit_code, "expected_success": expected,
            "success": exit_code == 0})
    };
    assert_eq!(
        host::read_json(&output.join("result.json")).expect("result"),
        json!({"scope": "adapter_selection_and_device_creation", "gpu": "RTX", "cases": [
            case("legacy-host-name", 3, json!(null)),
            case("uuid", 0, json!(true)),
            case("invalid-uuid", 1, json!(false)),
            case("uuid-hidden-vendor", 0, json!(true)),
            case("flightdeck-auto", 0, json!(true)),
            case("flightdeck-compatibility", 0, json!(true)),
        ]})
    );
    let runtime = output.join("runtime");
    let prefix = runtime.join("local/msfs-prefix");
    let program = output.join("adapter-probe.exe");
    assert_eq!(
        text(&output.join("uuid.log")),
        format!(
            "uuid=aabb name= hide= prepared= prefix={} cwd={} program={}\ndiagnostics\n",
            prefix.display(),
            output.display(),
            program.display()
        )
    );
    assert!(text(&output.join("legacy-host-name.log")).starts_with("uuid= name=RTX hide= "));
    assert!(
        text(&output.join("invalid-uuid.log")).starts_with(&format!("uuid={} ", "0".repeat(32)))
    );
    assert!(text(&output.join("uuid-hidden-vendor.log")).starts_with("uuid=aabb name= hide=1 "));
    for mode in ["auto", "compatibility"] {
        assert!(
            text(&output.join(format!("flightdeck-{mode}.log")))
                .starts_with("uuid=aabb name= hide= prepared=yes "),
            "{mode}"
        );
    }
    assert_eq!(fixtures::wineserver_calls(&runner), "-k\n-w\n".repeat(6));
    assert_eq!(fs::read_link(runtime.join("runner")).expect("link"), runner);
    assert_eq!(text(&runtime.join("private/synthetic-probe")), lab::MARKER);
    assert_eq!(mode(&runtime.join("private")), 0o700);
    assert_eq!(
        text(&runtime.join("private/graphics-settings.json")),
        r#"{"schema": 1, "nvidia_mode": "compatibility"}"#
    );
    assert_eq!(text(&program), "built adapter-probe.exe");
    assert_eq!(
        text(&work.join("mingw-gcc.calls")),
        format!(
            "-O2 -Wall -Wextra -Werror {} -o {} -ld3d12 -ldxgi -ldxguid\n",
            site.root.join("tests/graphics/adapter-probe.c").display(),
            program.display()
        )
    );
    let requests = fixtures::requests(&work);
    assert_eq!(
        operations(&work),
        ["probe", "prefix", "graphics", "graphics"]
    );
    assert_eq!(
        requests[1],
        json!({"operation": "prefix", "root": host::text(&runtime)})
    );
    let sent = &requests[3]["environment"];
    assert_eq!(sent["WINEPREFIX"], host::text(&prefix));
    assert_eq!(sent["VKD3D_DEBUG"], "info");
    assert_eq!(sent.get("DXVK_FILTER_DEVICE_UUID"), None);
    // The results of an earlier run are never overwritten.
    assert!(probe(&site, &words).is_err());
}

#[test]
fn the_adapter_probe_fails_on_a_wrong_outcome_and_refuses_unsuitable_hosts() {
    let (_temp, work) = work();
    let site = fixtures::site(&work, &format!("{AMD}\n{PREFIX}"));
    let runner = fixtures::runner(&work, "exit 0");
    let run = |site: &Site, output: &Path| {
        probe(
            site,
            &[
                &"graphics-adapter",
                &"--runner",
                &runner,
                &"--output",
                &output,
            ],
        )
    };
    // Every start succeeds, so the UUID that matches nothing was accepted.
    let accepted = work.join("accepted");
    assert_eq!(run(&site, &accepted).expect("probe"), 1);
    let result = host::read_json(&accepted.join("result.json")).expect("result");
    let cases = result["cases"].as_array().expect("cases");
    assert_eq!(
        cases.iter().map(|row| &row["case"]).collect::<Vec<_>>(),
        [
            "legacy-host-name",
            "uuid",
            "invalid-uuid",
            "uuid-hidden-vendor"
        ]
    );
    assert_eq!(cases[2]["success"], true);
    assert!(text(&work.join("mingw-gcc.calls")).contains("adapter-probe.c"));

    let (_other, elsewhere) = self::work();
    let crowded = fixtures::site(&elsewhere, TWO_GPUS);
    let refused = work.join("refused");
    assert_eq!(run(&crowded, &refused).expect("refused"), 2);
    assert!(!refused.exists());

    let missing = Site {
        root: site.root,
        lab: Lab::at(work.join("not-built")),
        compiler: site.compiler.clone(),
    };
    assert_eq!(
        run(&missing, &refused).expect_err("driver").0,
        lab::BUILD_FIRST
    );
    fs::remove_file(runner.join("files/bin/wineserver")).expect("wineserver");
    assert_eq!(run(&missing, &refused).expect("runner"), 2);
    assert!(!refused.exists());
}

const VR_ARMS: &str = r#"*'"operation":"renderer"'*) case "$request" in *no-bundle*) echo '"runner"' ;; *) echo '"backport"' ;; esac ;;
*'"operation":"vr"'*) echo '{"PREPARED_VR":"1"}' ;;"#;
const VR_WINE: &str = r#"echo "api=$2 vr=$PREPARED_VR config=$XDG_CONFIG_HOME display=${DISPLAY-unset} cwd=$PWD"
if [ "$2" = d3d11 ] || [ -n "$ALL_PASS" ]; then echo "PASS: stereo"; fi"#;

#[test]
fn the_vr_probe_tests_both_renderers_and_needs_the_programs_own_pass() {
    let (_temp, work) = work();
    let site = fixtures::site(&work, &format!("{PREFIX}\n{VR_ARMS}"));
    let runner = fixtures::runner(&work, VR_WINE);
    let (manifest, bundle, headers) = (
        work.join("openxr_monado.json"),
        work.join("graphics"),
        work.join("headers"),
    );
    fs::write(&manifest, "{}").expect("manifest");
    fs::create_dir(&bundle).expect("bundle");
    fs::create_dir_all(headers.join("extra")).expect("headers");
    fs::write(headers.join("openxr.h"), "// header").expect("header");
    fs::write(headers.join("extra/platform.h"), "// platform").expect("header");
    let output = work.join("vr");
    let code = probe(
        &site,
        &[
            &"vr",
            &"--runner",
            &runner,
            &"--runtime-json",
            &manifest,
            &"--bundle",
            &bundle,
            &"--headers",
            &headers,
            &"--simulate",
            &"--output",
            &output,
        ],
    )
    .expect("probe");
    // Direct3D 12 ended cleanly, but without reporting its pass.
    assert_eq!(code, 1);
    let case = |renderer: &str, api: &str| json!({"renderer": renderer, "api": api, "passed": api == "d3d11", "exit_code": 0});
    assert_eq!(
        host::read_json(&output.join("result.json")).expect("result"),
        json!({"scope": "Wine OpenXR stereo swapchains, tracking and frame submission",
            "simulated": true, "physical_headset_validated": false,
            "cases": [case("runner", "d3d11"), case("runner", "d3d12"),
                case("backport", "d3d11"), case("backport", "d3d12")]})
    );
    let runtime = output.join("runtime");
    // Wine receives exactly the prepared environment, not the host's.
    assert_eq!(
        text(&output.join("backport-d3d11.log")),
        format!(
            "api=d3d11 vr=1 config={} display=unset cwd={}\nPASS: stereo\n",
            runtime.join("private/xdg/config").display(),
            output.display()
        )
    );
    assert!(!text(&output.join("runner-d3d12.log")).contains("PASS:"));
    assert_eq!(fixtures::wineserver_calls(&runner), "-k\n-w\n".repeat(4));
    assert_eq!(
        text(&runtime.join("private/vr-settings.json")),
        r#"{"schema":1,"mode":"auto"}"#
    );
    assert_eq!(text(&output.join("include/openxr/openxr.h")), "// header");
    assert_eq!(
        text(&output.join("include/openxr/extra/platform.h")),
        "// platform"
    );
    assert!(text(&work.join("mingw-gcc.calls")).contains(&format!(
        " -Wno-missing-field-initializers -I{} ",
        output.join("include").display()
    )));
    assert_eq!(
        operations(&work),
        ["prefix", "renderer", "vr", "renderer", "vr"]
    );
    let requests = fixtures::requests(&work);
    assert_eq!(requests[1]["bundle"], host::text(&output.join("no-bundle")));
    assert_eq!(requests[3]["bundle"], host::text(&bundle));
    let sent = &requests[2]["environment"];
    assert_eq!(sent["XR_RUNTIME_JSON"], host::text(&manifest));
    assert_eq!(sent["SIMULATED_ENABLE"], "1");
    assert_eq!(sent["XRT_COMPOSITOR_NULL"], "1");
    assert_eq!(
        sent["WINEPREFIX"],
        host::text(&runtime.join("local/msfs-prefix"))
    );
}

#[test]
fn the_vr_probe_passes_without_a_bundle_and_refuses_a_mismatched_renderer() {
    let (_temp, work) = work();
    let arms = r#"*'"operation":"renderer"'*) echo '"runner"' ;;
*'"operation":"vr"'*) echo '{"ALL_PASS":"1"}' ;;"#;
    let site = fixtures::site(&work, &format!("{PREFIX}\n{arms}"));
    let runner = fixtures::runner(&work, VR_WINE);
    let (manifest, headers) = (work.join("runtime.json"), work.join("headers"));
    fs::write(&manifest, "{}").expect("manifest");
    fs::create_dir(&headers).expect("headers");
    let run = |site: &Site, output: &Path, bundle: &[&dyn AsRef<OsStr>]| {
        let mut words: Vec<&dyn AsRef<OsStr>> = vec![
            &"vr",
            &"--runner",
            &runner,
            &"--runtime-json",
            &manifest,
            &"--headers",
            &headers,
        ];
        words.extend(bundle);
        words.push(&"--output");
        words.push(&output);
        probe(site, &words)
    };
    let output = work.join("vr");
    assert_eq!(run(&site, &output, &[]).expect("probe"), 0);
    let result = host::read_json(&output.join("result.json")).expect("result");
    assert_eq!(result["simulated"], false);
    assert_eq!(result["cases"].as_array().expect("cases").len(), 2);
    assert!(
        fixtures::requests(&work)[2]["environment"]
            .get("SIMULATED_ENABLE")
            .is_none()
    );
    // This driver reports the runner's renderer for the bundle as well.
    let mismatched = work.join("mismatched");
    assert_eq!(
        run(&site, &mismatched, &[&"--bundle", &headers])
            .expect_err("mismatch")
            .0,
        "The renderer bundle does not match the runner"
    );
    assert!(run(&site, &output, &[]).is_err());
    fs::remove_file(runner.join("files/bin/wine")).expect("wine");
    let refused = work.join("refused");
    assert_eq!(run(&site, &refused, &[]).expect("refused"), 2);
    assert!(!refused.exists());
}

const LIBRARIES: [&str; 5] = [
    "d3d12.dll",
    "d3d12core.dll",
    "dxgi.dll",
    "d3d11.dll",
    "d3d10core.dll",
];
const LAYOUT_ERROR: &str = "VUID-VkCopyImageInfo2-srcImageLayout-00128";

/// A driver that notes the loaded bundle, and a Wine that shows the layout
/// error for a bundle directory named `released` while maintenance9 is on.
fn volume_site(work: &Path, gpu: &str, renderer: &str) -> (Site<'static>, PathBuf) {
    let arms = format!(
        "{gpu}\n{PREFIX}\n*'\"operation\":\"renderer\"'*) printf '%s\\n' \"$request\" > \"$0.renderer\"; echo '\"{renderer}\"' ;;"
    );
    let site = fixtures::site(work, &arms);
    let wine = format!(
        "IFS= read -r selected < '{}.renderer'\n\
         echo \"layers=$VK_LAYER_PATH uuid=$DXVK_FILTER_DEVICE_UUID disabled=$VKD3D_DISABLE_EXTENSIONS\"\n\
         echo 'info:vkd3d-proton: Validation layers seem to be loaded correctly.'\n\
         case \"$selected\" in *'/released\"'*) if [ -z \"$VKD3D_DISABLE_EXTENSIONS\" ]; then\n\
         echo 'err:vkd3d-proton:vkd3d_debug_messenger_callback: {LAYOUT_ERROR}: layer 2, mip 0'\n\
         fi ;; esac\n\
         echo 'VOLUME_COPY: pixels=2048 mismatches=0'",
        work.join("runtime-lab").display()
    );
    (site, fixtures::runner(work, &wine))
}

fn bundle(path: &Path) -> PathBuf {
    fs::create_dir_all(path).expect("bundle");
    for library in LIBRARIES {
        fs::write(path.join(library), format!("{} {library}", path.display())).expect("library");
    }
    path.to_path_buf()
}

fn volume(
    site: &Site,
    runner: &Path,
    baseline: &Path,
    candidate: &Path,
    output: &Path,
) -> Result<u8> {
    probe(
        site,
        &[
            &"volume-layout",
            &"--runner",
            &runner,
            &"--baseline",
            &baseline,
            &"--bundle",
            &candidate,
            &"--output",
            &output,
        ],
    )
}

#[test]
fn the_volume_probe_passes_when_only_the_released_renderer_shows_the_error() {
    let (_temp, work) = work();
    let (site, runner) = volume_site(&work, AMD, "backport");
    let (released, fixed) = (bundle(&work.join("released")), bundle(&work.join("fixed")));
    let layers = work.join("layers");
    fs::create_dir(&layers).expect("layers");
    let output = work.join("volume");
    let code = probe(
        &site,
        &[
            &"volume-layout",
            &"--runner",
            &runner,
            &"--baseline",
            &released,
            &"--bundle",
            &fixed,
            &"--validation-layer-path",
            &layers,
            &"--output",
            &output,
        ],
    )
    .expect("probe");
    assert_eq!(code, 0);
    let result = host::read_json(&output.join("result.json")).expect("result");
    assert_eq!(result["status"], "PASS");
    assert_eq!(
        result["scope"],
        "3D texture upload/copy/readback with Vulkan layout validation"
    );
    assert_eq!(result["gpu"], "RX");
    assert_eq!(result["nvidia_hardware"], false);
    assert_eq!(
        result["probe_sha256"],
        digest(&fs::read(site.root.join("tests/graphics/volume-copy.c")).expect("source"))
    );
    let cases = result["cases"].as_array().expect("cases");
    assert_eq!(
        cases.iter().map(|row| &row["case"]).collect::<Vec<_>>(),
        [
            "released",
            "fixed",
            "released-without-maintenance9",
            "fixed-without-maintenance9"
        ]
    );
    assert_eq!(cases[0]["validation_errors"], json!({LAYOUT_ERROR: 1}));
    assert_eq!(cases[0]["volume_layout_error"], true);
    for (row, loaded) in cases.iter().zip([&released, &fixed, &released, &fixed]) {
        assert_eq!(row["exit_code"], 0);
        assert_eq!(row["pixels_correct"], true);
        assert_eq!(row["validation_active"], true);
        for library in LIBRARIES {
            assert_eq!(
                row["renderer_sha256"][library],
                digest(format!("{} {library}", loaded.display()).as_bytes())
            );
        }
    }
    for row in &cases[1..] {
        assert_eq!(row["validation_errors"], json!({}));
        assert_eq!(row["volume_layout_error"], false);
    }
    assert!(text(&output.join("released.log")).starts_with(&format!(
        "layers={} uuid=ccdd disabled=\n",
        layers.display()
    )));
    assert!(
        text(&output.join("released-without-maintenance9.log"))
            .contains(" uuid=ccdd disabled=VK_KHR_maintenance9\n")
    );
    assert_eq!(fixtures::wineserver_calls(&runner), "-k\n-w\n".repeat(4));
    assert_eq!(
        operations(&work),
        [
            "probe", "prefix", "renderer", "renderer", "renderer", "renderer"
        ]
    );
    assert_eq!(
        text(&output.join("volume-copy.exe")),
        "built volume-copy.exe"
    );
}

#[test]
fn the_volume_probe_distinguishes_failure_inconclusive_and_refused_runs() {
    let (_temp, work) = work();
    let (site, runner) = volume_site(&work, AMD, "backport");
    let (released, fixed) = (bundle(&work.join("released")), bundle(&work.join("fixed")));
    // Nothing shows the error: maintenance9 was not exercised at all.
    let unexercised = work.join("unexercised");
    assert_eq!(
        volume(&site, &runner, &fixed, &fixed, &unexercised).expect("probe"),
        2
    );
    assert_eq!(
        host::read_json(&unexercised.join("result.json")).expect("result")["status"],
        "INCONCLUSIVE"
    );
    // The candidate still shows the error of the released renderer.
    let unfixed = work.join("unfixed");
    let candidate = bundle(&work.join("candidate/released"));
    assert_eq!(
        volume(&site, &runner, &released, &candidate, &unfixed).expect("probe"),
        1
    );
    assert_eq!(
        host::read_json(&unfixed.join("result.json")).expect("result")["status"],
        "FAIL"
    );
    assert!(
        volume(
            &site,
            &runner,
            &released,
            &work.join("missing"),
            &work.join("x")
        )
        .is_err()
    );

    let (_other, elsewhere) = self::work();
    let (crowded, _) = volume_site(&elsewhere, TWO_GPUS, "backport");
    let refused = work.join("refused");
    assert_eq!(
        volume(&crowded, &runner, &released, &fixed, &refused).expect("refused"),
        2
    );
    // As in the script, the output directory exists before the GPU is known.
    assert!(refused.join("runtime/local").is_dir());
    assert!(!refused.join("result.json").exists());

    let (_third, third) = self::work();
    let (mismatched, _) = volume_site(&third, AMD, "runner");
    assert_eq!(
        volume(
            &mismatched,
            &runner,
            &released,
            &fixed,
            &work.join("mismatched")
        )
        .expect_err("mismatch")
        .0,
        "Bundle does not match the runner"
    );
}

const BRIDGE: [&str; 3] = [
    "xgameruntime.dll",
    "xgameruntime_original.dll",
    "xodus_store_test.dll",
];

/// An installed runtime to copy from, a base runner and a driver that names
/// them. Its prefix holds a link where a Store library belongs.
fn proton_site(work: &Path) -> (Site<'static>, PathBuf, PathBuf) {
    let original = work.join("installed");
    let system = original.join("local/msfs-prefix/drive_c/windows/system32");
    fs::create_dir_all(&system).expect("system");
    for name in BRIDGE {
        fs::write(system.join(name), format!("store {name}")).expect("library");
    }
    let store = original.join("local/store-runtime");
    fs::create_dir_all(store.join("x86_64-windows")).expect("store");
    fs::write(store.join("x86_64-windows/xodus_store_test.dll"), "library").expect("library");
    symlink("x86_64-windows", store.join("current")).expect("link");
    let runner = fixtures::runner(
        work,
        r#"echo "render $1 map=${WINE_DLL_FILE_MAP-unset} loader=$FLIGHTDECK_PROTON_LOADER cwd=$PWD""#,
    );
    let arms = format!(
        r#"*'"operation":"proton-info"'*) echo '{{"selection":null,"base_runner":"{runner}","version":"Flightdeck-Runner-1","wine":"{runner}/files/bin/wine","bridge":["xgameruntime.dll","xgameruntime_original.dll","xodus_store_test.dll"]}}' ;;
*'"operation":"prefix"'*) root=${{request##*'"root":"'}}; root=${{root%%'"'*}}
  mkdir -p "$root/local/msfs-prefix/drive_c/windows/system32"
  ln -s "$0" "$root/local/msfs-prefix/drive_c/windows/system32/xgameruntime.dll"
  echo true ;;
*'"operation":"wine-env"'*) echo '{{"WINEPREFIX":"from-driver"}}' ;;"#,
        runner = runner.display()
    );
    (fixtures::site(work, &arms), original, runner)
}

/// A launcher that serves as the service and as `wine-launch`, where it
/// reads the first mapped image through its inherited descriptor.
fn launcher(path: &Path, status: u8) -> PathBuf {
    script(
        path,
        &format!(
            r#"case "$1" in
wine-launch)
  printf '%s\n' "$@" > "$0.launch"
  descriptor=${{WINE_DLL_FILE_MAP%%:*}}
  IFS= read -r image < "/proc/self/fd/$descriptor"
  printf '%s\n' "$image" "$WINE_DLL_FILE_MAP" "$FLIGHTDECK_PROTON_LOADER" "$XDG_DATA_HOME" "$WINEPREFIX" > "$0.loaded"
  echo loader output
  exit {status} ;;
esac
mkdir -p "$2"
printf '%s' '{{"token":"session"}}' > "$2/desktop-service.json"
echo 'Flightdeck: http://127.0.0.1:1'
exec sleep 60"#
        ),
    );
    path.to_path_buf()
}

#[test]
fn the_proton_probe_hands_memory_images_to_the_default_loader() {
    let (_temp, work) = work();
    let (site, original, runner) = proton_site(&work);
    let launcher = launcher(&work.join("flightdeck-rust"), 0);
    let output = work.join("proton");
    let code = probe(
        &site,
        &[
            &"proton",
            &"--runtime",
            &original,
            &"--default-runner",
            &"--output",
            &output,
            &"--native-launcher",
            &launcher,
        ],
    )
    .expect("probe");
    assert_eq!(code, 0);
    let result = host::read_json(&output.join("result.json")).expect("result");
    assert_eq!(result["implementation"], "rust");
    assert_eq!(result["mode"], "flightdeck");
    assert_eq!(result["version"], "Flightdeck-Runner-1");
    assert_eq!(result["loader"], "native");
    assert_eq!(result["return_to_default_passed"], Value::Null);
    assert_eq!(result["fenix_overlay"], false);
    for passed in [
        "loader_passed",
        "fastlaunch_argument_passed",
        "working_directory_dll_passed",
        "rendering_passed",
        "original_files_unchanged",
        "profile_identity_preserved",
        "addons_retained",
    ] {
        assert_eq!(result[passed], true, "{passed}");
    }
    let runtime = output.join("runtime");
    let prefix = runtime.join("local/msfs-prefix");
    let game = runtime.join("games/MSFS2024");

    // The loader saw the built program behind the first mapped descriptor.
    let loaded = text(&work.join("flightdeck-rust.loaded"));
    let loaded: Vec<&str> = loaded.lines().collect();
    assert_eq!(loaded[0], "built probe.exe");
    let mapped: Vec<(&str, &str)> = loaded[1]
        .split('|')
        .map(|entry| entry.split_once(':').expect("descriptor"))
        .collect();
    let nt = |name: &str| {
        format!(
            "\\??\\Z:{}",
            host::text(&game.join(name)).replace('/', "\\")
        )
    };
    assert_eq!(mapped.len(), 2);
    assert_eq!(mapped[0].1, nt("FlightSimulator2024.exe"));
    assert_eq!(mapped[1].1, nt("nested/probe.dll"));
    assert!(
        mapped
            .iter()
            .all(|(descriptor, _)| descriptor.parse::<u32>().is_ok_and(|d| d > 2))
    );
    assert_eq!(loaded[2], "native");
    assert_eq!(loaded[3], host::text(&runtime.join("private/xdg/data")));
    assert_eq!(loaded[4], "from-driver");
    assert_eq!(
        text(&work.join("flightdeck-rust.launch")),
        format!(
            "wine-launch\n--runtime\n{}\n--\n{}\n",
            runtime.display(),
            game.join("FlightSimulator2024.exe").display()
        )
    );
    assert_eq!(text(&output.join("loader.log")), "loader output\n");
    // Rendering runs without the map and in the output directory.
    assert_eq!(
        text(&output.join("render.log")),
        format!(
            "render {} map=unset loader=native cwd={}\n",
            output.join("multiwindow.exe").display(),
            output.display()
        )
    );
    assert_eq!(fixtures::wineserver_calls(&runner), "-k\n-w\n");

    // The Store libraries replace what the prefix held, never its target.
    let system = prefix.join("drive_c/windows/system32");
    for name in BRIDGE {
        assert!(
            fs::symlink_metadata(system.join(name))
                .expect(name)
                .is_file()
        );
        assert_eq!(text(&system.join(name)), format!("store {name}"));
    }
    assert!(text(&work.join("runtime-lab")).starts_with("#!/bin/sh\n"));
    assert_eq!(
        host::read_json(&runtime.join("private/import-manifest.json")).expect("manifest"),
        json!({"original_runtime_sha256": digest(b"store xgameruntime_original.dll"),
            "artifacts": {"files": {
                "runtime/xgameruntime.dll": digest(b"store xgameruntime.dll"),
                "builtin/x86_64-windows/xodus_store_test.dll": digest(b"store xodus_store_test.dll")}}})
    );
    assert_eq!(
        fs::read_link(runtime.join("local/store-runtime/current")).expect("link"),
        Path::new("x86_64-windows")
    );
    assert_eq!(
        text(&runtime.join("local/store-runtime/x86_64-windows/xodus_store_test.dll")),
        "library"
    );
    assert_eq!(
        text(&runtime.join("private/runtime.json")),
        "{\"game_id\":\"msfs2024\",\"market\":\"AT\"}\n"
    );
    let tools: Vec<_> = fs::read_dir(runtime.join("tools"))
        .expect("tools")
        .map(|tool| tool.expect("tool").path())
        .collect();
    assert!(tools.iter().any(|tool| tool.ends_with("play-msfs.sh")));
    assert!(tools.iter().all(|tool| mode(tool) == 0o700));
    assert_eq!(
        text(&prefix.join("original-marker")),
        "preserve this profile"
    );
    assert_eq!(
        text(&prefix.join("installed-on-proton")),
        "retain add-ons and settings"
    );
    for name in ["FlightSimulator2024.exe", "nested/probe.dll"] {
        assert_eq!(
            text(&game.join(name)),
            "Synthetic encrypted placeholder; must remain unchanged.\n"
        );
    }
    assert_eq!(text(&output.join("probe.dll")), "built probe.dll");
    assert_eq!(
        operations(&work),
        [
            "proton-info",
            "prefix",
            "proton-info",
            "proton-info",
            "wine-env",
            "proton-info"
        ]
    );
    assert_eq!(fixtures::requests(&work)[0]["root"], host::text(&original));
    // Nothing was written to the runtime the libraries were copied from.
    assert_eq!(fs::read_dir(&original).expect("original").count(), 1);
    assert!(output.join("native-service.log").is_file());
}

#[test]
fn the_proton_probe_fails_with_the_loaders_log_and_still_stops_wine() {
    let (_temp, work) = work();
    let (site, original, runner) = proton_site(&work);
    let launcher = launcher(&work.join("flightdeck-rust"), 5);
    let output = work.join("proton");
    let run = |words: &[&dyn AsRef<OsStr>]| {
        let mut line: Vec<&dyn AsRef<OsStr>> = vec![
            &"proton",
            &"--runtime",
            &original,
            &"--output",
            &output,
            &"--native-launcher",
            &launcher,
        ];
        line.extend(words);
        probe(&site, &line)
    };
    assert_eq!(
        run(&[&"--default-runner"]).expect_err("loader").0,
        "loader.log ended with exit status 5:\nloader output\n"
    );
    assert_eq!(fixtures::wineserver_calls(&runner), "-k\n-w\n");
    assert!(!output.join("result.json").exists());
    // The output directory of the failed run is not reused.
    assert!(run(&[&"--default-runner"]).is_err());
}

/// A launcher or interpreter that announces the fixture service and waits.
const SERVE: &str = "echo \"Flightdeck: http://127.0.0.1:$PORT\"\nexec sleep 60";

#[test]
fn the_benchmark_measures_both_launchers_and_refuses_an_existing_report() {
    let service = Service::start(|_, path, _, _| {
        let known = path.starts_with("/api/");
        (
            if known { 200 } else { 404 },
            json!({"runtime": {"configured": false}, "game": {"can_start": false}}),
        )
    });
    let (_temp, work) = work();
    let serve = SERVE.replace("$PORT", &service.port.to_string());
    let rust = work.join("flightdeck-rust");
    script(
        &rust,
        &format!(
            "if [ \"$1\" = save-check ]; then echo '{{\"valid\":true,\"blobs\":1,\"bytes\":9}}'; exit 0; fi\n\
             mkdir -p \"$2\"\nprintf '%s' '{{\"token\":\"session\"}}' > \"$2/desktop-service.json\"\n{serve}"
        ),
    );
    let python = work.join("python3");
    script(
        &python,
        &format!(
            "if [ \"$1\" = -c ]; then\n  case \"$2\" in\n  \
             *'print(sys.version'*) echo '3.99.1' ;;\n  \
             *'rng='*) file=$4; shift 4; printf '%s ' \"$@\" > \"$file\" ;;\n  \
             *'save_state.decode'*) echo '{{\"blobs\":1,\"bytes\":9}}' ;;\n  esac\n  exit 0\nfi\n\
             printf '%s\\n' \"$*\" >> \"$0.services\"\n{serve}"
        ),
    );
    let source = work.join("python-source");
    fs::create_dir(&source).expect("source");
    let installed = work.join("installed-python-launcher");
    fs::write(&installed, "").expect("launcher");
    let report = work.join("results/performance.json");
    let words: [&dyn AsRef<OsStr>; 19] = [
        &"benchmark-launchers",
        &"--binary",
        &rust,
        &"--python-source",
        &source,
        &"--python-commit",
        &"005194e",
        &"--rust-launcher",
        &rust,
        &"--python-launcher",
        &installed,
        &"--repeats",
        &"1",
        &"--requests",
        &"2",
        &"--python",
        &python,
        &"--output",
        &report,
    ];
    let site = fixtures::site(&work, "");
    assert_eq!(probe(&site, &words).expect("benchmark"), 0);
    let result = host::read_json(&report).expect("report");
    assert_eq!(result["schema"], 1);
    assert_eq!(result["python"], "3.99.1");
    assert_eq!(result["python_source_commit"], "005194e");
    assert_eq!(
        result["rust_binary_sha256"],
        digest(&fs::read(&rust).expect("binary"))
    );
    assert!(result["machine"]["logical_cpus"].as_u64().expect("cpus") >= 1);
    assert!(
        !result["machine"]["kernel"]
            .as_str()
            .expect("kernel")
            .is_empty()
    );
    for mode in ["direct", "installed"] {
        for name in ["rust", "python"] {
            let measured = &result["services"][mode][name];
            assert_eq!(measured["startup_ms"]["samples"], 1, "{mode} {name}");
            assert!(measured["rss_kib"]["median"].as_u64().expect("rss") > 0);
            assert!(measured["peak_rss_kib"]["max"].as_u64().expect("peak") > 0);
            for path in [
                "/api/status",
                "/api/setup",
                "/api/cloud-saves",
                "/api/launcher-update",
            ] {
                assert_eq!(measured["api_ms"][path]["samples"], 2, "{path}");
            }
        }
    }
    // Each workload was generated after the ones before it, then measured.
    for (label, specifications) in [
        ("small_128_kib", "32:4096 "),
        ("many_4096_blobs", "32:4096 4096:256 "),
        ("large_32_mib", "32:4096 4096:256 32:1048576 "),
    ] {
        let measured = &result["saves"][label];
        assert_eq!(measured["bytes"], specifications.len(), "{label}");
        assert_eq!(measured["identical_results"], true);
        for name in ["rust", "python"] {
            assert_eq!(measured["process_wall_ms"][name]["samples"], 1);
        }
    }
    // A warm-up and one measured start, as a module and as the installed file.
    let module = "-m flightdeck --state-dir ";
    let services = text(&work.join("python3.services"));
    let services: Vec<&str> = services.lines().collect();
    assert_eq!(services.len(), 4);
    assert!(services[..2].iter().all(|line| line.starts_with(module)));
    assert!(
        services[2..]
            .iter()
            .all(|line| line.starts_with(&format!("{} --state-dir ", installed.display())))
    );
    assert!(services.iter().all(|line| line.ends_with(" --no-browser")));
    let error = probe(&site, &words).expect_err("existing report");
    assert!(error.0.starts_with(&host::text(&report)), "{}", error.0);
    assert_eq!(host::read_json(&report).expect("report"), result);
}

#[test]
fn the_benchmark_needs_a_measured_run_and_the_python_checkout() {
    let (_temp, work) = work();
    let site = fixtures::site(&work, "");
    let binary = work.join("flightdeck-rust");
    fs::write(&binary, "").expect("binary");
    let python = work.join("python3");
    script(&python, "echo 'No module named flightdeck' >&2\nexit 1");
    let run = |repeats: &str| {
        probe(
            &site,
            &[
                &"benchmark-launchers",
                &"--binary",
                &binary,
                &"--python-source",
                &work,
                &"--python-commit",
                &"c",
                &"--output",
                &work.join("report.json"),
                &"--python",
                &python,
                &"--repeats",
                &repeats,
            ],
        )
    };
    assert_eq!(
        run("0").expect_err("repeats").0,
        "--repeats and --requests must be at least 1"
    );
    let error = run("1").expect_err("checkout");
    assert!(
        error.0.ends_with("failed: No module named flightdeck"),
        "{}",
        error.0
    );
    assert!(!work.join("report.json").exists());
}

#[test]
fn the_fenix_import_replaces_the_manifests_of_the_checkout() {
    let (_temp, work) = work();
    let (checkout, project) = (work.join("checkout"), work.join("patch"));
    fs::create_dir_all(checkout.join("compat/fenix")).expect("checkout");
    fs::create_dir_all(project.join("dist")).expect("project");
    fs::write(project.join("bundle.json"), r#"{"version":"1.4.0"}"#).expect("bundle");
    fs::write(project.join("LICENSE"), "MIT License\n").expect("license");
    let release = project.join("dist/flightdeck-release.json");
    fs::write(&release, r#"{"version":"1.4.1"}"#).expect("release");
    let site = Site {
        root: &checkout,
        lab: Lab::at(work.join("not-needed")),
        compiler: "false".into(),
    };
    let words: [&dyn AsRef<OsStr>; 2] = [&"sync-fenix", &project];
    assert_eq!(
        probe(&site, &words).expect_err("versions").0,
        "Release and installer versions differ"
    );
    assert_eq!(
        fs::read_dir(checkout.join("compat/fenix"))
            .expect("list")
            .count(),
        0
    );
    fs::write(
        &release,
        r#"{"version":"1.4.0","url":"https://example.invalid"}"#,
    )
    .expect("release");
    assert_eq!(probe(&site, &words).expect("import"), 0);
    assert_eq!(
        text(&checkout.join("compat/fenix/release.json")),
        r#"{"version":"1.4.0","url":"https://example.invalid"}"#
    );
    assert_eq!(
        text(&checkout.join("compat/fenix/LICENSE")),
        "MIT License\n"
    );
    assert_eq!(
        text(&checkout.join("compat/fenix/bundle.json")),
        r#"{"version":"1.4.0"}"#
    );
}
