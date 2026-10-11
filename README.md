# WolfLink Proximity

WolfLink is an in-progress Discord-powered proximity-voice client. The Roblox reader discovers the running process, identifies the local player's character, and reads its world position without modifying or controlling Roblox. The Discord voice bridge and authenticated telemetry relay are separate components; see [voice/README.md](voice/README.md) and [relay/README.md](relay/README.md).

The relay and attenuation logic have automated tests. The Windows voice bridge has been built against Discord Social SDK 1.10.19337. Two different users on two Windows PCs authenticated, joined the same call, and heard speech in both directions; changing the laptop participant's local volume from 100 to 0 and back audibly silenced and restored it on the PC. The combined clients also passed a live simulated-position test through a temporary `wss://` tunnel: speech was audible at 0 studs, quieter at 30, muted at 100, muted when telemetry became unavailable, and restored when valid coordinates returned. A stable hosted relay and independently verified Roblox game-session identity are still needed before production Roblox-derived proximity can be claimed.

## Run

One validated sample:

```powershell
.\get-roblox-telemetry.ps1
```

Expected output:

```text
Local player: orwell213
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

This correctly rejects the current game's additional stationary Humanoid named `Player` and selects the character Model named `orwell213`.

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
- The reader's `stream-telemetry --machine` output emits only validated `POS X Y Z` samples or `UNAVAILABLE`; malformed output is treated as unavailable. The combined Rust voice client now refuses Roblox-derived positions because the reader still has no independently verified Roblox server-instance identifier. The reader can be run separately for diagnostics.

Diagnostic commands are available through `target\release\roblox-memory-scanner.exe` for memory surveys, CFrame snapshots, vector tracking, RTTI classification, pointer ownership, and object inspection. The combined voice client currently permits simulated positions only; it rejects real Roblox-derived proximity until it can independently verify the server instance.
