#![cfg_attr(windows, windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
use wolflink_relay::invitation::{Invitation, RoomMode, unix_now};
use wolflink_relay::tailscale::{parse_local_status, status_json};
use wolflink_update::{
    Channel, CheckResult, UpdateClient, embedded_public_key, load_recorded_policy, record_policy,
};

const APPLICATION_ID: u64 = 1558817526497157150;
const ROOM_PORT: u16 = 8765;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Default, Deserialize, Serialize)]
struct Preferences {
    roblox_username: String,
    loudness: u16,
}

fn preferences_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(|path| PathBuf::from(path).join("WolfLink").join("settings.json"))
}

fn load_preferences() -> Preferences {
    preferences_path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or(Preferences {
            roblox_username: String::new(),
            loudness: 100,
        })
}

fn save_preferences(preferences: &Preferences) {
    let Some(path) = preferences_path() else {
        return;
    };
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    if let Ok(data) = serde_json::to_vec(preferences) {
        let _ = std::fs::write(path, data);
    }
}

enum Action {
    Create {
        mode: RoomMode,
        username: String,
        loudness: u16,
        acknowledged: bool,
    },
    Join {
        invitation: String,
        username: String,
        loudness: u16,
        acknowledged: bool,
    },
    Control(String),
    ApplyUpdate(CheckResult),
    Stop,
    Exit,
}

enum Event {
    Tailnet(Result<String, String>),
    Roblox(bool),
    Started {
        invitation: String,
        host: bool,
        mode: RoomMode,
    },
    Stopped,
    Line(String),
    Error(String),
    UpdateCurrent,
    UpdateAvailable(CheckResult),
    UpdateStatus(String),
    UpdateRestarting,
    UpdatePolicyBlocked(String),
}

fn update_policy_path() -> Result<PathBuf, String> {
    Ok(std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or("LOCALAPPDATA is unavailable")?
        .join("WolfLink")
        .join("updates")
        .join("policy.json"))
}

fn spawn_update_checks(events: Sender<Event>) {
    thread::spawn(move || {
        let key = match embedded_public_key() {
            Ok(key) => key,
            Err(error) => {
                let _ = events.send(Event::UpdateStatus(error));
                return;
            }
        };
        let client = match UpdateClient::official(key) {
            Ok(client) => client,
            Err(error) => {
                let _ = events.send(Event::UpdateStatus(error));
                return;
            }
        };
        let current = semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        let channel = Channel::for_version(&current);
        let policy_path = match update_policy_path() {
            Ok(path) => path,
            Err(error) => {
                let _ = events.send(Event::UpdatePolicyBlocked(error));
                return;
            }
        };
        let mut recorded = match load_recorded_policy(&policy_path, &key) {
            Ok(policy) => policy,
            Err(error) => {
                let _ = events.send(Event::UpdatePolicyBlocked(format!(
                    "Cannot verify recorded policy: {error}"
                )));
                return;
            }
        };
        if let Some(policy) = &recorded
            && let Ok(verified) = policy.verify(&key)
            && current < verified.minimum
        {
            let _ = events.send(Event::UpdatePolicyBlocked(format!(
                "Version {} or newer is required",
                verified.minimum
            )));
        }
        let mut failures = 0u32;
        loop {
            let _ = events.send(Event::UpdateStatus("Checking for updates…".into()));
            match client.check(&current, channel, recorded.as_ref()) {
                Ok(CheckResult::Current) => {
                    failures = 0;
                    let _ = events.send(Event::UpdateCurrent);
                }
                Ok(offer @ CheckResult::Available { .. }) => {
                    if let CheckResult::Available { signed, .. } = &offer {
                        match record_policy(&policy_path, signed, &key) {
                            Ok(()) => {
                                recorded = Some((**signed).clone());
                                failures = 0;
                                let _ = events.send(Event::UpdateAvailable(offer));
                            }
                            Err(error) => {
                                let _ = events.send(Event::UpdatePolicyBlocked(format!(
                                    "Cannot save verified policy: {error}"
                                )));
                                return;
                            }
                        }
                    }
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    let _ = events.send(Event::UpdateStatus(format!(
                        "Update check unavailable: {error}"
                    )));
                }
            }
            let delay = if failures == 0 {
                Duration::from_secs(6 * 60 * 60)
            } else {
                Duration::from_secs((5 * 60 * 2u64.pow(failures.min(4))).min(60 * 60))
            };
            thread::sleep(delay);
        }
    });
}

fn prepare_update(offer: CheckResult, events: &Sender<Event>) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let CheckResult::Available {
        manifest,
        signed,
        artifact_url,
        ..
    } = offer
    else {
        return Err("No verified update is available".into());
    };
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or("LOCALAPPDATA is unavailable")?
        .join("WolfLink")
        .join("updates");
    std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let session = root.join(format!(
        "download-{}-{}-{nonce}",
        manifest.version,
        std::process::id()
    ));
    std::fs::create_dir(&session).map_err(|error| error.to_string())?;
    let zip = session.join(&manifest.payload.artifact);
    let manifest_path = session.join(wolflink_update::MANIFEST_NAME);
    let client = UpdateClient::official(embedded_public_key()?)?;
    let _ = events.send(Event::UpdateStatus(format!(
        "Downloading WolfLink {}…",
        manifest.version
    )));
    client.download(&artifact_url, &manifest, &zip)?;
    let _ = events.send(Event::UpdateStatus("Verifying signed update…".into()));
    std::fs::write(
        &manifest_path,
        serde_json::to_vec(&signed).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let helper = session.join("WolfLinkUpdater.exe");
    let ready = session.join("ready");
    std::fs::copy(child_path("WolfLinkUpdater.exe")?, &helper)
        .map_err(|error| format!("Cannot stage update helper: {error}"))?;
    let install = std::env::current_exe()
        .map_err(|error| error.to_string())?
        .parent()
        .ok_or("Application install directory unavailable")?
        .to_path_buf();
    let mut helper_process = Command::new(helper)
        .creation_flags(CREATE_NO_WINDOW)
        .args([
            "--apply",
            install.to_str().ok_or("Invalid install path")?,
            zip.to_str().ok_or("Invalid update path")?,
            manifest_path.to_str().ok_or("Invalid manifest path")?,
            &std::process::id().to_string(),
            ready.to_str().ok_or("Invalid handoff path")?,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Could not start update helper: {error}"))?;
    for _ in 0..1200 {
        if let Some(status) = helper_process
            .try_wait()
            .map_err(|error| error.to_string())?
        {
            return Err(format!(
                "Update helper failed preflight ({status}); WolfLink remains open"
            ));
        }
        if ready.is_file() {
            let _ = events.send(Event::UpdateRestarting);
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = helper_process.kill();
    let _ = helper_process.wait();
    Err("Update helper preflight timed out; WolfLink remains open".into())
}

fn recover_interrupted_update() -> Result<bool, String> {
    use std::os::windows::process::CommandExt;
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or("LOCALAPPDATA is unavailable")?
        .join("WolfLink")
        .join("updates");
    let journal = root.join("pending.json");
    if !journal.exists() {
        return Ok(false);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&journal).map_err(|error| error.to_string())?)
            .map_err(|_| "Update recovery journal is malformed")?;
    let recorded_install = value
        .get("install")
        .and_then(|value| value.as_str())
        .ok_or("Update recovery journal lacks install identity")?;
    let this_install = std::env::current_exe()
        .map_err(|error| error.to_string())?
        .parent()
        .ok_or("Application install directory unavailable")?
        .to_path_buf();
    if std::fs::canonicalize(recorded_install).map_err(|error| error.to_string())?
        != std::fs::canonicalize(this_install).map_err(|error| error.to_string())?
    {
        return Ok(false);
    }
    let pid = value
        .get("helper_pid")
        .and_then(|value| value.as_u64())
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or("Update recovery journal lacks helper identity")?;
    let handle = unsafe { OpenProcess(0x0010_0000, 0, pid) };
    if !handle.is_null() {
        let running = unsafe { WaitForSingleObject(handle, 0) == WAIT_TIMEOUT };
        unsafe { CloseHandle(handle) };
        if running {
            return Ok(false);
        }
    }
    Command::new(child_path("WolfLinkUpdater.exe")?)
        .creation_flags(CREATE_NO_WINDOW)
        .args(["--recover", &std::process::id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Cannot start update recovery: {error}"))?;
    Ok(true)
}

fn show_previous_update_error() {
    let Some(local) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let path = PathBuf::from(local)
        .join("WolfLink")
        .join("updates")
        .join("last-error.txt");
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    let _ = std::fs::remove_file(&path);
    let error = String::from_utf8_lossy(&bytes);
    let message: Vec<u16> = format!(
        "WolfLink update could not finish. The previous version remains available.\n\n{}\0",
        error.chars().take(500).collect::<String>()
    )
    .encode_utf16()
    .collect();
    let title: Vec<u16> = "WolfLink Update Error\0".encode_utf16().collect();
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            0x10,
        );
    }
}

fn roblox_running() -> bool {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut entry = PROCESSENTRY32W::default();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let length = entry
                    .szExeFile
                    .iter()
                    .position(|character| *character == 0)
                    .unwrap_or(entry.szExeFile.len());
                if String::from_utf16_lossy(&entry.szExeFile[..length])
                    .eq_ignore_ascii_case("RobloxPlayerBeta.exe")
                {
                    found = true;
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        found
    }
}

struct Processes {
    _job: Job,
    relay: Option<Child>,
    client: Child,
    input: ChildStdin,
}

struct Job(HANDLE);

impl Job {
    fn new() -> Result<Self, String> {
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err("Could not create Windows process job".into());
            }
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            ) == 0
            {
                CloseHandle(handle);
                return Err("Could not configure child-process cleanup".into());
            }
            Ok(Self(handle))
        }
    }

    fn assign(&self, child: &Child) -> Result<(), String> {
        if unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) } == 0 {
            Err("Windows prevented safe child-process supervision".into())
        } else {
            Ok(())
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

impl Processes {
    fn stop(&mut self) {
        let _ = self.input.write_all(b"quit\n");
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if self.client.try_wait().ok().flatten().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        if self.client.try_wait().ok().flatten().is_none() {
            let _ = self.client.kill();
        }
        let _ = self.client.wait();
        if let Some(mut relay) = self.relay.take() {
            let _ = relay.kill();
            let _ = relay.wait();
        }
    }
}

fn child_path(name: &str) -> Result<PathBuf, String> {
    let path = std::env::current_exe()
        .map_err(|error| format!("Application location unavailable: {error}"))?
        .with_file_name(name);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("Missing bundled component: {name}"))
    }
}

fn pipe_lines<R: std::io::Read + Send + 'static>(
    reader: R,
    events: Sender<Event>,
    prefix: &'static str,
) {
    thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            if line.starts_with("AUTH ") || line.contains("WOLFLINK_ROOM_INVITE") {
                continue;
            }
            let _ = events.send(Event::Line(format!(
                "{prefix}: {}",
                line.chars().take(300).collect::<String>()
            )));
        }
    });
}

fn start_room(
    invitation: Invitation,
    encoded: String,
    host: bool,
    username: &str,
    loudness: u16,
    acknowledged: bool,
    events: &Sender<Event>,
) -> Result<Processes, String> {
    if invitation.mode == RoomMode::ExperimentalRoblox {
        if !acknowledged {
            return Err("Acknowledge Unverified Roblox Server before joining".into());
        }
        if username.is_empty()
            || username.len() > 64
            || !username
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            return Err("Enter your exact Roblox username".into());
        }
    }
    if !(50..=150).contains(&loudness) {
        return Err("Loudness must be 50–150%".into());
    }
    let job = Job::new()?;
    let mut relay = None;
    if host {
        let mut child = Command::new(child_path("server.exe")?)
            .creation_flags(CREATE_NO_WINDOW)
            .env(
                "WOLFLINK_DISCORD_APP_ID",
                invitation.application_id.to_string(),
            )
            .env("WOLFLINK_ROOM_INVITE", &encoded)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("Could not start relay: {error}"))?;
        if let Err(error) = job.assign(&child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        let stdout = child.stdout.take().ok_or("Relay output unavailable")?;
        let (first_tx, first_rx) = mpsc::channel();
        let remaining_events = events.clone();
        thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);
            let mut first = String::new();
            let result = stdout
                .read_line(&mut first)
                .map(|count| if count > 0 { first } else { String::new() });
            let _ = first_tx.send(result);
            pipe_lines(stdout, remaining_events, "Relay");
        });
        if let Some(stderr) = child.stderr.take() {
            pipe_lines(stderr, events.clone(), "Relay error");
        }
        match first_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(first)) if first.starts_with("WolfLink relay listening on ") => {
                let _ = events.send(Event::Line(
                    "Relay listening on verified Tailnet interface".into(),
                ));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Relay did not bind to the Tailscale interface within 10 seconds. Check Tailscale and port availability.".into());
            }
        }
        relay = Some(child);
    }
    let result = Command::new(child_path("client.exe")?)
        .creation_flags(CREATE_NO_WINDOW)
        .arg("--managed")
        .env("WOLFLINK_ROOM_INVITE", &encoded)
        .env("WOLFLINK_VOICE_EXE", child_path("wolflink_voice_poc.exe")?)
        .env(
            "WOLFLINK_READER_EXE",
            child_path("roblox-memory-scanner.exe")?,
        )
        .env(
            "WOLFLINK_EXPERIMENTAL_ACK",
            if acknowledged { "1" } else { "0" },
        )
        .env("WOLFLINK_ROBLOX_USERNAME", username)
        .env("WOLFLINK_LOUDNESS_PERCENT", loudness.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut client = match result {
        Ok(client) => client,
        Err(error) => {
            if let Some(mut relay) = relay {
                let _ = relay.kill();
                let _ = relay.wait();
            }
            return Err(format!("Could not start voice client: {error}"));
        }
    };
    if let Err(error) = job.assign(&client) {
        let _ = client.kill();
        let _ = client.wait();
        if let Some(mut relay) = relay {
            let _ = relay.kill();
            let _ = relay.wait();
        }
        return Err(error);
    }
    let input = client
        .stdin
        .take()
        .ok_or("Client control pipe unavailable")?;
    if let Some(stderr) = client.stderr.take() {
        pipe_lines(stderr, events.clone(), "WolfLink");
    }
    Ok(Processes {
        _job: job,
        relay,
        client,
        input,
    })
}

fn worker(actions: Receiver<Action>, events: Sender<Event>) {
    spawn_update_checks(events.clone());
    let mut current: Option<Processes> = None;
    let mut last_tailnet = Instant::now() - Duration::from_secs(10);
    loop {
        if last_tailnet.elapsed() >= Duration::from_secs(5) {
            let result = status_json()
                .and_then(|json| parse_local_status(&json))
                .map(|_| "Connected".to_string());
            let _ = events.send(Event::Tailnet(result));
            let _ = events.send(Event::Roblox(roblox_running()));
            last_tailnet = Instant::now();
        }
        if let Some(processes) = &mut current {
            let client_exit = processes.client.try_wait().ok().flatten().is_some();
            let relay_exit = processes
                .relay
                .as_mut()
                .is_some_and(|relay| relay.try_wait().ok().flatten().is_some());
            if client_exit || relay_exit {
                let _ = events.send(Event::Error(
                    if relay_exit {
                        "Host relay stopped"
                    } else {
                        "Voice client stopped"
                    }
                    .into(),
                ));
                processes.stop();
                current = None;
                let _ = events.send(Event::Stopped);
            }
        }
        match actions.recv_timeout(Duration::from_millis(200)) {
            Ok(Action::Create {
                mode,
                username,
                loudness,
                acknowledged,
            }) => {
                if let Some(mut processes) = current.take() {
                    processes.stop();
                }
                let result = status_json()
                    .and_then(|json| parse_local_status(&json))
                    .and_then(|local| {
                        Invitation::create(&local, APPLICATION_ID, ROOM_PORT, mode, unix_now()?)
                    })
                    .and_then(|invite| Ok((invite.encode()?, invite)));
                match result.and_then(|(encoded, invite)| {
                    start_room(
                        invite,
                        encoded.clone(),
                        true,
                        &username,
                        loudness,
                        acknowledged,
                        &events,
                    )
                    .map(|processes| (encoded, processes))
                }) {
                    Ok((invitation, processes)) => {
                        current = Some(processes);
                        let _ = events.send(Event::Started {
                            invitation,
                            host: true,
                            mode,
                        });
                    }
                    Err(error) => {
                        let _ = events.send(Event::Error(error));
                        let _ = events.send(Event::Stopped);
                    }
                }
            }
            Ok(Action::Join {
                invitation,
                username,
                loudness,
                acknowledged,
            }) => {
                if let Some(mut processes) = current.take() {
                    processes.stop();
                }
                let result = Invitation::decode(&invitation, unix_now().unwrap_or(u64::MAX))
                    .and_then(|invite| {
                        let mode = invite.mode;
                        start_room(
                            invite,
                            invitation.clone(),
                            false,
                            &username,
                            loudness,
                            acknowledged,
                            &events,
                        )
                        .map(|processes| (mode, processes))
                    });
                match result {
                    Ok((mode, processes)) => {
                        current = Some(processes);
                        let _ = events.send(Event::Started {
                            invitation,
                            host: false,
                            mode,
                        });
                    }
                    Err(error) => {
                        let _ = events.send(Event::Error(error));
                        let _ = events.send(Event::Stopped);
                    }
                }
            }
            Ok(Action::Control(command)) => {
                if let Some(processes) = &mut current {
                    if processes
                        .input
                        .write_all(format!("{command}\n").as_bytes())
                        .is_err()
                    {
                        let _ =
                            events.send(Event::Error("Client control pipe disconnected".into()));
                    }
                }
            }
            Ok(Action::ApplyUpdate(offer)) => {
                if current.is_some() {
                    let _ = events.send(Event::UpdateStatus(
                        "Update deferred until the room is closed".into(),
                    ));
                } else {
                    let events = events.clone();
                    thread::spawn(move || {
                        if let Err(error) = prepare_update(offer, &events) {
                            let _ = events.send(Event::UpdateStatus(format!(
                                "Update failed safely: {error}"
                            )));
                        }
                    });
                }
            }
            Ok(Action::Stop) => {
                if let Some(mut processes) = current.take() {
                    processes.stop();
                }
                let _ = events.send(Event::Stopped);
            }
            Ok(Action::Exit) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(mut processes) = current.take() {
                    processes.stop();
                }
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

mod ui;

fn main() {
    match recover_interrupted_update() {
        Ok(true) => return,
        Err(error) => {
            let message: Vec<u16> = format!("WolfLink update recovery failed: {error}\0")
                .encode_utf16()
                .collect();
            let title: Vec<u16> = "WolfLink Update Error\0".encode_utf16().collect();
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
                    std::ptr::null_mut(),
                    message.as_ptr(),
                    title.as_ptr(),
                    0x10,
                );
            }
            return;
        }
        Ok(false) => {}
    }
    show_previous_update_error();
    ui::run();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_job_can_supervise_a_child() {
        let job = Job::new().unwrap();
        let mut child = Command::new("cmd.exe")
            .args(["/C", "exit", "0"])
            .spawn()
            .unwrap();
        let assigned = job.assign(&child);
        if assigned.is_err() {
            let _ = child.kill();
        }
        let _ = child.wait();
        assigned.unwrap();
    }

    #[test]
    #[ignore = "requires installed, connected Tailscale and a release server.exe"]
    fn tailnet_relay_binds_to_local_authorized_interface() {
        use std::net::{TcpListener, TcpStream};
        let local = parse_local_status(&status_json().unwrap()).unwrap();
        let port = TcpListener::bind((local.address, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let invitation = Invitation::create(
            &local,
            APPLICATION_ID,
            port,
            RoomMode::Simulated,
            unix_now().unwrap(),
        )
        .unwrap()
        .encode()
        .unwrap();
        let server = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("target")
            .join("release")
            .join("server.exe");
        let mut child = Command::new(server)
            .creation_flags(CREATE_NO_WINDOW)
            .env("WOLFLINK_DISCORD_APP_ID", APPLICATION_ID.to_string())
            .env("WOLFLINK_ROOM_INVITE", invitation)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let connected = loop {
            if TcpStream::connect_timeout(
                &std::net::SocketAddr::from((local.address, port)),
                Duration::from_millis(200),
            )
            .is_ok()
            {
                break true;
            }
            if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
                break false;
            }
            thread::sleep(Duration::from_millis(100));
        };
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            connected,
            "host relay did not accept local Tailnet TCP connections"
        );
    }
}
