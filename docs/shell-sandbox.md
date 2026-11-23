# Contained shell commands: design

**Status:** proposed; nothing here is implemented.
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
| C1. Containment model, session field, protocol, runner seam | 3–4 |
| C2. macOS enforcement (sandbox profile), self-test, tests on macOS CI | 5–7 |
| C3. Approval ceiling for contained sessions, audit, descriptor, docs | 2–3 |
| C4. Linux enforcement (bubblewrap or Landlock) | 5–7 |
| **Total for macOS (C1–C3)** | **10–14** |

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
    /// Whether commands may use the network.
    pub network: bool,
}
```

On the wire, `SessionCreation` and `SessionDescriptor` gain an optional
`shell` field; absent means `ambient`, so existing clients and saved sessions
are unchanged:

```json
"shell": {"isolated": {"roots": ["/Users/rich/src/yapCAD"],
                       "read_only": ["/Users/rich/.cargo", "/Users/rich/.rustup"],
                       "network": true}}
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

The daemon's own configuration bounds what any client may ask for. A client,
including a gateway, can only stay within it.

```toml
[session_fabric.shell]
# Refuse to create a session whose shell is less contained than this.
minimum = "ambient"            # or "isolated"
# When set, every root a session asks for must be inside one of these.
allowed_roots = ["~/src", "/srv/work"]
# Read-only grants every isolated session gets, for toolchains.
read_only = ["~/.cargo", "~/.rustup", "/opt/homebrew"]
# The most permissive approval mode for sessions that are isolated (§6).
max_approval_isolated = "auto"
```

With `minimum = "isolated"` no client can create a session whose agent has an
ambient shell. That is the setting for a host where the operator does not
fully trust every client.

## 4. What a contained command can do

### 4.1 Files

| Path | Access |
|---|---|
| The session's roots | Read and write |
| A scratch directory made for the session, set as `TMPDIR` | Read and write; removed with the session |
| System paths: `/usr`, `/bin`, `/sbin`, `/System`, `/Library`, `/private/etc`, `/private/var/db/timezone`, selected `/dev` nodes | Read, and execute |
| The operator's `read_only` list, and the session's | Read, and execute |
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
  C1 adds a per-session list of extra variables to set.
- **Credentials are not inherited.** `~/.gitconfig`, `~/.ssh` and the macOS
  keychain are out of reach unless granted. For pushing code, the intended
  route is a narrow credential the operator places deliberately: a token in
  a file under a root or `read_only`, used through git's `credential.helper
  store` or an environment variable. This is a feature. A contained agent
  should hold a credential that can do the job and no more.

### 4.2 Network

`network = false` denies all network access. `network = true` allows outbound
connections and name resolution.

The macOS sandbox can allow or deny the network, and can limit it to the
local machine or to given ports. It cannot filter by host name. A session
that needs "only `github.com`" needs a filtering proxy that the sandbox allows
as its only destination. That is out of scope here and noted in §11.

With the network on, **anything a command can read, it can send somewhere.**
Roots and `read_only` grants are therefore also the list of what could leak.

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
sandbox. Process-group cleanup on timeout and cancellation (F2, F17) is
unchanged.

CPU, memory and process-count limits are not part of this design.

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
(allow process-fork process-exec* signal (target same-sandbox))
(allow sysctl-read)
(allow file-read* file-map-executable
       (subpath "/usr") (subpath "/bin") (subpath "/sbin")
       (subpath "/System") (subpath "/Library") (subpath "/private/etc"))
(allow file-read* file-write* (literal "/dev/null") (literal "/dev/tty"))
(allow file-read* (literal "/dev/urandom") (literal "/dev/random"))
(allow file-read-metadata)                       ; stat, for path walks
(allow file-read* file-write* (subpath (param "ROOT_0")) (subpath (param "TMP")))
(allow file-read* file-map-executable (subpath (param "RO_0")))
;; with network = true:
(allow network-outbound)
(allow mach-lookup (global-name "com.apple.mDNSResponder"))
```

This sketch is a starting point. The exact rule set is the main work of C2
and will be settled by running real toolchains under it (see §10).

Things known to need care:

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

**Fail closed.** At startup, on a host where isolated sessions are allowed,
the daemon runs a self-test: a contained command that tries to write outside
its root and to read a canary file outside it. If either succeeds, or the
sandbox cannot be started, isolated sessions are refused on that host with a
clear reason. A session that is `isolated` never falls back to running a
command ambient; the command fails and the model is told.

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

**What uses it.** The agent's `run_shell`, including in subagents, which
inherit the session's containment. Not the direct `$` commands a person types
or their interactive programs: those are the user acting as themselves. The
built-in file tools keep their own confinement to the working directory,
which §3.3 requires to be inside a root.

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
| C1 | `ShellContainment` on session creation and descriptor; root resolution and validation; daemon `[session_fabric.shell]` bounds; persisted with a state-format bump; session protocol bump; `ShellRunner` trait with the ambient runner; per-session extra environment variables | 3–4 |
| C2 | macOS runner: profile generation, the scratch directory, the startup self-test, fail-closed behaviour. Tests on the macOS CI runner: writes and reads outside the roots fail, including through a symbolic link, `..`, and the `/System/Volumes/Data` spelling; the daemon socket and token are unreachable; network off blocks a connection; a `git` clone, a `cargo build` and a Python script work inside a root with the documented grants | 5–7 |
| C3 | `max_approval_isolated`; `turn_started` and descriptor fields with an audit format bump; documentation; the security ledger | 2–3 |
| C4 | Linux runner (bubblewrap where present, Landlock otherwise) with the same tests | 5–7 |

**Risks:**

- **Real toolchains.** The profile that lets `cargo`, `git`, `python`, `node`
  and Xcode's command-line tools work is found by trying them. Expect
  iteration, and expect some tools to need a documented grant.
- **`sandbox-exec` deprecation** (§5).
- **A false sense of safety.** With the network on, containment limits what
  can be reached, not what can be sent. The documentation has to say so
  wherever the feature is described.

## 11. Open questions

1. **Default for `network`.** Off is safer and breaks `git fetch` and
   dependency downloads. On is what coding needs. Should the field be
   required, with no default?
2. **Host filtering.** Is "only these hosts" needed soon enough to design the
   filtering proxy with this work, or can it wait?
3. **File tools and roots.** Should `read_file`, `list_directory` and
   `view_image` be confined to the roots instead of to the working
   directory, so an agent can read across all its roots without a shell?
4. **Where grants for toolchains live.** Only in the operator's
   configuration (as above), or may a session add its own `read_only`
   entries within `allowed_roots`-style bounds?
5. **Credentials.** Is a token file under a granted path enough, or should
   the daemon inject a named secret into a contained command's environment,
   as the MCP bridge plans to for servers?
6. **`$` commands.** Should a session be able to ask for its user-typed
   commands to be contained too, for a client that wants that?
