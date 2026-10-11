# WolfLink Proximity v0.1.0 Alpha 1

Private Windows x64 pre-release for friends-only testing. This is not a stable Roblox-proximity release.

The portable ZIP contains `WolfLink.exe`, its managed voice/relay/reader components, the Discord Social SDK runtime and redistribution notices, and app-local Visual C++ runtime files. Discord desktop and an authorized, connected Tailscale installation are required for the current remote-room transport. The host must remain online. No port forwarding or developer tools are required.

Features: Discord Social SDK authorization and two-way voice; private invitations and an authenticated, room-isolated Tailnet relay; simulated XYZ proximity with smoothed per-participant volume; immediate muting for invalid, stale, or unavailable telemetry; and an explicit **Unverified Roblox Server** mode for trusted friends who acknowledge that they must join the same Roblox server. The reader uses only query/read process permissions and never modifies Roblox.

**Roblox telemetry compatibility:** Real XYZ was **not** restored for the Roblox client observed on 2026-10-11. Its five previously validated class-vtable RVAs no longer resolve to the required RTTI types. The reader refuses this layout, publishes no coordinates, and experimental real-game proximity remains muted. No replacement offsets have been guessed. Roblox server JobId verification is also unavailable, so strict verified-session Roblox proximity remains disabled. Simulated coordinates are never silently substituted for real telemetry.

**Verification limits:** Automated Rust tests, C++ Release compilation, local WebRTC loopback, a local Tailnet relay bind check, package inspection, and a Windows app launch check were run for this build. Earlier two-PC testing proved Discord voice and simulated-position attenuation over a temporary tunnel, but this new packaged Tailnet transport has **not** been verified between two PCs. Direct P2P, directional audio, a production updater, audio-device selection, and an installer are not included.

## Setup

1. Both friends install/sign in to Discord desktop and Tailscale. The host's device must be shared with the friend, or their Tailnet access rules must allow the connection.
2. Each extracts the entire ZIP, then launches `WolfLink.exe` from the extracted folder.
3. The host chooses **Create Room**, copies the invitation, and sends it privately. The friend pastes it and chooses **Join Room**. Approve Discord authorization when prompted.
4. For a working proximity demonstration on the current Roblox build, leave experimental mode unchecked and set simulated XYZ positions. **Unavailable** mutes proximity audio.

Do not use experimental real-Roblox mode expecting live proximity on the currently incompatible client. Its correct behavior is to stay unavailable and mute. Invitations are short-lived secrets; share only with trusted friends. The private repository's release assets require an authorized GitHub account.
