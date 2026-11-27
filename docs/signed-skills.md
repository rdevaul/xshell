# Signed skills: design

**Status:** stages K0 (skill-signer writes manifest version 2) and K1 (the
`xshell-skills` verifier crate) are implemented. Nothing loads a skill yet:
the store, sessions and the registry (K2–K5) are not built.
**Updated:** 27 November 2026

## 1. Summary

A **skill** is a directory of instructions, and sometimes scripts, that
teaches an agent how to do one kind of task: `SKILL.md`, plus whatever files
it refers to. This design lets xshell use skills, with three rules:

1. **Only signed skills.** A skill is usable only if a signer the operator
   trusts signed exactly the files xshell loads. Signing uses
   [skill-signer](https://github.com/rdevaul/skill-signer)'s format, with one
   change to it (§3).
2. **Off by default, and installed by a person.** A daemon has no skills
   until its operator turns the feature on, names trusted signers, and
   installs each skill. An agent can never install or enable one.
3. **Scripts run contained.** A skill that brings scripts is usable only in
   a session whose shell is contained
   ([contained shell commands](shell-sandbox.md)), unless the operator
   explicitly allows otherwise.

A signature says who published a skill and that it has not changed since. It
does not say the skill is safe. Signing and containment therefore come
together: signing decides what may be loaded, containment bounds what it can
do.

| Part | LoE (engineer-days) |
|---|---|
| K0. skill-signer: manifest version 2 (separate repository) | 2–3 |
| K1. Verifier: signature, trusted signers, manifest, directory check | 4–5 |
| K2. Skill store; install, list, remove, verify | 3–4 |
| K3. Skills in sessions: selection, index, `skill_read`, audit | 4–5 |
| K4. Script skills under containment (needs sandbox stage C2) | 2–3 |
| K5. Registry fetch and revocation | 3–4 |
| **Total** | **18–24** |

K1 to K3 do not depend on the sandbox and give signed, instruction-only
skills. K4 waits for enforcement to exist.

## 2. What signing does and does not give

| Question | Answered by |
|---|---|
| Who published this skill? | The signature, checked against the operator's trusted signers |
| Has it changed since it was signed? | The manifest's file hashes, checked every time a file is loaded |
| May this signer publish a skill by this name? | A pin made when the operator first installs the name (§4.3) |
| Is the skill safe to follow or run? | **Nothing here.** A trusted signer can publish a harmful skill, and a stolen key can sign one |
| What can a skill's script reach if it is harmful? | The session's containment (§7) |
| What about packages a script installs? | Not signed. Containment is the only limit |

## 3. The signed form (skill-signer manifest version 2)

### 3.1 Why version 1 will not do

skill-signer today writes one file, `MANIFEST.sig.json`, holding the manifest
and its signature together. The signature covers a re-encoding of the
manifest: Python's `json.dumps` with sorted keys and compact separators,
which also escapes every non-ASCII character. A verifier in another language
has to reproduce that encoding byte for byte, and any difference makes a
genuine skill fail or, worse, lets two programs disagree about what was
signed. xshell's audit log had the same problem and solved it the same way
(F14): sign the bytes as stored.

Version 1 also leaves three gaps a loader has to close:

- **Unlisted files.** The manifest skips dotfiles, `node_modules`,
  `__pycache__` and `.pyc` files. A signed skill directory can therefore
  hold unsigned code.
- **Symbolic links.** A linked file is hashed by its target's content, so
  the manifest can describe a file that lives outside the skill.
- **Self-asserted identity.** The signer's identity is read from the
  manifest being verified.

### 3.2 Version 2

Two files at the top of the skill directory:

| File | Content |
|---|---|
| `MANIFEST.json` | The manifest, as JSON |
| `MANIFEST.json.sig` | An OpenSSH signature over the exact bytes of `MANIFEST.json`, namespace `skill-manifest` |

This is what `ssh-keygen -Y sign -n skill-manifest -f KEY MANIFEST.json`
writes, so a skill can be signed and checked with stock OpenSSH and no other
tool:

```sh
ssh-keygen -Y verify -f allowed_signers -I alice@example.com \
    -n skill-manifest -s MANIFEST.json.sig < MANIFEST.json
```

Nothing is re-encoded. The verifier reads the file's bytes, checks the
signature over them, and only then parses them.

```json
{
  "manifest_version": 2,
  "name": "yapcad",
  "version": "1.4.0",
  "description": "Design parametric parts with yapCAD and export STEP.",
  "files": {
    "SKILL.md":           {"sha256": "…", "size": 4180},
    "scripts/export.py":  {"sha256": "…", "size": 912}
  },
  "scripts": ["scripts/export.py"],
  "requires": {
    "network": "none",
    "secrets": [],
    "read_only": []
  }
}
```

| Field | Meaning |
|---|---|
| `name` | 1–64 characters of `a-z`, `0-9`, `-` and `_`, starting with a letter |
| `version` | The publisher's version string, for people. Identity is the manifest's hash |
| `description` | One or two sentences, at most 500 characters. This is what the model sees in the skill index (§6.2) |
| `files` | **Every** file in the skill, by path relative to its top, with SHA-256 and size. No exclusions |
| `scripts` | The listed files the skill may ask an agent to run. Empty or absent for an instruction-only skill |
| `requires` | What the skill needs from a contained shell (§7). Absent means nothing |

The signer's identity is not in the manifest. It comes from the operator's
trusted-signers file, by the key that made the signature (§4.2).

**The manifest's identity** is the SHA-256 of `MANIFEST.json`'s bytes. It
names one exact skill: every file, every byte.

### 3.3 Checking a skill directory

A directory is a valid skill only if all of these hold:

1. `MANIFEST.json` and `MANIFEST.json.sig` exist and are regular files, and
   `MANIFEST.json` is at most 1 MiB.
2. The signature verifies over the bytes of `MANIFEST.json`, in namespace
   `skill-manifest`, by a key a trusted signer holds, and that signer is
   not revoked (§4).
3. The manifest parses strictly: no unknown fields, no duplicate keys,
   `manifest_version` is 2.
4. Every entry in the directory, at any depth, is a regular file or a
   directory. A symbolic link, device, socket or FIFO fails the skill.
5. The set of regular files is exactly `files` plus the two manifest files.
   A file that is not listed fails the skill; so does a listed file that is
   missing. Dotfiles are files like any other.
6. Each listed file has the listed size and SHA-256.
7. Each path is relative, uses `/`, has no `.` or `..` component, and is
   unique when letter case is ignored (so a skill behaves the same on
   case-insensitive volumes).
8. Each `scripts` entry is in `files`.
9. Limits: at most 512 files, 64 MiB in total, and paths of at most 255
   bytes.

Version 1 manifests are not accepted. skill-signer gains `sign` writing
version 2 and keeps `verify` for version 1 so existing skills can be checked
and re-signed.

## 4. Trust

### 4.1 Configuration

```toml
[skills]
enabled = false
# OpenSSH allowed_signers format: who may sign skills, and with which key.
allowed_signers = "/etc/xshell/skill_signers"
# Optional: keys that are no longer trusted. One per line: a public key or
# its SHA256 fingerprint.
revoked_keys = "/etc/xshell/skill_revoked"
# Whether a skill with scripts may be used in a session whose shell is not
# contained. Off by default (§7).
allow_scripts_uncontained = false
```

Both files belong to the operator. They are added to the daemon's protected
paths, so agent tools can never read or change them and no contained shell
can be granted them.

### 4.2 Signers

The trusted-signers file is OpenSSH's `allowed_signers` format: a principal
(the signer's name), optional restrictions, and a public key. xshell uses:

- the principal, as the signer's name in listings and audit records;
- the key, to check signatures. **Ed25519 keys only** at first;
- `namespaces="skill-manifest"`, honoured when present;
- `valid-after` and `valid-before`, checked against the time of
  installation.

Certificate authorities and other key types are refused with a clear
message, not ignored. GPG signatures are not supported.

The verifier reads a subset of the format and treats anything outside it as
an error naming the line, because a line the operator wrote and xshell
skipped would be a restriction, or a signer, they believe in and do not
have. Where it differs from `ssh-keygen`:

| | `ssh-keygen` | xshell |
|---|---|---|
| Principals | A list of patterns | One plain name per line |
| `namespaces` | A list of patterns | A list of exact names |
| `valid-after`, `valid-before` | Local time unless the value ends in `Z` | UTC only: the `Z` is required |
| Other options | `cert-authority`, `verify-required` | Refused |
| Key types | All | `ssh-ed25519` |
| A key on several lines | Each principal is tried | The first line that allows the signature names the signer |

Comment lines and blank lines are skipped, so a file `skill-signer trust`
maintains (which comments out a revoked signer's line) reads as intended.

The signer of a skill is whichever principal's key verifies its signature.
The skill does not get to say who signed it.

### 4.3 Name pins

Trusting a signer means trusting them for the skills they publish, not for
every name. When the operator first installs a skill called `yapcad`, the
store records the key that signed it. A later `yapcad` signed by a different
trusted signer is refused until the operator replaces the pin on purpose
(`xshell skills install --replace-signer`). This is the local form of the
registry's rule that a name belongs to its first publisher.

### 4.4 Revocation

A key in `revoked_keys` verifies nothing. The file lists one key per line,
as a public key of any type (`ssh-ed25519 AAAA…`) or as a fingerprint
(`SHA256:…`). OpenSSH's binary revocation lists are refused with a message
saying so. The check is made at installation,
when the daemon starts, and when a session that uses the skill is created,
so revoking a key stops its skills being offered without removing them by
hand. Installed skills signed by a revoked key are reported at startup.

## 5. The store

Installed skills live in a store the daemon owns, by default `skills/`
beside the session state directory:

```text
skills/
  pins.json                       name -> signing key
  yapcad/
    3f9a…c2/                      the manifest's SHA-256
      MANIFEST.json
      MANIFEST.json.sig
      SKILL.md
      scripts/export.py
```

- **Installing copies, then checks the copy.** The source directory is
  checked (§3.3), its listed files are copied into a new directory in the
  store, and the copy is checked again before it is made visible by an
  atomic rename. xshell never loads a skill from where the operator pointed.
- **Every load is checked.** A file is read from the store, hashed, and
  compared with the manifest before a byte of it is used. The signature is
  re-checked when the daemon starts. Integrity therefore rests on
  verification, not on the store's permissions: on a host with an
  uncontained shell, a command could alter a stored file, and the next load
  would refuse it.
- **Several versions can coexist.** Each is a different manifest hash. A
  name has one *current* version, which new sessions get.
- **Agent read tools cannot read the store.** It is a blocked path, like
  the daemon's other state. Skills reach an agent only through `skill_read`
  (§6.3).

### 5.1 Managing skills

| Command | Does |
|---|---|
| `xshell skills install DIR` | Check and install the skill in DIR, and make it current |
| `xshell skills list` | Installed skills: name, version, signer, manifest hash, whether it has scripts, whether its signer is revoked |
| `xshell skills show NAME` | The manifest, the signer, and what the skill requires |
| `xshell skills verify [NAME]` | Re-check installed skills against their signatures and hashes |
| `xshell skills remove NAME [HASH]` | Remove a skill, or one version of it |

These are requests to the daemon, which owns the store. Like turn lookup,
they are **local only**: refused over the SSH proxy. They also inherit the
daemon's connection rules (the control token, and the refusal of the
daemon's own child processes), so a command an agent runs cannot install a
skill. Installation is shown with the signer, the file count, whether there
are scripts, and what the skill requires, and asks for confirmation.

## 6. Skills in a session

### 6.1 Selection

A session names the skills its agent may use when it is created, as it does
tools and containment:

```json
"skills": ["yapcad", "print-farm"]
```

- **None by default.** Unlike tools, a session has no skills unless it asks.
- **Checked at creation.** Each name must be installed, valid, and signed
  by an unrevoked key. A skill with scripts needs the session's shell to be
  contained (§7). Otherwise the session is refused with the reason.
- **Pinned.** The session records each skill's manifest hash. It keeps
  using exactly that version for its life, even after the operator installs
  a newer one. A changed skill reaches an agent only through a new session.
  This is the same property as tool-list pinning in the
  [MCP bridge](mcp-bridge.md): nothing an agent was given changes under it.
- **Fixed for the session's life.**

`xshell --skill NAME` (repeatable) and `xshell run --skill NAME` select
skills from the command line. A program that embeds the daemon sets the
field itself, which is how a gateway gives each user their own skills.

### 6.2 What the model sees

At the start of every turn the system prompt gains a short index of the
session's skills: each name and its signed `description`. The index is
capped (64 skills, 500 characters each) and is added the way
[turn context](tool-providers.md#53-turn-context) is: sent with the turn's
requests, never stored in the history, recorded by hash in the audit log.

The index tells the model what exists. It carries no instructions. The
description is the only skill text in the prompt, and it is signed.

Today a daemon has one context provider slot, for the program that embeds
it. The skills index is a second source, so the daemon will combine sources
in a fixed order: the embedder's context, then the skills index.

### 6.3 Reading a skill

One built-in read-only tool, offered only to sessions that have skills:

```text
skill_read(name, path?)   path defaults to SKILL.md
```

It returns a listed file of one of the session's skills, checked against
the manifest as it is read, with a first line naming the skill, its version
and its signer. It is bounded like any tool output. It cannot read a skill
the session does not have, or a path the manifest does not list.

A skill's instructions therefore arrive as the output of a tool the agent
chose to call, attributed to a named signer, and appear in the transcript
and the audit log like any other tool result.

### 6.4 Running a skill's scripts

There is no `skill_run` tool. A script is run with `run_shell`, like any
other command, so it goes through the one path that has the approval
policy, the turn limits, the process-group cleanup and the audit records.

`skill_read` tells the agent where a skill's files are for this purpose:
in a contained session, the skill's directory in the store is granted
read-only to the sandbox and its path is given in the tool's output.

## 7. Containment for skills with scripts

A skill is *instruction-only* when its `scripts` list is empty. It can be
used in any session.

A skill with scripts can be used only in a session whose shell is
`isolated`. When such a session is created:

- each of its skills' store directories is added to the session's
  read-only grants. The operator does not have to list the store in
  `allowed_read_only`; using an installed skill is the grant. Only the
  directories of the session's own skills are granted, never the store as
  a whole, and this is the one case where a path the daemon otherwise
  keeps from agents is granted to a sandbox;
- `requires.network` must be at or below the session's network level, each
  of `requires.secrets` must be one the session has, and each of
  `requires.read_only` must be inside a grant the session has. If not, the
  session is refused, saying which skill needs what. A skill's needs never
  widen a session: the session asks, the operator's bounds allow, and the
  skill has to fit inside both.

The scripts then run with exactly the session's containment: its roots,
its network level, its secrets. They cannot write to their own directory
or to another skill's.

`allow_scripts_uncontained = true` lifts this, for a host with no sandbox
yet or an operator who has decided to trust their signers that far. The
skill's scripts then run with the shell's full authority, and the store
directory is readable only because that shell can read everything. This
setting is recorded in each turn's audit record.

Until sandbox stage C2 lands, no host can create an isolated session, so a
skill with scripts is usable only with that setting on.

## 8. Audit

- `turn_started` gains `skills`: for each of the session's skills, its name,
  version, manifest hash and signer.
- Each `skill_read` call is an ordinary tool request, decision and result.
- Installing, replacing a pin, and removing a skill are written to the
  daemon's own log with the same facts. Whether they also belong in the
  signed audit log, which today is organised by session, is open (§11).

## 9. The registry

[skill-registry](https://github.com/rdevaul/skill-registry) serves signed
skill packages. Fetching from it is a convenience on top of everything
above:

```sh
xshell skills install registry:yapcad@1.4.0
```

- The package is downloaded, unpacked into a new, empty directory, and
  checked exactly as a local directory is (§3.3). The registry is not
  trusted for integrity or for who signed what; it is trusted only to be
  reachable.
- Unpacking refuses absolute paths, `..`, links, and anything that is not a
  regular file or directory, and enforces the §3.3 limits before writing.
- `xshell skills refresh` fetches the registry's revocation list and merges
  it into the revoked keys. The daemon does not contact the registry by
  itself.

The registry needs a matching change to accept and serve version 2
packages.

## 10. Tasks and estimates

| # | Task | LoE (days) |
|---|---|---|
| K0 | skill-signer: write and verify manifest version 2 (`MANIFEST.json` and a detached OpenSSH signature); list every file; refuse links; keep version 1 `verify` and add a re-sign path; tests that stock `ssh-keygen -Y verify` accepts what it writes | 2–3 |
| K1 | `xshell-skills` crate: OpenSSH signature check for Ed25519; the `allowed_signers` subset; revoked keys; strict manifest parsing; the directory check of §3.3. Tests with fixtures signed by skill-signer and by `ssh-keygen`, and one tampering test per rule | 4–5 |
| K2 | The store: install by copy and re-check, atomic publish, pins, several versions, removal; the local-only daemon requests; `xshell skills` commands | 3–4 |
| K3 | `skills` on session creation and the descriptor, pinned by hash; the skills index as a second context source; `skill_read`; `turn_started` audit field; `--skill` | 4–5 |
| K4 | Script skills: the store directory as a read-only grant; `requires` checked against the session; `allow_scripts_uncontained`; tests on macOS under the sandbox | 2–3 |
| K5 | Registry fetch with safe unpacking; `refresh` for revocation; registry support for version 2 | 3–4 |

**Done:** K0 (skill-signer pull request 2) and K1 (`crates/xshell-skills`).

K1 parses the OpenSSH signature format itself, on `ed25519-dalek`, `sha2`
and `base64`, which the workspace already used; it adds no dependency. Its
tests run against a skill signed by skill-signer and one signed by stock
`ssh-keygen` (`crates/xshell-skills/tests/fixtures`), and, where OpenSSH is
installed, check that `ssh-keygen` accepts the signatures the tests make.
Where the verifier and `ssh-keygen` could disagree, the verifier is the
stricter: it refuses trailing bytes in a signature, a non-empty reserved
field, text around the armor, weak keys and non-canonical signatures.

Two details differ from skill-signer's own check and are worth knowing:
paths are compared for case-insensitive uniqueness by lowercasing, where
skill-signer uses Unicode case folding, and `requires: null` is refused
where skill-signer reads it as absent. A manifest skill-signer writes
passes both.

**Risks:**

- **Reimplementing signature checking.** K1 verifies OpenSSH signatures
  itself (decided, §11). The fixtures and the `ssh-keygen` cross-check are
  what keep it honest.
- **A false sense of safety.** "Signed" will be read as "safe". The
  documentation and the install prompt have to say what a signature means.
- **Three repositories.** The format change lands in skill-signer, the
  loader in xshell, and package support in skill-registry. K0 comes first
  so the others have real fixtures.

## 11. Decisions and open questions

Decided on 23 November 2026:

| Question | Decision |
|---|---|
| Instruction-only skills without a contained shell | Allowed (§7) |
| The signed form | Change skill-signer to sign the manifest's bytes as stored (§3.2) |
| Signature types | SSH only for now; no GPG (§4.2) |

Decided on 27 November 2026:

| Question | Decision |
|---|---|
| Own verifier, or `ssh-keygen`? | A Rust implementation, in the daemon's process (§10, K1) |
| Manifest shape | As in §3.2 |

Still open:

1. **How the model meets a skill.** This design puts only names and signed
   descriptions in the prompt and has the agent read `SKILL.md` when it
   decides to. The alternative is to put each selected skill's `SKILL.md`
   in the prompt. That is simpler for the model and costs context on every
   request.
2. **Dependencies between skills.** skill-signer's version 1 manifest has a
   `dependencies` field. Version 2 as written here has none, and a skill
   that needs another has to say so in its instructions. Should it return,
   with each dependency pinned by manifest hash?
3. **Install events in the signed audit log.** The log is organised by
   session. Skill management is not part of any session.
4. **Who may sign what, beyond pins.** A pin is made at first install. A
   stricter host might list, per signer, the names they may publish.
5. **Skills for subagents.** A subagent inherits its parent's tools and
   containment. Should it see the skills index and have `skill_read`?
