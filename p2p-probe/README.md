# Local P2P transport probe

This isolated Rust executable creates two `webrtc-rs` peers on `127.0.0.1`, exchanges their SDP descriptions in memory, and verifies one data-channel payload. Run it with:

```powershell
cargo run -p wolflink-p2p-probe --locked
```

It uses no Discord lobby, Roblox telemetry, relay, external STUN/TURN server, manual IP entry, or microphone audio. It does **not** establish a remote connection or authenticate a Discord participant. Its only purpose is to prove the current Windows Rust toolchain and `webrtc-rs` data-channel API before integrating bounded signaling, peer identity binding, ICE/STUN, reconnection, and validated position packets. It is not part of the end-user client.
