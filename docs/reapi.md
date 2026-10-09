# REAPI support

What FerrisRBE implements of the Remote Execution API v2 and the Remote
Asset API, and how it behaves where the spec leaves the choice to the
server. [`conformance/`](../conformance/README.md) checks it against the
clients of remote-apis-testing (Bazel 9.2, recc).

## Capabilities

| Field                              | Value                                     |
| ---------------------------------- | ----------------------------------------- |
| API versions                       | 2.0 to 2.4                                |
| Digest function                    | SHA-256 only                              |
| Compressors                        | `IDENTITY` only (no `compressed-blobs/`)  |
| `max_batch_total_size_bytes`       | 4 MiB; larger blobs go through ByteStream |
| Symlink absolute paths             | allowed                                   |
| `UpdateActionResult`               | enabled                                   |
| `SplitBlob` / `SpliceBlob`         | not supported                             |
| Execution and cache priority range | 0 to 1000                                 |

## `instance_name`

Accepted on every RPC and otherwise ignored: every instance shares one CAS
and one Action Cache. A result written under `remote-execution` is a hit
for `other` and for `""`.

## Execution

- **Platform.** `OSFamily`, `ISA` (`arm-a64`, `x86-64`) and `Pool` select
  workers by label; `Priority=critical` queues before everything else;
  `Network` (`on`/`off`) sets the action's network access. The names in
  `RBE_EXTRA_PLATFORM_PROPERTIES` require the worker label
  `<name>=<value>` verbatim (`CHROOT=false`). Any other property is
  `INVALID_ARGUMENT`. `Action.platform` wins over the deprecated
  `Command.platform`.
- **`ExecutionPolicy.priority`.** Clamped to 0..1000; lower runs sooner.
  The queues are fair across invocations (`tool_invocation_id`): each 100
  points halve an invocation's share against one 100 points lower. There
  is no preemption: a running action is never stopped for a more urgent
  one.
- **`ResultsCachePolicy.priority`.** Accepted and ignored.
- **Cache.** `skip_cache_lookup` and `do_not_cache` are honored. A hit is
  served only when every blob it references is still in the CAS; otherwise
  it is dropped and the action runs. Only a zero exit without error is
  written to the Action Cache, once per execution, whether or not any
  client is still listening.
- **Identical actions.** An `Execute` of a digest already in flight joins
  the running operation instead of running it again.
- **Outputs.** `output_paths` (Bazel 7+) and the deprecated
  `output_files`/`output_directories`. Symlinks come back as
  `output_symlinks`.
- **stdout and stderr.** Up to 1 MiB inline (`RBE_INLINE_OUTPUT_THRESHOLD`),
  larger by digest. The worker keeps the first 100 MiB
  (`RBE_MAX_CAPTURE_SIZE`) and appends a truncation notice.
- **Status.** `OK` for any action that ran, whatever its exit code;
  `DEADLINE_EXCEEDED` when the worker killed it at its timeout, with the
  partial result; `INTERNAL` when it could not run. An action that lost
  more than `RBE_MAX_WORKER_LOSSES` (3) workers fails.
- **Operations.** `Execute` streams `done: false` every 500 ms and the
  result as soon as it exists; `WaitExecution` on `operations/<id>` gets
  the same response. A queued operation no client follows for
  `RBE_UNWATCHED_OPERATION_TIMEOUT_SECS` (60 s) is cancelled; a running one
  finishes.

## CAS and ByteStream

- `FindMissingBlobs`, `BatchUpdateBlobs`, `BatchReadBlobs`, and `GetTree`
  with paging (`page_size`, 1000 by default); a missing child directory is
  left out, a missing root is `NOT_FOUND`.
- `ByteStream.Read` honors `read_offset` and `read_limit`.
- `ByteStream.Write` verifies the digest, answers after the request with
  `finish_write` without waiting for the client to close its side, and
  accepts `write_offset` 0, or the full size of a blob already stored.
  `QueryWriteStatus` reports 0 until a blob is complete, so there is no
  partial resume.

## Remote Asset

- `FetchBlob` downloads `http(s)` URIs through the pull-through proxy and
  stores the blob in the CAS. It honors the `checksum.sri` (SHA-256)
  qualifier and `http_header:<name>` / `http_header_url:<index>:<name>`.
  When every URI fails, the reason is in the response `status`, so Bazel
  falls back to downloading itself.
- `FetchDirectory` is `UNIMPLEMENTED`, and the `Push` service is not
  served; Bazel uses neither.
