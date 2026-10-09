//! Live channel for the UI: a broadcast bus fed by BES ingestion and the
//! worker registry, served over one WebSocket per browser tab.
//!
//! A client subscribes to topics and receives deltas; the REST API remains
//! the source of snapshots. A client that falls behind the bus receives
//! [`LiveEvent::Resync`] and refetches its snapshot.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::bes::models::{BuildListItem, TargetExecution};
use crate::worker::k8s::{WorkerRegistry, WorkerView};

/// Events buffered per subscriber before it is considered lagged.
const BUS_CAPACITY: usize = 8_192;
/// Interval of WebSocket pings that keep idle connections open through proxies.
const PING_INTERVAL: Duration = Duration::from_secs(20);
/// Interval at which the worker registry is compared for changes.
const WORKERS_POLL: Duration = Duration::from_secs(2);

/// A delta pushed to live clients, serialized as `{"type": "<snake_case>", ...}`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LiveEvent {
    /// A build's listing entry changed.
    BuildUpdated { build: Box<BuildListItem> },
    /// A target of a running build finished.
    TargetCompleted {
        invocation_id: String,
        label: String,
        success: bool,
    },
    /// Final target list with the ActionCache join, once the build is indexed.
    TargetsUpdated {
        invocation_id: String,
        targets: Vec<TargetExecution>,
    },
    /// Console lines from `from` to the end; the client replaces its tail.
    LogAppended {
        invocation_id: String,
        from: usize,
        lines: Vec<String>,
    },
    /// An action of a running build failed; `action` is its BEP payload.
    ActionFailed {
        invocation_id: String,
        action: Value,
    },
    /// The full worker list, sent whenever it changes.
    WorkersUpdated { workers: Vec<WorkerView> },
    /// The client fell behind the bus and must refetch its snapshot.
    Resync,
    /// Reply to a client `ping`.
    Pong,
    /// A client message was rejected.
    Error { message: String },
}

impl LiveEvent {
    /// The build the event belongs to, for events scoped to one build.
    fn invocation_id(&self) -> Option<&str> {
        match self {
            LiveEvent::BuildUpdated { build } => Some(&build.invocation_id),
            LiveEvent::TargetCompleted { invocation_id, .. }
            | LiveEvent::TargetsUpdated { invocation_id, .. }
            | LiveEvent::LogAppended { invocation_id, .. }
            | LiveEvent::ActionFailed { invocation_id, .. } => Some(invocation_id),
            _ => None,
        }
    }
}

/// Broadcast bus shared by every event producer and every live client.
#[derive(Debug, Clone)]
pub struct LiveBus {
    tx: broadcast::Sender<LiveEvent>,
}

impl Default for LiveBus {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BUS_CAPACITY);
        Self { tx }
    }

    /// Broadcasts `event` to every connected client. With no client connected
    /// the event is discarded; the bus keeps no history.
    pub fn publish(&self, event: LiveEvent) {
        let _ = self.tx.send(event);
    }

    /// A receiver for every event published from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<LiveEvent> {
        self.tx.subscribe()
    }

    /// Publishes the worker list every time it differs from the previous poll.
    pub fn watch_workers(&self, registry: Arc<WorkerRegistry>) {
        let bus = self.clone();
        tokio::spawn(async move {
            let mut last: Vec<WorkerView> = Vec::new();
            let mut interval = tokio::time::interval(WORKERS_POLL);
            loop {
                interval.tick().await;
                let workers = registry.snapshot();
                if workers != last {
                    last = workers.clone();
                    bus.publish(LiveEvent::WorkersUpdated { workers });
                }
            }
        });
    }
}

/// A stream of events a client can subscribe to.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Topic {
    Builds,
    Workers,
    Build { id: String },
}

/// A message sent by a live client.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClientMessage {
    Subscribe(Topic),
    Unsubscribe(Topic),
    Ping,
    /// An operational command such as retry or cancel. No command is
    /// supported; each one is answered with [`LiveEvent::Error`].
    Command {
        name: String,
    },
}

/// The topics one client is subscribed to.
#[derive(Default)]
struct Subscriptions {
    builds: bool,
    workers: bool,
    build_ids: HashSet<String>,
}

impl Subscriptions {
    fn apply(&mut self, topic: Topic, on: bool) {
        match topic {
            Topic::Builds => self.builds = on,
            Topic::Workers => self.workers = on,
            Topic::Build { id } => {
                if on {
                    self.build_ids.insert(id);
                } else {
                    self.build_ids.remove(&id);
                }
            }
        }
    }

    /// Whether `event` belongs to a subscribed topic. Control events
    /// (`resync`, `pong`, `error`) always pass.
    fn wants(&self, event: &LiveEvent) -> bool {
        match event {
            LiveEvent::BuildUpdated { build } => {
                self.builds || self.build_ids.contains(&build.invocation_id)
            }
            LiveEvent::WorkersUpdated { .. } => self.workers,
            LiveEvent::Resync | LiveEvent::Pong | LiveEvent::Error { .. } => true,
            other => other
                .invocation_id()
                .is_some_and(|id| self.build_ids.contains(id)),
        }
    }
}

#[derive(Clone)]
struct LiveState {
    bus: LiveBus,
    registry: Arc<WorkerRegistry>,
}

/// The `/api/ws` WebSocket endpoint.
pub fn router(bus: LiveBus, registry: Arc<WorkerRegistry>) -> Router {
    Router::new()
        .route("/api/ws", get(upgrade))
        .with_state(LiveState { bus, registry })
}

async fn upgrade(ws: WebSocketUpgrade, State(state): State<LiveState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| serve(socket, state))
}

/// `event` as a JSON text frame.
fn text(event: &LiveEvent) -> Message {
    Message::Text(serde_json::to_string(event).unwrap_or_default().into())
}

/// Runs one client connection until either side closes it: applies client
/// messages, forwards subscribed bus events and pings every
/// [`PING_INTERVAL`]. The interval's immediate first tick is consumed up
/// front, so the first ping goes out one full interval after connecting.
async fn serve(socket: WebSocket, state: LiveState) {
    let (mut sink, mut stream) = socket.split();
    let mut events = state.bus.subscribe();
    let mut subs = Subscriptions::default();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await;

    loop {
        tokio::select! {
            incoming = stream.next() => {
                let Some(Ok(message)) = incoming else { break };
                let reply = match message {
                    Message::Text(body) => handle_client_message(&body, &mut subs, &state),
                    Message::Close(_) => break,
                    _ => None,
                };
                if let Some(reply) = reply {
                    if sink.send(text(&reply)).await.is_err() {
                        break;
                    }
                }
            }
            event = events.recv() => {
                let outgoing = match event {
                    Ok(event) if subs.wants(&event) => Some(event),
                    Ok(_) => None,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("live client lagged {} events; asking it to resync", n);
                        Some(LiveEvent::Resync)
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if let Some(event) = outgoing {
                    if sink.send(text(&event)).await.is_err() {
                        break;
                    }
                }
            }
            _ = ping.tick() => {
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    debug!("live client disconnected");
}

/// Applies one client message and returns the reply to send, if any.
/// Subscribing to `workers` replies with the current list at once, so the
/// client does not wait for the next change.
fn handle_client_message(
    body: &str,
    subs: &mut Subscriptions,
    state: &LiveState,
) -> Option<LiveEvent> {
    let message: ClientMessage = match serde_json::from_str(body) {
        Ok(m) => m,
        Err(e) => {
            return Some(LiveEvent::Error {
                message: format!("unrecognized message: {}", e),
            })
        }
    };
    match message {
        ClientMessage::Subscribe(topic) => {
            let workers = matches!(topic, Topic::Workers);
            subs.apply(topic, true);
            workers.then(|| LiveEvent::WorkersUpdated {
                workers: state.registry.snapshot(),
            })
        }
        ClientMessage::Unsubscribe(topic) => {
            subs.apply(topic, false);
            None
        }
        ClientMessage::Ping => Some(LiveEvent::Pong),
        ClientMessage::Command { name } => Some(LiveEvent::Error {
            message: format!("command '{}' is not supported", name),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriptions_filter_events_by_topic() {
        let mut subs = Subscriptions::default();
        let build = BuildListItem {
            invocation_id: "inv".into(),
            ..Default::default()
        };
        let updated = LiveEvent::BuildUpdated {
            build: Box::new(build.clone()),
        };
        let log = LiveEvent::LogAppended {
            invocation_id: "inv".into(),
            from: 0,
            lines: vec![],
        };
        assert!(!subs.wants(&updated));
        subs.apply(Topic::Builds, true);
        assert!(subs.wants(&updated));
        assert!(!subs.wants(&log));
        subs.apply(Topic::Build { id: "inv".into() }, true);
        assert!(subs.wants(&log));
        subs.apply(Topic::Build { id: "inv".into() }, false);
        assert!(!subs.wants(&log));
        assert!(subs.wants(&LiveEvent::Resync));
    }

    #[test]
    fn client_messages_parse_from_json() {
        let m: ClientMessage =
            serde_json::from_str(r#"{"subscribe":{"build":{"id":"x"}}}"#).unwrap();
        assert!(matches!(m, ClientMessage::Subscribe(Topic::Build { id }) if id == "x"));
        let m: ClientMessage = serde_json::from_str(r#"{"subscribe":"builds"}"#).unwrap();
        assert!(matches!(m, ClientMessage::Subscribe(Topic::Builds)));
        let m: ClientMessage = serde_json::from_str(r#""ping""#).unwrap();
        assert!(matches!(m, ClientMessage::Ping));
    }
}
