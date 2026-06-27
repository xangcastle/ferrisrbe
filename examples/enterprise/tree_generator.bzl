"""Rule to generate a directory with many files for tree stress tests."""

def _tree_generator_impl(ctx):
    output_dir = ctx.actions.declare_directory(ctx.attr.output_name)
    ctx.actions.run_shell(
        outputs = [output_dir],
        command = """
mkdir -p {out}
python3 -c '
import os
out = "{out}"
for i in range(50000):
    with open(os.path.join(out, f"file_{{i}}.txt"), "w") as f:
        f.write(f"Test content {{i}}\\n")
'
""".format(out = output_dir.path),
        progress_message = "Generating massive tree (%s)" % ctx.attr.output_name,
    )
    return [DefaultInfo(files = depset([output_dir]))]

tree_generator = rule(
    implementation = _tree_generator_impl,
    attrs = {
        "output_name": attr.string(default = "massive_tree"),
    },
)
