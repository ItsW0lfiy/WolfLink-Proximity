use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub mod invitation;
pub mod tailscale;

pub const MAX_POSITION_AGE: Duration = Duration::from_millis(500);
pub const MAX_COORDINATE: f32 = 1_000_000.0;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Position {
    pub fn valid(self) -> bool {
        [self.x, self.y, self.z]
            .iter()
            .all(|value| value.is_finite() && value.abs() <= MAX_COORDINATE)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMessage {
    Hello {
        access_token: String,
        room_secret: String,
        game_session: String,
    },
    Position {
        position: Position,
    },
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Participant {
    pub discord_id: String,
    pub position: Position,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome { discord_id: String },
    Snapshot { participants: Vec<Participant> },
    Error { message: String },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RoomKey {
    secret_hash: [u8; 32],
    game_session: String,
}

impl RoomKey {
    pub fn new(secret: &str, game_session: &str) -> Option<Self> {
        if secret.len() < 16
            || secret.len() > 256
            || game_session.is_empty()
            || game_session.len() > 128
            || game_session.chars().any(char::is_control)
        {
            return None;
        }
        let mut secret_hash = [0; 32];
        secret_hash.copy_from_slice(&Sha256::digest(secret.as_bytes()));
        Some(Self {
            secret_hash,
            game_session: game_session.to_string(),
        })
    }
}

#[derive(Clone, Debug)]
struct Member {
    connection_id: u64,
    position: Option<(Position, Instant)>,
}

#[derive(Default)]
pub struct Rooms {
    members: HashMap<RoomKey, HashMap<String, Member>>,
}

impl Rooms {
    pub fn join(&mut self, room: RoomKey, discord_id: String, connection_id: u64) -> bool {
        let members = self.members.entry(room).or_default();
        if members.contains_key(&discord_id) {
            return false;
        }
        members.insert(
            discord_id,
            Member {
                connection_id,
                position: None,
            },
        );
        true
    }

    pub fn update(
        &mut self,
        room: &RoomKey,
        discord_id: &str,
        connection_id: u64,
        position: Option<Position>,
        now: Instant,
    ) -> bool {
        let Some(member) = self
            .members
            .get_mut(room)
            .and_then(|room| room.get_mut(discord_id))
        else {
            return false;
        };
        if member.connection_id != connection_id || position.is_some_and(|value| !value.valid()) {
            return false;
        }
        member.position = position.map(|value| (value, now));
        true
    }

    pub fn snapshot(&self, room: &RoomKey, recipient: &str, now: Instant) -> Vec<Participant> {
        let mut participants = self
            .members
            .get(room)
            .into_iter()
            .flat_map(|members| members.iter())
            .filter_map(|(discord_id, member)| {
                if discord_id == recipient {
                    return None;
                }
                let (position, observed_at) = member.position?;
                (now.saturating_duration_since(observed_at) <= MAX_POSITION_AGE).then(|| {
                    Participant {
                        discord_id: discord_id.clone(),
                        position,
                    }
                })
            })
            .collect::<Vec<_>>();
        participants.sort_by(|a, b| a.discord_id.cmp(&b.discord_id));
        participants
    }

    pub fn leave(&mut self, room: &RoomKey, discord_id: &str, connection_id: u64) {
        let Some(members) = self.members.get_mut(room) else {
            return;
        };
        if members.get(discord_id).map(|member| member.connection_id) == Some(connection_id) {
            members.remove(discord_id);
        }
        if members.is_empty() {
            self.members.remove(room);
        }
    }
}

pub fn volume_for_distance(local: Position, remote: Position, max_distance: f32) -> f32 {
    if !local.valid() || !remote.valid() || !max_distance.is_finite() || max_distance <= 0.0 {
        return 0.0;
    }
    let dx = (local.x - remote.x) as f64;
    let dy = (local.y - remote.y) as f64;
    let dz = (local.z - remote.z) as f64;
    let distance = (dx * dx + dy * dy + dz * dz).sqrt();
    let fraction = (1.0 - distance / f64::from(max_distance)).clamp(0.0, 1.0);
    (fraction * fraction * 100.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(x: f32) -> Position {
        Position { x, y: 0.0, z: 0.0 }
    }

    #[test]
    fn rooms_isolate_secrets_and_game_sessions() {
        let mut rooms = Rooms::default();
        let a = RoomKey::new("secret-abcdefghijkl", "server-a").unwrap();
        let b = RoomKey::new("secret-abcdefghijkl", "server-b").unwrap();
        let c = RoomKey::new("secret-qrstuvwxyz01", "server-a").unwrap();
        let now = Instant::now();
        assert!(rooms.join(a.clone(), "1".into(), 1));
        assert!(rooms.join(a.clone(), "2".into(), 2));
        assert!(rooms.join(b.clone(), "3".into(), 3));
        assert!(rooms.join(c.clone(), "4".into(), 4));
        assert!(rooms.update(&a, "2", 2, Some(position(10.0)), now));
        assert_eq!(rooms.snapshot(&a, "1", now).len(), 1);
        assert!(rooms.snapshot(&b, "3", now).is_empty());
        assert!(rooms.snapshot(&c, "4", now).is_empty());
    }

    #[test]
    fn stale_and_unavailable_telemetry_disappear() {
        let mut rooms = Rooms::default();
        let room = RoomKey::new("secret-abcdefghijkl", "server-a").unwrap();
        let now = Instant::now();
        assert!(rooms.join(room.clone(), "1".into(), 1));
        assert!(rooms.join(room.clone(), "2".into(), 2));
        assert!(rooms.update(&room, "2", 2, Some(position(10.0)), now));
        assert_eq!(rooms.snapshot(&room, "1", now).len(), 1);
        assert!(
            rooms
                .snapshot(&room, "1", now + Duration::from_millis(501))
                .is_empty()
        );
        assert!(rooms.update(&room, "2", 2, None, now));
        assert!(rooms.snapshot(&room, "1", now).is_empty());
    }

    #[test]
    fn rejects_duplicate_identity_and_invalid_position() {
        let mut rooms = Rooms::default();
        let room = RoomKey::new("secret-abcdefghijkl", "server-a").unwrap();
        let now = Instant::now();
        assert!(rooms.join(room.clone(), "1".into(), 1));
        assert!(!rooms.join(room.clone(), "1".into(), 2));
        assert!(!rooms.update(&room, "1", 2, Some(position(1.0)), now));
        assert!(!rooms.update(&room, "1", 1, Some(position(f32::NAN)), now));
    }

    #[test]
    fn distance_volume_fails_closed() {
        assert_eq!(
            volume_for_distance(position(0.0), position(0.0), 50.0),
            100.0
        );
        assert_eq!(
            volume_for_distance(position(0.0), position(25.0), 50.0),
            25.0
        );
        assert_eq!(
            volume_for_distance(position(0.0), position(50.0), 50.0),
            0.0
        );
        assert_eq!(
            volume_for_distance(position(f32::NAN), position(0.0), 50.0),
            0.0
        );
    }
}
