// SPDX-License-Identifier: MIT
//! Release gating, archive bounds and fail-closed publication without network.
use super::*;
use crate::components::fixtures::{archive, files, raw_member};
use std::cell::RefCell;

const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

type Answer<'a> = Box<dyn Fn(&[&str]) -> Done + 'a>;
struct Fake<'a> {
    variables: BTreeMap<&'static str, String>,
    calls: RefCell<Vec<(Vec<String>, bool)>>,
    answer: Answer<'a>,
}
impl Host for Fake<'_> {
    fn variable(&self, name: &str) -> Option<String> {
        self.variables.get(name).cloned()
    }
    fn run(&self, command: &[&str], github: bool) -> Result<Done> {
        self.calls.borrow_mut().push((
            command.iter().map(|part| part.to_string()).collect(),
            github,
        ));
        Ok((self.answer)(command))
    }
}
impl<'a> Fake<'a> {
    fn new(answer: impl Fn(&[&str]) -> Done + 'a) -> Self {
        let variables = [
            ("GITHUB_EVENT_NAME", "push".to_string()),
            ("GITHUB_REF", "refs/heads/main".to_string()),
            ("GITHUB_REPOSITORY", REPOSITORY.to_string()),
            ("GITHUB_SHA", COMMIT.to_string()),
        ]
        .into();
        Self {
            variables,
            calls: RefCell::new(Vec::new()),
            answer: Box::new(answer),
        }
    }
    fn called(&self, words: &[&str]) -> bool {
        self.calls.borrow().iter().any(|(command, _)| {
            words
                .iter()
                .all(|word| command.iter().any(|part| part == word))
        })
    }
}
fn ok(stdout: impl Into<String>) -> Done {
    Done {
        code: 0,
        stdout: stdout.into(),
        stderr: String::new(),
    }
}
fn failed(stderr: &str) -> Done {
    Done {
        code: 1,
        stdout: String::new(),
        stderr: stderr.into(),
    }
}

fn checkout(version: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("temp");
    fs::write(
        root.path().join("Cargo.toml"),
        format!("[package]\nname = \"flightdeck-linux\"\nversion = \"{version}\"\n"),
    )
    .expect("manifest");
    root
}
fn pins() -> Pins {
    Pins {
        sources: [("source.tar.gz", SOURCE_SHA)].into(),
        ..Pins::current()
    }
}
// SHA-256 of b"synthetic source".
const SOURCE_SHA: &str = "39631bb5e5dc9326b23f8c00bf43440a84ce711315c6b799de554c56aa04d133";

#[test]
fn gate_requires_push_main_exact_commit_subject_and_an_absent_tag() {
    let root = checkout("0.2.6");
    let tag = format!("{COMMIT}\trefs/tags/v0.2.6");
    for (subject, head, refs, accepted) in [
        ("Release Flightdeck 0.2.6", COMMIT, "", true),
        ("Release Flightdeck 0.2.6 extra", COMMIT, "", false),
        ("Release Flightdeck 0.2.5", COMMIT, "", false),
        ("Release Flightdeck 0.2.6", OTHER, "", false),
        ("Release Flightdeck 0.2.6", COMMIT, tag.as_str(), false),
    ] {
        let host = Fake::new(|command| match command[1] {
            "rev-parse" => ok(format!("{head}\n")),
            "log" => ok(format!("{subject}\n")),
            "ls-remote" => ok(refs),
            other => panic!("unexpected git {other}"),
        });
        assert_eq!(
            eligible(&host, root.path()).expect("gate"),
            accepted,
            "{subject} {head}"
        );
        // The remote is asked only about an otherwise exact release commit.
        let asked = host.called(&["ls-remote", "refs/tags/v0.2.6", "refs/tags/v0.2.6^{}"]);
        assert_eq!(
            asked,
            head == COMMIT && subject == "Release Flightdeck 0.2.6"
        );
        assert!(host.calls.borrow().iter().all(|(_, github)| !github));
    }
    for (key, value) in [
        ("GITHUB_EVENT_NAME", "pull_request"),
        ("GITHUB_REF", "refs/heads/another"),
        ("GITHUB_REPOSITORY", "another/repository"),
    ] {
        let mut host = Fake::new(|_| panic!("no command for another event"));
        host.variables.insert(key, value.into());
        assert!(!eligible(&host, root.path()).expect("gate"), "{key}");
        assert!(host.calls.borrow().is_empty());
    }
    let unstable = checkout("0.2.6-rc.1");
    let host = Fake::new(|command| match command[1] {
        "rev-parse" => ok(COMMIT),
        _ => ok("Release Flightdeck 0.2.6-rc.1"),
    });
    assert!(eligible(&host, unstable.path()).is_err());
}

#[test]
fn archive_rejects_links_and_traversal_before_writing() {
    let temp = tempfile::tempdir().expect("temp");
    let manifest = raw_member("flightdeck-linux/FLIGHTDECK-PACKAGE.json", b'0', b"{}");
    for bad in [
        raw_member("flightdeck-linux/../../escape", b'0', b""),
        raw_member("flightdeck-linux/link", b'2', b""),
        raw_member("elsewhere/file", b'0', b""),
        raw_member("flightdeck-linux/FLIGHTDECK-PACKAGE.json", b'0', b"{}"),
    ] {
        let path = temp.path().join("bad.tar.gz");
        fs::write(&path, archive(&[manifest.clone(), bad])).expect("archive");
        assert!(unpack(&path, &temp.path().join("extracted")).is_err());
        assert!(!temp.path().join("extracted").exists());
    }
    let path = temp.path().join("no-manifest.tar.gz");
    fs::write(&path, files(&[("flightdeck-linux/bin/flightdeck", b"x")])).expect("archive");
    let error = unpack(&path, &temp.path().join("extracted")).expect_err("manifest");
    assert!(error.0.contains("Missing native package manifest"));
}

#[test]
fn unpacking_restores_only_the_expected_executable_bits() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("package.tar.gz");
    fs::write(
        &path,
        files(&[
            ("flightdeck-linux/FLIGHTDECK-PACKAGE.json", b"{}"),
            ("flightdeck-linux/bin/flightdeck", b"binary"),
            ("flightdeck-linux/install.sh", b"#!/bin/sh\n"),
            ("flightdeck-linux/resources/native/bin/xodus-cli", b"cli"),
            (
                "flightdeck-linux/resources/native/runtime/xgameruntime.dll",
                b"dll",
            ),
        ]),
    )
    .expect("archive");
    let output = temp.path().join("nested/extracted");
    unpack(&path, &output).expect("unpack");
    let mode = |name: &str| {
        fs::metadata(output.join("flightdeck-linux").join(name))
            .expect("member")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("bin/flightdeck"), 0o755);
    assert_eq!(mode("install.sh"), 0o755);
    assert_eq!(mode("resources/native/bin/xodus-cli"), 0o755);
    assert_eq!(mode("resources/native/runtime/xgameruntime.dll"), 0o644);
    assert_eq!(mode("FLIGHTDECK-PACKAGE.json"), 0o644);
    assert!(
        unpack(&path, &output).is_err(),
        "an existing destination is never reused"
    );
}

/// A staged release directory of synthetic assets, as `stage` leaves it.
fn staged(root: &Path, work: &Path) -> std::path::PathBuf {
    let (assets, cache) = (work.join("assets"), work.join("cache"));
    fs::create_dir_all(&assets).expect("assets");
    fs::create_dir_all(&cache).expect("cache");
    let pins = pins();
    let version = stable_version(root).expect("version");
    for name in asset_names(&pins, &version) {
        if !pins.sources.contains_key(name.as_str()) {
            fs::write(assets.join(&name), format!("synthetic {name}")).expect("asset");
        }
    }
    let binary = b"synthetic executable";
    fs::write(
        assets.join(PACKAGE),
        files(&[
            ("flightdeck-linux/bin/flightdeck", binary),
            ("flightdeck-linux/FLIGHTDECK-PACKAGE.json", b"{}"),
        ]),
    )
    .expect("package");
    fs::write(cache.join("source.tar.gz"), "synthetic source").expect("source");
    fs::write(assets.join("SHA256SUMS"), "").expect("sums");
    let check = work.join("check.json");
    fs::write(
        &check,
        json!({"status": "PASS", "native_binary_sha256": digest(binary),
            "checks": {"native_update_and_rollback": "PASS"}})
        .to_string(),
    )
    .expect("check");
    stage(root, &pins, &assets, &cache, &check, COMMIT).expect("stage");
    assets
}

#[test]
fn the_synthetic_source_pin_matches_its_bytes() {
    assert_eq!(digest(b"synthetic source"), SOURCE_SHA);
}

#[test]
fn stage_binds_exact_assets_commit_and_sources_and_rejects_tampering() {
    let root = checkout("0.2.6");
    let work = tempfile::tempdir().expect("temp");
    let assets = staged(root.path(), work.path());
    let hashes = verify(root.path(), &pins(), &assets, COMMIT).expect("verified");
    assert!(hashes.contains_key("SHA256SUMS") && hashes.contains_key("RELEASE-MANIFEST.json"));
    let manifest: Value =
        serde_json::from_slice(&fs::read(assets.join("RELEASE-MANIFEST.json")).expect("manifest"))
            .expect("JSON");
    assert_eq!(manifest["commit"], COMMIT);
    assert_eq!(manifest["version"], "0.2.6");
    assert_eq!(manifest["files"]["source.tar.gz"], SOURCE_SHA);
    let listed = fs::read_to_string(assets.join("SHA256SUMS")).expect("sums");
    assert_eq!(listed.lines().count(), hashes.len() - 1);
    assert!(listed.contains(&format!(
        "{}  RELEASE-MANIFEST.json\n",
        hashes["RELEASE-MANIFEST.json"]
    )));
    let error = verify(root.path(), &pins(), &assets, OTHER).expect_err("another commit");
    assert!(error.0.contains("version and commit"));
    let newer = checkout("0.2.7");
    assert!(verify(newer.path(), &pins(), &assets, COMMIT).is_err());
    fs::write(assets.join("extra"), "unexpected").expect("extra");
    assert!(verify(root.path(), &pins(), &assets, COMMIT).is_err());
    fs::remove_file(assets.join("extra")).expect("remove");
    let sums = fs::read(assets.join("SHA256SUMS")).expect("sums");
    fs::write(assets.join("SHA256SUMS"), "").expect("empty sums");
    let error = verify(root.path(), &pins(), &assets, COMMIT).expect_err("checksums");
    assert!(error.0.contains("checksums do not describe"));
    fs::write(assets.join("SHA256SUMS"), sums).expect("restore");
    fs::write(assets.join(PACKAGE), "changed").expect("change");
    let error = verify(root.path(), &pins(), &assets, COMMIT).expect_err("tampered");
    assert!(error.0.contains("changed after validation"));
}

#[test]
fn stage_refuses_failed_checks_other_binaries_and_changed_sources() {
    let root = checkout("0.2.6");
    let pins = pins();
    let attempt = |change: &dyn Fn(&Path)| {
        let work = tempfile::tempdir().expect("temp");
        let (assets, cache) = (work.path().join("assets"), work.path().join("cache"));
        fs::create_dir_all(&assets).expect("assets");
        fs::create_dir_all(&cache).expect("cache");
        for name in asset_names(&pins, "0.2.6") {
            if name != "source.tar.gz" {
                fs::write(assets.join(&name), "synthetic").expect("asset");
            }
        }
        fs::write(
            assets.join(PACKAGE),
            files(&[
                ("flightdeck-linux/bin/flightdeck", b"synthetic executable"),
                ("flightdeck-linux/FLIGHTDECK-PACKAGE.json", b"{}"),
            ]),
        )
        .expect("package");
        fs::write(cache.join("source.tar.gz"), "synthetic source").expect("source");
        fs::write(assets.join("SHA256SUMS"), "").expect("sums");
        fs::write(
            work.path().join("check.json"),
            json!({"status": "PASS", "native_binary_sha256": digest(b"synthetic executable"),
                "checks": {"native_update_and_rollback": "PASS"}})
            .to_string(),
        )
        .expect("check");
        change(work.path());
        let result = stage(
            root.path(),
            &pins,
            &assets,
            &cache,
            &work.path().join("check.json"),
            COMMIT,
        );
        (result, assets.join("RELEASE-MANIFEST.json").exists())
    };
    assert!(attempt(&|_| {}).0.is_ok());
    fn write_check(work: &Path, value: Value) {
        fs::write(work.join("check.json"), value.to_string()).expect("check");
    }
    type Change = Box<dyn Fn(&Path)>;
    let cases: [(&str, Change); 6] = [
        (
            "must pass",
            Box::new(|work| {
                write_check(
                    work,
                    json!({"status": "FAIL", "checks": {"native_update_and_rollback": "PASS"}}),
                )
            }),
        ),
        (
            "must pass",
            Box::new(|work| write_check(work, json!({"status": "PASS", "checks": {}}))),
        ),
        (
            "different packaged executable",
            Box::new(|work| {
                write_check(
                    work,
                    json!({"status": "PASS", "native_binary_sha256": digest(b"another"),
                "checks": {"native_update_and_rollback": "PASS"}}),
                )
            }),
        ),
        (
            "source archive differs",
            Box::new(|work| {
                fs::write(work.join("cache/source.tar.gz"), "changed").expect("source")
            }),
        ),
        (
            "missing or unexpected",
            Box::new(|work| fs::write(work.join("assets/extra"), "x").expect("extra")),
        ),
        (
            "missing or unexpected",
            Box::new(|work| {
                fs::remove_file(work.join("assets/launcher-dependencies.json")).expect("remove")
            }),
        ),
    ];
    for (message, change) in cases {
        let (result, wrote) = attempt(&*change);
        let error = result.expect_err(message);
        assert!(error.0.contains(message), "{}", error.0);
        assert!(!wrote, "{message}");
    }
    let work = tempfile::tempdir().expect("temp");
    assert!(
        stage(
            root.path(),
            &pins,
            work.path(),
            work.path(),
            work.path(),
            "main"
        )
        .is_err()
    );
}

/// GitHub as `gh` and `git` report it while a release is published.
struct Remote<'a> {
    assets: &'a Path,
    readback: &'a Path,
    corrupt: Option<&'a str>,
    tag_after: &'a str,
    existing: Done,
    published: RefCell<bool>,
}
impl Remote<'_> {
    fn answer(&self, command: &[&str]) -> Done {
        let has = |word: &str| command.contains(&word);
        match (command[0], command[1]) {
            ("git", "rev-parse") => ok(COMMIT),
            ("git", "log") => ok("Release Flightdeck 0.2.6"),
            ("git", "status") => ok(""),
            ("git", "ls-remote") => ok(self.refs()),
            ("gh", "api") => {
                *self.published.borrow_mut() = true;
                ok("")
            }
            ("gh", "release") if has("view") && has("tagName") && command.len() == 8 => Done {
                code: self.existing.code,
                stdout: self.existing.stdout.clone(),
                stderr: self.existing.stderr.clone(),
            },
            ("gh", "release") if has("view") && has("tagName,targetCommitish,isDraft,isPrerelease,assets") => {
                let assets: Vec<Value> = fs::read_dir(self.assets)
                    .expect("assets")
                    .map(|entry| {
                        let entry = entry.expect("entry");
                        json!({"name": entry.file_name().to_string_lossy(), "size": entry.metadata().expect("info").len()})
                    })
                    .collect();
                ok(json!({"tagName": "v0.2.6", "targetCommitish": COMMIT, "isDraft": true,
                    "isPrerelease": false, "assets": assets})
                .to_string())
            }
            ("gh", "release") if has("download") => {
                for entry in fs::read_dir(self.assets).expect("assets") {
                    let entry = entry.expect("entry");
                    fs::copy(entry.path(), self.readback.join(entry.file_name())).expect("copy");
                }
                if let Some(name) = self.corrupt {
                    fs::write(self.readback.join(name), "corrupted upload").expect("corrupt");
                }
                ok("")
            }
            ("gh", "release") if has("view") && has("tagName,targetCommitish,isDraft,isPrerelease") => ok(json!({
                "tagName": "v0.2.6", "targetCommitish": COMMIT, "isDraft": false, "isPrerelease": false})
            .to_string()),
            ("gh", "release") if has("view") => ok(json!({"tagName": "v0.2.6"}).to_string()),
            ("gh", "release") => ok(""),
            other => panic!("unexpected command {other:?}"),
        }
    }
    fn refs(&self) -> &str {
        if *self.published.borrow() {
            self.tag_after
        } else {
            ""
        }
    }
}

fn publication(
    corrupt: Option<&str>,
    tag_after: &str,
    existing: Done,
) -> (Result<()>, Vec<Vec<String>>) {
    let root = checkout("0.2.6");
    let work = tempfile::tempdir().expect("temp");
    let assets = staged(root.path(), work.path());
    let readback = work.path().join("readback");
    let remote = Remote {
        assets: &assets,
        readback: &readback,
        corrupt,
        tag_after,
        existing,
        published: RefCell::new(false),
    };
    let host = Fake::new(|command| remote.answer(command));
    let result = publish(
        &host,
        root.path(),
        &pins(),
        &assets,
        &work.path().join("notes.md"),
        &readback,
    );
    // Only GitHub commands receive the token.
    for (command, github) in host.calls.borrow().iter() {
        assert_eq!(*github, command[0] == "gh", "{command:?}");
    }
    let calls = host
        .calls
        .borrow()
        .iter()
        .map(|(command, _)| command.clone())
        .collect();
    (result, calls)
}
fn did(calls: &[Vec<String>], words: &[&str]) -> bool {
    calls.iter().any(|command| {
        words
            .iter()
            .all(|word| command.iter().any(|part| part == word))
    })
}

#[test]
fn a_verified_draft_is_published_as_latest_in_order() {
    let tag = format!("{COMMIT}\trefs/tags/v0.2.6");
    let (result, calls) = publication(None, &tag, failed("release not found\n"));
    result.expect("published");
    let position = |words: &[&str]| {
        calls
            .iter()
            .position(|command| {
                words
                    .iter()
                    .all(|word| command.iter().any(|part| part == word))
            })
            .unwrap_or_else(|| panic!("missing {words:?}"))
    };
    let order = [
        position(&["api", "--method", "POST", "ref=refs/tags/v0.2.6"]),
        position(&["create", "--draft", "--latest=false", "--verify-tag"]),
        position(&["upload"]),
        position(&["download"]),
        position(&["edit", "--draft=false", "--prerelease=false", "--latest"]),
    ];
    assert!(order.is_sorted(), "{order:?}");
    assert!(did(&calls, &["api", &format!("sha={COMMIT}")]));
    assert!(did(
        &calls,
        &["create", "--target", COMMIT, "--title", "Flightdeck 0.2.6"]
    ));
    assert!(calls.iter().all(|command| {
        !command
            .iter()
            .any(|part| part.contains("force") || part == "delete")
    }));
}

#[test]
fn bad_uploaded_bytes_leave_the_draft_unpublished() {
    let tag = format!("{COMMIT}\trefs/tags/v0.2.6");
    let (result, calls) = publication(Some(PACKAGE), &tag, failed("release not found"));
    assert!(
        result
            .expect_err("corrupted")
            .0
            .contains("changed after validation")
    );
    assert!(did(&calls, &["create", "--draft"]));
    assert!(!did(&calls, &["edit"]));
}

#[test]
fn an_existing_tag_or_release_or_a_moved_tag_stops_publication() {
    let tag = format!("{COMMIT}\trefs/tags/v0.2.6");
    // A release that exists, or whose absence cannot be established.
    for existing in [
        ok("{\"tagName\":\"v0.2.6\"}"),
        failed("HTTP 502"),
        failed(""),
    ] {
        let (result, calls) = publication(None, &tag, existing);
        assert!(
            result
                .expect_err("existing")
                .0
                .contains("absence could not be verified")
        );
        assert!(!did(&calls, &["api"]) && !did(&calls, &["create"]));
    }
    // The tag points elsewhere, or to two objects, once the draft is verified.
    for refs in [
        format!("{OTHER}\trefs/tags/v0.2.6"),
        format!("{COMMIT}\trefs/tags/v0.2.6\n{OTHER}\trefs/tags/v0.2.6^{{}}"),
        String::new(),
    ] {
        let (result, calls) = publication(None, &refs, failed("release not found"));
        assert!(
            result
                .expect_err("moved")
                .0
                .contains("tag changed during draft verification")
        );
        assert!(did(&calls, &["download"]) && !did(&calls, &["edit"]));
    }
}

#[test]
fn publication_requires_the_clean_exact_release_commit() {
    let root = checkout("0.2.6");
    let work = tempfile::tempdir().expect("temp");
    let assets = staged(root.path(), work.path());
    for (subject, status) in [
        ("Release Flightdeck 0.2.6", " M Cargo.toml\n"),
        ("Another commit", ""),
    ] {
        let host = Fake::new(|command| match command[1] {
            "rev-parse" => ok(COMMIT),
            "log" => ok(subject),
            "status" => ok(status),
            "ls-remote" => ok(""),
            other => panic!("unexpected {other}"),
        });
        let error = publish(
            &host,
            root.path(),
            &pins(),
            &assets,
            Path::new("notes.md"),
            &work.path().join("readback"),
        )
        .expect_err("not a clean release commit");
        assert!(error.0.contains("clean, exact main-branch release commit"));
        assert!(
            host.calls
                .borrow()
                .iter()
                .all(|(command, _)| command[0] == "git")
        );
    }
}

#[test]
fn release_notes_are_the_section_of_exactly_this_version() {
    let changelog = "# Changes\n\n## 0.2.11 — 12 October 2026\n\nStable release.\n\n- One fix.\n\n## 0.2.1 — 1 October 2026\n\n- Older.\n";
    assert_eq!(
        notes(changelog, "0.2.11").expect("notes"),
        "Stable release.\n\n- One fix.\n"
    );
    assert_eq!(notes(changelog, "0.2.1").expect("notes"), "- Older.\n");
    assert!(notes(changelog, "0.2").is_err());
    assert!(notes(changelog, "0.3.0").is_err());
    assert!(notes("## Unreleased\n\n- Pending.\n", "0.2.11").is_err());
}
