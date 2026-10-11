# WolfLink telemetry relay

The relay accepts outbound WebSocket connections from clients. It verifies each Discord OAuth bearer token with Discord's `GET /oauth2/@me`, requires the configured Discord application ID and `identify` scope, and uses the returned Discord user ID as the participant identity. A client cannot claim another participant's ID.

Each connection also supplies a shared private room secret and an exact game-session key. Positions are sent only to authenticated participants with both values matching. The room secret is hashed in server memory. The relay stores no positions on disk and excludes positions older than 500 ms from snapshots.

## Run for local development

```powershell
$env:WOLFLINK_DISCORD_APP_ID = '<application_id>'
cargo run -p wolflink-relay --bin server
```

The default listener is `127.0.0.1:8765`. It is plain WebSocket for local development. A public deployment must terminate TLS and expose a `wss://` endpoint through a managed reverse proxy; clients never need port forwarding or VPN setup. `WOLFLINK_RELAY_BIND` configures the server-side listener only.

## Client prototype

Build the Rust client and relay with `cargo build --release -p wolflink-relay --bins`. After building the [Discord voice bridge](../voice/README.md), run this on each PC with the same application ID, room secret, game-session key, and hosted relay URL:

```powershell
.\target\release\client.exe <application_id> <shared_room_secret> <shared_test_session_key> wss://<host>/ <path_to_wolflink_voice_poc.exe> --simulate
```

The client launches the voice bridge, completes Discord authorization, connects outbound to the relay, and initially mutes every voice participant. In simulated mode, type `position X Y Z` to set a virtual position that is refreshed at 20 Hz, or `unavailable` to mute. Each user must authenticate with a different Discord account. Distance uses a 60-stud cutoff and squared falloff. Relay loss, stale or malformed telemetry, a missing remote position, or a Discord identity mismatch mutes playback.

The final argument may instead be the path to `roblox-memory-scanner.exe`, which the client starts in read-only `stream-telemetry --machine --hz 20` mode. This integration is **not** a validated multiplayer game-session solution: the reader does not yet expose a reliable Roblox server-instance ID. The supplied session key isolates relay traffic only if it has been independently verified to identify the same actual game instance on every PC. Never use a place ID or a guessed session key for production proximity audio.

The relay has no hosted endpoint yet. Running the server locally and connecting to `ws://127.0.0.1:8765` is for same-PC protocol testing only; two remote PCs need a deployed TLS endpoint. No end-user port forwarding or VPN is planned.

## Wire protocol

First client message:

```json
{"type":"hello","access_token":"<Discord OAuth token>","room_secret":"<random private secret>","game_session":"<exact game instance key>"}
```

Then send `{"type":"position","position":{"x":1.0,"y":2.0,"z":3.0}}` for validated positions, or `{"type":"unavailable"}` as soon as telemetry fails. The server sends a `welcome` message with the authenticated Discord ID, followed by `snapshot` messages at 20 Hz. Each snapshot contains recent positions of *other* participants in the same room and game session. Absence from a snapshot means mute that participant.

The application must never use a public Roblox place ID alone as the game-session key: different server instances of one place must remain isolated. Until a reliably validated instance identifier is available, simulated sessions should use a random shared test key.

## Current verification

Unit tests cover private room and session separation, duplicate identities, invalid coordinates, 500 ms expiry, distance attenuation, and client fail-closed parsing/muting. WebSocket integration tests use a fake local Discord authorization endpoint to verify multiple users, room isolation, and token rejection. Live Discord authentication, SDK audio, two-PC communication, and a public `wss://` deployment remain unverified.
