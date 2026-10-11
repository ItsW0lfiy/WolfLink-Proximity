use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, watch};
use tokio::time::{Duration, interval, sleep};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use wolflink_relay::invitation::{Invitation, RoomMode, unix_now};
use wolflink_relay::tailscale::{parse_local_status, status_json, verify_peer_status};
use wolflink_relay::{ClientMessage, Participant, Position, ServerMessage, volume_for_distance};

const LOCAL_MAX_AGE: Duration = Duration::from_millis(500);
const REMOTE_MAX_AGE: Duration = Duration::from_millis(500);
const MAX_DISTANCE: f32 = 60.0;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone)]
struct Sample {
    username: String,
    position: Position,
    received_at: Instant,
}

enum VoiceEvent {
    Auth(String),
    SelfId(u64),
    Peer(u64),
    Left(u64),
    Closed,
}

enum Control {
    Quit,
    Mute(bool),
    Loudness(u16),
}

struct Config {
    app_id: String,
    room_secret: String,
    game_session: String,
    relay_url: String,
    voice_exe: PathBuf,
    telemetry_exe: Option<PathBuf>,
    username: Option<String>,
    invitation: Option<Invitation>,
    loudness_percent: u16,
}

fn config() -> Result<Config, String> {
    let args: Vec<String> = env::args().collect();
    parse_config(&args)
}

fn parse_config(args: &[String]) -> Result<Config, String> {
    if args.len() == 2 && args[1] == "--managed" {
        return managed_config();
    }
    if args.len() != 7 {
        return Err("Usage: client <application_id> <room_secret> <simulated_session_key> <wss://relay> <voice_exe> --simulate".into());
    }
    if args[6] != "--simulate" {
        return Err("Roblox-derived proximity is disabled: the reader has no independently verified Roblox server-instance ID. Run the reader separately, or use --simulate for an explicitly simulated test room.".into());
    }
    args[1]
        .parse::<u64>()
        .map_err(|_| "invalid application ID")?;
    if args[2].len() < 16 || args[2].len() > 256 {
        return Err("room secret must be 16..256 characters".into());
    }
    if args[3].is_empty() || args[3].len() > 128 {
        return Err("invalid game-session key".into());
    }
    let relay_url = &args[4];
    if !relay_url.starts_with("wss://")
        && !relay_url.starts_with("ws://127.0.0.1:")
        && !relay_url.starts_with("ws://localhost:")
        && !relay_url.starts_with("ws://[::1]:")
    {
        return Err("relay must use wss://, or ws:// on loopback for local tests".into());
    }
    Ok(Config {
        app_id: args[1].clone(),
        room_secret: args[2].clone(),
        game_session: args[3].clone(),
        relay_url: relay_url.clone(),
        voice_exe: PathBuf::from(&args[5]),
        telemetry_exe: (args[6] != "--simulate").then(|| PathBuf::from(&args[6])),
        username: None,
        invitation: None,
        loudness_percent: 100,
    })
}

fn managed_config() -> Result<Config, String> {
    let encoded =
        env::var("WOLFLINK_ROOM_INVITE").map_err(|_| "Managed room invitation is missing")?;
    let invitation = Invitation::decode(&encoded, unix_now()?)?;
    let voice_exe =
        PathBuf::from(env::var_os("WOLFLINK_VOICE_EXE").ok_or("Voice bridge path is missing")?);
    let loudness_percent = env::var("WOLFLINK_LOUDNESS_PERCENT")
        .unwrap_or_else(|_| "100".into())
        .parse::<u16>()
        .map_err(|_| "Invalid loudness setting")?;
    if !(50..=150).contains(&loudness_percent) {
        return Err("Loudness must be 50..150 percent".into());
    }
    let (telemetry_exe, username) = match invitation.mode {
        RoomMode::Simulated => (None, None),
        RoomMode::ExperimentalRoblox => {
            if env::var("WOLFLINK_EXPERIMENTAL_ACK").ok().as_deref() != Some("1") {
                return Err(
                    "Unverified Roblox Server mode requires explicit acknowledgement".into(),
                );
            }
            let name = env::var("WOLFLINK_ROBLOX_USERNAME")
                .map_err(|_| "Your exact Roblox username is required in experimental mode")?;
            if name.is_empty()
                || name.len() > 64
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err("Invalid Roblox username".into());
            }
            let reader = PathBuf::from(
                env::var_os("WOLFLINK_READER_EXE").ok_or("Telemetry reader path is missing")?,
            );
            (Some(reader), Some(name))
        }
    };
    let relay_url = format!("ws://{}:{}", invitation.host_address, invitation.port);
    Ok(Config {
        app_id: invitation.application_id.to_string(),
        room_secret: invitation.room_secret.clone(),
        game_session: invitation.session_key.clone(),
        relay_url,
        voice_exe,
        telemetry_exe,
        username,
        invitation: Some(invitation),
        loudness_percent,
    })
}

fn verify_tailnet_target(invitation: &Invitation) -> Result<(), String> {
    invitation.validate(unix_now()?)?;
    let json = status_json()?;
    if parse_local_status(&json).is_ok_and(|local| {
        local.address == invitation.host_address && local.public_key == invitation.host_key
    }) {
        return Ok(());
    }
    verify_peer_status(&json, invitation.host_address, &invitation.host_key)
}

fn parse_reader_line(line: &str, now: Instant) -> Option<Sample> {
    if line == "UNAVAILABLE" {
        return None;
    }
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 5
        || fields[0] != "POS"
        || fields[1].is_empty()
        || !fields[1]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return None;
    }
    let position = Position {
        x: fields[2].parse().ok()?,
        y: fields[3].parse().ok()?,
        z: fields[4].parse().ok()?,
    };
    position.valid().then_some(Sample {
        username: fields[1].to_string(),
        position,
        received_at: now,
    })
}

async fn reader_task(path: PathBuf, username: String, samples: watch::Sender<Option<Sample>>) {
    let mut last_username = None::<String>;
    eprintln!("Roblox telemetry unavailable; discovering local player");
    loop {
        let child = Command::new(&path)
            .creation_flags(CREATE_NO_WINDOW)
            .args([
                "stream-telemetry",
                "--machine",
                "--hz",
                "20",
                "--username",
                &username,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        match child {
            Ok(mut child) => {
                if let Some(stdout) = child.stdout.take() {
                    let mut lines = BufReader::new(stdout).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let sample = parse_reader_line(&line, Instant::now())
                            .filter(|value| value.username == username);
                        if let Some(value) = &sample
                            && last_username.as_deref() != Some(&value.username)
                        {
                            eprintln!("Roblox player: {}", value.username);
                            last_username = Some(value.username.clone());
                        }
                        if sample.is_none() && last_username.take().is_some() {
                            eprintln!("Roblox telemetry unavailable");
                        }
                        let _ = samples.send(sample);
                    }
                }
                last_username = None;
                let _ = samples.send(None);
                let _ = child.wait().await;
            }
            Err(error) => {
                eprintln!("Could not start read-only telemetry reader: {error}");
                let _ = samples.send(None);
            }
        }
        sleep(Duration::from_secs(1)).await;
    }
}

async fn input_task(
    samples: Option<watch::Sender<Option<Sample>>>,
    controls: mpsc::Sender<Control>,
) {
    if samples.is_some() {
        eprintln!("Simulated telemetry: type 'position X Y Z', 'unavailable', or 'quit'.");
    }
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut position = None::<Position>;
    let mut ticks = interval(Duration::from_millis(50));
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break; };
                let line = line.trim();
                if line == "quit" { break; }
                if line == "mute on" || line == "mute off" {
                    let _ = controls.send(Control::Mute(line == "mute on")).await;
                    continue;
                }
                if let Some(value) = line.strip_prefix("loudness ") {
                    if let Ok(value) = value.parse::<u16>() && (50..=150).contains(&value) {
                        let _ = controls.send(Control::Loudness(value)).await;
                    } else {
                        eprintln!("Loudness must be 50..150 percent");
                    }
                    continue;
                }
                let Some(samples) = &samples else { eprintln!("Unknown control command"); continue; };
                if line.trim() == "unavailable" {
                    position = None;
                    let _ = samples.send(None);
                    continue;
                }
                if let Some(sample) = line.strip_prefix("position ")
                    .and_then(|coordinates| parse_reader_line(&format!("POS simulated {coordinates}"), Instant::now())) {
                    position = Some(sample.position);
                    let _ = samples.send(Some(sample));
                } else {
                    position = None;
                    let _ = samples.send(None);
                    eprintln!("Expected: position X Y Z | unavailable | quit");
                }
            }
            _ = ticks.tick() => {
                if let (Some(samples), Some(position)) = (&samples, position) {
                    let _ = samples.send(Some(Sample { username: "simulated".into(), position, received_at: Instant::now() }));
                }
            }
        }
    }
    if let Some(samples) = samples {
        let _ = samples.send(None);
    }
    let _ = controls.send(Control::Quit).await;
}

async fn apply_control(
    control: Control,
    voice_input: &mut ChildStdin,
    loudness: &AtomicU16,
) -> Result<bool, String> {
    match control {
        Control::Quit => Ok(true),
        Control::Mute(muted) => {
            voice_input
                .write_all(if muted {
                    b"self-mute on\n"
                } else {
                    b"self-mute off\n"
                })
                .await
                .map_err(|error| format!("Voice mute control failed: {error}"))?;
            Ok(false)
        }
        Control::Loudness(value) => {
            loudness.store(value, Ordering::Relaxed);
            Ok(false)
        }
    }
}

fn sample_is_fresh(sample: Option<Sample>, now: Instant) -> Option<Position> {
    sample.and_then(|value| {
        (now.saturating_duration_since(value.received_at) <= LOCAL_MAX_AGE)
            .then_some(value.position)
    })
}

async fn write_volume(
    voice_input: &mut ChildStdin,
    user_id: u64,
    volume: f32,
) -> Result<(), String> {
    voice_input
        .write_all(format!("volume {user_id} {volume:.1}\n").as_bytes())
        .await
        .map_err(|error| format!("Voice bridge input failed: {error}"))
}

fn volume_needs_update(previous: Option<f32>, target: f32) -> bool {
    previous.is_none_or(|previous| {
        (target == 0.0 && previous != 0.0) || (previous - target).abs() >= 0.5
    })
}

fn scaled_volume(gain: f32, loudness_percent: u16) -> f32 {
    (gain * f32::from(loudness_percent) / 100.0).clamp(0.0, 120.0)
}

struct GainState {
    value: f32,
    last_sent: Option<f32>,
    updated_at: Instant,
    positions: Option<(Position, Position)>,
}

impl GainState {
    fn muted(now: Instant) -> Self {
        Self {
            value: 0.0,
            last_sent: None,
            updated_at: now,
            positions: None,
        }
    }

    fn advance(&mut self, positions: Option<(Position, Position)>, now: Instant) -> f32 {
        let elapsed = now
            .saturating_duration_since(self.updated_at)
            .min(Duration::from_millis(100));
        self.updated_at = now;
        let Some((local, remote)) =
            positions.filter(|(local, remote)| local.valid() && remote.valid())
        else {
            self.value = 0.0;
            self.positions = None;
            return 0.0;
        };
        let target = volume_for_distance(local, remote, MAX_DISTANCE);
        let moved_far = self.positions.is_some_and(|(last_local, last_remote)| {
            let moved = |a: Position, b: Position| {
                let dx = f64::from(a.x) - f64::from(b.x);
                let dy = f64::from(a.y) - f64::from(b.y);
                let dz = f64::from(a.z) - f64::from(b.z);
                dx * dx + dy * dy + dz * dz > 20.0 * 20.0
            };
            moved(last_local, local) || moved(last_remote, remote)
        });
        self.positions = Some((local, remote));
        if target == 0.0 || (moved_far && target < self.value) {
            self.value = target;
        } else {
            let response = if target < self.value { 0.10 } else { 0.18 };
            let alpha = 1.0 - (-elapsed.as_secs_f32() / response).exp();
            self.value += (target - self.value) * alpha;
            if (target - self.value).abs() < 0.05 {
                self.value = target;
            }
        }
        self.value
    }
}

async fn apply_volumes(
    voice_input: &mut ChildStdin,
    peers: &HashSet<u64>,
    gains: &mut HashMap<u64, GainState>,
    local: Option<Position>,
    remote: &[Participant],
    remote_fresh: bool,
    loudness_percent: u16,
) -> Result<(), String> {
    let now = Instant::now();
    for peer in peers {
        let positions = if remote_fresh {
            local.and_then(|local| {
                remote
                    .iter()
                    .find(|person| person.discord_id == peer.to_string())
                    .map(|person| (local, person.position))
            })
        } else {
            None
        };
        let gain = gains.entry(*peer).or_insert_with(|| GainState::muted(now));
        let target = scaled_volume(gain.advance(positions, now), loudness_percent);
        if volume_needs_update(gain.last_sent, target) {
            write_volume(voice_input, *peer, target).await?;
            gain.last_sent = Some(target);
        }
    }
    Ok(())
}

async fn run_connected(
    config: &Config,
    token: &str,
    samples: &watch::Receiver<Option<Sample>>,
    voice_events: &mut mpsc::Receiver<VoiceEvent>,
    voice_input: &mut ChildStdin,
    peers: &mut HashSet<u64>,
    self_id: &mut Option<u64>,
    controls: &mut mpsc::Receiver<Control>,
    loudness: &AtomicU16,
) -> Result<bool, String> {
    if let Some(invitation) = &config.invitation {
        verify_tailnet_target(invitation)?;
    }
    let (mut ws, _) = connect_async(&config.relay_url)
        .await
        .map_err(|error| format!("Relay connection failed: {error}"))?;
    let hello = ClientMessage::Hello {
        access_token: token.to_string(),
        room_secret: config.room_secret.clone(),
        game_session: config.game_session.clone(),
    };
    ws.send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .map_err(|error| format!("Relay hello failed: {error}"))?;

    let mut remote = Vec::<Participant>::new();
    let mut last_snapshot = None::<Instant>;
    let mut gains = HashMap::<u64, GainState>::new();
    let mut server_id = None::<u64>;
    let mut ticks = interval(Duration::from_millis(50));
    let mut tail_checks = interval(Duration::from_secs(5));
    let mut last_tail_verified = Instant::now();
    let mut tail_check = None::<tokio::task::JoinHandle<Result<(), String>>>;
    loop {
        tokio::select! {
            control = controls.recv() => {
                if apply_control(control.unwrap_or(Control::Quit), voice_input, loudness).await? {
                    return Ok(true);
                }
            }
            _ = tail_checks.tick(), if config.invitation.is_some() && tail_check.is_none() => {
                let invitation = config.invitation.as_ref().unwrap().clone();
                tail_check = Some(tokio::task::spawn_blocking(move || verify_tailnet_target(&invitation)));
            }
            result = async { tail_check.as_mut().unwrap().await }, if tail_check.is_some() => {
                tail_check = None;
                match result {
                    Ok(Ok(())) => last_tail_verified = Instant::now(),
                    other => {
                        for peer in peers.iter() { write_volume(voice_input, *peer, 0.0).await?; }
                        return Err(match other {
                            Ok(Err(error)) => error,
                            Err(error) => format!("Tailnet identity check failed: {error}"),
                            _ => unreachable!(),
                        });
                    }
                }
            }
            frame = ws.next() => {
                let Some(Ok(Message::Text(text))) = frame else {
                    return Err("Relay disconnected".into());
                };
                match serde_json::from_str::<ServerMessage>(&text)
                    .map_err(|_| "Invalid relay message")? {
                    ServerMessage::Welcome { discord_id } => {
                        let id = discord_id.parse::<u64>().map_err(|_| "Invalid relay user ID")?;
                        if self_id.is_some_and(|value| value != id) {
                            return Err("Voice and relay Discord identities differ".into());
                        }
                        server_id = Some(id);
                        eprintln!("Relay authenticated Discord user {id}");
                    }
                    ServerMessage::Snapshot { participants } => {
                        if server_id.is_none() { return Err("Relay snapshot before welcome".into()); }
                        remote = participants;
                        last_snapshot = Some(Instant::now());
                    }
                    ServerMessage::Error { message } => return Err(format!("Relay rejected connection: {message}")),
                }
            }
            event = voice_events.recv() => {
                match event {
                    Some(VoiceEvent::Peer(id)) => {
                        peers.insert(id);
                        write_volume(voice_input, id, 0.0).await?;
                        let gain = gains.entry(id).or_insert_with(|| GainState::muted(Instant::now()));
                        gain.value = 0.0;
                        gain.last_sent = Some(0.0);
                        gain.positions = None;
                    }
                    Some(VoiceEvent::Left(id)) => {
                        peers.remove(&id);
                        gains.remove(&id);
                    }
                    Some(VoiceEvent::SelfId(id)) => {
                        if server_id.is_some_and(|value| value != id) {
                            return Err("Voice and relay Discord identities differ".into());
                        }
                        *self_id = Some(id);
                    }
                    Some(VoiceEvent::Closed) | None => return Err("Voice bridge stopped".into()),
                    Some(VoiceEvent::Auth(_)) => return Err("Voice identity changed; restart required".into()),
                }
            }
            _ = ticks.tick() => {
                let now = Instant::now();
                let tail_fresh = config.invitation.is_none()
                    || now.saturating_duration_since(last_tail_verified) <= Duration::from_secs(7);
                let local = if tail_fresh { sample_is_fresh(samples.borrow().clone(), now) } else { None };
                let outbound = match local {
                    Some(position) => ClientMessage::Position { position },
                    None => ClientMessage::Unavailable,
                };
                ws.send(Message::Text(serde_json::to_string(&outbound).unwrap().into()))
                    .await.map_err(|error| format!("Relay send failed: {error}"))?;
                let remote_fresh = tail_fresh && server_id.is_some_and(|id| *self_id == Some(id))
                    && last_snapshot.is_some_and(|time| now.saturating_duration_since(time) <= REMOTE_MAX_AGE);
                apply_volumes(voice_input, peers, &mut gains, local, &remote, remote_fresh, loudness.load(Ordering::Relaxed)).await?;
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = config()?;
    let (samples_tx, samples_rx) = watch::channel(None::<Sample>);
    let (controls_tx, mut controls_rx) = mpsc::channel::<Control>(32);
    let loudness = AtomicU16::new(config.loudness_percent);
    let telemetry_task = config.telemetry_exe.as_ref().map(|path| {
        tokio::spawn(reader_task(
            path.clone(),
            config.username.clone().unwrap(),
            samples_tx.clone(),
        ))
    });
    let input_samples = if config.telemetry_exe.is_none() {
        Some(samples_tx)
    } else {
        None
    };
    let input_handle = tokio::spawn(input_task(input_samples, controls_tx));

    let mut voice = Command::new(&config.voice_exe)
        .creation_flags(CREATE_NO_WINDOW)
        .args([&config.app_id, "--machine"])
        .env("WOLFLINK_LOBBY_SECRET", &config.room_secret)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut voice_input = voice.stdin.take().ok_or("Voice bridge stdin unavailable")?;
    let voice_output = voice
        .stdout
        .take()
        .ok_or("Voice bridge stdout unavailable")?;
    let (events_tx, mut events_rx) = mpsc::channel::<VoiceEvent>(64);
    tokio::spawn(async move {
        let mut lines = BufReader::new(voice_output).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let event = if let Some(token) = line.strip_prefix("AUTH ") {
                Some(VoiceEvent::Auth(token.to_string()))
            } else if let Some(id) = line.strip_prefix("SELF ") {
                id.parse().ok().map(VoiceEvent::SelfId)
            } else if let Some(id) = line.strip_prefix("PEER ") {
                id.parse().ok().map(VoiceEvent::Peer)
            } else if let Some(id) = line.strip_prefix("LEFT ") {
                id.parse().ok().map(VoiceEvent::Left)
            } else if line.starts_with("SIGNAL ") {
                // Negotiation is not enabled yet; do not log encoded peer candidates.
                None
            } else {
                eprintln!("Voice: {line}");
                None
            };
            if let Some(event) = event
                && events_tx.send(event).await.is_err()
            {
                return;
            }
        }
        let _ = events_tx.send(VoiceEvent::Closed).await;
    });

    let mut token = None::<String>;
    let mut peers = HashSet::<u64>::new();
    let mut self_id = None::<u64>;
    loop {
        if token.is_none() {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                control = controls_rx.recv() => {
                    if apply_control(control.unwrap_or(Control::Quit), &mut voice_input, &loudness).await? { break; }
                }
                event = events_rx.recv() => {
                    match event {
                        Some(VoiceEvent::Auth(value)) => token = Some(value),
                        Some(VoiceEvent::SelfId(id)) => self_id = Some(id),
                        Some(VoiceEvent::Peer(id)) => { peers.insert(id); write_volume(&mut voice_input, id, 0.0).await?; }
                        Some(VoiceEvent::Left(id)) => { peers.remove(&id); }
                        Some(VoiceEvent::Closed) | None => return Err("Voice bridge stopped before authentication".into()),
                    }
                }
            }
        } else {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                result = run_connected(&config, token.as_deref().unwrap(), &samples_rx, &mut events_rx, &mut voice_input, &mut peers, &mut self_id, &mut controls_rx, &loudness) => {
                    if matches!(result, Ok(true)) { break; }
                    if let Err(error) = result { eprintln!("{error}"); }
                    for id in &peers { write_volume(&mut voice_input, *id, 0.0).await?; }
                    if config.invitation.as_ref().is_some_and(|invite|
                        invite.validate(unix_now().unwrap_or(u64::MAX)).is_err()) {
                        eprintln!("Room invitation expired; leave and create a new room");
                        break;
                    }
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
    input_handle.abort();
    if let Some(task) = telemetry_task {
        task.abort();
    }
    let _ = voice_input.write_all(b"quit\n").await;
    let _ = voice.wait().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_lines_require_valid_position() {
        let now = Instant::now();
        assert!(parse_reader_line("POS orwell213 1 2 3", now).is_some());
        assert!(parse_reader_line("UNAVAILABLE", now).is_none());
        assert!(parse_reader_line("POS orwell213 NaN 2 3", now).is_none());
        assert!(parse_reader_line("POS <unknown> 1 2 3", now).is_none());
        assert!(parse_reader_line("Local player: x", now).is_none());
    }

    #[test]
    fn stale_local_sample_is_muted() {
        let now = Instant::now();
        let sample = Sample {
            username: "test".into(),
            position: Position {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            },
            received_at: now - Duration::from_millis(501),
        };
        assert!(sample_is_fresh(Some(sample), now).is_none());
    }

    #[test]
    fn tiny_remaining_volume_is_still_muted() {
        assert!(volume_needs_update(Some(0.2), 0.0));
        assert!(!volume_needs_update(Some(0.0), 0.0));
    }

    #[test]
    fn valid_gain_is_smoothed_but_invalid_and_out_of_range_mute_immediately() {
        let now = Instant::now();
        let origin = Position {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        let far = Position { x: 60.0, ..origin };
        let mut gain = GainState::muted(now);
        assert_eq!(gain.advance(Some((origin, origin)), now), 0.0);
        let rising = gain.advance(Some((origin, origin)), now + Duration::from_millis(50));
        assert!(rising > 0.0 && rising < 100.0);
        assert_eq!(
            gain.advance(Some((origin, far)), now + Duration::from_millis(100)),
            0.0
        );
        let return_gain = gain.advance(Some((origin, origin)), now + Duration::from_millis(150));
        assert!(return_gain > 0.0 && return_gain < 100.0);
        assert_eq!(gain.advance(None, now + Duration::from_millis(200)), 0.0);
        let recovering = gain.advance(Some((origin, origin)), now + Duration::from_millis(250));
        assert!(recovering > 0.0 && recovering < 100.0);
    }

    #[test]
    fn large_position_jump_does_not_leave_old_gain_audible() {
        let now = Instant::now();
        let origin = Position {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        let remote = Position { x: 30.0, ..origin };
        let mut gain = GainState::muted(now);
        gain.advance(Some((origin, origin)), now);
        gain.advance(Some((origin, origin)), now + Duration::from_secs(1));
        assert_eq!(
            gain.advance(Some((origin, remote)), now + Duration::from_millis(1050)),
            25.0
        );
    }

    #[test]
    fn unverified_roblox_game_session_cannot_enable_real_proximity() {
        let mut args = [
            "client",
            "123",
            "secret-abcdefghijkl",
            "claimed-session",
            "ws://127.0.0.1:8765",
            "voice.exe",
            "reader.exe",
        ]
        .map(str::to_string);
        assert!(
            parse_config(&args)
                .err()
                .unwrap()
                .contains("server-instance ID")
        );
        args[6] = "--simulate".into();
        assert!(parse_config(&args).is_ok());
    }

    #[test]
    fn loudness_scales_gain_without_changing_range_or_unsafe_mute() {
        assert_eq!(scaled_volume(25.0, 50), 12.5);
        assert_eq!(scaled_volume(25.0, 150), 37.5);
        assert_eq!(scaled_volume(100.0, 150), 120.0);
        assert_eq!(scaled_volume(0.0, 150), 0.0);
    }
}
