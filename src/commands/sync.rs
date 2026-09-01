//! `ferry sync` — bidirectional reconciliation in a single pass.
//!
//! Sync walks the union of local + remote + state and applies the design's
//! action matrix per file:
//!
//! | State          | Action                                                    |
//! |----------------|-----------------------------------------------------------|
//! | InSync         | noop                                                      |
//! | LocalChanged   | upload                                                    |
//! | RemoteChanged  | download                                                  |
//! | LocalOnly      | upload (new remote file)                                  |
//! | RemoteOnly     | download (new local file)                                 |
//! | BothChanged    | refuse without --force; with --force, local wins (upload) |
//! | Untracked      | refuse without --force; with --force, local wins (upload) |
//!
//! "Local wins on --force" matches the documented sync semantics: when the
//! user says "just sync it already," last-write-wins from the local side
//! since that's the side the user is actively editing.

pub use self::commit::{CommitDecision, CommitGate, UnconditionalCommitGate};
use self::scope::SyncScope;
use crate::commands::file_transfer::{RemoteDestinationSnapshot, RemoteWrite};
use crate::commands::pull::{ExpectedLocalDestination, download_one, download_one_guarded};
use crate::commands::push::{
    ExpectedLocalSource, ExpectedRemoteDestination, upload_one, upload_one_guarded,
};
use crate::commands::remote_hash::{self, RemoteFileRetrieval, RemoteHash};
use crate::commands::walk::{remote_join, walk_local, walk_remote_with_symlinks};
use crate::commands::{ExecutionMode, state_path_for};
use crate::config::Config;
use crate::ftp::{Entry, Ftp, Remote, StrictRemote};
use crate::hash::{hash_bytes, hash_file};
use crate::ignored::Matcher;
use crate::state::{FileState, StateFile, classify};
use anyhow::{Context, Result};
use same_file::Handle;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

// The scoped sync engine wires this collector in Task 5.
pub(crate) mod commit;
mod inventory;
mod picker;
#[cfg(test)]
mod production_tests;
pub use inventory::EntryKind;
pub mod scope;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncEventKind {
    Unchanged,
    Uploaded,
    Downloaded,
    CreatedLocalDirectory,
    CreatedRemoteDirectory,
    SkippedAbsent,
    ForcedRemoteOverwrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncEvent {
    pub path: String,
    pub kind: SyncEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncIssue {
    FileConflict {
        path: String,
        state: FileState,
    },
    TypeConflict {
        path: String,
        local: EntryKind,
        remote: EntryKind,
    },
}

impl SyncIssue {
    fn path(&self) -> &str {
        match self {
            Self::FileConflict { path, .. } | Self::TypeConflict { path, .. } => path,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncOutcome {
    pub events: Vec<SyncEvent>,
    pub issues: Vec<SyncIssue>,
    pub cancelled: bool,
}

fn is_at_or_below_conflict(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn type_conflict_prefixes(entries: &BTreeMap<String, inventory::InventoryEntry>) -> Vec<String> {
    let mut prefixes: Vec<String> = Vec::new();
    for (path, entry) in entries {
        let is_conflict = matches!(
            (entry.local, entry.remote),
            (Some(local), Some(remote)) if local != remote
        );
        if is_conflict
            && !prefixes
                .iter()
                .any(|prefix| is_at_or_below_conflict(path, prefix))
        {
            prefixes.push(path.clone());
        }
    }
    prefixes
}

fn type_conflict_issues(
    entries: &BTreeMap<String, inventory::InventoryEntry>,
    prefixes: &[String],
) -> Vec<SyncIssue> {
    prefixes
        .iter()
        .map(|prefix| {
            let entry = entries
                .get(prefix)
                .expect("type-conflict prefix came from inventory");
            let (Some(local), Some(remote)) = (entry.local, entry.remote) else {
                unreachable!("type conflicts require entries on both sides");
            };
            SyncIssue::TypeConflict {
                path: prefix.clone(),
                local,
                remote,
            }
        })
        .collect()
}

fn classify_inventory_shapes(
    entries: BTreeMap<String, inventory::InventoryEntry>,
) -> (Vec<String>, SyncOutcome) {
    let conflict_prefixes = type_conflict_prefixes(&entries);
    let mut files = Vec::new();
    let mut outcome = SyncOutcome {
        issues: type_conflict_issues(&entries, &conflict_prefixes),
        ..SyncOutcome::default()
    };

    for (path, entry) in entries {
        if conflict_prefixes
            .iter()
            .any(|prefix| is_at_or_below_conflict(&path, prefix))
        {
            continue;
        }
        match (entry.local, entry.remote) {
            (None, None) if entry.in_state => outcome.events.push(SyncEvent {
                path,
                kind: SyncEventKind::SkippedAbsent,
            }),
            (Some(EntryKind::File), Some(EntryKind::File))
            | (Some(EntryKind::File), None)
            | (None, Some(EntryKind::File)) => files.push(path),
            (Some(EntryKind::Directory), Some(EntryKind::Directory))
            | (Some(EntryKind::Directory), None)
            | (None, Some(EntryKind::Directory))
            | (None, None) => {}
            (Some(_), Some(_)) => {
                unreachable!("all type conflicts were suppressed by their prefix")
            }
        }
    }
    (files, outcome)
}

#[derive(Debug)]
struct ScheduledAction {
    path: String,
    kind: SyncEventKind,
}

fn execute_structured_plan(
    actions: Vec<ScheduledAction>,
    mut outcome: SyncOutcome,
    gate: &dyn CommitGate,
    mut execute: impl FnMut(&ScheduledAction) -> Result<CommitDecision>,
) -> Result<SyncOutcome> {
    for action in &actions {
        if !gate.is_current() {
            outcome.cancelled = true;
            return Ok(outcome);
        }
        match execute(action)? {
            CommitDecision::Committed => outcome.events.push(SyncEvent {
                path: action.path.clone(),
                kind: action.kind.clone(),
            }),
            CommitDecision::Cancelled => {
                outcome.cancelled = true;
                return Ok(outcome);
            }
        }
    }

    Ok(outcome)
}

#[derive(Debug)]
enum ExpectedLocalDirectory {
    Root(LocalDirectoryIdentity),
    Missing,
    Directory(LocalDirectoryIdentity),
}

#[derive(Debug)]
struct ExpectedDirectorySnapshots {
    relative: String,
    local: ExpectedLocalDirectory,
    remote: ExpectedRemoteDirectory,
}

#[derive(Debug)]
struct LocalDirectoryIdentity {
    canonical: PathBuf,
    identity: Handle,
    is_symlink: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpectedRemoteDirectory {
    Root,
    Missing,
    Directory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectoryAction {
    CreateLocal,
    CreateRemote,
}

#[derive(Debug)]
struct AuthoritativeUploadCandidate {
    state: FileState,
    destination: ExpectedRemoteDestination,
}

#[derive(Debug)]
struct PlannedTransfer {
    event: ScheduledAction,
    operation: PlannedOperation,
}

#[derive(Debug)]
enum PlannedOperation {
    Upload(Box<PlannedUpload>),
    Download(Box<PlannedDownload>),
    Preview,
}

#[derive(Debug)]
struct PlannedUpload {
    remote_path: String,
    hash: String,
    source: ExpectedLocalSource,
    destination: ExpectedRemoteDestination,
}

#[derive(Debug)]
struct PlannedDownload {
    local_path: PathBuf,
    remote_path: String,
    expected_remote: PlannedRemoteSource,
    expected_local_hash: Option<String>,
    destination: Option<ExpectedLocalDestination>,
    existing_local_source: Option<ExpectedLocalSource>,
}

#[derive(Debug)]
struct PlannedRemoteSource {
    sha256: String,
    size: u64,
    stable_mtime: Option<chrono::DateTime<chrono::Utc>>,
}

impl PlannedRemoteSource {
    fn capture(hash: &RemoteHash) -> Self {
        Self {
            sha256: hash.sha256.clone(),
            size: hash.size,
            stable_mtime: hash.metadata_stable.then_some(hash.mtime),
        }
    }

    fn matches(&self, hash: &RemoteHash) -> bool {
        self.sha256 == hash.sha256
            && self.size == hash.size
            && match self.stable_mtime {
                Some(mtime) => hash.metadata_stable && mtime == hash.mtime,
                None => true,
            }
    }
}

/// Scoped sync transport adapter.
///
/// The legacy route intentionally keeps its historical `Ftp` implementations.
/// Scoped sync instead delegates every reachable FTP operation to a strict,
/// source-dropping method so hostile server replies cannot become user output.
struct ScopedFtp<'a> {
    inner: &'a mut Ftp,
}

impl Remote for ScopedFtp<'_> {
    fn list_dir(&mut self, dir: &str) -> Result<Vec<Entry>> {
        self.inner.list_strict(dir)
    }

    fn file_size(&mut self, path: &str) -> Result<u64> {
        self.inner.size_scoped(path)
    }
}

impl StrictRemote for ScopedFtp<'_> {
    fn list_dir_strict(&mut self, dir: &str) -> Result<Vec<Entry>> {
        self.inner.list_strict(dir)
    }
}

impl RemoteFileRetrieval for ScopedFtp<'_> {
    fn mtime(&mut self, remote_path: &str) -> Result<chrono::DateTime<chrono::Utc>> {
        self.inner.mtime_scoped(remote_path)
    }

    fn size(&mut self, remote_path: &str) -> Result<u64> {
        self.inner.size_scoped(remote_path)
    }

    fn download(&mut self, remote_path: &str) -> Result<Vec<u8>> {
        self.inner.download_scoped(remote_path)
    }
}

impl RemoteWrite for ScopedFtp<'_> {
    fn upload_bytes(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
        self.inner.upload_bytes_scoped(path, bytes)
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.inner.rename_scoped(from, to)
    }

    fn rm(&mut self, path: &str) -> Result<()> {
        self.inner.rm_scoped(path)
    }

    fn mkdir(&mut self, path: &str) -> Result<()> {
        self.inner.mkdir_scoped_strict(path)
    }

    fn mkdir_scoped_strict(&mut self, path: &str) -> Result<()> {
        self.inner.mkdir_scoped_strict(path)
    }

    fn mtime(&mut self, path: &str) -> Result<chrono::DateTime<chrono::Utc>> {
        self.inner.mtime_scoped(path)
    }

    fn destination_snapshot(
        &mut self,
        remote_root: &str,
        path: &str,
    ) -> Result<RemoteDestinationSnapshot> {
        <Ftp as RemoteWrite>::destination_snapshot(self.inner, remote_root, path)
    }
}

pub fn run_scoped(
    config_path: &Path,
    scope: SyncScope,
    force: bool,
    mode: ExecutionMode,
    gate: &dyn CommitGate,
) -> Result<SyncOutcome> {
    if scope == SyncScope::LegacyProject {
        anyhow::bail!("scoped sync requires an explicit path");
    }

    let config = Config::load(config_path)?;
    run_scoped_from_config(&config, scope, force, mode, gate)
}

pub(crate) fn run_scoped_from_config(
    config: &Config,
    scope: SyncScope,
    force: bool,
    mode: ExecutionMode,
    gate: &dyn CommitGate,
) -> Result<SyncOutcome> {
    if scope == SyncScope::LegacyProject {
        anyhow::bail!("scoped sync requires an explicit path");
    }

    let local_root = config.paths.local_root.clone();
    let state_path = state_path_for(&local_root, mode);
    let mut state = StateFile::load_or_default(&state_path)?;
    let initial_files = state.files.clone();
    let matcher = Matcher::new(&config.sync.ignore, &local_root)?;
    let mut ftp = Ftp::connect(
        &config.connection.host,
        config.connection.port,
        &config.connection.user,
        &config.connection.password,
        config.connection.passive,
    )?;
    let mut remote = ScopedFtp { inner: &mut ftp };

    let execution = run_scoped_with(
        &mut remote,
        &mut state,
        &local_root,
        &config.paths.remote_root,
        &matcher,
        scope,
        force,
        mode,
        gate,
    );
    let should_save = state.files != initial_files;
    let save = if mode.should_apply() && should_save {
        state.save(&state_path)
    } else {
        Ok(())
    };

    match (execution, save) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(save_error)) => Err(error.context(format!(
            "also failed to save completed sync state: {save_error:#}"
        ))),
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_scoped_with_for_test<R>(
    remote: &mut R,
    state: &mut StateFile,
    local_root: &Path,
    remote_root: &str,
    matcher: &Matcher,
    scope: SyncScope,
    force: bool,
    mode: ExecutionMode,
    gate: &dyn CommitGate,
) -> Result<SyncOutcome>
where
    R: StrictRemote + RemoteFileRetrieval + RemoteWrite,
{
    run_scoped_with(
        remote,
        state,
        local_root,
        remote_root,
        matcher,
        scope,
        force,
        mode,
        gate,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_scoped_with<R>(
    remote: &mut R,
    state: &mut StateFile,
    local_root: &Path,
    remote_root: &str,
    matcher: &Matcher,
    scope: SyncScope,
    force: bool,
    mode: ExecutionMode,
    gate: &dyn CommitGate,
) -> Result<SyncOutcome>
where
    R: StrictRemote + RemoteFileRetrieval + RemoteWrite,
{
    let inventory = inventory::collect(remote, local_root, remote_root, matcher, state, scope)?;
    let ancestor_conflict_prefixes = type_conflict_prefixes(&inventory.implicit_ancestors);
    let ancestor_conflicts =
        type_conflict_issues(&inventory.implicit_ancestors, &ancestor_conflict_prefixes);
    let mut entries = inventory.entries;
    entries.retain(|path, _| {
        !ancestor_conflict_prefixes
            .iter()
            .any(|prefix| is_at_or_below_conflict(path, prefix))
    });
    let (file_paths, mut outcome) = classify_inventory_shapes(entries.clone());
    outcome.issues.extend(ancestor_conflicts);
    let mut directories = capture_directory_snapshots(remote, local_root, remote_root, &entries)?;

    let mut transfers = Vec::new();
    for relative in file_paths {
        if !gate.is_current() {
            return Ok(cancelled_sync_outcome(outcome));
        }
        let entry = entries
            .get(&relative)
            .expect("file path came from scoped inventory");
        let local_path = local_root.join(&relative);
        let remote_path = remote_join(remote_root, &relative);
        let mut local_source = if entry.local == Some(EntryKind::File) {
            Some(ExpectedLocalSource::capture(local_root, &local_path)?)
        } else {
            None
        };
        if local_source.is_some() && !gate.is_current() {
            return Ok(cancelled_sync_outcome(outcome));
        }
        let local_hash = if let Some(source) = &local_source {
            let hash = hash_file(&source.path)
                .with_context(|| format!("hashing local {}", source.path.display()))?;
            source.verify_unchanged(&hash)?;
            Some(hash)
        } else {
            None
        };
        if local_source.is_some() && !gate.is_current() {
            return Ok(cancelled_sync_outcome(outcome));
        }
        let remote_hash = if entry.remote == Some(EntryKind::File) {
            Some(remote_hash::compute_with(
                remote,
                state,
                &relative,
                &remote_path,
                false,
            )?)
        } else {
            None
        };
        if remote_hash.is_some() && !gate.is_current() {
            return Ok(cancelled_sync_outcome(outcome));
        }
        let known = state
            .files
            .get(&relative)
            .map(|record| record.sha256.as_str());
        let preliminary_state = classify(
            local_hash.as_deref(),
            remote_hash.as_ref().map(|hash| hash.sha256.as_str()),
            known,
        );
        let mut upload_candidate = if matches!(
            preliminary_state,
            FileState::LocalChanged | FileState::LocalOnly
        ) || force
            && matches!(
                preliminary_state,
                FileState::BothChanged | FileState::Untracked
            ) {
            Some(capture_authoritative_upload_candidate(
                capture_remote_file_destination(
                    remote,
                    remote_root,
                    &remote_path,
                    &relative,
                    &directories,
                )?,
                &relative,
                entry.remote,
                local_hash
                    .as_deref()
                    .expect("upload candidate has a local hash"),
                known,
            )?)
        } else {
            None
        };
        if upload_candidate.is_some() && !gate.is_current() {
            return Ok(cancelled_sync_outcome(outcome));
        }
        let file_state = upload_candidate
            .as_ref()
            .map_or(preliminary_state, |candidate| candidate.state);

        match file_state {
            FileState::InSync => outcome.events.push(SyncEvent {
                path: relative,
                kind: SyncEventKind::Unchanged,
            }),
            FileState::LocalChanged | FileState::LocalOnly => {
                let destination = upload_candidate
                    .take()
                    .expect("final upload state has an authoritative candidate")
                    .destination;
                if mode.is_dry_run() {
                    transfers.push(preview_transfer(relative, SyncEventKind::Uploaded));
                } else {
                    transfers.push(plan_upload(
                        relative,
                        remote_path,
                        local_hash.expect("upload state has a local hash"),
                        local_source
                            .take()
                            .expect("upload state has a captured local source"),
                        destination,
                        SyncEventKind::Uploaded,
                    ));
                }
            }
            FileState::RemoteChanged | FileState::RemoteOnly => {
                if mode.is_dry_run() {
                    transfers.push(preview_transfer(relative, SyncEventKind::Downloaded));
                } else {
                    transfers.push(plan_download(
                        local_root,
                        local_hash.as_deref(),
                        relative,
                        remote_path,
                        remote_hash.expect("download state has a remote hash"),
                        &directories,
                        local_source.take(),
                    )?);
                }
            }
            FileState::BothChanged | FileState::Untracked if force => {
                let destination = upload_candidate
                    .take()
                    .expect("final forced upload state has an authoritative candidate")
                    .destination;
                if mode.is_dry_run() {
                    transfers.push(preview_transfer(
                        relative,
                        SyncEventKind::ForcedRemoteOverwrite,
                    ));
                } else {
                    transfers.push(plan_upload(
                        relative,
                        remote_path,
                        local_hash.expect("forced upload has a local hash"),
                        local_source
                            .take()
                            .expect("forced upload has a captured local source"),
                        destination,
                        SyncEventKind::ForcedRemoteOverwrite,
                    ));
                }
            }
            FileState::BothChanged | FileState::Untracked => {
                outcome.issues.push(SyncIssue::FileConflict {
                    path: relative,
                    state: file_state,
                });
            }
        }
    }

    if execute_directory_plan(
        remote,
        local_root,
        remote_root,
        &mut directories,
        &mut outcome,
        mode,
        gate,
    )? {
        return Ok(cancelled_sync_outcome(outcome));
    }
    if finalize_download_destinations(local_root, &directories, &mut transfers, gate)? {
        return Ok(cancelled_sync_outcome(outcome));
    }

    let scheduled = transfers
        .iter()
        .map(|transfer| ScheduledAction {
            path: transfer.event.path.clone(),
            kind: transfer.event.kind.clone(),
        })
        .collect();
    let mut transfers = transfers.into_iter();
    outcome = execute_structured_plan(scheduled, outcome, gate, |_action| {
        let transfer = transfers
            .next()
            .expect("scheduled action has a planned transfer");
        match transfer.operation {
            PlannedOperation::Upload(plan) => materialize_upload(
                remote,
                state,
                &transfer.event.path,
                remote_root,
                &plan,
                mode,
                gate,
            ),
            PlannedOperation::Download(plan) => {
                materialize_download(remote, state, &transfer.event.path, &plan, mode, gate)
            }
            PlannedOperation::Preview => Ok(CommitDecision::Committed),
        }
    })?;
    if outcome.cancelled {
        sort_outcome(&mut outcome);
        return Ok(outcome);
    }

    if !gate.is_current() {
        outcome.cancelled = true;
        sort_outcome(&mut outcome);
        return Ok(outcome);
    }
    for (index, _expected) in directories.iter().enumerate() {
        if !gate.is_current() {
            outcome.cancelled = true;
            sort_outcome(&mut outcome);
            return Ok(outcome);
        }
        validate_directory_snapshot(remote, local_root, remote_root, &directories, index)?;
    }
    if !gate.is_current() {
        outcome.cancelled = true;
    }
    sort_outcome(&mut outcome);
    Ok(outcome)
}

fn cancelled_sync_outcome(mut outcome: SyncOutcome) -> SyncOutcome {
    outcome.cancelled = true;
    sort_outcome(&mut outcome);
    outcome
}

fn preview_transfer(relative: String, kind: SyncEventKind) -> PlannedTransfer {
    PlannedTransfer {
        event: ScheduledAction {
            path: relative,
            kind,
        },
        operation: PlannedOperation::Preview,
    }
}

fn plan_upload(
    relative: String,
    remote_path: String,
    local_hash: String,
    source: ExpectedLocalSource,
    destination: ExpectedRemoteDestination,
    kind: SyncEventKind,
) -> PlannedTransfer {
    PlannedTransfer {
        event: ScheduledAction {
            path: relative,
            kind,
        },
        operation: PlannedOperation::Upload(Box::new(PlannedUpload {
            remote_path,
            hash: local_hash,
            source,
            destination,
        })),
    }
}

fn capture_authoritative_upload_candidate(
    snapshot: RemoteDestinationSnapshot,
    relative: &str,
    inventory_remote: Option<EntryKind>,
    local_hash: &str,
    known: Option<&str>,
) -> Result<AuthoritativeUploadCandidate> {
    let state = match (inventory_remote, &snapshot) {
        (
            Some(EntryKind::File),
            RemoteDestinationSnapshot::File {
                sha256,
                size: _,
                modified: _,
            },
        ) => classify(Some(local_hash), Some(sha256), known),
        (Some(EntryKind::File), RemoteDestinationSnapshot::Missing) => {
            anyhow::bail!("remote file disappeared while planning {relative}")
        }
        (Some(EntryKind::File), RemoteDestinationSnapshot::Directory) => {
            anyhow::bail!("remote file became a directory while planning {relative}")
        }
        (None, RemoteDestinationSnapshot::Missing) => classify(Some(local_hash), None, known),
        (None, RemoteDestinationSnapshot::File { .. }) => {
            anyhow::bail!("remote file appeared while planning {relative}")
        }
        (None, RemoteDestinationSnapshot::Directory) => {
            anyhow::bail!("remote directory appeared while planning {relative}")
        }
        (Some(EntryKind::Directory), _) => {
            anyhow::bail!("remote directory is not an upload candidate at {relative}")
        }
    };
    Ok(AuthoritativeUploadCandidate {
        state,
        destination: ExpectedRemoteDestination { snapshot },
    })
}

fn capture_remote_file_destination<R: RemoteWrite>(
    remote: &mut R,
    remote_root: &str,
    remote_path: &str,
    relative: &str,
    directories: &[ExpectedDirectorySnapshots],
) -> Result<RemoteDestinationSnapshot> {
    if directories.iter().any(|directory| {
        directory.remote == ExpectedRemoteDirectory::Missing
            && is_strict_directory_ancestor(&directory.relative, relative)
    }) {
        return Ok(RemoteDestinationSnapshot::Missing);
    }
    remote
        .destination_snapshot(remote_root, remote_path)
        .with_context(|| format!("capturing remote destination for {relative}"))
}

fn plan_download(
    local_root: &Path,
    expected_local_hash: Option<&str>,
    relative: String,
    remote_path: String,
    remote_hash: RemoteHash,
    directories: &[ExpectedDirectorySnapshots],
    existing_local_source: Option<ExpectedLocalSource>,
) -> Result<PlannedTransfer> {
    let local_path = local_root.join(&relative);
    verify_planned_local_hash(&local_path, expected_local_hash)?;
    if let (Some(source), Some(hash)) = (&existing_local_source, expected_local_hash) {
        source.verify_unchanged(hash)?;
    }
    let parent_will_be_created = directories.iter().any(|directory| {
        matches!(directory.local, ExpectedLocalDirectory::Missing)
            && is_strict_directory_ancestor(&directory.relative, &relative)
    });
    let destination = if parent_will_be_created {
        None
    } else {
        Some(ExpectedLocalDestination::capture(local_root, &local_path)?)
    };
    if let (Some(source), Some(hash)) = (&existing_local_source, expected_local_hash) {
        source.verify_unchanged(hash)?;
    }
    Ok(PlannedTransfer {
        event: ScheduledAction {
            path: relative,
            kind: SyncEventKind::Downloaded,
        },
        operation: PlannedOperation::Download(Box::new(PlannedDownload {
            local_path,
            remote_path,
            expected_remote: PlannedRemoteSource::capture(&remote_hash),
            expected_local_hash: expected_local_hash.map(str::to_owned),
            destination,
            existing_local_source,
        })),
    })
}

fn finalize_download_destinations(
    local_root: &Path,
    directories: &[ExpectedDirectorySnapshots],
    transfers: &mut [PlannedTransfer],
    gate: &dyn CommitGate,
) -> Result<bool> {
    for transfer in transfers {
        let PlannedOperation::Download(plan) = &mut transfer.operation else {
            continue;
        };
        if plan.destination.is_some() {
            continue;
        }
        if !gate.is_current() {
            return Ok(true);
        }
        validate_local_directory_ancestors(local_root, directories, &transfer.event.path)?;
        verify_planned_local_hash(&plan.local_path, plan.expected_local_hash.as_deref())?;
        if let (Some(source), Some(hash)) = (
            plan.existing_local_source.as_ref(),
            plan.expected_local_hash.as_deref(),
        ) {
            source.verify_unchanged(hash)?;
        }
        if !gate.is_current() {
            return Ok(true);
        }

        let destination = ExpectedLocalDestination::capture(local_root, &plan.local_path)?;
        validate_local_directory_ancestors(local_root, directories, &transfer.event.path)?;
        verify_planned_local_hash(&plan.local_path, plan.expected_local_hash.as_deref())?;
        if let (Some(source), Some(hash)) = (
            plan.existing_local_source.as_ref(),
            plan.expected_local_hash.as_deref(),
        ) {
            source.verify_unchanged(hash)?;
        }
        if !gate.is_current() {
            return Ok(true);
        }
        plan.destination = Some(destination);
    }
    Ok(false)
}

fn validate_local_directory_ancestors(
    local_root: &Path,
    directories: &[ExpectedDirectorySnapshots],
    relative: &str,
) -> Result<()> {
    validate_local_directory(local_root, &directories[0])?;
    for directory in &directories[1..] {
        if is_strict_directory_ancestor(&directory.relative, relative) {
            validate_local_directory(local_root, directory)?;
        }
    }
    Ok(())
}

fn materialize_upload<R: RemoteWrite>(
    remote: &mut R,
    state: &mut StateFile,
    relative: &str,
    remote_root: &str,
    plan: &PlannedUpload,
    mode: ExecutionMode,
    gate: &dyn CommitGate,
) -> Result<CommitDecision> {
    plan.source.verify_unchanged(&plan.hash)?;
    if !gate.is_current() {
        return Ok(CommitDecision::Cancelled);
    }
    let bytes = std::fs::read(&plan.source.path)
        .with_context(|| format!("reading local {}", plan.source.path.display()))?;
    if hash_bytes(&bytes) != plan.hash {
        anyhow::bail!("local source changed while materializing {relative}");
    }
    plan.source.verify_unchanged(&plan.hash)?;
    if !gate.is_current() {
        return Ok(CommitDecision::Cancelled);
    }
    upload_one_guarded(
        remote,
        state,
        relative,
        remote_root,
        &plan.remote_path,
        &bytes,
        &plan.hash,
        &plan.source,
        &plan.destination,
        mode,
        gate,
    )
}

fn materialize_download<R: RemoteFileRetrieval>(
    remote: &mut R,
    state: &mut StateFile,
    relative: &str,
    plan: &PlannedDownload,
    mode: ExecutionMode,
    gate: &dyn CommitGate,
) -> Result<CommitDecision> {
    verify_planned_local_hash(&plan.local_path, plan.expected_local_hash.as_deref())?;
    if let (Some(source), Some(hash)) = (
        plan.existing_local_source.as_ref(),
        plan.expected_local_hash.as_deref(),
    ) {
        source.verify_unchanged(hash)?;
    }
    if !gate.is_current() {
        return Ok(CommitDecision::Cancelled);
    }

    let remote_hash = remote_hash::retrieve_fresh(remote, &plan.remote_path)?;
    if !plan.expected_remote.matches(&remote_hash) {
        anyhow::bail!("remote source changed while materializing {relative}");
    }
    if !gate.is_current() {
        return Ok(CommitDecision::Cancelled);
    }
    download_one_guarded(
        state,
        &plan.local_path,
        relative,
        &remote_hash,
        plan.destination
            .as_ref()
            .expect("download destination was finalized before materialization"),
        mode,
        gate,
    )
}

fn verify_planned_local_hash(path: &Path, expected: Option<&str>) -> Result<()> {
    let current = match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading local destination {}", path.display()));
        }
        Ok(_) => {
            let metadata = std::fs::metadata(path)
                .with_context(|| format!("reading local destination {}", path.display()))?;
            if !metadata.is_file() {
                anyhow::bail!("local destination changed at {}", path.display());
            }
            Some(
                hash_file(path)
                    .with_context(|| format!("hashing local destination {}", path.display()))?,
            )
        }
    };
    if current.as_deref() != expected {
        anyhow::bail!(
            "local destination changed while planning {}",
            path.display()
        );
    }
    Ok(())
}

fn capture_directory_snapshots<R: StrictRemote + RemoteWrite>(
    remote: &mut R,
    local_root: &Path,
    remote_root: &str,
    entries: &BTreeMap<String, inventory::InventoryEntry>,
) -> Result<Vec<ExpectedDirectorySnapshots>> {
    let canonical_root = local_root
        .canonicalize()
        .with_context(|| format!("canonicalizing local_root {}", local_root.display()))?;
    let conflict_prefixes = type_conflict_prefixes(entries);
    let mut paths = BTreeSet::from([String::new()]);
    for (relative, entry) in entries {
        if conflict_prefixes
            .iter()
            .any(|prefix| is_at_or_below_conflict(relative, prefix))
        {
            continue;
        }
        if matches!(entry.local, Some(EntryKind::Directory))
            || matches!(entry.remote, Some(EntryKind::Directory))
        {
            paths.insert(relative.clone());
        }
        if entry.local.is_some() || entry.remote.is_some() {
            let mut parent = relative.rsplit_once('/').map(|(parent, _)| parent);
            while let Some(relative_parent) = parent {
                if !relative_parent.is_empty()
                    && !conflict_prefixes
                        .iter()
                        .any(|prefix| is_at_or_below_conflict(relative_parent, prefix))
                {
                    paths.insert(relative_parent.to_string());
                }
                parent = relative_parent.rsplit_once('/').map(|(next, _)| next);
            }
        }
    }
    let mut paths = paths.into_iter().collect::<Vec<_>>();
    paths.sort_by(|left, right| {
        directory_depth(left)
            .cmp(&directory_depth(right))
            .then_with(|| left.cmp(right))
    });

    let mut snapshots = Vec::new();
    for relative in paths {
        let local_identity =
            capture_local_directory_identity(local_root, &canonical_root, &relative)?;
        let local = if relative.is_empty() {
            ExpectedLocalDirectory::Root(local_identity.ok_or_else(|| {
                anyhow::anyhow!("local root disappeared while planning directories")
            })?)
        } else if let Some(identity) = local_identity {
            ExpectedLocalDirectory::Directory(identity)
        } else {
            ExpectedLocalDirectory::Missing
        };
        if let Some(entry) = entries.get(&relative) {
            match (entry.local, &local) {
                (Some(EntryKind::Directory), ExpectedLocalDirectory::Directory(_))
                | (None, ExpectedLocalDirectory::Missing) => {}
                (Some(EntryKind::File), _) => {
                    anyhow::bail!("local file became a directory while planning {relative}")
                }
                _ => anyhow::bail!("local directory changed while planning {relative}"),
            }
        }

        let remote = if relative.is_empty() {
            remote
                .list_dir_strict(remote_root)
                .with_context(|| format!("validating remote root {remote_root:?}"))?;
            ExpectedRemoteDirectory::Root
        } else if snapshots
            .iter()
            .any(|ancestor: &ExpectedDirectorySnapshots| {
                ancestor.remote == ExpectedRemoteDirectory::Missing
                    && !ancestor.relative.is_empty()
                    && relative
                        .strip_prefix(&ancestor.relative)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            })
        {
            ExpectedRemoteDirectory::Missing
        } else {
            let remote_path = remote_join(remote_root, &relative);
            match remote.destination_snapshot(remote_root, &remote_path)? {
                RemoteDestinationSnapshot::Missing => ExpectedRemoteDirectory::Missing,
                RemoteDestinationSnapshot::Directory => ExpectedRemoteDirectory::Directory,
                RemoteDestinationSnapshot::File { .. } => {
                    anyhow::bail!("remote file appeared while planning directory {relative}")
                }
            }
        };
        if let Some(entry) = entries.get(&relative) {
            match (entry.remote, remote) {
                (Some(EntryKind::Directory), ExpectedRemoteDirectory::Directory)
                | (None, ExpectedRemoteDirectory::Missing) => {}
                (Some(EntryKind::File), _) => {
                    anyhow::bail!("remote file became a directory while planning {relative}")
                }
                _ => anyhow::bail!("remote directory changed while planning {relative}"),
            }
        }
        snapshots.push(ExpectedDirectorySnapshots {
            relative,
            local,
            remote,
        });
    }
    Ok(snapshots)
}

fn is_strict_directory_ancestor(ancestor: &str, relative: &str) -> bool {
    !ancestor.is_empty()
        && relative
            .strip_prefix(ancestor)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn directory_depth(relative: &str) -> usize {
    if relative.is_empty() {
        0
    } else {
        relative.split('/').count()
    }
}

fn capture_local_directory_identity(
    local_root: &Path,
    canonical_root: &Path,
    relative: &str,
) -> Result<Option<LocalDirectoryIdentity>> {
    let path = if relative.is_empty() {
        local_root.to_path_buf()
    } else {
        local_root.join(relative)
    };
    let symlink_metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading local directory {}", path.display()));
        }
    };
    let is_symlink = symlink_metadata.file_type().is_symlink();
    let metadata = if is_symlink {
        fs::metadata(&path)
            .with_context(|| format!("following local directory {}", path.display()))?
    } else {
        symlink_metadata
    };
    if !metadata.is_dir() {
        anyhow::bail!("local directory changed at {}", path.display());
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalizing local directory {}", path.display()))?;
    if !canonical.starts_with(canonical_root) {
        anyhow::bail!(
            "local directory {} resolves outside local_root {}",
            path.display(),
            local_root.display()
        );
    }
    let identity = Handle::from_path(&path)
        .with_context(|| format!("opening local directory {}", path.display()))?;
    Ok(Some(LocalDirectoryIdentity {
        canonical,
        identity,
        is_symlink,
    }))
}

fn directory_action(expected: &ExpectedDirectorySnapshots) -> Result<Option<DirectoryAction>> {
    match (&expected.local, expected.remote) {
        (ExpectedLocalDirectory::Root(_), ExpectedRemoteDirectory::Root)
        | (ExpectedLocalDirectory::Directory(_), ExpectedRemoteDirectory::Directory) => Ok(None),
        (ExpectedLocalDirectory::Missing, ExpectedRemoteDirectory::Directory) => {
            Ok(Some(DirectoryAction::CreateLocal))
        }
        (ExpectedLocalDirectory::Directory(_), ExpectedRemoteDirectory::Missing) => {
            Ok(Some(DirectoryAction::CreateRemote))
        }
        (ExpectedLocalDirectory::Missing, ExpectedRemoteDirectory::Missing) => Ok(None),
        _ => anyhow::bail!("invalid directory plan at {:?}", expected.relative),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_directory_plan<R: StrictRemote + RemoteWrite>(
    remote: &mut R,
    local_root: &Path,
    remote_root: &str,
    directories: &mut [ExpectedDirectorySnapshots],
    outcome: &mut SyncOutcome,
    mode: ExecutionMode,
    gate: &dyn CommitGate,
) -> Result<bool> {
    for index in 0..directories.len() {
        let Some(action) = directory_action(&directories[index])? else {
            continue;
        };
        if !gate.is_current() {
            return Ok(true);
        }
        let event_kind = match action {
            DirectoryAction::CreateLocal => SyncEventKind::CreatedLocalDirectory,
            DirectoryAction::CreateRemote => SyncEventKind::CreatedRemoteDirectory,
        };
        if mode.is_dry_run() {
            outcome.events.push(SyncEvent {
                path: directories[index].relative.clone(),
                kind: event_kind,
            });
            continue;
        }
        let relative = directories[index].relative.clone();
        let local_path = local_root.join(&relative);
        let remote_path = remote_join(remote_root, &relative);
        let mut created_local_identity = None;
        let decision = {
            let mut mutation = || {
                validate_directory_prefix(remote, local_root, remote_root, directories, index)?;
                match action {
                    DirectoryAction::CreateLocal => {
                        fs::create_dir(&local_path).with_context(|| {
                            format!("creating local directory {}", local_path.display())
                        })?;
                        let canonical_root = local_root.canonicalize().with_context(|| {
                            format!("canonicalizing local_root {}", local_root.display())
                        })?;
                        created_local_identity = Some(
                            capture_local_directory_identity(
                                local_root,
                                &canonical_root,
                                &relative,
                            )?
                            .ok_or_else(|| {
                                anyhow::anyhow!("created local directory disappeared at {relative}")
                            })?,
                        );
                    }
                    DirectoryAction::CreateRemote => remote
                        .mkdir_scoped_strict(&remote_path)
                        .with_context(|| format!("creating remote directory {relative:?}"))?,
                }
                Ok(())
            };
            gate.commit(&mut mutation)?
        };
        if decision == CommitDecision::Cancelled {
            return Ok(true);
        }
        match action {
            DirectoryAction::CreateLocal => {
                directories[index].local = ExpectedLocalDirectory::Directory(
                    created_local_identity
                        .take()
                        .expect("committed local directory has a captured identity"),
                );
            }
            DirectoryAction::CreateRemote => {
                directories[index].remote = ExpectedRemoteDirectory::Directory;
            }
        }
        outcome.events.push(SyncEvent {
            path: relative,
            kind: event_kind,
        });
    }
    Ok(false)
}

fn validate_directory_prefix<R: StrictRemote + RemoteWrite>(
    remote: &mut R,
    local_root: &Path,
    remote_root: &str,
    directories: &[ExpectedDirectorySnapshots],
    through: usize,
) -> Result<()> {
    validate_directory_entry(remote, local_root, remote_root, directories, 0)?;
    if through == 0 {
        return Ok(());
    }
    let relative = &directories[through].relative;
    for index in 1..through {
        if is_strict_directory_ancestor(&directories[index].relative, relative) {
            validate_directory_entry(remote, local_root, remote_root, directories, index)?;
        }
    }
    validate_directory_entry(remote, local_root, remote_root, directories, through)
}

fn validate_directory_snapshot<R: StrictRemote + RemoteWrite>(
    remote: &mut R,
    local_root: &Path,
    remote_root: &str,
    directories: &[ExpectedDirectorySnapshots],
    index: usize,
) -> Result<()> {
    if index > 0 {
        validate_directory_entry(remote, local_root, remote_root, directories, 0)?;
    }
    validate_directory_entry(remote, local_root, remote_root, directories, index)
}

fn validate_directory_entry<R: StrictRemote + RemoteWrite>(
    remote: &mut R,
    local_root: &Path,
    remote_root: &str,
    directories: &[ExpectedDirectorySnapshots],
    index: usize,
) -> Result<()> {
    let expected = &directories[index];
    validate_local_directory(local_root, expected)?;
    validate_remote_directory(remote, remote_root, directories, index)
}

fn validate_local_directory(
    local_root: &Path,
    expected: &ExpectedDirectorySnapshots,
) -> Result<()> {
    let local_path = if expected.relative.is_empty() {
        local_root.to_path_buf()
    } else {
        local_root.join(&expected.relative)
    };
    match &expected.local {
        ExpectedLocalDirectory::Missing => match fs::symlink_metadata(&local_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) => anyhow::bail!("local directory appeared at {}", local_path.display()),
            Err(error) => Err(error)
                .with_context(|| format!("reading local directory {}", local_path.display())),
        },
        ExpectedLocalDirectory::Root(identity) | ExpectedLocalDirectory::Directory(identity) => {
            let symlink_metadata = fs::symlink_metadata(&local_path)
                .with_context(|| format!("reading local directory {}", local_path.display()))?;
            let is_symlink = symlink_metadata.file_type().is_symlink();
            let metadata = if is_symlink {
                fs::metadata(&local_path).with_context(|| {
                    format!("following local directory {}", local_path.display())
                })?
            } else {
                symlink_metadata
            };
            let canonical = local_path.canonicalize().with_context(|| {
                format!("canonicalizing local directory {}", local_path.display())
            })?;
            let current_identity = Handle::from_path(&local_path)
                .with_context(|| format!("opening local directory {}", local_path.display()))?;
            if !metadata.is_dir()
                || is_symlink != identity.is_symlink
                || canonical != identity.canonical
                || current_identity != identity.identity
            {
                anyhow::bail!("local directory changed at {}", local_path.display());
            }
            Ok(())
        }
    }
}

fn validate_remote_directory<R: StrictRemote + RemoteWrite>(
    remote: &mut R,
    remote_root: &str,
    directories: &[ExpectedDirectorySnapshots],
    index: usize,
) -> Result<()> {
    let expected = &directories[index];
    if expected.remote == ExpectedRemoteDirectory::Root {
        remote
            .list_dir_strict(remote_root)
            .with_context(|| format!("validating remote root {remote_root:?}"))?;
        return Ok(());
    }
    if expected.remote == ExpectedRemoteDirectory::Missing
        && directories[..index].iter().any(|ancestor| {
            ancestor.remote == ExpectedRemoteDirectory::Missing
                && !ancestor.relative.is_empty()
                && expected
                    .relative
                    .strip_prefix(&ancestor.relative)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
    {
        return Ok(());
    }

    let remote_path = remote_join(remote_root, &expected.relative);
    let current = remote.destination_snapshot(remote_root, &remote_path)?;
    let matches = matches!(
        (expected.remote, current),
        (
            ExpectedRemoteDirectory::Missing,
            RemoteDestinationSnapshot::Missing
        ) | (
            ExpectedRemoteDirectory::Directory,
            RemoteDestinationSnapshot::Directory
        )
    );
    if !matches {
        anyhow::bail!(
            "remote directory changed during scoped sync at {:?}",
            expected.relative
        );
    }
    Ok(())
}

fn sort_outcome(outcome: &mut SyncOutcome) {
    outcome
        .events
        .sort_by(|left, right| left.path.cmp(&right.path));
    outcome
        .issues
        .sort_by(|left, right| left.path().cmp(right.path()));
}

fn outcome_error(outcome: &SyncOutcome) -> Result<Option<anyhow::Error>> {
    if let Some(SyncIssue::TypeConflict {
        path,
        local,
        remote,
    }) = outcome
        .issues
        .iter()
        .find(|issue| matches!(issue, SyncIssue::TypeConflict { .. }))
    {
        return Ok(Some(anyhow::anyhow!(
            "sync aborted: type conflict at {path} ({local:?} locally, {remote:?} remotely)"
        )));
    }
    if outcome.cancelled {
        return Ok(Some(anyhow::anyhow!(
            "sync cancelled because the selected scope changed; retry"
        )));
    }
    if outcome
        .issues
        .iter()
        .any(|issue| matches!(issue, SyncIssue::FileConflict { .. }))
    {
        return Ok(Some(
            crate::error::Exit::Conflict(
                "sync aborted: one or more files diverged on both sides (use --force to take local)"
                    .into(),
            )
            .into(),
        ));
    }
    Ok(None)
}

fn resolve_scoped_cli_path(config_path: &Path, input: &str) -> Result<SyncScope> {
    let cfg = Config::load(config_path)?;
    let scope = scope::from_cli_path(&cfg.paths.local_root, Some(input))?;
    if scope == SyncScope::LegacyProject {
        anyhow::bail!("explicit sync path resolved to legacy project scope");
    }
    Ok(scope)
}

fn render_outcome(outcome: &SyncOutcome, mode: ExecutionMode) -> Result<()> {
    for event in &outcome.events {
        match event.kind {
            SyncEventKind::Unchanged => {}
            SyncEventKind::Uploaded => {
                if mode.is_dry_run() {
                    println!("would upload {}", event.path);
                } else {
                    println!("uploaded {}", event.path);
                }
            }
            SyncEventKind::Downloaded => {
                if mode.is_dry_run() {
                    println!("would download {}", event.path);
                } else {
                    println!("downloaded {}", event.path);
                }
            }
            SyncEventKind::CreatedLocalDirectory => {
                if mode.is_dry_run() {
                    println!("would create local directory {}", event.path);
                } else {
                    println!("created local directory {}", event.path);
                }
            }
            SyncEventKind::CreatedRemoteDirectory => {
                if mode.is_dry_run() {
                    println!("would create remote directory {}", event.path);
                } else {
                    println!("created remote directory {}", event.path);
                }
            }
            SyncEventKind::SkippedAbsent => {
                eprintln!("skip (not on local or remote): {}", event.path);
            }
            SyncEventKind::ForcedRemoteOverwrite => {
                if mode.is_dry_run() {
                    eprintln!(
                        "would overwrite remote with local (--force): {}",
                        event.path
                    );
                } else {
                    eprintln!("overwriting remote with local (--force): {}", event.path);
                }
            }
        }
    }

    for issue in &outcome.issues {
        match issue {
            SyncIssue::FileConflict { path, state } => eprintln!(
                "conflict ({state:?}, local and remote diverged): {path} — pass --force to take local"
            ),
            SyncIssue::TypeConflict {
                path,
                local,
                remote,
            } => eprintln!("type conflict: {path} is {local:?} locally and {remote:?} remotely"),
        }
    }

    if let Some(error) = outcome_error(outcome)? {
        return Err(error);
    }
    Ok(())
}

fn require_interactive_terminal(stdin_is_terminal: bool, stdout_is_terminal: bool) -> Result<()> {
    if !stdin_is_terminal || !stdout_is_terminal {
        anyhow::bail!("ferry sync --select requires an interactive terminal; pass PATH directly");
    }
    Ok(())
}

pub fn require_select_terminal() -> Result<()> {
    use std::io::IsTerminal as _;

    require_interactive_terminal(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    )
}

fn select_from_terminal(config: &Config) -> Result<Option<SyncScope>> {
    let matcher = Matcher::new(&config.sync.ignore, &config.paths.local_root)?;
    let mut ftp = Ftp::connect(
        &config.connection.host,
        config.connection.port,
        &config.connection.user,
        &config.connection.password,
        config.connection.passive,
    )?;
    let mut remote = ScopedFtp { inner: &mut ftp };
    let mut source = picker::ProjectPickerSource::new(
        &mut remote,
        &config.paths.local_root,
        &config.paths.remote_root,
        &matcher,
    )?;
    let mut io = picker::StdioPickerIo;
    picker::select(&mut source, &mut io)
}

fn prepare_selected_sync(config_path: &Path, config: &Config, mode: ExecutionMode) {
    if !mode.should_apply() {
        return;
    }

    let config_dir = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    if let Err(error) = crate::names::migrate_legacy(config_dir) {
        eprintln!("warning: {error:#}");
    }
    if let Err(error) = crate::names::migrate_legacy(&config.paths.local_root) {
        eprintln!("warning: {error:#}");
    }
}

fn run_selected_with<Select, Run>(
    config_path: &Path,
    force: bool,
    mode: ExecutionMode,
    select_scope: Select,
    run_scope: Run,
) -> Result<()>
where
    Select: FnOnce(&Config) -> Result<Option<SyncScope>>,
    Run: FnOnce(&Config, SyncScope, bool, ExecutionMode) -> Result<SyncOutcome>,
{
    let config = Config::load(config_path)?;
    let Some(scope) = select_scope(&config)? else {
        return Ok(());
    };
    if scope == SyncScope::LegacyProject {
        anyhow::bail!("interactive selection cannot use legacy project scope");
    }
    prepare_selected_sync(config_path, &config, mode);
    let outcome = run_scope(&config, scope, force, mode)?;
    render_outcome(&outcome, mode)
}

pub fn run_cli(
    config_path: &Path,
    path: Option<&str>,
    select: bool,
    force: bool,
    mode: ExecutionMode,
) -> Result<()> {
    if path.is_none() && !select {
        return run_legacy(config_path, force, mode);
    }
    if select {
        require_select_terminal()?;
        return run_selected_with(
            config_path,
            force,
            mode,
            select_from_terminal,
            |config, scope, force, mode| {
                run_scoped_from_config(config, scope, force, mode, &UnconditionalCommitGate)
            },
        );
    }
    let input = path.expect("non-select scoped sync has a path");
    let scope = resolve_scoped_cli_path(config_path, input)?;
    let outcome = run_scoped(config_path, scope, force, mode, &UnconditionalCommitGate)?;
    render_outcome(&outcome, mode)
}

fn run_legacy(config_path: &Path, force: bool, mode: ExecutionMode) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let local_root = cfg.paths.local_root.clone();
    let state_path = state_path_for(&local_root, mode);
    let mut state = StateFile::load_or_default(&state_path)?;

    let matcher = Matcher::new(&cfg.sync.ignore, &local_root)?;

    let mut ftp = Ftp::connect(
        &cfg.connection.host,
        cfg.connection.port,
        &cfg.connection.user,
        &cfg.connection.password,
        cfg.connection.passive,
    )?;

    let mut local_paths: BTreeSet<String> = BTreeSet::new();
    walk_local(&local_root, &local_root, &matcher, &mut local_paths)?;
    let mut remote_paths: BTreeSet<String> = BTreeSet::new();
    // Remote paths skipped as symlinks. Absence from `remote_paths` alone
    // classifies as `LocalOnly` -> upload, and the server would resolve the
    // link, writing outside the configured remote root.
    let mut remote_symlinks: BTreeSet<String> = BTreeSet::new();
    walk_remote_with_symlinks(
        &mut ftp,
        &cfg.paths.remote_root,
        "",
        &mut remote_paths,
        &mut remote_symlinks,
    )?;

    // Union of every relative path we know about from any source.
    let mut targets: BTreeSet<String> = BTreeSet::new();
    targets.extend(local_paths.iter().cloned());
    targets.extend(remote_paths.iter().cloned());
    for k in state.files.keys() {
        targets.insert(k.clone());
    }

    let mut had_conflict = false;

    for rel in &targets {
        // Refuse before classification, and deliberately not overridable by
        // `--force`: that flag means "take local over remote edits", never
        // "write through a link to somewhere outside the remote root".
        if remote_symlinks.contains(rel) {
            eprintln!("refusing {rel}: the remote path is a symlink, not a file");
            had_conflict = true;
            continue;
        }

        let on_local = local_paths.contains(rel);
        let on_remote = remote_paths.contains(rel);

        if !on_local && !on_remote {
            // Stale state entry — exists in neither place. Nothing to do.
            eprintln!("skip (not on local or remote): {rel}");
            continue;
        }

        // Compute local hash from disk (cheap; we may still need bytes for upload).
        let local_hash = if on_local {
            Some(hash_file(&local_root.join(rel))?)
        } else {
            None
        };
        let remote_path = remote_join(&cfg.paths.remote_root, rel);
        // MDTM/SIZE fast path: if the cached (mtime, size) match, we trust
        // the cached hash and skip the download. When the fast path can't
        // fire we ask for bytes so we have them for the download branch.
        let rh = if on_remote {
            Some(remote_hash::compute(
                &mut ftp,
                &mut state,
                rel,
                &remote_path,
                true,
            )?)
        } else {
            None
        };
        let remote_hash_str = rh.as_ref().map(|r| r.sha256.clone());

        let known = state.files.get(rel).map(|r| r.sha256.as_str());
        let st = classify(local_hash.as_deref(), remote_hash_str.as_deref(), known);

        match st {
            FileState::InSync => {
                // Nothing to do; both sides agree.
            }
            FileState::LocalChanged | FileState::LocalOnly => {
                let bytes = std::fs::read(local_root.join(rel))
                    .with_context(|| format!("reading local {}", local_root.join(rel).display()))?;
                let new_hash = local_hash
                    .as_deref()
                    .expect("local_hash set when on_local is true");
                upload_one(
                    &mut ftp,
                    &mut state,
                    rel,
                    &remote_path,
                    &bytes,
                    new_hash,
                    mode,
                )?;
                if mode.is_dry_run() {
                    println!("would upload {rel}");
                } else {
                    println!("uploaded {rel}");
                }
            }
            FileState::RemoteChanged | FileState::RemoteOnly => {
                // We need real bytes to write locally. If the fast path
                // fired (rh.bytes is None) we would normally have classified
                // as InSync (since the cached hash matches state). Defensive
                // fallback: fetch fresh if bytes are absent.
                let rh_inner = rh.as_ref().expect("rh set when on_remote is true");
                let bytes_owned: Vec<u8> = match &rh_inner.bytes {
                    Some(b) => b.clone(),
                    None => ftp
                        .download(&remote_path)
                        .with_context(|| format!("downloading {remote_path}"))?,
                };
                download_one(
                    &mut ftp,
                    &mut state,
                    &local_root.join(rel),
                    rel,
                    &remote_path,
                    &bytes_owned,
                    &rh_inner.sha256,
                    mode,
                )?;
                if mode.is_dry_run() {
                    println!("would download {rel}");
                } else {
                    println!("downloaded {rel}");
                }
            }
            FileState::BothChanged | FileState::Untracked => {
                // Conflict: both sides moved away from the last known state
                // (or there is no known state and both sides have something).
                // Refuse unless --force, in which case the design says local
                // wins — sync's "force" is the user telling us to just push
                // their working copy as the canonical version.
                if force {
                    let bytes = std::fs::read(local_root.join(rel)).with_context(|| {
                        format!("reading local {}", local_root.join(rel).display())
                    })?;
                    let new_hash = local_hash
                        .as_deref()
                        .expect("local_hash set when on_local is true");
                    if mode.is_dry_run() {
                        eprintln!("would overwrite remote with local (--force): {rel}");
                    } else {
                        eprintln!("overwriting remote with local (--force): {rel}");
                    }
                    upload_one(
                        &mut ftp,
                        &mut state,
                        rel,
                        &remote_path,
                        &bytes,
                        new_hash,
                        mode,
                    )?;
                } else {
                    eprintln!(
                        "conflict ({:?}, local and remote diverged): {rel} — pass --force to take local",
                        st
                    );
                    had_conflict = true;
                }
            }
        }
    }

    // Persist apply progress even on conflict so the clean files don't have to be
    // re-hashed next run. Matches push/pull behavior.
    if mode.should_apply() {
        state.save(&state_path)?;
    }

    if had_conflict {
        // Tag as `Exit::Conflict` so `main()` returns exit code 2 — Zed's
        // tasks.json uses that to surface a "needs --force" message rather
        // than a generic failure.
        return Err(crate::error::Exit::Conflict(
            "sync aborted: one or more files diverged on both sides (use --force to take \
             local) or resolve to a remote symlink"
                .into(),
        )
        .into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::commit::CommitGate;
    use super::{SyncIssue, SyncOutcome};
    use crate::state::FileState;
    use anyhow::Result;

    #[test]
    fn structured_run_scoped_exposes_the_guarded_structured_api() {
        let _: fn(
            &std::path::Path,
            super::scope::SyncScope,
            bool,
            crate::commands::ExecutionMode,
            &dyn CommitGate,
        ) -> Result<SyncOutcome> = super::run_scoped;
    }

    #[test]
    fn structured_cli_file_conflicts_map_to_conflict_exit() {
        let outcome = SyncOutcome {
            issues: vec![SyncIssue::FileConflict {
                path: "conflict.c".into(),
                state: FileState::BothChanged,
            }],
            ..SyncOutcome::default()
        };

        let error = super::outcome_error(&outcome).unwrap().unwrap();
        assert!(matches!(
            error.downcast_ref::<crate::error::Exit>(),
            Some(crate::error::Exit::Conflict(_))
        ));
    }

    #[test]
    fn structured_cli_type_conflicts_map_to_generic_errors() {
        let outcome = SyncOutcome {
            issues: vec![SyncIssue::TypeConflict {
                path: "type.c".into(),
                local: super::EntryKind::File,
                remote: super::EntryKind::Directory,
            }],
            ..SyncOutcome::default()
        };

        let error = super::outcome_error(&outcome).unwrap().unwrap();
        assert!(error.downcast_ref::<crate::error::Exit>().is_none());
        assert!(format!("{error:#}").contains("type conflict"));
    }

    #[test]
    fn structured_cli_explicit_paths_never_resolve_to_legacy_project() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join(crate::names::CONFIG_FILE),
            r#"
[connection]
host = "example.invalid"
user = "u"
password = "p"
[paths]
local_root = "."
remote_root = "/remote"
"#,
        )
        .unwrap();
        let config = project.path().join(crate::names::CONFIG_FILE);

        assert_eq!(
            super::resolve_scoped_cli_path(&config, ".").unwrap(),
            super::scope::SyncScope::RootDirectory
        );
        assert_eq!(
            super::resolve_scoped_cli_path(&config, "file.c").unwrap(),
            super::scope::SyncScope::Path("file.c".into())
        );
    }

    #[test]
    fn structured_download_planning_rejects_local_hash_changes() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.c");
        std::fs::write(&target, b"current").unwrap();
        let current = crate::hash::hash_bytes(b"current");

        super::verify_planned_local_hash(&target, Some(&current)).unwrap();
        let error = super::verify_planned_local_hash(&target, Some("inventoried-old")).unwrap_err();
        assert!(format!("{error:#}").contains("local destination changed"));

        let missing = root.path().join("missing.c");
        super::verify_planned_local_hash(&missing, None).unwrap();
        assert!(super::verify_planned_local_hash(&missing, Some("expected-present")).is_err());
    }
}

#[cfg(test)]
mod picker_cli_tests {
    use super::scope::SyncScope;
    use crate::commands::ExecutionMode;
    use std::cell::Cell;

    fn config(project: &std::path::Path) -> std::path::PathBuf {
        let mirror = project.join("mirror");
        std::fs::create_dir(&mirror).unwrap();
        let path = project.join(crate::names::CONFIG_FILE);
        std::fs::write(
            &path,
            r#"
[connection]
host = "example.invalid"
user = "u"
password = "p"
[paths]
local_root = "mirror"
remote_root = "/chosen-root"
"#,
        )
        .unwrap();
        path
    }

    #[test]
    fn selected_cli_passes_one_scope_force_and_dry_run_to_structured_sync() {
        let project = tempfile::tempdir().unwrap();
        let config_path = config(project.path());
        let calls = Cell::new(0);

        super::run_selected_with(
            &config_path,
            true,
            ExecutionMode::DryRun,
            |loaded| {
                assert_eq!(loaded.paths.local_root, project.path().join("mirror"));
                assert_eq!(loaded.paths.remote_root, "/chosen-root");
                Ok(Some(SyncScope::Path("areas/smoke.c".into())))
            },
            |loaded, scope, force, mode| {
                calls.set(calls.get() + 1);
                assert_eq!(loaded.connection.host, "example.invalid");
                assert_eq!(loaded.paths.local_root, project.path().join("mirror"));
                assert_eq!(loaded.paths.remote_root, "/chosen-root");
                assert_eq!(scope, SyncScope::Path("areas/smoke.c".into()));
                assert!(force);
                assert_eq!(mode, ExecutionMode::DryRun);
                Ok(super::SyncOutcome::default())
            },
        )
        .unwrap();

        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn selected_cli_uses_the_browsed_config_snapshot_after_config_replacement() {
        let project = tempfile::tempdir().unwrap();
        let config_path = config(project.path());
        let replacement_root = project.path().join("replacement");
        std::fs::create_dir(&replacement_root).unwrap();
        let calls = Cell::new(0);

        super::run_selected_with(
            &config_path,
            true,
            ExecutionMode::DryRun,
            |loaded| {
                assert_eq!(loaded.connection.host, "example.invalid");
                assert_eq!(loaded.paths.local_root, project.path().join("mirror"));
                assert_eq!(loaded.paths.remote_root, "/chosen-root");
                std::fs::write(
                    &config_path,
                    format!(
                        r#"
[connection]
host = "replacement.invalid"
user = "replacement"
password = "replacement"
[paths]
local_root = "{}"
remote_root = "/replacement-root"
"#,
                        replacement_root.display()
                    ),
                )
                .unwrap();
                Ok(Some(SyncScope::Path("areas/smoke.c".into())))
            },
            |loaded, scope, force, mode| {
                calls.set(calls.get() + 1);
                assert_eq!(loaded.connection.host, "example.invalid");
                assert_eq!(loaded.paths.local_root, project.path().join("mirror"));
                assert_eq!(loaded.paths.remote_root, "/chosen-root");
                assert_eq!(scope, SyncScope::Path("areas/smoke.c".into()));
                assert!(force);
                assert_eq!(mode, ExecutionMode::DryRun);

                let replaced = crate::config::Config::load(&config_path).unwrap();
                assert_eq!(replaced.connection.host, "replacement.invalid");
                assert_eq!(replaced.paths.local_root, replacement_root);
                assert_eq!(replaced.paths.remote_root, "/replacement-root");
                Ok(super::SyncOutcome::default())
            },
        )
        .unwrap();

        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn selected_cli_apply_prepares_legacy_names_after_selection() {
        let project = tempfile::tempdir().unwrap();
        let mirror = project.path().join("mirror");
        std::fs::create_dir(&mirror).unwrap();
        let config_path = project.path().join(crate::names::LEGACY_CONFIG_FILE);
        std::fs::write(
            &config_path,
            r#"
[connection]
host = "example.invalid"
user = "u"
password = "p"
[paths]
local_root = "mirror"
remote_root = "/chosen-root"
"#,
        )
        .unwrap();
        let legacy_state = mirror.join(crate::names::LEGACY_STATE_DIR);
        std::fs::create_dir(&legacy_state).unwrap();
        std::fs::write(legacy_state.join("marker"), b"legacy").unwrap();
        let calls = Cell::new(0);

        super::run_selected_with(
            &config_path,
            false,
            ExecutionMode::Apply,
            |_| Ok(Some(SyncScope::RootDirectory)),
            |loaded, scope, _, _| {
                calls.set(calls.get() + 1);
                assert_eq!(loaded.paths.local_root, mirror);
                assert_eq!(scope, SyncScope::RootDirectory);
                assert!(
                    project.path().join(crate::names::CONFIG_FILE).exists(),
                    "accepted selection should migrate the legacy config"
                );
                assert!(!config_path.exists());
                assert!(
                    loaded
                        .paths
                        .local_root
                        .join(crate::names::STATE_DIR)
                        .join("marker")
                        .exists(),
                    "accepted selection should migrate legacy local-root state"
                );
                assert!(!legacy_state.exists());
                Ok(super::SyncOutcome::default())
            },
        )
        .unwrap();

        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn selected_cli_cancel_keeps_legacy_names_untouched() {
        let project = tempfile::tempdir().unwrap();
        let mirror = project.path().join("mirror");
        std::fs::create_dir(&mirror).unwrap();
        let config_path = project.path().join(crate::names::LEGACY_CONFIG_FILE);
        std::fs::write(
            &config_path,
            r#"
[connection]
host = "example.invalid"
user = "u"
password = "p"
[paths]
local_root = "mirror"
remote_root = "/chosen-root"
"#,
        )
        .unwrap();
        let config_before = std::fs::read(&config_path).unwrap();
        let legacy_state = mirror.join(crate::names::LEGACY_STATE_DIR);
        std::fs::create_dir(&legacy_state).unwrap();
        std::fs::write(legacy_state.join("marker"), b"legacy").unwrap();

        super::run_selected_with(
            &config_path,
            false,
            ExecutionMode::Apply,
            |_| Ok(None),
            |_, _, _, _| unreachable!("cancelled selection must not execute"),
        )
        .unwrap();

        assert_eq!(std::fs::read(&config_path).unwrap(), config_before);
        assert_eq!(
            std::fs::read(legacy_state.join("marker")).unwrap(),
            b"legacy"
        );
        assert!(!project.path().join(crate::names::CONFIG_FILE).exists());
        assert!(!mirror.join(crate::names::STATE_DIR).exists());
    }

    #[test]
    fn selected_cli_cancellation_is_success_and_never_falls_back_to_legacy() {
        let project = tempfile::tempdir().unwrap();
        let config_path = config(project.path());
        let calls = Cell::new(0);

        super::run_selected_with(
            &config_path,
            false,
            ExecutionMode::Apply,
            |_| Ok(None),
            |_, _, _, _| {
                calls.set(calls.get() + 1);
                Ok(super::SyncOutcome::default())
            },
        )
        .unwrap();

        assert_eq!(calls.get(), 0);
        assert!(
            !project.path().join(crate::names::STATE_DIR).exists(),
            "cancelled selection must not create state"
        );
    }

    #[test]
    fn selected_cli_rejects_legacy_project_without_running_sync() {
        let project = tempfile::tempdir().unwrap();
        let config_path = config(project.path());
        let calls = Cell::new(0);

        let error = super::run_selected_with(
            &config_path,
            false,
            ExecutionMode::Apply,
            |_| Ok(Some(SyncScope::LegacyProject)),
            |_, _, _, _| {
                calls.set(calls.get() + 1);
                Ok(super::SyncOutcome::default())
            },
        )
        .unwrap_err();

        assert_eq!(calls.get(), 0);
        assert!(
            error
                .to_string()
                .contains("cannot use legacy project scope")
        );
    }
    #[test]
    fn interactive_terminal_error_text_is_exact() {
        let error = super::require_interactive_terminal(false, true).unwrap_err();
        assert_eq!(
            error.to_string(),
            "ferry sync --select requires an interactive terminal; pass PATH directly"
        );
        assert!(super::require_interactive_terminal(true, false).is_err());
        assert!(super::require_interactive_terminal(true, true).is_ok());
    }
}
