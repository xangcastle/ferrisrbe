//! BES gRPC service implementation.

use std::pin::Pin;

use prost::Message;
use tokio_stream::Stream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, info, warn};

use crate::bes::storage::BesStorage;
use crate::proto::build::bazel::build_event_stream::BuildEvent as BazelBuildEvent;
use crate::proto::google::devtools::build::v1::publish_build_event_server::{
    PublishBuildEvent, PublishBuildEventServer,
};
use crate::proto::google::devtools::build::v1::{
    build_event::Event, PublishBuildToolEventStreamRequest, PublishBuildToolEventStreamResponse,
    PublishLifecycleEventRequest,
};

/// BES gRPC service.
#[derive(Debug, Clone)]
pub struct BesService {
    storage: BesStorage,
}

impl BesService {
    /// Create a new BES service backed by the given storage.
    pub fn new(storage: BesStorage) -> Self {
        Self { storage }
    }

    /// Convert the service into a Tonic server.
    pub fn into_service(self) -> PublishBuildEventServer<Self> {
        let max_msg_size = std::env::var("RBE_MAX_GRPC_MSG_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100 * 1024 * 1024);
        PublishBuildEventServer::new(self)
            .max_decoding_message_size(max_msg_size)
            .max_encoding_message_size(max_msg_size)
    }
}

#[tonic::async_trait]
impl PublishBuildEvent for BesService {
    type PublishBuildToolEventStreamStream = Pin<
        Box<
            dyn Stream<Item = Result<PublishBuildToolEventStreamResponse, Status>>
                + Send
                + 'static,
        >,
    >;

    /// Receive a bidirectional stream of build tool events.
    async fn publish_build_tool_event_stream(
        &self,
        request: Request<Streaming<PublishBuildToolEventStreamRequest>>,
    ) -> Result<Response<Self::PublishBuildToolEventStreamStream>, Status> {
        let storage = self.storage.clone();
        let mut stream = request.into_inner();

        info!("BES PublishBuildToolEventStream started");

        let output_stream = async_stream::try_stream! {
            while let Some(message) = stream.message().await? {
                let Some(ordered_event) = message.ordered_build_event else {
                    continue;
                };

                let sequence_number = ordered_event.sequence_number;
                let invocation_id = ordered_event
                    .stream_id
                    .as_ref()
                    .map(|s| s.invocation_id.clone())
                    .unwrap_or_default();

                debug!(
                    "BES event invocation={} sequence={}",
                    invocation_id, sequence_number
                );

                match decode_bazel_build_event(&ordered_event) {
                    Some(event) => {
                        let is_final = event.last_message;
                        if let Err(e) = storage.store_event(&invocation_id, event).await {
                            error!("Failed to store BES event: {}", e);
                        }
                        yield PublishBuildToolEventStreamResponse {
                            stream_id: ordered_event.stream_id.clone(),
                            sequence_number,
                        };
                        if is_final {
                            if let Err(e) = storage.finalize(&invocation_id).await {
                                error!("Failed to finalize BES invocation: {}", e);
                            }
                            info!("BES invocation {} finalized", invocation_id);
                        }
                    }
                    None => {
                        warn!("Could not decode BazelBuildEvent for invocation {}", invocation_id);
                        yield PublishBuildToolEventStreamResponse {
                            stream_id: ordered_event.stream_id.clone(),
                            sequence_number,
                        };
                    }
                }
            }
        };

        Ok(Response::new(Box::pin(output_stream)))
    }

    /// Publish a lifecycle event.
    ///
    /// The MVP implementation accepts lifecycle events but does not persist
    /// them; the real build state is tracked through the build tool event
    /// stream.
    async fn publish_lifecycle_event(
        &self,
        _request: Request<PublishLifecycleEventRequest>,
    ) -> Result<Response<()>, Status> {
        debug!("BES PublishLifecycleEvent received (no-op)");
        Ok(Response::new(()))
    }
}

/// Decode the inner Bazel `BuildEvent` from an `OrderedBuildEvent`.
///
/// Bazel sends the BEP payload as a `google.protobuf.Any` inside the
/// `BuildEvent.bazel_event` field. The type URL is ignored for the MVP and the
/// raw bytes are decoded directly as a `build_event_stream.BuildEvent`.
fn decode_bazel_build_event(
    ordered: &crate::proto::google::devtools::build::v1::OrderedBuildEvent,
) -> Option<BazelBuildEvent> {
    let bes_event = ordered.event.as_ref()?;
    let any = match &bes_event.event {
        Some(Event::BazelEvent(any)) => any,
        _ => {
            debug!("BES event does not contain a BazelEvent payload; skipping");
            return None;
        }
    };

    if any.type_url.ends_with("/build_event_stream.BuildEvent")
        || any.type_url.ends_with("/BuildEvent")
        || any.type_url.is_empty()
    {
        return BazelBuildEvent::decode(any.value.as_ref()).ok();
    }

    warn!("Unexpected BES event type_url: {}", any.type_url);
    None
}
