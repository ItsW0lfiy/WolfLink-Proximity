# Networking design and current boundary

Status: design for the next implementation increment, not a working direct-P2P release. The validated two-PC path still uses the authenticated outbound WebSocket relay with simulated positions. An isolated [local WebRTC data-channel probe](p2p-probe/README.md) passes over loopback; no integrated remote data channel, permanent relay, automatic room invitation, or Tailscale transport is implemented yet.

## Intended private-room path

1. Discord authenticates each user and provides the private voice lobby. A room invitation must convey a high-entropy lobby secret through a user-approved channel; a Discord user ID typed by a peer is never accepted as proof of identity.
2. The Discord Social SDK lobby can carry *low-rate* connection negotiation between current voice participants. The bridge's dormant `WL1` signal interface authenticates the message author via the SDK, checks the recipient and lobby, rejects linked-channel lobbies, and rate-limits sends. It does not yet parse a protocol or establish a connection. Other lobby members may see signals. Encoded payloads are not confidential.
3. A future Rust peer manager should try a [webrtc-rs](https://docs.rs/webrtc/latest/webrtc/) data channel using ICE/STUN first. The channel uses DTLS; the SDP fingerprint must be bound to the authenticated Discord sender, current lobby/room, intended recipient, and a fresh negotiation nonce. Candidate/SDP data needs bounded chunking because [Discord lobby messages are limited to 2,000 characters](https://discord.com/developers/docs/social-sdk/classdiscordpp_1_1Client.html). Do not send OAuth tokens, room secrets, microphone audio, or XYZ packets through lobby messages.
4. Negotiation messages require a version, unique session nonce, sender and recipient IDs, room/lobby binding, expiry, sequence/chunk bounds, and replay rejection. A peer connection must not publish or accept position data until the authenticated transport identity and room membership agree. Position packets then require monotonic sequence numbers, a bounded age, valid finite coordinates, and independent Roblox server-instance matching. Any mismatch mutes the peer.
5. If direct ICE fails, explicitly report the failure and try only fallbacks the user has authorized. Existing Tailscale can be detected read-only with `tailscale status --json`; peer endpoints must still be correlated with an authenticated room invitation, not selected from arbitrary tailnet devices. Do not install, sign in, or change Tailscale settings silently. A user/community relay is another optional fallback, not a mandatory service.

Direct ICE without a TURN relay cannot guarantee connectivity through every NAT or firewall. A friend group can still use a permitted Tailscale path or a voluntarily operated relay. WolfLink must never imply that a free, infrastructure-less fallback works universally.

## Scaling and privacy

For `n` participants, a full direct mesh has `n(n-1)/2` peer connections and each client sends to `n-1` peers. At 20 Hz this is small-position-packet traffic, but connection/ICE overhead and Discord call capacity still need tests at 3, 10, and 20+ participants. Filtering solely by current proximity is unsafe because a distant peer may move into range; room membership and coarse discovery must stay available. Discord voice remains the only audio transport.

P2P negotiation may reveal public IP candidates to room members. Tailscale has its own account, sharing, and free-plan limits; unrelated tailnets do not automatically interconnect. The current manual `game_session` key is only for simulated test rooms; the real Roblox path is blocked until the reader independently validates a server-instance identifier.

## Embedded Tailscale decision point

The official [tsnet](https://tailscale.com/docs/reference/tsnet-server-api) library is Go-based. If direct P2P and existing-app Tailscale prove insufficient, a possible isolated Go helper would expose only a local authenticated transport API to Rust, own a per-app Tailscale state directory, and package a compiled Windows binary. It would not require Go on user PCs or change the system VPN. This is a proposal only: no Go component has been introduced, and explicit approval is required before doing so. Account authorization, cross-tailnet sharing, state protection, redistribution, and [free-plan limits](https://tailscale.com/pricing) must be checked before product claims.
