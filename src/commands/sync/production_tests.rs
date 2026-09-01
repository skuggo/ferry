use super::commit::{CommitDecision, CommitGate, UnconditionalCommitGate};
use super::scope::SyncScope;
use super::{EntryKind, SyncEvent, SyncEventKind, SyncIssue, SyncOutcome, run_scoped_with};
use crate::commands::ExecutionMode;
use crate::commands::file_transfer::{RemoteDestinationSnapshot, RemoteWrite};
use crate::commands::remote_hash::RemoteFileRetrieval;
use crate::ftp::{Entry, ExactFilePresence, Remote, StrictRemote};
use crate::hash::hash_bytes;
use crate::ignored::Matcher;
use crate::state::{FileRecord, StateFile};
use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct TestRemoteFile {
    bytes: Vec<u8>,
    modified: DateTime<Utc>,
}

#[derive(Default)]
struct ProductionRemote {
    directories: BTreeSet<String>,
    files: BTreeMap<String, TestRemoteFile>,
    events: Vec<String>,
    invalidate_on_snapshot: Option<(String, usize, usize, Arc<AtomicBool>)>,
    snapshot_mutation: Option<SnapshotMutation>,
    invalidate_on_download: Option<(String, Arc<AtomicBool>)>,
    mutate_on_download: Option<DownloadMutation>,
    strict_mkdir_error: Option<String>,
    mdtm_error: Option<String>,
}

struct DownloadMutation {
    path: String,
    on_call: usize,
    calls: usize,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy)]
enum SnapshotMutationKind {
    RemoveDirectory,
    ReplaceDirectoryWithFile,
    AddDirectory,
    AddFile,
}

struct SnapshotMutation {
    path: String,
    on_call: usize,
    calls: usize,
    kind: SnapshotMutationKind,
}

impl ProductionRemote {
    fn with_root() -> Self {
        Self {
            directories: BTreeSet::from(["/remote".to_string()]),
            ..Self::default()
        }
    }

    fn directory(mut self, path: &str) -> Self {
        self.directories.insert(path.to_string());
        self
    }

    fn file(mut self, path: &str, bytes: &[u8]) -> Self {
        self.files.insert(
            path.to_string(),
            TestRemoteFile {
                bytes: bytes.to_vec(),
                modified: test_mtime(1),
            },
        );
        self
    }

    fn mutate_snapshot(mut self, path: &str, on_call: usize, kind: SnapshotMutationKind) -> Self {
        self.snapshot_mutation = Some(SnapshotMutation {
            path: path.to_string(),
            on_call,
            calls: 0,
            kind,
        });
        self
    }

    fn fail_strict_mkdir(mut self, message: &str) -> Self {
        self.strict_mkdir_error = Some(message.to_string());
        self
    }

    fn mutate_download(mut self, path: &str, on_call: usize, bytes: &[u8]) -> Self {
        self.mutate_on_download = Some(DownloadMutation {
            path: path.to_string(),
            on_call,
            calls: 0,
            bytes: bytes.to_vec(),
        });
        self
    }
    fn fail_mdtm(mut self, message: &str) -> Self {
        self.mdtm_error = Some(message.to_string());
        self
    }

    fn apply_snapshot_mutation(&mut self, path: &str) {
        let kind = self.snapshot_mutation.as_mut().and_then(|mutation| {
            if mutation.path != path {
                return None;
            }
            mutation.calls += 1;
            (mutation.calls == mutation.on_call).then_some(mutation.kind)
        });
        match kind {
            Some(SnapshotMutationKind::RemoveDirectory) => {
                self.directories.remove(path);
            }
            Some(SnapshotMutationKind::ReplaceDirectoryWithFile) => {
                self.directories.remove(path);
                self.files.insert(
                    path.to_string(),
                    TestRemoteFile {
                        bytes: b"raced file".to_vec(),
                        modified: test_mtime(40),
                    },
                );
            }
            Some(SnapshotMutationKind::AddDirectory) => {
                self.directories.insert(path.to_string());
            }
            Some(SnapshotMutationKind::AddFile) => {
                self.files.insert(
                    path.to_string(),
                    TestRemoteFile {
                        bytes: b"raced file".to_vec(),
                        modified: test_mtime(40),
                    },
                );
            }
            None => {}
        }
    }

    fn parent(path: &str) -> &str {
        match path.rsplit_once('/') {
            Some(("", _)) => "/",
            Some((parent, _)) => parent,
            None => "",
        }
    }

    fn child_name<'a>(directory: &str, path: &'a str) -> Option<&'a str> {
        let suffix = path.strip_prefix(directory)?.strip_prefix('/')?;
        (!suffix.is_empty() && !suffix.contains('/')).then_some(suffix)
    }

    fn entry(name: &str, is_dir: bool, size: u64, modified: DateTime<Utc>) -> Entry {
        Entry {
            name: name.to_string(),
            is_dir,
            is_symlink: false,
            size,
            modified,
        }
    }

    fn strict_children(&self, directory: &str) -> Vec<Entry> {
        let mut children = BTreeMap::new();
        for path in &self.directories {
            if path == directory {
                continue;
            }
            if let Some(name) = Self::child_name(directory, path) {
                children.insert(name.to_string(), Self::entry(name, true, 0, test_mtime(0)));
            }
        }
        for (path, file) in &self.files {
            if let Some(name) = Self::child_name(directory, path) {
                children.insert(
                    name.to_string(),
                    Self::entry(name, false, file.bytes.len() as u64, file.modified),
                );
            }
        }
        children.into_values().collect()
    }

    fn snapshot(&self, path: &str) -> Result<RemoteDestinationSnapshot> {
        if self.directories.contains(path) {
            return Ok(RemoteDestinationSnapshot::Directory);
        }
        if let Some(file) = self.files.get(path) {
            return Ok(RemoteDestinationSnapshot::File {
                size: file.bytes.len() as u64,
                modified: file.modified,
                sha256: hash_bytes(&file.bytes),
            });
        }
        if !self.directories.contains(Self::parent(path)) {
            anyhow::bail!("missing remote parent for {path}");
        }
        Ok(RemoteDestinationSnapshot::Missing)
    }

    fn has_transfer_operation_below(&self, prefix: &str) -> bool {
        self.events.iter().any(|event| {
            !event.starts_with("list ")
                && event
                    .split_ascii_whitespace()
                    .skip(1)
                    .any(|path| path.starts_with(prefix))
        })
    }

    fn has_mutation(&self) -> bool {
        self.events.iter().any(|event| {
            matches!(
                event.split_ascii_whitespace().next(),
                Some("upload" | "rename" | "rm" | "mkdir" | "mkdir_strict")
            )
        })
    }
}

impl Remote for ProductionRemote {
    fn list_dir(&mut self, _dir: &str) -> Result<Vec<Entry>> {
        anyhow::bail!("tolerant LIST must not be used")
    }

    fn file_size(&mut self, path: &str) -> Result<u64> {
        self.files
            .get(path)
            .map(|file| file.bytes.len() as u64)
            .ok_or_else(|| anyhow::anyhow!("missing remote file {path}"))
    }

    fn exact_file_presence(&mut self, path: &str) -> Result<ExactFilePresence> {
        Ok(if self.files.contains_key(path) {
            ExactFilePresence::Present
        } else {
            ExactFilePresence::Missing
        })
    }
}

impl StrictRemote for ProductionRemote {
    fn list_dir_strict(&mut self, dir: &str) -> Result<Vec<Entry>> {
        self.events.push(format!("list {dir}"));
        if !self.directories.contains(dir) {
            anyhow::bail!("strict LIST of non-directory {dir}");
        }
        Ok(self.strict_children(dir))
    }
}

impl RemoteFileRetrieval for ProductionRemote {
    fn mtime(&mut self, remote_path: &str) -> Result<DateTime<Utc>> {
        if let Some(message) = &self.mdtm_error {
            anyhow::bail!(message.clone());
        }
        self.events.push(format!("mtime {remote_path}"));
        self.files
            .get(remote_path)
            .map(|file| file.modified)
            .ok_or_else(|| anyhow::anyhow!("missing remote mtime {remote_path}"))
    }

    fn size(&mut self, remote_path: &str) -> Result<u64> {
        self.events.push(format!("size {remote_path}"));
        self.files
            .get(remote_path)
            .map(|file| file.bytes.len() as u64)
            .ok_or_else(|| anyhow::anyhow!("missing remote size {remote_path}"))
    }

    fn download(&mut self, remote_path: &str) -> Result<Vec<u8>> {
        self.events.push(format!("download {remote_path}"));
        if let Some((path, current)) = &self.invalidate_on_download
            && path == remote_path
        {
            current.store(false, Ordering::SeqCst);
        }
        let mutation = self.mutate_on_download.as_mut().and_then(|mutation| {
            if mutation.path != remote_path {
                return None;
            }
            mutation.calls += 1;
            (mutation.calls == mutation.on_call).then(|| mutation.bytes.clone())
        });
        if let Some(bytes) = mutation {
            self.files.insert(
                remote_path.to_string(),
                TestRemoteFile {
                    bytes,
                    modified: test_mtime(41),
                },
            );
        }
        self.files
            .get(remote_path)
            .map(|file| file.bytes.clone())
            .ok_or_else(|| anyhow::anyhow!("missing remote download {remote_path}"))
    }
}

impl RemoteWrite for ProductionRemote {
    fn upload_bytes(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
        self.events.push(format!("upload {path}"));
        if !self.directories.contains(Self::parent(path)) {
            anyhow::bail!("missing remote upload parent for {path}");
        }
        self.files.insert(
            path.to_string(),
            TestRemoteFile {
                bytes: bytes.to_vec(),
                modified: test_mtime(50),
            },
        );
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.events.push(format!("rename {from} {to}"));
        let file = self
            .files
            .remove(from)
            .ok_or_else(|| anyhow::anyhow!("missing remote rename source {from}"))?;
        self.files.insert(to.to_string(), file);
        Ok(())
    }

    fn rm(&mut self, path: &str) -> Result<()> {
        self.events.push(format!("rm {path}"));
        self.files.remove(path);
        Ok(())
    }

    fn mkdir(&mut self, path: &str) -> Result<()> {
        self.events.push(format!("mkdir {path}"));
        self.directories.insert(path.to_string());
        Ok(())
    }

    fn mkdir_scoped_strict(&mut self, path: &str) -> Result<()> {
        self.events.push(format!("mkdir_strict {path}"));
        if let Some(message) = &self.strict_mkdir_error {
            anyhow::bail!(message.clone());
        }
        self.directories.insert(path.to_string());
        Ok(())
    }

    fn mtime(&mut self, path: &str) -> Result<DateTime<Utc>> {
        self.events.push(format!("exact_mtime {path}"));
        self.files
            .get(path)
            .map(|file| file.modified)
            .ok_or_else(|| anyhow::anyhow!("missing exact remote mtime {path}"))
    }

    fn destination_snapshot(
        &mut self,
        _remote_root: &str,
        path: &str,
    ) -> Result<RemoteDestinationSnapshot> {
        self.events.push(format!("snapshot {path}"));
        self.apply_snapshot_mutation(path);
        let result = self.snapshot(path);
        if let Some((expected, on_call, calls, current)) = &mut self.invalidate_on_snapshot
            && path == expected
        {
            *calls += 1;
            if *calls == *on_call {
                current.store(false, Ordering::SeqCst);
            }
        }
        result
    }
}

fn test_mtime(second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, second).unwrap()
}

fn matcher(root: &Path) -> Matcher {
    Matcher::new(&[], root).unwrap()
}

fn run_production(
    remote: &mut ProductionRemote,
    root: &Path,
    state: &mut StateFile,
    scope: SyncScope,
    force: bool,
    mode: ExecutionMode,
    gate: &dyn CommitGate,
) -> Result<SyncOutcome> {
    run_scoped_with(
        remote,
        state,
        root,
        "/remote",
        &matcher(root),
        scope,
        force,
        mode,
        gate,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_production_with_patterns(
    remote: &mut ProductionRemote,
    root: &Path,
    state: &mut StateFile,
    scope: SyncScope,
    force: bool,
    mode: ExecutionMode,
    gate: &dyn CommitGate,
    patterns: &[&str],
) -> Result<SyncOutcome> {
    let patterns = patterns
        .iter()
        .map(|pattern| (*pattern).to_string())
        .collect::<Vec<_>>();
    let matcher = Matcher::new(&patterns, root).unwrap();
    run_scoped_with(
        remote, state, root, "/remote", &matcher, scope, force, mode, gate,
    )
}

#[test]
fn structured_type_conflict_subtree_local_directory_remote_file_keeps_clean_siblings() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("conflict")).unwrap();
    std::fs::write(root.path().join("conflict/local-child.c"), b"local child").unwrap();
    std::fs::write(root.path().join("clean.c"), b"clean local").unwrap();
    std::fs::write(root.path().join("conflict-old.c"), b"near miss").unwrap();
    let mut remote = ProductionRemote::with_root().file("/remote/conflict", b"remote file");
    let mut state = StateFile::default();

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::RootDirectory,
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap();

    assert_eq!(
        outcome.issues,
        vec![SyncIssue::TypeConflict {
            path: "conflict".into(),
            local: EntryKind::Directory,
            remote: EntryKind::File,
        }]
    );
    assert_eq!(
        outcome.events,
        vec![
            SyncEvent {
                path: "clean.c".into(),
                kind: SyncEventKind::Uploaded,
            },
            SyncEvent {
                path: "conflict-old.c".into(),
                kind: SyncEventKind::Uploaded,
            },
        ]
    );
    assert!(state.files.contains_key("clean.c"));
    assert!(state.files.contains_key("conflict-old.c"));
    assert!(!state.files.contains_key("conflict/local-child.c"));
    assert!(!remote.has_transfer_operation_below("/remote/conflict/"));
}

#[test]
fn structured_type_conflict_subtree_local_file_remote_directory_keeps_clean_siblings() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("conflict"), b"local file").unwrap();
    std::fs::write(root.path().join("clean.c"), b"clean local").unwrap();
    std::fs::write(root.path().join("conflict-old.c"), b"near miss").unwrap();
    let mut remote = ProductionRemote::with_root()
        .directory("/remote/conflict")
        .file("/remote/conflict/remote-child.c", b"remote child");
    let mut state = StateFile::default();

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::RootDirectory,
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap();

    assert_eq!(
        outcome.issues,
        vec![SyncIssue::TypeConflict {
            path: "conflict".into(),
            local: EntryKind::File,
            remote: EntryKind::Directory,
        }]
    );
    assert_eq!(
        outcome.events,
        vec![
            SyncEvent {
                path: "clean.c".into(),
                kind: SyncEventKind::Uploaded,
            },
            SyncEvent {
                path: "conflict-old.c".into(),
                kind: SyncEventKind::Uploaded,
            },
        ]
    );
    assert!(state.files.contains_key("clean.c"));
    assert!(state.files.contains_key("conflict-old.c"));
    assert!(!state.files.contains_key("conflict/remote-child.c"));
    assert!(!remote.has_transfer_operation_below("/remote/conflict/"));
}

fn state_record(bytes: &[u8]) -> FileRecord {
    FileRecord {
        sha256: hash_bytes(bytes),
        size: bytes.len() as u64,
        remote_mtime: test_mtime(0),
        last_synced: test_mtime(2),
    }
}

fn stale_cached_state_record(bytes: &[u8]) -> FileRecord {
    FileRecord {
        sha256: hash_bytes(bytes),
        size: bytes.len() as u64,
        remote_mtime: test_mtime(1),
        last_synced: test_mtime(2),
    }
}

#[test]
fn stale_cached_remote_hash_reclassifies_actual_both_changed_as_conflict() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.c"), b"loc").unwrap();
    let mut remote = ProductionRemote::with_root().file("/remote/file.c", b"rem");
    let mut state = StateFile::default();
    state
        .files
        .insert("file.c".into(), stale_cached_state_record(b"old"));
    let original_record = state.files["file.c"].clone();

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("file.c".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap();

    assert!(outcome.events.is_empty());
    assert_eq!(
        outcome.issues,
        vec![SyncIssue::FileConflict {
            path: "file.c".into(),
            state: crate::state::FileState::BothChanged,
        }]
    );
    assert_eq!(remote.files["/remote/file.c"].bytes, b"rem");
    assert!(
        !remote
            .events
            .iter()
            .any(|event| event.starts_with("upload ") || event.starts_with("rename "))
    );
    assert_eq!(
        remote
            .events
            .iter()
            .filter(|event| *event == "snapshot /remote/file.c")
            .count(),
        1
    );
    assert_eq!(state.files["file.c"], original_record);
}

#[test]
fn stale_cached_remote_hash_force_uses_actual_destination_snapshot() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.c"), b"loc").unwrap();
    let mut remote = ProductionRemote::with_root().file("/remote/file.c", b"rem");
    let mut state = StateFile::default();
    state
        .files
        .insert("file.c".into(), stale_cached_state_record(b"old"));

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("file.c".into()),
        true,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap();

    assert_eq!(
        outcome.events,
        vec![SyncEvent {
            path: "file.c".into(),
            kind: SyncEventKind::ForcedRemoteOverwrite,
        }]
    );
    assert!(outcome.issues.is_empty());
    assert_eq!(remote.files["/remote/file.c"].bytes, b"loc");
    assert_eq!(state.files["file.c"].sha256, hash_bytes(b"loc"));
    assert_eq!(
        remote
            .events
            .iter()
            .filter(|event| *event == "snapshot /remote/file.c")
            .count(),
        3
    );
}

#[test]
fn stale_cached_remote_hash_matching_local_becomes_unchanged_without_upload() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.c"), b"new").unwrap();
    let mut remote = ProductionRemote::with_root().file("/remote/file.c", b"new");
    let mut state = StateFile::default();
    state
        .files
        .insert("file.c".into(), stale_cached_state_record(b"old"));
    let original_record = state.files["file.c"].clone();

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("file.c".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap();

    assert_eq!(
        outcome.events,
        vec![SyncEvent {
            path: "file.c".into(),
            kind: SyncEventKind::Unchanged,
        }]
    );
    assert!(outcome.issues.is_empty());
    assert_eq!(remote.files["/remote/file.c"].bytes, b"new");
    assert!(
        !remote
            .events
            .iter()
            .any(|event| event.starts_with("upload ") || event.starts_with("rename "))
    );
    assert_eq!(
        remote
            .events
            .iter()
            .filter(|event| *event == "snapshot /remote/file.c")
            .count(),
        1
    );
    assert_eq!(state.files["file.c"], original_record);
}

fn run_hash_case(
    local: Option<&[u8]>,
    remote_bytes: Option<&[u8]>,
    known: Option<&[u8]>,
) -> SyncOutcome {
    let root = tempfile::tempdir().unwrap();
    if let Some(bytes) = local {
        std::fs::write(root.path().join("file.c"), bytes).unwrap();
    }
    let mut remote = ProductionRemote::with_root();
    if let Some(bytes) = remote_bytes {
        remote = remote.file("/remote/file.c", bytes);
    }
    let mut state = StateFile::default();
    if let Some(bytes) = known {
        state.files.insert("file.c".into(), state_record(bytes));
    }
    run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("file.c".into()),
        false,
        ExecutionMode::DryRun,
        &UnconditionalCommitGate,
    )
    .unwrap()
}

#[test]
fn structured_production_hash_combinations_keep_existing_action_semantics() {
    let cases = [
        (
            Some(b"a".as_slice()),
            Some(b"a".as_slice()),
            Some(b"a".as_slice()),
            SyncEventKind::Unchanged,
        ),
        (
            Some(b"b".as_slice()),
            Some(b"a".as_slice()),
            Some(b"a".as_slice()),
            SyncEventKind::Uploaded,
        ),
        (
            Some(b"a".as_slice()),
            Some(b"b".as_slice()),
            Some(b"a".as_slice()),
            SyncEventKind::Downloaded,
        ),
        (Some(b"a".as_slice()), None, None, SyncEventKind::Uploaded),
        (None, Some(b"a".as_slice()), None, SyncEventKind::Downloaded),
    ];

    for (local, remote, known, expected) in cases {
        let outcome = run_hash_case(local, remote, known);
        assert_eq!(
            outcome.events,
            vec![SyncEvent {
                path: "file.c".into(),
                kind: expected,
            }]
        );
        assert!(outcome.issues.is_empty());
        assert!(!outcome.cancelled);
    }
}

fn conflict_project() -> (tempfile::TempDir, ProductionRemote, StateFile) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("both.c"), b"local").unwrap();
    std::fs::write(root.path().join("untracked.c"), b"same").unwrap();
    let remote = ProductionRemote::with_root()
        .file("/remote/both.c", b"remote")
        .file("/remote/untracked.c", b"same");
    let mut state = StateFile::default();
    state.files.insert("both.c".into(), state_record(b"known"));
    (root, remote, state)
}

#[test]
fn structured_production_conflicts_and_force_use_local_wins() {
    let (root, mut remote, mut state) = conflict_project();
    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::RootDirectory,
        false,
        ExecutionMode::DryRun,
        &UnconditionalCommitGate,
    )
    .unwrap();

    assert_eq!(
        outcome.issues,
        vec![
            SyncIssue::FileConflict {
                path: "both.c".into(),
                state: crate::state::FileState::BothChanged,
            },
            SyncIssue::FileConflict {
                path: "untracked.c".into(),
                state: crate::state::FileState::Untracked,
            },
        ]
    );
    assert!(outcome.events.is_empty());

    let (root, mut remote, mut state) = conflict_project();
    let forced = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::RootDirectory,
        true,
        ExecutionMode::DryRun,
        &UnconditionalCommitGate,
    )
    .unwrap();

    assert_eq!(
        forced.events,
        vec![
            SyncEvent {
                path: "both.c".into(),
                kind: SyncEventKind::ForcedRemoteOverwrite,
            },
            SyncEvent {
                path: "untracked.c".into(),
                kind: SyncEventKind::ForcedRemoteOverwrite,
            },
        ]
    );
    assert!(forced.issues.is_empty());
}

#[test]
fn structured_production_orders_events_and_distinguishes_type_and_stale_issues() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("z.c"), b"local").unwrap();
    std::fs::write(root.path().join("m.c"), b"local").unwrap();
    std::fs::write(root.path().join("type.c"), b"local file").unwrap();
    let mut remote = ProductionRemote::with_root()
        .directory("/remote/type.c")
        .file("/remote/a.c", b"remote")
        .file("/remote/m.c", b"remote");
    let mut state = StateFile::default();
    state.files.insert("m.c".into(), state_record(b"known"));
    state.files.insert("stale.c".into(), state_record(b"stale"));

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::RootDirectory,
        false,
        ExecutionMode::DryRun,
        &UnconditionalCommitGate,
    )
    .unwrap();

    assert_eq!(
        outcome.events,
        vec![
            SyncEvent {
                path: "a.c".into(),
                kind: SyncEventKind::Downloaded,
            },
            SyncEvent {
                path: "stale.c".into(),
                kind: SyncEventKind::SkippedAbsent,
            },
            SyncEvent {
                path: "z.c".into(),
                kind: SyncEventKind::Uploaded,
            },
        ]
    );
    assert_eq!(
        outcome.issues,
        vec![
            SyncIssue::FileConflict {
                path: "m.c".into(),
                state: crate::state::FileState::BothChanged,
            },
            SyncIssue::TypeConflict {
                path: "type.c".into(),
                local: EntryKind::File,
                remote: EntryKind::Directory,
            },
        ]
    );
}

#[test]
fn exact_stale_state_only_selection_skips_without_local_remote_or_state_mutation() {
    let root = tempfile::tempdir().unwrap();
    let mut remote = ProductionRemote::with_root();
    let mut state = StateFile::default();
    state
        .files
        .insert("gone.c".into(), state_record(b"previous bytes"));
    let state_before = serde_json::to_vec(&state).unwrap();

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("gone.c".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap();

    assert_eq!(
        outcome,
        SyncOutcome {
            events: vec![SyncEvent {
                path: "gone.c".into(),
                kind: SyncEventKind::SkippedAbsent,
            }],
            ..SyncOutcome::default()
        }
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    assert!(remote.files.is_empty());
    assert_eq!(remote.directories, BTreeSet::from(["/remote".to_string()]));
    assert!(!remote.has_mutation());
    assert_eq!(serde_json::to_vec(&state).unwrap(), state_before);
}

struct SequenceGate {
    current: Mutex<VecDeque<bool>>,
    commits: AtomicUsize,
}

impl SequenceGate {
    fn new(results: impl IntoIterator<Item = bool>) -> Self {
        Self {
            current: Mutex::new(results.into_iter().collect()),
            commits: AtomicUsize::new(0),
        }
    }
}

impl CommitGate for SequenceGate {
    fn is_current(&self) -> bool {
        self.current.lock().unwrap().pop_front().unwrap_or(false)
    }

    fn commit(&self, mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision> {
        self.commits.fetch_add(1, Ordering::SeqCst);
        mutation()?;
        Ok(CommitDecision::Committed)
    }
}

#[test]
fn structured_production_outer_cancellation_stages_nothing() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.c"), b"local").unwrap();
    let mut remote = ProductionRemote::with_root();
    let mut state = StateFile::default();
    let gate = SequenceGate::new([false]);

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::RootDirectory,
        false,
        ExecutionMode::Apply,
        &gate,
    )
    .unwrap();

    assert!(outcome.cancelled);
    assert!(outcome.events.is_empty());
    assert_eq!(gate.commits.load(Ordering::SeqCst), 0);
    assert!(
        !remote
            .events
            .iter()
            .any(|event| event.starts_with("upload "))
    );
    assert!(state.files.is_empty());
}

#[test]
fn structured_production_upload_boundary_cancellation_stages_nothing() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.c"), b"local").unwrap();
    let mut remote = ProductionRemote::with_root();
    let mut state = StateFile::default();
    let gate = SequenceGate::new([true, false]);

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::RootDirectory,
        false,
        ExecutionMode::Apply,
        &gate,
    )
    .unwrap();

    assert!(outcome.cancelled);
    assert!(outcome.events.is_empty());
    assert_eq!(gate.commits.load(Ordering::SeqCst), 0);
    assert!(
        !remote
            .events
            .iter()
            .any(|event| event.starts_with("upload "))
    );
    assert!(state.files.is_empty());
}

#[test]
fn structured_production_download_boundary_cancellation_stages_nothing() {
    let root = tempfile::tempdir().unwrap();
    let mut remote = ProductionRemote::with_root().file("/remote/file.c", b"remote");
    let mut state = StateFile::default();
    let gate = SequenceGate::new([true, false]);

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::RootDirectory,
        false,
        ExecutionMode::Apply,
        &gate,
    )
    .unwrap();

    assert!(outcome.cancelled);
    assert!(outcome.events.is_empty());
    assert_eq!(gate.commits.load(Ordering::SeqCst), 0);
    assert!(!root.path().join("file.c").exists());
    assert!(state.files.is_empty());
}

#[test]
fn structured_production_between_entries_keeps_the_first_commit_only() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.c"), b"first").unwrap();
    std::fs::write(root.path().join("b.c"), b"second").unwrap();
    let mut remote = ProductionRemote::with_root();
    let mut state = StateFile::default();
    // Complete both descriptors, then permit the first transfer through its
    // final pre-stage poll before cancelling at the second transfer boundary.
    let gate = SequenceGate::new([
        true, true, true, true, true, true, true, true, true, true, true, true, false,
    ]);

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::RootDirectory,
        false,
        ExecutionMode::Apply,
        &gate,
    )
    .unwrap();

    assert!(outcome.cancelled);
    assert_eq!(
        outcome.events,
        vec![SyncEvent {
            path: "a.c".into(),
            kind: SyncEventKind::Uploaded,
        }]
    );
    assert_eq!(gate.commits.load(Ordering::SeqCst), 1);
    assert_eq!(remote.files["/remote/a.c"].bytes, b"first");
    assert!(!remote.files.contains_key("/remote/b.c"));
    assert!(state.files.contains_key("a.c"));
    assert!(!state.files.contains_key("b.c"));
}

struct LiveGate {
    current: Arc<AtomicBool>,
}

impl CommitGate for LiveGate {
    fn is_current(&self) -> bool {
        self.current.load(Ordering::SeqCst)
    }

    fn commit(&self, mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision> {
        if !self.is_current() {
            return Ok(CommitDecision::Cancelled);
        }
        mutation()?;
        Ok(CommitDecision::Committed)
    }
}

#[test]
fn structured_production_final_directory_validation_invalidation_cancels_unchanged_scope() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("shared")).unwrap();
    let current = Arc::new(AtomicBool::new(true));
    let mut remote = ProductionRemote::with_root().directory("/remote/shared");
    remote.invalidate_on_snapshot = Some(("/remote/shared".into(), 2, 0, Arc::clone(&current)));
    let mut state = StateFile::default();
    let gate = LiveGate { current };

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::RootDirectory,
        false,
        ExecutionMode::Apply,
        &gate,
    )
    .unwrap();

    assert!(outcome.cancelled);
    assert!(outcome.events.is_empty());
    assert!(outcome.issues.is_empty());
    assert!(
        remote
            .events
            .iter()
            .any(|event| event == "snapshot /remote/shared")
    );
    assert!(state.files.is_empty());
}

struct MutatingCommitGate {
    mutation: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl MutatingCommitGate {
    fn new(mutation: impl FnOnce() + Send + 'static) -> Self {
        Self {
            mutation: Mutex::new(Some(Box::new(mutation))),
        }
    }
}

impl CommitGate for MutatingCommitGate {
    fn is_current(&self) -> bool {
        true
    }

    fn commit(&self, mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision> {
        if let Some(race) = self.mutation.lock().unwrap().take() {
            race();
        }
        mutation()?;
        Ok(CommitDecision::Committed)
    }
}
struct PostCommitMutatingGate {
    mutation: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl PostCommitMutatingGate {
    fn new(mutation: impl FnOnce() + Send + 'static) -> Self {
        Self {
            mutation: Mutex::new(Some(Box::new(mutation))),
        }
    }
}

impl CommitGate for PostCommitMutatingGate {
    fn is_current(&self) -> bool {
        true
    }

    fn commit(&self, mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision> {
        mutation()?;
        if let Some(race) = self.mutation.lock().unwrap().take() {
            race();
        }
        Ok(CommitDecision::Committed)
    }
}

struct MutatingCurrentGate {
    calls: AtomicUsize,
    on_call: usize,
    mutation: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl MutatingCurrentGate {
    fn new(on_call: usize, mutation: impl FnOnce() + Send + 'static) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            on_call,
            mutation: Mutex::new(Some(Box::new(mutation))),
        }
    }
}

impl CommitGate for MutatingCurrentGate {
    fn is_current(&self) -> bool {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == self.on_call
            && let Some(race) = self.mutation.lock().unwrap().take()
        {
            race();
        }
        true
    }

    fn commit(&self, mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision> {
        mutation()?;
        Ok(CommitDecision::Committed)
    }
}

#[test]
fn directory_race_local_only_source_disappears_before_remote_create() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let raced = source.clone();
    let gate = MutatingCommitGate::new(move || std::fs::remove_dir(raced).unwrap());
    let mut remote = ProductionRemote::with_root();
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("source".into()),
        false,
        ExecutionMode::Apply,
        &gate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("source"));
    assert!(!remote.directories.contains("/remote/source"));
    assert!(!remote.events.iter().any(|event| event.starts_with("mkdir")));
}

#[test]
fn directory_race_local_only_source_becomes_file_before_remote_create() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let raced = source.clone();
    let gate = MutatingCommitGate::new(move || {
        std::fs::remove_dir(&raced).unwrap();
        std::fs::write(raced, b"raced local file").unwrap();
    });
    let mut remote = ProductionRemote::with_root();
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("source".into()),
        false,
        ExecutionMode::Apply,
        &gate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("source"));
    assert_eq!(std::fs::read(source).unwrap(), b"raced local file");
    assert!(!remote.directories.contains("/remote/source"));
}

#[test]
fn directory_race_remote_only_source_disappears_before_local_create() {
    let root = tempfile::tempdir().unwrap();
    let mut remote = ProductionRemote::with_root()
        .directory("/remote/source")
        .mutate_snapshot("/remote/source", 2, SnapshotMutationKind::RemoveDirectory);
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("source".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("source"));
    assert!(!root.path().join("source").exists());
}

#[test]
fn directory_race_remote_only_source_becomes_file_before_local_create() {
    let root = tempfile::tempdir().unwrap();
    let mut remote = ProductionRemote::with_root()
        .directory("/remote/source")
        .mutate_snapshot(
            "/remote/source",
            2,
            SnapshotMutationKind::ReplaceDirectoryWithFile,
        );
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("source".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("source"));
    assert!(!root.path().join("source").exists());
    assert_eq!(remote.files["/remote/source"].bytes, b"raced file");
}

#[test]
fn directory_race_missing_local_destination_appears_before_create() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("source");
    let raced = destination.clone();
    let gate = MutatingCommitGate::new(move || std::fs::create_dir(raced).unwrap());
    let mut remote = ProductionRemote::with_root().directory("/remote/source");
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("source".into()),
        false,
        ExecutionMode::Apply,
        &gate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("source"));
    assert!(destination.is_dir());
    assert!(remote.directories.contains("/remote/source"));
}

#[test]
fn directory_race_missing_remote_destination_appears_before_create() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("source")).unwrap();
    let mut remote = ProductionRemote::with_root().mutate_snapshot(
        "/remote/source",
        2,
        SnapshotMutationKind::AddDirectory,
    );
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("source".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("source"));
    assert!(root.path().join("source").is_dir());
    assert!(remote.directories.contains("/remote/source"));
    assert!(
        !remote
            .events
            .iter()
            .any(|event| event == "mkdir_strict /remote/source")
    );
}

#[test]
fn directory_race_shared_local_directory_changes_type_before_final_validation() {
    let root = tempfile::tempdir().unwrap();
    let shared = root.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    let raced = shared.clone();
    let gate = MutatingCurrentGate::new(2, move || {
        std::fs::remove_dir(&raced).unwrap();
        std::fs::write(raced, b"raced local file").unwrap();
    });
    let mut remote = ProductionRemote::with_root().directory("/remote/shared");
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("shared".into()),
        false,
        ExecutionMode::Apply,
        &gate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("shared"));
    assert_eq!(std::fs::read(shared).unwrap(), b"raced local file");
    assert!(remote.directories.contains("/remote/shared"));
}

#[test]
fn directory_race_shared_remote_directory_changes_type_before_final_validation() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("shared")).unwrap();
    let mut remote = ProductionRemote::with_root()
        .directory("/remote/shared")
        .mutate_snapshot(
            "/remote/shared",
            2,
            SnapshotMutationKind::ReplaceDirectoryWithFile,
        );
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("shared".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("shared"));
    assert!(root.path().join("shared").is_dir());
    assert_eq!(remote.files["/remote/shared"].bytes, b"raced file");
}

#[test]
fn directory_race_unchanged_empty_scope_final_invalidation_cancels() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("empty")).unwrap();
    let current = Arc::new(AtomicBool::new(true));
    let mut remote = ProductionRemote::with_root().directory("/remote/empty");
    remote.invalidate_on_snapshot = Some(("/remote/empty".into(), 2, 0, Arc::clone(&current)));
    let mut state = StateFile::default();

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("empty".into()),
        false,
        ExecutionMode::Apply,
        &LiveGate { current },
    )
    .unwrap();

    assert!(outcome.cancelled);
    assert!(outcome.events.is_empty());
    assert!(root.path().join("empty").is_dir());
    assert!(remote.directories.contains("/remote/empty"));
}

#[test]
fn directory_race_strict_mkd_550_propagates_without_tolerant_fallback() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("source")).unwrap();
    let mut remote = ProductionRemote::with_root().fail_strict_mkdir("550 denied");
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("source".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("550 denied"));
    assert!(root.path().join("source").is_dir());
    assert!(!remote.directories.contains("/remote/source"));
    assert!(
        remote
            .events
            .iter()
            .any(|event| event == "mkdir_strict /remote/source")
    );
    assert!(
        !remote
            .events
            .iter()
            .any(|event| event == "mkdir /remote/source")
    );
}

#[test]
fn directory_race_missing_remote_destination_file_appears_without_overwrite() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("source")).unwrap();
    let mut remote = ProductionRemote::with_root().mutate_snapshot(
        "/remote/source",
        2,
        SnapshotMutationKind::AddFile,
    );
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("source".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("source"));
    assert_eq!(remote.files["/remote/source"].bytes, b"raced file");
    assert!(root.path().join("source").is_dir());
}

#[test]
fn selected_missing_leaf_below_local_directory_prefix_is_not_found_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("area")).unwrap();
    let mut remote = ProductionRemote::with_root();
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("area/missing.c".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("path not found locally or remotely"));
    assert!(root.path().join("area").is_dir());
    assert_eq!(remote.directories, BTreeSet::from(["/remote".to_string()]));
    assert!(!remote.has_mutation());
    assert!(state.files.is_empty());
}

#[test]
fn selected_missing_leaf_below_remote_directory_prefix_is_not_found_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    let mut remote = ProductionRemote::with_root().directory("/remote/area");
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("area/missing.c".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("path not found locally or remotely"));
    assert!(!root.path().join("area").exists());
    assert!(remote.directories.contains("/remote/area"));
    assert!(!remote.has_mutation());
    assert!(state.files.is_empty());
}

#[test]
fn selected_missing_descendant_does_not_upload_its_local_file_ancestor() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("area"), b"local ancestor").unwrap();
    let mut remote = ProductionRemote::with_root();
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("area/missing.c".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("path not found locally or remotely"));
    assert_eq!(
        std::fs::read(root.path().join("area")).unwrap(),
        b"local ancestor"
    );
    assert!(!remote.files.contains_key("/remote/area"));
    assert!(!remote.has_mutation());
    assert!(state.files.is_empty());
}

#[test]
fn selected_missing_descendant_does_not_download_its_remote_file_ancestor() {
    let root = tempfile::tempdir().unwrap();
    let mut remote = ProductionRemote::with_root().file("/remote/area", b"remote ancestor");
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("area/missing.c".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("path not found locally or remotely"));
    assert!(!root.path().join("area").exists());
    assert_eq!(remote.files["/remote/area"].bytes, b"remote ancestor");
    assert!(!remote.has_mutation());
    assert!(state.files.is_empty());
}

#[test]
fn remote_selected_leaf_with_local_file_parent_reports_typed_conflict_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("area"), b"local ancestor").unwrap();
    let mut remote = ProductionRemote::with_root()
        .directory("/remote/area")
        .file("/remote/area/selected.c", b"remote selected");
    let mut state = StateFile::default();

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("area/selected.c".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap();

    assert_eq!(
        outcome.issues,
        vec![SyncIssue::TypeConflict {
            path: "area".into(),
            local: EntryKind::File,
            remote: EntryKind::Directory,
        }]
    );
    assert_eq!(
        std::fs::read(root.path().join("area")).unwrap(),
        b"local ancestor"
    );
    assert_eq!(
        remote.files["/remote/area/selected.c"].bytes,
        b"remote selected"
    );
    assert!(!remote.has_mutation());
    assert!(remote.events.iter().all(|event| event.starts_with("list ")));
    assert!(state.files.is_empty());
}

#[test]
fn local_selected_leaf_with_remote_file_parent_reports_typed_conflict_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("area")).unwrap();
    std::fs::write(root.path().join("area/selected.c"), b"local selected").unwrap();
    let mut remote = ProductionRemote::with_root().file("/remote/area", b"remote ancestor");
    let mut state = StateFile::default();

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("area/selected.c".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap();

    assert_eq!(
        outcome.issues,
        vec![SyncIssue::TypeConflict {
            path: "area".into(),
            local: EntryKind::Directory,
            remote: EntryKind::File,
        }]
    );
    assert_eq!(
        std::fs::read(root.path().join("area/selected.c")).unwrap(),
        b"local selected"
    );
    assert_eq!(remote.files["/remote/area"].bytes, b"remote ancestor");
    assert!(!remote.has_mutation());
    assert!(remote.events.iter().all(|event| event.starts_with("list ")));
    assert!(state.files.is_empty());
}

#[test]
fn ignored_selected_leaf_under_ignored_ancestor_creates_nothing_and_keeps_state() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("ignored")).unwrap();
    std::fs::write(root.path().join("ignored/selected.c"), b"ignored local").unwrap();
    let mut remote = ProductionRemote::with_root();
    let mut state = StateFile::default();
    state.files.insert(
        "ignored/selected.c".into(),
        state_record(b"previous ignored bytes"),
    );
    let before = state.files.clone();

    let error = run_production_with_patterns(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("ignored/selected.c".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
        &["ignored/"],
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("path not found locally or remotely"));
    assert_eq!(
        std::fs::read(root.path().join("ignored/selected.c")).unwrap(),
        b"ignored local"
    );
    assert_eq!(state.files, before);
    assert_eq!(remote.directories, BTreeSet::from(["/remote".to_string()]));
    assert!(!remote.has_mutation());
}

#[test]
fn ignored_directory_near_miss_still_materializes_parent_and_syncs_selected_leaf() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("ignored-old")).unwrap();
    std::fs::write(root.path().join("ignored-old/selected.c"), b"kept local").unwrap();
    let mut remote = ProductionRemote::with_root();
    let mut state = StateFile::default();

    let outcome = run_production_with_patterns(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("ignored-old/selected.c".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
        &["ignored/"],
    )
    .unwrap();

    assert!(outcome.issues.is_empty());
    assert_eq!(
        remote.files["/remote/ignored-old/selected.c"].bytes,
        b"kept local"
    );
    assert!(remote.directories.contains("/remote/ignored-old"));
    assert!(state.files.contains_key("ignored-old/selected.c"));
}
#[test]
fn planning_cancellation_stops_large_retrievals_before_any_mutation() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("pending")).unwrap();
    let large_a = vec![b'a'; 2 * 1024 * 1024];
    let large_b = vec![b'b'; 2 * 1024 * 1024];
    let current = Arc::new(AtomicBool::new(true));
    let mut remote = ProductionRemote::with_root()
        .file("/remote/a.bin", &large_a)
        .file("/remote/b.bin", &large_b);
    remote.invalidate_on_download = Some(("/remote/a.bin".into(), Arc::clone(&current)));
    let mut state = StateFile::default();

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::RootDirectory,
        false,
        ExecutionMode::Apply,
        &LiveGate { current },
    )
    .unwrap();

    assert!(outcome.cancelled);
    assert!(outcome.events.is_empty());
    assert!(!remote.has_mutation());
    assert_eq!(
        remote
            .events
            .iter()
            .filter(|event| event.starts_with("download "))
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["download /remote/a.bin"]
    );
    assert!(!remote.directories.contains("/remote/pending"));
    assert!(state.files.is_empty());
}

#[test]
fn cancellation_before_materialization_does_not_retrieve_or_stage() {
    let root = tempfile::tempdir().unwrap();
    let mut remote = ProductionRemote::with_root().file("/remote/file.c", b"remote");
    let mut state = StateFile::default();
    state
        .files
        .insert("file.c".into(), stale_cached_state_record(b"remote"));
    let gate = SequenceGate::new([true, true, false]);

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("file.c".into()),
        false,
        ExecutionMode::Apply,
        &gate,
    )
    .unwrap();

    assert!(outcome.cancelled);
    assert!(!root.path().join("file.c").exists());
    assert!(
        !remote
            .events
            .iter()
            .any(|event| event.starts_with("download ") || event.starts_with("upload "))
    );
    assert!(state.files.contains_key("file.c"));
}

#[test]
fn local_upload_change_between_preflight_and_directory_phase_fails_safely() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("nested")).unwrap();
    let source = root.path().join("nested/file.c");
    std::fs::write(&source, b"planned").unwrap();
    let raced = source.clone();
    let gate = MutatingCommitGate::new(move || std::fs::write(raced, b"changed").unwrap());
    let mut remote = ProductionRemote::with_root();
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("nested".into()),
        false,
        ExecutionMode::Apply,
        &gate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("local source changed"));
    assert!(remote.directories.contains("/remote/nested"));
    assert!(!remote.files.contains_key("/remote/nested/file.c"));
    assert!(state.files.is_empty());
}

#[test]
fn remote_download_change_between_preflight_and_materialization_fails_safely() {
    let root = tempfile::tempdir().unwrap();
    let mut remote = ProductionRemote::with_root()
        .directory("/remote/nested")
        .file("/remote/nested/file.c", b"planned")
        .mutate_download("/remote/nested/file.c", 2, b"changed");
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("nested".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("remote changed"));
    assert!(root.path().join("nested").is_dir());
    assert!(!root.path().join("nested/file.c").exists());
    assert!(state.files.is_empty());
}

#[test]
fn directory_validation_does_not_revalidate_an_earlier_sibling() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("a")).unwrap();
    std::fs::create_dir(root.path().join("b")).unwrap();
    let mut remote = ProductionRemote::with_root();
    let mut state = StateFile::default();

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::RootDirectory,
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap();

    assert!(!outcome.cancelled);
    assert_eq!(
        remote
            .events
            .iter()
            .filter(|event| *event == "snapshot /remote/a")
            .count(),
        3
    );
    assert!(remote.directories.contains("/remote/a"));
    assert!(remote.directories.contains("/remote/b"));
}

#[test]
fn missing_parent_refresh_rejects_a_file_that_appears_after_directory_creation() {
    let root = tempfile::tempdir().unwrap();
    let local_path = root.path().join("nested/file.c");
    let raced_path = local_path.clone();
    let gate = PostCommitMutatingGate::new(move || {
        std::fs::write(raced_path, b"foreign").unwrap();
    });
    let mut remote = ProductionRemote::with_root()
        .directory("/remote/nested")
        .file("/remote/nested/file.c", b"planned");
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("nested".into()),
        false,
        ExecutionMode::Apply,
        &gate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("local destination changed"));
    assert_eq!(std::fs::read(local_path).unwrap(), b"foreign");
    assert_eq!(
        remote
            .events
            .iter()
            .filter(|event| *event == "download /remote/nested/file.c")
            .count(),
        1
    );
    assert!(state.files.is_empty());
}

#[test]
fn missing_parent_refresh_rejects_a_replaced_created_parent() {
    let root = tempfile::tempdir().unwrap();
    let local_parent = root.path().join("nested");
    let raced_parent = local_parent.clone();
    let replacement = root.path().join("replacement");
    std::fs::create_dir(&replacement).unwrap();
    let gate = PostCommitMutatingGate::new(move || {
        std::fs::remove_dir(&raced_parent).unwrap();
        std::fs::rename(replacement, &raced_parent).unwrap();
    });
    let mut remote = ProductionRemote::with_root()
        .directory("/remote/nested")
        .file("/remote/nested/file.c", b"planned");
    let mut state = StateFile::default();

    let error = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("nested".into()),
        false,
        ExecutionMode::Apply,
        &gate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("local directory changed"));
    assert!(local_parent.is_dir());
    assert!(!local_parent.join("file.c").exists());
    assert_eq!(
        remote
            .events
            .iter()
            .filter(|event| *event == "download /remote/nested/file.c")
            .count(),
        1
    );
    assert!(state.files.is_empty());
}

#[test]
fn fresh_download_falls_back_to_hash_and_size_without_mdtm() {
    let root = tempfile::tempdir().unwrap();
    let mut remote = ProductionRemote::with_root()
        .file("/remote/file.c", b"remote")
        .fail_mdtm("500 MDTM unsupported");
    let mut state = StateFile::default();

    let outcome = run_production(
        &mut remote,
        root.path(),
        &mut state,
        SyncScope::Path("file.c".into()),
        false,
        ExecutionMode::Apply,
        &UnconditionalCommitGate,
    )
    .unwrap();

    assert_eq!(
        outcome.events,
        vec![SyncEvent {
            path: "file.c".into(),
            kind: SyncEventKind::Downloaded,
        }]
    );
    assert_eq!(
        std::fs::read(root.path().join("file.c")).unwrap(),
        b"remote"
    );
    assert_eq!(
        remote
            .events
            .iter()
            .filter(|event| *event == "download /remote/file.c")
            .count(),
        2
    );
    assert_eq!(state.files["file.c"].sha256, hash_bytes(b"remote"));
}
