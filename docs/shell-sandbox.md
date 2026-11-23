# Contained shell commands: design

**Status:** proposed, with the decisions of 23 November recorded in §11.
Stage C1 is implemented: the model, the operator's bounds, root resolution
and the runner seam. Nothing enforces containment yet, so a daemon refuses
to create a session that asks for an isolated shell.
**Updated:** 23 November 2026

## 1. Summary

An agent's `run_shell` commands run with the full authority of the account
`xshelld` runs under. The approval policy can prompt for a command or deny one
by pattern, but once a command runs, nothing limits what it touches.

This design lets a session say where its agent's shell commands may work: a
set of directory **roots**. The operating system enforces it while the command
runs, for the command and everything it starts. xshell does not try to work
out from a command's text which files it will touch.

Containment is a property of the session, fixed when the session is created,
like its tool selection ([tool providers §5.2](tool-providers.md#52-per-session-tool-selections)).
A program that embeds the daemon, such as a chat gateway, gives each of its
users' sessions the roots that user may work in.

macOS comes first, using a generated sandbox profile. Linux follows with the
same model. A stronger tier that runs commands in a container or lightweight
virtual machine fits behind the same seam and is sketched in §9.

| Part | LoE (engineer-days) |
|---|---|
| C1. Containment model, session field, operator bounds, protocol, runner seam | 4–5 |
| C2. macOS enforcement (sandbox profile), self-test, tests on macOS CI | 5–7 |
| C3. File tools confined to roots; named secrets; contained user commands | 4–6 |
| C4. Approval ceiling for contained sessions, presets, audit, descriptor, docs | 3–4 |
| C5. Linux enforcement (bubblewrap or Landlock) | 5–7 |
| **Total for macOS (C1–C4)** | **16–22** |

## 2. Why the command's text cannot decide

It is tempting to resolve the paths in a command, follow symbolic links, and
approve the command if every path is inside an allowed directory. That works
for a tool whose arguments are its whole effect. `read_file` is one, and
xshell already confines it that way: the path is walked one component at a
time and the object that walk opened is the object read.

A shell command is a program. `make test`, `python build.py`, `cargo build`
with a build script, a `git` hook, or `npm install` with a postinstall script
open whatever files their code opens. None of those paths are in the command.
The same holds for `$(...)`, variables, `cd`, and anything read from a file.
A check on the text would stop accidents and nothing else.

The findings on shell deny rules show the same limit from the other side: F19
made denials see the programs a command starts, and F31 (#157) lists launchers
that still get past. Pattern rules remain useful as a guard against mistakes.
They are not containment.

So the rule is: **the configuration names directories, xshell resolves those
once, and the kernel enforces them for the whole process tree.**

## 3. The model

### 3.1 Containment levels

The names follow the assurance levels FutureShell already defines
([roadmap §11](futureshell-roadmap.md#11-assurance-levels)), so a contract can
later require one.

| Level | Meaning | Status |
|---|---|---|
| `ambient` | Commands run with the daemon account's full authority. Today's behaviour, and the default | Exists |
| `isolated` | The platform confines file access to declared roots and applies the session's network setting | This design |
| `external` | Commands run in a container or virtual machine with its own boundary | Sketched in §9 |

### 3.2 What a session declares

```rust
pub enum ShellContainment {
    Ambient,
    Isolated(IsolatedShell),
}

pub struct IsolatedShell {
    /// Directories commands may read and write. At least one.
    pub roots: Vec<PathBuf>,
    /// Further directories and files commands may read but not write,
    /// beyond the system paths every command needs (§4.1).
    pub read_only: Vec<PathBuf>,
    /// How much of the network commands may use: `none` (the default),
    /// `loopback`, or `full` (§4.2).
    pub network: ShellNetwork,
    /// Names of secrets the daemon puts in commands' environment (§4.5).
    pub secrets: Vec<String>,
    /// Whether the commands a person types (`$` commands and interactive
    /// programs) are contained too, not only the agent's.
    pub user_commands: bool,
}
```

Every field but `roots` defaults to the most restrictive value: no extra
read-only grants, no network, no secrets, and only the agent's commands
contained.

On the wire, `SessionCreation` and `SessionDescriptor` gain an optional
`shell` field; absent means `ambient`, so existing clients and saved sessions
are unchanged:

```json
"shell": {"isolated": {"roots": ["/Users/rich/src/yapCAD"],
                       "read_only": ["/Users/rich/.cargo", "/Users/rich/.rustup"],
                       "network": "full",
                       "secrets": ["GH_TOKEN"]}}
```

- **Fixed for the session's life.** No request changes it, so a session's
  reach never grows after it is created. To change a user's roots, create a
  new session, as for tools.
- **Persisted** for durable sessions. The session state file format moves on
  by one, so an older daemon refuses the file instead of dropping the field
  and running a contained session's commands ambient.
- **Visible** in the session descriptor and recorded in each turn's audit
  record (§7).

### 3.3 Resolving roots

Roots are resolved once, when the session is created, by the daemon:

1. `~` is expanded for the daemon's account. The path must then be absolute.
2. It is canonicalized: every symbolic link resolved, `.` and `..` removed.
   The canonical path is what is stored, shown, audited and enforced.
3. It must exist and be a directory (`read_only` entries may be files).
4. It may not be `/`, and may not be, contain, or lie inside one of the
   daemon's protected paths: its state directory, configuration file, audit
   directory and control token (the F24 set).
5. The session's working directory must be inside a root.

On macOS the same file can be reached through `/System/Volumes/Data` as well
as through `/` (F32, #158). The sandbox profile is written from the canonical
path, and the kernel's check resolves the path a process actually opens, so a
second spelling does not get around it. The C2 tests cover this case
explicitly, because the daemon's own text matching of blocked paths is known
to miss it.

A root that is later replaced by a symbolic link to somewhere else does not
widen the session: the profile names the canonical path, and the link would
resolve outside it.

### 3.4 What the operator controls

What a session gets is **the most restrictive combination of what it asks
for and what the daemon's own configuration allows.** A session never gets
something it did not ask for, and never gets something the operator's
configuration does not allow.

```toml
[session_fabric.shell]
# Refuse to create a session whose shell is less contained than this.
minimum = "ambient"            # or "isolated"
# Every root a session asks for must be inside one of these.
allowed_roots = ["~/src", "/srv/work"]
# Every read-only grant a session asks for must be inside one of these.
allowed_read_only = ["~/.cargo", "~/.rustup", "/opt/homebrew"]
# The most network access a session may ask for: "none", "loopback" or "full".
max_network = "full"
# The most permissive approval mode for sessions that are isolated (§6).
# Part of stage C4; not accepted yet.
# max_approval_isolated = "auto"

# The secrets a session may ask for by name, and where each comes from (§4.5).
[session_fabric.shell.secrets]
GH_TOKEN = { env = "XSHELL_SECRET_GH_TOKEN" }
```

| The session asks for | The operator's setting | Result |
|---|---|---|
| A root | `allowed_roots` is set and the root is inside one | Granted |
| A root | `allowed_roots` is set and the root is outside all | Session refused |
| A read-only grant | Inside an `allowed_read_only` entry | Granted |
| A read-only grant | Outside all, or the list is absent | Session refused |
| A network level | At or below `max_network` | Granted |
| A network level | Above `max_network`, or `max_network` is absent | Session refused |
| A secret by name | Named in the `secrets` table | Injected |
| A secret by name | Not in the table | Session refused |

A request outside the bounds refuses the session when it is created, with
the reason. It is not silently narrowed: a session that quietly lacked its
network or a grant would fail later in ways that are hard to trace.

One bound has a different default. When `allowed_roots` is absent, a session
may name any root (subject to §3.3), because a host that allows ambient
shells already lets a session's commands reach everything, and a contained
session only ever reaches less. Setting `minimum = "isolated"` requires
`allowed_roots` to be set.

With `minimum = "isolated"` no client can create a session whose agent has an
ambient shell. That is the setting for a host where the operator does not
fully trust every client.

### 3.5 Presets, for clients that cannot describe containment

A client built on the library sends the `shell` field itself. `xshell run`
creates its own session and has no such field to fill in, and an unattended
runner should not be naming its own roots on a host that requires isolation.
For both, the operator can define named presets:

```toml
[session_fabric.shell.presets.bench]
roots = ["/srv/bench/work"]
read_only = ["/srv/bench/venv", "/srv/bench/renderer"]
network = "none"
```

```sh
xshell run --shell bench --approval auto "Render every part"
```

A session created with `--shell NAME` gets exactly that preset, resolved and
fixed as in §3.3. A preset is the operator's own choice, so it is not
checked against `allowed_roots` and the other bounds; it is the natural way
to use a host whose `minimum` is `isolated`. An unknown name refuses the
session. Presets are part of stage C4.

## 4. What a contained command can do

### 4.1 Files

| Path | Access |
|---|---|
| The session's roots | Read and write |
| A scratch directory made for the session, set as `TMPDIR` | Read and write; removed with the session |
| System paths: `/usr`, `/bin`, `/sbin`, `/System`, `/Library`, `/private/etc`, `/private/var/db/timezone`, selected `/dev` nodes | Read, and execute |
| The session's `read_only` grants, within the operator's bounds | Read, and execute |
| Everything else, including the rest of the home directory | Nothing |

The last row is the point. A contained command cannot read `~/.ssh`, other
users' data, shell history, browser profiles, or the daemon's state, and it
cannot write outside its roots.

Two consequences need saying plainly:

- **Toolchains must be granted.** `~/.cargo`, `~/.rustup`, `~/.nvm`,
  Homebrew, and similar are outside the system paths. They go in `read_only`.
  A tool that writes a cache in the home directory (`~/.cargo/registry`,
  `~/.npm`) needs that cache redirected into a root or the scratch directory
  through its own environment variable (`CARGO_HOME`, `npm_config_cache`).
  The session's environment allowlist will need to carry such variables, so
  the runner sets them from the session's containment (part of C2).
- **`HOME` is the scratch directory.** Tools that read their configuration
  from the home directory start with none, rather than failing on one they
  cannot read.
- **Credentials are not inherited.** `~/.gitconfig`, `~/.ssh` and the macOS
  keychain are out of reach. A contained command gets a credential only as
  a named secret the daemon injects (§4.5). This is a feature: a contained
  agent should hold a credential that can do the job and no more.

### 4.2 Network

A session asks for one of three levels. The default is `none`, and the
operator's `max_network` caps what may be asked for.

| Level | What commands may do |
|---|---|
| `none` | No network |
| `loopback` | Listen on, and connect to, this machine's loopback addresses. This is what test suites and local development servers need |
| `full` | Loopback, plus outbound connections anywhere and name resolution |

**Unix sockets inside the roots and the scratch directory are allowed at
every level.** They are files in directories the session already owns, and
tools use them to talk to their own helpers. Unix sockets anywhere else are
denied at every level, which is what keeps the daemon's socket and agent
sockets (ssh, gpg, Docker) out of reach (§4.3).

`loopback` is not harmless. It reaches every service listening on this
machine: a local model server, a database, a Docker API on a TCP port. An
operator who runs such services should think before allowing it.

The macOS sandbox can allow or deny the network, and can limit it to the
local machine or to given ports. It cannot filter by host name. A session
that needs "only `github.com`" would need a filtering proxy that the sandbox
allows as its only destination. That is not needed yet and is not part of
this work.

With `full`, **anything a command can read, it can send somewhere.** Roots
and `read_only` grants are therefore also the list of what could leak.

### 4.3 The daemon

A contained command cannot open the daemon's socket or read its control
token: neither is in a granted path, and the profile denies connecting to
Unix sockets outside the roots. This is a second, kernel-enforced layer under
the F27 check, and it closes the F27 gaps (a double-forked process on macOS,
a process started through something outside the daemon's tree) for contained
sessions.

### 4.4 Processes

Commands may start other programs; the sandbox is inherited by every
descendant and cannot be dropped. They may signal only processes in the same
sandbox, and should not be able to see other processes' command lines.
Process-group cleanup on timeout and cancellation (F2, F17) is unchanged,
with its known limit: a process that leaves its group with `setsid` is not
killed with it. Such a process is still inside the sandbox, so it keeps the
session's limits, but it can outlive the command. On Linux, bubblewrap's
`--unshare-pid --die-with-parent` closes that gap (C5); the macOS runner
does not.

A contained command has no terminal. `/dev/tty` is the daemon's own
terminal when `xshelld` runs in the foreground, and an agent's command must
not read or write it. A terminal device is granted only for the commands a
person types, when the session sets `user_commands` (§8).

Asking another application to act is the classic way out of a sandbox:
`open` handing a file to LaunchServices, `osascript` sending an Apple
event. Both go through system services the profile must leave denied. The
probe script checks them, and so must the self-test (§5).

CPU, memory and process-count limits are not part of this design.

### 4.5 Secrets

A contained command gets a credential as a **named secret**. The session
asks for it by name; the operator's `secrets` table says where its value
comes from; the daemon puts it in the command's environment under that name.
The value is never in a file a command reads, never in the session's
description, and never sent over the session protocol.

- **Source.** The first source is a variable in the daemon's own
  environment (`{ env = "NAME" }`). The daemon's environment is already kept
  from agent commands (F18), and each source variable is registered as a
  credential variable so it is stripped everywhere it was not asked for. A
  keychain source can follow.
- **Scope.** Only sessions that ask for a secret by name get it, and only
  if the operator's table defines it. An ambient session cannot ask for
  secrets: this is a feature of contained sessions.
- **What is recorded.** The names of the secrets a turn's commands could
  see, in the audit record. Never a value.
- **What it does not protect against.** Every process in the sandbox can
  read the variable, and with the network on, can send it. A command can
  also print it, which puts it in the tool's output and so in the model's
  context and the audit log. The daemon replaces exact occurrences of an
  injected value in tool output with a placeholder; that catches the plain
  case and not an encoded one. So a secret should be scoped to the work: a
  token that can open pull requests on one repository, not an account
  password.

## 5. Enforcement on macOS

`run_shell` builds a profile from the session's resolved containment and
starts the shell under it:

```text
/usr/bin/sandbox-exec -p PROFILE -D ROOT_0=... -D TMP=... $SHELL -c COMMAND
```

The profile is generated, never read from a file a command could change:

```scheme
(version 1)
(deny default)
(import "system.sb")                             ; dyld, libSystem start-up
(allow process-fork process-exec*)
(allow signal (target same-sandbox))             ; its own rule: see below
(allow sysctl-read)
(allow file-read* file-map-executable
       (subpath "/usr") (subpath "/bin") (subpath "/sbin")
       (subpath "/System") (subpath "/Library") (subpath "/private/etc"))
(allow file-read* file-write* (literal "/dev/null"))   ; no /dev/tty: §4.4
(allow file-read* (literal "/dev/urandom") (literal "/dev/random"))
(allow file-read-metadata)                       ; stat, for path walks
(allow file-read* file-write* (subpath (param "ROOT_0")) (subpath (param "TMP")))
(allow file-read* file-map-executable (subpath (param "RO_0")))
;; unix sockets inside the roots and scratch directory, at every level:
(allow network-outbound (subpath (param "ROOT_0")) (subpath (param "TMP")))
(allow network-bind network-inbound (subpath (param "ROOT_0")) (subpath (param "TMP")))
;; network = "loopback" and "full":
(allow network-bind network-inbound (local ip "localhost:*"))
(allow network-outbound (remote ip "localhost:*"))
;; network = "full":
(allow network-outbound (remote ip "*:*"))       ; IP only, not unix sockets
(allow network-outbound (literal "/private/var/run/mDNSResponder"))
(allow mach-lookup (global-name "com.apple.mDNSResponder"))
```

**Not yet measured:** the unix-socket and loopback rules above, the removal
of `/dev/tty`, and the checks for `open`, `osascript`, the keychain and
other processes' command lines were added after the macOS 26.6 run, from a
machine that cannot run `sandbox-exec`. The probe script has checks for all
of them. Treat these rules as proposals until it has been run again.

The shell runs with `HOME` and `XDG_CONFIG_HOME` set to the scratch
directory and `TMPDIR` set to it as well (§4.1).

This sketch is a starting point. The exact rule set is the main work of C2
and will be settled by running real toolchains under it (see §10).

Things known to need care:

- **Measured on macOS 26.6 (Apple silicon)** with
  `scripts/macos-shell-sandbox-probe.sh`, which runs the profile above with
  the network off and on, plus the two broken variants below:
  - Without `(import "system.sb")` the shell aborts at start-up (SIGABRT).
  - A filter on a rule applies to every operation in it. Written as
    `(allow process-fork process-exec* signal (target same-sandbox))`, the
    target filter also applies to exec, and `execvp` of the shell is denied.
    Signals therefore get their own rule.
  - An unfiltered `(allow network-outbound)` also allows connecting to any
    unix socket, including the daemon's own and ssh, gpg or Docker agent
    sockets. `(remote ip "*:*")` allows IP only. Name resolution then
    needs the `mDNSResponder` socket as a literal, or every lookup fails.
  - As measured, with the network off, unix sockets inside the roots were
    blocked too, and the "network on" rule (IP only) would not have allowed
    them either. The profile now allows them at every level (§4.2); that
    rule is one of those not yet measured.
  - `(import "system.sb")` brings in Apple's base profile, which allows a
    set of system services and changes between macOS versions. The profile
    hash in the audit record does not cover it, so the record also carries
    the macOS version and build. What the import lets through is exactly
    what the escape checks are for.
  - git exits 128 on an unreadable `~/.gitconfig` (a missing one is fine),
    which is why `HOME` points at the scratch directory.
  - The `/usr/bin` shims (`git`, `python3`, …) run `xcrun`, which writes a
    cache into `$(getconf DARWIN_USER_TEMP_DIR)` whatever `TMPDIR` says. The
    write is denied and each call prints `couldn't create cache file`, but
    the command still works. The runner should put the developer directory
    (`xcode-select -p`, then `usr/bin`) ahead of `/usr/bin` on `PATH` rather
    than grant the per-user temp directory, which holds other processes'
    files.

- **`sandbox-exec` is marked deprecated** in its manual page, and has been for
  years. It is still present and is what Chromium, Bazel and several coding
  agents use, since Apple offers no public replacement for sandboxing an
  arbitrary child process. If it is ever removed, the fallback is the same
  profile applied through `sandbox_init` in a small helper. The runner seam
  (§8) keeps that change in one place.
- **A sandbox cannot be nested.** If `xshelld` itself runs inside a sandbox,
  `sandbox-exec` fails. The daemon detects this at startup (below) and
  refuses contained sessions.
- **Path spellings.** `/tmp`, `/var` and `/etc` are symbolic links into
  `/private`. Profiles use the canonical forms.
- **Stat is allowed everywhere** (`file-read-metadata`), because programs
  walk paths. A contained command can therefore learn that a file exists and
  its size, though not its contents. Tightening this breaks many tools.
- **Mach services.** Most system services are reached by Mach lookup, which
  is denied by default. Name resolution needs one; the keychain needs
  several and is deliberately not granted.

**Fail closed, on this host's own evidence.** At startup, on a host where
isolated sessions are allowed, the daemon runs a self-test that covers every
claim the level makes, not a sample of them (ADR 0002 lets a provider report
`isolated` only after it "has passed the denial and resource tests for every
enforced claim"). A contained command must fail to:

- write outside its roots, and write to a read-only grant;
- read a canary file outside its roots, directly, through a symbolic link
  inside a root, and through the `/System/Volumes/Data` spelling;
- connect to the daemon's socket and read its control token, at every
  network level the host allows;
- connect out at the `none` level, and beyond this machine at `loopback`;
- hand a file to another application with `open`, or send an Apple event;
- signal a process outside the sandbox.

Each denial is paired with a control: the same command must succeed outside
the sandbox, or the test cannot tell a denial from an unrelated failure and
counts as failed. The outcome goes in the audit log as a `sandbox_probe`
record with the platform, the macOS version and build, and the profile hash,
so a host's claim rests on that host's test and not on a CI runner's.

If any check fails, or the sandbox cannot be started, isolated sessions are
refused on that host with a clear reason. A session that is `isolated` never
falls back to running a command ambient; the command fails and the model is
told.

`xshelld` itself must not be run inside a sandbox on a host that offers
isolated sessions, because sandboxes do not nest (above). Harnesses that
wrap their workers in `sandbox-exec` need to leave the daemon outside it.

## 6. Approval

Containment and approval answer different questions. Containment bounds what
a command can reach. Approval decides whether a person looks at it first.

Today the daemon has one ceiling, `max_approval`. An operator who wants
unattended contained work has to raise it to `auto` for every session,
ambient ones included. This design adds a second ceiling,
`max_approval_isolated`, that applies to sessions whose shell is isolated.
The useful setting is then:

```toml
[session_fabric]
max_approval = "ask"             # ambient shells still ask
[session_fabric.shell]
max_approval_isolated = "auto"   # contained shells may run unattended
```

The approval policy's rules apply unchanged in both. A `denied` rule still
refuses a command inside a sandbox.

## 7. Audit and reporting

- `turn_started` gains a `shell` field for sessions that are not ambient: the
  level, the canonical roots and read-only grants, the network setting, and
  the SHA-256 of the generated profile. A reviewer can see what a turn's
  commands could reach.
- The session descriptor carries the same, so catalogs and `xshell status`
  show it.
- A command the sandbox blocks simply sees "Operation not permitted". xshell
  cannot reliably tell that apart from an ordinary permission error, so tool
  results are not labelled. The denial itself is in the system log.

## 8. The runner seam

`run_shell` today builds a `Command` for `$SHELL -c` and supervises it. C1
puts one trait between the two:

```rust
/// How a session's shell commands are started.
pub trait ShellRunner: Send + Sync {
    /// The level this runner enforces.
    fn assurance(&self) -> ShellAssurance;          // Ambient | Isolated | External
    /// The command line that runs `command` in `cwd` under this runner.
    fn command(&self, shell: &Path, command: &str, cwd: &Path) -> Result<Command>;
    /// A stable description of what is enforced, for audit.
    fn describe(&self) -> serde_json::Value;
}
```

The ambient runner is today's code. The macOS runner wraps the same command
in `sandbox-exec`. Everything after spawning is shared: the process group,
bounded output, the timeout, cancellation, the linger grace, the filtered
environment and descriptor hygiene.

This is the "interface for enforced providers" that roadmap milestone M2 asks
for, limited to shell execution. It does not change model calls or the file
tools.

**MCP servers.** The [MCP bridge](mcp-bridge.md) plans to sandbox each local
server (§3.4 there) with its own `read`, `write` and `network` keys. Those
servers should use this design's containment model and runner, so that the
profile rules and their tests exist once. The bridge's keys then become a
containment declaration per server.

**What uses it.**

- **The agent's `run_shell`**, including in subagents, which inherit the
  session's containment.
- **The commands a person types**, when the session sets `user_commands`:
  direct `$` commands and interactive programs then start through the same
  runner. Without it they are the user acting as themselves and run as they
  do today. A client that hands its users a shared host can ask for it; the
  profile then also allows the session's own terminal device.
- **The built-in file tools.** In an isolated session `read_file`,
  `list_directory` and `view_image` are confined to the session's roots,
  not to its working directory: a path is accepted when its resolved
  location is inside a root. They do not read the `read_only` grants, which
  exist for toolchains. Relative paths are still taken from the working
  directory. The same prepared, link-safe walk is used, started from the
  root that contains the path, and the sensitive-path and blocked-path
  rules apply as before.

## 9. A stronger tier: containers and virtual machines

A sandbox profile shares the host's kernel and file system. For work that
should not see the host at all, such as untrusted repositories or FutureShell
tasks that must return only declared artifacts, the `external` level runs
commands in a container or a lightweight virtual machine.

The same model carries over: roots become mounts into the guest, `read_only`
grants become read-only mounts or are replaced by a prepared image, and
`network` becomes the guest's network setting. An `external` runner
implements the same trait; its `command` is the invocation that executes in
the guest.

Candidates to evaluate, not yet chosen:

| Platform | Option | Notes |
|---|---|---|
| macOS | Apple's `container` tool and Containerization framework | One lightweight VM per container; needs a recent macOS on Apple silicon |
| macOS | Virtualization.framework through Lima or a small helper | A long-lived Linux VM with shared directories |
| Linux | bubblewrap or rootless Podman | Namespaces, no VM |
| Linux | Firecracker or a similar microVM | A real VM boundary, at the cost of image management |

What changes compared with `isolated`:

- Commands run on Linux in the guest even on a macOS host, so the toolchain
  is the image's, not the host's. That suits reproducible builds and does not
  suit work that needs macOS tools.
- Start-up cost and state: a guest per command is slow, so the runner would
  keep one per session, which then has state between commands. That needs a
  lifecycle (start with the first command, stop with the session) much like
  the MCP bridge's per-session server instances.
- File sharing has its own semantics and speed; writes in a root must still
  be attributable to the session.

This is a separate piece of work with its own design. The point here is only
that C1's seam and containment model do not have to change to admit it.

## 10. Tasks and estimates

| # | Task | LoE (days) |
|---|---|---|
| C1 (done) | `ShellContainment` on session creation and descriptor; root resolution and validation; the operator's `[session_fabric.shell]` bounds and the most-restrictive rule; persisted with a state-format bump; session protocol bump; `ShellRunner` trait with the ambient runner. A host with no enforcing runner refuses isolated sessions | 4–5 |
| C2 | macOS runner: profile generation, the scratch directory and environment, the startup self-test covering every claim with paired controls (§5), the `sandbox_probe` audit record, fail-closed behaviour. Tests on the macOS CI runner: writes and reads outside the roots fail, including through a symbolic link, `..`, and the `/System/Volumes/Data` spelling; the daemon socket and token are unreachable, with the network off and on; network off blocks a connection; a `git` clone, a `cargo build` and a Python script work inside a root with the documented grants | 5–7 |
| C3 | File tools confined to roots in isolated sessions; named secrets with output redaction; `user_commands` for `$` commands and interactive programs | 4–6 |
| C4 | `max_approval_isolated`; named presets and `xshell run --shell NAME` (§3.5); `turn_started` and descriptor fields with an audit format bump; documentation; the security ledger | 3–4 |
| C5 | Linux runner with the same tests. bubblewrap where present, with `--unshare-pid --die-with-parent` so a command that leaves its process group dies with its parent. Landlock otherwise; its network rules cover only TCP bind and connect, so the `none` level also needs a seccomp filter refusing sockets other than `AF_UNIX` | 5–7 |

**Risks:**

- **Real toolchains.** The profile that lets `cargo`, `git`, `python`, `node`
  and Xcode's command-line tools work is found by trying them. Expect
  iteration, and expect some tools to need a documented grant.
- **`sandbox-exec` deprecation** (§5).
- **A false sense of safety.** With the network on, containment limits what
  can be reached, not what can be sent. The documentation has to say so
  wherever the feature is described.

## 11. Decisions and open questions

Decided on 23 November 2026:

| Question | Decision |
|---|---|
| Default for `network` | Off. A session turns it on explicitly, and the operator must allow it (§4.2, §3.4) |
| Filtering by host | Not needed yet; not part of this work |
| File tools | Confined to the session's roots, not its working directory (§8) |
| Who may grant | The most restrictive combination of the operator's configuration and the session's request (§3.4) |
| Credentials | The daemon injects named secrets; no token files (§4.5) |
| Commands a person types | A session can ask for them to be contained too (`user_commands`, §8) |

Adopted from the review of this document (#167, #169), also on 23 November:

| Point | Outcome |
|---|---|
| Network levels | Three, not two: `none`, `loopback`, `full`. Loopback exists because test suites bind local ports (§4.2) |
| Unix sockets | Allowed inside the roots and scratch directory at every level; denied elsewhere at every level (§4.2) |
| The terminal | No `/dev/tty` for an agent's commands (§4.4) |
| Self-test | Covers every claim, with a control for each denial, and is recorded in the audit log (§5) |
| `xshell run` | Asks for containment through operator-defined presets (§3.5) |
| MCP servers | Use this model and runner, not a second one (§8) |

Still open:

1. **Keychain as a secret source.** An environment variable in the daemon's
   environment is the first source. Reading from the macOS keychain or the
   Linux secret service is the natural second one; the MCP bridge plans the
   same storage for its tokens, and the two should share it.
2. **How far output redaction goes.** Exact matches of an injected value are
   replaced. Is that worth doing at all, given it cannot catch an encoded
   value, or does it give a false sense of safety?
3. **`sandbox-exec` deprecation.** Whether to write the `sandbox_init`
   helper now or only if the tool is removed (§5).
4. **Interactive programs under a sandbox.** Full-screen programs need the
   session's terminal device and sometimes more. How much breaks under the
   profile is unknown until C3 tries it.
5. **A GPU grant.** Inside a macOS sandbox, Metal finds no adapter, so GPU
   renders fail. A documented grant for the IOKit classes Metal needs (on
   Linux, `/dev/dri` and `/dev/nvidia*`) would save each toolchain finding
   this out. It is not needed for the first use and is not designed yet.
6. **What the base profile allows.** `(import "system.sb")` is needed for a
   shell to start, and nobody has yet listed the services it lets through.
   The probe checks the known escapes; a review of the import itself, or a
   hand-written replacement for it, may be worth doing before this is relied
   on against a hostile command.
