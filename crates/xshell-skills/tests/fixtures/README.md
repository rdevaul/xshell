# Fixtures

Real signed skills for `tests/verify.rs`. The private keys were thrown away
after signing; nothing here can sign anything.

| Path | Made with |
|---|---|
| `signed-by-skill-signer/` | `skill-signer sign` (manifest version 2), key `signer.pub` |
| `signed-by-ssh-keygen/` | a hand-written `MANIFEST.json` and `ssh-keygen -Y sign -n skill-manifest` (SHA-512, the default), key `signer.pub` |
| `signatures/sha256.sig` | the same manifest, `-O hashalg=sha256` |
| `signatures/wrong-namespace.sig` | the same manifest, `-n file` |
| `signatures/other-key.sig` | the same manifest, key `other.pub` |
| `signatures/rsa.sig`, `ecdsa.sig` | the same manifest, RSA and ECDSA keys |
| `allowed_signers` | `signer.pub` as `fixture-signer@example.com` |
| `signer.fingerprint` | what `ssh-keygen -l` prints for `signer.pub` |

To check a fixture with stock OpenSSH:

```sh
cd signed-by-ssh-keygen
ssh-keygen -Y verify -f ../allowed_signers -I fixture-signer@example.com \
    -n skill-manifest -s MANIFEST.json.sig < MANIFEST.json
```

Do not edit these files: any change breaks a signature or a hash.
