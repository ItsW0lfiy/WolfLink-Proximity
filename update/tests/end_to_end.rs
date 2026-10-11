#![cfg(windows)]

use semver::Version;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wolflink_update::{
    Channel, CheckResult, MANIFEST_NAME, REQUIRED_FILES, Release, ReleaseAsset, UpdateClient,
    embedded_public_key,
};
use zip::write::SimpleFileOptions;

fn temporary_dir() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "wolflink-update-e2e-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    root
}

fn build_fixture(path: &Path, old: bool) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_wolflink.rs");
    let mut command = Command::new("rustc");
    command
        .arg(fixture)
        .args(["--edition", "2024", "-o"])
        .arg(path);
    if old {
        command.args(["--cfg", "build_v1"]);
    }
    assert!(command.status().unwrap().success());
}

fn wait_for(path: &Path) {
    let until = Instant::now() + Duration::from_secs(15);
    while !path.exists() && Instant::now() < until {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(path.exists(), "timed out waiting for {}", path.display());
}

#[test]
#[ignore = "requires owner-authorized DPAPI signing key; runs isolated signed install and localhost HTTP fixture"]
fn signed_http_download_and_controlled_windows_replacement() {
    let root = temporary_dir();
    let install = root.join("install");
    let state = root.join("state");
    fs::create_dir(&install).unwrap();
    fs::create_dir(&state).unwrap();
    let old = root.join("old.exe");
    let new = root.join("new.exe");
    build_fixture(&old, true);
    build_fixture(&new, false);
    fs::copy(&old, install.join("WolfLink.exe")).unwrap();
    fs::write(install.join("client.exe"), b"old client").unwrap();
    fs::write(install.join("settings.json"), b"preserve me").unwrap();

    let zip = root.join("WolfLink-0.1.0-alpha.3.zip");
    let mut archive = zip::ZipWriter::new(File::create(&zip).unwrap());
    for name in REQUIRED_FILES {
        archive
            .start_file(*name, SimpleFileOptions::default())
            .unwrap();
        if *name == "WolfLink.exe" {
            archive.write_all(&fs::read(&new).unwrap()).unwrap();
        } else if *name == "WolfLinkUpdater.exe" {
            archive
                .write_all(&fs::read(env!("CARGO_BIN_EXE_WolfLinkUpdater")).unwrap())
                .unwrap();
        } else if matches!(
            *name,
            "msvcp140.dll" | "vcruntime140.dll" | "vcruntime140_1.dll"
        ) {
            let system = PathBuf::from(std::env::var_os("WINDIR").unwrap())
                .join("System32")
                .join(name);
            archive.write_all(&fs::read(system).unwrap()).unwrap();
        } else {
            archive.write_all(b"fixture component").unwrap();
        }
    }
    archive.finish().unwrap();
    let signed_path = root.join(MANIFEST_NAME);
    assert!(
        Command::new(env!("CARGO_BIN_EXE_publisher"))
            .args([
                "sign",
                zip.to_str().unwrap(),
                "0.1.0-alpha.2",
                "3",
                signed_path.to_str().unwrap()
            ])
            .status()
            .unwrap()
            .success()
    );
    let signed_bytes = fs::read(&signed_path).unwrap();
    let signed: wolflink_update::SignedManifest = serde_json::from_slice(&signed_bytes).unwrap();
    signed.verify(&embedded_public_key().unwrap()).unwrap();
    let zip_bytes = fs::read(&zip).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let listing = serde_json::to_vec(&vec![Release {
        tag_name: "v0.1.0-alpha.3".into(),
        prerelease: true,
        draft: false,
        body: Some("Isolated local fixture".into()),
        assets: vec![
            ReleaseAsset {
                name: MANIFEST_NAME.into(),
                browser_download_url: format!("{base}/manifest"),
                size: signed_bytes.len() as u64,
            },
            ReleaseAsset {
                name: signed.payload.artifact.clone(),
                browser_download_url: format!("{base}/bundle"),
                size: zip_bytes.len() as u64,
            },
        ],
    }])
    .unwrap();
    let server = thread::spawn(move || {
        for body in [listing, signed_bytes, zip_bytes] {
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
    let client =
        UpdateClient::new(embedded_public_key().unwrap(), format!("{base}/releases")).unwrap();
    let offer = client
        .check(
            &Version::parse("0.1.0-alpha.2").unwrap(),
            Channel::Alpha,
            None,
        )
        .unwrap();
    let CheckResult::Available {
        manifest,
        artifact_url,
        ..
    } = offer
    else {
        panic!("fixture update not found")
    };
    let downloaded = root.join(&manifest.payload.artifact);
    // The fixture ZIP was created at the signed path; download into an independent file.
    let downloaded = root.join("download").join(downloaded.file_name().unwrap());
    fs::create_dir(downloaded.parent().unwrap()).unwrap();
    client
        .download(&artifact_url, &manifest, &downloaded)
        .unwrap();
    server.join().unwrap();
    assert_eq!(fs::read(&downloaded).unwrap(), fs::read(&zip).unwrap());

    let mut old_process = Command::new(install.join("WolfLink.exe"))
        .current_dir(&install)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&install.join("launched.txt"));
    let ready = root.join("ready");
    let mut updater = Command::new(env!("CARGO_BIN_EXE_WolfLinkUpdater"))
        .args([
            "--apply",
            install.to_str().unwrap(),
            downloaded.to_str().unwrap(),
            signed_path.to_str().unwrap(),
            &old_process.id().to_string(),
            ready.to_str().unwrap(),
        ])
        .env("WOLFLINK_TEST_UPDATE_ROOT", &state)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&ready);
    fs::write(install.join("close-v1"), b"close").unwrap();
    assert!(old_process.wait().unwrap().success());
    assert!(updater.wait().unwrap().success());
    assert_eq!(
        fs::read(install.join("WolfLink.exe")).unwrap(),
        fs::read(&new).unwrap()
    );
    assert_eq!(fs::read(install.join("launched.txt")).unwrap(), b"v2");
    assert_eq!(
        fs::read(install.join("settings.json")).unwrap(),
        b"preserve me"
    );
    assert!(!state.join("pending.json").exists());
    for _ in 0..100 {
        if !root.exists() || fs::remove_dir_all(&root).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("Temporary test bundle remained locked: {}", root.display());
}
