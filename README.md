# Roblox read-only telemetry reader

This project discovers the running Roblox process, identifies the local player's character, and reads its world position without modifying or controlling Roblox.

## Run

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

## Read-only boundary

The scanner opens Roblox with only:

- `PROCESS_QUERY_INFORMATION` (`0x0400`)
- `PROCESS_VM_READ` (`0x0010`)

It uses `VirtualQueryEx` and `ReadProcessMemory`. It contains no process-write, injection, hook, patch, remote-thread, input, suspension, or termination operations.

## Discovery chain

The current one-shot locator:

1. Auto-detects the live `RobloxPlayerBeta.exe` PID.
2. Discovers the relocated Roblox PE image from committed memory and validates the expected classes through MSVC RTTI.
3. Enumerates `RBX::Player` and `RBX::Humanoid` instances.
4. Follows each Humanoid's root `RBX::Primitive` and requires its two internal position copies to agree.
5. Resolves the Primitive's owning `RBX::Part`, then the parent `RBX::ModelInstance` and its name.
6. Selects the one character Model whose name matches an `RBX::Player` name.

This correctly rejects the current game's additional stationary Humanoid named `Player` and selects the character Model named `orwell213`.

## Reliability and limitations

- Heap addresses and the module base are rediscovered every run, so Roblox restarts, character respawns, and game changes do not depend on old absolute addresses.
- The current class RVAs and field offsets are version-specific. Every class RVA is checked against its RTTI name before use; an incompatible Roblox update fails explicitly instead of returning guessed telemetry.
- A one-shot discovery currently scans roughly 2.3 GiB of readable private memory twice and takes about four seconds on this system.
- This is not yet a high-frequency Discord telemetry stream. The next step is to keep the validated Primitive pointer and poll only its position, rediscovering when the object or process changes.

Diagnostic commands are available through `target\release\roblox-memory-scanner.exe` for memory surveys, CFrame snapshots, vector tracking, RTTI classification, pointer ownership, and object inspection.
