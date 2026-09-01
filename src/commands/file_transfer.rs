use crate::ftp::{Entry, ExactFilePresence, Ftp, Remote};
use crate::hash::{hash_bytes, hash_file};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use same_file::Handle;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalLeafKind {
    RegularFile,
    SymlinkToFile,
    Directory,
    SymlinkToDirectory,
}

impl LocalLeafKind {
    pub(crate) fn is_file(self) -> bool {
        matches!(self, Self::RegularFile | Self::SymlinkToFile)
    }
}

#[derive(Debug)]
struct PresentLocalEntry {
    kind: LocalLeafKind,
    canonical: PathBuf,
    identity: Handle,
    size: u64,
    modified: SystemTime,
    sha256: Option<String>,
}

#[derive(Debug)]
enum LocalEntrySnapshot {
    Missing,
    Present(PresentLocalEntry),
}

/// Immutable identity and containment snapshot for one local leaf.
///
/// Scoped mutations use the canonical parent plus `leaf`, never the unresolved
/// caller path, so a stable in-root symlinked ancestor cannot redirect a
/// staged or committed write.
#[derive(Debug)]
pub(crate) struct LocalPathExpectation {
    canonical_root: PathBuf,
    root_identity: Handle,
    canonical_parent: PathBuf,
    parent_identity: Handle,
    leaf: OsString,
    entry: LocalEntrySnapshot,
}

impl LocalPathExpectation {
    pub(crate) fn capture(local_root: &Path, path: &Path) -> Result<Self> {
        let canonical_root = local_root
            .canonicalize()
            .with_context(|| format!("canonicalizing local root {}", local_root.display()))?;
        let root_metadata = std::fs::symlink_metadata(&canonical_root)
            .with_context(|| format!("reading local root {}", canonical_root.display()))?;
        if !root_metadata.is_dir() {
            anyhow::bail!("local root {} is not a directory", local_root.display());
        }
        let root_identity = Handle::from_path(&canonical_root)
            .with_context(|| format!("opening local root {}", canonical_root.display()))?;

        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("local target {} has no parent", path.display()))?;
        let parent_symlink_metadata = std::fs::symlink_metadata(parent)
            .with_context(|| format!("reading local target parent {}", parent.display()))?;
        let canonical_parent = parent
            .canonicalize()
            .with_context(|| format!("canonicalizing local target parent {}", parent.display()))?;
        let parent_metadata = if parent_symlink_metadata.file_type().is_symlink() {
            std::fs::metadata(parent)
                .with_context(|| format!("following local target parent {}", parent.display()))?
        } else {
            parent_symlink_metadata
        };
        if !parent_metadata.is_dir() {
            anyhow::bail!(
                "local target parent {} is not a directory",
                parent.display()
            );
        }
        if !canonical_parent.starts_with(&canonical_root) {
            anyhow::bail!(
                "local target parent {} resolves outside local root {}",
                parent.display(),
                local_root.display()
            );
        }
        let parent_identity = Handle::from_path(&canonical_parent)
            .with_context(|| format!("opening local target parent {}", parent.display()))?;
        let leaf = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("local target {} has no file name", path.display()))?
            .to_os_string();
        let resolved = canonical_parent.join(&leaf);
        let entry = capture_local_entry(&canonical_root, &resolved)?;

        Ok(Self {
            canonical_root,
            root_identity,
            canonical_parent,
            parent_identity,
            leaf,
            entry,
        })
    }

    pub(crate) fn resolved_path(&self) -> PathBuf {
        self.canonical_parent.join(&self.leaf)
    }

    pub(crate) fn verify_supplied_path(&self, path: &Path) -> Result<()> {
        if path.file_name() != Some(self.leaf.as_os_str()) {
            anyhow::bail!("local path leaf changed");
        }
        let supplied_parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("local path has no parent"))?;
        let supplied_parent_metadata =
            std::fs::symlink_metadata(supplied_parent).with_context(|| {
                format!("reading local target parent {}", supplied_parent.display())
            })?;
        let supplied_parent_canonical = supplied_parent.canonicalize().with_context(|| {
            format!(
                "canonicalizing local target parent {}",
                supplied_parent.display()
            )
        })?;
        let followed = if supplied_parent_metadata.file_type().is_symlink() {
            std::fs::metadata(supplied_parent).with_context(|| {
                format!(
                    "following local target parent {}",
                    supplied_parent.display()
                )
            })?
        } else {
            supplied_parent_metadata
        };
        if !followed.is_dir() || supplied_parent_canonical != self.canonical_parent {
            anyhow::bail!("local path parent changed");
        }
        Ok(())
    }

    pub(crate) fn verify_anchor(&self) -> Result<()> {
        let current_root = self.canonical_root.canonicalize().with_context(|| {
            format!(
                "canonicalizing local root {}",
                self.canonical_root.display()
            )
        })?;
        if current_root != self.canonical_root {
            anyhow::bail!("local root changed");
        }
        let root_metadata = std::fs::symlink_metadata(&self.canonical_root)
            .with_context(|| format!("reading local root {}", self.canonical_root.display()))?;
        if !root_metadata.is_dir()
            || Handle::from_path(&self.canonical_root)
                .with_context(|| format!("opening local root {}", self.canonical_root.display()))?
                != self.root_identity
        {
            anyhow::bail!("local root changed");
        }

        let current_parent = self.canonical_parent.canonicalize().with_context(|| {
            format!(
                "canonicalizing local target parent {}",
                self.canonical_parent.display()
            )
        })?;
        if current_parent != self.canonical_parent
            || !current_parent.starts_with(&self.canonical_root)
        {
            anyhow::bail!("local target parent changed");
        }
        let parent_metadata =
            std::fs::symlink_metadata(&self.canonical_parent).with_context(|| {
                format!(
                    "reading local target parent {}",
                    self.canonical_parent.display()
                )
            })?;
        if !parent_metadata.is_dir()
            || Handle::from_path(&self.canonical_parent).with_context(|| {
                format!(
                    "opening local target parent {}",
                    self.canonical_parent.display()
                )
            })? != self.parent_identity
        {
            anyhow::bail!("local target parent changed");
        }
        Ok(())
    }

    pub(crate) fn verify_unchanged(&self) -> Result<()> {
        self.verify_anchor()?;
        let current = capture_local_entry(&self.canonical_root, &self.resolved_path())?;
        if !same_local_entry(&self.entry, &current) {
            anyhow::bail!("local entry changed");
        }
        Ok(())
    }

    pub(crate) fn is_file_or_missing(&self) -> bool {
        match &self.entry {
            LocalEntrySnapshot::Missing => true,
            LocalEntrySnapshot::Present(entry) => entry.kind.is_file(),
        }
    }

    pub(crate) fn expected_file_hash(&self) -> Option<&str> {
        match &self.entry {
            LocalEntrySnapshot::Present(entry) if entry.kind.is_file() => entry.sha256.as_deref(),
            _ => None,
        }
    }
}

fn capture_local_entry(canonical_root: &Path, path: &Path) -> Result<LocalEntrySnapshot> {
    let symlink_metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LocalEntrySnapshot::Missing);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading local entry {}", path.display()));
        }
    };
    let is_symlink = symlink_metadata.file_type().is_symlink();
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalizing local entry {}", path.display()))?;
    if !canonical.starts_with(canonical_root) {
        anyhow::bail!("local entry {} resolves outside local root", path.display());
    }
    let metadata = if is_symlink {
        std::fs::metadata(path)
            .with_context(|| format!("following local entry {}", path.display()))?
    } else {
        symlink_metadata
    };
    let kind = match (is_symlink, metadata.is_file(), metadata.is_dir()) {
        (false, true, false) => LocalLeafKind::RegularFile,
        (true, true, false) => LocalLeafKind::SymlinkToFile,
        (false, false, true) => LocalLeafKind::Directory,
        (true, false, true) => LocalLeafKind::SymlinkToDirectory,
        _ => anyhow::bail!("unsupported local entry type at {}", path.display()),
    };
    let sha256 = kind.is_file().then(|| hash_file(path)).transpose()?;
    Ok(LocalEntrySnapshot::Present(PresentLocalEntry {
        kind,
        canonical,
        identity: Handle::from_path(path)
            .with_context(|| format!("opening local entry {}", path.display()))?,
        size: metadata.len(),
        modified: metadata
            .modified()
            .with_context(|| format!("reading modified time for {}", path.display()))?,
        sha256,
    }))
}

fn same_local_entry(expected: &LocalEntrySnapshot, current: &LocalEntrySnapshot) -> bool {
    match (expected, current) {
        (LocalEntrySnapshot::Missing, LocalEntrySnapshot::Missing) => true,
        (LocalEntrySnapshot::Present(expected), LocalEntrySnapshot::Present(current)) => {
            expected.kind == current.kind
                && expected.canonical == current.canonical
                && expected.identity == current.identity
                && expected.size == current.size
                && expected.modified == current.modified
                && expected.sha256 == current.sha256
        }
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteDestinationSnapshot {
    Missing,
    File {
        size: u64,
        modified: DateTime<Utc>,
        sha256: String,
    },
    Directory,
}

pub(crate) trait RemoteWrite {
    fn upload_bytes(&mut self, path: &str, bytes: &[u8]) -> Result<()>;
    fn rename(&mut self, from: &str, to: &str) -> Result<()>;
    fn rm(&mut self, path: &str) -> Result<()>;
    #[allow(dead_code, reason = "wired by scoped directory commits in Task 6")]
    fn mkdir(&mut self, path: &str) -> Result<()>;
    #[allow(dead_code, reason = "wired by scoped directory commits in Task 6")]
    fn mkdir_scoped_strict(&mut self, path: &str) -> Result<()>;
    fn mtime(&mut self, path: &str) -> Result<DateTime<Utc>>;
    fn destination_snapshot(
        &mut self,
        remote_root: &str,
        path: &str,
    ) -> Result<RemoteDestinationSnapshot>;
}

trait StrictDestinationRead {
    fn list_destination_strict(&mut self, directory: &str) -> Result<Vec<Entry>>;
    fn download_destination(&mut self, path: &str) -> Result<Vec<u8>>;
}

impl StrictDestinationRead for Ftp {
    fn list_destination_strict(&mut self, directory: &str) -> Result<Vec<Entry>> {
        Ftp::list_strict(self, directory)
    }

    fn download_destination(&mut self, path: &str) -> Result<Vec<u8>> {
        Ftp::download_scoped(self, path)
    }
}

impl RemoteWrite for Ftp {
    fn upload_bytes(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
        Ftp::upload_bytes_scoped(self, path, bytes)
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        Ftp::rename_scoped(self, from, to)
    }

    fn rm(&mut self, path: &str) -> Result<()> {
        Ftp::rm_scoped(self, path)
    }

    fn mkdir(&mut self, path: &str) -> Result<()> {
        Ftp::mkdir(self, path)
    }

    fn mkdir_scoped_strict(&mut self, path: &str) -> Result<()> {
        Ftp::mkdir_scoped_strict(self, path)
    }

    fn mtime(&mut self, path: &str) -> Result<DateTime<Utc>> {
        Ftp::mtime_scoped(self, path)
    }

    fn destination_snapshot(
        &mut self,
        remote_root: &str,
        path: &str,
    ) -> Result<RemoteDestinationSnapshot> {
        snapshot_remote_destination(self, remote_root, path)
    }
}

fn snapshot_remote_destination<R: StrictDestinationRead + ?Sized>(
    remote: &mut R,
    remote_root: &str,
    path: &str,
) -> Result<RemoteDestinationSnapshot> {
    match resolve_remote_destination(remote, remote_root, path)? {
        StrictRemoteResolution::Missing => Ok(RemoteDestinationSnapshot::Missing),
        StrictRemoteResolution::Directory => Ok(RemoteDestinationSnapshot::Directory),
        StrictRemoteResolution::File {
            size: before_size,
            modified: before_modified,
        } => {
            let bytes = remote
                .download_destination(path)
                .with_context(|| format!("downloading remote destination {path:?}"))?;
            let after = resolve_remote_destination(remote, remote_root, path)?;
            let StrictRemoteResolution::File {
                size: after_size,
                modified: after_modified,
            } = after
            else {
                anyhow::bail!("remote destination changed while hashing {path:?}");
            };
            if before_size != after_size
                || before_modified != after_modified
                || bytes.len() as u64 != before_size
            {
                anyhow::bail!("remote destination changed while hashing {path:?}");
            }
            Ok(RemoteDestinationSnapshot::File {
                size: before_size,
                modified: before_modified,
                sha256: hash_bytes(&bytes),
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StrictRemoteResolution {
    Missing,
    File { size: u64, modified: DateTime<Utc> },
    Directory,
}

/// Resolve a destination from the configured remote root through complete,
/// strict directory listings. Only absence of the final leaf from a
/// successfully parsed parent listing is authoritative `Missing`.
fn resolve_remote_destination<R: StrictDestinationRead + ?Sized>(
    remote: &mut R,
    remote_root: &str,
    path: &str,
) -> Result<StrictRemoteResolution> {
    let remote_root = normalize_remote_root(remote_root)?;
    let relative = relative_remote_destination(&remote_root, path)?;
    let segments = relative.split('/').collect::<Vec<_>>();
    let mut directory = remote_root.clone();

    for (index, expected) in segments.iter().enumerate() {
        let entries = remote
            .list_destination_strict(&directory)
            .with_context(|| format!("listing remote directory {directory:?}"))?;
        let mut names = BTreeSet::new();
        let mut matched = None;
        for entry in entries {
            if entry.name == "." || entry.name == ".." {
                continue;
            }
            let name = strict_remote_child_name(&directory, &entry)?;
            if !names.insert(name.clone()) {
                anyhow::bail!("duplicate remote entry {name:?} in {directory:?}");
            }
            if name == *expected {
                matched = Some(entry);
            }
        }

        let is_leaf = index + 1 == segments.len();
        let Some(entry) = matched else {
            if is_leaf {
                return Ok(StrictRemoteResolution::Missing);
            }
            anyhow::bail!("remote destination parent is missing below {directory:?}");
        };
        // A symlink is neither a safe destination nor a safe path to descend:
        // the server resolves the target, which can sit outside the configured
        // remote root. Refuse before the leaf/parent split so both are covered.
        if entry.is_symlink {
            anyhow::bail!(
                "remote destination is a symlink at {expected:?} in {directory:?}: refusing to follow it because the target can resolve outside the configured remote root"
            );
        }
        if is_leaf {
            return Ok(if entry.is_dir {
                StrictRemoteResolution::Directory
            } else {
                StrictRemoteResolution::File {
                    size: entry.size,
                    modified: entry.modified,
                }
            });
        }
        if !entry.is_dir {
            anyhow::bail!("remote destination parent is not a directory at {expected:?}");
        }
        directory = remote_path_join(&directory, expected);
    }

    unreachable!("validated destination has at least one segment")
}

fn normalize_remote_root(root: &str) -> Result<String> {
    if root.is_empty() {
        anyhow::bail!("remote_root must not be empty");
    }
    let trimmed = root.trim_end_matches('/');
    Ok(if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    })
}

fn relative_remote_destination<'a>(remote_root: &str, path: &'a str) -> Result<&'a str> {
    if path.chars().any(char::is_control) || path.contains('\\') || path.ends_with('/') {
        anyhow::bail!("unsafe remote destination");
    }
    let relative = if remote_root == "/" {
        path.strip_prefix('/')
    } else {
        path.strip_prefix(remote_root)
            .and_then(|suffix| suffix.strip_prefix('/'))
    }
    .ok_or_else(|| anyhow::anyhow!("remote destination is outside configured remote root"))?;
    if relative.is_empty()
        || relative
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        anyhow::bail!("unsafe remote destination");
    }
    Ok(relative)
}

fn strict_remote_child_name(directory: &str, entry: &Entry) -> Result<String> {
    let supplied = entry.name.as_str();
    if supplied.chars().any(char::is_control) {
        anyhow::bail!("unsafe remote entry in {directory:?}");
    }
    let name = if !supplied.contains('/') && !supplied.contains('\\') {
        supplied
    } else if supplied.starts_with('/') {
        let prefix = if directory == "/" {
            "/".to_string()
        } else {
            format!("{}/", directory.trim_end_matches('/'))
        };
        supplied.strip_prefix(&prefix).unwrap_or("")
    } else {
        ""
    };
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        anyhow::bail!("unsafe remote entry in {directory:?}");
    }
    Ok(name.to_string())
}

fn remote_path_join(directory: &str, name: &str) -> String {
    if directory == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", directory.trim_end_matches('/'))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferStatus {
    Unchanged,
    Transferred,
    SkippedMissingSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferOutcome {
    pub path: String,
    pub status: TransferStatus,
}

impl TransferOutcome {
    pub fn new(path: &str, status: TransferStatus) -> Self {
        Self {
            path: path.to_string(),
            status,
        }
    }
}

/// Proven remote-file presence. A failed metadata probe is never represented
/// as `Missing`: callers must return the indeterminate error instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemotePresence {
    Present,
    Missing,
}

/// Determine whether `path` names a remote file without failing open.
///
/// A successful `SIZE` proves presence. Servers that do not support `SIZE`
/// are common, so its error is followed by an exact `NLST` lookup. An
/// authoritative exact lookup proves either presence or absence; when neither
/// operation succeeds we preserve both errors and make no transfer decision.
pub fn probe_remote_file<R: Remote + ?Sized>(remote: &mut R, path: &str) -> Result<RemotePresence> {
    match remote.file_size(path) {
        Ok(_) => Ok(RemotePresence::Present),
        Err(size_error) => {
            match remote.exact_file_presence(path) {
                Ok(ExactFilePresence::Present) => Ok(RemotePresence::Present),
                Ok(ExactFilePresence::Missing) => Ok(RemotePresence::Missing),
                Err(exact_error) => Err(size_error).with_context(|| {
                    format!(
                        "remote presence for {path} is indeterminate after exact lookup: {exact_error:#}"
                    )
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RemoteDestinationSnapshot, RemotePresence, StrictDestinationRead, TransferOutcome,
        TransferStatus, normalize_remote_root, probe_remote_file, snapshot_remote_destination,
    };
    use crate::ftp::{Entry, ExactFilePresence, Remote};
    use crate::hash::hash_bytes;
    use anyhow::Result;
    use chrono::{DateTime, TimeZone, Utc};
    use std::collections::BTreeMap;

    struct ScriptedRemote {
        size: Option<Result<u64>>,
        listing: Option<Result<Vec<Entry>>>,
        exact: Option<Result<ExactFilePresence>>,
    }

    impl Remote for ScriptedRemote {
        fn list_dir(&mut self, _dir: &str) -> Result<Vec<Entry>> {
            self.listing.take().expect("one LIST call")
        }

        fn file_size(&mut self, _path: &str) -> Result<u64> {
            self.size.take().expect("one SIZE call")
        }

        fn exact_file_presence(&mut self, _path: &str) -> Result<ExactFilePresence> {
            self.exact.take().expect("one exact probe call")
        }
    }

    #[derive(Default)]
    struct ScriptedDestinationRead {
        listings: BTreeMap<String, Vec<Entry>>,
        downloads: BTreeMap<String, Vec<u8>>,
        events: Vec<String>,
    }

    impl StrictDestinationRead for ScriptedDestinationRead {
        fn list_destination_strict(&mut self, directory: &str) -> Result<Vec<Entry>> {
            self.events.push(format!("list {directory}"));
            self.listings
                .get(directory)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unexpected LIST {directory}"))
        }

        fn download_destination(&mut self, path: &str) -> Result<Vec<u8>> {
            self.events.push(format!("download {path}"));
            self.downloads
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unexpected RETR {path}"))
        }
    }

    fn destination_entry(name: &str, is_dir: bool, bytes: &[u8], modified: DateTime<Utc>) -> Entry {
        Entry {
            name: name.to_string(),
            is_dir,
            is_symlink: false,
            size: bytes.len() as u64,
            modified,
        }
    }

    fn file(name: &str, size: u64) -> Entry {
        Entry {
            name: name.to_string(),
            is_dir: false,
            is_symlink: false,
            size,
            modified: Utc::now(),
        }
    }

    fn symlink_entry(name: &str) -> Entry {
        Entry {
            name: name.to_string(),
            is_dir: false,
            is_symlink: true,
            size: 8,
            modified: Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap(),
        }
    }

    #[test]
    fn a_symlink_leaf_destination_is_refused_rather_than_reported_missing() {
        let mut remote = ScriptedDestinationRead::default();
        remote
            .listings
            .insert("/root".into(), vec![symlink_entry("secrets.c")]);

        let error =
            snapshot_remote_destination(&mut remote, "/root", "/root/secrets.c").unwrap_err();

        // Reporting `Missing` here is what let a guarded upload STOR straight
        // through the link and out of the configured remote root.
        assert!(format!("{error:#}").contains("remote destination is a symlink"));
    }

    #[test]
    fn a_symlink_parent_segment_is_refused_before_descending_into_it() {
        let mut remote = ScriptedDestinationRead::default();
        remote
            .listings
            .insert("/root".into(), vec![symlink_entry("nested")]);

        let error =
            snapshot_remote_destination(&mut remote, "/root", "/root/nested/page.txt").unwrap_err();

        assert!(format!("{error:#}").contains("symlink"));
        // The traversal must stop at the link, never list through it.
        assert_eq!(remote.events, ["list /root"]);
    }

    #[test]
    fn a_symlink_refusal_message_carries_no_control_characters() {
        let mut remote = ScriptedDestinationRead::default();
        remote
            .listings
            .insert("/root".into(), vec![symlink_entry("secrets.c")]);

        let error =
            snapshot_remote_destination(&mut remote, "/root", "/root/secrets.c").unwrap_err();

        let message = format!("{error:#}");
        assert!(!message.chars().any(char::is_control));
    }

    #[test]
    fn configured_relative_remote_root_is_preserved() {
        assert_eq!(normalize_remote_root("public_html").unwrap(), "public_html");
    }

    #[test]
    fn configured_dot_remote_root_is_preserved() {
        assert_eq!(normalize_remote_root(".").unwrap(), ".");
    }

    #[test]
    fn relative_remote_root_strictly_traverses_and_hashes_the_destination() {
        let bytes = b"remote bytes";
        let modified = Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap();
        let mut remote = ScriptedDestinationRead::default();
        remote.listings.insert(
            "public_html".into(),
            vec![destination_entry("nested", true, b"", modified)],
        );
        remote.listings.insert(
            "public_html/nested".into(),
            vec![destination_entry("page.txt", false, bytes, modified)],
        );
        remote
            .downloads
            .insert("public_html/nested/page.txt".into(), bytes.to_vec());

        let snapshot =
            snapshot_remote_destination(&mut remote, "public_html", "public_html/nested/page.txt")
                .unwrap();

        assert_eq!(
            snapshot,
            RemoteDestinationSnapshot::File {
                size: bytes.len() as u64,
                modified,
                sha256: hash_bytes(bytes),
            }
        );
        assert_eq!(
            remote.events,
            [
                "list public_html",
                "list public_html/nested",
                "download public_html/nested/page.txt",
                "list public_html",
                "list public_html/nested",
            ]
        );
    }

    #[test]
    fn dot_remote_root_strictly_snapshots_without_server_root_assumption() {
        let bytes = b"dot-root bytes";
        let modified = Utc.with_ymd_and_hms(2026, 8, 10, 12, 1, 0).unwrap();
        let mut remote = ScriptedDestinationRead::default();
        remote.listings.insert(
            ".".into(),
            vec![destination_entry("page.txt", false, bytes, modified)],
        );
        remote.downloads.insert("./page.txt".into(), bytes.to_vec());

        let snapshot = snapshot_remote_destination(&mut remote, ".", "./page.txt").unwrap();

        assert_eq!(
            snapshot,
            RemoteDestinationSnapshot::File {
                size: bytes.len() as u64,
                modified,
                sha256: hash_bytes(bytes),
            }
        );
        assert_eq!(remote.events, ["list .", "download ./page.txt", "list ."]);
    }

    #[test]
    fn relative_remote_root_rejects_a_destination_outside_its_boundary() {
        let mut remote = ScriptedDestinationRead::default();

        let error = snapshot_remote_destination(
            &mut remote,
            "public_html",
            "public_html-elsewhere/page.txt",
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("outside configured remote root"));
        assert!(remote.events.is_empty());
    }

    #[test]
    fn outcome_keeps_the_relative_path_and_status() {
        assert_eq!(
            TransferOutcome::new("src/main.rs", TransferStatus::Transferred),
            TransferOutcome {
                path: "src/main.rs".to_string(),
                status: TransferStatus::Transferred,
            }
        );
    }

    #[test]
    fn size_failure_falls_back_to_exact_lookup_for_an_existing_file() {
        let mut remote = ScriptedRemote {
            size: Some(Err(anyhow::anyhow!("SIZE unsupported"))),
            listing: Some(Ok(vec![file("target.txt", 7)])),
            exact: Some(Ok(ExactFilePresence::Present)),
        };

        assert_eq!(
            probe_remote_file(&mut remote, "/home/test/target.txt").unwrap(),
            RemotePresence::Present
        );
    }

    #[test]
    fn size_failure_accepts_an_exact_lookup_of_the_full_file_path() {
        let mut remote = ScriptedRemote {
            size: Some(Err(anyhow::anyhow!("SIZE unsupported"))),
            listing: Some(Ok(vec![file("/home/test/target.txt", 7)])),
            exact: Some(Ok(ExactFilePresence::Present)),
        };

        assert_eq!(
            probe_remote_file(&mut remote, "/home/test/target.txt").unwrap(),
            RemotePresence::Present
        );
    }

    #[test]
    fn size_failure_exact_lookup_proves_a_file_is_missing() {
        let mut remote = ScriptedRemote {
            size: Some(Err(anyhow::anyhow!("SIZE unsupported"))),
            listing: Some(Ok(vec![])),
            exact: Some(Ok(ExactFilePresence::Missing)),
        };

        assert_eq!(
            probe_remote_file(&mut remote, "/home/test/target.txt").unwrap(),
            RemotePresence::Missing
        );
    }

    #[test]
    fn size_and_exact_lookup_failure_is_indeterminate() {
        let mut remote = ScriptedRemote {
            size: Some(Err(anyhow::anyhow!("SIZE unsupported"))),
            listing: Some(Err(anyhow::anyhow!("LIST permission denied"))),
            exact: Some(Err(anyhow::anyhow!("NLST permission denied"))),
        };

        let error = probe_remote_file(&mut remote, "/home/test/target.txt").unwrap_err();

        assert!(format!("{error:#}").contains("SIZE unsupported"));
        assert!(format!("{error:#}").contains("NLST permission denied"));
    }

    #[test]
    fn incomplete_exact_lookup_cannot_prove_file_absence() {
        let mut remote = ScriptedRemote {
            size: Some(Err(anyhow::anyhow!("SIZE unsupported"))),
            listing: Some(Ok(vec![file("other.txt", 3)])),
            exact: Some(Err(anyhow::anyhow!("NLST malformed response"))),
        };

        let error = probe_remote_file(&mut remote, "/home/test/target.txt").unwrap_err();

        assert!(format!("{error:#}").contains("NLST malformed response"));
    }
}
