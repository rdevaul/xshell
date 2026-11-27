#!/usr/bin/env python3
"""Live check of the Anthropic provider's server-side compaction against the API.

    KEY_ENV=ANTHROPIC_API_KEY python3 scripts/live-compaction-check.py [--xshell-repo DIR]

It costs at most a few dollars at list prices: about a dozen requests of up to 65K tokens, mostly cache
reads (each turn's `usage.tokens` in turn-N.jsonl has the counts). It is a manual check, not part
of CI. See docs/anthropic-provider.md, "As built (server compaction)".

It runs two headless turns in one session, with a long-run profile (adaptive thinking, 1-hour
cache, xshell's own compaction off) and `compact_at_tokens = 50000`, the API's minimum, so the
conversation compacts after a few file reads:

  turn 1  reads docs/part-01..08.txt one file per response: the prompt passes 50K tokens, the
          API compacts, and the turn keeps calling tools after the compaction block.
  turn 2  a new turn in the same session: its first request opens with xshell's stand-in user
          message, then the compaction block and the signed thinking after it.

xshell talks to the API through a local logging proxy, so the check sees what xshell sent, not
only what came back. The proxy records the shape of each request (message roles and block types,
the beta header, the body size) and the status and stop reason of each response. It records no
message text, no headers other than anthropic-beta, and never the API key.

Pass conditions, each printed as PASS, FAIL or WARN:
  - every request is accepted (no 4xx/5xx);
  - at least one compaction happened (the response and xshell's usage both say so);
  - every request after a compaction starts with the stand-in user message and the compaction
    block, and is smaller than the request that compacted;
  - each of those requests carries the compaction beta;
  - signed thinking produced after the compaction is replayed and accepted (WARN if the model
    happened to produce none, as the check was then not exercised);
  - each response's usage is consistent (one-hour cache writes within all writes);
  - both turns end with outcome completed and report the files' keys.

Not covered: replaying a compaction block with compaction turned off. A session keeps the
model settings it was created with, and only the interactive `//model` command rebinds it; the
script prints the manual step at the end.

Everything goes in --workdir (default ~/.cache/xshell/compaction-check-TIMESTAMP): the
generated files, the config, the private daemon's state, turn-N.jsonl, requests.jsonl and
report.json. The shell is denied (`--approval off`), so the agent can only read files.
"""
from __future__ import annotations

import argparse
import datetime
import http.client
import http.server
import json
import os
import random
import subprocess
import sys
import threading
import urllib.parse
from pathlib import Path

COMPACT_BETA = "compact-2026-01-12"
STAND_IN = "(The conversation before this point was compacted. Its summary follows.)"
WORDS = ("bracket flange bearing shaft collar gusset fillet chamfer bore boss rib web pocket slot "
         "tab clevis yoke pin bushing spacer washer nut screw stud dowel key spline hub rim spoke "
         "arm link joint pivot hinge mount plate panel frame rail beam post column base foot pad "
         "cap cover lid shell housing case enclosure duct vent port inlet outlet nozzle valve seat "
         "seal gasket ring groove thread pitch lead taper draft radius angle offset clearance fit "
         "tolerance datum surface edge vertex face loop sketch profile extrude revolve sweep loft "
         "shell mirror pattern array union subtract intersect section view detail note revision "
         "material steel aluminium brass nylon acetal polycarbonate density mass inertia load "
         "stress strain yield fatigue torque speed ratio gear pinion rack belt pulley chain sprocket "
         "motor servo encoder sensor limit switch cable harness connector terminal").split()


# ------------------------------------------------------------------ the logging proxy

def shape(body: dict) -> dict:
    """What a request looked like, without its text."""
    messages = body.get("messages", [])
    found = None
    for i, message in enumerate(messages):
        blocks = message.get("content")
        if not isinstance(blocks, list):
            continue
        for j, block in enumerate(blocks):
            if block.get("type") == "compaction":
                found = (i, j)
    first = messages[0] if messages else {}
    first_blocks = first.get("content") if isinstance(first.get("content"), list) else []
    thinking_after = 0
    if found:
        for i, message in enumerate(messages[found[0]:], start=found[0]):
            blocks = message.get("content") if isinstance(message.get("content"), list) else []
            start = found[1] + 1 if i == found[0] else 0
            thinking_after += sum(1 for b in blocks[start:]
                                  if b.get("type") in ("thinking", "redacted_thinking"))
    return {
        "messages": len(messages),
        "first_role": first.get("role"),
        "first_is_stand_in": (len(first_blocks) == 1 and first_blocks[0].get("type") == "text"
                              and first_blocks[0].get("text") == STAND_IN),
        "compaction_at": list(found) if found else None,
        "compaction_null": bool(found) and messages[found[0]]["content"][found[1]].get("content") is None,
        "thinking_after_compaction": thinking_after,
        "edits": [e.get("type") for e in (body.get("context_management") or {}).get("edits", [])],
    }


def make_proxy(upstream: str, log_path: Path):
    target = urllib.parse.urlsplit(upstream)
    lock = threading.Lock()
    counter = [0]

    class Proxy(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *args):  # quiet
            pass

        def do_POST(self):
            raw = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            with lock:
                counter[0] += 1
                seq = counter[0]
            record = {"seq": seq, "at": datetime.datetime.now(datetime.UTC).isoformat(timespec="seconds"),
                      "path": self.path, "body_bytes": len(raw),
                      "beta": self.headers.get("anthropic-beta", "")}
            try:
                record.update(shape(json.loads(raw)))
            except (ValueError, AttributeError, TypeError, KeyError, IndexError) as error:
                record["shape_error"] = repr(error)
            headers = {k: v for k, v in self.headers.items()
                       if k.lower() not in ("host", "content-length", "accept-encoding", "connection")}
            headers["Accept-Encoding"] = "identity"
            connection = http.client.HTTPSConnection(target.hostname, target.port or 443, timeout=900)
            connection.request("POST", target.path.rstrip("/") + self.path, body=raw, headers=headers)
            response = connection.getresponse()
            record["status"] = response.status
            self.send_response(response.status)
            for k, v in response.getheaders():
                if k.lower() not in ("transfer-encoding", "content-length", "connection", "content-encoding"):
                    self.send_header(k, v)
            self.send_header("Connection", "close")
            self.end_headers()
            self.close_connection = True
            received = bytearray()
            while chunk := response.read1(65536):
                received += chunk
                try:
                    self.wfile.write(chunk)
                    self.wfile.flush()
                except OSError:
                    record["client_gone"] = True
                    break
            connection.close()
            text = received.decode("utf-8", "replace")
            if response.status >= 400:
                record["error"] = text[:2000]
            else:
                record.update(response_shape(text))
            with lock, open(log_path, "a") as fh:
                fh.write(json.dumps(record) + "\n")

    return Proxy


def response_shape(sse: str) -> dict:
    compactions, stop, errors, usage = 0, None, [], []
    for line in sse.splitlines():
        if not line.startswith("data:"):
            continue
        try:
            event = json.loads(line[5:])
        except ValueError:
            continue
        kind = event.get("type")
        # The raw usage objects: numbers only, kept to check xshell's accounting.
        raw = (event.get("message") or {}).get("usage") if kind == "message_start" else event.get("usage")
        if raw:
            usage.append({"event": kind, "usage": raw})
        if kind == "content_block_start" and event.get("content_block", {}).get("type") == "compaction":
            compactions += 1
        elif kind == "message_delta":
            stop = event.get("delta", {}).get("stop_reason", stop)
        elif kind == "error":
            errors.append(event.get("error"))
    out = {"response_compactions": compactions, "stop_reason": stop, "usage": usage}
    if errors:
        out["stream_errors"] = errors
    return out


# ------------------------------------------------------------------ the run

def make_docs(docs: Path, count: int, size: int, seed: int) -> dict[str, str]:
    rng = random.Random(seed)
    docs.mkdir(parents=True, exist_ok=True)
    keys = {}
    for n in range(1, count + 1):
        name = f"part-{n:02d}"
        key = "".join(rng.choice("ABCDEFGHJKLMNPQRSTUVWXYZ23456789") for _ in range(8))
        keys[name] = key
        lines, length = [], 0
        while length < size:
            line = f"{len(lines) + 1:05d} " + " ".join(rng.choice(WORDS) for _ in range(12))
            lines.append(line)
            length += len(line) + 1
        lines.insert(len(lines) // 2, f"KEY {name}: {key}")
        (docs / f"{name}.txt").write_text("\n".join(lines) + "\n")
    return keys


def config(model: str, key_env: str, effort: str, compact_at: int, proxy_url: str, state: Path) -> str:
    limits = """provider_requests = 60
tool_calls = 80
wall_time = "40m"
shell_timeout = "1m"
tool_calls_per_response = 8"""
    return f"""default_model = "check"

[models.check]
provider = "anthropic"
model = "{model}"
api_key_env = "{key_env}"
base_url = "{proxy_url}"
effort = "{effort}"
thinking = "adaptive"
thinking_display = "summarized"
max_output_tokens = 16000
context_tokens = 1000000
max_history_bytes = 0
cache_ttl = "1h"
context_management = "compact"
compact_at_tokens = {compact_at}

[agent]
limits = "check"

[agent.limit_profiles.check]
{limits}

[session_fabric]
enabled = true
socket = "{state.parent / 'd.sock'}"
state_directory = "{state}"
max_approval = "auto"

[session_fabric.max_limits]
{limits}
"""


TURN_1 = """This is a test of long-context handling; there is no other task.

The directory docs/ holds part-01.txt to part-08.txt. Each holds exactly one line of the form
`KEY part-NN: CODE`. Read the files in order with read_file, one file per response: never two
reads at once, never the shell, and no max_bytes. After each file, write one line, "part-NN:
CODE", before you read the next. When you have read all eight, finish with the eight lines."""

TURN_2 = """Now read docs/part-09.txt and then docs/part-10.txt the same way, one read_file per
response. Then finish with exactly three lines:
part-09: CODE
part-10: CODE
part-02: CODE  (the code you recorded earlier in this conversation; write "unknown" if you no
longer have it)"""


def events(path: Path):
    for line in path.read_text().splitlines():
        try:
            yield json.loads(line)
        except ValueError:
            continue


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--xshell-repo", default=str(Path(__file__).resolve().parent.parent),
                    help="the checkout to build and test (default: the one holding this script)")
    ap.add_argument("--workdir", default=None)
    ap.add_argument("--model", default=os.environ.get("MODEL", "claude-opus-5-5"))
    ap.add_argument("--effort", default=os.environ.get("EFFORT", "high"))
    ap.add_argument("--compact-at", type=int, default=50000)
    ap.add_argument("--upstream", default="https://api.anthropic.com")
    args = ap.parse_args()

    key_env = os.environ.get("KEY_ENV", "ANTHROPIC_API_KEY")
    if not os.environ.get(key_env):
        print(f"set {key_env} to the API key (or KEY_ENV to the variable that holds it)", file=sys.stderr)
        return 2
    repo = Path(args.xshell_repo).resolve()
    stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    work = Path(args.workdir or Path.home() / ".cache/xshell" / f"compaction-check-{stamp}").resolve()
    state, ws = work / "x" / "state", work / "ws"
    state.mkdir(parents=True)  # fails on an existing --workdir: one run per directory
    os.chmod(work / "x", 0o700)
    os.chmod(state, 0o700)
    if len(str(work / "x" / "d.sock")) >= 100:
        print("workdir path too long for the daemon socket; pass a shorter --workdir", file=sys.stderr)
        return 2
    keys = make_docs(ws / "docs", 10, 60000, seed=173)
    requests_log = work / "requests.jsonl"

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), make_proxy(args.upstream, requests_log))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    proxy_url = f"http://127.0.0.1:{server.server_address[1]}"
    cfg = work / "x" / "config.toml"
    cfg.write_text(config(args.model, key_env, args.effort, args.compact_at, proxy_url, state))

    print(f"workdir {work}")
    print("building xshell ...", flush=True)
    subprocess.run(["cargo", "build", "-q", "--release", "--manifest-path", str(repo / "Cargo.toml"),
                    "-p", "xshell-cli"], check=True)
    xshell = repo / "target" / "release" / "xshell"
    env = dict(os.environ, XSHELL_CONFIG=str(cfg), XSHELL_ENV_FILE="/dev/null")
    runall = [str(repo / "scripts" / "runall.sh")]
    # A named session stays in the daemon until it stops; --durable, where the build has it,
    # also keeps it across a restart, for the manual step at the end.
    helptext = subprocess.run([str(xshell), "run", "--help"], capture_output=True, text=True).stdout
    durable = ["--durable"] if "--durable" in helptext else []
    commit = subprocess.run(["git", "-C", str(repo), "rev-parse", "--short", "HEAD"],
                            capture_output=True, text=True).stdout.strip()
    with open(work / "daemon.log", "w") as log:
        subprocess.run(runall + ["start"], env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
    outcomes = {}
    try:
        for n, (prompt, session_args) in enumerate(
                [(TURN_1, [*durable, "--session", "compaction-check"]), (TURN_2, ["--session", "compaction-check"])], start=1):
            (work / f"prompt-{n}.md").write_text(prompt + "\n")
            print(f"turn {n} ...", flush=True)
            with open(work / f"turn-{n}.jsonl", "w") as out, open(work / f"turn-{n}.stderr", "w") as err:
                rc = subprocess.run([str(xshell), "--config", str(cfg), "run", *session_args,
                                     "--output", "jsonl", "--approval", "off", "--limits", "check",
                                     "--prompt-file", str(work / f"prompt-{n}.md")],
                                    cwd=ws, env=env, stdout=out, stderr=err).returncode
            outcomes[n] = rc
            if rc != 0:
                print(f"turn {n} exited {rc}; see turn-{n}.stderr", flush=True)
                break
    finally:
        subprocess.run(runall + ["stop"], env=env, capture_output=True)
        server.shutdown()

    return report(work, keys, commit, outcomes)


def report(work: Path, keys: dict, commit: str, outcomes: dict) -> int:
    requests = list(events(work / "requests.jsonl")) if (work / "requests.jsonl").exists() else []
    requests.sort(key=lambda r: r["seq"])
    checks = []

    def check(name, status, detail):
        checks.append({"check": name, "status": status, "detail": detail})

    bad = [r for r in requests if r.get("status", 0) >= 400 or r.get("stream_errors")]
    check("accepted", "FAIL" if bad or not requests else "PASS",
          [{k: r.get(k) for k in ("seq", "status", "error", "stream_errors", "compaction_at",
                                  "first_is_stand_in", "thinking_after_compaction")} for r in bad]
          or f"{len(requests)} requests, all accepted")

    xshell_compactions = sum((e.get("usage") or {}).get("compactions", 0)
                             for n in outcomes for e in events(work / f"turn-{n}.jsonl")
                             if e.get("event") == "assistant_message")
    compacting = [r for r in requests if r.get("response_compactions")]
    check("compacted", "PASS" if compacting and xshell_compactions else "FAIL",
          f"responses with a compaction block: {[r['seq'] for r in compacting]}; "
          f"xshell counted {xshell_compactions}")

    after = [r for r in requests if compacting and r["seq"] > compacting[0]["seq"]]
    misshapen = [r["seq"] for r in after
                 if not (r.get("first_is_stand_in") and r.get("compaction_at") == [1, 0])]
    check("starts at the summary", "FAIL" if misshapen or not after else "PASS",
          f"requests out of shape: {misshapen}" if misshapen else
          (f"{len(after)} requests after the first compaction, each opening with the stand-in "
           "and the compaction block" if after else "no request followed a compaction"))

    # The request after each compaction must shrink: it starts at the new summary. (Later
    # requests grow back, and the next compaction comes at the same size as the last.)
    by_seq = {r["seq"]: r for r in requests}
    pairs = [(c, by_seq[c["seq"] + 1]) for c in compacting if c["seq"] + 1 in by_seq]
    grew = [(n["seq"], n["body_bytes"], c["body_bytes"]) for c, n in pairs
            if n["body_bytes"] >= c["body_bytes"]]
    check("trimmed", "FAIL" if grew or not pairs else "PASS",
          f"not smaller than the request that compacted (seq, bytes, before): {grew}" if grew else
          "; ".join(f"{c['body_bytes']} bytes at #{c['seq']} -> {n['body_bytes']} at #{n['seq']}"
                    for c, n in pairs) or "no request followed a compaction")

    unbeta = [r["seq"] for r in requests if r.get("compaction_at") and COMPACT_BETA not in r.get("beta", "")]
    check("beta on replay", "FAIL" if unbeta else ("PASS" if after else "WARN"),
          f"replayed a compaction block without {COMPACT_BETA}: {unbeta}" if unbeta else
          "every request with a compaction block sent the beta")

    # xshell's per-response counts: the one-hour writes are a part of all writes.
    inconsistent = [(n, i, u) for n in outcomes for i, u in enumerate(
                        e["usage"] for e in events(work / f"turn-{n}.jsonl")
                        if e.get("event") == "assistant_message" and e.get("usage"))
                    if u.get("cache_write_1h", 0) > u.get("cache_write", 0)]
    check("usage consistent", "FAIL" if inconsistent else "PASS",
          f"cache_write_1h above cache_write (turn, response, usage): {inconsistent}; the API's raw "
          "usage is in requests.jsonl" if inconsistent else "every response's cache_write_1h is within cache_write")

    nulls = [r["seq"] for r in requests if r.get("compaction_null")]
    if nulls:
        check("no null summaries sent", "FAIL", f"requests replaying a null compaction: {nulls}")

    thinking = [r for r in after if r.get("thinking_after_compaction") and r.get("status") == 200]
    check("thinking after compaction", "PASS" if thinking else "WARN",
          f"{len(thinking)} accepted requests replayed signed thinking after the compaction block"
          if thinking else "no thinking was replayed after a compaction, so the prefix check was "
                           "not exercised; rerun with --effort max")

    turn_two = [r for r in requests if r.get("first_is_stand_in")]
    for n, wanted in ((1, [f"part-{i:02d}" for i in range(1, 9)]), (2, ["part-09", "part-10"])):
        path = work / f"turn-{n}.jsonl"
        if n not in outcomes:
            check(f"turn {n}", "FAIL", "not run: an earlier turn failed")
            continue
        result = next((e for e in events(path) if e.get("event") == "result"), {}) if path.exists() else {}
        answer = result.get("answer") or ""
        missing = [p for p in wanted if keys[p] not in answer]
        ok = result.get("outcome") == "completed" and not missing
        check(f"turn {n}", "PASS" if ok else "FAIL",
              {"exit": outcomes.get(n), "outcome": result.get("outcome"), "missing_keys": missing,
               "tokens": (result.get("usage") or {}).get("tokens")})
        if n == 2 and result:
            check("turn 2 remembers part-02", "PASS" if keys["part-02"] in answer else "WARN",
                  "the summary kept a key read before the compaction" if keys["part-02"] in answer
                  else "the key of part-02 did not survive the summary (a quality note, not a failure)")
    if outcomes.get(2) is not None and not turn_two:
        check("turn 2 opens at the summary", "FAIL", "no request started with the stand-in message")

    (work / "report.json").write_text(json.dumps({"xshell_commit": commit, "checks": checks}, indent=2))
    print(f"\nxshell {commit}; {len(requests)} requests; details in {work}/report.json\n")
    for c in checks:
        detail = c["detail"] if isinstance(c["detail"], str) else json.dumps(c["detail"])
        print(f"{c['status']:4}  {c['check']}: {detail}")
    print("\nNot run: replaying the compaction with compaction off. To check it by hand, with a\n"
          "build that has `run --durable` (the session must survive the daemon's restart):\n"
          f"  1. add to {work}/x/config.toml a [models.nocompact] profile, a copy of [models.check]\n"
          "     with context_management = \"none\", no compact_at_tokens and no base_url\n"
          "     (the script's proxy has stopped);\n"
          f"  2. XSHELL_CONFIG={work}/x/config.toml XSHELL_ENV_FILE=/dev/null REPO/scripts/runall.sh start\n"
          f"  3. cd {work}/ws && REPO/target/release/xshell --config {work}/x/config.toml --session compaction-check\n"
          "  4. type //model nocompact, then ask for part-01's key: it must answer with no API error\n"
          "     (requests.jsonl does not log this step);\n"
          "  5. REPO/scripts/runall.sh stop (with the same XSHELL_CONFIG).")
    return 1 if any(c["status"] == "FAIL" for c in checks) else 0


if __name__ == "__main__":
    sys.exit(main())
