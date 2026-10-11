# WolfLink Proximity v0.1.0 Alpha 2

Experimental Windows x64 pre-release. It is intended for friends-only testing, not a stable Roblox-proximity release.

Alpha.2 preserves Discord Social SDK voice, private invitations, authenticated Tailscale-bound relay hosting, simulated XYZ, fixed 60-stud smooth attenuation, and immediate muting for invalid or stale telemetry. The native app now includes an automatic GitHub updater: it checks alpha/stable release channels, verifies an Ed25519-signed update manifest and SHA-256-bound portable ZIP, stages a complete managed bundle, waits for the app to close, replaces it with rollback recovery, and restarts. Optional updates wait until the room ends. A signed minimum-version policy can block new rooms for an outdated client. Preferences remain outside the app directory.

The telemetry client now represents read-only memory, cooperating-experience, and simulated sources through a common validity/identity/timestamp structure. An unsupported or disconnected real source never silently becomes simulated proximity. The cooperating-experience source is an interface only: no authorized game feed or external HTTPS endpoint is configured in this release.

**Real Roblox status:** The Roblox client checked on 2026-10-12 failed all five previously validated RTTI layout checks. No current-build XYZ was obtained. The read-only reader correctly refuses to publish guessed coordinates, so real-game proximity remains unavailable and muted on that build. No verified server JobId is available. Experimental **Unverified Roblox Server** mode still requires explicit acknowledgement and valid real XYZ; it does not make this incompatible client work. Simulated proximity remains usable.

**Verification:** Automated Rust tests, updater archive/signature/download/recovery tests, a locally signed HTTP-to-install update test using two separately built fake app versions, C++ Release build, Windows portable package integrity, and local app launch checks were run. Earlier two-PC tests verified Discord voice and simulated attenuation. This alpha.2 build, its automatic update path against an actual published future release, and Tailscale transport between two PCs have not been externally verified. Direct P2P, directional audio, audio-device selection, and a universal supported Roblox XYZ source are not implemented.

## Setup

1. Alpha.1 users must manually download and extract this entire alpha.2 ZIP once. Alpha.1 has no updater and cannot discover alpha.2 itself. Keep all bundled files next to `WolfLink.exe`; settings in `%APPDATA%\WolfLink` are preserved.
2. Both friends sign in to Discord desktop and Tailscale. The host device must be shared with the friend or both devices allowed by Tailnet access rules; no port forwarding or IP entry is needed.
3. Each launches `WolfLink.exe`. The host selects **Create Room**, copies the invitation, and sends it privately. The friend pastes it in **Join Room**. Both approve Discord authorization if prompted; the host PC must remain online.
4. Use simulated XYZ for a working proximity demonstration. `unavailable` immediately mutes. Do not expect real Roblox proximity on the currently incompatible client.

Invitations are short-lived room secrets. Share them only with trusted friends. The source repository and release visibility are controlled by GitHub repository settings; no Discord or GitHub personal access token is embedded in the app.
