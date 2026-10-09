"""Clippy as a linter of the aspect CLI (`bazel aspect lint`).

The aspect runs `rules_rust`'s own clippy action (`rust_clippy_action`, the
public API `rust_clippy_aspect` is built on) and publishes the result through
the output contract the aspect CLI reads from any linter:

- output groups `rules_lint_human` (terminal) and `rules_lint_machine` (CI);
- per report, a `.exit_code` file holding `0` or `1`;
- file names containing `AspectRulesLint`, which the CLI downloads from the
  remote cache and uses to name the failing tool.

Every crate with `CrateInfo` is visited (`rust_library`, `rust_binary`,
`rust_test`, ...) except external ones and those tagged `no-lint` or
`no-clippy`. A finding is any `warning:` or `error:` diagnostic: clippy runs
with `--cap-lints=warn` so that a finding is reported rather than aborting the
build, and the exit code is derived from the captured diagnostics. A hard
compile error still fails the action. Clippy produces no fixes, so there is
no `rules_lint_patch` output.
"""

load("@rules_rust//rust:defs.bzl", "rust_clippy_action", "rust_common")

_MNEMONIC = "AspectRulesLintClippy"

def _declare(ctx, target, suffix, sibling):
    return ctx.actions.declare_file("{}.{}.{}".format(target.label.name, _MNEMONIC, suffix), sibling = sibling)

def _clippy_aspect_impl(target, ctx):
    crate_info = rust_clippy_action.get_clippy_ready_crate_info(target, ctx)
    if not crate_info:
        return []

    sibling = crate_info.output
    clippy_out = _declare(ctx, target, "clippy", sibling)
    human = _declare(ctx, target, "out", sibling)
    human_exit_code = _declare(ctx, target, "out.exit_code", sibling)
    machine = _declare(ctx, target, "report", sibling)
    machine_exit_code = _declare(ctx, target, "report.exit_code", sibling)

    toolchain = ctx.toolchains[str(Label("@rules_rust//rust:toolchain_type"))]
    rust_clippy_action.action(
        ctx = ctx,
        clippy_executable = toolchain.clippy_driver,
        process_wrapper = ctx.executable._process_wrapper,
        crate_info = crate_info,
        config = ctx.file._config,
        output = clippy_out,
        cap_at_warnings = True,
        extra_clippy_flags = ctx.attr._extra_clippy_flags,
        error_format = "human",
    )

    ctx.actions.run_shell(
        inputs = [clippy_out],
        outputs = [human, human_exit_code, machine, machine_exit_code],
        command = """
set -eu
esc=$(printf '\\033')
cp "$1" "$2"
sed "s/${esc}\\[[0-9;]*m//g" "$1" > "$4"
code=0
grep -Eq '^(warning|error)(\\[|:)' "$4" && code=1
echo "$code" > "$3"
echo "$code" > "$5"
""",
        arguments = [f.path for f in [clippy_out, human, human_exit_code, machine, machine_exit_code]],
        mnemonic = _MNEMONIC,
        progress_message = "Reporting clippy findings for %{label}",
    )

    return [OutputGroupInfo(
        rules_lint_human = depset([human, human_exit_code]),
        rules_lint_machine = depset([machine, machine_exit_code]),
    )]

def lint_clippy_aspect(config, extra_flags = []):
    """A clippy linter aspect.

    Args:
        config: label of the `clippy.toml` (or `.clippy.toml`) to apply; clippy
            only honors those two file names.
        extra_flags: flags appended to every clippy invocation, after the
            crate's own `lint_config`. Without any, clippy uses its default
            lint groups.

    Returns:
        The aspect, to be referenced from the aspect CLI lint configuration.
    """
    return aspect(
        implementation = _clippy_aspect_impl,
        fragments = ["cpp"],
        attrs = {
            "_config": attr.label(
                default = config,
                allow_single_file = True,
            ),
            "_error_format": attr.label(
                default = "@rules_rust//rust/settings:error_format",
            ),
            "_extra_clippy_flags": attr.string_list(
                default = extra_flags,
            ),
            "_extra_rustc_flag": attr.label(
                default = "@rules_rust//rust/settings:extra_rustc_flag",
            ),
            "_per_crate_rustc_flag": attr.label(
                default = "@rules_rust//rust/settings:experimental_per_crate_rustc_flag",
            ),
            "_process_wrapper": attr.label(
                default = "@rules_rust//util/process_wrapper",
                executable = True,
                cfg = "exec",
            ),
        },
        required_providers = [
            [rust_common.crate_info],
            [rust_common.test_crate_info],
        ],
        toolchains = [
            str(Label("@rules_rust//rust:toolchain_type")),
            config_common.toolchain_type("@bazel_tools//tools/cpp:toolchain_type", mandatory = False),
        ],
    )
