// SPDX-License-Identifier: MIT
//! The ConnectedStorage helper's pure protocol validators, compiled natively.
//! No Wine, account or network is involved.
use std::process::Command;

fn compiled(source: &str) -> Option<String> {
    let root = crate::util::root();
    let compiler = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|folder| folder.join("g++").is_file());
    if !compiler {
        eprintln!("skipped: no C++ compiler");
        return None;
    }
    let temporary = tempfile::tempdir().expect("temp");
    let output = temporary.path().join("protocol-test");
    let build = Command::new("g++")
        .args(["-std=c++17", "-O1", "-I"])
        .arg(root.join("compat/runtime/include"))
        .arg("-I")
        .arg(root.join("compat/runtime/tools"))
        .arg(root.join("tests/compat/cloud_storage").join(source))
        .arg("-o")
        .arg(&output)
        .output()
        .expect("g++");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let result = Command::new(&output).output().expect("protocol test");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    Some(String::from_utf8(result.stdout).expect("output"))
}

#[test]
fn fixed_read_requests_and_unique_title_binding() {
    if let Some(output) = compiled("protocol-test.cpp") {
        assert_eq!(output, "33 protocol checks PASS\n");
    }
}

#[test]
fn write_lease_fencing_and_private_azure_targets() {
    if let Some(output) = compiled("write-protocol-test.cpp") {
        let count = output
            .strip_suffix(" write protocol checks PASS\n")
            .expect("summary");
        assert!(
            !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()),
            "{output}"
        );
    }
}
