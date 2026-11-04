# Security policy

## Reporting a vulnerability

Report vulnerabilities privately through GitHub: open the repository's
**Security** tab and choose **Report a vulnerability**. This creates a private
security advisory that only the maintainers can see.

Please do not report vulnerabilities in public issues, pull requests, or
discussions.

A useful report includes:

- the affected component (for example `xshelld`, the CLI, the audit service,
  or an agent tool) and version or commit;
- the mode it affects (standalone CLI, local daemon, remote session over SSH);
- steps or a test that reproduce it; and
- what an attacker gains: for example reading a sensitive file without
  approval, running a command without approval, or escaping the working
  directory.

## Supported versions

Fixes land on `main` and in the next release. Older releases are not patched
separately.

## Known issues

Findings from the design and security review, and their status, are tracked in
the [security findings ledger](docs/xshell-security-findings.md). The
[threat model](docs/futureshell-threat-model.md) describes what xshell does and
does not defend against. A report about a finding the ledger already lists as
open is still welcome when it shows a new way to exploit it.
