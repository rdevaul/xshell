use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use ed25519_dalek::SigningKey;
use rand_core::OsRng;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use xshell_release::{
    RELEASE_MANIFEST_SCHEMA_VERSION, ReleaseArtifact, ReleaseManifestBody, parse_signing_key,
    sign_manifest,
};

#[derive(Debug, Parser)]
#[command(version, about = "Create and sign xshell release manifests")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate a new Ed25519 release signing keypair.
    Keygen {
        #[arg(long)]
        private_key: PathBuf,
        #[arg(long)]
        public_key: PathBuf,
    },
    /// Hash release artifacts and write a signed manifest.
    Create {
        #[arg(long)]
        version: String,
        #[arg(long)]
        protocol_version: u32,
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        output: PathBuf,
        #[arg(required = true)]
        artifacts: Vec<PathBuf>,
    },
}

fn main() -> Result<()> {
    match Args::parse().command {
        Command::Keygen {
            private_key,
            public_key,
        } => generate_keypair(&private_key, &public_key),
        Command::Create {
            version,
            protocol_version,
            base_url,
            output,
            artifacts,
        } => create_manifest(version, protocol_version, &base_url, &output, &artifacts),
    }
}

fn generate_keypair(private_path: &Path, public_path: &Path) -> Result<()> {
    let signing_key = SigningKey::generate(&mut OsRng);
    write_new(
        private_path,
        hex::encode(signing_key.to_bytes()).as_bytes(),
        0o600,
    )
    .context("cannot write release private key")?;
    if let Err(error) = write_new(
        public_path,
        hex::encode(signing_key.verifying_key().as_bytes()).as_bytes(),
        0o644,
    ) {
        let _ = fs::remove_file(private_path);
        return Err(error).context("cannot write release public key");
    }
    Ok(())
}

fn create_manifest(
    version: String,
    protocol_version: u32,
    base_url: &str,
    output: &Path,
    artifact_paths: &[PathBuf],
) -> Result<()> {
    if version != env!("CARGO_PKG_VERSION") {
        bail!(
            "requested release version {version:?} does not match workspace version {:?}",
            env!("CARGO_PKG_VERSION")
        );
    }
    let private_hex = std::env::var("XSHELL_RELEASE_SIGNING_KEY")
        .context("XSHELL_RELEASE_SIGNING_KEY is not set")?;
    let signing_key = parse_signing_key(&private_hex)?;
    let base_url = base_url.trim_end_matches('/');
    let mut artifacts = artifact_paths
        .iter()
        .map(|path| describe_artifact(path, base_url))
        .collect::<Result<Vec<_>>>()?;
    artifacts.sort_by(|left, right| left.target.cmp(&right.target));
    let manifest = sign_manifest(
        ReleaseManifestBody {
            schema_version: RELEASE_MANIFEST_SCHEMA_VERSION,
            version,
            protocol_version,
            artifacts,
        },
        &signing_key,
    )?;
    let mut encoded = serde_json::to_vec_pretty(&manifest)?;
    encoded.push(b'\n');
    fs::write(output, encoded)
        .with_context(|| format!("cannot write release manifest {}", output.display()))
}

fn describe_artifact(path: &Path, base_url: &str) -> Result<ReleaseArtifact> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("artifact has no UTF-8 file name: {}", path.display()))?;
    let target = file_name.strip_prefix("xshelld-").with_context(|| {
        format!("artifact name must have the form xshelld-TARGET: {file_name:?}")
    })?;
    if target.is_empty()
        || !target
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("artifact target contains unsupported characters: {target:?}");
    }
    let mut file = File::open(path)
        .with_context(|| format!("cannot open release artifact {}", path.display()))?;
    let size = file.metadata()?.len();
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(ReleaseArtifact {
        target: target.into(),
        url: format!("{base_url}/{file_name}"),
        size,
        sha256: hex::encode(hasher.finalize()),
    })
}

fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    file.write_all(bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn describes_artifact_from_release_file_name() {
        let temporary = TempDir::new().unwrap();
        let path = temporary.path().join("xshelld-aarch64-apple-darwin");
        fs::write(&path, b"binary").unwrap();
        let artifact = describe_artifact(&path, "https://example.test/v0.2.0").unwrap();
        assert_eq!(artifact.target, "aarch64-apple-darwin");
        assert_eq!(artifact.size, 6);
        assert_eq!(artifact.sha256, hex::encode(Sha256::digest(b"binary")));
    }

    #[test]
    fn key_generation_refuses_to_overwrite_files() {
        let temporary = TempDir::new().unwrap();
        let private = temporary.path().join("private");
        let public = temporary.path().join("public");
        generate_keypair(&private, &public).unwrap();
        assert!(generate_keypair(&private, &public).is_err());
        assert_eq!(fs::read_to_string(&private).unwrap().trim().len(), 64);
        assert_eq!(fs::read_to_string(&public).unwrap().trim().len(), 64);
    }
}
