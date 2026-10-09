//! Access to the REAPI `RequestMetadata` that Bazel attaches to every call as
//! a binary gRPC header: the invocation, the owning target and the action
//! mnemonic.

use prost::Message;
use tonic::Request;

use crate::proto::build::bazel::remote::execution::v2::RequestMetadata;

/// The binary metadata key carrying the encoded `RequestMetadata`.
pub const HEADER: &str = "build.bazel.remote.execution.v2.requestmetadata-bin";

/// The request's `RequestMetadata`; `None` when the header is absent or does
/// not decode.
pub fn request_metadata<T>(request: &Request<T>) -> Option<RequestMetadata> {
    let bytes = request.metadata().get_bin(HEADER)?.to_bytes().ok()?;
    RequestMetadata::decode(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::metadata::MetadataValue;

    #[test]
    fn decodes_the_binary_header() {
        let meta = RequestMetadata {
            tool_invocation_id: "inv-1".into(),
            target_id: "//pkg:target".into(),
            action_mnemonic: "GoCompilePkg".into(),
            ..Default::default()
        };
        let mut request = Request::new(());
        request
            .metadata_mut()
            .insert_bin(HEADER, MetadataValue::from_bytes(&meta.encode_to_vec()));

        let decoded = request_metadata(&request).expect("header decodes");
        assert_eq!(decoded.tool_invocation_id, "inv-1");
        assert_eq!(decoded.target_id, "//pkg:target");
        assert_eq!(decoded.action_mnemonic, "GoCompilePkg");
        assert!(request_metadata(&Request::new(())).is_none());
    }
}
