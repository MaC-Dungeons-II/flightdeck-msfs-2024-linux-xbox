// SPDX-License-Identifier: MIT
//! Remove only demonstrably missing persisted .NET startup-hook paths.
//! Existing hooks and unresolved/relative/expanded values are preserved.
use crate::{Result, error::require, files, process, wine::Wine};
use serde_json::{Value, json};
use std::{fs, path::Path, time::Duration};

fn missing(prefix: &Path, hook: &str) -> bool {
    let bytes = hook.as_bytes();
    if !hook.is_ascii()
        || bytes.len() < 3
        || !bytes[0].eq_ignore_ascii_case(&b'c')
        || bytes[1] != b':'
        || !b"\\/".contains(&bytes[2])
        || hook.contains(['%', '\0', '\n', '\r'])
    {
        return false;
    }
    let mut path = prefix.join("drive_c");
    for part in hook[3..].split(['\\', '/']) {
        if part.is_empty() || part == "." {
            continue;
        }
        // Ambiguous paths must not cause a destructive registry change.
        if part == ".."
            || part.ends_with(['.', ' '])
            || part.contains(['*', '?', '<', '>', '|', ':', '"'])
        {
            return false;
        }
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) => return false,
        };
        let mut next = None;
        for (count, entry) in entries.take(100_001).enumerate() {
            if count >= 100_000 {
                return false;
            }
            let Ok(entry) = entry else {
                return false;
            };
            if entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(part)
            {
                next = Some(entry.path());
                break;
            }
        }
        let Some(next) = next else {
            return true;
        };
        path = next;
    }
    matches!(fs::metadata(path),Err(error) if error.kind()==std::io::ErrorKind::NotFound)
}

fn filtered(prefix: &Path, value: &str) -> Option<String> {
    let hooks: Vec<_> = value.split(';').collect();
    let kept: Vec<_> = hooks
        .iter()
        .copied()
        .filter(|hook| !missing(prefix, hook.trim()))
        .collect();
    (kept.len() != hooks.len()).then(|| kept.join(";"))
}

pub(crate) fn prepare(wine: &Wine<'_>) -> Result<()> {
    for key in [
        r"HKCU\Environment",
        r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
    ] {
        let mut command = wine.command()?;
        let (status, output) = process::output_status(
            command.args(["reg", "query", key, "/v", "DOTNET_STARTUP_HOOKS"]),
            Duration::from_secs(30),
            16384,
            wine.cancel,
        )?;
        if !status.success() {
            continue;
        }
        let output = String::from_utf8_lossy(&output);
        let entry = output.lines().find_map(|line| {
            let line = line.trim_start();
            let (name, rest) = line.split_once(char::is_whitespace)?;
            if !name.eq_ignore_ascii_case("DOTNET_STARTUP_HOOKS") {
                return None;
            }
            let (kind, value) = rest.trim_start().split_once(char::is_whitespace)?;
            ["REG_SZ", "REG_EXPAND_SZ"]
                .contains(&kind)
                .then(|| (kind, value.trim_start()))
        });
        let Some((kind, value)) = entry else {
            continue;
        };
        let Some(replacement) = filtered(&wine.prefix, value) else {
            continue;
        };
        // Keep the original value locally, never in safe diagnostics or HTTP.
        let backup = wine.prefix.join(".flightdeck-fenix-environment.json");
        let mut record: Value = if files::exists(&backup) {
            files::json(&backup, 65536)?
        } else {
            json!({"schema":1,"changes":[]})
        };
        require(
            record["schema"] == 1 && record["changes"].as_array().is_some_and(|v| v.len() < 32),
            "Die Sicherung der Fenix-Umgebung konnte nicht geprüft werden.",
        )?;
        record["changes"].as_array_mut().expect("checked array").push(json!({"key":key,"name":"DOTNET_STARTUP_HOOKS","kind":kind,"before":value,"after":replacement,"at":files::now()}));
        files::atomic_json(&backup, &record)?;
        if replacement.is_empty() {
            wine.run(
                &["reg", "delete", key, "/v", "DOTNET_STARTUP_HOOKS", "/f"],
                None,
                &[0],
                Duration::from_secs(30),
            )?;
        } else {
            wine.reg(key, "DOTNET_STARTUP_HOOKS", &replacement, kind)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_missing_absolute_paths_are_removed_and_existing_case_insensitive_hooks_survive() {
        let temp = tempfile::tempdir().expect("prefix");
        files::private_dir(&temp.path().join("drive_c/Hooks")).expect("hooks");
        fs::write(
            temp.path().join("drive_c/Hooks/Valid.dll"),
            b"retained user hook",
        )
        .expect("hook");
        let value = r"C:\Missing.dll;C:\hooks\valid.DLL;CustomAssembly;%USERPROFILE%\hook.dll;Z:\outside.dll";
        assert_eq!(
            filtered(temp.path(), value).as_deref(),
            Some(r"C:\hooks\valid.DLL;CustomAssembly;%USERPROFILE%\hook.dll;Z:\outside.dll")
        );
        assert_eq!(
            filtered(temp.path(), r"C:\missing.dll"),
            Some(String::new())
        );
        assert_eq!(filtered(temp.path(), r"C:\..\ambiguous.dll"), None);
        assert_eq!(filtered(temp.path(), r"C:\Hooks\Valid.dll"), None);
        assert_eq!(filtered(temp.path(), r"C:\Hooks\Valid.dll."), None);
        assert_eq!(filtered(temp.path(), r"C:\Hooks\Ünicode.dll"), None);
        assert_eq!(filtered(temp.path(), r"C:\Hooks\Valid.dll:stream"), None);
    }
}
