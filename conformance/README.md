# REAPI conformance

There is no RPC-level conformance suite for the Remote Execution API. The
closest thing is [remote-apis-testing](https://gitlab.com/remote-apis-testing/remote-apis-testing),
a matrix that builds a small project with each REAPI client against each
server (Buildbarn, Buildfarm, BuildGrid) and records the exit code. This
directory runs the same clients, with the same flags and versions (`matrix.yml`
at 575ce53), against the local FerrisRBE:

| Client      | Builds                                  | Needs on the worker            |
| ----------- | --------------------------------------- | ------------------------------ |
| Bazel 9.2.0 | `abseil-hello` with a hermetic GCC      | nothing                        |
| recc 1.4.17 | `libcyaml`, each `gcc -c` sent remotely | `/usr/bin/gcc` and its headers |

Every client sends instance `remote-execution` and the platform
`OSFamily=linux`, `CHROOT=false`. As the matrix servers do, the server accepts
`CHROOT` (`RBE_EXTRA_PLATFORM_PROPERTIES`) and the worker declares
`CHROOT=false` among its labels. The matrix runs actions in its `build_env`
image (Ubuntu 26.04 with `build-essential`), so the local worker uses it too:

```bash
cd bazel/rbe
bazel run //conformance:run -- worker-image
RBE_EXTRA_PLATFORM_PROPERTIES=CHROOT \
  FERRIS_LOCAL_WORKER_LABELS=os=linux,arch=arm64,CHROOT=false \
  FERRIS_LOCAL_WORKER_IMAGE=localhost/ferrisrbe-conformance-worker \
  bazel run //local:up

# in another terminal
bazel run //conformance:run -- bazel
bazel run //conformance:run -- recc
```

The images are built for the podman host architecture, and the Bazel client
targets it (`aarch64` on Apple silicon and on the production workers) instead
of the matrix's `x86_64`. Bazel runs with `--noremote_accept_cached`, so
every run executes its actions again. Extra arguments go to Bazel:
`bazel run //conformance:run -- bazel --verbose_failures`.
