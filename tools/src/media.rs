// SPDX-License-Identifier: MIT
//! Build the one pinned LGPL Wine module; no installed runner is modified.
use crate::util::{Failure, Result, digest, pretty, read_regular, require};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(clap::Args)]
pub struct Arguments {
    /// Directory containing the two original, hash-pinned source archives.
    #[arg(long)]
    pub sources: PathBuf,
    /// Fetch missing pinned archives over HTTPS before building.
    #[arg(long)]
    pub download: bool,
    /// A new directory for the disposable source and object trees.
    #[arg(long)]
    pub work: PathBuf,
    /// A new directory; use build/media when compiling bundled-media.
    #[arg(long)]
    pub output: PathBuf,
    #[arg(long, default_value_t=2, value_parser=clap::value_parser!(u32).range(1..=8))]
    pub jobs: u32,
}
pub fn lock(root: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&read_regular(
        &root.join("compat/media.lock.json"),
        65536,
    )?)?)
}
fn checked(command: &mut Command) -> Result<()> {
    let status = command.stdin(Stdio::null()).status()?;
    require(
        status.success(),
        format!(
            "Media build command failed: {}",
            command.get_program().to_string_lossy()
        ),
    )
}
fn pinned(path: &Path, expected: &Value) -> Result<Vec<u8>> {
    let bytes = read_regular(path, 128 * 1024 * 1024)?;
    require(
        expected.as_str() == Some(digest(&bytes).as_str()),
        "Media source or payload checksum differs",
    )?;
    Ok(bytes)
}

/// These two compressed upstream sources are the only extra source-release
/// inputs. Build outputs and arbitrary directories can never enter the archive.
pub fn source_files(root: &Path, folder: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let lock = lock(root)?;
    let mut result = BTreeMap::new();
    for (component, name) in [
        ("wine", "wine-source.tar.gz"),
        ("xgameruntime", "xgameruntime-source.tar.gz"),
    ] {
        result.insert(
            format!("media-sources/{name}"),
            pinned(&folder.join(name), &lock[component]["archive_sha256"])?,
        );
    }
    Ok(result)
}
pub fn verify_bundle(root: &Path, folder: &Path) -> Result<()> {
    let lock = lock(root)?;
    let bytes = pinned(
        &folder.join("winegstreamer.so"),
        &lock["files"]["winegstreamer.so"]["sha256"],
    )?;
    require(
        bytes.len() >= 64
            && bytes.starts_with(b"\x7fELF\x02\x01")
            && bytes[18..20] == [62, 0]
            && lock["files"]["winegstreamer.so"]["bytes"] == bytes.len(),
        "Expected the pinned Linux x86-64 media module",
    )
}

fn unpack(bytes: &[u8], destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    let mut gdb_link = false;
    for (index, entry) in archive.entries()?.enumerate() {
        require(index < 30000, "Too many Wine source entries")?;
        let mut entry = entry?;
        let full = entry.path()?.into_owned();
        require(
            full.components().all(|c| matches!(c, Component::Normal(_))),
            "Invalid Wine source path",
        )?;
        let relative: PathBuf = full.components().skip(1).collect();
        if relative.as_os_str().is_empty() {
            continue;
        }
        require(seen.insert(relative.clone()), "Duplicate Wine source entry")?;
        let target = destination.join(&relative);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if kind.is_symlink() && relative == Path::new("tools/gdbinit.py.in") {
            require(
                entry.link_name()?.as_deref() == Some(Path::new("gdbinit.py")),
                "Unexpected source link",
            )?;
            gdb_link = true;
            continue;
        }
        require(
            kind.is_file(),
            "Links and special source entries are not accepted",
        )?;
        let size = entry.size();
        total = total
            .checked_add(size)
            .ok_or_else(|| Failure("Source size overflow".into()))?;
        require(
            size <= 64 * 1024 * 1024 && total <= 1024 * 1024 * 1024,
            "Wine sources exceed the build limit",
        )?;
        fs::create_dir_all(target.parent().expect("source parent"))?;
        let mut output = fs::File::create_new(&target)?;
        let copied = std::io::copy(&mut (&mut entry).take(size + 1), &mut output)?;
        require(copied == size, "Truncated Wine source file")?;
        fs::set_permissions(
            &target,
            fs::Permissions::from_mode(if entry.header().mode()? & 0o111 != 0 {
                0o700
            } else {
                0o600
            }),
        )?;
    }
    // The sole upstream relative link is debugger input, not runtime code.
    // Materialize its already pinned target instead of allowing link traversal.
    if gdb_link {
        fs::copy(
            destination.join("tools/gdbinit.py"),
            destination.join("tools/gdbinit.py.in"),
        )?;
    }
    Ok(())
}

pub fn build(root: &Path, args: &Arguments) -> Result<()> {
    let lock = lock(root)?;
    fs::create_dir_all(&args.sources)?;
    for (component, name) in [
        ("wine", "wine-source.tar.gz"),
        ("xgameruntime", "xgameruntime-source.tar.gz"),
    ] {
        let path = args.sources.join(name);
        if !path.exists() && args.download {
            let url = lock[component]["archive_url"]
                .as_str()
                .ok_or_else(|| Failure("Missing media source URL".into()))?;
            require(
                url.starts_with("https://codeload.github.com/xodus-gaming/"),
                "Unexpected media source origin",
            )?;
            let temporary = path.with_extension("part");
            require(
                !temporary.exists(),
                "Unfinished media source download exists",
            )?;
            checked(
                Command::new("curl")
                    .args([
                        "--fail",
                        "--location",
                        "--silent",
                        "--show-error",
                        "--proto",
                        "=https",
                        "--proto-redir",
                        "=https",
                        "--connect-timeout",
                        "30",
                        "--max-time",
                        "300",
                        "--max-filesize",
                        "134217728",
                        "--output",
                    ])
                    .arg(&temporary)
                    .arg(url),
            )?;
            pinned(&temporary, &lock[component]["archive_sha256"])?;
            fs::rename(temporary, path)?;
        }
    }
    let inputs = source_files(root, &args.sources)?;
    require(
        fs::symlink_metadata(&args.work).is_err() && fs::symlink_metadata(&args.output).is_err(),
        "Media build/output directories must be new",
    )?;
    fs::create_dir_all(args.work.parent().unwrap_or(Path::new(".")))?;
    fs::create_dir(&args.work)?;
    let work = args.work.canonicalize()?;
    let source = work.join(format!(
        "wine-{}",
        lock["wine"]["revision"]
            .as_str()
            .ok_or_else(|| Failure("Missing Wine revision".into()))?
    ));
    unpack(&inputs["media-sources/wine-source.tar.gz"], &source)?;
    unpack(
        &inputs["media-sources/xgameruntime-source.tar.gz"],
        &source.join("dlls/xgameruntime"),
    )?;
    let parser = source.join("dlls/winegstreamer/wg_parser.c");
    pinned(&parser, &lock["parser_before_sha256"])?;
    let patch = root.join(
        lock["patch"]
            .as_str()
            .ok_or_else(|| Failure("Missing media patch".into()))?,
    );
    pinned(&patch, &lock["patch_sha256"])?;
    checked(
        Command::new("patch")
            .args(["--batch", "--fuzz=0", "-p1", "-i"])
            .arg(patch)
            .current_dir(&source),
    )?;
    pinned(&parser, &lock["parser_after_sha256"])?;
    let call = |program: &Path, arguments: &[String], cwd: &Path| -> Result<()> {
        checked(
            Command::new(program)
                .args(arguments)
                .current_dir(cwd)
                .env("LC_ALL", "C")
                .env("SOURCE_DATE_EPOCH", "1792627200")
                .env("XDG_CACHE_HOME", work.join("cache")),
        )
    };
    call(&source.join("tools/make_specfiles"), &[], &source)?;
    call(&source.join("tools/make_requests"), &[], &source)?;
    call(
        Path::new("python3"),
        &[
            source
                .join("dlls/winevulkan/make_vulkan")
                .display()
                .to_string(),
            "--xml".into(),
            source.join("dlls/winevulkan/vk.xml").display().to_string(),
            "--video-xml".into(),
            source
                .join("dlls/winevulkan/video.xml")
                .display()
                .to_string(),
        ],
        &source,
    )?;
    call(Path::new("autoreconf"), &["-f".into()], &source)?;
    let objects = work.join("objects");
    fs::create_dir(&objects)?;
    call(
        &source.join("configure"),
        &[
            "--enable-win64",
            "--disable-tests",
            "--enable-silent-rules",
            "--without-ffmpeg",
            "--without-unwind",
            "--without-x",
            "--without-freetype",
        ]
        .map(str::to_string)
        .into_iter()
        .chain([
            format!(
                "CFLAGS=-O2 -g0 -ffile-prefix-map={}=/usr/src/flightdeck-media",
                work.display()
            ),
            "LDFLAGS=-Wl,--build-id=none".into(),
        ])
        .collect::<Vec<_>>(),
        &objects,
    )?;
    call(
        Path::new("make"),
        &[
            format!("-j{}", args.jobs),
            "dlls/winegstreamer/winegstreamer.so".into(),
        ],
        &objects,
    )?;
    let module = objects.join("dlls/winegstreamer/winegstreamer.so");
    call(
        Path::new("strip"),
        &["--strip-unneeded".into(), module.display().to_string()],
        &objects,
    )?;
    fs::create_dir_all(&args.output)?;
    fs::copy(module, args.output.join("winegstreamer.so"))?;
    verify_bundle(root, &args.output)?;
    fs::write(
        args.output.join("build-manifest.json"),
        pretty(
            &json!({"schema":1,"id":lock["id"],"files":lock["files"],"source_inputs":lock["wine"],"patch_sha256":lock["patch_sha256"]}),
        ),
    )?;
    println!(
        "{}",
        json!({"status":"PASS","module_sha256":lock["files"]["winegstreamer.so"]["sha256"],"output":args.output})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corresponding_sources_are_exact_pinned_inputs_not_a_directory_export() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir(root.path().join("compat")).expect("compat");
        let inputs = root.path().join("inputs");
        fs::create_dir(&inputs).expect("inputs");
        fs::write(inputs.join("wine-source.tar.gz"), b"wine").expect("wine");
        fs::write(inputs.join("xgameruntime-source.tar.gz"), b"gdk").expect("gdk");
        fs::write(inputs.join("private.txt"), b"never export").expect("unrelated");
        fs::write(
            root.path().join("compat/media.lock.json"),
            pretty(&json!({
                "wine":{"archive_sha256":digest(b"wine")},
                "xgameruntime":{"archive_sha256":digest(b"gdk")}
            })),
        )
        .expect("lock");
        let sources = source_files(root.path(), &inputs).expect("pinned sources");
        assert_eq!(sources.len(), 2);
        assert_eq!(sources["media-sources/wine-source.tar.gz"], b"wine");
        fs::write(inputs.join("wine-source.tar.gz"), b"different source").expect("change");
        assert!(source_files(root.path(), &inputs).is_err());
    }
}
