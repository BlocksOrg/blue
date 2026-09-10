//! Portable, integrity-checked native session bundles.
//!
//! The archive is deliberately boring: a gzip-compressed tar containing a
//! manifest and regular files below `files/`.  Object storage remains opaque;
//! all trust decisions are repeated by the restoring client.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use gh_common::{GhError, Harness};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const ARTIFACT_FORMAT: &str = "blue-session-bundle-v1";
pub const CONTENT_TYPE: &str = "application/vnd.blue.session-bundle+tar+gzip";
pub const MAX_BUNDLE_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_FILES: usize = 2_048;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepositoryIdentity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BundleFile {
    pub role: String,
    /// Safe relative path used inside the archive.
    pub path: String,
    /// Original location relative to the user's home, when restorable in place.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_path: Option<String>,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BundleManifest {
    pub schema_version: u32,
    pub artifact_format: String,
    pub harness: String,
    pub compatibility_profile: String,
    pub native_session_id: String,
    pub captured_at_unix_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<RepositoryIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub files: Vec<BundleFile>,
}

#[derive(Debug, Clone)]
pub struct SessionSource {
    pub role: String,
    pub source: PathBuf,
    pub native_path: Option<PathBuf>,
}

#[derive(Debug)]
pub struct CreatedBundle {
    pub bytes: Vec<u8>,
    pub manifest: BundleManifest,
}

#[derive(Debug)]
pub struct VerifiedBundle {
    pub manifest: BundleManifest,
    pub files: BTreeMap<String, Vec<u8>>,
}

fn invalid(message: impl Into<String>) -> GhError {
    GhError::config(format!("invalid session bundle: {}", message.into()))
}

fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn path_belongs_to_session(harness: Harness, path: &Path, session_id: &str) -> bool {
    let exact_component = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .any(|component| component == session_id);
    if exact_component {
        return true;
    }
    let stem = path.file_stem().and_then(|value| value.to_str());
    match harness {
        Harness::Codex => {
            stem.is_some_and(|stem| stem == session_id || stem.ends_with(&format!("-{session_id}")))
        }
        Harness::Claude => stem == Some(session_id),
        Harness::Kimi | Harness::Opencode => false,
    }
}

fn starts_with_any(path: &Path, roots: &[&str]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

fn portable_relative(path: &Path) -> Result<String, GhError> {
    if !safe_relative(path) {
        return Err(invalid("native path is not a safe home-relative path"));
    }
    path.components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .ok_or_else(|| invalid("native path is not valid UTF-8"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|components| components.join("/"))
}

/// Validate the harness-specific artifact and destination contract. This is
/// deliberately repeated by the restoring client: bundles and object storage
/// are untrusted input, even when their transport checksum is valid.
pub fn validate_manifest_contract(
    harness: Harness,
    manifest: &BundleManifest,
) -> Result<(), GhError> {
    if manifest.harness != harness.key() {
        return Err(invalid(
            "manifest harness does not match the selected adapter",
        ));
    }
    if manifest.native_session_id.is_empty()
        || manifest.native_session_id.chars().count() > 512
        || !manifest
            .native_session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(invalid(
            "native session id is empty, too long, or contains unsafe characters",
        ));
    }
    if manifest.compatibility_profile.is_empty()
        || manifest.compatibility_profile.chars().count() > 256
        || manifest
            .title
            .as_ref()
            .is_some_and(|value| value.chars().count() > 256)
        || manifest
            .summary
            .as_ref()
            .is_some_and(|value| value.chars().count() > 1024)
        || manifest
            .cwd
            .as_ref()
            .is_some_and(|value| value.chars().count() > 4096)
    {
        return Err(invalid("manifest metadata exceeds its supported bounds"));
    }

    let required_role = match harness {
        Harness::Codex => "rollout",
        Harness::Claude => "primary_transcript",
        Harness::Kimi => "agent_wire_history",
        Harness::Opencode => "portable_export",
    };
    if manifest
        .files
        .iter()
        .filter(|file| file.role == required_role)
        .count()
        != 1
    {
        return Err(invalid(format!(
            "{harness} bundle must contain exactly one {required_role} artifact"
        )));
    }
    if harness == Harness::Kimi
        && manifest
            .files
            .iter()
            .filter(|file| file.role == "kimi_state")
            .count()
            != 1
    {
        return Err(invalid(
            "Kimi bundle must contain exactly one state artifact",
        ));
    }

    let mut native_targets = BTreeSet::new();
    for file in &manifest.files {
        if file.role.is_empty() || file.role.chars().count() > 64 {
            return Err(invalid("artifact role is empty or too long"));
        }
        let allowed_role = match harness {
            Harness::Codex => file.role == "rollout",
            Harness::Claude => matches!(
                file.role.as_str(),
                "primary_transcript" | "session_companion"
            ),
            Harness::Kimi => matches!(
                file.role.as_str(),
                "agent_wire_history" | "kimi_state" | "session_state"
            ),
            Harness::Opencode => file.role == "portable_export",
        };
        if !allowed_role {
            return Err(invalid(format!(
                "unsupported {} artifact role {}",
                harness, file.role
            )));
        }

        if harness == Harness::Opencode {
            if file.native_path.is_some() {
                return Err(invalid(
                    "OpenCode portable exports must be restored through native import",
                ));
            }
            continue;
        }
        let native =
            file.native_path.as_deref().map(Path::new).ok_or_else(|| {
                invalid(format!("{} artifact has no native destination", file.role))
            })?;
        if !native_targets.insert(native.to_path_buf()) {
            return Err(invalid("multiple artifacts target the same native path"));
        }
        let in_native_root = match harness {
            Harness::Codex => starts_with_any(native, &[".codex/sessions"]),
            Harness::Claude => starts_with_any(native, &[".claude/projects"]),
            Harness::Kimi => starts_with_any(
                native,
                &[".config/blue/runtime/kimi/sessions", ".kimi-code/sessions"],
            ),
            Harness::Opencode => unreachable!(),
        };
        if !in_native_root || !path_belongs_to_session(harness, native, &manifest.native_session_id)
        {
            return Err(invalid(format!(
                "{} destination is outside the advertised native session",
                native.display()
            )));
        }
    }
    Ok(())
}

fn bounded(value: Option<String>, max: usize) -> Option<String> {
    value
        .map(|value| value.chars().take(max).collect())
        .filter(|v: &String| !v.trim().is_empty())
}

pub fn repository_identity(cwd: Option<&str>) -> Option<RepositoryIdentity> {
    let cwd = Path::new(cwd?);
    let output = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .ok()
            .filter(|value| value.status.success())
            .and_then(|value| String::from_utf8(value.stdout).ok())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let root = output(&["rev-parse", "--show-toplevel"]);
    let remote = output(&["remote", "get-url", "origin"]);
    (root.is_some() || remote.is_some()).then_some(RepositoryIdentity { root, remote })
}

/// Create a deterministic-entry-order bundle from adapter-approved regular files.
pub fn create(
    harness: Harness,
    profile: &str,
    session_id: &str,
    cwd: Option<String>,
    title: Option<String>,
    summary: Option<String>,
    sources: Vec<SessionSource>,
) -> Result<CreatedBundle, GhError> {
    create_with(
        harness,
        profile,
        session_id,
        cwd,
        title,
        summary,
        sources,
        |_, bytes| Ok(bytes),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn create_with<F>(
    harness: Harness,
    profile: &str,
    session_id: &str,
    cwd: Option<String>,
    title: Option<String>,
    summary: Option<String>,
    mut sources: Vec<SessionSource>,
    prepare_file: F,
) -> Result<CreatedBundle, GhError>
where
    F: Fn(&str, Vec<u8>) -> Result<Vec<u8>, GhError>,
{
    if session_id.is_empty() || session_id.chars().count() > 512 {
        return Err(invalid("native session id is empty or too long"));
    }
    if sources.is_empty() || sources.len() > MAX_FILES {
        return Err(invalid("artifact count is outside the supported range"));
    }
    sources.sort_by(|a, b| a.role.cmp(&b.role).then(a.source.cmp(&b.source)));
    let mut bodies = Vec::with_capacity(sources.len());
    let mut files = Vec::with_capacity(sources.len());
    let mut names = BTreeSet::new();
    let mut expanded_size = 0usize;
    let home = gh_common::paths::home_dir().ok();
    for (index, source) in sources.into_iter().enumerate() {
        if let Some(home) = home.as_deref() {
            if let Ok(relative) = source.source.strip_prefix(home) {
                reject_symlinked_destination(home, relative)?;
            }
        }
        let metadata =
            fs::symlink_metadata(&source.source).map_err(|source_error| GhError::Io {
                path: source.source.clone(),
                source: source_error,
            })?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(invalid(format!(
                "{} is not a regular file",
                source.source.display()
            )));
        }
        if metadata.len() as usize > MAX_FILE_BYTES {
            return Err(invalid(format!(
                "{} exceeds the per-file limit",
                source.source.display()
            )));
        }
        let bytes = fs::read(&source.source).map_err(|source_error| GhError::Io {
            path: source.source.clone(),
            source: source_error,
        })?;
        let bytes = prepare_file(&source.role, bytes)?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(invalid(format!(
                "{} exceeds the per-file limit",
                source.source.display()
            )));
        }
        expanded_size = expanded_size
            .checked_add(bytes.len())
            .ok_or_else(|| invalid("expanded size overflow"))?;
        if expanded_size > MAX_BUNDLE_BYTES {
            return Err(invalid("expanded archive exceeds size limit"));
        }
        let file_name = source
            .source
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("artifact");
        let archive_path = format!("files/{index:04}-{file_name}");
        if !names.insert(archive_path.clone()) {
            return Err(invalid("duplicate archive path"));
        }
        // Native destinations are security-sensitive bundle metadata. Require
        // the harness capture operation to provide them explicitly rather than
        // inferring one from an incidental source location. OpenCode exports,
        // in particular, may temporarily live under HOME but must only be
        // restored through the native importer.
        let native_path = source.native_path;
        files.push(BundleFile {
            role: source.role,
            path: archive_path,
            native_path: native_path.as_deref().map(portable_relative).transpose()?,
            size: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(&bytes)),
        });
        bodies.push(bytes);
    }
    let manifest = BundleManifest {
        schema_version: 1,
        artifact_format: ARTIFACT_FORMAT.into(),
        harness: harness.key().into(),
        compatibility_profile: profile.into(),
        native_session_id: session_id.into(),
        captured_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        repository: repository_identity(cwd.as_deref()),
        cwd: bounded(cwd, 4096),
        title: bounded(title, 256),
        summary: bounded(summary, 1024),
        files,
    };
    validate_manifest_contract(harness, &manifest)?;
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut archive = tar::Builder::new(encoder);
    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).map_err(|e| GhError::Serde(e.to_string()))?;
    append(&mut archive, "manifest.json", &manifest_bytes)?;
    for (file, bytes) in manifest.files.iter().zip(bodies.iter()) {
        append(&mut archive, &file.path, bytes)?;
    }
    let encoder = archive.into_inner().map_err(GhError::IoBare)?;
    let bytes = encoder.finish().map_err(GhError::IoBare)?;
    if bytes.len() > MAX_BUNDLE_BYTES {
        return Err(invalid("compressed archive exceeds size limit"));
    }
    Ok(CreatedBundle { bytes, manifest })
}

fn append<W: std::io::Write>(
    archive: &mut tar::Builder<W>,
    path: &str,
    bytes: &[u8],
) -> Result<(), GhError> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o600);
    header.set_mtime(0);
    header.set_cksum();
    archive
        .append_data(&mut header, path, Cursor::new(bytes))
        .map_err(GhError::IoBare)
}

/// Parse and verify an untrusted bundle without extracting it to disk.
pub fn verify(bytes: &[u8]) -> Result<VerifiedBundle, GhError> {
    if bytes.len() > MAX_BUNDLE_BYTES {
        return Err(invalid("compressed archive exceeds size limit"));
    }
    let mut archive = tar::Archive::new(GzDecoder::new(Cursor::new(bytes)));
    let mut entries = BTreeMap::new();
    let mut total = 0usize;
    for entry in archive.entries().map_err(GhError::IoBare)? {
        let mut entry = entry.map_err(GhError::IoBare)?;
        if !entry.header().entry_type().is_file() {
            return Err(invalid("only regular-file entries are supported"));
        }
        let path = entry.path().map_err(GhError::IoBare)?.into_owned();
        if !safe_relative(&path) {
            return Err(invalid("archive contains an unsafe path"));
        }
        let name = path.to_string_lossy().into_owned();
        if entries.contains_key(&name) {
            return Err(invalid(format!("duplicate entry {name}")));
        }
        let declared = entry.size() as usize;
        if declared > MAX_FILE_BYTES {
            return Err(invalid(format!("entry {name} exceeds size limit")));
        }
        total = total
            .checked_add(declared)
            .ok_or_else(|| invalid("expanded size overflow"))?;
        if total > MAX_BUNDLE_BYTES {
            return Err(invalid("expanded archive exceeds size limit"));
        }
        let mut body = Vec::with_capacity(declared);
        entry.read_to_end(&mut body).map_err(GhError::IoBare)?;
        entries.insert(name, body);
        if entries.len() > MAX_FILES + 1 {
            return Err(invalid("archive has too many entries"));
        }
    }
    let manifest_bytes = entries
        .remove("manifest.json")
        .ok_or_else(|| invalid("manifest.json is missing"))?;
    let manifest: BundleManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| invalid(format!("manifest JSON: {e}")))?;
    if manifest.schema_version != 1 || manifest.artifact_format != ARTIFACT_FORMAT {
        return Err(invalid("unsupported schema or artifact format"));
    }
    let harness: Harness = manifest
        .harness
        .parse()
        .map_err(|_| invalid("unknown harness"))?;
    validate_manifest_contract(harness, &manifest)?;
    if manifest.files.len() != entries.len() {
        return Err(invalid("manifest file count does not match archive"));
    }
    let mut declared = BTreeSet::new();
    for file in &manifest.files {
        let path = Path::new(&file.path);
        if !safe_relative(path) || !file.path.starts_with("files/") || !declared.insert(&file.path)
        {
            return Err(invalid(
                "manifest contains an unsafe or duplicate file path",
            ));
        }
        if file
            .native_path
            .as_deref()
            .is_some_and(|path| !safe_relative(Path::new(path)))
        {
            return Err(invalid("manifest contains an unsafe native path"));
        }
        let body = entries
            .get(&file.path)
            .ok_or_else(|| invalid(format!("missing entry {}", file.path)))?;
        if body.len() as u64 != file.size || hex::encode(Sha256::digest(body)) != file.sha256 {
            return Err(invalid(format!(
                "digest or size mismatch for {}",
                file.path
            )));
        }
    }
    Ok(VerifiedBundle {
        manifest,
        files: entries,
    })
}

/// Atomically restore all home-relative files, refusing differing collisions.
pub fn restore_home_files(bundle: &VerifiedBundle, home: &Path) -> Result<Vec<PathBuf>, GhError> {
    restore_home_files_with(bundle, home, |_, existing, bundled| existing == bundled)
}

pub fn restore_home_files_with<F>(
    bundle: &VerifiedBundle,
    home: &Path,
    equivalent: F,
) -> Result<Vec<PathBuf>, GhError>
where
    F: Fn(&str, &[u8], &[u8]) -> bool,
{
    let targets = preflight_home_files_with(bundle, home, equivalent)?;
    let mut restored = Vec::new();
    for (target, bytes) in targets {
        let existed = target.exists();
        if let Err(error) = gh_common::write_atomic(&target, bytes) {
            for (written, existed) in &restored {
                if !existed {
                    let _ = fs::remove_file(written);
                }
            }
            return Err(error);
        }
        restored.push((target, existed));
    }
    Ok(restored.into_iter().map(|(path, _)| path).collect())
}

pub fn preflight_home_files<'a>(
    bundle: &'a VerifiedBundle,
    home: &Path,
) -> Result<Vec<(PathBuf, &'a [u8])>, GhError> {
    preflight_home_files_with(bundle, home, |_, existing, bundled| existing == bundled)
}

pub fn preflight_home_files_with<'a, F>(
    bundle: &'a VerifiedBundle,
    home: &Path,
    equivalent: F,
) -> Result<Vec<(PathBuf, &'a [u8])>, GhError>
where
    F: Fn(&str, &[u8], &[u8]) -> bool,
{
    let mut targets = Vec::new();
    for file in &bundle.manifest.files {
        let Some(relative) = file.native_path.as_deref() else {
            continue;
        };
        let target = home.join(relative);
        reject_symlinked_destination(home, Path::new(relative))?;
        let bytes = bundle.files.get(&file.path).expect("verified file exists");
        match fs::read(&target) {
            Ok(existing) if equivalent(&file.role, &existing, bytes) => {
                // Preserve equivalent local files rather than rewriting them.
                continue;
            }
            Ok(_) => {
                return Err(GhError::config(format!(
                    "native session collision at {}; the local file differs and was not changed",
                    target.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(GhError::Io {
                    path: target,
                    source,
                })
            }
        }
        targets.push((target, bytes.as_slice()));
    }
    Ok(targets)
}

fn reject_symlinked_destination(home: &Path, relative: &Path) -> Result<(), GhError> {
    let mut current = home.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if destination_is_link_or_reparse(&metadata) => {
                return Err(invalid(format!(
                    "native destination traverses symlink {}",
                    current.display()
                )))
            }
            Ok(metadata) if current != home.join(relative) && !metadata.is_dir() => {
                return Err(invalid(format!(
                    "native destination parent is not a directory: {}",
                    current.display()
                )))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(source) => {
                return Err(GhError::Io {
                    path: current,
                    source,
                })
            }
        }
    }
    Ok(())
}

fn destination_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    false
}

pub fn ensure_safe_home_destination(home: &Path, target: &Path) -> Result<(), GhError> {
    let relative = target
        .strip_prefix(home)
        .map_err(|_| invalid("native destination is outside the home directory"))?;
    if !safe_relative(relative) {
        return Err(invalid(
            "native destination is not a safe home-relative path",
        ));
    }
    reject_symlinked_destination(home, relative)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("blue-bundle-{name}-{}", std::process::id()))
    }

    fn encode_archive(build: impl FnOnce(&mut tar::Builder<GzEncoder<Vec<u8>>>)) -> Vec<u8> {
        let mut archive = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
        build(&mut archive);
        archive.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn round_trip_and_collision_protection() {
        let root = temp("roundtrip");
        let source = root.join("source.jsonl");
        fs::create_dir_all(&root).unwrap();
        fs::write(&source, b"{\"hello\":\"world\"}\n").unwrap();
        let made = create(
            Harness::Codex,
            "codex-v0_145_0",
            "s1",
            None,
            Some("Title".into()),
            None,
            vec![SessionSource {
                role: "rollout".into(),
                source,
                native_path: Some(PathBuf::from(".codex/sessions/s1.jsonl")),
            }],
        )
        .unwrap();
        let verified = verify(&made.bytes).unwrap();
        let home = root.join("home");
        restore_home_files(&verified, &home).unwrap();
        assert_eq!(
            fs::read(home.join(".codex/sessions/s1.jsonl")).unwrap(),
            b"{\"hello\":\"world\"}\n"
        );
        fs::write(home.join(".codex/sessions/s1.jsonl"), b"different").unwrap();
        assert!(restore_home_files(&verified, &home)
            .unwrap_err()
            .to_string()
            .contains("collision"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_truncated_and_tampered_archives() {
        assert!(verify(b"not gzip").is_err());
    }

    #[test]
    fn rejects_symlinks_duplicate_entries_and_bad_digests() {
        let symlink = encode_archive(|archive| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            header.set_link_name("/tmp/escape").unwrap();
            header.set_cksum();
            archive
                .append_data(&mut header, "files/link", Cursor::new([]))
                .unwrap();
        });
        assert!(verify(&symlink)
            .unwrap_err()
            .to_string()
            .contains("regular-file"));

        let duplicate = encode_archive(|archive| {
            append(archive, "manifest.json", b"{}").unwrap();
            append(archive, "manifest.json", b"{}").unwrap();
        });
        assert!(verify(&duplicate)
            .unwrap_err()
            .to_string()
            .contains("duplicate"));

        let manifest = BundleManifest {
            schema_version: 1,
            artifact_format: ARTIFACT_FORMAT.into(),
            harness: "codex".into(),
            compatibility_profile: "codex-v0_145_0".into(),
            native_session_id: "s".into(),
            captured_at_unix_ms: 0,
            cwd: None,
            repository: None,
            title: None,
            summary: None,
            files: vec![BundleFile {
                role: "rollout".into(),
                path: "files/a".into(),
                native_path: Some(".codex/sessions/s.jsonl".into()),
                size: 3,
                sha256: "0".repeat(64),
            }],
        };
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let bad_digest = encode_archive(|archive| {
            append(archive, "manifest.json", &manifest).unwrap();
            append(archive, "files/a", b"abc").unwrap();
        });
        assert!(verify(&bad_digest)
            .unwrap_err()
            .to_string()
            .contains("digest"));
    }

    #[test]
    fn rejects_cross_harness_and_wrong_session_destinations() {
        let root = temp("destination-contract");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("rollout.jsonl");
        fs::write(&source, b"{}\n").unwrap();
        for destination in [
            ".ssh/config",
            ".claude/projects/s/rollout.jsonl",
            ".codex/sessions/a-different-session.jsonl",
        ] {
            let error = create(
                Harness::Codex,
                "codex-v0_145_0",
                "expected-session",
                None,
                None,
                None,
                vec![SessionSource {
                    role: "rollout".into(),
                    source: source.clone(),
                    native_path: Some(PathBuf::from(destination)),
                }],
            )
            .unwrap_err();
            assert!(error
                .to_string()
                .contains("outside the advertised native session"));
        }
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn restore_rejects_symlinked_destination_parents() {
        use std::os::unix::fs::symlink;

        let root = temp("restore-parent-symlink");
        let source = root.join("source.jsonl");
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(&source, b"{}\n").unwrap();
        let made = create(
            Harness::Codex,
            "codex-v0_145_0",
            "s1",
            None,
            None,
            None,
            vec![SessionSource {
                role: "rollout".into(),
                source,
                native_path: Some(PathBuf::from(".codex/sessions/s1.jsonl")),
            }],
        )
        .unwrap();
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        symlink(&outside, home.join(".codex")).unwrap();
        let error = restore_home_files(&verify(&made.bytes).unwrap(), &home).unwrap_err();
        assert!(error.to_string().contains("traverses symlink"));
        assert!(!outside.join("sessions/s1.jsonl").exists());
        let _ = fs::remove_dir_all(root);
    }
}
