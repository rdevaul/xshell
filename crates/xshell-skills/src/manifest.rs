//! `MANIFEST.json`, version 2, parsed strictly.
//!
//! These bytes come from whoever published the skill, so every rule is
//! checked here and nothing is repaired or defaulted beyond what the format
//! says may be absent (`scripts`, `requires` and its fields).

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use serde::Deserialize;
use serde::de::{MapAccess, Visitor};

use crate::SkillError;

pub const MANIFEST_NAME: &str = "MANIFEST.json";
pub const SIGNATURE_NAME: &str = "MANIFEST.json.sig";
pub const MANIFEST_VERSION: u32 = 2;

pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
pub const MAX_SIGNATURE_BYTES: u64 = 64 * 1024;
pub const MAX_FILES: usize = 512;
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_PATH_BYTES: usize = 255;
pub const MAX_NAME_BYTES: usize = 64;
pub const MAX_DESCRIPTION_CHARS: usize = 500;

/// One listed file.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    /// Lowercase hex.
    pub sha256: String,
    pub size: u64,
}

/// The network level a skill's scripts need.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillNetwork {
    #[default]
    None,
    Loopback,
    Full,
}

/// What a skill's scripts need from a contained shell.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requires {
    #[serde(default)]
    pub network: SkillNetwork,
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub read_only: Vec<String>,
}

/// A manifest that passed every rule that can be checked without the files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub description: String,
    /// Every file in the skill, by `/`-separated path relative to its top.
    pub files: BTreeMap<String, FileEntry>,
    /// The listed files an agent may be asked to run. Sorted.
    pub scripts: Vec<String>,
    pub requires: Requires,
}

impl Manifest {
    /// Whether the skill has no scripts.
    pub fn is_instruction_only(&self) -> bool {
        self.scripts.is_empty()
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, SkillError> {
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(rule(format!("{MANIFEST_NAME} is larger than 1 MiB")));
        }
        // The version is read first so that a manifest of another version is
        // reported as that, not as a list of unknown fields.
        let probe: VersionProbe = serde_json::from_slice(bytes)
            .map_err(|error| rule(format!("{MANIFEST_NAME} is not valid: {error}")))?;
        if probe.manifest_version != Some(u64::from(MANIFEST_VERSION)) {
            return Err(rule(match probe.manifest_version {
                Some(version) => format!(
                    "manifest_version is {version}; only version {MANIFEST_VERSION} is accepted"
                ),
                None => format!(
                    "manifest_version is missing; only version {MANIFEST_VERSION} is accepted"
                ),
            }));
        }
        let raw: RawManifest = serde_json::from_slice(bytes)
            .map_err(|error| rule(format!("{MANIFEST_NAME} is not valid: {error}")))?;
        raw.validate()
    }
}

fn rule(message: String) -> SkillError {
    SkillError::Manifest(message)
}

#[derive(Deserialize)]
struct VersionProbe {
    manifest_version: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    #[allow(dead_code)]
    manifest_version: u32,
    name: String,
    version: String,
    description: String,
    files: Files,
    #[serde(default)]
    scripts: Vec<String>,
    #[serde(default)]
    requires: Requires,
}

/// The `files` object. serde's map types keep the last of two equal keys;
/// this refuses the second.
struct Files(BTreeMap<String, FileEntry>);

impl<'de> Deserialize<'de> for Files {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FilesVisitor;

        impl<'de> Visitor<'de> for FilesVisitor {
            type Value = Files;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object of file entries")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Files, A::Error> {
                let mut files = BTreeMap::new();
                while let Some(path) = map.next_key::<String>()? {
                    if files.len() >= MAX_FILES {
                        return Err(serde::de::Error::custom(format!(
                            "a skill may hold at most {MAX_FILES} files"
                        )));
                    }
                    let entry = map.next_value()?;
                    if files.insert(path.clone(), entry).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "file {path:?} is listed twice"
                        )));
                    }
                }
                Ok(Files(files))
            }
        }

        deserializer.deserialize_map(FilesVisitor)
    }
}

impl RawManifest {
    fn validate(self) -> Result<Manifest, SkillError> {
        check_name(&self.name)?;
        if self.version.is_empty() {
            return Err(rule("version is empty".into()));
        }
        if self.description.trim().is_empty() {
            return Err(rule("description is empty".into()));
        }
        if self.description.chars().count() > MAX_DESCRIPTION_CHARS {
            return Err(rule(format!(
                "description is longer than {MAX_DESCRIPTION_CHARS} characters"
            )));
        }

        let files = self.files.0;
        let mut total: u64 = 0;
        let mut folded = HashSet::new();
        for (path, entry) in &files {
            check_path(path)?;
            if path == MANIFEST_NAME || path == SIGNATURE_NAME {
                return Err(rule(format!("{path} must not list itself")));
            }
            let hex_ok = entry.sha256.len() == 64
                && entry
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !hex_ok {
                return Err(rule(format!("file {path:?} has an invalid sha256")));
            }
            total = total
                .checked_add(entry.size)
                .filter(|total| *total <= MAX_TOTAL_BYTES)
                .ok_or_else(|| rule("a skill may hold at most 64 MiB".into()))?;
            if !folded.insert(path.to_lowercase()) {
                return Err(rule(format!(
                    "file {path:?} differs from another only by letter case"
                )));
            }
        }

        let mut scripts = self.scripts;
        scripts.sort();
        for pair in scripts.windows(2) {
            if pair[0] == pair[1] {
                return Err(rule(format!("script {:?} is listed twice", pair[0])));
            }
        }
        if let Some(script) = scripts.iter().find(|script| !files.contains_key(*script)) {
            return Err(rule(format!("script {script:?} is not in files")));
        }

        let requires = self.requires;
        if requires.secrets.iter().any(String::is_empty)
            || requires.read_only.iter().any(String::is_empty)
        {
            return Err(rule("requires has an empty entry".into()));
        }

        Ok(Manifest {
            name: self.name,
            version: self.version,
            description: self.description,
            files,
            scripts,
            requires,
        })
    }
}

/// 1–64 characters of `a-z`, `0-9`, `-` and `_`, starting with a letter.
fn check_name(name: &str) -> Result<(), SkillError> {
    let mut bytes = name.bytes();
    let starts_with_letter = bytes.next().is_some_and(|b| b.is_ascii_lowercase());
    let rest_ok =
        bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if starts_with_letter && rest_ok && name.len() <= MAX_NAME_BYTES {
        Ok(())
    } else {
        Err(rule(format!(
            "name {name:?} is not 1-64 characters of a-z, 0-9, '-' and '_' starting with a letter"
        )))
    }
}

/// Relative, `/`-separated, no empty, `.` or `..` component, at most 255
/// bytes.
pub(crate) fn check_path(path: &str) -> Result<(), SkillError> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err(rule(format!(
            "path {path:?} is empty or longer than {MAX_PATH_BYTES} bytes"
        )));
    }
    if path.contains(['\\', '\0']) {
        return Err(rule(format!("path {path:?} must use '/' separators")));
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(rule(format!(
            "path {path:?} must be relative, with no empty, '.' or '..' component"
        )));
    }
    Ok(())
}
