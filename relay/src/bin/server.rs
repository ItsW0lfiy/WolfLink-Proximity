use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::{Duration, interval, timeout};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use wolflink_relay::invitation::{Invitation, unix_now};
use wolflink_relay::tailscale::{parse_local_status, status_json};
use wolflink_relay::{ClientMessage, RoomKey, Rooms, ServerMessage};

#[derive(Deserialize)]
struct Authorization {
    application: Application,
    scopes: Vec<String>,
    user: Option<User>,
}

#[derive(Deserialize)]
struct Application {
    id: String,
}

#[derive(Deserialize)]
struct User {
    id: String,
}

#[derive(Clone)]
struct Server {
    rooms: Arc<Mutex<Rooms>>,
    http: reqwest::Client,
    app_id: String,
    auth_url: String,
    next_connection_id: Arc<AtomicU64>,
    hosted_room: Option<HostedRoom>,
}

#[derive(Clone)]
struct HostedRoom {
    key: RoomKey,
    expires_at: u64,
}

impl Server {
    async fn authenticate(&self, token: &str) -> Result<String, &'static str> {
        if token.is_empty() || token.len() > 4096 || token.chars().any(char::is_control) {
            return Err("invalid bearer token");
        }
        let response = self
            .http
            .get(&self.auth_url)
            .bearer_auth(token)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|_| "Discord authentication unavailable")?;
        if !response.status().is_success() {
            return Err("Discord authentication rejected");
        }
        let auth: Authorization = response
            .json()
            .await
            .map_err(|_| "invalid Discord authorization response")?;
        let user = auth.user.ok_or("Discord authorization has no user")?;
        if auth.application.id != self.app_id
            || !auth.scopes.iter().any(|scope| scope == "identify")
            || user.id.parse::<u64>().ok().filter(|id| *id != 0).is_none()
        {
            return Err("Discord authorization does not match this application or user");
        }
        Ok(user.id)
    }
}

async fn send(
    ws: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    message: ServerMessage,
) -> bool {
    let Ok(json) = serde_json::to_string(&message) else {
        return false;
    };
    ws.send(Message::Text(json.into())).await.is_ok()
}

async fn handle(stream: TcpStream, server: Server) {
    let Ok(mut ws) = accept_async(stream).await else {
        return;
    };
    let Some(Ok(Message::Text(first))) = timeout(Duration::from_secs(10), ws.next())
        .await
        .ok()
        .flatten()
    else {
        let _ = send(
            &mut ws,
            ServerMessage::Error {
                message: "expected hello".into(),
            },
        )
        .await;
        return;
    };
    if first.len() > 8192 {
        let _ = send(
            &mut ws,
            ServerMessage::Error {
                message: "hello too large".into(),
            },
        )
        .await;
        return;
    }
    let Ok(ClientMessage::Hello {
        access_token,
        room_secret,
        game_session,
    }) = serde_json::from_str::<ClientMessage>(&first)
    else {
        let _ = send(
            &mut ws,
            ServerMessage::Error {
                message: "invalid hello".into(),
            },
        )
        .await;
        return;
    };
    let Some(room) = RoomKey::new(&room_secret, &game_session) else {
        let _ = send(
            &mut ws,
            ServerMessage::Error {
                message: "invalid room or game session".into(),
            },
        )
        .await;
        return;
    };
    if server.hosted_room.as_ref().is_some_and(|hosted| {
        hosted.key != room || unix_now().map_or(true, |now| now >= hosted.expires_at)
    }) {
        let _ = send(
            &mut ws,
            ServerMessage::Error {
                message: "room invitation is invalid or expired".into(),
            },
        )
        .await;
        return;
    }
    let discord_id = match server.authenticate(&access_token).await {
        Ok(id) => id,
        Err(error) => {
            let _ = send(
                &mut ws,
                ServerMessage::Error {
                    message: error.into(),
                },
            )
            .await;
            return;
        }
    };
    let connection_id = server.next_connection_id.fetch_add(1, Ordering::Relaxed);
    if !server
        .rooms
        .lock()
        .await
        .join(room.clone(), discord_id.clone(), connection_id)
    {
        let _ = send(
            &mut ws,
            ServerMessage::Error {
                message: "Discord user already connected to this room".into(),
            },
        )
        .await;
        return;
    }
    if !send(
        &mut ws,
        ServerMessage::Welcome {
            discord_id: discord_id.clone(),
        },
    )
    .await
    {
        server
            .rooms
            .lock()
            .await
            .leave(&room, &discord_id, connection_id);
        return;
    }

    let mut ticks = interval(Duration::from_millis(50));
    loop {
        tokio::select! {
            frame = ws.next() => {
                let Some(Ok(frame)) = frame else { break; };
                let Message::Text(text) = frame else {
                    if matches!(frame, Message::Close(_)) { break; }
                    continue;
                };
                if text.len() > 2048 { break; }
                match serde_json::from_str::<ClientMessage>(&text) {
                    Ok(ClientMessage::Position { position }) => {
                        if !server.rooms.lock().await.update(&room, &discord_id, connection_id, Some(position), Instant::now()) {
                            break;
                        }
                    }
                    Ok(ClientMessage::Unavailable) => {
                        server.rooms.lock().await.update(&room, &discord_id, connection_id, None, Instant::now());
                    }
                    _ => break,
                }
            }
            _ = ticks.tick() => {
                if server.hosted_room.as_ref().is_some_and(|hosted| unix_now().map_or(true, |now| now >= hosted.expires_at)) {
                    break;
                }
                let participants = server.rooms.lock().await.snapshot(&room, &discord_id, Instant::now());
                if !send(&mut ws, ServerMessage::Snapshot { participants }).await { break; }
            }
        }
    }
    server
        .rooms
        .lock()
        .await
        .leave(&room, &discord_id, connection_id);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app_id =
        env::var("WOLFLINK_DISCORD_APP_ID").map_err(|_| "WOLFLINK_DISCORD_APP_ID is required")?;
    app_id
        .parse::<u64>()
        .map_err(|_| "invalid Discord application ID")?;
    let hosted = env::var("WOLFLINK_ROOM_INVITE")
        .ok()
        .map(|encoded| Invitation::decode(&encoded, unix_now()?))
        .transpose()?;
    let bind = if let Some(invitation) = &hosted {
        if invitation.application_id.to_string() != app_id {
            return Err("Hosted invitation application ID does not match".into());
        }
        let local = parse_local_status(&status_json()?)?;
        if local.address != invitation.host_address || local.public_key != invitation.host_key {
            return Err("Hosted invitation does not match the current Tailscale device".into());
        }
        format!("{}:{}", local.address, invitation.port)
    } else {
        env::var("WOLFLINK_RELAY_BIND").unwrap_or_else(|_| "127.0.0.1:8765".into())
    };
    let listener = TcpListener::bind(&bind).await?;
    let hosted_room = hosted.as_ref().map(|invitation| HostedRoom {
        key: RoomKey::new(&invitation.room_secret, &invitation.session_key)
            .expect("validated invitation room"),
        expires_at: invitation.expires_at,
    });
    let server = Server {
        rooms: Arc::new(Mutex::new(Rooms::default())),
        http: reqwest::Client::builder().build()?,
        app_id,
        auth_url: "https://discord.com/api/v10/oauth2/@me".into(),
        next_connection_id: Arc::new(AtomicU64::new(1)),
        hosted_room,
    };
    println!("WolfLink relay listening on {bind}");
    let mut checks = interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let server = server.clone();
                tokio::spawn(handle(stream, server));
            }
            _ = checks.tick(), if server.hosted_room.is_some() => {
                if server.hosted_room.as_ref().is_some_and(|hosted| unix_now().map_or(true, |now| now >= hosted.expires_at)) {
                    break;
                }
                let local = parse_local_status(&status_json()?);
                if hosted.as_ref().is_some_and(|invitation| local.map_or(true, |local| local.address != invitation.host_address || local.public_key != invitation.host_key)) {
                    return Err("Tailscale host identity or connection changed".into());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

    async fn fake_discord() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = [0u8; 8192];
                    let size = stream.read(&mut request).await.unwrap();
                    let text = String::from_utf8_lossy(&request[..size]);
                    let user_id = if text.contains("token-a") {
                        Some("101")
                    } else if text.contains("token-b") {
                        Some("102")
                    } else if text.contains("token-c") {
                        Some("103")
                    } else {
                        None
                    };
                    let (status, body) = match user_id {
                        Some(id) => (
                            "200 OK",
                            format!(
                                "{{\"application\":{{\"id\":\"1234\"}},\"scopes\":[\"identify\"],\"user\":{{\"id\":\"{id}\"}}}}"
                            ),
                        ),
                        None => ("401 Unauthorized", "{}".to_string()),
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                });
            }
        });
        format!("http://{address}/oauth2/@me")
    }

    async fn test_relay_with_room(auth_url: String, hosted_room: Option<HostedRoom>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = Server {
            rooms: Arc::new(Mutex::new(Rooms::default())),
            http: reqwest::Client::new(),
            app_id: "1234".into(),
            auth_url,
            next_connection_id: Arc::new(AtomicU64::new(1)),
            hosted_room,
        };
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(handle(stream, server.clone()));
            }
        });
        format!("ws://{address}")
    }

    async fn test_relay(auth_url: String) -> String {
        test_relay_with_room(auth_url, None).await
    }

    async fn connect(
        url: &str,
        token: &str,
        room: &str,
        session: &str,
    ) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
        let (mut ws, _) = connect_async(url).await.unwrap();
        let hello = serde_json::json!({
            "type": "hello",
            "access_token": token,
            "room_secret": room,
            "game_session": session
        });
        ws.send(Message::Text(hello.to_string().into()))
            .await
            .unwrap();
        ws
    }

    async fn next_json(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> serde_json::Value {
        let frame = timeout(Duration::from_secs(3), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(frame.to_text().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn authenticated_users_share_only_their_room_and_session() {
        let url = test_relay(fake_discord().await).await;
        let secret = "private-abcdefghijkl";
        let mut a = connect(&url, "token-a", secret, "server-1").await;
        let mut b = connect(&url, "token-b", secret, "server-1").await;
        let mut c = connect(&url, "token-c", secret, "server-2").await;
        assert_eq!(next_json(&mut a).await["discord_id"], "101");
        assert_eq!(next_json(&mut b).await["discord_id"], "102");
        assert_eq!(next_json(&mut c).await["discord_id"], "103");

        b.send(Message::Text(
            r#"{"type":"position","position":{"x":10.0,"y":0.0,"z":0.0}}"#.into(),
        ))
        .await
        .unwrap();
        let from_b = timeout(Duration::from_secs(3), async {
            loop {
                let message = next_json(&mut a).await;
                if message["participants"]
                    .as_array()
                    .is_some_and(|people| !people.is_empty())
                {
                    break message;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(from_b["participants"][0]["discord_id"], "102");
        assert_eq!(from_b["participants"][0]["position"]["x"], 10.0);
        assert!(
            next_json(&mut c).await["participants"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn rejects_unknown_discord_token() {
        let url = test_relay(fake_discord().await).await;
        let mut ws = connect(&url, "invalid", "private-abcdefghijkl", "server-1").await;
        let error = next_json(&mut ws).await;
        assert_eq!(error["type"], "error");
    }

    #[tokio::test]
    async fn hosted_relay_rejects_other_rooms_and_expired_invites() {
        let secret = "private-abcdefghijkl";
        let session = "server-1";
        let key = RoomKey::new(secret, session).unwrap();
        let url = test_relay_with_room(
            fake_discord().await,
            Some(HostedRoom {
                key: key.clone(),
                expires_at: unix_now().unwrap() + 60,
            }),
        )
        .await;
        let mut other = connect(&url, "token-a", "private-qrstuvwxyz01", session).await;
        assert_eq!(next_json(&mut other).await["type"], "error");
        let mut accepted = connect(&url, "token-a", secret, session).await;
        assert_eq!(next_json(&mut accepted).await["type"], "welcome");

        let expired = test_relay_with_room(
            fake_discord().await,
            Some(HostedRoom {
                key,
                expires_at: unix_now().unwrap() - 1,
            }),
        )
        .await;
        let mut rejected = connect(&expired, "token-b", secret, session).await;
        assert_eq!(next_json(&mut rejected).await["type"], "error");
    }
}
