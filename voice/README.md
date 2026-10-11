# Discord voice proof of concept

This small C++ program uses the Discord Social SDK to authenticate a user, join a shared lobby, start a voice call, and adjust one participant's local playback volume. It does not depend on Roblox or the telemetry reader.

Status: built against Discord Social SDK 1.10.19337 on Windows x64. Two different Discord accounts on two Windows PCs authenticated, joined the same lobby, and reached call status `Connected`. Speech was audible in both directions. Laptop microphone speech was audible on the PC at participant volume 100, silent at 0, and audible again at 100 with the laptop microphone settings unchanged. This verifies live two-way speech and audible local volume control. The distance-driven relay path still needs a clean two-PC test.

## Setup

1. Create one application at the [Discord Developer Portal](https://discord.com/developers/applications).
2. Complete its Social SDK Getting Started flow. On OAuth2, add `http://127.0.0.1/callback` as a redirect and enable Public Client for this proof of concept.
3. Download the Windows standalone C++ Social SDK package from that application's Social SDK page. Pass the extracted `discord_social_sdk` directory as `DISCORD_SDK_ROOT`. A local copy in `dependancies/` is ignored by Git.
4. Build with Visual Studio C++ and CMake:

   ```powershell
   cmake -S voice -B voice/build -A x64 -DDISCORD_SDK_ROOT="C:\path\to\discord_social_sdk"
   cmake --build voice/build --config Release
   ```

5. On both Windows PCs, run the resulting executable with the same application ID and the same hard-to-guess lobby secret (at least 16 characters):

   ```powershell
   .\voice\build\Release\wolflink_voice_poc.exe <application_id> <shared_lobby_secret>
   ```

6. Authenticate each PC with a different Discord account. The program prints the Discord user ID and voice participant IDs. Speak into one microphone and listen on the other PC. To prove local volume control, enter `volume <other_user_id> 0`, then `volume <other_user_id> 100`. `participants` lists current call users. `self-mute on|off` and `self-deaf on|off` help isolate audio routing; `quit` exits.

For a second-PC test, copy the built executable and every `.dll` and `.kef` file from `voice/build/Release`, plus the SDK's `License-Notices.txt`, to the other PC. Both PCs need the same application ID and lobby secret but must authorize different Discord accounts. This standalone voice test does not require a telemetry relay or Roblox.

If authorization never appears, check that the bridge and the Discord desktop client run under the same Windows user. In the Codex sandbox, the SDK repeatedly reported `RPC Connect error: -4058`; running the bridge as the signed-in desktop user made the authorization prompt appear.

The SDK owns capture and playback. No Discord token or client secret should be entered on the command line. This program currently reauthorizes at startup and does not persist tokens.

During the two-PC test, an active RustDesk remote-desktop session caused a delayed self-echo; the echo was gone with RustDesk disconnected and WolfLink self-mute/self-deaf off. A separate fan-like sound persisted after WolfLink exited and was traced to the running Roblox game. Keep other audio/remote-desktop sessions out of controlled voice tests.

The Rust relay client starts this executable with `--machine`, using a private stdout pipe for the short-lived OAuth token and stdin for volume commands. `--machine` refuses to run with terminal stdout. New voice participants are muted before their IDs are sent to the Rust client; the Rust client unmutes them only after authenticated relay identities and fresh matching-session positions are available.
