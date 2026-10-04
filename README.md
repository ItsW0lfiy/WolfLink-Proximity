# Roblox read-only telemetry reader

This project discovers the running Roblox process, identifies the local player's character, and reads its world position without modifying or controlling Roblox.

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
- No networking or Discord integration is included yet.

Diagnostic commands are available through `target\release\roblox-memory-scanner.exe` for memory surveys, CFrame snapshots, vector tracking, RTTI classification, pointer ownership, and object inspection.
