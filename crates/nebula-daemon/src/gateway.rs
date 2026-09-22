//! The GATEWAY: the daemon's protocol over a WebSocket, for clients that
//! are not the TUI — the editor extension, or a page in a browser.
//!
//! It is a transport and nothing else. Every request and every event is the
//! same `ClientRequest` / `ServerEvent` the unix socket carries, handed to
//! the same [`crate::server::serve_client`], so a second client can never
//! drift from the first: there is one protocol, one handler, one place a
//! feature is added.
//!
//! Framing is the WebSocket's own — one binary message per MessagePack
//! payload, no length prefix, since the transport already delimits. PTY
//! output arrives as the bytes it is (`Output`, `Scrollback`), which is
//! what a terminal emulator on the other end wants.
//!
//! **What this opens.** A client here can start sessions and type into
//! them, which is to say run commands as this user. So the listener binds
//! `127.0.0.1` and nothing else — no flag widens it — and every connection
//! must present a token this boot generated, compared in constant time. The
//! token is published to a 0600 file in the runtime dir for local clients to
//! read; anything off this machine belongs behind `nebula tunnel`'s ssh
//! forwarding, which is already how a remote nebula is reached.

use crate::registry::Daemon;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use nebula_core::protocol::{ClientRequest, ServerEvent};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;

/// What the daemon publishes for local clients: where the gateway listens
/// and the token to present. Written fresh each boot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayInfo {
    pub port: u16,
    pub token: String,
    /// The protocol the daemon speaks, so a client can refuse early and say
    /// which build it needs rather than failing at the handshake.
    pub protocol_version: u32,
}

struct GatewayState {
    daemon: Arc<Daemon>,
    token: String,
}

#[derive(Deserialize)]
struct Auth {
    token: Option<String>,
}

/// Bind `127.0.0.1:0`, serve the gateway, and publish the handshake file.
/// Returns what was published. A failure here never takes the daemon down:
/// the unix socket is the primary transport and does not depend on this.
pub async fn start(daemon: Arc<Daemon>) -> anyhow::Result<GatewayInfo> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let token = generate_token();
    let info = GatewayInfo {
        port,
        token: token.clone(),
        protocol_version: nebula_core::protocol::PROTOCOL_VERSION,
    };
    publish(&info)?;

    let state = Arc::new(GatewayState {
        daemon,
        token: token.clone(),
    });
    let app = Router::new()
        .route("/health", get(health))
        .route("/ws", get(upgrade))
        .with_state(state);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "gateway died");
        }
    });
    tracing::info!(port, "gateway listening");
    Ok(info)
}

/// Write the handshake file 0600, creating the runtime dir if the socket
/// has not already. Owner-only from the moment it exists: written to a
/// temporary file in the same directory and renamed over, so no reader ever
/// sees it at the default mode.
fn publish(info: &GatewayInfo) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = nebula_core::paths::gateway_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(serde_json::to_string(info)?.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Remove the handshake file. A token that outlives its daemon opens
/// nothing, but a stale file makes a client dial a port that is gone and
/// report the wrong thing.
pub fn unpublish() {
    let _ = std::fs::remove_file(nebula_core::paths::gateway_path());
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "nebula gateway")
}

async fn upgrade(
    State(state): State<Arc<GatewayState>>,
    Query(auth): Query<Auth>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if !token_ok(auth.token.as_deref(), &state.token) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    ws.on_upgrade(move |socket| serve(socket, state))
}

/// One WebSocket client, wired to the same handler the unix socket uses.
async fn serve(socket: WebSocket, state: Arc<GatewayState>) {
    use futures_util::{SinkExt, StreamExt};
    let (mut sink, mut stream) = socket.split();

    let (req_tx, req_rx) = mpsc::channel::<ClientRequest>(64);
    let (out_tx, mut out_rx) = mpsc::channel::<ServerEvent>(256);

    // Events out: one message per event, so the client never has to reframe.
    let writer = tokio::spawn(async move {
        while let Some(ev) = out_rx.recv().await {
            let Ok(payload) = rmp_serde::to_vec_named(&ev) else {
                continue;
            };
            if sink.send(Message::Binary(payload.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    // Requests in. A frame that will not decode is dropped rather than
    // taken as the end of the connection: one malformed message from a
    // half-written client must not sever a session the user is typing into.
    let reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            let bytes = match msg {
                Message::Binary(b) => b.to_vec(),
                Message::Text(t) => t.as_bytes().to_vec(),
                Message::Close(_) => break,
                // Ping/Pong are answered by axum itself.
                _ => continue,
            };
            match rmp_serde::from_slice::<ClientRequest>(&bytes) {
                Ok(req) => {
                    if req_tx.send(req).await.is_err() {
                        break;
                    }
                }
                Err(e) => tracing::debug!(error = %e, "gateway: undecodable request"),
            }
        }
    });

    if let Err(e) = crate::server::serve_client(state.daemon.clone(), req_rx, out_tx).await {
        tracing::debug!(error = %e, "gateway client ended with error");
    }
    reader.abort();
    let _ = writer.await;
}

/// Whether `given` is the gateway's token. Constant time over the bytes,
/// and a missing token takes the same path as a wrong one — the answer
/// must not say which of the two it was, nor take a different amount of
/// time to say it.
///
/// The length check short-circuits, which leaks the token's length. That is
/// public knowledge (32 bytes, hex, stated here) and is not what an
/// attacker is missing.
fn token_ok(given: Option<&str>, expected: &str) -> bool {
    let given = given.unwrap_or("");
    given.len() == expected.len() && bool::from(given.as_bytes().ct_eq(expected.as_bytes()))
}

fn generate_token() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every way of not having the token is refused, and the real one is
    /// taken. The empty and missing cases matter most: a client that omits
    /// the parameter must not be told apart from one that guesses wrong.
    #[test]
    fn only_the_exact_token_opens_the_gateway() {
        let token = generate_token();
        assert!(token_ok(Some(&token), &token));
        assert!(!token_ok(None, &token));
        assert!(!token_ok(Some(""), &token));
        assert!(
            !token_ok(Some(&token[..token.len() - 1]), &token),
            "a prefix"
        );
        assert!(
            !token_ok(Some(&format!("{token}0")), &token),
            "an extension"
        );
        let mut wrong = token.clone();
        wrong.replace_range(0..1, if token.starts_with('a') { "b" } else { "a" });
        assert!(!token_ok(Some(&wrong), &token), "one byte off");
    }

    /// A fresh token every boot, of the documented shape.
    #[test]
    fn tokens_are_fresh_and_hex() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64, "32 bytes, hex");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// The handshake file is owner-only from the moment it exists, and
    /// unpublishing removes it: whoever can read it can run commands as
    /// this user, so the mode is the whole security boundary.
    #[test]
    fn the_handshake_file_is_owner_only_and_removable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        // `gateway_path` reads the runtime dir from the environment.
        std::env::set_var(nebula_core::env::RUNTIME_DIR, tmp.path());
        let info = GatewayInfo {
            port: 1234,
            token: generate_token(),
            protocol_version: 41,
        };
        publish(&info).unwrap();
        let path = nebula_core::paths::gateway_path();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "owner read/write only");
        let back: GatewayInfo =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.port, info.port);
        assert_eq!(back.token, info.token);
        // No temporary file is left beside it.
        assert!(!path.with_extension("json.tmp").exists());
        unpublish();
        assert!(!path.exists());
        std::env::remove_var(nebula_core::env::RUNTIME_DIR);
    }
}
