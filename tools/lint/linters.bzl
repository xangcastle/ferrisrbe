"""Linter aspects of the FerrisRBE workspace, wired into `bazel aspect lint`."""

load(":clippy.bzl", "lint_clippy_aspect")

clippy = lint_clippy_aspect(
    config = Label("//:clippy.toml"),
)
