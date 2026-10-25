// SPDX-License-Identifier: MIT
//! The account-free GameSave, Store, catalog and user-lookup tests. Each is
//! compiled with MinGW from the staged runtime sources and run under the
//! supplied Wine in a new prefix of its own. This establishes how those
//! sources behave against synthetic providers and fixtures, not against an
//! account, a license or the Store. It needs a staged tree, the MinGW
//! compilers, a Wine runner and, for the save interchange, the built
//! `runtime-lab` example.
use super::common::{self, Environment, Prefix, checked, exit_code, split_lines, truthy};
use crate::probes::lab::Lab;
use crate::util::{Failure, Result, digest, pretty};
use serde_json::{Map, Value, json};
use std::{
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub const SUITES: [&str; 6] = ["user", "gamesave", "store", "catalog", "queue", "all"];
const USER: &str = "user-lookup";
const GAMESAVE: [&str; 4] = ["core", "bridge", "async", "save-interchange"];
const STORE: [&str; 5] = [
    "explicit-products",
    "durable-license",
    "package-updates",
    "purchase",
    "store-preview",
];
const CATALOG: [&str; 3] = ["catalog", "catalog-batch", "catalog-coins"];
const QUEUE: [&str; 4] = [
    "XAsync.cpp",
    "XTaskQueue.cpp",
    "ThreadPool.cpp",
    "WaitTimer.cpp",
];
const REMOVED: [&str; 10] = [
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "WINE_DLL_FILE_MAP",
    "WINEDLLPATH",
    "XODUS_USER_RUNTIME",
    "XODUS_USER_SOCKET_SUFFIX",
    "XODUS_LOCAL_GAMESAVE",
    "XODUS_LOCAL_GAMESAVE_ROOT",
    "XODUS_STORE_MARKET",
    "XODUS_STORE_LANGUAGE",
];
fn names(suite: &str) -> Vec<&'static str> {
    let mut names = Vec::new();
    if matches!(suite, "queue" | "all") {
        names.push("taskqueue");
    }
    if matches!(suite, "user" | "all") {
        names.push(USER);
    }
    if matches!(suite, "gamesave" | "all") {
        names.extend(GAMESAVE);
    }
    if matches!(suite, "store" | "all") {
        names.extend(STORE);
    }
    if matches!(suite, "store" | "catalog" | "all") {
        names.extend(CATALOG);
    }
    names
}

struct Plan {
    compile: Vec<OsString>,
    /// Everything the test binary was made from, for the provenance record.
    hashed: Vec<PathBuf>,
}

fn plan(name: &str, root: &Path, stage: &Path, binary: &Path) -> Plan {
    let runtime = stage.join("runtime");
    let sources = runtime.join("src");
    let user = stage.join("wine-src/dlls/xgameruntime/GDKComponent/System");
    let tests = root.join("tests/compat");
    let mut parts = Vec::new();
    if name == "taskqueue" {
        parts.extend(QUEUE);
    } else if STORE.contains(&name) {
        parts.extend([
            "StoreQueries.cpp",
            "StoreContext.cpp",
            "StoreDurableLicense.cpp",
        ]);
        parts.extend(QUEUE);
    } else if name.starts_with("catalog") {
        parts.push("StoreCatalog.cpp");
        if name != "catalog" {
            parts.push("StoreCatalogBatch.cpp");
        }
        if name == "catalog-coins" {
            parts.extend(["StoreCatalogCoinMapper.cpp", "StoreCatalogProvider.cpp"]);
        }
    } else if name != USER {
        parts.push("GameSaveLocalCore.cpp");
        if !matches!(name, "core" | "save-interchange") {
            parts.push("GameSaveBridge.cpp");
        }
        if name == "async" {
            parts.push("GameSaveAsync.cpp");
            parts.extend(QUEUE);
        }
    }
    let mut selected: Vec<PathBuf> = parts.iter().map(|part| sources.join(part)).collect();
    let (compiler, standard, included, suffix) = if name == USER {
        selected.push(user.join("UserSessionCache.c"));
        ("x86_64-w64-mingw32-gcc", "-std=c11", &user, "-test.c")
    } else {
        (
            "x86_64-w64-mingw32-g++",
            "-std=c++17",
            &sources,
            "-test.cpp",
        )
    };
    selected.push(tests.join(format!("{name}{suffix}")));
    let mut compile: Vec<OsString> = vec![
        compiler.into(),
        standard.into(),
        "-O2".into(),
        "-static".into(),
        "-I".into(),
        runtime.join("include").into(),
        "-I".into(),
        included.into(),
    ];
    if matches!(name, "core" | "bridge" | "async") {
        compile.push("-DXODUS_GAMESAVE_TESTING".into());
    }
    if name == "durable-license" {
        compile.push("-DSTORE_DURABLES_TESTING".into());
    }
    if matches!(name, "core" | "async" | "save-interchange") {
        compile.push("-municode".into());
    }
    compile.extend(selected.iter().map(OsString::from));
    compile.extend(["-o".into(), binary.into()]);
    if name.starts_with("catalog") {
        compile.push("-lwinhttp".into());
    } else {
        compile.extend(["-lbcrypt", "-lole32", "-luuid"].map(OsString::from));
    }
    let mut hashed = selected;
    if name == "taskqueue" {
        hashed.extend(
            [
                "XTaskQueue.h",
                "ThreadPool.h",
                "WaitTimer.h",
                "AtomicVector.h",
                "LocklessQueue.h",
                "referenced_ptr.h",
                "SpinLock.h",
                "StaticArray.h",
            ]
            .map(|n| sources.join(n)),
        );
        hashed.push(runtime.join("include/compat.h"));
    }
    if name == USER {
        hashed.extend(
            [
                "UserSessionCache.h",
                "UserLookup.inc",
                "UserStoreAccount.inc",
            ]
            .map(|part| user.join(part)),
        );
    }
    // The batch/provider tests reuse the embedded synthetic catalog fixture.
    // Include it in provenance without compiling a second entry point.
    if matches!(name, "catalog-batch" | "catalog-coins") {
        hashed.push(tests.join("catalog-test.cpp"));
    }
    if name == "save-interchange" {
        hashed.extend([
            root.join("native/save_state.rs"),
            root.join("examples/runtime-lab.rs"),
        ]);
    }
    Plan { compile, hashed }
}

/// The name of a hashed source in the report: relative to the staged runtime,
/// else to the stage, else to the checkout.
fn key(path: &Path, root: &Path, stage: &Path) -> Result<String> {
    path.strip_prefix(stage.join("runtime"))
        .or_else(|_| path.strip_prefix(stage))
        .or_else(|_| path.strip_prefix(root))
        .map(|relative| relative.display().to_string())
        .map_err(|_| Failure(format!("{} is outside stage and checkout", path.display())))
}

fn environment(case: &Path) -> Environment {
    Environment {
        removed: &REMOVED,
        set: vec![
            ("WINEPREFIX", case.join("prefix").into()),
            ("WINEARCH", "win64".into()),
            ("WINEDEBUG", "-all".into()),
            ("WINEESYNC", "0".into()),
            ("WINEFSYNC", "0".into()),
            (
                "WINEDLLOVERRIDES",
                "winemenubuilder.exe,mscoree,mshtml=d".into(),
            ),
        ],
    }
}

fn text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| Failure(format!("{} is not UTF-8", path.display())))
}

/// A host directory as the test binary names it through Wine's Z: drive.
fn drive(directory: &Path) -> Result<String> {
    Ok(format!("Z:{}", text(directory)?.replace('/', "\\")))
}

/// A test passed if it exited cleanly and printed its summary.
fn passed(code: i64, lines: &[String]) -> bool {
    code == 0
        && lines
            .iter()
            .any(|line| line.contains("SUMMARY") || line.contains("failures=0"))
}

/// One operation of the development probe executable, which holds the
/// launcher's own save format.
fn probe(lab: &Lab, operation: &str, file: &Path) -> Result<Value> {
    lab.call(operation, json!({"file": text(file)?}))
}

pub fn run(root: &Path, stage: &Path, wine: &Path, suite: &str) -> Result<u8> {
    // Prefixes, saves and results are private to the invoking user.
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
    let stage = common::stage(stage)?;
    let work = tempfile::Builder::new()
        .prefix("compat-tests-")
        .tempdir_in(&stage)?
        .keep();
    let wine =
        fs::canonicalize(wine).map_err(|error| Failure(format!("{}: {error}", wine.display())))?;
    let mut cases = Map::new();
    for name in names(suite) {
        let case = work.join(name);
        let data = case.join("synthetic-saves");
        fs::create_dir(&case)?;
        fs::create_dir(&data)?;
        let binary = case.join(format!("{name}-test.exe"));
        let plan = plan(name, root, &stage, &binary);
        checked(Command::new(&plan.compile[0]).args(&plan.compile[1..]))?;
        let namespace = data.join("a".repeat(64));
        let state = namespace.join("state.bin");
        if name == "save-interchange" {
            fs::create_dir(&namespace)?;
            probe(&Lab::new(root), "save-write", &state)?;
        }
        let environment = environment(&case);
        let mut prefix = Prefix::new(&wine, &environment);
        let result = (|| -> Result<Value> {
            prefix.boot()?;
            let directory = GAMESAVE.contains(&name).then(|| drive(&data)).transpose()?;
            let mut invocation = vec![binary.as_os_str()];
            invocation.extend(directory.as_deref().map(OsStr::new));
            let test = prefix.run(&invocation, None, 60)?;
            let code = exit_code(test.status);
            let mut lines = split_lines(&String::from_utf8_lossy(&test.stdout));
            let mut passed = passed(code, &lines);
            if name == "save-interchange" && passed {
                passed = truthy(&probe(&Lab::new(root), "save-reply", &state)?);
                lines.push(
                    if passed {
                        "PASS C++ save read back by Rust"
                    } else {
                        "FAIL native save interchange"
                    }
                    .into(),
                );
            }
            let mut sources = Map::new();
            for path in &plan.hashed {
                sources.insert(key(path, root, &stage)?, digest(&fs::read(path)?).into());
            }
            Ok(json!({"exit_code": code, "passed": passed, "checks": lines,
                "source_sha256": sources, "binary_sha256": digest(&fs::read(&binary)?)}))
        })();
        // Stops only the wineserver of this test's prefix, also after a failure.
        let stopped = prefix.stop();
        cases.insert(name.into(), result?);
        stopped?;
    }
    let passed = cases.values().all(|case| case["passed"] == true);
    let report = json!({"format": 1, "suite": suite, "synthetic_only": true,
        "account_calls": false, "cases": cases, "passed": passed});
    let path = work.join("results.json");
    fs::write(&path, pretty(&report))?;
    println!(
        "{}",
        json!({"passed": passed, "report": path.display().to_string()})
    );
    Ok(u8::from(!passed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(plan: &Plan) -> Vec<String> {
        plan.compile
            .iter()
            .map(|word| word.to_string_lossy().into_owned())
            .collect()
    }
    fn planned(name: &str) -> Plan {
        plan(
            name,
            Path::new("/synthetic/checkout"),
            Path::new("/synthetic/stage"),
            Path::new("/synthetic/work/test.exe"),
        )
    }

    #[test]
    fn suites_select_their_families_in_a_fixed_order() {
        assert_eq!(names("user"), ["user-lookup"]);
        assert_eq!(
            names("gamesave"),
            ["core", "bridge", "async", "save-interchange"]
        );
        assert_eq!(
            names("catalog"),
            ["catalog", "catalog-batch", "catalog-coins"]
        );
        assert_eq!(names("store").len(), 8);
        assert_eq!(names("store")[5..], names("catalog"));
        let all = names("all");
        assert_eq!(all.len(), 14);
        assert_eq!(
            (all[0], all[1], all[2], all[6], all[13]),
            (
                "taskqueue",
                "user-lookup",
                "core",
                "explicit-products",
                "catalog-coins"
            )
        );
    }

    #[test]
    fn every_test_is_compiled_from_its_own_sources_with_its_own_flags() {
        let source = |name: &str| format!("/synthetic/stage/runtime/src/{name}");
        let head = |extra: &[&str]| {
            let mut head: Vec<String> = [
                "x86_64-w64-mingw32-g++",
                "-std=c++17",
                "-O2",
                "-static",
                "-I",
                "/synthetic/stage/runtime/include",
                "-I",
                "/synthetic/stage/runtime/src",
            ]
            .map(String::from)
            .to_vec();
            head.extend(extra.iter().map(|word| word.to_string()));
            head
        };
        let tail = |libraries: &[&str]| {
            let mut tail = vec!["-o".to_string(), "/synthetic/work/test.exe".to_string()];
            tail.extend(libraries.iter().map(|word| word.to_string()));
            tail
        };
        let system = ["-lbcrypt", "-lole32", "-luuid"];
        let queue = QUEUE.map(source);
        let test = |name: &str| format!("/synthetic/checkout/tests/compat/{name}");

        let mut expected = head(&["-DXODUS_GAMESAVE_TESTING", "-municode"]);
        expected.extend([source("GameSaveLocalCore.cpp"), test("core-test.cpp")]);
        expected.extend(tail(&system));
        assert_eq!(words(&planned("core")), expected);

        let mut expected = head(&["-DXODUS_GAMESAVE_TESTING"]);
        expected.extend([
            source("GameSaveLocalCore.cpp"),
            source("GameSaveBridge.cpp"),
            test("bridge-test.cpp"),
        ]);
        expected.extend(tail(&system));
        assert_eq!(words(&planned("bridge")), expected);

        let mut expected = head(&["-DXODUS_GAMESAVE_TESTING", "-municode"]);
        expected.extend([
            source("GameSaveLocalCore.cpp"),
            source("GameSaveBridge.cpp"),
            source("GameSaveAsync.cpp"),
        ]);
        expected.extend(queue.clone());
        expected.push(test("async-test.cpp"));
        expected.extend(tail(&system));
        assert_eq!(words(&planned("async")), expected);

        let mut expected = head(&["-municode"]);
        expected.extend([
            source("GameSaveLocalCore.cpp"),
            test("save-interchange-test.cpp"),
        ]);
        expected.extend(tail(&system));
        let interchange = planned("save-interchange");
        assert_eq!(words(&interchange), expected);
        assert_eq!(
            interchange.hashed[2..],
            [
                Path::new("/synthetic/checkout/native/save_state.rs"),
                Path::new("/synthetic/checkout/examples/runtime-lab.rs")
            ]
        );

        for name in STORE {
            let flags: &[&str] = if name == "durable-license" {
                &["-DSTORE_DURABLES_TESTING"]
            } else {
                &[]
            };
            let mut expected = head(flags);
            expected.extend(
                [
                    "StoreQueries.cpp",
                    "StoreContext.cpp",
                    "StoreDurableLicense.cpp",
                ]
                .map(source),
            );
            expected.extend(queue.clone());
            expected.push(test(&format!("{name}-test.cpp")));
            expected.extend(tail(&system));
            let plan = planned(name);
            assert_eq!(words(&plan), expected, "{name}");
            assert_eq!(plan.hashed.len(), 8, "{name}");
        }

        let mut expected = head(&[]);
        expected.extend([source("StoreCatalog.cpp"), test("catalog-test.cpp")]);
        expected.extend(tail(&["-lwinhttp"]));
        assert_eq!(words(&planned("catalog")), expected);
        let mut expected = head(&[]);
        expected.extend([
            source("StoreCatalog.cpp"),
            source("StoreCatalogBatch.cpp"),
            source("StoreCatalogCoinMapper.cpp"),
            source("StoreCatalogProvider.cpp"),
            test("catalog-coins-test.cpp"),
        ]);
        expected.extend(tail(&["-lwinhttp"]));
        let coins = planned("catalog-coins");
        assert_eq!(words(&coins), expected);
        // The fixture the batch tests embed is recorded, not compiled twice.
        assert_eq!(
            coins.hashed.last().expect("fixture"),
            Path::new("/synthetic/checkout/tests/compat/catalog-test.cpp")
        );
        assert_eq!(planned("catalog-batch").hashed.len(), 4);

        let system_sources = "/synthetic/stage/wine-src/dlls/xgameruntime/GDKComponent/System";
        let user = planned("user-lookup");
        let mut expected: Vec<String> = [
            "x86_64-w64-mingw32-gcc",
            "-std=c11",
            "-O2",
            "-static",
            "-I",
            "/synthetic/stage/runtime/include",
            "-I",
            system_sources,
        ]
        .map(String::from)
        .to_vec();
        expected.extend([
            format!("{system_sources}/UserSessionCache.c"),
            test("user-lookup-test.c"),
        ]);
        expected.extend(tail(&system));
        assert_eq!(words(&user), expected);
        assert_eq!(
            user.hashed[2..]
                .iter()
                .map(|path| path
                    .file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned())
                .collect::<Vec<_>>(),
            [
                "UserSessionCache.h",
                "UserLookup.inc",
                "UserStoreAccount.inc"
            ]
        );
    }

    #[test]
    fn test_sources_exist_for_every_planned_case() {
        let root = crate::util::root();
        for name in names("all") {
            let plan = plan(name, &root, Path::new("/synthetic/stage"), Path::new("x"));
            let local: Vec<_> = plan
                .hashed
                .iter()
                .filter(|path| path.starts_with(&root))
                .collect();
            assert!(!local.is_empty(), "{name}");
            for path in local {
                assert!(path.is_file(), "{}", path.display());
            }
        }
    }

    #[test]
    fn hashed_sources_are_named_relative_to_runtime_stage_or_checkout() {
        let (root, stage) = (
            Path::new("/synthetic/checkout"),
            Path::new("/synthetic/checkout/build/compat"),
        );
        let name = |path: &str| key(Path::new(path), root, stage);
        assert_eq!(
            name("/synthetic/checkout/build/compat/runtime/src/StoreCatalog.cpp").expect("runtime"),
            "src/StoreCatalog.cpp"
        );
        assert_eq!(
            name("/synthetic/checkout/build/compat/wine-src/dlls/x/UserLookup.inc").expect("stage"),
            "wine-src/dlls/x/UserLookup.inc"
        );
        assert_eq!(
            name("/synthetic/checkout/tests/compat/core-test.cpp").expect("checkout"),
            "tests/compat/core-test.cpp"
        );
        assert!(name("/synthetic/elsewhere/file.cpp").is_err());
    }

    #[test]
    fn prefix_environment_is_isolated_and_save_directories_use_the_wine_drive() {
        let environment = environment(Path::new("/synthetic/work/core"));
        let mut command = Command::new("wine");
        environment.apply(&mut command);
        let variables: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for name in REMOVED {
            assert!(variables.contains(&(name.to_string(), None)), "{name}");
        }
        let set: Vec<_> = variables
            .iter()
            .filter_map(|(name, value)| Some((name.as_str(), value.as_deref()?)))
            .collect();
        assert_eq!(
            set,
            [
                ("WINEARCH", "win64"),
                ("WINEDEBUG", "-all"),
                ("WINEDLLOVERRIDES", "winemenubuilder.exe,mscoree,mshtml=d"),
                ("WINEESYNC", "0"),
                ("WINEFSYNC", "0"),
                ("WINEPREFIX", "/synthetic/work/core/prefix"),
            ]
        );
        assert_eq!(
            drive(Path::new("/synthetic/work/core/synthetic-saves")).expect("drive"),
            "Z:\\synthetic\\work\\core\\synthetic-saves"
        );
    }

    #[test]
    fn a_test_passes_with_a_clean_exit_and_a_summary() {
        let lines = |text: &str| split_lines(text);
        assert!(passed(0, &lines("PASS a\r\nSUMMARY failures=0\r\n")));
        assert!(passed(0, &lines("checks=4 failures=0")));
        assert!(!passed(1, &lines("SUMMARY failures=1")));
        assert!(!passed(-11, &lines("SUMMARY failures=0")));
        assert!(!passed(0, &lines("PASS a\nPASS b\n")));
        assert!(!passed(0, &[]));
    }
}
