use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use reqwest::blocking::Client;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use zip::ZipArchive;

pub const APP_ID: &str = "dev.wolflink.proximity";
pub const REPOSITORY: &str = "ItsW0lfiy/WolfLink-Proximity";
pub const MANIFEST_NAME: &str = "update-manifest.json";
pub const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_UNPACKED_BYTES: u64 = 384 * 1024 * 1024;
pub const REQUIRED_FILES: &[&str] = &[
    "WolfLink.exe",
    "WolfLinkUpdater.exe",
    "client.exe",
    "server.exe",
    "roblox-memory-scanner.exe",
    "wolflink_voice_poc.exe",
    "discord_partner_sdk.dll",
    "discord_krisp.dll",
    "msvcp140.dll",
    "vcruntime140.dll",
    "vcruntime140_1.dll",
    "License-Notices.txt",
];
pub const MANAGED_FILES: &[&str] = &[
    "WolfLink.exe",
    "WolfLinkUpdater.exe",
    "client.exe",
    "server.exe",
    "roblox-memory-scanner.exe",
    "wolflink_voice_poc.exe",
    "discord_krisp.dll",
    "discord_partner_sdk.dll",
    "krisp-bvc-o-pro-v3.kef",
    "krisp-nc-o-lite-v1.kef",
    "krisp-nc-o-med-v7.kef",
    "krisp-nc-o-nb-v2.kef",
    "krisp-vad-o-v2.kef",
    "msvcp140.dll",
    "vcruntime140.dll",
    "vcruntime140_1.dll",
    "License-Notices.txt",
    "FRIENDS-QUICKSTART.txt",
    "SHA256SUMS.txt",
];

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Alpha,
    Stable,
}

impl Channel {
    pub fn for_version(version: &Version) -> Self {
        if version.pre.is_empty() {
            Self::Stable
        } else {
            Self::Alpha
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestPayload {
    pub schema: u32,
    pub application_id: String,
    pub version: String,
    pub channel: Channel,
    pub artifact: String,
    pub sha256: String,
    pub size: u64,
    pub minimum_supported_version: String,
    pub sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedManifest {
    pub payload: ManifestPayload,
    pub signature: String,
}

fn signed_message(payload: &ManifestPayload) -> Result<Vec<u8>, String> {
    let mut bytes = b"WOLFLINK-UPDATE-MANIFEST-V1\0".to_vec();
    bytes.extend(serde_json::to_vec(payload).map_err(|error| error.to_string())?);
    Ok(bytes)
}

impl SignedManifest {
    pub fn sign(payload: ManifestPayload, key: &SigningKey) -> Result<Self, String> {
        use ed25519_dalek::Signer;
        let signature = key.sign(&signed_message(&payload)?);
        Ok(Self {
            payload,
            signature: STANDARD.encode(signature.to_bytes()),
        })
    }

    pub fn verify(&self, public_key: &VerifyingKey) -> Result<VerifiedManifest, String> {
        let signature_bytes = STANDARD
            .decode(&self.signature)
            .map_err(|_| "Invalid update signature encoding")?;
        let signature_bytes: [u8; 64] = signature_bytes
            .try_into()
            .map_err(|_| "Invalid update signature length")?;
        let signature = Signature::from_bytes(&signature_bytes);
        public_key
            .verify_strict(&signed_message(&self.payload)?, &signature)
            .map_err(|_| "Update signature is not trusted")?;
        let payload = &self.payload;
        if payload.schema != 1 || payload.application_id != APP_ID || payload.sequence == 0 {
            return Err("Update manifest has an unsupported identity or schema".into());
        }
        let version = Version::parse(&payload.version).map_err(|_| "Invalid update version")?;
        let minimum = Version::parse(&payload.minimum_supported_version)
            .map_err(|_| "Invalid minimum supported version")?;
        if minimum > version || payload.channel != Channel::for_version(&version) {
            return Err("Inconsistent update channel or minimum version".into());
        }
        if payload.artifact != format!("WolfLink-{}.zip", payload.version)
            || payload.artifact.contains(['/', '\\', ':'])
        {
            return Err("Unexpected update artifact name".into());
        }
        if payload.sha256.len() != 64
            || !payload.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            || payload.size == 0
            || payload.size > MAX_ARCHIVE_BYTES
        {
            return Err("Invalid update artifact digest or size".into());
        }
        Ok(VerifiedManifest {
            payload: payload.clone(),
            version,
            minimum,
        })
    }
}

#[derive(Clone, Debug)]
pub struct VerifiedManifest {
    pub payload: ManifestPayload,
    pub version: Version,
    pub minimum: Version,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Release {
    pub tag_name: String,
    pub prerelease: bool,
    pub draft: bool,
    pub body: Option<String>,
    pub assets: Vec<ReleaseAsset>,
}

pub fn select_release<'a>(
    releases: &'a [Release],
    current: &Version,
    channel: Channel,
) -> Option<(&'a Release, Version)> {
    releases
        .iter()
        .filter(|release| !release.draft)
        .filter_map(|release| {
            let version = Version::parse(release.tag_name.strip_prefix('v')?).ok()?;
            if version <= *current
                || release.prerelease != !version.pre.is_empty()
                || (channel == Channel::Stable && release.prerelease)
            {
                return None;
            }
            Some((release, version))
        })
        .max_by(|left, right| left.1.cmp(&right.1))
}

pub fn release_asset<'a>(release: &'a Release, name: &str) -> Result<&'a ReleaseAsset, String> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .ok_or_else(|| format!("Release is missing {name}"))
}

pub fn official_asset_url(release: &Release, asset: &ReleaseAsset) -> Result<(), String> {
    let expected = format!(
        "https://github.com/{REPOSITORY}/releases/download/{}/{}",
        release.tag_name, asset.name
    );
    if asset.browser_download_url != expected {
        return Err("Release asset URL is outside the official repository".into());
    }
    Ok(())
}

pub struct UpdateClient {
    http: Client,
    public_key: VerifyingKey,
    api_url: String,
}

#[derive(Clone)]
pub enum CheckResult {
    Current,
    Available {
        manifest: Box<VerifiedManifest>,
        signed: Box<SignedManifest>,
        artifact_url: String,
        notes: String,
        mandatory: bool,
    },
}

impl UpdateClient {
    pub fn official(public_key: VerifyingKey) -> Result<Self, String> {
        Self::new(
            public_key,
            format!("https://api.github.com/repos/{REPOSITORY}/releases?per_page=100"),
        )
    }

    pub fn new(public_key: VerifyingKey, api_url: String) -> Result<Self, String> {
        let http = Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("WolfLink-Proximity-Updater")
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            http,
            public_key,
            api_url,
        })
    }

    pub fn check(
        &self,
        current: &Version,
        channel: Channel,
        recorded: Option<&SignedManifest>,
    ) -> Result<CheckResult, String> {
        let releases: Vec<Release> = self
            .http
            .get(&self.api_url)
            .send()
            .and_then(|response| response.error_for_status())
            .map_err(|error| format!("Update listing unavailable: {error}"))?
            .json()
            .map_err(|error| format!("Invalid release listing: {error}"))?;
        let Some((release, version)) = select_release(&releases, current, channel) else {
            return Ok(CheckResult::Current);
        };
        let manifest_asset = release_asset(release, MANIFEST_NAME)?;
        let manifest_url = &manifest_asset.browser_download_url;
        if self.api_url.starts_with("https://api.github.com/") {
            official_asset_url(release, manifest_asset)?;
        }
        let bytes = self
            .http
            .get(manifest_url)
            .send()
            .and_then(|response| response.error_for_status())
            .map_err(|error| format!("Update manifest unavailable: {error}"))?
            .bytes()
            .map_err(|error| error.to_string())?;
        if bytes.len() > 16 * 1024 {
            return Err("Update manifest exceeds size limit".into());
        }
        let signed: SignedManifest =
            serde_json::from_slice(&bytes).map_err(|_| "Malformed signed update manifest")?;
        let manifest = signed.verify(&self.public_key)?;
        if let Some(previous) = recorded {
            let trusted_previous = previous.verify(&self.public_key)?;
            if manifest.payload.sequence < trusted_previous.payload.sequence
                || (manifest.payload.sequence == trusted_previous.payload.sequence
                    && manifest.payload != trusted_previous.payload)
                || manifest.version < trusted_previous.version
            {
                return Err("Release metadata is older than the last trusted update policy".into());
            }
        }
        if manifest.version != version {
            return Err("Signed version does not match release tag".into());
        }
        let artifact = release_asset(release, &manifest.payload.artifact)?;
        if artifact.size != manifest.payload.size {
            return Err("Signed artifact size does not match release".into());
        }
        if self.api_url.starts_with("https://api.github.com/") {
            official_asset_url(release, artifact)?;
        }
        Ok(CheckResult::Available {
            mandatory: current < &manifest.minimum,
            manifest: Box::new(manifest),
            signed: Box::new(signed),
            artifact_url: artifact.browser_download_url.clone(),
            notes: release
                .body
                .clone()
                .unwrap_or_default()
                .chars()
                .take(3000)
                .collect(),
        })
    }

    pub fn download(
        &self,
        url: &str,
        manifest: &VerifiedManifest,
        destination: &Path,
    ) -> Result<(), String> {
        let partial = destination.with_extension("zip.part");
        let result = (|| {
            let mut response = self
                .http
                .get(url)
                .send()
                .and_then(|response| response.error_for_status())
                .map_err(|error| format!("Update download failed: {error}"))?;
            let mut file = File::create(&partial).map_err(|error| error.to_string())?;
            let mut digest = Sha256::new();
            let mut received = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let count = response
                    .read(&mut buffer)
                    .map_err(|error| error.to_string())?;
                if count == 0 {
                    break;
                }
                received += count as u64;
                if received > manifest.payload.size {
                    return Err("Update download exceeded signed size".into());
                }
                digest.update(&buffer[..count]);
                file.write_all(&buffer[..count])
                    .map_err(|error| error.to_string())?;
            }
            file.sync_all().map_err(|error| error.to_string())?;
            if received != manifest.payload.size
                || hex::encode(digest.finalize()) != manifest.payload.sha256.to_lowercase()
            {
                return Err("Update size or SHA-256 does not match signed manifest".into());
            }
            validate_archive(&partial)?;
            fs::rename(&partial, destination).map_err(|error| error.to_string())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&partial);
        }
        result
    }
}

pub fn load_recorded_policy(
    path: &Path,
    public_key: &VerifyingKey,
) -> Result<Option<SignedManifest>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Cannot read recorded update policy: {error}")),
    };
    if bytes.len() > 16 * 1024 {
        return Err("Recorded update policy exceeds size limit".into());
    }
    let signed: SignedManifest =
        serde_json::from_slice(&bytes).map_err(|_| "Recorded update policy is malformed")?;
    signed.verify(public_key)?;
    Ok(Some(signed))
}

pub fn record_policy(
    path: &Path,
    signed: &SignedManifest,
    public_key: &VerifyingKey,
) -> Result<(), String> {
    let verified = signed.verify(public_key)?;
    if let Some(previous) = load_recorded_policy(path, public_key)? {
        let previous = previous.verify(public_key)?;
        if verified.payload.sequence < previous.payload.sequence
            || (verified.payload.sequence == previous.payload.sequence
                && verified.payload != previous.payload)
            || verified.version < previous.version
        {
            return Err("Refusing update policy rollback".into());
        }
    }
    let parent = path.parent().ok_or("Update policy path has no parent")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec(signed).map_err(|error| error.to_string())?;
    let pending = path.with_extension("json.pending");
    let result = (|| {
        let mut file = File::create(&pending).map_err(|error| error.to_string())?;
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
            if path.exists() {
                let wide = |value: &std::ffi::OsStr| {
                    value
                        .encode_wide()
                        .chain(std::iter::once(0))
                        .collect::<Vec<_>>()
                };
                let target = wide(path.as_os_str());
                let replacement = wide(pending.as_os_str());
                if unsafe {
                    ReplaceFileW(
                        target.as_ptr(),
                        replacement.as_ptr(),
                        std::ptr::null(),
                        0,
                        std::ptr::null(),
                        std::ptr::null(),
                    )
                } == 0
                {
                    return Err(format!(
                        "Cannot replace update policy: {}",
                        std::io::Error::last_os_error()
                    ));
                }
            } else {
                fs::rename(&pending, path).map_err(|error| error.to_string())?;
            }
        }
        #[cfg(not(windows))]
        fs::rename(&pending, path).map_err(|error| error.to_string())?;
        Ok::<(), String>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&pending);
    }
    result
}

pub fn embedded_public_key() -> Result<VerifyingKey, String> {
    let value = include_str!("../trusted-key.hex").trim();
    if value == "UNCONFIGURED" {
        return Err("Updater signing trust is not configured in this build".into());
    }
    let bytes = hex::decode(value).map_err(|_| "Invalid embedded update public key")?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "Invalid embedded update public key length")?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| "Invalid embedded update public key".into())
}

pub fn validate_archive(path: &Path) -> Result<Vec<String>, String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    let mut archive = ZipArchive::new(file).map_err(|error| error.to_string())?;
    if archive.is_empty() || archive.len() > 64 {
        return Err("Update ZIP has an invalid number of files".into());
    }
    let mut names = Vec::new();
    let mut unique = HashSet::new();
    let mut total = 0u64;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let name = entry.name();
        let file_name = Path::new(name)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("Unsafe update ZIP path")?;
        if entry.is_dir()
            || file_name != name
            || name.contains(['/', '\\', ':'])
            || name == "."
            || name == ".."
            || !MANAGED_FILES.contains(&name)
            || entry
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
            || !unique.insert(name.to_ascii_lowercase())
        {
            return Err("Unsafe or duplicate update ZIP entry".into());
        }
        total = total.saturating_add(entry.size());
        if total > MAX_UNPACKED_BYTES {
            return Err("Update ZIP exceeds unpacked size limit".into());
        }
        names.push(name.to_string());
    }
    if REQUIRED_FILES
        .iter()
        .any(|required| !names.iter().any(|name| name == required))
    {
        return Err("Update ZIP lacks required application components".into());
    }
    Ok(names)
}

pub fn extract_archive(path: &Path, destination: &Path) -> Result<Vec<PathBuf>, String> {
    let names = validate_archive(path)?;
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    let file = File::open(path).map_err(|error| error.to_string())?;
    let mut archive = ZipArchive::new(file).map_err(|error| error.to_string())?;
    let mut extracted = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let source = archive.by_index(index).map_err(|error| error.to_string())?;
        let target = destination.join(name);
        let mut output = File::create(&target).map_err(|error| error.to_string())?;
        let expected = source.size();
        let size = std::io::copy(&mut source.take(expected + 1), &mut output)
            .map_err(|error| error.to_string())?;
        if size != expected {
            return Err("Update ZIP entry is truncated".into());
        }
        output.sync_all().map_err(|error| error.to_string())?;
        extracted.push(target);
    }
    Ok(extracted)
}

pub fn signing_key_from_random() -> Result<SigningKey, String> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|_| "Secure key generation failed")?;
    Ok(SigningKey::from_bytes(&seed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;

    fn temporary_dir() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "wolflink-update-test-{}-{}-{}",
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

    fn test_zip(path: &Path, extra: Option<&str>) {
        let file = File::create(path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        for name in REQUIRED_FILES.iter().copied().chain(extra) {
            archive
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            archive.write_all(b"fixture").unwrap();
        }
        archive.finish().unwrap();
    }

    fn signed_for_zip(key: &SigningKey, zip: &Path) -> SignedManifest {
        let bytes = fs::read(zip).unwrap();
        SignedManifest::sign(
            ManifestPayload {
                schema: 1,
                application_id: APP_ID.into(),
                version: "0.1.0-alpha.2".into(),
                channel: Channel::Alpha,
                artifact: "WolfLink-0.1.0-alpha.2.zip".into(),
                sha256: hex::encode(Sha256::digest(&bytes)),
                size: bytes.len() as u64,
                minimum_supported_version: "0.1.0-alpha.2".into(),
                sequence: 2,
            },
            key,
        )
        .unwrap()
    }

    fn test_manifest(key: &SigningKey) -> SignedManifest {
        SignedManifest::sign(
            ManifestPayload {
                schema: 1,
                application_id: APP_ID.into(),
                version: "0.1.0-alpha.2".into(),
                channel: Channel::Alpha,
                artifact: "WolfLink-0.1.0-alpha.2.zip".into(),
                sha256: "a".repeat(64),
                size: 100,
                minimum_supported_version: "0.1.0-alpha.2".into(),
                sequence: 2,
            },
            key,
        )
        .unwrap()
    }

    #[test]
    fn signature_binds_all_policy_and_artifact_fields() {
        let key = signing_key_from_random().unwrap();
        let signed = test_manifest(&key);
        let verified = signed.verify(&key.verifying_key()).unwrap();
        assert_eq!(verified.version, Version::parse("0.1.0-alpha.2").unwrap());
        let mut tampered = signed;
        tampered.payload.minimum_supported_version = "0.1.0-alpha.1".into();
        assert!(tampered.verify(&key.verifying_key()).is_err());
        tampered.signature = "not base64".into();
        assert!(tampered.verify(&key.verifying_key()).is_err());
    }

    #[test]
    fn release_selection_obeys_semver_and_channels() {
        let release = |tag: &str, prerelease| Release {
            tag_name: tag.into(),
            prerelease,
            draft: false,
            body: None,
            assets: vec![],
        };
        let releases = vec![
            release("v0.1.0-alpha.2", true),
            release("v0.1.0-alpha.10", true),
            release("v0.1.0", false),
            release("v0.2.0-alpha.1", true),
        ];
        let current = Version::parse("0.1.0-alpha.1").unwrap();
        assert_eq!(
            select_release(&releases, &current, Channel::Alpha)
                .unwrap()
                .1,
            Version::parse("0.2.0-alpha.1").unwrap()
        );
        assert_eq!(
            select_release(&releases, &current, Channel::Stable)
                .unwrap()
                .1,
            Version::parse("0.1.0").unwrap()
        );
        assert!(
            select_release(
                &releases,
                &Version::parse("0.2.0-alpha.1").unwrap(),
                Channel::Alpha
            )
            .is_none()
        );
    }

    #[test]
    fn archive_accepts_managed_bundle_and_rejects_unrelated_or_unsafe_entries() {
        let directory = temporary_dir();
        let valid = directory.join("valid.zip");
        test_zip(&valid, Some("krisp-vad-o-v2.kef"));
        assert!(validate_archive(&valid).is_ok());
        for unsafe_name in ["../WolfLink.exe", "settings.json", "a\\b.txt", "C:payload"] {
            let path = directory.join(format!("bad-{}.zip", unsafe_name.len()));
            test_zip(&path, Some(unsafe_name));
            assert!(validate_archive(&path).is_err(), "accepted {unsafe_name}");
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn recorded_policy_rejects_bad_signature_and_rollback() {
        let directory = temporary_dir();
        let path = directory.join("policy.json");
        let key = signing_key_from_random().unwrap();
        let newer = test_manifest(&key);
        record_policy(&path, &newer, &key.verifying_key()).unwrap();
        assert_eq!(
            load_recorded_policy(&path, &key.verifying_key())
                .unwrap()
                .unwrap()
                .payload
                .sequence,
            2
        );
        let mut older = newer.clone();
        older.payload.sequence = 1;
        older = SignedManifest::sign(older.payload, &key).unwrap();
        assert!(record_policy(&path, &older, &key.verifying_key()).is_err());
        fs::write(&path, b"corrupt").unwrap();
        assert!(load_recorded_policy(&path, &key.verifying_key()).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn download_rejects_corruption_and_interruption_without_publishing_partial_file() {
        use std::net::TcpListener;
        let directory = temporary_dir();
        let zip = directory.join("WolfLink-0.1.0-alpha.2.zip");
        test_zip(&zip, None);
        let bytes = fs::read(&zip).unwrap();
        let key = signing_key_from_random().unwrap();
        let signed = signed_for_zip(&key, &zip);
        let manifest = signed.verify(&key.verifying_key()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/bundle", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for (body, declared) in [
                (bytes.clone(), bytes.len()),
                (b"bad zip".to_vec(), 7usize),
                (bytes[..bytes.len() / 2].to_vec(), bytes.len()),
            ] {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request);
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                socket.write_all(&body).unwrap();
            }
        });
        let client =
            UpdateClient::new(key.verifying_key(), "http://127.0.0.1/unused".into()).unwrap();
        let destination = directory.join("download.zip");
        client.download(&url, &manifest, &destination).unwrap();
        assert!(destination.exists());
        fs::remove_file(&destination).unwrap();
        assert!(client.download(&url, &manifest, &destination).is_err());
        assert!(!destination.exists());
        assert!(client.download(&url, &manifest, &destination).is_err());
        assert!(!destination.exists());
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn release_check_requires_signed_manifest_and_matching_assets() {
        use std::net::TcpListener;
        let directory = temporary_dir();
        let zip = directory.join("WolfLink-0.1.0-alpha.2.zip");
        test_zip(&zip, None);
        let key = signing_key_from_random().unwrap();
        let signed = signed_for_zip(&key, &zip);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mut release = Release {
            tag_name: "v0.1.0-alpha.2".into(),
            prerelease: true,
            draft: false,
            body: Some("Alpha update".into()),
            assets: vec![
                ReleaseAsset {
                    name: MANIFEST_NAME.into(),
                    browser_download_url: format!("{base}/manifest"),
                    size: 1,
                },
                ReleaseAsset {
                    name: signed.payload.artifact.clone(),
                    browser_download_url: format!("{base}/bundle"),
                    size: signed.payload.size,
                },
            ],
        };
        let valid_listing = serde_json::to_vec(&vec![release.clone()]).unwrap();
        release.assets.pop();
        let missing_asset_listing = serde_json::to_vec(&vec![release]).unwrap();
        let valid_manifest = serde_json::to_vec(&signed).unwrap();
        let mut invalid = signed.clone();
        invalid.payload.sequence += 1;
        let invalid_manifest = serde_json::to_vec(&invalid).unwrap();
        let server = std::thread::spawn(move || {
            for body in [
                valid_listing.clone(),
                valid_manifest.clone(),
                missing_asset_listing,
                valid_manifest,
                valid_listing,
                invalid_manifest,
            ] {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request);
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                socket.write_all(&body).unwrap();
            }
        });
        let client = UpdateClient::new(key.verifying_key(), format!("{base}/releases")).unwrap();
        let current = Version::parse("0.1.0-alpha.1").unwrap();
        assert!(matches!(
            client.check(&current, Channel::Alpha, None).unwrap(),
            CheckResult::Available {
                mandatory: true,
                ..
            }
        ));
        assert!(client.check(&current, Channel::Alpha, None).is_err());
        assert!(client.check(&current, Channel::Alpha, None).is_err());
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }
}
