use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::tailscale::{LocalDevice, is_tailnet_ipv4};

pub const INVITATION_LIFETIME_SECS: u64 = 24 * 60 * 60;
const MAX_CLOCK_SKEW_SECS: u64 = 5 * 60;
const PREFIX: &str = "wolflink://join/";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomMode {
    Simulated,
    ExperimentalRoblox,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Invitation {
    version: u8,
    pub application_id: u64,
    pub host_address: Ipv4Addr,
    pub host_key: String,
    pub port: u16,
    pub room_secret: String,
    pub session_key: String,
    pub expires_at: u64,
    pub mode: RoomMode,
}

pub fn unix_now() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "System clock is before the Unix epoch".into())
}

impl Invitation {
    pub fn create(
        local: &LocalDevice,
        application_id: u64,
        port: u16,
        mode: RoomMode,
        now: u64,
    ) -> Result<Self, String> {
        let mut secret = [0u8; 32];
        let mut session = [0u8; 16];
        getrandom::fill(&mut secret).map_err(|_| "Could not generate a secure room secret")?;
        getrandom::fill(&mut session).map_err(|_| "Could not generate a secure session key")?;
        let invitation = Self {
            version: 1,
            application_id,
            host_address: local.address,
            host_key: local.public_key.clone(),
            port,
            room_secret: URL_SAFE_NO_PAD.encode(secret),
            session_key: URL_SAFE_NO_PAD.encode(session),
            expires_at: now
                .checked_add(INVITATION_LIFETIME_SECS)
                .ok_or("Invalid invitation time")?,
            mode,
        };
        invitation.validate(now)?;
        Ok(invitation)
    }

    pub fn encode(&self) -> Result<String, String> {
        let json = serde_json::to_vec(self).map_err(|_| "Could not encode invitation")?;
        Ok(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(json)))
    }

    pub fn decode(text: &str, now: u64) -> Result<Self, String> {
        if text.len() > 4096 {
            return Err("Invitation is too large".into());
        }
        let encoded = text
            .trim()
            .strip_prefix(PREFIX)
            .ok_or("Invalid WolfLink invitation")?;
        let json = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| "Invalid invitation encoding")?;
        let invitation: Self =
            serde_json::from_slice(&json).map_err(|_| "Invalid invitation contents")?;
        invitation.validate(now)?;
        Ok(invitation)
    }

    pub fn validate(&self, now: u64) -> Result<(), String> {
        if self.version != 1
            || self.application_id == 0
            || self.port == 0
            || !is_tailnet_ipv4(self.host_address)
            || !self.host_key.starts_with("nodekey:")
            || self.host_key.len() > 256
            || self.expires_at <= now
            || self.expires_at - now > INVITATION_LIFETIME_SECS + MAX_CLOCK_SKEW_SECS
        {
            return Err("Invalid or expired invitation".into());
        }
        let secret = URL_SAFE_NO_PAD
            .decode(&self.room_secret)
            .map_err(|_| "Invalid room secret")?;
        let session = URL_SAFE_NO_PAD
            .decode(&self.session_key)
            .map_err(|_| "Invalid session key")?;
        if secret.len() != 32 || session.len() != 16 {
            return Err("Invalid room key lengths".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> LocalDevice {
        LocalDevice {
            address: Ipv4Addr::new(100, 75, 2, 3),
            public_key: "nodekey:host".into(),
        }
    }

    #[test]
    fn private_invite_round_trip_and_expiry() {
        let invite =
            Invitation::create(&local(), 1234, 8765, RoomMode::ExperimentalRoblox, 100).unwrap();
        let encoded = invite.encode().unwrap();
        let parsed = Invitation::decode(&encoded, 101).unwrap();
        assert_eq!(parsed.host_address, local().address);
        assert_eq!(parsed.mode, RoomMode::ExperimentalRoblox);
        assert_eq!(parsed.room_secret.len(), 43);
        assert!(!encoded.contains("oauth"));
        assert!(Invitation::decode(&encoded, 100 + INVITATION_LIFETIME_SECS).is_err());
    }

    #[test]
    fn rejects_non_tailnet_endpoint_and_malformed_invites() {
        let mut invite =
            Invitation::create(&local(), 1234, 8765, RoomMode::Simulated, 100).unwrap();
        invite.host_address = Ipv4Addr::new(8, 8, 8, 8);
        assert!(Invitation::decode(&invite.encode().unwrap(), 101).is_err());
        assert!(Invitation::decode("wolflink://join/!", 101).is_err());
        assert!(Invitation::decode(&"x".repeat(4097), 101).is_err());
    }
}
