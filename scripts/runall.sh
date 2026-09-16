#!/usr/bin/env sh

## script to run xshell services in "home lab" testing mode, where audit
## service, session service, and CLI are started together.
##
## Checks for source changes via cargo build, restarts services if needed,
## and then runs the CLI interactively.

CONFIGDIR=~/.config/xshell
AUDITDIR=/tmp/xshell-audit
AUDITSOCK=/tmp/xshell-audit.sock

# Kill a service gracefully: SIGTERM, wait up to 5s, then SIGKILL.
# $1 = exact binary name
kill_service() {
    _name="$1"
    _pids=$(pgrep -x "$_name" 2>/dev/null)
    if [ -z "$_pids" ]; then
        return 0
    fi
    echo "  sending SIGTERM to $_name (pids: $_pids)..."
    pkill -x -TERM "$_name" 2>/dev/null || true
    _waited=0
    while [ $_waited -lt 5 ]; do
        sleep 1
        _waited=$(( _waited + 1 ))
        if ! pgrep -x "$_name" >/dev/null 2>&1; then
            echo "  $_name stopped."
            return 0
        fi
    done
    echo "  $_name still running, sending SIGKILL..."
    pkill -x -KILL "$_name" 2>/dev/null || true
    sleep 0.5
    echo "  $_name killed."
}

# Build a binary and set _rebuild=1 if any source was compiled.
# $1 = package name, $2 = binary name
check_build() {
    _pkg="$1"
    _bin="$2"
    _output=$(cargo build -p "$_pkg" --bin "$_bin" 2>&1)
    _status=$?
    if [ $_status -ne 0 ]; then
        echo "  ERROR: cargo build failed for $_pkg ($_bin)"
        echo "$_output"
        exit 1
    fi
    if echo "$_output" | grep -q "^Compiling "; then
        _rebuild=1
    else
        _rebuild=0
    fi
}

###############################################################################
# AUDIT SERVICE
###############################################################################
echo "checking audit service..."
_rebuild=0
check_build xshell-audit xshell-auditd

if [ $_rebuild -eq 1 ]; then
    echo "  audit service rebuilt — restarting..."
    kill_service xshell-auditd
elif ! pgrep -x xshell-auditd >/dev/null 2>&1; then
    echo "  audit service not running — starting..."
else
    echo "  audit service is up to date and running."
fi

# Start if we rebuilt or if it wasn't already running
if [ $_rebuild -eq 1 ] || ! pgrep -x xshell-auditd >/dev/null 2>&1; then
    echo "starting audit service"
    cargo run -p xshell-audit --bin xshell-auditd -- \
        --directory ${AUDITDIR} \
        --socket ${AUDITSOCK} &
    sleep 1
fi

###############################################################################
# SESSION SERVICE
###############################################################################
echo "checking session service..."
_rebuild=0
check_build xshell-session xshelld

if [ $_rebuild -eq 1 ]; then
    echo "  session service rebuilt — restarting..."
    kill_service xshelld
elif ! pgrep -x xshelld >/dev/null 2>&1; then
    echo "  session service not running — starting..."
else
    echo "  session service is up to date and running."
fi

if [ $_rebuild -eq 1 ] || ! pgrep -x xshelld >/dev/null 2>&1; then
    echo "starting session service"
    cargo run -p xshell-session --bin xshelld -- \
        --config ${CONFIGDIR}/config.toml &
    sleep 1
fi

###############################################################################
# CLI
###############################################################################
echo "checking CLI..."
_rebuild=0
check_build xshell-cli xshell

if [ $_rebuild -eq 1 ]; then
    echo "  CLI rebuilt."
else
    echo "  CLI is up to date."
fi

echo "ready — starting CLI"
exec cargo run -p xshell-cli --bin xshell
