"""GCC toolchain configuration for Linux remote execution."""

def _gcc_toolchain_impl(ctx):
    toolchain_info = ctx.attr.toolchain_config
    return [toolchain_info]

gcc_toolchain = rule(
    implementation = _gcc_toolchain_impl,
    attrs = {
        "toolchain_config": attr.label(mandatory = True),
    },
)

# Toolchain configuration for GCC on Linux ARM64
gcc_linux_arm64_toolchain_config = repository_rule(
    implementation = lambda ctx: _create_gcc_toolchain_config(ctx, "aarch64-linux-gnu"),
    local = True,
)

gcc_linux_amd64_toolchain_config = repository_rule(
    implementation = lambda ctx: _create_gcc_toolchain_config(ctx, "x86_64-linux-gnu"),
    local = True,
)

def _create_gcc_toolchain_config(rctx, target_cpu):
    """Generate a CC toolchain configuration for GCC on Linux."""
    rctx.file("WORKSPACE", "workspace(name = \"%s\")" % rctx.name)
    rctx.file("BUILD.bazel", """
package(default_visibility = ["//visibility:public"])

load("@bazel_tools//tools/cpp:cc_toolchain_config_lib.bzl", "action_config", "feature", "flag_group", "flag_set", "tool", "tool_path", "with_feature_set")

tool_path(
    name = "gcc",
    path = "/usr/bin/gcc",
)

tool_path(
    name = "ld",
    path = "/usr/bin/ld",
)

tool_path(
    name = "ar",
    path = "/usr/bin/ar",
)

tool_path(
    name = "cpp",
    path = "/usr/bin/cpp",
)

tool_path(
    name = "gcov",
    path = "/usr/bin/gcov",
)

tool_path(
    name = "nm",
    path = "/usr/bin/nm",
)

tool_path(
    name = "objdump",
    path = "/usr/bin/objdump",
)

tool_path(
    name = "strip",
    path = "/usr/bin/strip",
)

cc_toolchain_config(
    name = "gcc_linux_toolchain_config",
    cpu = "{target_cpu}",
)

cc_toolchain(
    name = "gcc_linux_toolchain",
    toolchain_identifier = "gcc-linux-toolchain",
    toolchain_config = ":gcc_linux_toolchain_config",
    all_files = ":empty",
    ar_files = ":empty",
    as_files = ":empty",
    compiler_files = ":empty",
    dwp_files = ":empty",
    linker_files = ":empty",
    objcopy_files = ":empty",
    strip_files = ":empty",
    supports_param_files = 0,
)

filegroup(name = "empty")

# Exports the toolchain config function
exports_files(["gcc_toolchain_config.bzl"])
""".format(target_cpu = target_cpu))
    
    # Create the toolchain config bzl file
    rctx.file("gcc_toolchain_config.bzl", '''
load("@bazel_tools//tools/cpp:cc_toolchain_config_lib.bzl", "action_config", "feature", "flag_group", "flag_set", "tool", "tool_path")

def _impl(ctx):
    tool_paths = [
        tool_path(name = "gcc", path = "/usr/bin/gcc"),
        tool_path(name = "ld", path = "/usr/bin/ld"),
        tool_path(name = "ar", path = "/usr/bin/ar"),
        tool_path(name = "cpp", path = "/usr/bin/cpp"),
        tool_path(name = "gcov", path = "/usr/bin/gcov"),
        tool_path(name = "nm", path = "/usr/bin/nm"),
        tool_path(name = "objdump", path = "/usr/bin/objdump"),
        tool_path(name = "strip", path = "/usr/bin/strip"),
    ]
    
    default_compile_flags = [
        flag_set(
            actions = [
                "@bazel_tools//tools/cpp:cc_flags_action",
                "@bazel_tools//tools/cpp:c_compile_action",
                "@bazel_tools//tools/cpp:c++_compile_action",
            ],
            flag_groups = [
                flag_group(flags = [
                    "-nostdinc",
                    "-isystem", "/usr/lib/gcc/{target_cpu}/12/include",
                    "-isystem", "/usr/include/c++/12",
                    "-isystem", "/usr/include/{target_cpu}/c++/12",
                ]),
            ],
        ),
    ]
    
    default_link_flags = [
        flag_set(
            actions = [
                "@bazel_tools//tools/cpp:c++_link_executable_action",
                "@bazel_tools//tools/cpp:c++_link_dynamic_library_action",
                "@bazel_tools//tools/cpp:c++_link_nodeps_dynamic_library_action",
            ],
            flag_groups = [flag_group(flags = ["-lstdc++", "-lm"])],
        ),
    ]

    return cc_common.create_cc_toolchain_config_info(
        ctx = ctx,
        toolchain_identifier = "gcc-linux-toolchain",
        host_system_name = "local",
        target_system_name = "linux_gnu",
        target_cpu = ctx.attr.cpu,
        target_libc = "unknown",
        compiler = "gcc",
        abi_version = "unknown",
        abi_libc_version = "unknown",
        tool_paths = tool_paths,
    )

gcc_toolchain_config = rule(
    implementation = _impl,
    attrs = {
        "cpu": attr.string(mandatory = True),
    },
    provides = [CcToolchainConfigInfo],
)
'''.format(target_cpu = target_cpu))
