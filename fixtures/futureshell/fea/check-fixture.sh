#!/bin/sh
set -eu

fixture_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/xshell-fs0-fea.XXXXXX")

cleanup() {
    for name in model.step mesh.msh analysis.json undeclared.txt; do
        unlink "$work_dir/$name" 2>/dev/null || :
    done
    rmdir "$work_dir" 2>/dev/null || :
}
trap cleanup EXIT HUP INT TERM

sha256() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        sha256sum "$1" | awk '{ print $1 }'
    fi
}

mesh_hash=$(jq -r '.programs[] | select(.id == "mesh") | .executable_sha256' "$fixture_dir/programs.json")
solve_hash=$(jq -r '.programs[] | select(.id == "solve") | .executable_sha256' "$fixture_dir/programs.json")
schema_hash=$(jq -r '.contract.schema.sha256' "$fixture_dir/programs.json")
test "$(sha256 "$fixture_dir/tools/fake-gmsh")" = "$mesh_hash"
test "$(sha256 "$fixture_dir/tools/fake-fenics")" = "$solve_hash"
test "$(sha256 "$fixture_dir/schemas/fea-result-v1.schema.json")" = "$schema_hash"

cp "$fixture_dir/workspace/input/model.step" "$work_dir/model.step"
(cd "$work_dir" && "$fixture_dir/tools/fake-gmsh" model.step mesh.msh)
(cd "$work_dir" && "$fixture_dir/tools/fake-fenics" mesh.msh analysis.json)
cmp "$work_dir/analysis.json" "$fixture_dir/expected/analysis.json"
jq -e '.schema == "fea-result-v1" and (.max_von_mises_mpa | type == "number") and .solver == "fake-fenics"' "$work_dir/analysis.json" >/dev/null

if (cd "$work_dir" && "$fixture_dir/tools/fake-gmsh" model.step mesh.msh fail) 2>/dev/null; then
    printf '%s\n' 'fake-gmsh failure mode unexpectedly succeeded' >&2
    exit 1
fi
if (cd "$work_dir" && "$fixture_dir/tools/fake-fenics" mesh.msh analysis.json fail) 2>/dev/null; then
    printf '%s\n' 'fake-fenics failure mode unexpectedly succeeded' >&2
    exit 1
fi
(cd "$work_dir" && "$fixture_dir/tools/fake-fenics" mesh.msh analysis.json invalid_json)
if jq empty "$work_dir/analysis.json" 2>/dev/null; then
    printf '%s\n' 'invalid_json mode unexpectedly emitted JSON' >&2
    exit 1
fi
(cd "$work_dir" && "$fixture_dir/tools/fake-fenics" mesh.msh analysis.json invalid_schema)
if jq -e '.schema == "fea-result-v1" and (.max_von_mises_mpa | type == "number") and .solver == "fake-fenics"' "$work_dir/analysis.json" >/dev/null; then
    printf '%s\n' 'invalid_schema mode unexpectedly matched the fixture schema' >&2
    exit 1
fi
(cd "$work_dir" && "$fixture_dir/tools/fake-fenics" mesh.msh analysis.json undeclared_write)
test -f "$work_dir/undeclared.txt"

printf '%s\n' 'FutureShell FEA fixture verified'
