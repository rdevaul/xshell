# Contributing to xshell

xshell is a Rust workspace (`crates/*`) built and tested on macOS and Linux.
Every pull request runs the gates below in CI
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)). Run them locally
before pushing.

## Toolchain

`rust-toolchain.toml` pins the development toolchain, so `rustup` installs and
selects it automatically. The workspace's minimum supported Rust version is
**1.95** (`rust-version` in `Cargo.toml`), and CI checks that it still builds
with exactly that version.

## The gates

| Gate | Command | Runs on |
|---|---|---|
| Formatting | `cargo fmt --all -- --check` | macOS, Linux |
| Lints | `cargo clippy --workspace --all-targets -- -D warnings` | macOS, Linux x86_64, Linux arm64 |
| Tests | `cargo test --workspace --all-targets` | macOS, Linux x86_64, Linux arm64 |
| Minimum Rust version | `RUSTUP_TOOLCHAIN=1.95.0 cargo check --locked --workspace --all-targets` | Linux |
| Dependency advisories | `cargo audit` (RustSec) | Linux |
| Home-lab runner | `cargo build --locked -p xshell-audit -p xshell-session -p xshell-cli --bins`, then `XSHELL_RUNALL_REAL_BIN_DIR=target/debug python3 -B -m unittest discover -s scripts/tests -v` | macOS, Linux |
| FutureShell fixtures | `./scripts/futureshell-workspace-probe.sh` and `./fixtures/futureshell/fea/check-fixture.sh` | macOS, Linux |

Clippy warnings fail the build. The workspace enables
`clippy::undocumented_unsafe_blocks`, so every `unsafe` block needs a
`// SAFETY:` comment explaining why it is sound.

Linux arm64 catches libc ABI differences that x86_64 does not (for example,
`c_char` is unsigned there), so prefer libc's own types over hard-coded
integer types.

## Tests

- Put a regression test next to every fix, and check that it fails without
  the fix.
- Process, filesystem, and shutdown changes must pass on both macOS and Linux;
  their timing and signal behaviour differ.
- A fix for a finding in the
  [security findings ledger](docs/xshell-security-findings.md) names the
  finding in its test (for example `f6_...`) and follows the ledger's evidence
  rules before the finding is marked Corrected.
- Timing-based tests should leave wide margins: CI machines, macOS runners in
  particular, are slower and noisier than a development machine.

## Documentation

Behaviour changes update the documents that describe them. The
[development roadmap](docs/xshell-development-roadmap.md) is the sequencing
authority; the [specification](xshell-specification.md) describes the target
product, not only what is built.

## Security issues

Do not open a public issue for a vulnerability. See [SECURITY.md](SECURITY.md).
