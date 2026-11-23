#!/bin/bash
# Run the docs/shell-sandbox.md §5 profile under sandbox-exec and check what it
# allows and denies, with the network off and on. Also runs two broken variants
# (no system.sb; signal filter combined with exec) to show why the profile is
# written as it is. Exits non-zero if any check differs from what is expected.
#
# Run it outside any sandbox (sandboxes do not nest). It touches only the
# temporary directory it creates, and removes it on exit.
set -u
[ "$(uname)" = Darwin ] || { echo "macOS only" >&2; exit 2; }

W=$(cd "$(mktemp -d /tmp/xshell-sbx.XXXXXX)" && pwd -P)
ROOT=$W/root; TMPD=$W/tmp; RO=$W/ro; OUT=$W/outside
mkdir -p "$ROOT" "$TMPD" "$RO" "$OUT"
echo secret > "$OUT/canary"; echo tool > "$RO/tool.txt"
ln -s "$OUT" "$ROOT/link"

DEV=$(xcode-select -p 2>/dev/null)/usr/bin
PY=$DEV/python3; [ -x "$PY" ] || PY=/usr/bin/python3

listen() {  # listen SOCKET: a unix socket listener for 120 s
    "$PY" -I -c "
import socket,os,sys,time
s=socket.socket(socket.AF_UNIX); s.bind('$1'); s.listen(8)
if os.fork(): sys.exit(0)
time.sleep(120)"
}
SOCK_OUT=$OUT/daemon.sock; SOCK_IN=$ROOT/local.sock
listen "$SOCK_OUT"; listen "$SOCK_IN"
LISTENERS=$(pgrep -f "bind\('$W" | tr '\n' ' ')
trap 'kill $LISTENERS 2>/dev/null; rm -rf "$W"' EXIT
sleep 0.5

HEAD='(version 1)
(deny default)'
FILES='(allow sysctl-read)
(allow file-read* file-map-executable
       (subpath "/usr") (subpath "/bin") (subpath "/sbin")
       (subpath "/System") (subpath "/Library") (subpath "/private/etc"))
(allow file-read* file-write* (literal "/dev/null") (literal "/dev/tty"))
(allow file-read* (literal "/dev/urandom") (literal "/dev/random"))
(allow file-read-metadata)
(allow file-read* file-write* (subpath (param "ROOT_0")) (subpath (param "TMP")))
(allow file-read* file-map-executable (subpath (param "RO_0")))'
PROC='(allow process-fork process-exec*)
(allow signal (target same-sandbox))'
NET='(allow network-outbound (remote ip "*:*"))
(allow network-outbound (literal "/private/var/run/mDNSResponder"))
(allow mach-lookup (global-name "com.apple.mDNSResponder"))'
P_OFF="$HEAD
(import \"system.sb\")
$PROC
$FILES"
P_ON="$P_OFF
$NET"
P_NO_SYSTEM="$HEAD
$PROC
$FILES"
P_COMBINED="$HEAD
(import \"system.sb\")
(allow process-fork process-exec* signal (target same-sandbox))
$FILES"

run() {  # run PROFILE COMMAND -> ok | denied(rc)
    local out rc
    out=$(cd "$ROOT" && env -i HOME="$TMPD" XDG_CONFIG_HOME="$TMPD" TMPDIR="$TMPD/" \
          PATH="$DEV:/usr/bin:/bin:/usr/sbin:/sbin" LANG=C \
          /usr/bin/sandbox-exec -p "$1" -D ROOT_0="$ROOT" -D TMP="$TMPD" -D RO_0="$RO" \
          /bin/zsh -fc "$2" 2>&1); rc=$?
    [ $rc -eq 0 ] && echo ok || echo "denied($rc)"
    [ -n "${VERBOSE:-}" ] && [ -n "$out" ] && echo "    -> $(echo "$out" | tail -2 | tr '\n' ' ')" >&2
}

FAIL=0
check() {  # check NAME PROFILE EXPECT COMMAND
    local got; got=$(run "$2" "$4")
    local mark=PASS; case "$got" in "$3"*) ;; *) mark=FAIL; FAIL=1 ;; esac
    printf '%-4s %-48s %-7s %s\n' "$mark" "$1" "$3" "$got"
}
connect() { echo "$PY -I -c \"import socket;socket.socket(socket.AF_UNIX).connect('$1')\""; }

printf '%-4s %-48s %-7s %s\n' "" CHECK EXPECT GOT
check "no system.sb: shell starts"               "$P_NO_SYSTEM" denied "true"
check "signal filter on exec rule: shell starts"  "$P_COMBINED"  denied "true"
for mode in off on; do
    [ $mode = off ] && P=$P_OFF || P=$P_ON
    echo "-- network $mode"
    check "shell starts"                          "$P" ok     "echo hi"
    check "write in root, scratch dir"            "$P" ok     "echo x > $ROOT/a && echo x > $TMPD/a"
    check "read a read_only grant"                "$P" ok     "cat $RO/tool.txt"
    check "write a read_only grant"               "$P" denied "echo x > $RO/b"
    check "write outside roots"                   "$P" denied "echo x > $OUT/x"
    check "read canary outside roots"             "$P" denied "cat $OUT/canary"
    check "read canary via /System/Volumes/Data"  "$P" denied "cat /System/Volumes/Data$OUT/canary"
    check "read canary via symlink in root"       "$P" denied "cat $ROOT/link/canary"
    check "read the real home directory"          "$P" denied "ls $HOME"
    check "unix socket outside roots"             "$P" denied "$(connect "$SOCK_OUT")"
    check "signal a process outside the sandbox"  "$P" denied "kill -0 $$"
    check "python3, uname, sw_vers"               "$P" ok     "python3 -c 'print(1)' && uname -a && sw_vers"
    check "git init + commit in root"             "$P" ok     "git init -q r$mode && cd r$mode && git -c user.email=a@b -c user.name=a commit -q --allow-empty -m x"
    if [ $mode = off ]; then
        check "unix socket inside root"           "$P" denied "$(connect "$SOCK_IN")"
        check "curl by name"                      "$P" denied "curl -sS -m 5 -o /dev/null https://example.com"
        check "curl by IP"                        "$P" denied "curl -sS -m 5 -o /dev/null -k https://1.1.1.1"
    else
        check "curl by name"                      "$P" ok     "curl -sS -m 10 -o /dev/null https://example.com"
    fi
done
echo; echo "macOS $(sw_vers -productVersion) $(uname -m)"
exit $FAIL
