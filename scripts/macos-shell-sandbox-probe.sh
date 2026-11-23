#!/bin/bash
# Run the docs/shell-sandbox.md §5 profile under sandbox-exec and check what it
# allows and denies, at each network level (none, loopback, full). Also runs
# two broken variants (no system.sb; signal filter combined with exec) to show
# why the profile is written as it is.
#
# A check that expects "denied" first runs the same command WITHOUT the
# sandbox and requires it to succeed. Otherwise a command that fails for an
# unrelated reason (a missing tool, a listener that did not start, no network)
# would pass as "denied". A few checks have no such control because running
# them unsandboxed would open an application or prompt for a permission; they
# are marked "(no control)".
#
# Exits non-zero if any check, or any control, differs from what is expected.
#
# Run it outside any sandbox (sandboxes do not nest). It touches only the
# temporary directory it creates, and removes it on exit. VERBOSE=1 shows the
# last lines of each command's output.
set -u
[ "$(uname)" = Darwin ] || { echo "macOS only" >&2; exit 2; }

W=$(cd "$(mktemp -d /tmp/xshell-sbx.XXXXXX)" && pwd -P)
ROOT=$W/root; TMPD=$W/tmp; RO=$W/ro; OUT=$W/outside
mkdir -p "$ROOT" "$TMPD" "$RO" "$OUT"
echo secret > "$OUT/canary"; echo tool > "$RO/tool.txt"
ln -s "$OUT" "$ROOT/link"
# For the LaunchServices check: a script that would run outside the sandbox
# if `open` could hand it to another application.
printf '#!/bin/sh\ntouch "%s/escaped"\n' "$OUT" > "$ROOT/escape.command"
chmod +x "$ROOT/escape.command"

DEV=$(xcode-select -p 2>/dev/null)/usr/bin
PY=$DEV/python3; [ -x "$PY" ] || PY=/usr/bin/python3

listen() {  # listen SOCKET: a unix socket listener for 300 s
    "$PY" -I -c "
import socket,os,sys,time
s=socket.socket(socket.AF_UNIX); s.bind('$1'); s.listen(8)
if os.fork(): sys.exit(0)
time.sleep(300)"
}
SOCK_OUT=$OUT/daemon.sock; SOCK_IN=$ROOT/local.sock
listen "$SOCK_OUT"; listen "$SOCK_IN"
LISTENERS=$(pgrep -f "bind\('$W" | tr '\n' ' ')
trap 'kill $LISTENERS 2>/dev/null; rm -rf "$W"' EXIT
sleep 0.5

HEAD='(version 1)
(deny default)'
# No terminal device here. /dev/tty is the daemon's own terminal when xshelld
# runs in the foreground, and an agent's command must not read or write it.
FILES='(allow sysctl-read)
(allow file-read* file-map-executable
       (subpath "/usr") (subpath "/bin") (subpath "/sbin")
       (subpath "/System") (subpath "/Library") (subpath "/private/etc"))
(allow file-read* file-write* (literal "/dev/null"))
(allow file-read* (literal "/dev/urandom") (literal "/dev/random"))
(allow file-read-metadata)
(allow file-read* file-write* (subpath (param "ROOT_0")) (subpath (param "TMP")))
(allow file-read* file-map-executable (subpath (param "RO_0")))'
PROC='(allow process-fork process-exec*)
(allow signal (target same-sandbox))'
# Unix sockets inside the roots and the scratch directory are local to the
# session, so they are allowed at every network level.
# UNVERIFIED: these two rules were written without a Mac to run them on. If
# the "unix socket inside root" or "bind a unix socket in root" checks fail,
# this is the rule to fix.
LOCAL_SOCKETS='(allow network-outbound (subpath (param "ROOT_0")) (subpath (param "TMP")))
(allow network-bind network-inbound (subpath (param "ROOT_0")) (subpath (param "TMP")))'
# Loopback: listen on and connect to this machine only.
# UNVERIFIED in the same way.
LOOPBACK='(allow network-bind network-inbound (local ip "localhost:*"))
(allow network-outbound (remote ip "localhost:*"))'
# Full: outbound IP anywhere (not unix sockets), and name resolution.
FULL='(allow network-outbound (remote ip "*:*"))
(allow network-outbound (literal "/private/var/run/mDNSResponder"))
(allow mach-lookup (global-name "com.apple.mDNSResponder"))'

P_NONE="$HEAD
(import \"system.sb\")
$PROC
$FILES
$LOCAL_SOCKETS"
P_LOOPBACK="$P_NONE
$LOOPBACK"
P_FULL="$P_LOOPBACK
$FULL"
P_NO_SYSTEM="$HEAD
$PROC
$FILES"
P_COMBINED="$HEAD
(import \"system.sb\")
(allow process-fork process-exec* signal (target same-sandbox))
$FILES"

OUTPUT=
exec_in() {  # exec_in PROFILE-or-empty COMMAND -> sets OUTPUT, returns the command's status
    local rc
    if [ -n "$1" ]; then
        OUTPUT=$(cd "$ROOT" && env -i HOME="$TMPD" XDG_CONFIG_HOME="$TMPD" TMPDIR="$TMPD/" \
              PATH="$DEV:/usr/bin:/bin:/usr/sbin:/sbin" LANG=C \
              /usr/bin/sandbox-exec -p "$1" -D ROOT_0="$ROOT" -D TMP="$TMPD" -D RO_0="$RO" \
              /bin/zsh -fc "$2" 2>&1); rc=$?
    else
        OUTPUT=$(cd "$ROOT" && env -i HOME="$TMPD" XDG_CONFIG_HOME="$TMPD" TMPDIR="$TMPD/" \
              PATH="$DEV:/usr/bin:/bin:/usr/sbin:/sbin" LANG=C \
              /bin/zsh -fc "$2" 2>&1); rc=$?
    fi
    if [ -n "${VERBOSE:-}" ] && [ -n "$OUTPUT" ]; then
        echo "    -> $(echo "$OUTPUT" | tail -2 | tr '\n' ' ')" >&2
    fi
    return $rc
}

FAIL=0
report() { printf '%-4s %-52s %-7s %s\n' "$1" "$2" "$3" "$4"; [ "$1" != FAIL ] || FAIL=1; }

allowed() {  # allowed NAME PROFILE COMMAND: must succeed in the sandbox
    if exec_in "$2" "$3"; then report PASS "$1" ok ok
    else report FAIL "$1" ok "denied($?)"; fi
}

denied() {  # denied NAME PROFILE COMMAND: must succeed outside, fail inside
    exec_in "" "$3"; local control=$?
    if [ $control -ne 0 ]; then
        report FAIL "$1" denied "control failed($control): cannot tell"
        return
    fi
    if exec_in "$2" "$3"; then report FAIL "$1" denied ok
    else report PASS "$1" denied "denied($?)"; fi
}

denied_soft() {  # as `denied`, but a failed control is a skip, not a failure
    exec_in "" "$3"; local control=$?
    if [ $control -ne 0 ]; then
        report SKIP "$1" denied "control failed($control): not checked"
        return
    fi
    if exec_in "$2" "$3"; then report FAIL "$1" denied ok
    else report PASS "$1" denied "denied($?)"; fi
}

denied_nc() {  # as `denied`, with no unsandboxed control
    if exec_in "$2" "$3"; then report FAIL "$1 (no control)" denied ok
    else report PASS "$1 (no control)" denied "denied($?)"; fi
}

connect() { echo "$PY -I -c \"import socket;socket.socket(socket.AF_UNIX).connect('$1')\""; }
# Bind a loopback TCP port, connect to it, and accept the connection.
LOOP_TCP="$PY -I -c \"
import socket
s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(1)
c=socket.socket(); c.settimeout(5); c.connect(s.getsockname()); s.accept()\""
# The listeners' command lines hold \$W; seeing them means other processes'
# arguments are readable.
SEE_PROCESSES="ps -axo command | grep -v grep | grep -q 'bind(.$W'"
# 44 is \"item not found\": the request reached the keychain service.
KEYCHAIN="security find-generic-password -s xshell-sandbox-probe-missing >/dev/null 2>&1; test \$? -eq 44"

printf '%-4s %-52s %-7s %s\n' "" CHECK EXPECT GOT
denied "no system.sb: shell starts"                  "$P_NO_SYSTEM" "true"
denied "signal filter on exec rule: shell starts"     "$P_COMBINED"  "true"
for mode in none loopback full; do
    case $mode in none) P=$P_NONE ;; loopback) P=$P_LOOPBACK ;; full) P=$P_FULL ;; esac
    echo "-- network $mode"
    allowed "shell starts"                            "$P" "echo hi"
    allowed "write in root, scratch dir"              "$P" "echo x > $ROOT/a && echo x > $TMPD/a"
    allowed "read a read_only grant"                  "$P" "cat $RO/tool.txt"
    denied  "write a read_only grant"                 "$P" "echo x > $RO/b"
    denied  "write outside roots"                     "$P" "echo x > $OUT/x"
    denied  "read canary outside roots"               "$P" "cat $OUT/canary"
    denied  "read canary via /System/Volumes/Data"    "$P" "cat /System/Volumes/Data$OUT/canary"
    denied  "read canary via symlink in root"         "$P" "cat $ROOT/link/canary"
    denied  "read the real home directory"            "$P" "ls $HOME"
    denied  "read the login keychain file"            "$P" "head -c 1 $HOME/Library/Keychains/login.keychain-db"
    # The control depends on how `security` behaves with HOME moved, which
    # has not been measured, so a failed control only skips this check.
    denied_soft "reach the keychain service"          "$P" "$KEYCHAIN"
    denied  "see other processes' command lines"      "$P" "$SEE_PROCESSES"
    denied  "signal a process outside the sandbox"    "$P" "kill -0 $$"
    denied  "unix socket outside roots"               "$P" "$(connect "$SOCK_OUT")"
    allowed "unix socket inside root"                 "$P" "$(connect "$SOCK_IN")"
    allowed "bind a unix socket in root"              "$P" "$PY -I -c \"import socket;socket.socket(socket.AF_UNIX).bind('$ROOT/bound-$mode.sock')\""
    if [ -t 0 ]; then
        denied "open the controlling terminal"        "$P" ": < /dev/tty"
    fi
    # Escapes through other applications. Unsandboxed these would open
    # Terminal or ask for an Automation permission, so they have no control.
    denied_nc "hand a script to LaunchServices (open)" "$P" "open -g $ROOT/escape.command"
    denied_nc "send an Apple event (osascript)"        "$P" "osascript -e 'tell application \"System Events\" to get name of first process'"
    allowed "python3, uname, sw_vers"                 "$P" "python3 -c 'print(1)' && uname -a && sw_vers"
    allowed "git init + commit in root"               "$P" "git init -q r$mode && cd r$mode && git -c user.email=a@b -c user.name=a commit -q --allow-empty -m x"
    case $mode in
        none)
            denied  "loopback TCP: bind, connect, accept" "$P" "$LOOP_TCP"
            denied  "curl by name"                        "$P" "curl -sS -m 10 -o /dev/null https://example.com"
            denied  "curl by IP"                          "$P" "curl -sS -m 10 -o /dev/null -k https://1.1.1.1" ;;
        loopback)
            allowed "loopback TCP: bind, connect, accept" "$P" "$LOOP_TCP"
            denied  "curl by name"                        "$P" "curl -sS -m 10 -o /dev/null https://example.com"
            denied  "curl by IP"                          "$P" "curl -sS -m 10 -o /dev/null -k https://1.1.1.1" ;;
        full)
            allowed "loopback TCP: bind, connect, accept" "$P" "$LOOP_TCP"
            allowed "curl by name"                        "$P" "curl -sS -m 10 -o /dev/null https://example.com" ;;
    esac
done
sleep 2
if [ -e "$OUT/escaped" ]; then
    report FAIL "a script ran outside the sandbox through open" denied ok
fi
echo; echo "macOS $(sw_vers -productVersion) $(uname -m)"
[ $FAIL -eq 0 ] && echo "all checks passed" || echo "SOME CHECKS FAILED"
exit $FAIL
