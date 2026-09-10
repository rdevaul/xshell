# xshell release artifacts

Remote bootstrap trusts an Ed25519-signed manifest rather than a checksum
downloaded beside an executable. The manifest signature covers the exact
xshell version, session protocol version, and each artifact's target, HTTPS
URL, byte length, and SHA-256 digest. The controller verifies the manifest
before selecting an artifact and verifies the downloaded bytes before they can
be presented for installation approval.

## One-time signing-key setup

Generate the release keypair on a protected operator machine:

```sh
cargo run -p xshell-release --bin xshell-release-manifest -- \
  keygen --private-key xshell-release.key --public-key xshell-release.pub
gh secret set XSHELL_RELEASE_SIGNING_KEY < xshell-release.key
```

The private key file is mode `0600`, is never committed, and should be moved to
protected offline storage after the GitHub Actions secret is configured. The
public key is the trust anchor supplied to xshell controllers:

```toml
[remote_bootstrap]
manifest_url = "https://github.com/rdevaul/xshell/releases/download/v{version}/xshell-release.json"
public_key = "ED25519_PUBLIC_KEY_HEX"
```

Key rotation requires distributing the new public key through an already
trusted channel before publishing manifests signed only by the new key.

## Publishing

The release workflow runs for a pushed `vVERSION` tag and rejects a tag whose
version differs from the Cargo workspace version. It builds raw `xshelld`
binaries for:

- `aarch64-apple-darwin` and `x86_64-apple-darwin`, targeting macOS 13 or later;
- `aarch64-unknown-linux-musl` and `x86_64-unknown-linux-musl`, statically linked
  so bootstrap does not depend on a remote distribution's glibc version.

After all four builds succeed, the workflow derives the protocol version from
the tagged `xshelld`, signs `xshell-release.json`, and creates the GitHub
release with the manifest and binaries in one operation. Existing releases are
not overwritten by the workflow.

Before tagging, run the full workspace tests and confirm that the intended
release version is set in the workspace manifest. Then create and push the
annotated tag:

```sh
git tag -s v0.2.0 -m "xshell 0.2.0"
git push origin v0.2.0
```

The Git tag signature and GitHub permissions protect the source/publishing
path. The Ed25519 manifest signature is the trust decision made by xshell at
artifact-acquisition time and remains independently verifiable after download.
