#![cfg_attr(windows, windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use wolflink_relay::invitation::{Invitation, RoomMode, unix_now};
use wolflink_relay::tailscale::{parse_local_status, status_json};

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
                let _ = events.send(Event::Line(first.trim().to_string()));
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
    let mut current: Option<Processes> = None;
    let mut last_tailnet = Instant::now() - Duration::from_secs(10);
    loop {
        if last_tailnet.elapsed() >= Duration::from_secs(5) {
            let result = status_json()
                .and_then(|json| parse_local_status(&json))
                .map(|device| format!("Connected ({})", device.address));
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
