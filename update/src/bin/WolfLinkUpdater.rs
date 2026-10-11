#![cfg_attr(windows, windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
use wolflink_update::{
    MANAGED_FILES, SignedManifest, embedded_public_key, extract_archive, validate_archive,
};

const SYNCHRONIZE: u32 = 0x0010_0000;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Deserialize, Serialize)]
struct Journal {
    install: PathBuf,
    backup: PathBuf,
    files: Vec<String>,
    helper_pid: u32,
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn replace_file(target: &Path, replacement: &Path) -> Result<(), String> {
    let target = wide(target.as_os_str());
    let replacement = wide(replacement.as_os_str());
    let ok = unsafe {
        ReplaceFileW(
            target.as_ptr(),
            replacement.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if ok == 0 {
        return Err(format!(
            "Atomic file replacement failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn wait_for_parent(pid: u32) -> Result<(), String> {
    let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        // ERROR_INVALID_PARAMETER means the PID no longer exists. Access denied is not proof of exit.
        if std::io::Error::last_os_error().raw_os_error() == Some(87) {
            return Ok(());
        }
        return Err("Cannot verify that WolfLink has exited".into());
    }
    let waited = unsafe { WaitForSingleObject(handle, 60_000) };
    unsafe { CloseHandle(handle) };
    if waited != WAIT_OBJECT_0 {
        return Err("WolfLink did not shut down within 60 seconds; update was not applied".into());
    }
    Ok(())
}

fn verify_zip(zip: &Path, manifest_path: &Path) -> Result<SignedManifest, String> {
    let signed: SignedManifest =
        serde_json::from_slice(&fs::read(manifest_path).map_err(|error| error.to_string())?)
            .map_err(|_| "Malformed update manifest")?;
    let verified = signed.verify(&embedded_public_key()?)?;
    if zip.file_name().and_then(|name| name.to_str()) != Some(&verified.payload.artifact) {
        return Err("Update ZIP name differs from signed manifest".into());
    }
    let mut file = File::open(zip).map_err(|error| error.to_string())?;
    let mut digest = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        size += count as u64;
        if size > verified.payload.size {
            return Err("Update ZIP exceeds signed size".into());
        }
        digest.update(&buffer[..count]);
    }
    if size != verified.payload.size
        || hex::encode(digest.finalize()) != verified.payload.sha256.to_lowercase()
    {
        return Err("Update ZIP does not match signed size and digest".into());
    }
    validate_archive(zip)?;
    Ok(signed)
}

fn journal_path() -> Result<PathBuf, String> {
    #[cfg(debug_assertions)]
    if let Some(root) = std::env::var_os("WOLFLINK_TEST_UPDATE_ROOT") {
        return Ok(PathBuf::from(root).join("pending.json"));
    }
    let local = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is unavailable")?;
    Ok(PathBuf::from(local)
        .join("WolfLink")
        .join("updates")
        .join("pending.json"))
}

fn write_journal(path: &Path, journal: &Journal) -> Result<(), String> {
    let bytes = serde_json::to_vec(journal).map_err(|error| error.to_string())?;
    let mut file = File::create(path).map_err(|error| error.to_string())?;
    use std::io::Write;
    file.write_all(&bytes).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())
}

fn rollback(journal: &Journal) -> Result<(), String> {
    for name in &journal.files {
        let old = journal.backup.join(name);
        let target = journal.install.join(name);
        if old.is_file() {
            if target.exists() {
                let restore = journal.backup.join(format!("{name}.restore"));
                fs::copy(&old, &restore).map_err(|error| error.to_string())?;
                if let Err(error) = replace_file(&target, &restore) {
                    let _ = fs::remove_file(&restore);
                    return Err(error);
                }
            } else {
                fs::copy(&old, &target).map_err(|error| error.to_string())?;
            }
        } else if target.exists() {
            fs::remove_file(&target).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn recover(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let journal: Journal =
        serde_json::from_slice(&fs::read(path).map_err(|error| error.to_string())?)
            .map_err(|_| "Update recovery journal is corrupt")?;
    let root = path.parent().ok_or("Update recovery root is unavailable")?;
    let canonical_root = fs::canonicalize(root).map_err(|error| error.to_string())?;
    let canonical_backup = fs::canonicalize(&journal.backup).map_err(|error| error.to_string())?;
    if !journal.install.is_dir()
        || !journal.backup.is_dir()
        || canonical_backup.parent() != Some(canonical_root.as_path())
        || !canonical_backup
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("backup-"))
        || !journal.install.join("WolfLink.exe").is_file()
        || journal
            .files
            .iter()
            .any(|name| !MANAGED_FILES.contains(&name.as_str()))
    {
        return Err("Update recovery journal has unsafe paths".into());
    }
    rollback(&journal)?;
    fs::remove_file(path).map_err(|error| error.to_string())?;
    Ok(())
}

fn launch(install: &Path) -> Result<std::process::Child, String> {
    use std::os::windows::process::CommandExt;
    Command::new(install.join("WolfLink.exe"))
        .current_dir(install)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|error| format!("Updated WolfLink did not start: {error}"))
}

fn apply(
    install: &Path,
    zip: &Path,
    manifest: &Path,
    parent_pid: u32,
    ready: &Path,
) -> Result<(), String> {
    let canonical_install = fs::canonicalize(install).map_err(|error| error.to_string())?;
    if canonical_install.parent().is_none()
        || !canonical_install.join("WolfLink.exe").is_file()
        || !canonical_install.join("client.exe").is_file()
    {
        return Err("Update target is not a WolfLink portable installation".into());
    }
    let journal_file = journal_path()?;
    if journal_file.exists() {
        return Err("An unfinished update requires recovery first".into());
    }
    let signed = verify_zip(zip, manifest)?;
    let version = signed.payload.version;
    let root = journal_file
        .parent()
        .ok_or("Update storage path unavailable")?;
    fs::create_dir_all(root).map_err(|error| error.to_string())?;
    let stage = root.join(format!("stage-{version}-{}", std::process::id()));
    let backup = root.join(format!("backup-{version}-{}", std::process::id()));
    if stage.exists() || backup.exists() {
        return Err("Update staging or backup already exists; refusing to overwrite".into());
    }
    fs::create_dir(&stage).map_err(|error| error.to_string())?;
    fs::create_dir(&backup).map_err(|error| error.to_string())?;
    let extracted = extract_archive(zip, &stage)?;
    let mut files: Vec<String> = extracted
        .iter()
        .filter_map(|path| path.file_name()?.to_str().map(ToOwned::to_owned))
        .collect();
    // Replace the launcher last so interrupted updates can still start recovery.
    files.sort_by_key(|name| name == "WolfLink.exe");
    for name in &files {
        let target = canonical_install.join(name);
        if target
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err("Refusing to replace a symlinked application file".into());
        }
        if target.exists() {
            fs::copy(&target, backup.join(name)).map_err(|error| error.to_string())?;
        }
    }
    let journal = Journal {
        install: canonical_install.clone(),
        backup: backup.clone(),
        files,
        helper_pid: std::process::id(),
    };
    if ready.exists() {
        return Err("Update handoff marker already exists".into());
    }
    fs::write(ready, b"ready").map_err(|error| error.to_string())?;
    wait_for_parent(parent_pid)?;
    if let Err(error) = write_journal(&journal_file, &journal) {
        let _ = launch(&canonical_install);
        return Err(error);
    }
    let install_result = (|| {
        for name in &journal.files {
            let target = canonical_install.join(name);
            let replacement = stage.join(name);
            if target.exists() {
                replace_file(&target, &replacement)?;
            } else {
                fs::rename(&replacement, &target).map_err(|error| error.to_string())?;
            }
        }
        let mut restarted = launch(&canonical_install)?;
        thread::sleep(Duration::from_secs(3));
        if let Some(status) = restarted.try_wait().map_err(|error| error.to_string())? {
            return Err(format!(
                "Updated WolfLink exited immediately after restart ({status})"
            ));
        }
        Ok::<(), String>(())
    })();
    if let Err(error) = install_result {
        recover(&journal_file)?;
        let _ = launch(&canonical_install);
        return Err(error);
    }
    fs::remove_file(&journal_file).map_err(|error| error.to_string())?;
    let _ = fs::remove_dir_all(&stage);
    let _ = fs::remove_dir_all(&backup);
    Ok(())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    match args.as_slice() {
        [_, command, install, zip, manifest, pid, ready] if command == "--apply" => {
            let pid = pid.parse().map_err(|_| "Invalid WolfLink process ID")?;
            apply(
                Path::new(install),
                Path::new(zip),
                Path::new(manifest),
                pid,
                Path::new(ready),
            )
        }
        [_, command, pid] if command == "--recover" => {
            let pid = pid.parse().map_err(|_| "Invalid WolfLink process ID")?;
            wait_for_parent(pid)?;
            let path = journal_path()?;
            let journal: Journal =
                serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
                    .map_err(|_| "Update recovery journal is corrupt")?;
            recover(&path)?;
            launch(&journal.install)?;
            Ok(())
        }
        _ => Err(
            "Usage: WolfLinkUpdater --apply INSTALL ZIP MANIFEST PID READY | --recover PID".into(),
        ),
    }
}

fn main() {
    if let Err(error) = run() {
        if let Ok(path) = journal_path()
            && let Some(root) = path.parent()
        {
            let _ = fs::create_dir_all(root);
            let _ = fs::write(root.join("last-error.txt"), &error);
        }
        eprintln!("WolfLink update failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "wolflink-helper-test-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("Cannot create isolated update test directory: {error}"),
            }
        }
    }

    #[test]
    fn interrupted_replacement_restores_previous_bundle_and_preserves_settings() {
        let root = temp_dir();
        let install = root.join("install");
        let updates = root.join("updates");
        let backup = updates.join("backup-test");
        fs::create_dir(&install).unwrap();
        fs::create_dir(&updates).unwrap();
        fs::create_dir(&backup).unwrap();
        fs::write(install.join("WolfLink.exe"), b"new").unwrap();
        fs::write(install.join("client.exe"), b"new").unwrap();
        fs::write(install.join("settings.json"), b"user setting").unwrap();
        fs::write(backup.join("WolfLink.exe"), b"old").unwrap();
        fs::write(backup.join("client.exe"), b"old").unwrap();
        let journal = Journal {
            install: install.clone(),
            backup: backup.clone(),
            files: vec![
                "client.exe".into(),
                "WolfLink.exe".into(),
                "discord_krisp.dll".into(),
            ],
            helper_pid: 1,
        };
        fs::write(install.join("discord_krisp.dll"), b"new only").unwrap();
        let path = updates.join("pending.json");
        write_journal(&path, &journal).unwrap();
        recover(&path).unwrap();
        assert_eq!(fs::read(install.join("WolfLink.exe")).unwrap(), b"old");
        assert_eq!(fs::read(install.join("client.exe")).unwrap(), b"old");
        assert!(!install.join("discord_krisp.dll").exists());
        assert_eq!(
            fs::read(install.join("settings.json")).unwrap(),
            b"user setting"
        );
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_rejects_backup_outside_update_root() {
        let root = temp_dir();
        let install = root.join("install");
        let updates = root.join("updates");
        let other = root.join("other");
        fs::create_dir(&install).unwrap();
        fs::create_dir(&updates).unwrap();
        fs::create_dir(&other).unwrap();
        fs::write(install.join("WolfLink.exe"), b"old").unwrap();
        let path = updates.join("pending.json");
        write_journal(
            &path,
            &Journal {
                install,
                backup: other,
                files: vec!["WolfLink.exe".into()],
                helper_pid: 1,
            },
        )
        .unwrap();
        assert!(recover(&path).is_err());
        assert!(path.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
