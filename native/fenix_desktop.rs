// SPDX-License-Identifier: MIT
//! Modern .NET Desktop prerequisite of the Fenix v3 installer.
use crate::{Result, backend::Context, bootstrap, error::require, wine::Wine};
use std::{fs, path::Path, time::Duration};

// Verified against Microsoft's 10.0 release metadata (SHA-512), then pinned here.
const PACKAGE: &str = "windowsdesktop-runtime-10.0.13-win-x64.exe";
const URL: &str = "https://builds.dotnet.microsoft.com/dotnet/WindowsDesktop/10.0.13/windowsdesktop-runtime-10.0.13-win-x64.exe";
const SHA256: &str = "f5f0268b3d89ec3b9eb952b620330360b35c63593ead45816aeb26e934a4b7b6";

fn version(name: &str) -> bool {
    let mut parts = name.split('.');
    parts.next() == Some("10")
        && parts.next() == Some("0")
        && parts
            .next()
            .is_some_and(|patch| !patch.is_empty() && patch.bytes().all(|b| b.is_ascii_digit()))
        && parts.next().is_none()
}

pub fn installed(prefix: &Path) -> Result<bool> {
    let dotnet = prefix.join("drive_c/Program Files/dotnet");
    if !dotnet.join("dotnet.exe").is_file() {
        return Ok(false);
    }
    let entries = match fs::read_dir(dotnet.join("shared/Microsoft.WindowsDesktop.App")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|name| version(name)) else {
            continue;
        };
        if ["PresentationFramework.dll", "WindowsBase.dll"]
            .iter()
            .all(|file| entry.path().join(file).is_file())
            && ["coreclr.dll", "System.Private.CoreLib.dll"]
                .iter()
                .all(|file| {
                    dotnet
                        .join("shared/Microsoft.NETCore.App")
                        .join(name)
                        .join(file)
                        .is_file()
                })
            && dotnet
                .join("host/fxr")
                .join(name)
                .join("hostfxr.dll")
                .is_file()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn ensure(wine: &Wine<'_>, cache: &Path, ctx: &Context) -> Result<()> {
    if installed(&wine.prefix)? {
        return Ok(());
    }
    ctx.progress(
        "Microsoft .NET 10 Desktop Runtime für den Fenix-Installer wird heruntergeladen …",
    );
    let package = bootstrap::download(URL, SHA256, &cache.join(PACKAGE), &ctx.cancel, |_| {})?;
    ctx.progress(
        "Microsoft .NET 10 Desktop Runtime wird installiert. Das kann einige Minuten dauern …",
    );
    wine.run(
        &[
            package.as_os_str(),
            "/install".as_ref(),
            "/quiet".as_ref(),
            "/norestart".as_ref(),
        ],
        None,
        &[0, 3010],
        Duration::from_secs(1800),
    )?;
    require(
        installed(&wine.prefix)?,
        "Microsoft .NET 10 Desktop Runtime wurde nicht vollständig installiert. Bitte erneut versuchen; Einzelheiten stehen im privaten Fenix-Protokoll.",
    )?;
    ctx.progress("Microsoft .NET 10 Desktop Runtime ist installiert.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn put(path: &Path) {
        fs::create_dir_all(path.parent().expect("fixture parent")).expect("fixture directory");
        fs::write(path, "fixture").expect("fixture file");
    }
    fn runtime(prefix: &Path, name: &str) {
        let dotnet = prefix.join("drive_c/Program Files/dotnet");
        put(&dotnet.join("dotnet.exe"));
        put(&dotnet.join("host/fxr").join(name).join("hostfxr.dll"));
        for file in ["coreclr.dll", "System.Private.CoreLib.dll"] {
            put(&dotnet
                .join("shared/Microsoft.NETCore.App")
                .join(name)
                .join(file));
        }
        for file in ["PresentationFramework.dll", "WindowsBase.dll"] {
            put(&dotnet
                .join("shared/Microsoft.WindowsDesktop.App")
                .join(name)
                .join(file));
        }
    }
    #[test]
    fn desktop_10_requires_stable_x64_host_core_and_wpf_files() {
        let temp = tempfile::tempdir().expect("fixture directory");
        let prefix = temp.path();
        assert!(!installed(prefix).expect("runtime detection"));
        runtime(prefix, "8.0.31");
        runtime(prefix, "10.0.0-preview.1");
        assert!(!installed(prefix).expect("runtime detection"));
        runtime(prefix, "10.0.13");
        assert!(installed(prefix).expect("runtime detection"));
        fs::remove_file(
            prefix.join(
                "drive_c/Program Files/dotnet/shared/Microsoft.NETCore.App/10.0.13/coreclr.dll",
            ),
        )
        .expect("update fixture");
        assert!(!installed(prefix).expect("runtime detection"));
        runtime(prefix, "10.0.99");
        assert!(installed(prefix).expect("runtime detection"));
        fs::rename(
            prefix.join("drive_c/Program Files"),
            prefix.join("drive_c/Program Files (x86)"),
        )
        .expect("update fixture");
        assert!(!installed(prefix).expect("runtime detection"));
    }
}
