# Home-lab service runner

`scripts/runall.sh` builds xshell and manages the services enabled in one TOML
configuration. It requires macOS or Linux, Rust, and Python 3.11 or newer.
It is intended for source-checkout development. For an installed daemon use
`xshelld service` ([session fabric](session-fabric.md#per-user-service-management)),
which manages a per-user launchd or systemd unit; give it separate sockets and
data directories from any runner instance.

Enable `[session_fabric]` and, optionally, `[audit]` in your configuration.
Use absolute socket and data-directory paths. Each directory must belong to
you and have mode `0700`. For separate checkouts or configurations, select
separate service data directories and sockets. Approval, audit requirements,
and model settings retain their configured values.

```sh
./scripts/runall.sh --config /absolute/path/to/config.toml
./scripts/runall.sh status --config /absolute/path/to/config.toml
./scripts/runall.sh stop --config /absolute/path/to/config.toml
./scripts/runall.sh restart --config /absolute/path/to/config.toml
./scripts/runall.sh run --config /absolute/path/to/config.toml -- --approval auto
```

The default action is `run`: build, start or reuse compatible services, check
readiness, then open the CLI. `start` does the same without opening a CLI.
Services stay alive when the CLI exits so sessions can be reattached later.
`restart` builds first, then stops the existing services and starts the new
build. It does not open a CLI. **Stopping or restarting ends active daemon
work and PTYs.** The runner never restarts merely because it detects a change.

`--config` defaults to `XSHELL_CONFIG`, then `~/.config/xshell/config.toml`.
Arguments after `--` go to the CLI; pass `--config` to the runner itself so
every component uses the same configuration. The runner works from any current
directory and preserves that directory for the CLI. Cargo always builds in
the script's checkout; its JSON artifact output locates the executables,
including when `CARGO_TARGET_DIR` is set.

## Ownership and readiness

A detached supervisor owns the actual daemon children. Stop requests go over
a private control socket to that supervisor; the runner never signals a
stored PID or a process selected by name. Locks serialize commands for each
checkout/config pair and prevent cooperating runners from sharing service
directories. The supervisor starts audit first, then sessions. On shutdown
it stops sessions first, allowing 15 seconds for the daemon's 10-second work
cleanup deadline before forced termination; audit remains available until
session shutdown finishes.

Readiness requires a successful protocol exchange and a live child process.
The session probe checks protocol compatibility. `xshell-auditd --probe`
opens and closes a log with bounded socket read/write waits, exercising the
audit write path and leaving an empty, signed audit log. Probe subprocesses
have a five-second deadline; each service has a bounded startup window.
`status` also probes each enabled service and exits unsuccessfully if one is
unresponsive. A child exit or replaced service socket stops the remaining
owned services; failed startup cleans up children already launched.

Existing service sockets, including stale sockets, cause startup to fail.
To migrate from the old runner, stop its daemons using their original terminal
or service manager, then remove their stale sockets after confirming the
owner has exited. The runner does not adopt those processes. Independent
service managers do not honor its locks, so do not configure them to use the
same paths concurrently.

## Build and configuration changes

Each launch takes private snapshots of all three executables and the config.
SHA-256 fingerprints cover their contents, resolved service paths, and the
supervisor implementation. A build performed outside the runner is detected
even if Cargo subsequently reports everything as fresh. Changed binaries or
config require an explicit `restart`; existing sessions continue on their
original snapshot until then. A failed build leaves the existing services
running. Environment credentials are inherited at service startup; restart
to apply changed credentials or other daemon environment settings.

State and logs live in `~/.local/state/xshell/runall/<checkout-config-id>/`.
`--state-root /short/private/path` selects a different root, which must be used
consistently for subsequent commands. The script prints the instance directory.
`supervisor.log`, `xshelld.log`, and `xshell-auditd.log` contain diagnostics.
Build snapshots remain after shutdown for diagnosis and can be deleted with
the runner's instance directory once its services and CLIs have stopped.
Actual session data and audit logs remain in the configured service directories.

A forcibly killed supervisor cannot clean up its children. Subsequent launches
refuse their occupied sockets; stop those services manually before recovery.
This is a single-user development convenience. It runs with the same OS
authority as xshell and does not provide protected administrative supervision
or tamper-resistant audit retention for controlled deployments.

## Regression checks

```sh
python3 -B -m unittest discover -s scripts/tests -v
cargo test -p xshell-audit -p xshell-view
cargo build -p xshell-audit -p xshell-session -p xshell-cli --bins
XSHELL_RUNALL_REAL_BIN_DIR=target/debug python3 -B -m unittest discover -s scripts/tests -v
```

The supervisor tests launch real fixture processes to exercise ownership,
startup failure, unresponsive probes, child crashes, socket replacement,
competing checkouts, changed builds/configs, and CLI attachment. Audit tests
exercise successful probes, unavailable sockets, and nonresponsive peers.
Setting `XSHELL_RUNALL_REAL_BIN_DIR` also runs a smoke test with the compiled
daemons and CLI, using temporary sockets and data directories without a model
provider request. CI runs both forms on macOS and Linux.
