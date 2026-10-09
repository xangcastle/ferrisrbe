//! Build Event Service (`google.devtools.build.v1.PublishBuildEvent`).
//!
//! Receives the build tool event stream Bazel publishes with `--bes_backend`,
//! hands every BEP event to [`BesStorage`] and acknowledges each one.

use std::pin::Pin;

use prost::Message;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::Stream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, info, warn};

use crate::bes::config::is_invocation_id;
use crate::bes::storage::BesStorage;
use crate::proto::build::bazel::build_event_stream::BuildEvent as BazelBuildEvent;
use crate::proto::google::devtools::build::v1::publish_build_event_server::{
    PublishBuildEvent, PublishBuildEventServer,
};
use crate::proto::google::devtools::build::v1::{
    build_event::Event, PublishBuildToolEventStreamRequest, PublishBuildToolEventStreamResponse,
    PublishLifecycleEventRequest,
};

/// Message size limit, in bytes, when `RBE_MAX_GRPC_MSG_SIZE` is unset.
const DEFAULT_MAX_GRPC_MESSAGE_BYTES: usize = 100 * 1024 * 1024;

/// BES gRPC service.
#[derive(Debug, Clone)]
pub struct BesService {
    storage: BesStorage,
}

impl BesService {
    /// Creates a BES service backed by `storage`.
    pub fn new(storage: BesStorage) -> Self {
        Self { storage }
    }

    /// The Tonic server for this service, with encoding and decoding limits
    /// taken from `RBE_MAX_GRPC_MSG_SIZE`.
    pub fn into_service(self) -> PublishBuildEventServer<Self> {
        let max_msg_size = std::env::var("RBE_MAX_GRPC_MSG_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_GRPC_MESSAGE_BYTES);
        PublishBuildEventServer::new(self)
            .max_decoding_message_size(max_msg_size)
            .max_encoding_message_size(max_msg_size)
    }
}

#[tonic::async_trait]
impl PublishBuildEvent for BesService {
    type PublishBuildToolEventStreamStream = Pin<
        Box<
            dyn Stream<Item = Result<PublishBuildToolEventStreamResponse, Status>> + Send + 'static,
        >,
    >;

    /// Consumes a build tool event stream and acknowledges every event, in
    /// order, including those it cannot decode or does not store.
    ///
    /// Bazel retries the stream when an acknowledgement is missing and
    /// aspect-cli resends every unacknowledged event, so no ACK may be
    /// dropped. The ACK channel is therefore unbounded: reading the next
    /// event never waits on the client draining ACKs.
    async fn publish_build_tool_event_stream(
        &self,
        request: Request<Streaming<PublishBuildToolEventStreamRequest>>,
    ) -> Result<Response<Self::PublishBuildToolEventStreamStream>, Status> {
        let storage = self.storage.clone();
        let mut stream = request.into_inner();

        info!("BES PublishBuildToolEventStream started");

        let (ack_tx, ack_rx) =
            mpsc::unbounded_channel::<Result<PublishBuildToolEventStreamResponse, Status>>();

        tokio::spawn(async move {
            while let Ok(Some(message)) = stream.message().await {
                let Some(ordered_event) = message.ordered_build_event else {
                    continue;
                };

                let sequence_number = ordered_event.sequence_number;
                let invocation_id = ordered_event
                    .stream_id
                    .as_ref()
                    .map(|s| s.invocation_id.clone())
                    .unwrap_or_default();
                if !is_invocation_id(&invocation_id) {
                    warn!("Refusing BES stream of invocation {:?}", invocation_id);
                    let _ = ack_tx.send(Err(Status::invalid_argument(format!(
                        "invalid invocation_id {invocation_id:?}"
                    ))));
                    break;
                }

                debug!(
                    "BES event invocation={} sequence={}",
                    invocation_id, sequence_number
                );

                match classify(&ordered_event) {
                    ToolEvent::Bazel(event) => {
                        if let Err(e) = storage.store_event(&invocation_id, *event).await {
                            error!("Failed to store BES event: {}", e);
                        }
                    }
                    ToolEvent::StreamFinished => finalize(&storage, &invocation_id).await,
                    ToolEvent::Undecodable(reason) => {
                        warn!(
                            "Dropping BES event {} of invocation {}: {}",
                            sequence_number, invocation_id, reason
                        );
                    }
                    ToolEvent::Ignored => {}
                }
                let _ = ack_tx.send(Ok(PublishBuildToolEventStreamResponse {
                    stream_id: ordered_event.stream_id.clone(),
                    sequence_number,
                }));
            }
        });

        Ok(Response::new(Box::pin(UnboundedReceiverStream::new(
            ack_rx,
        ))))
    }

    /// Accepts a lifecycle event without persisting it. Build state is derived
    /// entirely from the build tool event stream.
    async fn publish_lifecycle_event(
        &self,
        _request: Request<PublishLifecycleEventRequest>,
    ) -> Result<Response<()>, Status> {
        debug!("BES PublishLifecycleEvent received (no-op)");
        Ok(Response::new(()))
    }
}

/// What one message of the build tool event stream carries.
enum ToolEvent {
    /// A decoded BEP event.
    Bazel(Box<BazelBuildEvent>),
    /// The end of the invocation's stream.
    StreamFinished,
    /// A BEP payload that could not be decoded, with the reason.
    Undecodable(String),
    /// A message with no BEP content.
    Ignored,
}

/// Marks the invocation complete in storage, logging the outcome.
async fn finalize(storage: &BesStorage, invocation_id: &str) {
    match storage.finalize(invocation_id).await {
        Ok(()) => info!("BES invocation {} finalized", invocation_id),
        Err(e) => error!("Failed to finalize BES invocation {}: {}", invocation_id, e),
    }
}

/// Classifies one ordered event. Bazel wraps each BEP event as a
/// `google.protobuf.Any` in `bazel_event` and closes the tool stream with a
/// `component_stream_finished` message.
fn classify(ordered: &crate::proto::google::devtools::build::v1::OrderedBuildEvent) -> ToolEvent {
    let Some(bes_event) = ordered.event.as_ref() else {
        return ToolEvent::Ignored;
    };
    match &bes_event.event {
        Some(Event::BazelEvent(any)) => {
            if !(any.type_url.ends_with("/build_event_stream.BuildEvent")
                || any.type_url.is_empty())
            {
                return ToolEvent::Undecodable(format!("unexpected type_url {}", any.type_url));
            }
            match BazelBuildEvent::decode(any.value.as_ref()) {
                Ok(event) => ToolEvent::Bazel(Box::new(event)),
                Err(e) => ToolEvent::Undecodable(format!("build_event_stream.BuildEvent: {}", e)),
            }
        }
        Some(Event::ComponentStreamFinished(_)) => ToolEvent::StreamFinished,
        Some(other) => {
            debug!("Ignoring non-BEP tool event: {:?}", other);
            ToolEvent::Ignored
        }
        None => ToolEvent::Ignored,
    }
}
