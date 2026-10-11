use ed25519_dalek::SigningKey;
use semver::Version;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use wolflink_update::{
    APP_ID, Channel, ManifestPayload, SignedManifest, signing_key_from_random, validate_archive,
};

fn key_path() -> Result<PathBuf, String> {
    let appdata = std::env::var_os("APPDATA").ok_or("APPDATA is unavailable")?;
    Ok(PathBuf::from(appdata)
        .join("WolfLinkPublisher")
        .join("signing-key.dpapi"))
}

fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let result = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if result == 0 {
        return Err(format!(
            "DPAPI protection failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let encrypted = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let encrypted = encrypted.to_vec();
    unsafe { LocalFree(output.pbData as _) };
    Ok(encrypted)
}

fn unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let result = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if result == 0 {
        return Err(format!(
            "DPAPI decryption failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let plain = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let plain = plain.to_vec();
    unsafe { LocalFree(output.pbData as _) };
    Ok(plain)
}

fn read_key(path: &Path) -> Result<SigningKey, String> {
    let encrypted = fs::read(path).map_err(|error| format!("Read signing key: {error}"))?;
    let plain = unprotect(&encrypted)?;
    let bytes: [u8; 32] = plain
        .try_into()
        .map_err(|_| "Protected signing key has invalid length")?;
    Ok(SigningKey::from_bytes(&bytes))
}

fn init(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let key = signing_key_from_random()?;
    let encrypted = protect(&key.to_bytes())?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("Signing key already exists or cannot be created: {error}"))?;
    output
        .write_all(&encrypted)
        .map_err(|error| error.to_string())?;
    output.sync_all().map_err(|error| error.to_string())?;
    println!(
        "Signing key protected by Windows DPAPI at {}",
        path.display()
    );
    println!(
        "Public verification key: {}",
        hex::encode(key.verifying_key().as_bytes())
    );
    Ok(())
}

fn sign(
    zip_path: &Path,
    minimum: &str,
    sequence: u64,
    output: &Path,
    key_path: &Path,
) -> Result<(), String> {
    let file_name = zip_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("Invalid release ZIP filename")?;
    let version = file_name
        .strip_prefix("WolfLink-")
        .and_then(|value| value.strip_suffix(".zip"))
        .ok_or("Expected WolfLink-VERSION.zip")?;
    let parsed = Version::parse(version).map_err(|_| "Invalid ZIP version")?;
    let minimum = Version::parse(minimum).map_err(|_| "Invalid minimum version")?;
    if minimum > parsed || sequence == 0 {
        return Err("Invalid minimum version or sequence".into());
    }
    validate_archive(zip_path)?;
    let mut file = File::open(zip_path).map_err(|error| error.to_string())?;
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        size += count as u64;
        hash.update(&buffer[..count]);
    }
    let payload = ManifestPayload {
        schema: 1,
        application_id: APP_ID.into(),
        version: version.into(),
        channel: Channel::for_version(&parsed),
        artifact: file_name.into(),
        sha256: hex::encode(hash.finalize()),
        size,
        minimum_supported_version: minimum.to_string(),
        sequence,
    };
    let key = read_key(key_path)?;
    let manifest = SignedManifest::sign(payload, &key)?;
    manifest.verify(&key.verifying_key())?;
    let encoded = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|error| format!("Refusing to overwrite existing manifest: {error}"))?;
    file.write_all(&encoded)
        .map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    println!("Signed update manifest: {}", output.display());
    Ok(())
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let key = key_path()?;
    match args.as_slice() {
        [_, command] if command == "init" => init(&key),
        [_, command, zip, minimum, sequence, output] if command == "sign" => {
            let sequence = sequence.parse().map_err(|_| "Invalid sequence")?;
            sign(Path::new(zip), minimum, sequence, Path::new(output), &key)
        }
        _ => Err("Usage: publisher init | publisher sign ZIP MIN_VERSION SEQUENCE OUTPUT".into()),
    }
}
