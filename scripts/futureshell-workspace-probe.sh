#!/bin/sh
set -eu

# This reports mechanisms; it does not claim that FutureShell isolation exists.
probe_dir=$(mktemp -d "${TMPDIR:-/tmp}/xshell-fs0-probe.XXXXXX")

cleanup() {
    unlink "$probe_dir/staging/escape" 2>/dev/null || :
    unlink "$probe_dir/staging/input.txt" 2>/dev/null || :
    unlink "$probe_dir/source/input.txt" 2>/dev/null || :
    unlink "$probe_dir/destination/analysis.json" 2>/dev/null || :
    unlink "$probe_dir/destination/.analysis.tmp" 2>/dev/null || :
    rmdir "$probe_dir/staging" 2>/dev/null || :
    rmdir "$probe_dir/source" 2>/dev/null || :
    rmdir "$probe_dir/destination" 2>/dev/null || :
    rmdir "$probe_dir" 2>/dev/null || :
}
trap cleanup EXIT HUP INT TERM

mkdir "$probe_dir/source" "$probe_dir/staging" "$probe_dir/destination"
printf '%s\n' baseline > "$probe_dir/source/input.txt"

kernel=$(uname -s)
case "$kernel" in
    Darwin)
        device=$(df "$probe_dir" | awk 'END { print $1 }')
        filesystem=$(diskutil info "$device" 2>/dev/null | awk -F: '/File System Personality/ { gsub(/^[[:space:]]+/, "", $2); print $2; exit }')
        filesystem=${filesystem:-unknown}
        if cp -c "$probe_dir/source/input.txt" "$probe_dir/staging/input.txt" 2>/dev/null; then
            clone=clonefile
        else
            cp "$probe_dir/source/input.txt" "$probe_dir/staging/input.txt"
            clone=copy
        fi
        if command -v sandbox-exec >/dev/null 2>&1; then
            native_sandbox=seatbelt-command-present
        else
            native_sandbox=unavailable
        fi
        resource_control=setrlimit
        ;;
    Linux)
        filesystem=$(stat -f -c %T "$probe_dir")
        if cp --reflink=always "$probe_dir/source/input.txt" "$probe_dir/staging/input.txt" 2>/dev/null; then
            clone=reflink
        else
            cp "$probe_dir/source/input.txt" "$probe_dir/staging/input.txt"
            clone=copy
        fi
        if command -v unshare >/dev/null 2>&1 && unshare --user --map-root-user true 2>/dev/null; then
            native_sandbox=user-namespace-usable
        else
            native_sandbox=user-namespace-unavailable
        fi
        if [ -r /sys/fs/cgroup/cgroup.controllers ]; then
            resource_control=cgroup-v2-present
        else
            resource_control=setrlimit-only
        fi
        ;;
    *)
        printf '%s\n' "unsupported kernel: $kernel" >&2
        exit 2
        ;;
esac

cmp "$probe_dir/source/input.txt" "$probe_dir/staging/input.txt"
ln -s ../source/input.txt "$probe_dir/staging/escape"
test -L "$probe_dir/staging/escape"
test "$(readlink "$probe_dir/staging/escape")" = ../source/input.txt

printf '%s\n' result > "$probe_dir/destination/.analysis.tmp"
mv "$probe_dir/destination/.analysis.tmp" "$probe_dir/destination/analysis.json"
test "$(sed -n '1p' "$probe_dir/destination/analysis.json")" = result

printf 'kernel=%s\n' "$kernel"
printf 'filesystem=%s\n' "$filesystem"
printf 'staging_copy=verified\n'
printf 'clone_acceleration=%s\n' "$clone"
printf 'symlink_detection=verified\n'
printf 'same_filesystem_replace=verified\n'
printf 'native_sandbox=%s\n' "$native_sandbox"
printf 'resource_control=%s\n' "$resource_control"
