//! `ferry pull` — one-way download from remote into the local mirror.
//!
//! Pull is asymmetric with push by design: it only ever writes local files;
//! it never deletes locally just because the remote is missing a file. That
//! way an accidentally-empty remote (or an interrupted upload by someone
//! else) cannot wipe your working tree.

mod prepared;

pub use prepared::{
    LocalIdentity, PreparedPull, RemoteFile, apply_prepared_pull, apply_prepared_pull_if,
    fetch_remote_one, prepare_force_pull_one, prepare_pull_one, pull_one,
};

use crate::commands::file_transfer::LocalPathExpectation;
use crate::commands::remote_hash;
use crate::commands::remote_hash::RemoteHash;
use crate::commands::sync::commit::{CommitDecision, CommitGate, UnconditionalCommitGate};
use crate::commands::transfer_temp::fresh_local_candidate;
use crate::commands::walk::{
    collect_remote_arg, remote_join, safe_arg, safe_rel, walk_local, walk_remote,
};
use crate::commands::{ExecutionMode, state_path_for};
use crate::config::Config;
use crate::ftp::Ftp;
use crate::hash::{hash_bytes, hash_file};
use crate::ignored::Matcher;
use crate::state::{FileRecord, FileState, StateFile, classify};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use same_file::Handle;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Normalize a pull argument. Absolute paths inside local_root remain local
/// paths; an absolute path outside local_root is interpreted as a path from
/// the configured remote root. This lets `pull /players/shaman/` work when
/// local_root is `/home/nicke/3S` and remote_root is `/`.
fn normalize_pull_arg(local_root: &Path, input: &str) -> Result<String> {
    if Path::new(input).is_absolute() {
        if let Ok(relative) = crate::project::relative_to_local_root(local_root, Path::new(input)) {
            return Ok(relative);
        }
        return safe_rel(input.trim_start_matches('/'));
    }
    safe_arg(local_root, input)
}

pub fn run(config_path: &Path, paths: &[String], force: bool, mode: ExecutionMode) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let local_root = cfg.paths.local_root.clone();
    let paths: Vec<String> = paths
        .iter()
        .map(|path| normalize_pull_arg(&local_root, path))
        .collect::<Result<_>>()?;
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

    // Scope the walks to the paths we actually care about, and build the
    // target set in one pass so we emit exactly one status message per arg.
    let mut local_paths: BTreeSet<String> = BTreeSet::new();
    let mut remote_paths: BTreeSet<String> = BTreeSet::new();
    let targets: Vec<String> = if paths.is_empty() {
        walk_local(&local_root, &local_root, &matcher, &mut local_paths)?;
        walk_remote(&mut ftp, &cfg.paths.remote_root, "", &mut remote_paths)?;
        let mut all: BTreeSet<String> = BTreeSet::new();
        all.extend(local_paths.iter().cloned());
        all.extend(remote_paths.iter().cloned());
        for k in state.files.keys() {
            all.insert(k.clone());
        }
        all.into_iter().collect()
    } else {
        let mut out: BTreeSet<String> = BTreeSet::new();
        for rel in &paths {
            let rel_no_slash = rel.trim_end_matches('/');
            let local_full = local_root.join(rel_no_slash);
            let mut found_here = 0usize;

            // Local: directory, file, or missing.
            if local_full.is_dir() {
                let before = local_paths.len();
                walk_local(&local_root, &local_full, &matcher, &mut local_paths)?;
                found_here += local_paths.len() - before;
            } else if local_full.is_file()
                && !matcher.is_ignored(&local_full, false)
                && local_paths.insert(rel_no_slash.to_string())
            {
                found_here += 1;
            }

            // Remote: subtree walk, or single-file resolution.
            found_here += collect_remote_arg(
                &mut ftp,
                &cfg.paths.remote_root,
                rel_no_slash,
                &mut remote_paths,
            );

            if found_here == 0 {
                eprintln!("skip (not on local or remote): {rel_no_slash}");
                continue;
            }

            // Add matches under this arg to targets. Exact match first,
            // then prefix expansion for the folder case.
            if (local_paths.contains(rel_no_slash) || remote_paths.contains(rel_no_slash))
                && !matcher.is_ignored(&local_root.join(rel_no_slash), false)
            {
                out.insert(rel_no_slash.to_string());
            }
            let prefix = format!("{rel_no_slash}/");
            for path in local_paths.iter().chain(remote_paths.iter()) {
                if path.starts_with(&prefix) && !matcher.is_ignored(&local_root.join(path), false) {
                    out.insert(path.clone());
                }
            }
        }
        out.into_iter().collect()
    };

    let mut had_conflict = false;

    for rel in &targets {
        let on_local = local_paths.contains(rel);
        let on_remote = remote_paths.contains(rel);

        if !on_local && !on_remote {
            // Stale state entry or path that exists on neither side. Nothing
            // to pull.
            eprintln!("skip (not on local or remote): {rel}");
            continue;
        }

        let local_hash = if on_local {
            Some(hash_file(&local_root.join(rel))?)
        } else {
            None
        };
        let remote_path = remote_join(&cfg.paths.remote_root, rel);
        // Use the MDTM/SIZE fast path: skip downloading entirely when the
        // server's (mtime, size) match the cached state — that case is
        // exactly the InSync branch below, which doesn't need the bytes.
        // When the fast path can't fire we ask for bytes (`want_bytes=true`)
        // so we have them in hand for the actual local write.
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
                // Nothing to write. Local matches remote.
            }
            FileState::LocalOnly => {
                // Pull is one-way: we don't delete local files because the
                // remote is missing them. Skip.
            }
            FileState::RemoteOnly | FileState::RemoteChanged => {
                // We need the actual remote bytes to write locally. If the
                // fast path fired (from_cache=true), we got here with the
                // hash but no bytes — which can only happen if state has a
                // record AND it matches AND yet classify said the remote
                // changed. That implies the local file diverged from `known`
                // while remote matches `known` (LocalChanged) — not this
                // branch. So in practice rh.bytes is Some here. Defensive
                // fallback: if bytes are missing, fetch them now.
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
                println!(
                    "{} {rel}",
                    if mode.is_dry_run() {
                        "would pull"
                    } else {
                        "pulled"
                    }
                );
            }
            FileState::LocalChanged | FileState::BothChanged | FileState::Untracked => {
                // Untracked = both sides have a file but no record of a prior sync.
                // Design action matrix treats this as "as if both-changed": refuse
                // without --force so the user makes an explicit choice.
                if force {
                    let rh_inner = rh.as_ref().expect("rh set when on_remote is true");
                    let bytes_owned: Vec<u8> = match &rh_inner.bytes {
                        Some(b) => b.clone(),
                        None => ftp
                            .download(&remote_path)
                            .with_context(|| format!("downloading {remote_path}"))?,
                    };
                    if mode.is_dry_run() {
                        eprintln!("would overwrite local with remote (--force): {rel}");
                    } else {
                        eprintln!("overwriting local with remote (--force): {rel}");
                    }
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
                } else {
                    eprintln!(
                        "conflict ({:?}, would overwrite local edits): {rel} — pass --force to override",
                        st
                    );
                    had_conflict = true;
                }
            }
        }
    }

    // Save state even if we hit a conflict — partial progress is still
    // worth persisting (e.g. clean RemoteChanged pulls that succeeded
    // before the conflict file).
    if mode.should_apply() {
        state.save(&state_path)?;
    }

    if had_conflict {
        // Tag as `Exit::Conflict` so `main()` returns exit code 2 — Zed's
        // tasks.json uses that to surface a "needs --force" message rather
        // than a generic failure.
        return Err(crate::error::Exit::Conflict(
            "pull aborted: one or more files have local changes (use --force to overwrite)".into(),
        )
        .into());
    }

    Ok(())
}

/// In [`ExecutionMode::Apply`], write `bytes` to `local_path` atomically (via
/// temp + rename) and refresh the corresponding state entry. In
/// [`ExecutionMode::DryRun`], perform neither mutation.
///
/// Shared with the sync command on its remote-wins branch.
// The state update needs both local and remote identities plus the downloaded
// payload metadata; wrapping these one-to-one inputs would only hide them.
#[allow(clippy::too_many_arguments)]
pub fn download_one(
    ftp: &mut Ftp,
    state: &mut StateFile,
    local_path: &Path,
    rel: &str,
    remote_path: &str,
    bytes: &[u8],
    new_hash: &str,
    mode: ExecutionMode,
) -> Result<()> {
    if mode.is_dry_run() {
        return Ok(());
    }
    let remote_mtime = ftp
        .mtime(remote_path)
        .with_context(|| format!("fetching mtime for {remote_path}"))?;
    let mut staged = Some(stage_local_write(local_path, bytes)?);
    let mut mutation = || {
        staged
            .take()
            .ok_or_else(|| anyhow::anyhow!("download commit mutation invoked more than once"))?
            .commit()?;
        record_download(state, rel, new_hash, bytes.len() as u64, remote_mtime);
        Ok(())
    };
    UnconditionalCommitGate.commit(&mut mutation)?;
    Ok(())
}

#[derive(Debug)]
pub(crate) struct ExpectedLocalDestination {
    snapshot: LocalPathExpectation,
}

impl ExpectedLocalDestination {
    pub(crate) fn capture(local_root: &Path, local_path: &Path) -> Result<Self> {
        Ok(Self {
            snapshot: LocalPathExpectation::capture(local_root, local_path)?,
        })
    }

    fn verify_unchanged(&self, local_path: &Path) -> Result<()> {
        self.snapshot
            .verify_supplied_path(local_path)
            .with_context(|| format!("local destination changed for {}", local_path.display()))?;
        self.snapshot
            .verify_unchanged()
            .with_context(|| format!("local destination changed for {}", local_path.display()))?;
        if !self.snapshot.is_file_or_missing() {
            anyhow::bail!(
                "local destination changed for {}: not a file",
                local_path.display()
            );
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn download_one_guarded(
    state: &mut StateFile,
    local_path: &Path,
    rel: &str,
    remote: &RemoteHash,
    expected: &ExpectedLocalDestination,
    mode: ExecutionMode,
    gate: &dyn CommitGate,
) -> Result<CommitDecision> {
    if mode.is_dry_run() {
        return Ok(CommitDecision::Committed);
    }
    let bytes = remote
        .bytes
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("remote payload missing for guarded download {rel}"))?;
    if bytes.len() as u64 != remote.size || hash_bytes(bytes) != remote.sha256 {
        anyhow::bail!("remote payload changed before guarded download {rel}");
    }

    expected.verify_unchanged(local_path)?;
    if !gate.is_current() {
        return Ok(CommitDecision::Cancelled);
    }
    let mut staged = Some(stage_local_write_scoped(expected, bytes)?);
    let mut mutation = || {
        expected.verify_unchanged(local_path)?;
        staged
            .take()
            .ok_or_else(|| anyhow::anyhow!("download commit mutation invoked more than once"))?
            .commit()?;
        record_download(state, rel, &remote.sha256, remote.size, remote.mtime);
        Ok(())
    };
    gate.commit(&mut mutation)
}

/// A uniquely owned sibling write that is revalidated immediately before
/// rename so foreign or changed bytes cannot be committed.
pub(crate) struct StagedLocalWrite {
    tmp: PathBuf,
    target: PathBuf,
    canonical_parent: PathBuf,
    parent_identity: Handle,
    temp_identity: Handle,
    expected_size: u64,
    expected_modified: Option<SystemTime>,
    expected_sha256: String,
    created_dirs: Vec<PathBuf>,
    committed: bool,
}

const MAX_TEMP_CANDIDATES: usize = 32;

pub(crate) fn stage_local_write(path: &Path, bytes: &[u8]) -> Result<StagedLocalWrite> {
    let created_dirs = create_missing_parent_dirs(path)?;
    let result = stage_local_write_with_candidates(
        path,
        bytes,
        created_dirs.clone(),
        || fresh_local_candidate(path),
        || Ok(()),
    );
    if result.is_err() {
        remove_created_dirs(&created_dirs);
    }
    result
}

fn stage_local_write_scoped(
    expected: &ExpectedLocalDestination,
    bytes: &[u8],
) -> Result<StagedLocalWrite> {
    expected.snapshot.verify_anchor()?;
    let target = expected.snapshot.resolved_path();
    stage_local_write_with_candidates(
        &target,
        bytes,
        Vec::new(),
        || fresh_local_candidate(&target),
        || {
            expected
                .snapshot
                .verify_anchor()
                .context("local target parent changed after staging")
        },
    )
}

fn stage_local_write_with_candidates(
    target: &Path,
    bytes: &[u8],
    created_dirs: Vec<PathBuf>,
    candidate: impl FnMut() -> Result<PathBuf>,
    after_create: impl FnOnce() -> Result<()>,
) -> Result<StagedLocalWrite> {
    stage_local_write_with_candidates_and_identity(
        target,
        bytes,
        created_dirs,
        candidate,
        after_create,
        |file, _tmp| file.try_clone().and_then(Handle::from_file),
    )
}

fn stage_local_write_with_candidates_and_identity(
    target: &Path,
    bytes: &[u8],
    created_dirs: Vec<PathBuf>,
    mut candidate: impl FnMut() -> Result<PathBuf>,
    after_create: impl FnOnce() -> Result<()>,
    capture_identity: impl FnOnce(&std::fs::File, &Path) -> std::io::Result<Handle>,
) -> Result<StagedLocalWrite> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("local target {} has no parent", target.display()))?;
    let canonical_parent = parent
        .canonicalize()
        .with_context(|| format!("canonicalizing local target parent {}", parent.display()))?;
    let parent_identity = Handle::from_path(&canonical_parent)
        .with_context(|| format!("opening local target parent {}", parent.display()))?;
    let target_leaf = target
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("local target {} has no file name", target.display()))?;
    let resolved_target = canonical_parent.join(target_leaf);
    let (mut file, tmp) = open_unique_local_temp(&mut candidate)?;
    let temp_identity = match capture_identity(&file, &tmp) {
        Ok(identity) => identity,
        Err(error) => {
            drop(file);
            return Err(error)
                .with_context(|| format!("capturing temp identity {}", tmp.display()));
        }
    };
    let mut staged = StagedLocalWrite {
        tmp,
        target: resolved_target,
        canonical_parent,
        parent_identity,
        temp_identity,
        expected_size: bytes.len() as u64,
        expected_modified: None,
        expected_sha256: hash_bytes(bytes),
        created_dirs,
        committed: false,
    };

    if let Err(error) = after_create() {
        drop(file);
        drop(staged);
        return Err(error);
    }
    let write_result = (|| -> Result<()> {
        file.write_all(bytes)
            .with_context(|| format!("writing temp file {}", staged.tmp.display()))?;
        file.flush()
            .with_context(|| format!("flushing temp file {}", staged.tmp.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("reading temp metadata {}", staged.tmp.display()))?;
        if !metadata.is_file() || metadata.len() != staged.expected_size {
            anyhow::bail!("local temp changed while staging {}", staged.tmp.display());
        }
        staged.expected_modified = Some(
            metadata
                .modified()
                .with_context(|| format!("reading temp mtime {}", staged.tmp.display()))?,
        );
        Ok(())
    })();
    drop(file);
    write_result?;
    staged.verify_for_commit()?;
    Ok(staged)
}

fn open_unique_local_temp(
    candidate: &mut impl FnMut() -> Result<PathBuf>,
) -> Result<(std::fs::File, PathBuf)> {
    for _ in 0..MAX_TEMP_CANDIDATES {
        let tmp = candidate()?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => return Ok((file, tmp)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("creating temp file {}", tmp.display()));
            }
        }
    }
    anyhow::bail!("unable to reserve a unique local transfer temp")
}

fn create_missing_parent_dirs(path: &Path) -> Result<Vec<PathBuf>> {
    let Some(parent) = path.parent() else {
        return Ok(Vec::new());
    };
    let mut missing = Vec::new();
    let mut current = parent;
    while !current.as_os_str().is_empty() && !current.exists() {
        missing.push(current.to_path_buf());
        let Some(next) = current.parent() else {
            break;
        };
        current = next;
    }

    let mut created = Vec::new();
    for directory in missing.iter().rev() {
        match std::fs::create_dir(directory) {
            Ok(()) => created.push(directory.clone()),
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists && directory.is_dir() => {}
            Err(error) => {
                remove_created_dirs(&created);
                return Err(error)
                    .with_context(|| format!("creating parent dir {}", directory.display()));
            }
        }
    }
    Ok(created)
}

fn remove_created_dirs(created_dirs: &[PathBuf]) {
    for directory in created_dirs.iter().rev() {
        let _ = std::fs::remove_dir(directory);
    }
}

impl StagedLocalWrite {
    fn verify_for_commit(&self) -> Result<()> {
        let current_parent = self.canonical_parent.canonicalize().with_context(|| {
            format!(
                "canonicalizing temp parent {}",
                self.canonical_parent.display()
            )
        })?;
        if current_parent != self.canonical_parent
            || Handle::from_path(&self.canonical_parent).with_context(|| {
                format!("opening temp parent {}", self.canonical_parent.display())
            })? != self.parent_identity
        {
            anyhow::bail!("local temp parent changed before commit");
        }
        let metadata = std::fs::symlink_metadata(&self.tmp)
            .with_context(|| format!("reading local temp {}", self.tmp.display()))?;
        if !metadata.file_type().is_file()
            || Handle::from_path(&self.tmp)
                .with_context(|| format!("opening local temp {}", self.tmp.display()))?
                != self.temp_identity
            || metadata.len() != self.expected_size
            || metadata
                .modified()
                .with_context(|| format!("reading local temp mtime {}", self.tmp.display()))?
                != self
                    .expected_modified
                    .ok_or_else(|| anyhow::anyhow!("local temp staging did not complete"))?
            || hash_file(&self.tmp)? != self.expected_sha256
        {
            anyhow::bail!("local temp changed before commit at {}", self.tmp.display());
        }
        Ok(())
    }

    fn commit(mut self) -> Result<()> {
        self.verify_for_commit()?;
        std::fs::rename(&self.tmp, &self.target).with_context(|| {
            format!(
                "renaming {} -> {}",
                self.tmp.display(),
                self.target.display()
            )
        })?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for StagedLocalWrite {
    fn drop(&mut self) {
        if !self.committed {
            let parent_is_owned = self
                .canonical_parent
                .canonicalize()
                .ok()
                .is_some_and(|parent| parent == self.canonical_parent)
                && Handle::from_path(&self.canonical_parent)
                    .ok()
                    .is_some_and(|identity| identity == self.parent_identity);
            let temp_is_owned = std::fs::symlink_metadata(&self.tmp)
                .ok()
                .is_some_and(|metadata| metadata.file_type().is_file())
                && Handle::from_path(&self.tmp)
                    .ok()
                    .is_some_and(|identity| identity == self.temp_identity);
            if parent_is_owned && temp_is_owned {
                let _ = std::fs::remove_file(&self.tmp);
            }
            remove_created_dirs(&self.created_dirs);
        }
    }
}

#[cfg(test)]
fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp.zedftp");
    PathBuf::from(s)
}

fn record_download(
    state: &mut StateFile,
    rel: &str,
    new_hash: &str,
    size: u64,
    remote_mtime: DateTime<Utc>,
) {
    state.files.insert(
        rel.to_string(),
        FileRecord {
            sha256: new_hash.to_string(),
            size,
            remote_mtime,
            last_synced: Utc::now(),
        },
    );
}

#[cfg(test)]
mod argument_tests {
    use super::normalize_pull_arg;
    use std::path::Path;

    #[test]
    fn absolute_remote_directory_is_relative_to_remote_root() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            normalize_pull_arg(root.path(), "/players/shaman/").unwrap(),
            "players/shaman"
        );
    }

    #[test]
    fn absolute_local_path_inside_root_stays_local_relative() {
        let root = tempfile::tempdir().unwrap();
        let local = root.path().join("players");
        std::fs::create_dir(&local).unwrap();
        assert_eq!(
            normalize_pull_arg(root.path(), local.to_str().unwrap()).unwrap(),
            "players"
        );
    }

    #[test]
    fn absolute_remote_parent_traversal_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        assert!(normalize_pull_arg(root.path(), "/players/../outside").is_err());
        assert!(
            normalize_pull_arg(
                root.path(),
                Path::new("/players/../outside").to_str().unwrap()
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod staging_tests {
    use super::{
        ExpectedLocalDestination, download_one_guarded, stage_local_write,
        stage_local_write_with_candidates, stage_local_write_with_candidates_and_identity,
    };
    use crate::commands::ExecutionMode;
    use crate::commands::remote_hash::RemoteHash;
    use crate::commands::sync::commit::{CommitDecision, CommitGate, UnconditionalCommitGate};
    use crate::hash::hash_bytes;
    use crate::state::{FileRecord, StateFile};
    use anyhow::Result;
    use chrono::{TimeZone, Utc};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn transfer_temps(directory: &Path) -> Vec<PathBuf> {
        let mut temps = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(crate::commands::transfer_temp::is_reserved_transfer_temp)
            })
            .collect::<Vec<_>>();
        temps.sort();
        temps
    }

    fn only_transfer_temp(directory: &Path) -> PathBuf {
        let temps = transfer_temps(directory);
        assert_eq!(temps.len(), 1, "expected one staged transfer temp");
        temps.into_iter().next().unwrap()
    }

    fn remote(bytes: &[u8]) -> RemoteHash {
        RemoteHash {
            sha256: hash_bytes(bytes),
            size: bytes.len() as u64,
            mtime: Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap(),
            from_cache: false,
            metadata_stable: true,
            bytes: Some(bytes.to_vec()),
            pre_download: None,
        }
    }

    fn old_record() -> FileRecord {
        FileRecord {
            sha256: hash_bytes(b"old state"),
            size: b"old state".len() as u64,
            remote_mtime: Utc.with_ymd_and_hms(2026, 8, 9, 9, 0, 0).unwrap(),
            last_synced: Utc.with_ymd_and_hms(2026, 8, 9, 9, 1, 0).unwrap(),
        }
    }

    struct CancelAtPreStage {
        checks: AtomicUsize,
        commits: AtomicUsize,
        directory: PathBuf,
        saw_temp: AtomicBool,
    }

    impl CommitGate for CancelAtPreStage {
        fn is_current(&self) -> bool {
            self.checks.fetch_add(1, Ordering::SeqCst) == 0
        }

        fn commit(&self, _mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision> {
            self.commits.fetch_add(1, Ordering::SeqCst);
            self.saw_temp.store(
                !transfer_temps(&self.directory).is_empty(),
                Ordering::SeqCst,
            );
            Ok(CommitDecision::Cancelled)
        }
    }

    struct CancelAfterStaging {
        directory: PathBuf,
        saw_temp: AtomicBool,
    }

    impl CommitGate for CancelAfterStaging {
        fn is_current(&self) -> bool {
            true
        }

        fn commit(&self, _mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision> {
            self.saw_temp.store(
                only_transfer_temp(&self.directory).is_file(),
                Ordering::SeqCst,
            );
            Ok(CommitDecision::Cancelled)
        }
    }

    #[derive(Clone, Copy)]
    enum HookOrder {
        Before,
        After,
    }

    struct HookGate {
        order: HookOrder,
        hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl HookGate {
        fn before(hook: impl FnOnce() + Send + 'static) -> Self {
            Self {
                order: HookOrder::Before,
                hook: Mutex::new(Some(Box::new(hook))),
            }
        }

        fn after(hook: impl FnOnce() + Send + 'static) -> Self {
            Self {
                order: HookOrder::After,
                hook: Mutex::new(Some(Box::new(hook))),
            }
        }

        fn run_hook(&self) {
            self.hook.lock().unwrap().take().unwrap()();
        }
    }

    impl CommitGate for HookGate {
        fn is_current(&self) -> bool {
            true
        }

        fn commit(&self, mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision> {
            if matches!(self.order, HookOrder::Before) {
                self.run_hook();
            }
            mutation()?;
            if matches!(self.order, HookOrder::After) {
                self.run_hook();
            }
            Ok(CommitDecision::Committed)
        }
    }

    struct PanicGate;

    impl CommitGate for PanicGate {
        fn is_current(&self) -> bool {
            panic!("dry run must not inspect the gate")
        }

        fn commit(&self, _mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision> {
            panic!("dry run must not claim the gate")
        }
    }

    #[test]
    fn guarded_download_dry_run_commits_without_staging_or_state_mutation() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("page.txt");
        std::fs::write(&target, b"local").unwrap();
        let expected = ExpectedLocalDestination::capture(root.path(), &target).unwrap();
        let mut state = StateFile::default();

        let decision = download_one_guarded(
            &mut state,
            &target,
            "page.txt",
            &remote(b"remote"),
            &expected,
            ExecutionMode::DryRun,
            &PanicGate,
        )
        .unwrap();

        assert_eq!(decision, CommitDecision::Committed);
        assert_eq!(std::fs::read(&target).unwrap(), b"local");
        assert!(transfer_temps(root.path()).is_empty());
        assert!(state.files.is_empty());
    }

    #[test]
    fn guarded_download_cancels_at_pre_stage_boundary_without_creating_a_temp() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("page.txt");
        std::fs::write(&target, b"original local").unwrap();
        let expected = ExpectedLocalDestination::capture(root.path(), &target).unwrap();
        let mut state = StateFile::default();
        let gate = CancelAtPreStage {
            checks: AtomicUsize::new(0),
            commits: AtomicUsize::new(0),
            directory: root.path().to_path_buf(),
            saw_temp: AtomicBool::new(false),
        };

        assert!(gate.is_current(), "models the outer scoped-plan check");
        let decision = download_one_guarded(
            &mut state,
            &target,
            "page.txt",
            &remote(b"new remote"),
            &expected,
            ExecutionMode::Apply,
            &gate,
        )
        .unwrap();

        assert_eq!(decision, CommitDecision::Cancelled);
        assert_eq!(gate.commits.load(Ordering::SeqCst), 0);
        assert!(!gate.saw_temp.load(Ordering::SeqCst));
        assert_eq!(std::fs::read(&target).unwrap(), b"original local");
        assert!(transfer_temps(root.path()).is_empty());
        assert!(state.files.is_empty());
    }

    #[test]
    fn guarded_download_cancellation_after_staging_preserves_destination_and_state() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("page.txt");
        std::fs::write(&target, b"original local").unwrap();
        let expected = ExpectedLocalDestination::capture(root.path(), &target).unwrap();
        let mut state = StateFile::default();
        let original_record = old_record();
        state
            .files
            .insert("page.txt".into(), original_record.clone());
        let gate = CancelAfterStaging {
            directory: root.path().to_path_buf(),
            saw_temp: AtomicBool::new(false),
        };

        let decision = download_one_guarded(
            &mut state,
            &target,
            "page.txt",
            &remote(b"new remote"),
            &expected,
            ExecutionMode::Apply,
            &gate,
        )
        .unwrap();

        assert_eq!(decision, CommitDecision::Cancelled);
        assert!(gate.saw_temp.load(Ordering::SeqCst));
        assert_eq!(std::fs::read(&target).unwrap(), b"original local");
        assert_eq!(state.files.get("page.txt"), Some(&original_record));
        assert!(transfer_temps(root.path()).is_empty());
    }

    #[test]
    fn guarded_download_fails_when_the_parent_disappears_without_recreating_it() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("nested");
        let target = parent.join("page.txt");
        std::fs::create_dir(&parent).unwrap();
        let expected = ExpectedLocalDestination::capture(root.path(), &target).unwrap();
        std::fs::remove_dir(&parent).unwrap();
        let mut state = StateFile::default();

        let error = download_one_guarded(
            &mut state,
            &target,
            "nested/page.txt",
            &remote(b"new remote"),
            &expected,
            ExecutionMode::Apply,
            &UnconditionalCommitGate,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("parent"));
        assert!(!parent.exists());
        assert!(transfer_temps(root.path()).is_empty());
        assert!(state.files.is_empty());
    }

    #[test]
    fn guarded_download_rejects_destination_identity_change_inside_claim() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("page.txt");
        std::fs::write(&target, b"same bytes").unwrap();
        let expected = ExpectedLocalDestination::capture(root.path(), &target).unwrap();
        let changed = target.clone();
        let gate = HookGate::before(move || {
            std::fs::remove_file(&changed).unwrap();
            std::fs::write(&changed, b"same bytes").unwrap();
        });
        let mut state = StateFile::default();

        let error = download_one_guarded(
            &mut state,
            &target,
            "page.txt",
            &remote(b"new remote"),
            &expected,
            ExecutionMode::Apply,
            &gate,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("destination changed"));
        assert_eq!(std::fs::read(&target).unwrap(), b"same bytes");
        assert!(state.files.is_empty());
        assert!(transfer_temps(root.path()).is_empty());
    }

    #[test]
    fn guarded_download_rejects_destination_type_change_inside_claim() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("page.txt");
        std::fs::write(&target, b"local").unwrap();
        let expected = ExpectedLocalDestination::capture(root.path(), &target).unwrap();
        let changed = target.clone();
        let gate = HookGate::before(move || {
            std::fs::remove_file(&changed).unwrap();
            std::fs::create_dir(&changed).unwrap();
        });
        let mut state = StateFile::default();

        let error = download_one_guarded(
            &mut state,
            &target,
            "page.txt",
            &remote(b"new remote"),
            &expected,
            ExecutionMode::Apply,
            &gate,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("destination changed"));
        assert!(target.is_dir());
        assert!(state.files.is_empty());
        assert!(transfer_temps(root.path()).is_empty());
    }

    #[test]
    fn guarded_download_rejects_temp_disappearance_inside_claim() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("page.txt");
        std::fs::write(&target, b"local").unwrap();
        let expected = ExpectedLocalDestination::capture(root.path(), &target).unwrap();
        let temp_directory = root.path().to_path_buf();
        let gate = HookGate::before(move || {
            std::fs::remove_file(only_transfer_temp(&temp_directory)).unwrap();
        });
        let mut state = StateFile::default();

        let error = download_one_guarded(
            &mut state,
            &target,
            "page.txt",
            &remote(b"new remote"),
            &expected,
            ExecutionMode::Apply,
            &gate,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("local temp"));
        assert_eq!(std::fs::read(&target).unwrap(), b"local");
        assert!(state.files.is_empty());
        assert!(transfer_temps(root.path()).is_empty());
    }

    #[test]
    fn guarded_download_rejects_temp_content_change_inside_claim() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("page.txt");
        std::fs::write(&target, b"local").unwrap();
        let expected = ExpectedLocalDestination::capture(root.path(), &target).unwrap();
        let temp_directory = root.path().to_path_buf();
        let gate = HookGate::before(move || {
            std::fs::write(only_transfer_temp(&temp_directory), b"foreign bytes").unwrap();
        });
        let mut state = StateFile::default();

        let error = download_one_guarded(
            &mut state,
            &target,
            "page.txt",
            &remote(b"new remote"),
            &expected,
            ExecutionMode::Apply,
            &gate,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("local temp changed"));
        assert_eq!(std::fs::read(&target).unwrap(), b"local");
        assert!(state.files.is_empty());
        assert!(transfer_temps(root.path()).is_empty());
    }

    #[test]
    fn guarded_download_does_not_clean_up_a_replaced_temp() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("page.txt");
        std::fs::write(&target, b"local").unwrap();
        let expected = ExpectedLocalDestination::capture(root.path(), &target).unwrap();
        let temp_directory = root.path().to_path_buf();
        let gate = HookGate::before(move || {
            let temp = only_transfer_temp(&temp_directory);
            std::fs::remove_file(&temp).unwrap();
            std::fs::write(&temp, b"foreign replacement").unwrap();
        });
        let mut state = StateFile::default();

        let error = download_one_guarded(
            &mut state,
            &target,
            "page.txt",
            &remote(b"new remote"),
            &expected,
            ExecutionMode::Apply,
            &gate,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("local temp changed"));
        assert_eq!(std::fs::read(&target).unwrap(), b"local");
        assert!(state.files.is_empty());
        let foreign = only_transfer_temp(root.path());
        assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign replacement");
        std::fs::remove_file(foreign).unwrap();
    }

    #[test]
    fn guarded_download_claim_can_commit_before_late_invalidation() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("page.txt");
        std::fs::write(&target, b"local").unwrap();
        let expected = ExpectedLocalDestination::capture(root.path(), &target).unwrap();
        let invalidated = target.clone();
        let gate = HookGate::after(move || std::fs::write(invalidated, b"late edit").unwrap());
        let mut state = StateFile::default();
        let expected_remote = remote(b"new remote");

        let decision = download_one_guarded(
            &mut state,
            &target,
            "page.txt",
            &expected_remote,
            &expected,
            ExecutionMode::Apply,
            &gate,
        )
        .unwrap();

        assert_eq!(decision, CommitDecision::Committed);
        assert_eq!(std::fs::read(&target).unwrap(), b"late edit");
        assert_eq!(state.files["page.txt"].sha256, expected_remote.sha256);
        assert!(transfer_temps(root.path()).is_empty());
    }

    #[test]
    fn staging_leaves_a_preexisting_legacy_temp_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("page.txt");
        let tmp = super::tmp_path(&target);
        let original_target = b"original target";
        let original_tmp = b"another writer's staged bytes";
        std::fs::write(&target, original_target).unwrap();
        std::fs::write(&tmp, original_tmp).unwrap();

        let staged = stage_local_write(&target, b"replacement").unwrap();
        let owned_tmp = staged.tmp.clone();
        drop(staged);

        assert!(!owned_tmp.exists());
        assert_eq!(std::fs::read(&target).unwrap(), original_target);
        assert_eq!(std::fs::read(&tmp).unwrap(), original_tmp);
    }

    #[test]
    fn staging_gives_interleaved_writers_distinct_owned_temps() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("page.txt");

        let first = stage_local_write(&target, b"first writer").unwrap();
        let second = stage_local_write(&target, b"second writer").unwrap();
        let first_tmp = first.tmp.clone();
        let second_tmp = second.tmp.clone();

        assert_ne!(first_tmp, second_tmp);
        assert!(crate::commands::transfer_temp::is_reserved_transfer_temp(
            first_tmp.file_name().unwrap().to_str().unwrap()
        ));
        assert!(crate::commands::transfer_temp::is_reserved_transfer_temp(
            second_tmp.file_name().unwrap().to_str().unwrap()
        ));
        assert_eq!(std::fs::read(&first_tmp).unwrap(), b"first writer");
        assert_eq!(std::fs::read(&second_tmp).unwrap(), b"second writer");

        drop(first);
        assert!(!first_tmp.exists());
        assert!(second_tmp.exists());
        drop(second);

        assert!(!second_tmp.exists());
    }

    #[test]
    fn identity_capture_failure_preserves_unproven_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("page.txt");
        let candidate = crate::commands::transfer_temp::local_candidate(
            &target,
            "22222222222222222222222222222222",
        )
        .unwrap();

        let error = stage_local_write_with_candidates_and_identity(
            &target,
            b"our staged bytes",
            Vec::new(),
            || Ok(candidate.clone()),
            || Ok(()),
            |_file, temp_path| {
                std::fs::remove_file(temp_path)?;
                std::fs::write(temp_path, b"foreign replacement")?;
                Err(std::io::Error::other("injected identity capture failure"))
            },
        )
        .err()
        .expect("identity capture must fail");

        assert!(format!("{error:#}").contains("capturing temp identity"));
        assert_eq!(std::fs::read(&candidate).unwrap(), b"foreign replacement");
        assert!(!target.exists());
    }

    #[test]
    fn local_staging_retries_an_occupied_unique_candidate_without_touching_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("page.txt");
        let occupied = crate::commands::transfer_temp::local_candidate(
            &target,
            "00000000000000000000000000000000",
        )
        .unwrap();
        let fresh = crate::commands::transfer_temp::local_candidate(
            &target,
            "11111111111111111111111111111111",
        )
        .unwrap();
        std::fs::write(&occupied, b"foreign temp").unwrap();
        let mut candidates = [occupied.clone(), fresh.clone()].into_iter();

        let staged = stage_local_write_with_candidates(
            &target,
            b"our staged bytes",
            Vec::new(),
            || Ok(candidates.next().expect("candidate")),
            || Ok(()),
        )
        .unwrap();

        assert_eq!(staged.tmp, fresh);
        assert_eq!(std::fs::read(&occupied).unwrap(), b"foreign temp");
        assert_eq!(std::fs::read(&staged.tmp).unwrap(), b"our staged bytes");
        drop(staged);

        assert_eq!(std::fs::read(&occupied).unwrap(), b"foreign temp");
        assert!(!fresh.exists());
    }
    #[cfg(unix)]
    #[test]
    fn staging_leaves_a_preexisting_legacy_temp_symlink_untouched() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("page.txt");
        let tmp = super::tmp_path(&target);
        let symlink_target = dir.path().join("do-not-touch.txt");
        let original = b"unrelated bytes";
        std::fs::write(&symlink_target, original).unwrap();
        symlink(&symlink_target, &tmp).unwrap();

        let staged = stage_local_write(&target, b"replacement").unwrap();
        let owned_tmp = staged.tmp.clone();
        drop(staged);

        assert!(!owned_tmp.exists());
        assert!(
            std::fs::symlink_metadata(&tmp)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&symlink_target).unwrap(), original);
        assert!(!target.exists());
    }
}
