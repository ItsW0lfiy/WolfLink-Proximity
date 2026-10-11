use crate::{MAX_POSITION_AGE, Position};
use std::time::{Instant, SystemTime};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    ReadOnlyMemory,
    CooperatingExperience,
    Simulated,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Validity {
    Valid,
    Unavailable(String),
    Unsupported(String),
}

#[derive(Clone, Debug)]
pub struct Telemetry {
    pub source: Source,
    pub roblox_username: Option<String>,
    pub roblox_user_id: Option<u64>,
    pub position: Option<Position>,
    pub sample_timestamp: Option<SystemTime>,
    pub observed_at: Option<Instant>,
    pub validity: Validity,
    pub server_instance: Option<String>,
}

impl Telemetry {
    pub fn unavailable(source: Source, reason: impl Into<String>) -> Self {
        Self {
            source,
            roblox_username: None,
            roblox_user_id: None,
            position: None,
            sample_timestamp: None,
            observed_at: None,
            validity: Validity::Unavailable(reason.into()),
            server_instance: None,
        }
    }

    pub fn unsupported(source: Source, reason: impl Into<String>) -> Self {
        let mut result = Self::unavailable(source, reason);
        if let Validity::Unavailable(reason) = result.validity {
            result.validity = Validity::Unsupported(reason);
        }
        result
    }

    pub fn validated(
        source: Source,
        username: Option<String>,
        user_id: Option<u64>,
        position: Position,
        server_instance: Option<String>,
        observed_at: Instant,
    ) -> Self {
        if !position.valid()
            || (source != Source::Simulated
                && username.as_deref().is_none_or(|name| {
                    name.is_empty()
                        || name.len() > 64
                        || !name
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                }))
            || (source == Source::CooperatingExperience
                && (user_id.is_none() || server_instance.as_deref().is_none_or(str::is_empty)))
        {
            return Self::unavailable(source, "Invalid identity, session, or position");
        }
        Self {
            source,
            roblox_username: username,
            roblox_user_id: user_id,
            position: Some(position),
            sample_timestamp: Some(SystemTime::now()),
            observed_at: Some(observed_at),
            validity: Validity::Valid,
            server_instance,
        }
    }

    pub fn fresh_position(
        &self,
        now: Instant,
        expected_username: Option<&str>,
        allow_simulated: bool,
    ) -> Option<Position> {
        if self.validity != Validity::Valid
            || (self.source == Source::Simulated && !allow_simulated)
            || (self.source != Source::Simulated
                && expected_username
                    .is_none_or(|expected| self.roblox_username.as_deref() != Some(expected)))
            || self
                .observed_at
                .is_none_or(|time| now.saturating_duration_since(time) > MAX_POSITION_AGE)
        {
            return None;
        }
        self.position.filter(|position| position.valid())
    }
}

pub struct ProviderSet {
    pub memory: Telemetry,
    pub cooperative: Telemetry,
    pub simulated: Telemetry,
}

impl Default for ProviderSet {
    fn default() -> Self {
        Self {
            memory: Telemetry::unavailable(Source::ReadOnlyMemory, "Reader not connected"),
            cooperative: Telemetry::unsupported(
                Source::CooperatingExperience,
                "No developer-authorized HTTPS feed configured",
            ),
            simulated: Telemetry::unavailable(Source::Simulated, "No test position"),
        }
    }
}

impl ProviderSet {
    pub fn publish(&mut self, telemetry: Telemetry) {
        match telemetry.source {
            Source::ReadOnlyMemory => self.memory = telemetry,
            Source::CooperatingExperience => self.cooperative = telemetry,
            Source::Simulated => self.simulated = telemetry,
        }
    }

    pub fn position(
        &self,
        now: Instant,
        expected_username: Option<&str>,
        simulated_room: bool,
    ) -> Option<(Source, Position)> {
        if simulated_room {
            return self
                .simulated
                .fresh_position(now, None, true)
                .map(|position| (Source::Simulated, position));
        }
        for provider in [&self.cooperative, &self.memory] {
            if let Some(position) = provider.fresh_position(now, expected_username, false) {
                return Some((provider.source, position));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position() -> Position {
        Position {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        }
    }

    #[test]
    fn invalid_and_unsupported_sources_have_no_coordinates_or_timestamp() {
        for value in [
            Telemetry::unavailable(Source::ReadOnlyMemory, "Roblox exited"),
            Telemetry::unsupported(Source::CooperatingExperience, "No feed"),
            Telemetry::validated(
                Source::ReadOnlyMemory,
                Some("self".into()),
                None,
                Position {
                    x: f32::NAN,
                    y: 0.0,
                    z: 0.0,
                },
                None,
                Instant::now(),
            ),
        ] {
            assert!(value.position.is_none());
            assert!(value.sample_timestamp.is_none());
        }
    }

    #[test]
    fn restart_respawn_and_disconnection_clear_old_position() {
        let now = Instant::now();
        let mut providers = ProviderSet::default();
        providers.publish(Telemetry::validated(
            Source::ReadOnlyMemory,
            Some("self".into()),
            None,
            position(),
            None,
            now,
        ));
        assert!(providers.position(now, Some("self"), false).is_some());
        providers.publish(Telemetry::unavailable(
            Source::ReadOnlyMemory,
            "Process restarted",
        ));
        assert!(providers.position(now, Some("self"), false).is_none());
        providers.publish(Telemetry::validated(
            Source::ReadOnlyMemory,
            Some("self".into()),
            None,
            position(),
            None,
            now,
        ));
        providers.publish(Telemetry::unavailable(
            Source::ReadOnlyMemory,
            "Character respawned",
        ));
        assert!(providers.position(now, Some("self"), false).is_none());
        providers.publish(Telemetry::validated(
            Source::ReadOnlyMemory,
            Some("self".into()),
            None,
            position(),
            None,
            now,
        ));
        assert!(
            providers
                .position(
                    now + MAX_POSITION_AGE + std::time::Duration::from_millis(1),
                    Some("self"),
                    false
                )
                .is_none()
        );
    }

    #[test]
    fn simulated_never_falls_back_into_real_room_and_identity_is_exact() {
        let now = Instant::now();
        let mut providers = ProviderSet::default();
        providers.publish(Telemetry::validated(
            Source::Simulated,
            None,
            None,
            position(),
            None,
            now,
        ));
        assert!(providers.position(now, Some("self"), false).is_none());
        assert_eq!(
            providers.position(now, None, true).unwrap().0,
            Source::Simulated
        );
        providers.publish(Telemetry::validated(
            Source::ReadOnlyMemory,
            Some("other".into()),
            None,
            position(),
            None,
            now,
        ));
        assert!(providers.position(now, Some("self"), false).is_none());
        assert!(providers.position(now, Some("other"), false).is_some());
    }

    #[test]
    fn cooperative_requires_verified_identity_and_server_instance() {
        let now = Instant::now();
        let invalid = Telemetry::validated(
            Source::CooperatingExperience,
            Some("self".into()),
            None,
            position(),
            Some("job".into()),
            now,
        );
        assert_ne!(invalid.validity, Validity::Valid);
        let valid = Telemetry::validated(
            Source::CooperatingExperience,
            Some("self".into()),
            Some(42),
            position(),
            Some("job".into()),
            now,
        );
        assert_eq!(valid.validity, Validity::Valid);
    }
}
