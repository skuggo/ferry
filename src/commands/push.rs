//! `ferry push` — one-way upload from the local mirror to remote.
//!
//! Push is asymmetric with pull by design: it only ever writes remote files;
//! it never deletes a remote file because the local mirror is missing it. A
//! locally-missing file is treated as "not yours to delete" — `rm` is its
//! own deliberate command.

use crate::commands::file_transfer::{
    LocalPathExpectation, RemoteDestinationSnapshot, RemotePresence, RemoteWrite, TransferOutcome,
    TransferStatus, probe_remote_file,
};
use crate::commands::remote_hash;
use crate::commands::sync::commit::{CommitDecision, CommitGate, UnconditionalCommitGate};
use crate::commands::transfer_temp::fresh_remote_candidate;
use crate::commands::walk::{
    collect_remote_arg_with_symlinks, leaf_is_symlink, remote_join, safe_arg, safe_rel, walk_local,
    walk_remote_with_symlinks,
};
use crate::commands::{ExecutionMode, state_path_for};
use crate::config::Config;
use crate::ftp::Ftp;
use crate::hash::hash_bytes;
use crate::ignored::Matcher;
use crate::state::{FileRecord, FileState, StateFile, classify};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub fn run(config_path: &Path, paths: &[String], force: bool, mode: ExecutionMode) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let local_root = cfg.paths.local_root.clone();
    let paths: Vec<String> = paths
        .iter()
        .map(|path| safe_arg(&local_root, path))
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

    // Scope the walks to the paths we actually care about. Bare `push`
    // walks the whole tree; `push <folder>` walks only that subtree,
    // `push <file>` skips the walks entirely.
    let mut local_paths: BTreeSet<String> = BTreeSet::new();
    let mut remote_paths: BTreeSet<String> = BTreeSet::new();
    // Remote paths the walk refused to enumerate because the server described
    // them as symlinks. They are absent from `remote_paths`, and absence alone
    // classifies as `LocalOnly` -> upload; the server would then resolve the
    // link and put the write outside the remote root.
    let mut remote_symlinks: BTreeSet<String> = BTreeSet::new();
    if paths.is_empty() {
        walk_local(&local_root, &local_root, &matcher, &mut local_paths)?;
        walk_remote_with_symlinks(
            &mut ftp,
            &cfg.paths.remote_root,
            "",
            &mut remote_paths,
            &mut remote_symlinks,
        )?;
    } else {
        for rel_no_slash in &paths {
            let local_full = local_root.join(rel_no_slash);
            if local_full.is_dir() {
                walk_local(&local_root, &local_full, &matcher, &mut local_paths)?;
            } else if local_full.is_file() {
                local_paths.insert(rel_no_slash.clone());
            }
            collect_remote_arg_with_symlinks(
                &mut ftp,
                &cfg.paths.remote_root,
                rel_no_slash,
                &mut remote_paths,
                &mut remote_symlinks,
            );
        }
    }

    // Build the target set. With explicit args, restrict targets to the
    // union of files under those args (via scoped walks above) — this
    // gives folder-expansion semantics matching `pull`.
    let targets: Vec<String> = {
        let mut all: BTreeSet<String> = BTreeSet::new();
        all.extend(local_paths.iter().cloned());
        all.extend(remote_paths.iter().cloned());
        if paths.is_empty() {
            for k in state.files.keys() {
                all.insert(k.clone());
            }
        }
        all.into_iter()
            .filter(|rel| !matcher.is_ignored(&local_root.join(rel), false))
            .collect()
    };

    let mut had_conflict = false;

    for rel in &targets {
        // Refuse before classification. Deliberately not overridable by
        // `--force`: that flag means "overwrite remote edits", never "write
        // through a link to somewhere outside the remote root".
        if remote_symlinks.contains(rel) {
            eprintln!("refusing {rel}: the remote path is a symlink, not a file");
            had_conflict = true;
            continue;
        }

        let on_local = local_paths.contains(rel);
        let on_remote = remote_paths.contains(rel);

        if !on_local && !on_remote {
            // Stale state entry or path that exists on neither side. Nothing
            // to push.
            eprintln!("skip (not on local or remote): {rel}");
            continue;
        }

        // Read local bytes once (we need them for both hashing and upload),
        // but only when there's actually a local file to consider.
        let local_bytes = if on_local {
            Some(
                std::fs::read(local_root.join(rel))
                    .with_context(|| format!("reading local {}", local_root.join(rel).display()))?,
            )
        } else {
            None
        };
        let local_hash = local_bytes.as_deref().map(hash_bytes);

        let remote_path = remote_join(&cfg.paths.remote_root, rel);
        // Hash the remote, using the MDTM/SIZE fast path when possible.
        // Push only needs the hash for classification — never the bytes —
        // so request `want_bytes=false`.
        let remote_hash = if on_remote {
            Some(remote_hash::compute(&mut ftp, &mut state, rel, &remote_path, false)?.sha256)
        } else {
            None
        };

        let known = state.files.get(rel).map(|r| r.sha256.as_str());
        let st = classify(local_hash.as_deref(), remote_hash.as_deref(), known);

        match st {
            FileState::InSync => {
                // Nothing to upload. Local matches remote.
            }
            FileState::RemoteOnly => {
                // Push is one-way: don't delete the remote file just because
                // the local mirror is missing it. The user can `rm` deliberately.
            }
            FileState::LocalOnly | FileState::LocalChanged => {
                let bytes = local_bytes
                    .as_deref()
                    .expect("local_bytes set when on_local is true");
                let new_hash = local_hash
                    .as_deref()
                    .expect("local_hash matches local_bytes");
                upload_one(
                    &mut ftp,
                    &mut state,
                    rel,
                    &remote_path,
                    bytes,
                    new_hash,
                    mode,
                )?;
                println!(
                    "{} {rel}",
                    if mode.is_dry_run() {
                        "would push"
                    } else {
                        "pushed"
                    }
                );
            }
            FileState::RemoteChanged | FileState::BothChanged | FileState::Untracked => {
                // Untracked = both sides have a file but no record of a prior sync.
                // Design action matrix treats this as "as if both-changed": refuse
                // without --force so the user makes an explicit choice.
                if force {
                    let bytes = local_bytes
                        .as_deref()
                        .expect("local_bytes set when on_local is true");
                    let new_hash = local_hash
                        .as_deref()
                        .expect("local_hash matches local_bytes");
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
                        bytes,
                        new_hash,
                        mode,
                    )?;
                } else {
                    eprintln!(
                        "conflict ({:?}, would overwrite remote edits): {rel} — pass --force to override",
                        st
                    );
                    had_conflict = true;
                }
            }
        }
    }

    // Save state even if we hit a conflict — partial progress is still
    // worth persisting (e.g. clean LocalChanged pushes that succeeded
    // before the conflict file).
    if mode.should_apply() {
        state.save(&state_path)?;
    }

    if had_conflict {
        // Tag as `Exit::Conflict` so `main()` returns exit code 2 — Zed's
        // tasks.json uses that to surface a "needs --force" message rather
        // than a generic failure.
        return Err(crate::error::Exit::Conflict(
            "push aborted: one or more files have remote changes (use --force to overwrite) \
             or resolve to a remote symlink"
                .into(),
        )
        .into());
    }

    Ok(())
}

/// Fast single-file push that bypasses the local + remote tree walks used by
/// [`run`]. It returns structured data and leaves all reporting to its caller.
pub fn push_one(
    config_path: &Path,
    rel: &str,
    force: bool,
    mode: ExecutionMode,
) -> Result<TransferOutcome> {
    let rel = safe_rel(rel).with_context(|| format!("push {rel}"))?;
    (|| {
        let cfg = Config::load(config_path)?;
        let local_root = cfg.paths.local_root.clone();
        let state_path = state_path_for(&local_root, mode);
        let mut state = StateFile::load_or_default(&state_path)?;

        let mut ftp = Ftp::connect(
            &cfg.connection.host,
            cfg.connection.port,
            &cfg.connection.user,
            &cfg.connection.password,
            cfg.connection.passive,
        )?;

        let local_path = local_root.join(&rel);
        let local_bytes = if local_path.exists() {
            Some(
                std::fs::read(&local_path)
                    .with_context(|| format!("reading local {}", local_path.display()))?,
            )
        } else {
            None
        };
        let local_hash = local_bytes.as_deref().map(hash_bytes);

        let remote_path = remote_join(&cfg.paths.remote_root, &rel);
        let remote_exists = match probe_remote_file(&mut ftp, &remote_path)? {
            RemotePresence::Present => true,
            RemotePresence::Missing => false,
        };
        // `push_one` runs no tree walk, so nothing else here can tell a file
        // from a symlink to one -- `SIZE` and `NLST` both succeed for a link.
        // One listing of the parent settles it, which is affordable for a
        // single explicit path (this is the editor's push-on-save path).
        if remote_exists
            && leaf_is_symlink(&mut ftp, &cfg.paths.remote_root, &remote_path) == Some(true)
        {
            return Err(crate::error::Exit::Conflict(format!(
                "refusing to push {rel}: the remote path is a symlink, not a file"
            ))
            .into());
        }
        if !remote_exists && local_hash.is_none() {
            anyhow::bail!("neither local nor remote has {rel}");
        }
        let remote_hash = if remote_exists {
            Some(remote_hash::compute(&mut ftp, &mut state, &rel, &remote_path, false)?.sha256)
        } else {
            None
        };

        let known = state.files.get(&rel).map(|r| r.sha256.as_str());
        let st = classify(local_hash.as_deref(), remote_hash.as_deref(), known);
        let status = match st {
            FileState::InSync => TransferStatus::Unchanged,
            FileState::RemoteOnly => TransferStatus::SkippedMissingSource,
            FileState::LocalOnly | FileState::LocalChanged => {
                let bytes = local_bytes
                    .as_deref()
                    .expect("local_bytes set when local file exists");
                let new_hash = local_hash
                    .as_deref()
                    .expect("local_hash matches local_bytes");
                upload_one(
                    &mut ftp,
                    &mut state,
                    &rel,
                    &remote_path,
                    bytes,
                    new_hash,
                    mode,
                )?;
                TransferStatus::Transferred
            }
            FileState::RemoteChanged | FileState::BothChanged | FileState::Untracked => {
                if !force {
                    return Err(crate::error::Exit::Conflict(format!(
                        "conflict ({st:?}) on {rel}: remote changes present; pass --force to override",
                    ))
                    .into());
                }
                let bytes = local_bytes
                    .as_deref()
                    .expect("local_bytes set when local file exists");
                let new_hash = local_hash
                    .as_deref()
                    .expect("local_hash matches local_bytes");
                upload_one(
                    &mut ftp,
                    &mut state,
                    &rel,
                    &remote_path,
                    bytes,
                    new_hash,
                    mode,
                )?;
                TransferStatus::Transferred
            }
        };

        if mode.should_apply() && status == TransferStatus::Transferred {
            state.save(&state_path)?;
        }
        Ok(TransferOutcome::new(&rel, status))
    })()
    .with_context(|| format!("push {rel}"))
}

/// Upload `bytes` to `remote_path` atomically (via temp + rename) and refresh
/// the corresponding state entry. Shared with the sync command — both push
/// and sync need exactly this "upload + record the new hash" sequence on the
/// local-wins branch.
pub fn upload_one(
    ftp: &mut Ftp,
    state: &mut StateFile,
    rel: &str,
    remote_path: &str,
    bytes: &[u8],
    new_hash: &str,
    mode: ExecutionMode,
) -> Result<()> {
    if mode.is_dry_run() {
        return Ok(());
    }
    ensure_remote_parents(ftp, remote_path)?;
    let staged = stage_remote_write(ftp, remote_path, bytes, new_hash)?;
    let mut renamed = false;
    let result = {
        let mut mutation = || {
            ftp.rename(&staged.temp_path, &staged.target_path)
                .with_context(|| {
                    format!("renaming {} -> {}", staged.temp_path, staged.target_path)
                })?;
            record_upload(state, rel, &staged, Utc::now());
            renamed = true;
            Ok(())
        };
        UnconditionalCommitGate.commit(&mut mutation)
    };
    if !renamed {
        let _ = ftp.rm(&staged.temp_path);
    }
    result.map(|_| ())
}

#[derive(Debug)]
pub(crate) struct ExpectedLocalSource {
    pub path: PathBuf,
    snapshot: LocalPathExpectation,
}

impl ExpectedLocalSource {
    pub(crate) fn capture(local_root: &Path, path: &Path) -> Result<Self> {
        let snapshot = LocalPathExpectation::capture(local_root, path)?;
        if snapshot.expected_file_hash().is_none() {
            anyhow::bail!("local source {} is not a regular file", path.display());
        }
        Ok(Self {
            path: snapshot.resolved_path(),
            snapshot,
        })
    }

    pub(crate) fn verify_unchanged(&self, expected_hash: &str) -> Result<()> {
        if self.path != self.snapshot.resolved_path()
            || self.snapshot.expected_file_hash() != Some(expected_hash)
        {
            anyhow::bail!("local source changed at {}", self.path.display());
        }
        self.snapshot
            .verify_unchanged()
            .with_context(|| format!("local source changed at {}", self.path.display()))?;
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct ExpectedRemoteDestination {
    pub snapshot: RemoteDestinationSnapshot,
}

#[derive(Debug)]
pub(crate) struct StagedRemoteWrite {
    temp_path: String,
    target_path: String,
    size: u64,
    modified: DateTime<Utc>,
    sha256: String,
    temp_snapshot: RemoteDestinationSnapshot,
}

#[derive(Debug, Clone, Copy)]
struct IntendedRemoteTempPayload<'a> {
    size: u64,
    sha256: &'a str,
}

impl IntendedRemoteTempPayload<'_> {
    fn matches_snapshot(self, snapshot: &RemoteDestinationSnapshot) -> bool {
        matches!(
            snapshot,
            RemoteDestinationSnapshot::File { size, sha256, .. }
                if *size == self.size && sha256 == self.sha256
        )
    }
}

const MAX_REMOTE_TEMP_CANDIDATES: usize = 32;

fn stage_remote_write<R: RemoteWrite>(
    remote: &mut R,
    remote_path: &str,
    bytes: &[u8],
    hash: &str,
) -> Result<StagedRemoteWrite> {
    stage_remote_write_with_candidates(remote, remote_path, bytes, hash, || {
        fresh_remote_candidate(remote_path)
    })
}

fn stage_remote_write_with_candidates<R: RemoteWrite>(
    remote: &mut R,
    remote_path: &str,
    bytes: &[u8],
    hash: &str,
    mut candidate: impl FnMut() -> Result<String>,
) -> Result<StagedRemoteWrite> {
    let temp_path = candidate()?;
    if let Err(error) = remote.upload_bytes(&temp_path, bytes) {
        let _ = remote.rm(&temp_path);
        return Err(error).with_context(|| format!("uploading temp {temp_path}"));
    }
    let modified = match remote.mtime(&temp_path) {
        Ok(modified) => modified,
        Err(error) => {
            let _ = remote.rm(&temp_path);
            return Err(error).with_context(|| format!("fetching mtime for temp {temp_path}"));
        }
    };
    Ok(StagedRemoteWrite {
        temp_path,
        target_path: remote_path.to_string(),
        size: bytes.len() as u64,
        modified,
        sha256: hash.to_string(),
        temp_snapshot: RemoteDestinationSnapshot::File {
            size: bytes.len() as u64,
            modified,
            sha256: hash.to_string(),
        },
    })
}

fn stage_remote_write_guarded<R: RemoteWrite>(
    remote: &mut R,
    remote_root: &str,
    remote_path: &str,
    bytes: &[u8],
    hash: &str,
) -> Result<StagedRemoteWrite> {
    stage_remote_write_guarded_with_candidates(
        remote,
        remote_root,
        remote_path,
        bytes,
        hash,
        || fresh_remote_candidate(remote_path),
    )
}

fn stage_remote_write_guarded_with_candidates<R: RemoteWrite>(
    remote: &mut R,
    remote_root: &str,
    remote_path: &str,
    bytes: &[u8],
    hash: &str,
    mut candidate: impl FnMut() -> Result<String>,
) -> Result<StagedRemoteWrite> {
    let intended = IntendedRemoteTempPayload {
        size: bytes.len() as u64,
        sha256: hash,
    };
    for _ in 0..MAX_REMOTE_TEMP_CANDIDATES {
        let temp_path = candidate()?;
        if remote.destination_snapshot(remote_root, &temp_path)?
            != RemoteDestinationSnapshot::Missing
        {
            continue;
        }
        if let Err(error) = remote.upload_bytes(&temp_path, bytes) {
            cleanup_intended_remote_temp(remote, remote_root, &temp_path, intended);
            return Err(error).with_context(|| format!("uploading temp {temp_path}"));
        }
        let temp_snapshot = match remote.destination_snapshot(remote_root, &temp_path) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                cleanup_intended_remote_temp(remote, remote_root, &temp_path, intended);
                return Err(error).with_context(|| format!("capturing remote temp {temp_path:?}"));
            }
        };
        if !intended.matches_snapshot(&temp_snapshot) {
            anyhow::bail!("remote temp changed while staging at {temp_path:?}");
        }
        let modified = match remote.mtime(&temp_path) {
            Ok(modified) => modified,
            Err(error) => {
                cleanup_remote_snapshot(remote, remote_root, &temp_path, &temp_snapshot);
                return Err(error).with_context(|| {
                    format!("fetching exact mtime for remote temp {temp_path:?}")
                });
            }
        };
        return Ok(StagedRemoteWrite {
            temp_path,
            target_path: remote_path.to_string(),
            size: bytes.len() as u64,
            modified,
            sha256: hash.to_string(),
            temp_snapshot,
        });
    }
    anyhow::bail!("unable to reserve a unique remote transfer temp")
}

fn cleanup_intended_remote_temp<R: RemoteWrite>(
    remote: &mut R,
    remote_root: &str,
    temp_path: &str,
    intended: IntendedRemoteTempPayload<'_>,
) {
    let Ok(snapshot) = remote.destination_snapshot(remote_root, temp_path) else {
        return;
    };
    if intended.matches_snapshot(&snapshot) {
        let _ = remote.rm(temp_path);
    }
}

fn cleanup_remote_snapshot<R: RemoteWrite>(
    remote: &mut R,
    remote_root: &str,
    temp_path: &str,
    expected: &RemoteDestinationSnapshot,
) {
    if remote
        .destination_snapshot(remote_root, temp_path)
        .ok()
        .as_ref()
        == Some(expected)
    {
        let _ = remote.rm(temp_path);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn upload_one_guarded<R: RemoteWrite>(
    remote: &mut R,
    state: &mut StateFile,
    rel: &str,
    remote_root: &str,
    remote_path: &str,
    bytes: &[u8],
    hash: &str,
    source: &ExpectedLocalSource,
    destination: &ExpectedRemoteDestination,
    mode: ExecutionMode,
    gate: &dyn CommitGate,
) -> Result<CommitDecision> {
    if mode.is_dry_run() {
        return Ok(CommitDecision::Committed);
    }
    if hash_bytes(bytes) != hash {
        anyhow::bail!("local payload changed before guarded upload {rel}");
    }
    source.verify_unchanged(hash)?;
    verify_remote_destination(remote, remote_root, remote_path, destination)?;
    if !gate.is_current() {
        return Ok(CommitDecision::Cancelled);
    }

    let staged = stage_remote_write_guarded(remote, remote_root, remote_path, bytes, hash)?;
    let mut renamed = false;
    let result = {
        let mut mutation = || {
            source.verify_unchanged(hash)?;
            verify_remote_destination(remote, remote_root, remote_path, destination)?;
            verify_remote_temp(remote, remote_root, &staged)?;
            remote
                .rename(&staged.temp_path, &staged.target_path)
                .with_context(|| {
                    format!("renaming {} -> {}", staged.temp_path, staged.target_path)
                })?;
            record_upload(state, rel, &staged, Utc::now());
            renamed = true;
            Ok(())
        };
        gate.commit(&mut mutation)
    };
    if !renamed {
        cleanup_remote_snapshot(
            remote,
            remote_root,
            &staged.temp_path,
            &staged.temp_snapshot,
        );
    }
    result
}

fn verify_remote_temp<R: RemoteWrite>(
    remote: &mut R,
    remote_root: &str,
    staged: &StagedRemoteWrite,
) -> Result<()> {
    let current = remote.destination_snapshot(remote_root, &staged.temp_path)?;
    if current != staged.temp_snapshot {
        anyhow::bail!(
            "remote temp changed before commit at {:?}",
            staged.temp_path
        );
    }
    let current_mtime = remote.mtime(&staged.temp_path).with_context(|| {
        format!(
            "fetching exact mtime for remote temp {:?}",
            staged.temp_path
        )
    })?;
    if current_mtime != staged.modified {
        anyhow::bail!(
            "remote temp mtime changed before commit at {:?}",
            staged.temp_path
        );
    }
    Ok(())
}

fn verify_remote_destination<R: RemoteWrite>(
    remote: &mut R,
    remote_root: &str,
    remote_path: &str,
    expected: &ExpectedRemoteDestination,
) -> Result<()> {
    let current = remote.destination_snapshot(remote_root, remote_path)?;
    if current != expected.snapshot {
        anyhow::bail!("remote destination changed before commit at {remote_path:?}");
    }
    Ok(())
}

/// Walk the prefix segments of `remote_path` and `mkdir` each one. Tolerates
/// already-existing directories via `Ftp::mkdir`'s built-in idempotency.
fn ensure_remote_parents(ftp: &mut Ftp, remote_path: &str) -> Result<()> {
    // Find the last '/'; everything before it is the directory portion.
    let Some(dir_end) = remote_path.rfind('/') else {
        // No slash, no parent to create.
        return Ok(());
    };
    let dir = &remote_path[..dir_end];
    if dir.is_empty() {
        // File at root, e.g. "/foo.txt" → dir = "" → nothing to create.
        return Ok(());
    }
    // Walk prefixes: e.g. "/a/b/c" → "/a", "/a/b", "/a/b/c".
    // We skip the leading empty segment so absolute paths work.
    let mut segments = dir.split('/').peekable();
    let leading_slash = dir.starts_with('/');
    let mut acc = String::new();
    // Consume the empty leading segment for absolute paths.
    if leading_slash {
        let _ = segments.next();
    }
    for seg in segments {
        if seg.is_empty() {
            // Defensive: skip empty segments from double slashes.
            continue;
        }
        if leading_slash || !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(seg);
        ftp.mkdir(&acc)?;
    }
    Ok(())
}

fn record_upload(
    state: &mut StateFile,
    rel: &str,
    staged: &StagedRemoteWrite,
    last_synced: DateTime<Utc>,
) {
    state.files.insert(
        rel.to_string(),
        FileRecord {
            sha256: staged.sha256.clone(),
            size: staged.size,
            remote_mtime: staged.modified,
            last_synced,
        },
    );
}

#[cfg(test)]
mod staging_tests {
    use super::{
        ExpectedLocalSource, ExpectedRemoteDestination, stage_remote_write,
        stage_remote_write_guarded_with_candidates, upload_one_guarded,
    };
    use crate::commands::ExecutionMode;
    use crate::commands::file_transfer::{RemoteDestinationSnapshot, RemoteWrite};
    use crate::commands::sync::commit::{CommitDecision, CommitGate};
    use crate::hash::hash_bytes;
    use crate::state::{FileRecord, StateFile};
    use anyhow::Result;
    use chrono::{DateTime, TimeZone, Utc};
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn mtime(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 10, 10, 0, second).unwrap()
    }

    fn file_snapshot(bytes: &[u8], modified: DateTime<Utc>) -> RemoteDestinationSnapshot {
        RemoteDestinationSnapshot::File {
            size: bytes.len() as u64,
            modified,
            sha256: hash_bytes(bytes),
        }
    }

    fn transfer_temp_paths(remote: &FakeRemote) -> Vec<String> {
        remote
            .files
            .keys()
            .filter(|path| crate::commands::transfer_temp::is_reserved_transfer_temp(path))
            .cloned()
            .collect()
    }

    fn transfer_temp_events(events: &[String], operation: &str) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| event.strip_prefix(operation))
            .filter(|path| crate::commands::transfer_temp::is_reserved_transfer_temp(path))
            .map(str::to_string)
            .collect()
    }

    fn transfer_candidate(nonce: &str) -> String {
        crate::commands::transfer_temp::remote_candidate("/remote/page.txt", nonce).unwrap()
    }

    #[derive(Default)]
    struct FakeRemote {
        directories: BTreeSet<String>,
        files: BTreeMap<String, Vec<u8>>,
        mtimes: BTreeMap<String, DateTime<Utc>>,
        list_mtimes: BTreeMap<String, DateTime<Utc>>,
        snapshots: BTreeMap<String, VecDeque<Result<RemoteDestinationSnapshot>>>,
        temp_mtime_results: VecDeque<Result<DateTime<Utc>>>,
        temp_snapshot_count: usize,
        replace_temp_on_snapshot: Option<(usize, Vec<u8>)>,
        replace_temp_on_upload_error: Option<Vec<u8>>,
        events: Vec<String>,
        upload_error: bool,
        mtime_error: bool,
        rename_error: bool,
        tolerant_mkdir_calls: usize,
        strict_mkdir_calls: usize,
        strict_mkdir_error: bool,
    }

    impl FakeRemote {
        fn with_remote_root() -> Self {
            Self {
                directories: BTreeSet::from(["/remote".to_string()]),
                ..Self::default()
            }
        }

        fn put_existing(&mut self, path: &str, bytes: &[u8], modified: DateTime<Utc>) {
            self.files.insert(path.to_string(), bytes.to_vec());
            self.list_mtimes.insert(path.to_string(), modified);
            self.mtimes.insert(path.to_string(), modified);
        }

        fn script_snapshots(
            &mut self,
            path: &str,
            snapshots: impl IntoIterator<Item = RemoteDestinationSnapshot>,
        ) {
            self.snapshots
                .insert(path.to_string(), snapshots.into_iter().map(Ok).collect());
        }

        fn replace_temp_on_snapshot(&mut self, count: usize, bytes: &[u8]) {
            self.replace_temp_on_snapshot = Some((count, bytes.to_vec()));
        }

        fn parent(path: &str) -> &str {
            match path.rsplit_once('/') {
                Some(("", _)) => "/",
                Some((parent, _)) => parent,
                None => "",
            }
        }
    }

    impl RemoteWrite for FakeRemote {
        fn upload_bytes(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
            self.events.push(format!("upload {path}"));
            if !self.directories.contains(Self::parent(path)) {
                anyhow::bail!("missing remote parent");
            }
            self.files.insert(path.to_string(), bytes.to_vec());
            self.list_mtimes.insert(path.to_string(), mtime(40));
            self.mtimes.insert(path.to_string(), mtime(40));
            if self.upload_error {
                if let Some(replacement) = self.replace_temp_on_upload_error.clone() {
                    self.files.insert(path.to_string(), replacement);
                    self.list_mtimes.insert(path.to_string(), mtime(58));
                    self.mtimes.insert(path.to_string(), mtime(58));
                }
                anyhow::bail!("scripted partial upload failure");
            }
            Ok(())
        }

        fn rename(&mut self, from: &str, to: &str) -> Result<()> {
            self.events.push(format!("rename {from} {to}"));
            if self.rename_error {
                anyhow::bail!("scripted rename failure");
            }
            let bytes = self
                .files
                .remove(from)
                .ok_or_else(|| anyhow::anyhow!("missing rename source"))?;
            let list_modified = self
                .list_mtimes
                .remove(from)
                .ok_or_else(|| anyhow::anyhow!("missing rename source LIST mtime"))?;
            let modified = self
                .mtimes
                .remove(from)
                .ok_or_else(|| anyhow::anyhow!("missing rename source mtime"))?;
            self.files.insert(to.to_string(), bytes);
            self.mtimes.insert(to.to_string(), modified);
            self.list_mtimes.insert(to.to_string(), list_modified);
            Ok(())
        }

        fn rm(&mut self, path: &str) -> Result<()> {
            self.events.push(format!("rm {path}"));
            self.files.remove(path);
            self.mtimes.remove(path);
            self.list_mtimes.remove(path);
            Ok(())
        }

        fn mkdir(&mut self, path: &str) -> Result<()> {
            self.tolerant_mkdir_calls += 1;
            self.directories.insert(path.to_string());
            Ok(())
        }

        fn mkdir_scoped_strict(&mut self, path: &str) -> Result<()> {
            self.strict_mkdir_calls += 1;
            if self.strict_mkdir_error {
                anyhow::bail!("scripted strict MKD failure");
            }
            self.directories.insert(path.to_string());
            Ok(())
        }

        fn mtime(&mut self, path: &str) -> Result<DateTime<Utc>> {
            self.events.push(format!("mtime {path}"));
            if crate::commands::transfer_temp::is_reserved_transfer_temp(path)
                && let Some(result) = self.temp_mtime_results.pop_front()
            {
                return result;
            }
            if self.mtime_error {
                anyhow::bail!("scripted mtime failure");
            }
            self.mtimes
                .get(path)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("missing mtime"))
        }

        fn destination_snapshot(
            &mut self,
            _remote_root: &str,
            path: &str,
        ) -> Result<RemoteDestinationSnapshot> {
            self.events.push(format!("snapshot {path}"));
            if crate::commands::transfer_temp::is_reserved_transfer_temp(path) {
                self.temp_snapshot_count += 1;
                let replacement = self
                    .replace_temp_on_snapshot
                    .as_ref()
                    .filter(|(count, _)| *count == self.temp_snapshot_count)
                    .map(|(_, bytes)| bytes.clone());
                if let Some(bytes) = replacement {
                    self.files.insert(path.to_string(), bytes);
                    self.list_mtimes.insert(path.to_string(), mtime(59));
                    self.mtimes.insert(path.to_string(), mtime(59));
                }
            }
            if let Some(script) = self.snapshots.get_mut(path)
                && let Some(snapshot) = script.pop_front()
            {
                return snapshot;
            }
            if !self.directories.contains(Self::parent(path)) {
                anyhow::bail!("missing remote parent");
            }
            if self.directories.contains(path) {
                return Ok(RemoteDestinationSnapshot::Directory);
            }
            match self.files.get(path) {
                Some(bytes) => Ok(file_snapshot(
                    bytes,
                    *self.list_mtimes.get(path).expect("file LIST mtime"),
                )),
                None => Ok(RemoteDestinationSnapshot::Missing),
            }
        }
    }

    struct CancelAtPreStage {
        checks: AtomicUsize,
        commits: AtomicUsize,
    }

    impl CommitGate for CancelAtPreStage {
        fn is_current(&self) -> bool {
            self.checks.fetch_add(1, Ordering::SeqCst) == 0
        }

        fn commit(&self, _mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision> {
            self.commits.fetch_add(1, Ordering::SeqCst);
            Ok(CommitDecision::Cancelled)
        }
    }

    struct CancelAfterUpload {
        called: AtomicBool,
    }

    impl CommitGate for CancelAfterUpload {
        fn is_current(&self) -> bool {
            true
        }

        fn commit(&self, _mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision> {
            self.called.store(true, Ordering::SeqCst);
            Ok(CommitDecision::Cancelled)
        }
    }

    struct HookGate {
        hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl HookGate {
        fn before(hook: impl FnOnce() + Send + 'static) -> Self {
            Self {
                hook: Mutex::new(Some(Box::new(hook))),
            }
        }
    }

    impl CommitGate for HookGate {
        fn is_current(&self) -> bool {
            true
        }

        fn commit(&self, mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision> {
            self.hook.lock().unwrap().take().unwrap()();
            mutation()?;
            Ok(CommitDecision::Committed)
        }
    }

    struct CommitGateNow;

    impl CommitGate for CommitGateNow {
        fn is_current(&self) -> bool {
            true
        }

        fn commit(&self, mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision> {
            mutation()?;
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

    fn source(root: &std::path::Path, bytes: &[u8]) -> (PathBuf, ExpectedLocalSource) {
        let path = root.join("page.txt");
        std::fs::write(&path, bytes).unwrap();
        let expected = ExpectedLocalSource::capture(root, &path).unwrap();
        (path, expected)
    }

    fn upload(
        remote: &mut FakeRemote,
        state: &mut StateFile,
        bytes: &[u8],
        source: &ExpectedLocalSource,
        destination: &ExpectedRemoteDestination,
        gate: &dyn CommitGate,
    ) -> Result<CommitDecision> {
        upload_one_guarded(
            remote,
            state,
            "page.txt",
            "/remote",
            "/remote/page.txt",
            bytes,
            &hash_bytes(bytes),
            source,
            destination,
            ExecutionMode::Apply,
            gate,
        )
    }

    #[test]
    fn guarded_upload_dry_run_commits_without_remote_or_state_mutation() {
        let root = tempfile::tempdir().unwrap();
        let bytes = b"new local";
        let (_path, source) = source(root.path(), bytes);
        let destination = ExpectedRemoteDestination {
            snapshot: RemoteDestinationSnapshot::Missing,
        };
        let mut remote = FakeRemote::with_remote_root();
        let mut state = StateFile::default();

        let decision = upload_one_guarded(
            &mut remote,
            &mut state,
            "page.txt",
            "/remote",
            "/remote/page.txt",
            bytes,
            "deliberately unchecked in dry run",
            &source,
            &destination,
            ExecutionMode::DryRun,
            &PanicGate,
        )
        .unwrap();

        assert_eq!(decision, CommitDecision::Committed);
        assert!(remote.events.is_empty());
        assert!(remote.files.is_empty());
        assert!(state.files.is_empty());
    }

    #[test]
    fn guarded_upload_cancels_at_pre_stage_boundary_without_creating_a_temp() {
        let root = tempfile::tempdir().unwrap();
        let bytes = b"new local";
        let (_path, source) = source(root.path(), bytes);
        let destination = ExpectedRemoteDestination {
            snapshot: RemoteDestinationSnapshot::Missing,
        };
        let mut remote = FakeRemote::with_remote_root();
        let mut state = StateFile::default();
        let gate = CancelAtPreStage {
            checks: AtomicUsize::new(0),
            commits: AtomicUsize::new(0),
        };

        assert!(gate.is_current(), "models the outer scoped-plan check");
        let decision =
            upload(&mut remote, &mut state, bytes, &source, &destination, &gate).unwrap();

        assert_eq!(decision, CommitDecision::Cancelled);
        assert_eq!(gate.commits.load(Ordering::SeqCst), 0);
        assert!(
            !remote
                .events
                .iter()
                .any(|event| event.starts_with("upload "))
        );
        assert!(transfer_temp_paths(&remote).is_empty());
        assert!(state.files.is_empty());
    }

    #[test]
    fn remote_staging_gives_interleaved_writers_distinct_owned_temps() {
        let mut remote = FakeRemote::with_remote_root();

        let first = stage_remote_write(
            &mut remote,
            "/remote/page.txt",
            b"first writer",
            &hash_bytes(b"first writer"),
        )
        .unwrap();
        let second = stage_remote_write(
            &mut remote,
            "/remote/page.txt",
            b"second writer",
            &hash_bytes(b"second writer"),
        )
        .unwrap();

        assert_ne!(first.temp_path, second.temp_path);
        assert!(crate::commands::transfer_temp::is_reserved_transfer_temp(
            &first.temp_path
        ));
        assert!(crate::commands::transfer_temp::is_reserved_transfer_temp(
            &second.temp_path
        ));
        assert_eq!(remote.files[&first.temp_path], b"first writer");
        assert_eq!(remote.files[&second.temp_path], b"second writer");

        remote.rm(&first.temp_path).unwrap();
        assert!(!remote.files.contains_key(&first.temp_path));
        assert_eq!(remote.files[&second.temp_path], b"second writer");
    }

    #[test]
    fn remote_temp_upload_error_attempts_cleanup() {
        let mut remote = FakeRemote {
            upload_error: true,
            ..FakeRemote::with_remote_root()
        };
        let error = stage_remote_write(
            &mut remote,
            "/remote/page.txt",
            b"partial local",
            &hash_bytes(b"partial local"),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("upload"));
        assert!(transfer_temp_paths(&remote).is_empty());
        assert_eq!(transfer_temp_events(&remote.events, "rm ").len(), 1);
    }

    #[test]
    fn remote_temp_mtime_error_attempts_cleanup() {
        let mut remote = FakeRemote {
            mtime_error: true,
            ..FakeRemote::with_remote_root()
        };
        let error = stage_remote_write(
            &mut remote,
            "/remote/page.txt",
            b"new local",
            &hash_bytes(b"new local"),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("mtime"));
        assert!(transfer_temp_paths(&remote).is_empty());
        assert_eq!(transfer_temp_events(&remote.events, "rm ").len(), 1);
    }

    #[test]
    fn guarded_upload_cancellation_removes_temp_without_renaming_or_updating_state() {
        let root = tempfile::tempdir().unwrap();
        let bytes = b"new local";
        let (_path, source) = source(root.path(), bytes);
        let old_remote = b"old remote";
        let old_snapshot = file_snapshot(old_remote, mtime(1));
        let destination = ExpectedRemoteDestination {
            snapshot: old_snapshot,
        };
        let mut remote = FakeRemote::with_remote_root();
        remote.put_existing("/remote/page.txt", old_remote, mtime(1));
        let mut state = StateFile::default();
        let original_record = FileRecord {
            sha256: hash_bytes(b"old state"),
            size: 9,
            remote_mtime: mtime(0),
            last_synced: mtime(0),
        };
        state
            .files
            .insert("page.txt".into(), original_record.clone());
        let gate = CancelAfterUpload {
            called: AtomicBool::new(false),
        };

        let decision =
            upload(&mut remote, &mut state, bytes, &source, &destination, &gate).unwrap();

        assert_eq!(decision, CommitDecision::Cancelled);
        assert!(gate.called.load(Ordering::SeqCst));
        assert_eq!(remote.files["/remote/page.txt"], old_remote);
        assert!(transfer_temp_paths(&remote).is_empty());
        assert!(
            !remote
                .events
                .iter()
                .any(|event| event.starts_with("rename "))
        );
        assert_eq!(transfer_temp_events(&remote.events, "rm ").len(), 1);
        assert_eq!(state.files.get("page.txt"), Some(&original_record));
        assert_eq!(remote.tolerant_mkdir_calls, 0);
        assert_eq!(remote.strict_mkdir_calls, 0);
    }

    #[test]
    fn guarded_upload_missing_parent_fails_without_upload_or_directory_creation() {
        let root = tempfile::tempdir().unwrap();
        let bytes = b"new local";
        let (_path, source) = source(root.path(), bytes);
        let destination = ExpectedRemoteDestination {
            snapshot: RemoteDestinationSnapshot::Missing,
        };
        let mut remote = FakeRemote::default();
        let mut state = StateFile::default();

        let error = upload(
            &mut remote,
            &mut state,
            bytes,
            &source,
            &destination,
            &CommitGateNow,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("parent"));
        assert!(
            !remote
                .events
                .iter()
                .any(|event| event.starts_with("upload "))
        );
        assert_eq!(remote.tolerant_mkdir_calls, 0);
        assert_eq!(remote.strict_mkdir_calls, 0);
        assert!(state.files.is_empty());
    }

    #[test]
    fn guarded_upload_leaves_a_preexisting_legacy_temp_file_untouched() {
        let root = tempfile::tempdir().unwrap();
        let bytes = b"new local";
        let (_path, source) = source(root.path(), bytes);
        let destination = ExpectedRemoteDestination {
            snapshot: RemoteDestinationSnapshot::Missing,
        };
        let mut remote = FakeRemote::with_remote_root();
        let legacy_temp = "/remote/page.txt.tmp.zedftp";
        remote.put_existing(legacy_temp, b"other writer", mtime(2));
        let mut state = StateFile::default();

        let decision = upload(
            &mut remote,
            &mut state,
            bytes,
            &source,
            &destination,
            &CommitGateNow,
        )
        .unwrap();

        assert_eq!(decision, CommitDecision::Committed);
        assert_eq!(remote.files[legacy_temp], b"other writer");
        assert_eq!(remote.files["/remote/page.txt"], bytes);
        assert_eq!(transfer_temp_events(&remote.events, "upload ").len(), 1);
        assert_eq!(state.files["page.txt"].sha256, hash_bytes(bytes));
    }

    #[test]
    fn guarded_upload_rename_error_removes_temp_without_updating_state() {
        let root = tempfile::tempdir().unwrap();
        let bytes = b"new local";
        let (_path, source) = source(root.path(), bytes);
        let destination = ExpectedRemoteDestination {
            snapshot: RemoteDestinationSnapshot::Missing,
        };
        let mut remote = FakeRemote::with_remote_root();
        remote.rename_error = true;
        let mut state = StateFile::default();

        let error = upload(
            &mut remote,
            &mut state,
            bytes,
            &source,
            &destination,
            &CommitGateNow,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("rename"));
        assert!(!remote.files.contains_key("/remote/page.txt"));
        assert!(transfer_temp_paths(&remote).is_empty());
        assert!(state.files.is_empty());
    }

    #[test]
    fn guarded_upload_records_temp_metadata_without_post_rename_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let bytes = b"new local";
        let (_path, source) = source(root.path(), bytes);
        let destination = ExpectedRemoteDestination {
            snapshot: RemoteDestinationSnapshot::Missing,
        };
        let mut remote = FakeRemote::with_remote_root();
        remote.temp_mtime_results = VecDeque::from([Ok(mtime(41)), Ok(mtime(41))]);
        let mut state = StateFile::default();
        let claimed_at = Arc::new(Mutex::new(None));
        let claimed_at_for_gate = Arc::clone(&claimed_at);
        let gate = HookGate::before(move || {
            *claimed_at_for_gate.lock().unwrap() = Some(Utc::now());
        });

        let decision =
            upload(&mut remote, &mut state, bytes, &source, &destination, &gate).unwrap();

        assert_eq!(decision, CommitDecision::Committed);
        assert_eq!(remote.files["/remote/page.txt"], bytes);
        assert_eq!(state.files["page.txt"].remote_mtime, mtime(41));
        assert!(state.files["page.txt"].last_synced >= claimed_at.lock().unwrap().unwrap());
        let temp_snapshot_indices: Vec<_> = remote
            .events
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                event
                    .strip_prefix("snapshot ")
                    .is_some_and(crate::commands::transfer_temp::is_reserved_transfer_temp)
            })
            .map(|(index, _)| index)
            .collect();
        let mtime_indices: Vec<_> = remote
            .events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.starts_with("mtime "))
            .map(|(index, _)| index)
            .collect();
        let rename_index = remote
            .events
            .iter()
            .position(|event| event.starts_with("rename "))
            .unwrap();
        assert_eq!(temp_snapshot_indices.len(), 3);
        assert_eq!(mtime_indices.len(), 2);
        assert!(temp_snapshot_indices[1] < mtime_indices[0]);
        assert!(mtime_indices[0] < temp_snapshot_indices[2]);
        assert!(temp_snapshot_indices[2] < mtime_indices[1]);
        assert!(mtime_indices[1] < rename_index);
        assert!(
            remote.events[rename_index + 1..]
                .iter()
                .all(|event| !event.starts_with("snapshot ") && !event.starts_with("mtime "))
        );
    }

    #[test]
    fn guarded_remote_staging_preserves_replacement_seen_by_first_capture() {
        let mut remote = FakeRemote::with_remote_root();
        let candidate = transfer_candidate("33333333333333333333333333333333");
        remote.replace_temp_on_snapshot(2, b"foreign replacement");

        let error = stage_remote_write_guarded_with_candidates(
            &mut remote,
            "/remote",
            "/remote/page.txt",
            b"new local",
            &hash_bytes(b"new local"),
            || Ok(candidate.clone()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("remote temp changed"));
        assert_eq!(remote.files[&candidate], b"foreign replacement");
        assert!(transfer_temp_events(&remote.events, "rm ").is_empty());
    }

    #[test]
    fn guarded_remote_staging_upload_error_preserves_foreign_replacement() {
        let mut remote = FakeRemote {
            upload_error: true,
            replace_temp_on_upload_error: Some(b"foreign replacement".to_vec()),
            ..FakeRemote::with_remote_root()
        };
        let candidate = transfer_candidate("44444444444444444444444444444444");

        let error = stage_remote_write_guarded_with_candidates(
            &mut remote,
            "/remote",
            "/remote/page.txt",
            b"new local",
            &hash_bytes(b"new local"),
            || Ok(candidate.clone()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("upload"));
        assert_eq!(remote.files[&candidate], b"foreign replacement");
        assert!(transfer_temp_events(&remote.events, "rm ").is_empty());
    }

    #[test]
    fn guarded_remote_staging_capture_error_preserves_unprovable_replacement() {
        let mut remote = FakeRemote::with_remote_root();
        let candidate = transfer_candidate("55555555555555555555555555555555");
        remote.snapshots.insert(
            candidate.clone(),
            VecDeque::from([
                Ok(RemoteDestinationSnapshot::Missing),
                Err(anyhow::anyhow!("scripted temp snapshot failure")),
            ]),
        );
        remote.replace_temp_on_snapshot(3, b"foreign replacement");

        let error = stage_remote_write_guarded_with_candidates(
            &mut remote,
            "/remote",
            "/remote/page.txt",
            b"new local",
            &hash_bytes(b"new local"),
            || Ok(candidate.clone()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("capturing remote temp"));
        assert_eq!(remote.files[&candidate], b"foreign replacement");
        assert!(transfer_temp_events(&remote.events, "rm ").is_empty());
        assert_eq!(transfer_temp_events(&remote.events, "snapshot ").len(), 3);
    }

    #[test]
    fn guarded_remote_staging_upload_error_cleans_only_after_proving_intended_payload() {
        let mut remote = FakeRemote {
            upload_error: true,
            ..FakeRemote::with_remote_root()
        };
        let candidate = transfer_candidate("66666666666666666666666666666666");

        let error = stage_remote_write_guarded_with_candidates(
            &mut remote,
            "/remote",
            "/remote/page.txt",
            b"new local",
            &hash_bytes(b"new local"),
            || Ok(candidate.clone()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("upload"));
        assert!(!remote.files.contains_key(&candidate));
        assert_eq!(transfer_temp_events(&remote.events, "snapshot ").len(), 2);
        assert_eq!(transfer_temp_events(&remote.events, "rm "), [candidate]);
    }

    #[test]
    fn guarded_remote_staging_capture_error_cleans_after_retry_proves_intended_payload() {
        let mut remote = FakeRemote::with_remote_root();
        let candidate = transfer_candidate("22222222222222222222222222222222");
        remote.snapshots.insert(
            candidate.clone(),
            VecDeque::from([
                Ok(RemoteDestinationSnapshot::Missing),
                Err(anyhow::anyhow!("scripted temp snapshot failure")),
            ]),
        );

        let error = stage_remote_write_guarded_with_candidates(
            &mut remote,
            "/remote",
            "/remote/page.txt",
            b"new local",
            &hash_bytes(b"new local"),
            || Ok(candidate.clone()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("capturing remote temp"));
        assert!(!remote.files.contains_key(&candidate));
        assert_eq!(transfer_temp_events(&remote.events, "snapshot ").len(), 3);
        assert_eq!(transfer_temp_events(&remote.events, "rm "), [candidate]);
    }

    #[test]
    fn guarded_remote_staging_retries_an_occupied_unique_candidate() {
        let mut remote = FakeRemote::with_remote_root();
        let occupied = crate::commands::transfer_temp::remote_candidate(
            "/remote/page.txt",
            "00000000000000000000000000000000",
        )
        .unwrap();
        let fresh = crate::commands::transfer_temp::remote_candidate(
            "/remote/page.txt",
            "11111111111111111111111111111111",
        )
        .unwrap();
        remote.put_existing(&occupied, b"foreign temp", mtime(2));
        let mut candidates = VecDeque::from([occupied.clone(), fresh.clone()]);

        let staged = stage_remote_write_guarded_with_candidates(
            &mut remote,
            "/remote",
            "/remote/page.txt",
            b"new local",
            &hash_bytes(b"new local"),
            || Ok(candidates.pop_front().expect("candidate")),
        )
        .unwrap();

        assert_eq!(staged.temp_path, fresh);
        assert_eq!(remote.files[&occupied], b"foreign temp");
        assert_eq!(remote.files[&staged.temp_path], b"new local");
        remote.rm(&staged.temp_path).unwrap();
        assert_eq!(remote.files[&occupied], b"foreign temp");
    }

    #[test]
    fn guarded_remote_staging_mdtm_error_cleans_the_exact_staged_snapshot() {
        let mut remote = FakeRemote::with_remote_root();
        remote.temp_mtime_results =
            VecDeque::from([Err(anyhow::anyhow!("scripted staging MDTM failure"))]);
        let candidate = transfer_candidate("77777777777777777777777777777777");

        let error = stage_remote_write_guarded_with_candidates(
            &mut remote,
            "/remote",
            "/remote/page.txt",
            b"new local",
            &hash_bytes(b"new local"),
            || Ok(candidate.clone()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("exact mtime"));
        assert!(!remote.files.contains_key(&candidate));
        assert_eq!(transfer_temp_events(&remote.events, "snapshot ").len(), 3);
        assert_eq!(transfer_temp_events(&remote.events, "rm "), [candidate]);
    }

    #[test]
    fn guarded_remote_staging_mdtm_error_preserves_a_replacement() {
        let mut remote = FakeRemote::with_remote_root();
        remote.temp_mtime_results =
            VecDeque::from([Err(anyhow::anyhow!("scripted staging MDTM failure"))]);
        let candidate = transfer_candidate("88888888888888888888888888888888");
        remote.replace_temp_on_snapshot(3, b"foreign replacement");

        let error = stage_remote_write_guarded_with_candidates(
            &mut remote,
            "/remote",
            "/remote/page.txt",
            b"new local",
            &hash_bytes(b"new local"),
            || Ok(candidate.clone()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("exact mtime"));
        assert_eq!(remote.files[&candidate], b"foreign replacement");
        assert_eq!(transfer_temp_events(&remote.events, "snapshot ").len(), 3);
        assert!(transfer_temp_events(&remote.events, "rm ").is_empty());
    }

    #[test]
    fn guarded_upload_rejects_claim_time_exact_mtime_change() {
        let root = tempfile::tempdir().unwrap();
        let bytes = b"new local";
        let (_path, source) = source(root.path(), bytes);
        let destination = ExpectedRemoteDestination {
            snapshot: RemoteDestinationSnapshot::Missing,
        };
        let mut remote = FakeRemote::with_remote_root();
        remote.temp_mtime_results = VecDeque::from([Ok(mtime(41)), Ok(mtime(42))]);
        let mut state = StateFile::default();

        let error = upload(
            &mut remote,
            &mut state,
            bytes,
            &source,
            &destination,
            &CommitGateNow,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("remote temp mtime changed"));
        assert!(!remote.files.contains_key("/remote/page.txt"));
        assert!(transfer_temp_paths(&remote).is_empty());
        assert!(state.files.is_empty());
        assert!(
            !remote
                .events
                .iter()
                .any(|event| event.starts_with("rename "))
        );
    }

    #[test]
    fn guarded_upload_rejects_claim_time_exact_mtime_error() {
        let root = tempfile::tempdir().unwrap();
        let bytes = b"new local";
        let (_path, source) = source(root.path(), bytes);
        let destination = ExpectedRemoteDestination {
            snapshot: RemoteDestinationSnapshot::Missing,
        };
        let mut remote = FakeRemote::with_remote_root();
        remote.temp_mtime_results = VecDeque::from([
            Ok(mtime(41)),
            Err(anyhow::anyhow!("scripted claim MDTM failure")),
        ]);
        let mut state = StateFile::default();

        let error = upload(
            &mut remote,
            &mut state,
            bytes,
            &source,
            &destination,
            &CommitGateNow,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("exact mtime"));
        assert!(!remote.files.contains_key("/remote/page.txt"));
        assert!(transfer_temp_paths(&remote).is_empty());
        assert!(state.files.is_empty());
        assert!(
            !remote
                .events
                .iter()
                .any(|event| event.starts_with("rename "))
        );
    }

    #[test]
    fn guarded_upload_rejects_claim_time_temp_replacement_without_cleaning_it() {
        let root = tempfile::tempdir().unwrap();
        let bytes = b"new local";
        let (_path, source) = source(root.path(), bytes);
        let destination = ExpectedRemoteDestination {
            snapshot: RemoteDestinationSnapshot::Missing,
        };
        let mut remote = FakeRemote::with_remote_root();
        remote.replace_temp_on_snapshot(3, b"foreign replacement");
        let mut state = StateFile::default();

        let error = upload(
            &mut remote,
            &mut state,
            bytes,
            &source,
            &destination,
            &CommitGateNow,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("remote temp changed"));
        assert!(!remote.files.contains_key("/remote/page.txt"));
        assert!(state.files.is_empty());
        let temps = transfer_temp_paths(&remote);
        assert_eq!(temps.len(), 1);
        assert_eq!(remote.files[&temps[0]], b"foreign replacement");
        assert!(transfer_temp_events(&remote.events, "rm ").is_empty());
        remote.rm(&temps[0]).unwrap();
    }

    fn assert_source_change_rejected(mutate: impl FnOnce(&std::path::Path) + Send + 'static) {
        let root = tempfile::tempdir().unwrap();
        let bytes = b"new local";
        let (path, source) = source(root.path(), bytes);
        let destination = ExpectedRemoteDestination {
            snapshot: RemoteDestinationSnapshot::Missing,
        };
        let changed_path = path.clone();
        let gate = HookGate::before(move || mutate(&changed_path));
        let mut remote = FakeRemote::with_remote_root();
        let mut state = StateFile::default();

        let error =
            upload(&mut remote, &mut state, bytes, &source, &destination, &gate).unwrap_err();

        assert!(format!("{error:#}").contains("local source changed"));
        assert!(!remote.files.contains_key("/remote/page.txt"));
        assert!(transfer_temp_paths(&remote).is_empty());
        assert!(state.files.is_empty());
    }

    #[test]
    fn guarded_upload_rejects_source_disappearance_inside_claim() {
        assert_source_change_rejected(|path| std::fs::remove_file(path).unwrap());
    }

    #[test]
    fn guarded_upload_rejects_source_type_change_inside_claim() {
        assert_source_change_rejected(|path| {
            std::fs::remove_file(path).unwrap();
            std::fs::create_dir(path).unwrap();
        });
    }

    #[test]
    fn guarded_upload_rejects_source_identity_change_even_with_same_bytes() {
        assert_source_change_rejected(|path| {
            std::fs::remove_file(path).unwrap();
            std::fs::write(path, b"new local").unwrap();
        });
    }

    #[test]
    fn guarded_upload_rejects_source_hash_change_inside_claim() {
        assert_source_change_rejected(|path| std::fs::write(path, b"edited local").unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn guarded_upload_rejects_source_symlink_change_inside_claim() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let bytes = b"new local";
        let (path, source) = source(root.path(), bytes);
        let linked = root.path().join("linked.txt");
        std::fs::write(&linked, bytes).unwrap();
        let changed_path = path.clone();
        let gate = HookGate::before(move || {
            std::fs::remove_file(&changed_path).unwrap();
            symlink(&linked, &changed_path).unwrap();
        });
        let destination = ExpectedRemoteDestination {
            snapshot: RemoteDestinationSnapshot::Missing,
        };
        let mut remote = FakeRemote::with_remote_root();
        let mut state = StateFile::default();

        let error =
            upload(&mut remote, &mut state, bytes, &source, &destination, &gate).unwrap_err();

        assert!(format!("{error:#}").contains("local source changed"));
        assert!(!remote.files.contains_key("/remote/page.txt"));
        assert!(state.files.is_empty());
    }

    #[test]
    fn guarded_upload_rejects_every_remote_destination_snapshot_change() {
        let original = file_snapshot(b"old remote", mtime(1));
        let changes = [
            RemoteDestinationSnapshot::Missing,
            RemoteDestinationSnapshot::Directory,
            file_snapshot(b"different", mtime(1)),
            file_snapshot(b"old remote", mtime(2)),
            file_snapshot(b"different size", mtime(1)),
        ];

        for changed in changes {
            let root = tempfile::tempdir().unwrap();
            let bytes = b"new local";
            let (_path, source) = source(root.path(), bytes);
            let destination = ExpectedRemoteDestination {
                snapshot: original.clone(),
            };
            let mut remote = FakeRemote::with_remote_root();
            remote.put_existing("/remote/page.txt", b"old remote", mtime(1));
            remote.script_snapshots("/remote/page.txt", [original.clone(), changed]);
            let mut state = StateFile::default();

            let error = upload(
                &mut remote,
                &mut state,
                bytes,
                &source,
                &destination,
                &CommitGateNow,
            )
            .unwrap_err();

            assert!(format!("{error:#}").contains("remote destination changed"));
            assert_eq!(remote.files["/remote/page.txt"], b"old remote");
            assert!(transfer_temp_paths(&remote).is_empty());
            assert!(state.files.is_empty());
        }
    }

    #[test]
    fn guarded_upload_rejects_destination_appearance() {
        let root = tempfile::tempdir().unwrap();
        let bytes = b"new local";
        let (_path, source) = source(root.path(), bytes);
        let destination = ExpectedRemoteDestination {
            snapshot: RemoteDestinationSnapshot::Missing,
        };
        let mut remote = FakeRemote::with_remote_root();
        remote.script_snapshots(
            "/remote/page.txt",
            [
                RemoteDestinationSnapshot::Missing,
                file_snapshot(b"appeared", mtime(3)),
            ],
        );
        let mut state = StateFile::default();

        let error = upload(
            &mut remote,
            &mut state,
            bytes,
            &source,
            &destination,
            &CommitGateNow,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("remote destination changed"));
        assert!(
            !remote
                .events
                .iter()
                .any(|event| event.starts_with("rename "))
        );
        assert!(state.files.is_empty());
    }

    #[test]
    fn strict_mkdir_error_is_not_reinterpreted_through_tolerant_mkdir() {
        let mut remote = FakeRemote {
            strict_mkdir_error: true,
            ..FakeRemote::default()
        };

        let error = RemoteWrite::mkdir_scoped_strict(&mut remote, "/remote/new").unwrap_err();

        assert!(format!("{error:#}").contains("strict MKD failure"));
        assert_eq!(remote.strict_mkdir_calls, 1);
        assert_eq!(remote.tolerant_mkdir_calls, 0);
    }
}
