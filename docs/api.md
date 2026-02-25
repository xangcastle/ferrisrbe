# API Reference

FerrisRBE implements the [Bazel Remote Execution API v2.4](https://github.com/bazelbuild/remote-apis).

## REAPI Services

### Execution Service

Manages action execution lifecycle.

```protobuf
service Execution {
  rpc Execute(ExecuteRequest) returns (stream google.longrunning.Operation);
  rpc WaitExecution(WaitExecutionRequest) returns (stream google.longrunning.Operation);
}
```

### ContentAddressableStorage

Stores and retrieves blobs by content digest.

```protobuf
service ContentAddressableStorage {
  rpc FindMissingBlobs(FindMissingBlobsRequest) returns (FindMissingBlobsResponse);
  rpc BatchUpdateBlobs(BatchUpdateBlobsRequest) returns (BatchUpdateBlobsResponse);
  rpc BatchReadBlobs(BatchReadBlobsRequest) returns (BatchReadBlobsResponse);
  rpc GetTree(GetTreeRequest) returns (stream GetTreeResponse);
}
```

### ActionCache

Caches action results by action digest.

```protobuf
service ActionCache {
  rpc GetActionResult(GetActionResultRequest) returns (ActionResult);
  rpc UpdateActionResult(UpdateActionResultRequest) returns (ActionResult);
}
```

### ByteStream

Streaming upload/download for large blobs.

```protobuf
service ByteStream {
  rpc Read(ReadRequest) returns (stream ReadResponse);
  rpc Write(stream WriteRequest) returns (WriteResponse);
}
```

### Capabilities

Server capability discovery.

```protobuf
service Capabilities {
  rpc GetCapabilities(GetCapabilitiesRequest) returns (ServerCapabilities);
}
```

## Worker Service (Custom)

Bidirectional streaming for worker management.

```protobuf
service WorkerService {
  rpc StreamWork(stream WorkerMessage) returns (stream ServerMessage);
}

message WorkerMessage {
  oneof message {
    WorkerRegistration registration = 1;
    WorkerHeartbeat heartbeat = 2;
    ExecutionResult result = 3;
  }
}

message ServerMessage {
  oneof message {
    RegistrationAck ack = 1;
    WorkAssignment assignment = 2;
    CancelExecution cancel = 3;
  }
}
```

## Protocol Buffer Definitions

Located in `proto/`:

- `build/bazel/remote/execution/v2/remote_execution.proto` - REAPI v2.4
- `google/bytestream/bytestream.proto` - ByteStream API
- `worker.proto` - WorkerService API

## Example gRPC Calls

### Get Capabilities

```bash
grpcurl -plaintext localhost:9092 \
  build.bazel.remote.execution.v2.Capabilities/GetCapabilities
```

### Find Missing Blobs

```bash
grpcurl -plaintext -d '{
  "instance_name": "",
  "blob_digests": [
    {"hash": "abc123...", "size_bytes": 1024}
  ]
}' localhost:9092 \
  build.bazel.remote.execution.v2.ContentAddressableStorage/FindMissingBlobs
```
