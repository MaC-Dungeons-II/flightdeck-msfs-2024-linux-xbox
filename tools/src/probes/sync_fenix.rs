// SPDX-License-Identifier: MIT
//! The explicit maintainer import from a locally built Fenix patch project:
//! its installer manifest, release manifest and license replace the copies in
//! `compat/fenix/` when both manifests name the same version. Files are only
//! copied. Whether the release they describe is published, downloadable and
//! matches its hash is not checked here, and neither is the Rust integration.
use super::host;
use crate::util::{Failure, Result, require};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(clap::Args)]
pub struct Arguments {
    project: PathBuf,
}

/// Source in the patch project and name under `compat/fenix/`.
const FILES: [(&str, &str); 3] = [
    ("bundle.json", "bundle.json"),
    ("dist/flightdeck-release.json", "release.json"),
    ("LICENSE", "LICENSE"),
];

fn version(project: &Path, name: &str) -> Result<Value> {
    match host::read_json(&project.join(name))?.get("version") {
        Some(version) => Ok(version.clone()),
        None => Err(Failure(format!("{name} has no version"))),
    }
}

/// Copies the three files and returns the imported version.
fn import(root: &Path, project: &Path) -> Result<String> {
    let project = host::canonical(project)?;
    let bundle = version(&project, "bundle.json")?;
    require(
        bundle == version(&project, "dist/flightdeck-release.json")?,
        "Release and installer versions differ",
    )?;
    for (source, name) in FILES {
        host::copy2(&project.join(source), &root.join("compat/fenix").join(name))?;
    }
    Ok(match bundle {
        Value::String(version) => version,
        other => other.to_string(),
    })
}

pub fn run(root: &Path, arguments: &Arguments) -> Result<u8> {
    let version = import(root, &arguments.project)?;
    println!(
        "Imported Fenix component manifests {version} — review manifests and run Fenix tests before release."
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{fs, os::unix::fs::PermissionsExt, time::Duration};

    /// A checkout with earlier imports and a patch project next to it.
    fn fixture(bundle: Value, release: Value) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().expect("temp");
        let (root, project) = (temp.path().join("checkout"), temp.path().join("patch"));
        fs::create_dir_all(root.join("compat/fenix")).expect("checkout");
        for name in ["bundle.json", "release.json", "LICENSE"] {
            fs::write(root.join("compat/fenix").join(name), "earlier import").expect("earlier");
        }
        fs::create_dir_all(project.join("dist")).expect("project");
        fs::write(project.join("bundle.json"), bundle.to_string()).expect("bundle");
        fs::write(
            project.join("dist/flightdeck-release.json"),
            release.to_string(),
        )
        .expect("release");
        fs::write(project.join("LICENSE"), "MIT License\n").expect("license");
        (temp, root, project)
    }

    fn imported(root: &Path) -> [String; 3] {
        ["bundle.json", "release.json", "LICENSE"]
            .map(|name| fs::read_to_string(root.join("compat/fenix").join(name)).expect(name))
    }

    #[test]
    fn matching_manifests_and_the_license_replace_the_earlier_import() {
        let bundle = json!({"version": "1.4.0", "files": {"a.dll": "00"}});
        let release = json!({"version": "1.4.0", "url": "https://example.invalid/patch.zip"});
        let (_temp, root, project) = fixture(bundle.clone(), release.clone());
        let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
        fs::File::open(project.join("LICENSE"))
            .expect("license")
            .set_times(fs::FileTimes::new().set_modified(old))
            .expect("times");
        fs::set_permissions(project.join("LICENSE"), fs::Permissions::from_mode(0o640))
            .expect("mode");
        assert_eq!(import(&root, &project).expect("import"), "1.4.0");
        assert_eq!(
            imported(&root),
            [
                bundle.to_string(),
                release.to_string(),
                "MIT License\n".to_string()
            ]
        );
        // Like the copy it replaces, the import keeps mode and time.
        let license = fs::metadata(root.join("compat/fenix/LICENSE")).expect("license");
        assert_eq!(license.modified().expect("time"), old);
        assert_eq!(license.permissions().mode() & 0o777, 0o640);
        // Nothing else is created in the checkout or changed in the project.
        assert_eq!(
            fs::read_dir(root.join("compat/fenix"))
                .expect("list")
                .count(),
            3
        );
        assert_eq!(
            fs::read_to_string(project.join("bundle.json")).expect("bundle"),
            bundle.to_string()
        );
        // The project may be named through a link or a relative detour.
        let link = root.join("patch-link");
        std::os::unix::fs::symlink(&project, &link).expect("link");
        assert_eq!(
            import(&root, &link.join("dist/..")).expect("import"),
            "1.4.0"
        );
        assert!(run(&root, &Arguments { project }).is_ok());
    }

    #[test]
    fn differing_versions_import_nothing() {
        let (_temp, root, project) =
            fixture(json!({"version": "1.4.0"}), json!({"version": "1.4.1"}));
        assert_eq!(
            import(&root, &project).expect_err("differ").0,
            "Release and installer versions differ"
        );
        assert_eq!(imported(&root), ["earlier import"; 3].map(String::from));
        // The same number as text and as a number is not the same version.
        let (_temp, root, project) = fixture(json!({"version": "2"}), json!({"version": 2}));
        assert!(import(&root, &project).is_err());
        assert_eq!(imported(&root), ["earlier import"; 3].map(String::from));
    }

    #[test]
    fn incomplete_projects_are_refused_before_anything_is_copied() {
        let (_temp, root, project) = fixture(json!({"version": "1.4.0"}), json!({"name": "patch"}));
        assert_eq!(
            import(&root, &project).expect_err("no version").0,
            "dist/flightdeck-release.json has no version"
        );
        let (_temp2, root2, project2) = fixture(json!({}), json!({}));
        assert_eq!(
            import(&root2, &project2).expect_err("no version").0,
            "bundle.json has no version"
        );
        fs::write(project.join("bundle.json"), "{not json").expect("bundle");
        assert!(import(&root, &project).is_err());
        fs::remove_file(project.join("dist/flightdeck-release.json")).expect("release");
        fs::write(project.join("bundle.json"), r#"{"version":"1.4.0"}"#).expect("bundle");
        assert!(import(&root, &project).is_err());
        assert!(import(&root, &root.join("missing")).is_err());
        for root in [&root, &root2] {
            assert_eq!(imported(root), ["earlier import"; 3].map(String::from));
        }
    }

    #[test]
    fn a_missing_license_fails_after_the_manifests_as_in_the_script() {
        let (_temp, root, project) = fixture(json!({"version": 7}), json!({"version": 7}));
        fs::remove_file(project.join("LICENSE")).expect("license");
        assert!(import(&root, &project).is_err());
        // The copies run in order, so both manifests are already replaced.
        assert_eq!(
            imported(&root),
            [
                r#"{"version":7}"#.to_string(),
                r#"{"version":7}"#.to_string(),
                "earlier import".to_string()
            ]
        );
        fs::write(project.join("LICENSE"), "MIT License\n").expect("license");
        // A version that is not text is shown as JSON writes it.
        assert_eq!(import(&root, &project).expect("import"), "7");
    }
}
