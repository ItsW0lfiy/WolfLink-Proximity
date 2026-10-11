use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, watch};
use tokio::time::{Duration, interval, sleep};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use wolflink_relay::{ClientMessage, Participant, Position, ServerMessage, volume_for_distance};

const LOCAL_MAX_AGE: Duration = Duration::from_millis(500);
const REMOTE_MAX_AGE: Duration = Duration::from_millis(500);
const MAX_DISTANCE: f32 = 60.0;

#[derive(Clone, Copy)]
struct Sample {
    position: Position,
    received_at: Instant,
}

enum VoiceEvent {
    Auth(String),
    SelfId(u64),
    Peer(u64),
    Closed,
}

struct Config {
    app_id: String,
    room_secret: String,
    game_session: String,
    relay_url: String,
    voice_exe: PathBuf,
    telemetry_exe: Option<PathBuf>,
}

fn config() -> Result<Config, String> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 7 {
        return Err("Usage: client <application_id> <room_secret> <game_session> <wss://relay> <voice_exe> <telemetry_exe|--simulate>".into());
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
    })
}

fn parse_reader_line(line: &str, now: Instant) -> Option<Sample> {
    if line == "UNAVAILABLE" {
        return None;
    }
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 4 || fields[0] != "POS" {
        return None;
    }
    let position = Position {
        x: fields[1].parse().ok()?,
        y: fields[2].parse().ok()?,
        z: fields[3].parse().ok()?,
    };
    position.valid().then_some(Sample {
        position,
        received_at: now,
    })
}

async fn reader_task(path: PathBuf, samples: watch::Sender<Option<Sample>>) {
    loop {
        let child = Command::new(&path)
            .args(["stream-telemetry", "--machine", "--hz", "20"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        match child {
            Ok(mut child) => {
                if let Some(stdout) = child.stdout.take() {
                    let mut lines = BufReader::new(stdout).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let _ = samples.send(parse_reader_line(&line, Instant::now()));
                    }
                }
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

async fn simulated_input(samples: watch::Sender<Option<Sample>>) {
    eprintln!("Simulated telemetry: type 'position X Y Z', 'unavailable', or 'quit'.");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut position = None::<Position>;
    let mut ticks = interval(Duration::from_millis(50));
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break; };
                if line.trim() == "quit" { break; }
                if line.trim() == "unavailable" {
                    position = None;
                    let _ = samples.send(None);
                    continue;
                }
                if let Some(sample) = parse_reader_line(&line.replacen("position", "POS", 1), Instant::now()) {
                    position = Some(sample.position);
                    let _ = samples.send(Some(sample));
                } else {
                    position = None;
                    let _ = samples.send(None);
                    eprintln!("Expected: position X Y Z | unavailable | quit");
                }
            }
            _ = ticks.tick() => {
                if let Some(position) = position {
                    let _ = samples.send(Some(Sample { position, received_at: Instant::now() }));
                }
            }
        }
    }
    let _ = samples.send(None);
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

async fn apply_volumes(
    voice_input: &mut ChildStdin,
    peers: &HashSet<u64>,
    last_volumes: &mut HashMap<u64, f32>,
    local: Option<Position>,
    remote: &[Participant],
    remote_fresh: bool,
) -> Result<(), String> {
    for peer in peers {
        let target = if remote_fresh {
            local
                .and_then(|local| {
                    remote
                        .iter()
                        .find(|person| person.discord_id == peer.to_string())
                        .map(|person| volume_for_distance(local, person.position, MAX_DISTANCE))
                })
                .unwrap_or(0.0)
        } else {
            0.0
        };
        if volume_needs_update(last_volumes.get(peer).copied(), target) {
            write_volume(voice_input, *peer, target).await?;
            last_volumes.insert(*peer, target);
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
) -> Result<(), String> {
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
    let mut last_volumes = HashMap::<u64, f32>::new();
    let mut server_id = None::<u64>;
    let mut ticks = interval(Duration::from_millis(50));
    loop {
        tokio::select! {
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
                        last_volumes.insert(id, 0.0);
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
                let local = sample_is_fresh(*samples.borrow(), now);
                let outbound = match local {
                    Some(position) => ClientMessage::Position { position },
                    None => ClientMessage::Unavailable,
                };
                ws.send(Message::Text(serde_json::to_string(&outbound).unwrap().into()))
                    .await.map_err(|error| format!("Relay send failed: {error}"))?;
                let remote_fresh = server_id.is_some_and(|id| *self_id == Some(id))
                    && last_snapshot.is_some_and(|time| now.saturating_duration_since(time) <= REMOTE_MAX_AGE);
                apply_volumes(voice_input, peers, &mut last_volumes, local, &remote, remote_fresh).await?;
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = config()?;
    let (samples_tx, samples_rx) = watch::channel(None::<Sample>);
    let telemetry_task = match &config.telemetry_exe {
        Some(path) => tokio::spawn(reader_task(path.clone(), samples_tx)),
        None => tokio::spawn(simulated_input(samples_tx)),
    };

    let mut voice = Command::new(&config.voice_exe)
        .args([&config.app_id, &config.room_secret, "--machine"])
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
                event = events_rx.recv() => {
                    match event {
                        Some(VoiceEvent::Auth(value)) => token = Some(value),
                        Some(VoiceEvent::SelfId(id)) => self_id = Some(id),
                        Some(VoiceEvent::Peer(id)) => { peers.insert(id); write_volume(&mut voice_input, id, 0.0).await?; }
                        Some(VoiceEvent::Closed) | None => return Err("Voice bridge stopped before authentication".into()),
                    }
                }
            }
        } else {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                result = run_connected(&config, token.as_deref().unwrap(), &samples_rx, &mut events_rx, &mut voice_input, &mut peers, &mut self_id) => {
                    if let Err(error) = result { eprintln!("{error}"); }
                    for id in &peers { write_volume(&mut voice_input, *id, 0.0).await?; }
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
    telemetry_task.abort();
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
        assert!(parse_reader_line("POS 1 2 3", now).is_some());
        assert!(parse_reader_line("UNAVAILABLE", now).is_none());
        assert!(parse_reader_line("POS NaN 2 3", now).is_none());
        assert!(parse_reader_line("Local player: x", now).is_none());
    }

    #[test]
    fn stale_local_sample_is_muted() {
        let now = Instant::now();
        let sample = Sample {
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
}
