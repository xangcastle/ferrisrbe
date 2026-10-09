#!/usr/bin/env bash
# Formatea con rustfmt los .rs recibidos (los que no son .rs se ignoran); sin argumentos, todo .rs versionado.
set -euo pipefail

runfiles="${RUNFILES_DIR:-$0.runfiles}"
rustfmt="$(cd "$runfiles" && pwd)/$1"
shift

# Como bazel run: cwd en los runfiles, rutas relativas al workspace.
cd "${BUILD_WORKSPACE_DIRECTORY:-.}"

files=()
if [[ $# -eq 0 ]]; then
	while IFS= read -r f; do
		files+=("$f")
	done < <(git ls-files -- '*.rs')
else
	for f in "$@"; do
		if [[ "$f" == *.rs && -f "$f" ]]; then
			files+=("$f")
		fi
	done
fi

if [[ ${#files[@]} -eq 0 ]]; then
	exit 0
fi
exec "$rustfmt" "${files[@]}"
