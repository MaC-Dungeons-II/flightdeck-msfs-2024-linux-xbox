// SPDX-License-Identifier: MIT
//! Bounded validation of the compatibility components a package may contain.
//! Archives are read member by member into memory and never extracted.
use crate::util::{
    Failure, JSON_MAX, Result, digest, read_regular, require, safe_name, strict_json,
};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
};

pub const NATIVE_FEATURES: [&str; 2] = ["connected-storage-read-v1", "connected-storage-sync-v1"];
pub const NATIVE_FILES: [&str; 6] = [
    "bin/xodus-cli",
    "bin/xodus-service",
    "bin/flightdeck-connected-storage.exe",
    "runtime/xgameruntime.dll",
    "builtin/x86_64-windows/xodus_store_test.dll",
    "builtin/x86_64-unix/xodus_store_test.so",
];
const NATIVE_EXTRA: [&str; 2] = ["manifest.json", "THIRD-PARTY-NOTICES.txt"];
const GRAPHICS_FILES: [&str; 5] = [
    "d3d12.dll",
    "d3d12core.dll",
    "dxgi.dll",
    "d3d11.dll",
    "d3d10core.dll",
];
pub const NATIVE_ARCHIVE_MAX: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy)]
pub struct Limits {
    pub file: u64,
    pub notice: u64,
    pub json: u64,
    pub total: u64,
}
pub const LIMITS: Limits = Limits {
    file: 128 * 1024 * 1024,
    notice: 16 * 1024 * 1024,
    json: JSON_MAX,
    total: 256 * 1024 * 1024,
};

pub type Members = BTreeMap<String, Vec<u8>>;

fn hash(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| {
        text.len() == 64
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// A non-empty map of canonical relative names to lowercase SHA-256 values.
pub fn hash_map<'a>(value: Option<&'a Value>, label: &str) -> Result<&'a Map<String, Value>> {
    let map = value
        .and_then(Value::as_object)
        .filter(|map| !map.is_empty())
        .ok_or_else(|| Failure(format!("{label} must contain file hashes")))?;
    for (name, checksum) in map {
        safe_name(name)?;
        require(
            hash(checksum).is_some(),
            format!("{label} contains an invalid SHA256"),
        )?;
    }
    Ok(map)
}

/// A regular file must not also be the directory of another member.
pub fn reject_parent_files(contents: &Members) -> Result<()> {
    for name in contents.keys() {
        let mut parent = name.as_str();
        while let Some((directory, _)) = parent.rsplit_once('/') {
            require(
                !contents.contains_key(directory),
                "A regular file cannot also be a parent directory",
            )?;
            parent = directory;
        }
    }
    Ok(())
}

/// Reads every member of a gzip-compressed tar archive into memory. `accept`
/// sees each canonical name with the members read so far and returns the size
/// limit of that member; anything but a plain regular file is refused.
pub fn regular_members(
    data: &[u8],
    total_max: u64,
    mut accept: impl FnMut(&str, &Members) -> Result<u64>,
) -> Result<Members> {
    let malformed = |_| Failure("Archive is malformed or truncated".into());
    let mut contents = Members::new();
    let mut total = 0u64;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(data));
    for entry in archive.entries().map_err(malformed)? {
        let mut entry = entry.map_err(malformed)?;
        let name = String::from_utf8(entry.path_bytes().into_owned()).map_err(|_| {
            Failure("Archive path must be a unique canonical relative file path".into())
        })?;
        let maximum = accept(safe_name(&name)?, &contents)?;
        let sparse = entry
            .pax_extensions()
            .map_err(malformed)?
            .into_iter()
            .flatten()
            .any(|extension| {
                extension.is_ok_and(|extension| extension.key_bytes().starts_with(b"GNU.sparse"))
            });
        require(
            entry.header().entry_type() == tar::EntryType::Regular && !sparse,
            "Archives may contain only regular files",
        )?;
        let size = entry.header().size().map_err(malformed)?;
        total = total.saturating_add(size);
        require(
            size <= maximum && total <= total_max,
            "Archive exceeds its expanded size limit",
        )?;
        let mut payload = Vec::new();
        entry
            .by_ref()
            .take(size + 1)
            .read_to_end(&mut payload)
            .map_err(malformed)?;
        require(payload.len() as u64 == size, "Archive member is truncated")?;
        contents.insert(name, payload);
    }
    reject_parent_files(&contents)?;
    Ok(contents)
}

/// The pinned compatibility archive: exactly six binaries, manifest and notices.
pub fn read_native(data: &[u8], limits: Limits) -> Result<Members> {
    regular_members(data, limits.total, |name, contents| {
        require(
            contents.len() < NATIVE_FILES.len() + NATIVE_EXTRA.len(),
            "Archive contains too many members",
        )?;
        require(
            !contents.contains_key(name)
                && (NATIVE_FILES.contains(&name) || NATIVE_EXTRA.contains(&name)),
            "Unexpected or duplicate native archive file",
        )?;
        Ok(match name {
            "manifest.json" => limits.json,
            "THIRD-PARTY-NOTICES.txt" => limits.notice,
            _ => limits.file,
        })
    })
}

pub fn validate_native_lock(lock: &Value) -> Result<()> {
    let features: Option<BTreeSet<&str>> = lock["features"]
        .as_array()
        .and_then(|items| items.iter().map(Value::as_str).collect());
    require(
        features.is_some_and(|found| NATIVE_FEATURES.iter().all(|name| found.contains(name))),
        "Native lock is missing a required ConnectedStorage capability",
    )?;
    let files = hash_map(lock.get("files"), "Native lock")?;
    require(
        files.len() == NATIVE_FILES.len()
            && NATIVE_FILES.iter().all(|name| files.contains_key(*name)),
        "Native lock must name exactly the six supported binaries",
    )?;
    require(
        hash(&lock["archive_sha256"]).is_some() && hash(&lock["notice_sha256"]).is_some(),
        "Native lock has an invalid archive or notice SHA256",
    )
}

pub fn verify_native(contents: &Members, lock: &Value, limits: Limits) -> Result<()> {
    validate_native_lock(lock)?;
    require(
        contents.len() == NATIVE_FILES.len() + NATIVE_EXTRA.len()
            && NATIVE_FILES
                .iter()
                .chain(&NATIVE_EXTRA)
                .all(|name| contents.contains_key(*name)),
        "Native archive must contain exactly six binaries, manifest and notices",
    )?;
    let manifest = strict_json(&contents["manifest.json"], "Native manifest", limits.json)?;
    require(
        manifest.len() == 2
            && manifest.get("format").and_then(Value::as_i64) == Some(1)
            && manifest["format"].is_u64()
            && manifest.get("files") == Some(&lock["files"]),
        "Native manifest differs from the verified source lock",
    )?;
    let expected = lock["files"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, checksum)| (name.as_str(), checksum))
        .chain([("THIRD-PARTY-NOTICES.txt", &lock["notice_sha256"])]);
    for (name, checksum) in expected {
        require(
            Some(digest(&contents[name]).as_str()) == checksum.as_str(),
            "Native binary or notice hash differs from the verified source lock",
        )?;
    }
    Ok(())
}

/// The graphics bundle named by the lock: five libraries, licence and manifest.
pub fn graphics_files(lock: &[u8], directory: &Path, limits: Limits) -> Result<Members> {
    let lock = Value::Object(strict_json(lock, "Graphics lock", limits.json)?);
    let files = hash_map(lock.get("files"), "Graphics lock")?;
    let base = hash_map(lock.get("base"), "Graphics base")?;
    let exact = |map: &Map<String, Value>| {
        map.len() == GRAPHICS_FILES.len()
            && GRAPHICS_FILES.iter().all(|name| map.contains_key(*name))
    };
    require(
        lock["schema"].as_u64() == Some(2) && exact(files) && exact(base),
        "Invalid graphics lock",
    )?;
    let license = hash(&lock["license_sha256"])
        .ok_or_else(|| Failure("Invalid graphics license hash".into()))?;
    let mut expected: BTreeMap<&str, &str> = files
        .iter()
        .map(|(name, checksum)| (name.as_str(), checksum.as_str().unwrap_or_default()))
        .collect();
    expected.insert("LICENSE", license);
    let unexpected = || Failure("Unexpected graphics bundle contents".into());
    require(
        !std::fs::symlink_metadata(directory)
            .map_err(|_| unexpected())?
            .file_type()
            .is_symlink(),
        "Unexpected graphics bundle contents",
    )?;
    let present: BTreeSet<String> = std::fs::read_dir(directory)
        .map_err(|_| unexpected())?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<_>>()?;
    let wanted: BTreeSet<String> = expected
        .keys()
        .map(|name| name.to_string())
        .chain(["manifest.json".to_string()])
        .collect();
    require(present == wanted, "Unexpected graphics bundle contents")?;
    let mut values = Members::new();
    for (name, checksum) in &expected {
        let data = read_regular(&directory.join(name), limits.file)?;
        require(
            digest(&data) == *checksum,
            "Graphics bundle differs from source lock",
        )?;
        values.insert(name.to_string(), data);
    }
    let manifest = read_regular(&directory.join("manifest.json"), limits.json)?;
    require(
        Value::Object(strict_json(&manifest, "Graphics manifest", limits.json)?)
            == json!({"schema": 2, "base": base, "files": files}),
        "Graphics manifest differs from source lock",
    )?;
    values.insert("manifest.json".into(), manifest);
    Ok(values)
}

#[cfg(test)]
pub mod fixtures {
    use std::io::Write;

    /// A tar member written byte for byte, so that tests can supply names and
    /// types a well-behaved archiver refuses to produce.
    pub fn raw_member(name: &str, kind: u8, data: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = kind;
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        let mut member = header.to_vec();
        member.extend_from_slice(data);
        member.resize(member.len().div_ceil(512) * 512, 0);
        member
    }

    pub fn archive(members: &[Vec<u8>]) -> Vec<u8> {
        let mut encoder = flate2::GzBuilder::new().write(Vec::new(), flate2::Compression::fast());
        for member in members {
            encoder.write_all(member).expect("member");
        }
        encoder.write_all(&[0; 1024]).expect("end");
        encoder.finish().expect("archive")
    }

    pub fn files(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let members: Vec<_> = entries
            .iter()
            .map(|(name, data)| raw_member(name, b'0', data))
            .collect();
        archive(&members)
    }
}

#[cfg(test)]
mod tests {
    use super::{fixtures::*, *};

    struct Fixture {
        members: Vec<(String, Vec<u8>)>,
        lock: Value,
    }
    fn fixture() -> Fixture {
        let mut members: Vec<(String, Vec<u8>)> = NATIVE_FILES
            .iter()
            .map(|name| (name.to_string(), format!("synthetic {name}").into_bytes()))
            .collect();
        let hashes: Map<String, Value> = members
            .iter()
            .map(|(name, data)| (name.clone(), digest(data).into()))
            .collect();
        members.push((
            "THIRD-PARTY-NOTICES.txt".into(),
            b"Synthetic fixture notice.".to_vec(),
        ));
        members.push((
            "manifest.json".into(),
            json!({"format": 1, "files": hashes})
                .to_string()
                .into_bytes(),
        ));
        let lock = json!({"features": NATIVE_FEATURES, "files": hashes,
            "archive_sha256": digest(b"archive"),
            "notice_sha256": digest(b"Synthetic fixture notice.")});
        Fixture { members, lock }
    }
    fn packed(members: &[(String, Vec<u8>)]) -> Vec<u8> {
        let entries: Vec<(&str, &[u8])> = members
            .iter()
            .map(|(name, data)| (name.as_str(), data.as_slice()))
            .collect();
        files(&entries)
    }
    fn verify(members: &[(String, Vec<u8>)], lock: &Value, limits: Limits) -> Result<()> {
        verify_native(&read_native(&packed(members), limits)?, lock, limits)
    }

    #[test]
    fn exact_native_members_and_notices_survive_without_extraction() {
        let fixture = fixture();
        let contents = read_native(&packed(&fixture.members), LIMITS).expect("archive");
        assert_eq!(
            contents,
            fixture.members.iter().cloned().collect::<Members>()
        );
        verify_native(&contents, &fixture.lock, LIMITS).expect("verified");
    }

    #[test]
    fn changed_missing_extra_duplicate_and_unsafe_members_fail() {
        let fixture = fixture();
        let entries = &fixture.members;
        let mut changes: Vec<Vec<(String, Vec<u8>)>> = vec![
            entries[..entries.len() - 1].to_vec(),
            [entries.as_slice(), &entries[..1]].concat(),
            [entries.as_slice(), &[("extra".to_string(), b"x".to_vec())]].concat(),
        ];
        for (name, _) in entries {
            changes.push(
                entries
                    .iter()
                    .map(|(n, data)| {
                        (
                            n.clone(),
                            if n == name {
                                b"changed".to_vec()
                            } else {
                                data.clone()
                            },
                        )
                    })
                    .collect(),
            );
        }
        for name in [
            "/absolute",
            "../escape",
            "bin//xodus-cli",
            "bin/./xodus-cli",
            "bin/back\\slash",
            "bin",
        ] {
            changes.push(
                [
                    &entries[..entries.len() - 1],
                    &[(name.to_string(), b"x".to_vec())],
                ]
                .concat(),
            );
        }
        for changed in changes {
            assert!(
                verify(&changed, &fixture.lock, LIMITS).is_err(),
                "{}",
                changed[0].0
            );
        }
        // Links, FIFOs, directories, devices and sparse files are never members.
        for kind in *b"21653S" {
            let mut members = vec![raw_member(&entries[0].0, kind, b"")];
            members.extend(
                entries[1..]
                    .iter()
                    .map(|(name, data)| raw_member(name, b'0', data)),
            );
            let error = read_native(&archive(&members), LIMITS).expect_err("irregular member");
            assert!(
                error.0.contains("regular files") || error.0.contains("malformed"),
                "{}",
                kind as char
            );
        }
    }

    #[test]
    fn capability_and_hash_pins_are_required() {
        let fixture = fixture();
        let contents: Members = fixture.members.iter().cloned().collect();
        let with = |key: &str, value: Value| {
            let mut lock = fixture.lock.clone();
            lock[key] = value;
            lock
        };
        for features in [
            Value::Null,
            json!([]),
            json!("connected-storage-read-v1"),
            json!([true]),
            json!(["connected-storage-read-v1"]),
            json!(["connected-storage-read-v1", "connected-storage-sync-v1", 1]),
        ] {
            assert!(verify_native(&contents, &with("features", features), LIMITS).is_err());
        }
        for (key, value) in [
            ("files", json!({"other": "0".repeat(64)})),
            ("archive_sha256", json!("bad")),
            ("notice_sha256", json!("0".repeat(64))),
        ] {
            assert!(
                verify_native(&contents, &with(key, value), LIMITS).is_err(),
                "{key}"
            );
        }
    }

    #[test]
    fn inputs_are_bounded_and_json_is_strict() {
        let fixture = fixture();
        for limits in [
            Limits { file: 1, ..LIMITS },
            Limits { total: 1, ..LIMITS },
            Limits {
                notice: 1,
                ..LIMITS
            },
            Limits { json: 1, ..LIMITS },
        ] {
            assert!(verify(&fixture.members, &fixture.lock, limits).is_err());
        }
        assert_eq!(
            read_native(b"not an archive", LIMITS)
                .expect_err("garbage")
                .0,
            "Archive is malformed or truncated"
        );
        let whole = packed(&fixture.members);
        assert!(read_native(&whole[..whole.len() / 2], LIMITS).is_err());
        let nine: Vec<(String, Vec<u8>)> = (0..9).map(|n| (format!("bin/{n}"), vec![])).collect();
        assert!(read_native(&packed(&nine), LIMITS).is_err());
    }

    #[test]
    fn graphics_bundle_requires_exact_files_hashes_and_manifest() {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path();
        let files: Map<String, Value> = GRAPHICS_FILES
            .iter()
            .map(|name| (name.to_string(), digest(name.as_bytes()).into()))
            .collect();
        let lock = json!({"schema": 2, "base": files, "files": files, "license_sha256": digest(b"license")})
            .to_string()
            .into_bytes();
        for name in GRAPHICS_FILES {
            std::fs::write(root.join(name), name).expect("library");
        }
        std::fs::write(root.join("LICENSE"), "license").expect("license");
        std::fs::write(
            root.join("manifest.json"),
            json!({"schema": 2, "base": files, "files": files}).to_string(),
        )
        .expect("manifest");
        let found = graphics_files(&lock, root, LIMITS).expect("bundle");
        assert_eq!(found.len(), 7);
        assert!(found.contains_key("manifest.json") && found.contains_key("LICENSE"));
        std::fs::write(root.join("dxgi.dll"), "changed").expect("change");
        assert!(graphics_files(&lock, root, LIMITS).is_err());
        std::fs::write(root.join("dxgi.dll"), "dxgi.dll").expect("restore");
        std::fs::write(root.join("extra"), "unreviewed").expect("extra");
        assert!(graphics_files(&lock, root, LIMITS).is_err());
        std::fs::remove_file(root.join("extra")).expect("remove");
        std::fs::write(
            root.join("manifest.json"),
            json!({"schema": 2, "base": files, "files": {}}).to_string(),
        )
        .expect("manifest");
        assert!(graphics_files(&lock, root, LIMITS).is_err());
    }

    #[test]
    fn a_file_cannot_also_be_a_directory() {
        let members: Members = [("a".to_string(), vec![]), ("a/b/c".to_string(), vec![])].into();
        assert!(reject_parent_files(&members).is_err());
        let members: Members = [("a/b".to_string(), vec![]), ("a/c".to_string(), vec![])].into();
        assert!(reject_parent_files(&members).is_ok());
    }
}
