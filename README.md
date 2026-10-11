# WolfLink Proximity

## Friends-only Windows alpha (experimental)

`WolfLink.exe` is a native Windows frontend. It starts and supervises the read-only Roblox reader, authenticated telemetry client, Discord Social SDK voice bridge, and (for hosts) Tailnet-bound relay. No developer terminal is needed after packaging. The private alpha build is in `dist/WolfLink-0.1.0-alpha.1/WolfLink.exe` and `dist/WolfLink-0.1.0-alpha.1.zip`; it is not a stable or public release.

For a private test, both people install and sign in to Tailscale and ensure the two devices are permitted to communicate through Tailscale device sharing or Tailnet access rules. Both install Discord desktop, then run the same WolfLink development bundle. The host selects **Experimental private room**, enters their exact Roblox username, acknowledges that the Roblox server is unverified, clicks **Create Room**, and privately sends the copied invitation. The friend enters their own exact username, acknowledges the same limitation, pastes the invitation, and clicks **Join Room**. Both approve the Discord prompt. The host PC must stay online. Roblox telemetry and voice remain unavailable or muted when the memory layout, local-player identity, position copies, Tailnet peer identity, or relay session cannot be validated. No firewall rules are changed by WolfLink; Windows may ask the host to allow the app's Tailnet-bound listener.

The default non-experimental room uses simulated XYZ only. A regular production Roblox room remains disabled until a trustworthy Roblox server-instance identifier can be read. Direct P2P, spatial audio, audio-device selection, and production signed updates are not part of this build. Discord tokens are not written to preferences; the room invitation is a short-lived secret and should be shared only with trusted friends.

The Roblox client running on 2026-10-11 did not match the reader's validated RTTI layout. The old five vtable RVAs produce no valid RTTI in the live executable, and an earlier read-only scan found none of their expected type-descriptor strings in mapped images. This proves the old layout is incompatible; it does not establish replacement object offsets. On that build, experimental real-Roblox telemetry remains **unavailable** and proximity audio stays muted. `roblox-memory-scanner.exe diagnose-layout` reports the read-only RTTI check for future compatibility work.

Developers can build the local bundle with `cargo build --release --workspace --locked --offline`, the configured `voice/build` CMake Release target, then `scripts/package-dev.ps1`. The script copies required Discord SDK files, notices, and the app-local Visual C++ runtime files from the installed Build Tools without committing them. Microsoft permits app-local copying of files in the Visual Studio VC redist folder subject to its license terms ([redistribution guidance](https://learn.microsoft.com/en-us/visualstudio/releases/2022/redistribution)).

WolfLink is an in-progress Discord-powered proximity-voice client. The Roblox reader discovers the running process, identifies the local player's character, and reads its world position without modifying or controlling Roblox. The native frontend now manages the Discord voice bridge and authenticated telemetry relay as bundled components; see [voice/README.md](voice/README.md), [relay/README.md](relay/README.md), the [local P2P probe](p2p-probe/README.md), and the [networking design/status](NETWORKING.md).

The relay and attenuation logic have automated tests. The Windows voice bridge has been built against Discord Social SDK 1.10.19337. Two different users on two Windows PCs authenticated, joined the same call, and heard speech in both directions; changing the laptop participant's local volume from 100 to 0 and back audibly silenced and restored it on the PC. The combined clients also passed a live simulated-position test through a temporary `wss://` tunnel: speech was audible at 0 studs, quieter at 30, muted at 100, muted when telemetry became unavailable, and restored when valid coordinates returned. A Tailnet-bound host relay is now implemented and passed a local bind test, but this new transport has not been verified between two PCs. Independently verified Roblox game-session identity is still unavailable.

## Run

One validated sample:

```powershell
.\get-roblox-telemetry.ps1
```

Expected output:

```text
Local player: example_user
X: -6.92462
Y: 8.97397
Z: -4.055195
```

The wrapper builds the Rust scanner with the repository's existing Cargo workflow if the release executable is absent.

Continuous validated telemetry at the default 20 Hz:

```powershell
.\get-roblox-telemetry.ps1 -Continuous
```

The optional `-Hz` value accepts rates from 1 through 240. Continuous mode updates one terminal line instead of emitting a line per sample.

## Read-only boundary

The scanner opens Roblox with only:

- `PROCESS_QUERY_INFORMATION` (`0x0400`)
- `PROCESS_VM_READ` (`0x0010`)

It uses `VirtualQueryEx` and `ReadProcessMemory`. It contains no process-write, injection, hook, patch, remote-thread, input, suspension, or termination operations.

## Discovery and continuous validation

The locator:

1. Auto-detects the live `RobloxPlayerBeta.exe` PID.
2. Discovers the relocated Roblox PE image from committed memory and validates the expected classes through MSVC RTTI.
3. Enumerates `RBX::Player` and `RBX::Humanoid` instances.
4. Follows each Humanoid's root `RBX::Primitive` and requires its two internal position copies to agree.
5. Resolves the Primitive's owning `RBX::Part`, then the parent `RBX::ModelInstance` and its name.
6. Selects the one character Model whose name matches an `RBX::Player` name.

In the previously validated single-player game, this rejected an additional stationary Humanoid named `Player` and selected the local character Model by its matching Player name.

After discovery, continuous mode caches the validated Player, character Model, Humanoid, Part, and Primitive addresses. Normal samples only perform small pointer/identity reads and read the Primitive's two 12-byte position copies. A position is published only when both copies agree exactly and all coordinates are finite and within the accepted world range.

The cached identity chain and process identity are revalidated periodically. Unreadable or changed identity pointers immediately make telemetry unavailable and discard the cached target. Five consecutive duplicate-position disagreements also discard it. Discovery then starts again automatically, covering Roblox restarts, character respawns, teleports/game changes, and object replacement without retaining old coordinates.

## Internal telemetry model

Every polling result is represented by the `Telemetry` structure in `src/main.rs`:

- `username: Option<String>`
- `user_id: Option<u64>`
- `x`, `y`, and `z: Option<f32>`
- `sample_timestamp: Option<SystemTime>`
- `validity: TelemetryValidity`

Coordinates and timestamps are populated only for `TelemetryValidity::Valid`. Invalid states never retain or publish an earlier position. `user_id` currently remains `None` because no Roblox UserId field offset has been independently validated for this client build.

## Reliability and limitations

- Heap addresses and the module base are rediscovered whenever the cached target becomes invalid, so Roblox restarts, character respawns, and game changes do not depend on old absolute addresses.
- The current class RVAs and field offsets are version-specific. Every class RVA is checked against its RTTI name before use; an incompatible Roblox update fails explicitly instead of returning guessed telemetry.
- A one-shot discovery currently scans roughly 2.3 GiB of readable private memory twice and takes about four seconds on this system.
- Local-player selection currently requires exactly one character Model name to match exactly one `RBX::Player` instance. This is intentionally fail-closed for the current single-player test environment rather than guessing in an ambiguous multiplayer process.
- The reader's `stream-telemetry --machine` output emits only validated `POS X Y Z` samples or `UNAVAILABLE`; malformed output is treated as unavailable. The combined Rust voice client accepts real positions only in an explicitly acknowledged Unverified Roblox Server private room. Strict verified-session Roblox proximity remains disabled because no independently verified server-instance identifier is available.

Diagnostic commands are available through `target\release\roblox-memory-scanner.exe` for read-only layout checks, memory surveys, CFrame snapshots, vector tracking, RTTI classification, pointer ownership, and object inspection. The combined voice client supports simulated positions or validated real positions in the explicitly acknowledged experimental room; the currently observed Roblox build provides no validated real positions.
