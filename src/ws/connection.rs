use crate::auth::Credentials;
use crate::api::client::WebSocketUrlResponse;
use crate::ws::events::{WsEnvelope, WsEvent, WsUpdate};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use std::time::Duration;
use tracing::{error, info};

/// Slack WebSocket client for real-time events.
///
/// Connects to `wss://wss-primary.slack.com` using the xoxc token
/// and xoxd cookie for authentication. Receives real-time events
/// (messages, presence, typing indicators, call signals).
pub struct Client {
    creds: Credentials,
    ws_url: WebSocketUrlResponse,
    filter: Option<String>,
    /// Channel for dispatching received events
    event_tx: Option<tokio::sync::mpsc::Sender<WsUpdate>>,
    /// Write half of the WebSocket connection
    write_tx: Option<Arc<Mutex<tokio::sync::mpsc::UnboundedSender<Message>>>>,
    /// Users whose presence to follow, updatable while connected.
    presence: Option<tokio::sync::watch::Receiver<Vec<String>>>,
}

impl Client {
    pub fn new(creds: Credentials, ws_url: WebSocketUrlResponse, filter: Option<String>) -> Self {
        Client {
            creds,
            ws_url,
            filter,
            event_tx: None,
            write_tx: None,
            presence: None,
        }
    }

    /// Follows the presence of the given users, readjusting when the list
    /// changes — the sidebar does not always show the same people.
    pub fn set_presence_source(
        &mut self,
        rx: tokio::sync::watch::Receiver<Vec<String>>,
    ) {
        self.presence = Some(rx);
    }

    /// Set the event channel for dispatching.
    pub fn set_event_channel(&mut self, tx: tokio::sync::mpsc::Sender<WsUpdate>) {
        self.event_tx = Some(tx);
    }

    /// Connect and listen for events.
    pub async fn listen(&mut self) -> Result<()> {
        let api_client = crate::api::Client::new(self.creds.clone());
        let full_url = api_client.build_ws_url(&self.ws_url)?;

        info!("Connecting to WebSocket...");

        let mut request = full_url
            .into_client_request()
            .context("Failed to build WebSocket request")?;
        let cookie_val = format!("d={}", self.creds.xoxd);
        request
            .headers_mut()
            .insert("Cookie", cookie_val.parse().context("Invalid cookie value")?);
        let (ws_stream, response) = connect_async(request)
            .await
            .context("Failed to connect to Slack WebSocket")?;

        info!(
            "WebSocket connected: HTTP {}",
            response.status()
        );

        if let Some(ref tx) = self.event_tx {
            let _ = tx.send(WsUpdate::Connected).await;
        }

        let (write, read) = ws_stream.split();

        let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let write_tx = Arc::new(Mutex::new(msg_tx));
        self.write_tx = Some(write_tx.clone());



        // Spawn write task
        let _write_handle = tokio::spawn(async move {
            let mut write = write;
            while let Some(msg) = msg_rx.recv().await {
                if let Err(e) = write.send(msg).await {
                    error!("WebSocket write error: {e}");
                    break;
                }
            }
        });

        // Read loop
        let presence = self.presence.clone();

        // When the list changes the subscription is resent: `presence_sub`
        // replaces the previous one, it does not add to it.
        let watcher = presence.clone().map(|mut rx| {
            let write_tx = write_tx.clone();
            tokio::spawn(async move {
                while rx.changed().await.is_ok() {
                    let ids = rx.borrow_and_update().clone();
                    send_presence_sub(&write_tx, &ids).await;
                }
            })
        });

        let filter = self.filter.clone();
        let event_tx = self.event_tx.clone();
        let mut read = read;

        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    // Handle Slack-specific ping frames
                    if text == r#"{"type":"ping"}"# {
                        let pong = Message::Text(r#"{"type":"pong"}"#.into());
                        let _ = write_tx.lock().await.send(pong);
                        continue;
                    }

                    match serde_json::from_str::<WsEvent>(&text) {
                        Ok(event) => {
                            // The subscription has to be sent **after** `hello`:
                            // before the handshake the gateway drops it in
                            // silence, without even an error.
                            if event.event_type == "hello" {
                                if let Some(rx) = &presence {
                                    let ids = rx.borrow().clone();
                                    send_presence_sub(&write_tx, &ids).await;
                                }
                            }
                            if let Some(ref filter) = filter {
                                if &event.event_type != filter {
                                    continue;
                                }
                            }

                            if let Some(ref tx) = event_tx {
                                let raw = serde_json::from_str(&text)
                                    .unwrap_or(serde_json::Value::Null);
                                if tx
                                    .send(WsUpdate::Event(WsEnvelope { event, raw }))
                                    .await
                                    .is_err()
                                {
                                    // The consumer is gone: no point
                                    // reading on.
                                    break;
                                }
                            } else {
                                // Default: print to stdout
                                println!("[{}] {}", event.event_type, text);
                            }
                        }
                        Err(e) => {
                            tracing::debug!("Failed to parse event: {e}: {text}");
                        }
                    }
                }
                Ok(Message::Ping(data)) => {
                    let _ = write_tx.lock().await.send(Message::Pong(data));
                }
                Ok(Message::Close(frame)) => {
                    info!("WebSocket closed: {frame:?}");
                    break;
                }
                Err(e) => {
                    error!("WebSocket error: {e}");
                    break;
                }
                _ => {}
            }
        }

        if let Some(w) = watcher {
            // Without this a task would be left for every reconnection, all of
            // them writing to sockets that are closed by now.
            w.abort();
        }

        info!("WebSocket disconnected");
        Ok(())
    }
}

/// Sends `presence_sub` for the given users.
///
/// The answer is batched — `{"type":"presence_change","presence":"away",
/// "users":[…]}` — because the socket URL carries `batch_presence_aware=1`.
/// A snapshot of every subscriber arrives at once, then the changes.
async fn send_presence_sub(
    write_tx: &Arc<Mutex<tokio::sync::mpsc::UnboundedSender<Message>>>,
    ids: &[String],
) {
    if ids.is_empty() {
        return;
    }
    let sub = serde_json::json!({ "type": "presence_sub", "ids": ids });
    info!("presence_sub for {} users", ids.len());
    let _ = write_tx
        .lock()
        .await
        .send(Message::Text(sub.to_string().into()));
}

/// Listens forever, reconnecting when the socket drops.
///
/// Slack closes RTM connections regularly and the URL obtained from
/// `get_websocket_url` is single-use: a new one is requested on every attempt.
/// The backoff is exponential with a ceiling, so as not to hammer the API when
/// the network is down — it is also the kind of traffic that gets tokens
/// invalidated.
pub async fn listen_forever(
    creds: Credentials,
    filter: Option<String>,
    tx: tokio::sync::mpsc::Sender<WsUpdate>,
    presence: Option<tokio::sync::watch::Receiver<Vec<String>>>,
) {
    const BACKOFF_START: Duration = Duration::from_secs(1);
    const BACKOFF_MAX: Duration = Duration::from_secs(60);

    let mut backoff = BACKOFF_START;

    loop {
        let api = crate::api::Client::new(creds.clone());

        let attempt = match api.get_websocket_url().await {
            Ok(ws_url) => {
                let mut client = Client::new(creds.clone(), ws_url, filter.clone());
                client.set_event_channel(tx.clone());
                // On every reconnection the subscription has to be redone: it lives
                // in the socket, not in the account.
                if let Some(rx) = presence.clone() {
                    client.set_presence_source(rx);
                }
                client.listen().await
            }
            Err(e) => Err(e),
        };

        let reason = match attempt {
            Ok(()) => {
                // Clean close: we start again from a short backoff.
                backoff = BACKOFF_START;
                "connection closed by Slack".to_string()
            }
            Err(e) => format!("{e}"),
        };

        if tx.send(WsUpdate::Disconnected(reason)).await.is_err() {
            // Nobody is listening any more: the UI is closed.
            return;
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}
