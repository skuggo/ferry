use super::{record_download, stage_local_write};
use crate::commands::file_transfer::{
    ProbedRemote, RemotePresence, TransferOutcome, TransferStatus, probe_remote, probe_remote_file,
};
use crate::commands::remote_hash::{self, RemoteFileRetrieval, RemoteHash};
use crate::commands::walk::{remote_join, safe_rel};
use crate::commands::{ExecutionMode, state_path_for};
use crate::config::Config;
use crate::ftp::Ftp;
#[cfg(test)]
use crate::hash::hash_bytes;
use crate::state::{FileState, StateFile, classify};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalIdentity {
    Missing,
    Present(String),
}

impl LocalIdentity {
    pub fn capture(path: &Path) -> Result<Self> {
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => Ok(Self::Present(crate::hash::hash_file(path)?)),
            Ok(_) => anyhow::bail!("{} is not a regular file", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::Missing),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn capture_for_apply(path: &Path, rel: &str) -> Result<Self> {
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => Ok(Self::Present(crate::hash::hash_file(path)?)),
            Ok(_) => Err(local_identity_conflict(rel)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::Missing),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RemoteFile {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub size: u64,
    pub mtime: DateTime<Utc>,
}

#[derive(Clone, Debug)]
enum PreparedAction {
    Noop(TransferStatus),
    /// Local and remote are byte-identical but untracked: record the state
    /// entry without touching the file.
    Adopt {
        sha256: String,
        size: u64,
        mtime: DateTime<Utc>,
    },
    Install(RemoteFile),
}

#[derive(Clone, Debug)]
pub struct PreparedPull {
    config_path: PathBuf,
    local_root: PathBuf,
    local_path: PathBuf,
    relative_path: String,
    expected_local: LocalIdentity,
    action: PreparedAction,
}

pub fn fetch_remote_one(config_path: &Path, rel: &str) -> Result<RemoteFile> {
    let rel = safe_rel(rel).with_context(|| format!("pull {rel}"))?;
    (|| {
        let cfg = Config::load(config_path)?;
        let mut ftp = connect(&cfg)?;
        let remote_path = remote_join(&cfg.paths.remote_root, &rel);
        if probe_remote_file(&mut ftp, &remote_path)? == RemotePresence::Missing {
            anyhow::bail!("remote has no {rel}");
        }

        retrieve_remote_file(&mut ftp, &remote_path)
    })()
    .with_context(|| format!("pull {rel}"))
}

pub fn prepare_pull_one(config_path: &Path, rel: &str, force: bool) -> Result<PreparedPull> {
    prepare_pull_one_on(None, None, config_path, rel, force)
}

/// [`prepare_pull_one`] on an already open connection (`Some`), or on a new
/// one opened here (`None`). The hook helper uses the first form to keep one
/// logged-in session across requests.
fn prepare_pull_one_on(
    session: Option<&mut Ftp>,
    held_state: Option<&mut StateFile>,
    config_path: &Path,
    rel: &str,
    force: bool,
) -> Result<PreparedPull> {
    let rel = safe_rel(rel).with_context(|| format!("pull {rel}"))?;
    let mut session = session;
    let mut held_state = held_state;
    (|| {
        let cfg = Config::load(config_path)?;
        let local_root = cfg.paths.local_root.clone();
        let local_path = local_root.join(&rel);
        let expected_local = LocalIdentity::capture(&local_path)?;
        let local_hash = match &expected_local {
            LocalIdentity::Missing => None,
            LocalIdentity::Present(hash) => Some(hash.as_str()),
        };
        let state_path = state_path_for(&local_root, ExecutionMode::Apply);
        let mut loaded;
        let state: &mut StateFile = match held_state.as_deref_mut() {
            Some(held) => held,
            None => {
                loaded = StateFile::load_or_default(&state_path)?;
                &mut loaded
            }
        };
        let mut owned;
        let ftp: &mut Ftp = match session.as_deref_mut() {
            Some(open) => open,
            None => {
                owned = connect(&cfg)?;
                &mut owned
            }
        };
        let remote_path = remote_join(&cfg.paths.remote_root, &rel);
        let probed = probe_remote(ftp, &remote_path)?;
        let remote_exists = probed != ProbedRemote::Missing;
        let probed_size = match probed {
            ProbedRemote::Present { size } => size,
            ProbedRemote::Missing => None,
        };
        if !remote_exists && local_hash.is_none() {
            anyhow::bail!("neither local nor remote has {rel}");
        }

        let remote_hash = if remote_exists {
            Some(remote_hash::compute_after_probe(
                ftp,
                state,
                &rel,
                &remote_path,
                true,
                probed_size,
            )?)
        } else {
            None
        };
        let remote_hash_str = remote_hash.as_ref().map(|remote| remote.sha256.as_str());
        let known = state.files.get(&rel).map(|record| record.sha256.as_str());
        let file_state = classify(local_hash, remote_hash_str, known);
        if file_state == FileState::InSync
            && let Some(remote) = remote_hash.as_ref()
        {
            refresh_in_sync_record(state, &state_path, &rel, remote)?;
        }
        let action = match file_state {
            FileState::InSync => PreparedAction::Noop(TransferStatus::Unchanged),
            FileState::LocalOnly => PreparedAction::Noop(TransferStatus::SkippedMissingSource),
            FileState::RemoteOnly | FileState::RemoteChanged => PreparedAction::Install(
                remote_file_for_install(
                    ftp,
                    &remote_path,
                    remote_hash.expect("remote hash set when remote exists"),
                )?,
            ),
            FileState::Untracked if local_hash == remote_hash_str => {
                let remote = remote_hash.expect("remote hash set when remote exists");
                PreparedAction::Adopt {
                    sha256: remote.sha256,
                    size: remote.size,
                    mtime: remote.mtime,
                }
            }
            FileState::Untracked if !force => {
                let local_size = std::fs::metadata(&local_path)
                    .with_context(|| format!("reading {}", local_path.display()))?
                    .len();
                let remote_size = remote_hash.as_ref().map_or(0, |remote| remote.size);
                return Err(super::untracked_conflict(&rel, local_size, remote_size).into());
            }
            FileState::LocalChanged | FileState::BothChanged | FileState::Untracked => {
                if !force {
                    return Err(crate::error::Exit::Conflict(format!(
                        "conflict ({file_state:?}) on {rel}: local changes present; pass --force to override",
                    ))
                    .into());
                }
                PreparedAction::Install(remote_file_for_install(
                    ftp,
                    &remote_path,
                    remote_hash.expect("remote hash set when remote exists"),
                )?)
            }
        };

        Ok(PreparedPull {
            config_path: config_path.to_path_buf(),
            local_root,
            local_path,
            relative_path: rel.clone(),
            expected_local,
            action,
        })
    })()
    .with_context(|| format!("pull {rel}"))
}

pub fn prepare_force_pull_one(config_path: &Path, rel: &str) -> Result<PreparedPull> {
    let rel = safe_rel(rel).with_context(|| format!("pull {rel}"))?;
    (|| {
        let cfg = Config::load(config_path)?;
        let local_root = cfg.paths.local_root.clone();
        let local_path = local_root.join(&rel);
        let expected_local = LocalIdentity::capture(&local_path)?;
        let remote = fetch_remote_one(config_path, &rel)?;

        Ok::<PreparedPull, anyhow::Error>(PreparedPull {
            config_path: config_path.to_path_buf(),
            local_root,
            local_path,
            relative_path: rel.clone(),
            expected_local,
            action: PreparedAction::Install(remote),
        })
    })()
    .with_context(|| format!("pull {rel}"))
}

pub fn apply_prepared_pull(prepared: PreparedPull, mode: ExecutionMode) -> Result<TransferOutcome> {
    apply_prepared_pull_if(prepared, mode, || true)
}

pub fn apply_prepared_pull_if<F>(
    prepared: PreparedPull,
    mode: ExecutionMode,
    authorize: F,
) -> Result<TransferOutcome>
where
    F: FnOnce() -> bool,
{
    let PreparedPull {
        config_path,
        local_root,
        local_path,
        relative_path,
        expected_local,
        action,
    } = prepared;
    let context_path = relative_path.clone();

    (|| {
        let cfg = Config::load(&config_path)?;
        if cfg.paths.local_root != local_root {
            return Err(crate::error::Exit::Conflict(format!(
                "local root changed while preparing pull for {relative_path}",
            ))
            .into());
        }

        let saved_local = LocalIdentity::capture_for_apply(&local_path, &relative_path)?;
        if saved_local != expected_local {
            return Err(local_identity_conflict(&relative_path));
        }

        // The state file is loaded only when there is a record to write: on
        // a large mirror each load costs ~10 ms, and the hook runs this for
        // every Read and Edit, almost always as a no-op.
        let state_path = state_path_for(&local_root, mode);
        let recorded = |sha256: &str, size: u64, mtime: DateTime<Utc>| -> Result<StateFile> {
            let mut state = StateFile::load_or_default(&state_path)?;
            record_download(&mut state, &relative_path, sha256, size, mtime);
            Ok(state)
        };
        let (status, staged, updated_state) = match action {
            PreparedAction::Noop(status) => (status, None, None),
            PreparedAction::Adopt { .. } if !mode.should_apply() => {
                (TransferStatus::Unchanged, None, None)
            }
            PreparedAction::Adopt {
                sha256,
                size,
                mtime,
            } => (
                TransferStatus::Unchanged,
                None,
                Some(recorded(&sha256, size, mtime)?),
            ),
            PreparedAction::Install(_) if !mode.should_apply() => {
                (TransferStatus::Transferred, None, None)
            }
            PreparedAction::Install(remote) => (
                TransferStatus::Transferred,
                Some(stage_local_write(&local_path, &remote.bytes)?),
                Some(recorded(&remote.sha256, remote.size, remote.mtime)?),
            ),
        };
        let outcome = TransferOutcome::new(&relative_path, status);

        if LocalIdentity::capture_for_apply(&local_path, &relative_path)? != saved_local {
            return Err(local_identity_conflict(&relative_path));
        }

        if !authorize() {
            return Err(crate::error::Exit::Conflict(format!(
                "pull for {relative_path} was cancelled before commit",
            ))
            .into());
        }

        if let Some(staged) = staged {
            staged.commit()?;
        }
        if let Some(state) = updated_state {
            state.save(&state_path)?;
        }
        Ok(outcome)
    })()
    .with_context(|| format!("pull {context_path}"))
}

pub fn pull_one(
    config_path: &Path,
    rel: &str,
    force: bool,
    mode: ExecutionMode,
) -> Result<TransferOutcome> {
    apply_prepared_pull(prepare_pull_one(config_path, rel, force)?, mode)
}

/// [`pull_one`] on an already open connection and an already loaded state
/// (the hook helper keeps both between requests).
pub fn pull_one_on(
    ftp: &mut Ftp,
    state: &mut StateFile,
    config_path: &Path,
    rel: &str,
    force: bool,
    mode: ExecutionMode,
) -> Result<TransferOutcome> {
    let prepared = prepare_pull_one_on(Some(ftp), Some(state), config_path, rel, force)?;
    apply_prepared_pull(prepared, mode)
}

/// [`pull_one`] for a caller that has already loaded this project's state
/// file, so it is not parsed a second time.
pub fn pull_one_with_state(
    config_path: &Path,
    rel: &str,
    force: bool,
    mode: ExecutionMode,
    mut state: StateFile,
) -> Result<TransferOutcome> {
    let prepared = prepare_pull_one_on(None, Some(&mut state), config_path, rel, force)?;
    apply_prepared_pull(prepared, mode)
}

/// For a file found in sync, keep its record useful, as the full pull does:
/// a record confirmed from the server (MDTM or listing) gets its verification
/// time refreshed, and a stale or missing record is replaced when the file was
/// hashed and found identical, so the next check is cheap instead of another
/// download. Saved (merged) only when something changed.
fn refresh_in_sync_record(
    state: &mut StateFile,
    state_path: &Path,
    rel: &str,
    remote: &RemoteHash,
) -> Result<()> {
    let now = Utc::now();
    let changed = if remote.from_cache {
        match state.files.get_mut(rel) {
            Some(record) if record.last_synced < now => {
                record.last_synced = now;
                true
            }
            _ => false,
        }
    } else if remote.metadata_stable {
        record_download(state, rel, &remote.sha256, remote.size, remote.mtime);
        true
    } else {
        false
    };
    if changed {
        state.save(state_path)?;
    }
    Ok(())
}

fn retrieve_remote_file<R: RemoteFileRetrieval>(
    remote: &mut R,
    remote_path: &str,
) -> Result<RemoteFile> {
    remote_file_from_hash(remote_hash::retrieve_stable(remote, remote_path)?)
}

fn connect(cfg: &Config) -> Result<Ftp> {
    Ftp::connect(
        &cfg.connection.host,
        cfg.connection.port,
        &cfg.connection.user,
        &cfg.connection.password,
        cfg.connection.passive,
    )
}

fn remote_file_for_install<R: RemoteFileRetrieval>(
    remote: &mut R,
    remote_path: &str,
    remote_hash: RemoteHash,
) -> Result<RemoteFile> {
    remote_file_from_hash(remote_hash::complete_for_install(
        remote,
        remote_path,
        remote_hash,
    )?)
}

fn remote_file_from_hash(remote_hash: RemoteHash) -> Result<RemoteFile> {
    let bytes = remote_hash
        .bytes
        .ok_or_else(|| anyhow::anyhow!("completed remote hash has no payload"))?;
    Ok(RemoteFile {
        bytes,
        sha256: remote_hash.sha256,
        size: remote_hash.size,
        mtime: remote_hash.mtime,
    })
}

#[cfg(test)]
fn remote_file_from_payload(bytes: Vec<u8>, mtime: DateTime<Utc>) -> RemoteFile {
    let size = bytes.len() as u64;
    let sha256 = hash_bytes(&bytes);
    RemoteFile {
        bytes,
        sha256,
        size,
        mtime,
    }
}

fn local_identity_conflict(rel: &str) -> anyhow::Error {
    crate::error::Exit::Conflict(format!("local file changed while preparing pull for {rel}",))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ExecutionMode;
    use crate::hash::hash_bytes;
    use crate::state::{FileRecord, StateFile};
    use chrono::{TimeZone, Utc};
    use std::{collections::VecDeque, path::Path};

    fn write_config(path: &Path, local_root: &Path) {
        std::fs::write(
            path,
            format!(
                r#"
[connection]
host = "example.invalid"
user = "test"
password = "test"

[paths]
local_root = "{}"
remote_root = "/remote"
"#,
                local_root.display()
            ),
        )
        .unwrap();
    }

    #[test]
    fn local_identity_distinguishes_missing_and_present_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("page.txt");

        assert_eq!(
            LocalIdentity::capture(&path).unwrap(),
            LocalIdentity::Missing
        );

        std::fs::write(&path, b"local bytes").unwrap();
        assert_eq!(
            LocalIdentity::capture(&path).unwrap(),
            LocalIdentity::Present(hash_bytes(b"local bytes"))
        );
    }

    #[test]
    fn apply_rejects_a_changed_local_identity_without_writing_state() {
        let dir = tempfile::tempdir().unwrap();
        let local_root = dir.path().join("site");
        let local_path = local_root.join("page.txt");
        let config_path = dir.path().join("ferry.toml");
        let state_path = local_root.join(crate::names::STATE_DIR).join("state.json");
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        std::fs::write(&local_path, b"prepared local bytes").unwrap();
        write_config(&config_path, &local_root);
        let original_state = br#"{
  "version": 1,
  "files": {},
  "server_supports_mdtm": true
}"#;
        std::fs::write(&state_path, original_state).unwrap();

        let prepared = PreparedPull {
            config_path,
            local_root,
            local_path: local_path.clone(),
            relative_path: "page.txt".to_string(),
            expected_local: LocalIdentity::capture(&local_path).unwrap(),
            action: PreparedAction::Install(RemoteFile {
                bytes: b"remote bytes".to_vec(),
                sha256: hash_bytes(b"remote bytes"),
                size: b"remote bytes".len() as u64,
                mtime: Utc.with_ymd_and_hms(2026, 8, 7, 8, 30, 0).unwrap(),
            }),
        };
        std::fs::write(&local_path, b"edited after prepare").unwrap();

        let error = apply_prepared_pull(prepared, ExecutionMode::Apply).unwrap_err();

        assert!(
            error
                .downcast_ref::<crate::error::Exit>()
                .is_some_and(|exit| matches!(exit, crate::error::Exit::Conflict(_))),
            "got: {error:#}"
        );
        assert_eq!(std::fs::read(&local_path).unwrap(), b"edited after prepare");
        assert_eq!(std::fs::read(&state_path).unwrap(), original_state);
    }

    #[test]
    fn apply_installs_bytes_and_records_supplied_remote_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let local_root = dir.path().join("site");
        let local_path = local_root.join("nested/page.txt");
        let config_path = dir.path().join("ferry.toml");
        std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        std::fs::write(&local_path, b"local bytes").unwrap();
        write_config(&config_path, &local_root);
        let remote_mtime = Utc.with_ymd_and_hms(2026, 8, 7, 9, 45, 0).unwrap();
        let remote_bytes = b"installed remote bytes";
        let remote_sha256 = hash_bytes(remote_bytes);
        let prepared = PreparedPull {
            config_path,
            local_root: local_root.clone(),
            local_path: local_path.clone(),
            relative_path: "nested/page.txt".to_string(),
            expected_local: LocalIdentity::capture(&local_path).unwrap(),
            action: PreparedAction::Install(RemoteFile {
                bytes: remote_bytes.to_vec(),
                sha256: remote_sha256.clone(),
                size: remote_bytes.len() as u64,
                mtime: remote_mtime,
            }),
        };

        let outcome = apply_prepared_pull(prepared, ExecutionMode::Apply).unwrap();

        assert_eq!(outcome.path, "nested/page.txt");
        assert_eq!(
            outcome.status,
            crate::commands::file_transfer::TransferStatus::Transferred
        );
        assert_eq!(std::fs::read(&local_path).unwrap(), remote_bytes);
        let state_path = crate::commands::state_path_for(&local_root, ExecutionMode::Apply);
        let state = StateFile::load_or_default(&state_path).unwrap();
        let record = state.files.get("nested/page.txt").unwrap();
        assert_eq!(record.sha256, remote_sha256);
        assert_eq!(record.size, remote_bytes.len() as u64);
        assert_eq!(record.remote_mtime, remote_mtime);
    }
    #[test]
    fn install_denial_calls_authorization_once_and_preserves_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let local_root = dir.path().join("site");
        let local_path = local_root.join("page.txt");
        let config_path = dir.path().join("ferry.toml");
        let state_path = local_root.join(crate::names::STATE_DIR).join("state.json");
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        let original_local = b"local bytes before denial";
        let original_state = br#"{
  "version": 1,
  "files": {},
  "server_supports_mdtm": true
}"#;
        std::fs::write(&local_path, original_local).unwrap();
        std::fs::write(&state_path, original_state).unwrap();
        write_config(&config_path, &local_root);
        let prepared = PreparedPull {
            config_path,
            local_root,
            local_path: local_path.clone(),
            relative_path: "page.txt".to_string(),
            expected_local: LocalIdentity::capture(&local_path).unwrap(),
            action: PreparedAction::Install(RemoteFile {
                bytes: b"denied remote bytes".to_vec(),
                sha256: hash_bytes(b"denied remote bytes"),
                size: b"denied remote bytes".len() as u64,
                mtime: Utc.with_ymd_and_hms(2026, 8, 7, 10, 0, 0).unwrap(),
            }),
        };
        let authorization_calls = std::cell::Cell::new(0);

        let error = apply_prepared_pull_if(prepared, ExecutionMode::Apply, || {
            authorization_calls.set(authorization_calls.get() + 1);
            false
        })
        .unwrap_err();

        assert!(error.downcast_ref::<crate::error::Exit>().is_some());
        assert_eq!(authorization_calls.get(), 1);
        assert_eq!(std::fs::read(&local_path).unwrap(), original_local);
        assert_eq!(std::fs::read(&state_path).unwrap(), original_state);
        assert!(!super::super::tmp_path(&local_path).exists());
    }

    #[test]
    fn noop_denial_calls_authorization_once_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let local_root = dir.path().join("site");
        let local_path = local_root.join("page.txt");
        let config_path = dir.path().join("ferry.toml");
        let state_path = local_root.join(crate::names::STATE_DIR).join("state.json");
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        let original_local = b"unchanged local bytes";
        let original_state = br#"{
  "version": 1,
  "files": {},
  "server_supports_mdtm": null
}"#;
        std::fs::write(&local_path, original_local).unwrap();
        std::fs::write(&state_path, original_state).unwrap();
        write_config(&config_path, &local_root);
        let prepared = PreparedPull {
            config_path,
            local_root,
            local_path: local_path.clone(),
            relative_path: "page.txt".to_string(),
            expected_local: LocalIdentity::capture(&local_path).unwrap(),
            action: PreparedAction::Noop(TransferStatus::Unchanged),
        };
        let authorization_calls = std::cell::Cell::new(0);

        let error = apply_prepared_pull_if(prepared, ExecutionMode::Apply, || {
            authorization_calls.set(authorization_calls.get() + 1);
            false
        })
        .unwrap_err();

        assert!(error.downcast_ref::<crate::error::Exit>().is_some());
        assert_eq!(authorization_calls.get(), 1);
        assert_eq!(std::fs::read(&local_path).unwrap(), original_local);
        assert_eq!(std::fs::read(&state_path).unwrap(), original_state);
        assert!(!super::super::tmp_path(&local_path).exists());
    }

    // A no-op has nothing to record, so it must not pay for parsing the state
    // file (~10 ms per load on a 16k-entry mirror, on every hook call). An
    // unparseable state file makes any read observable.
    #[test]
    fn noop_apply_does_not_read_the_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let local_root = dir.path().join("site");
        let local_path = local_root.join("page.txt");
        let config_path = dir.path().join("ferry.toml");
        let state_path = local_root.join(crate::names::STATE_DIR).join("state.json");
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        std::fs::write(&local_path, b"unchanged local bytes").unwrap();
        std::fs::write(&state_path, b"not json").unwrap();
        write_config(&config_path, &local_root);
        let prepared = PreparedPull {
            config_path,
            local_root,
            local_path: local_path.clone(),
            relative_path: "page.txt".to_string(),
            expected_local: LocalIdentity::capture(&local_path).unwrap(),
            action: PreparedAction::Noop(TransferStatus::Unchanged),
        };

        let outcome = apply_prepared_pull(prepared, ExecutionMode::Apply).unwrap();

        assert_eq!(outcome.status, TransferStatus::Unchanged);
    }

    #[test]
    fn changed_local_root_rejects_before_authorization() {
        let dir = tempfile::tempdir().unwrap();
        let local_root = dir.path().join("site");
        let changed_root = dir.path().join("other-site");
        let local_path = local_root.join("page.txt");
        let config_path = dir.path().join("ferry.toml");
        let state_path = local_root.join(crate::names::STATE_DIR).join("state.json");
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        let original_local = b"local bytes";
        let original_state = br#"{
  "version": 1,
  "files": {},
  "server_supports_mdtm": false
}"#;
        std::fs::write(&local_path, original_local).unwrap();
        std::fs::write(&state_path, original_state).unwrap();
        write_config(&config_path, &local_root);
        let prepared = PreparedPull {
            config_path: config_path.clone(),
            local_root,
            local_path: local_path.clone(),
            relative_path: "page.txt".to_string(),
            expected_local: LocalIdentity::capture(&local_path).unwrap(),
            action: PreparedAction::Install(RemoteFile {
                bytes: b"remote bytes".to_vec(),
                sha256: hash_bytes(b"remote bytes"),
                size: b"remote bytes".len() as u64,
                mtime: Utc.with_ymd_and_hms(2026, 8, 7, 11, 0, 0).unwrap(),
            }),
        };
        write_config(&config_path, &changed_root);
        let authorization_calls = std::cell::Cell::new(0);

        let error = apply_prepared_pull_if(prepared, ExecutionMode::Apply, || {
            authorization_calls.set(authorization_calls.get() + 1);
            true
        })
        .unwrap_err();

        assert!(error.downcast_ref::<crate::error::Exit>().is_some());
        assert_eq!(authorization_calls.get(), 0);
        assert_eq!(std::fs::read(&local_path).unwrap(), original_local);
        assert_eq!(std::fs::read(&state_path).unwrap(), original_state);
        assert!(!super::super::tmp_path(&local_path).exists());
    }

    #[test]
    fn denied_missing_install_removes_only_created_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let local_root = dir.path().join("site");
        let nested_parent = local_root.join("new/deep");
        let local_path = nested_parent.join("page.txt");
        let config_path = dir.path().join("ferry.toml");
        let state_path = local_root.join(crate::names::STATE_DIR).join("state.json");
        std::fs::create_dir(&local_root).unwrap();
        std::fs::write(local_root.join("keep.txt"), b"keep").unwrap();
        write_config(&config_path, &local_root);
        let prepared = PreparedPull {
            config_path,
            local_root: local_root.clone(),
            local_path: local_path.clone(),
            relative_path: "new/deep/page.txt".to_string(),
            expected_local: LocalIdentity::Missing,
            action: PreparedAction::Install(RemoteFile {
                bytes: b"denied remote bytes".to_vec(),
                sha256: hash_bytes(b"denied remote bytes"),
                size: b"denied remote bytes".len() as u64,
                mtime: Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).unwrap(),
            }),
        };

        let error = apply_prepared_pull_if(prepared, ExecutionMode::Apply, || false).unwrap_err();

        assert!(error.downcast_ref::<crate::error::Exit>().is_some());
        assert!(!local_path.exists());
        assert!(!super::super::tmp_path(&local_path).exists());
        assert!(!local_root.join("new").exists());
        assert!(local_root.is_dir());
        assert_eq!(std::fs::read(local_root.join("keep.txt")).unwrap(), b"keep");
        assert!(!state_path.exists());
    }

    #[test]
    fn apply_maps_a_file_replaced_by_a_directory_to_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let local_root = dir.path().join("site");
        let local_path = local_root.join("page.txt");
        let config_path = dir.path().join("ferry.toml");
        let state_path = local_root.join(crate::names::STATE_DIR).join("state.json");
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        let original_state = br#"{
  "version": 1,
  "files": {},
  "server_supports_mdtm": true
}"#;
        std::fs::write(&local_path, b"file at preparation").unwrap();
        std::fs::write(&state_path, original_state).unwrap();
        write_config(&config_path, &local_root);
        let prepared = PreparedPull {
            config_path,
            local_root,
            local_path: local_path.clone(),
            relative_path: "page.txt".to_string(),
            expected_local: LocalIdentity::capture(&local_path).unwrap(),
            action: PreparedAction::Install(RemoteFile {
                bytes: b"remote bytes".to_vec(),
                sha256: hash_bytes(b"remote bytes"),
                size: b"remote bytes".len() as u64,
                mtime: Utc.with_ymd_and_hms(2026, 8, 7, 12, 30, 0).unwrap(),
            }),
        };
        std::fs::remove_file(&local_path).unwrap();
        std::fs::create_dir(&local_path).unwrap();
        let authorization_calls = std::cell::Cell::new(0);

        let error = apply_prepared_pull_if(prepared, ExecutionMode::Apply, || {
            authorization_calls.set(authorization_calls.get() + 1);
            true
        })
        .unwrap_err();

        assert!(
            error
                .downcast_ref::<crate::error::Exit>()
                .is_some_and(|exit| matches!(exit, crate::error::Exit::Conflict(_))),
            "got: {error:#}"
        );
        assert_eq!(authorization_calls.get(), 0);
        assert!(local_path.is_dir());
        assert_eq!(std::fs::read(&state_path).unwrap(), original_state);
        assert!(!super::super::tmp_path(&local_path).exists());
    }

    #[test]
    fn remote_file_identity_comes_from_payload_not_stale_cache_metadata() {
        let mtime = Utc.with_ymd_and_hms(2026, 8, 7, 13, 0, 0).unwrap();
        let cached = RemoteHash {
            sha256: "stale cached hash".to_string(),
            size: 9_999,
            mtime,
            from_cache: true,
            metadata_stable: true,
            bytes: None,
            pre_download: None,
        };
        let payload = b"actual downloaded payload";

        let remote = remote_file_from_payload(payload.to_vec(), cached.mtime);

        assert_eq!(remote.bytes, payload);
        assert_eq!(remote.sha256, hash_bytes(payload));
        assert_eq!(remote.size, payload.len() as u64);
        assert_eq!(remote.mtime, mtime);
        assert_ne!(remote.sha256, cached.sha256);
        assert_ne!(remote.size, cached.size);
    }

    struct ScriptedRetrieval {
        mtimes: VecDeque<DateTime<Utc>>,
        sizes: VecDeque<Option<u64>>,
        bytes: Option<Vec<u8>>,
        events: Vec<&'static str>,
    }

    impl ScriptedRetrieval {
        fn new(
            mtimes: impl IntoIterator<Item = DateTime<Utc>>,
            sizes: impl IntoIterator<Item = Option<u64>>,
            bytes: &[u8],
        ) -> Self {
            Self {
                mtimes: mtimes.into_iter().collect(),
                sizes: sizes.into_iter().collect(),
                bytes: Some(bytes.to_vec()),
                events: Vec::new(),
            }
        }

        fn assert_bracketed_one_download(&self) {
            assert_eq!(self.events, ["mtime", "size", "download", "mtime", "size"]);
            assert_eq!(
                self.events
                    .iter()
                    .filter(|event| **event == "download")
                    .count(),
                1
            );
        }
    }

    impl RemoteFileRetrieval for ScriptedRetrieval {
        fn mtime(&mut self, _remote_path: &str) -> Result<DateTime<Utc>> {
            self.events.push("mtime");
            Ok(self.mtimes.pop_front().expect("scripted MDTM call"))
        }

        fn size(&mut self, _remote_path: &str) -> Result<u64> {
            self.events.push("size");
            self.sizes
                .pop_front()
                .expect("scripted SIZE call")
                .ok_or_else(|| anyhow::anyhow!("SIZE unsupported"))
        }

        fn download(&mut self, _remote_path: &str) -> Result<Vec<u8>> {
            self.events.push("download");
            Ok(self.bytes.take().expect("exactly one RETR call"))
        }
    }

    #[test]
    fn remote_snapshot_brackets_exactly_one_download_with_metadata() {
        let mtime = Utc.with_ymd_and_hms(2026, 8, 7, 14, 0, 0).unwrap();
        let payload = b"stable remote payload";
        let mut retrieval = ScriptedRetrieval::new(
            [mtime, mtime],
            [Some(payload.len() as u64), Some(payload.len() as u64)],
            payload,
        );

        let remote = retrieve_remote_file(&mut retrieval, "/remote/page.txt").unwrap();

        retrieval.assert_bracketed_one_download();
        assert_eq!(remote.bytes, payload);
        assert_eq!(remote.sha256, hash_bytes(payload));
        assert_eq!(remote.size, payload.len() as u64);
        assert_eq!(remote.mtime, mtime);
    }

    #[test]
    fn remote_snapshot_rejects_changed_mtime_after_one_download() {
        let before = Utc.with_ymd_and_hms(2026, 8, 7, 14, 0, 0).unwrap();
        let after = Utc.with_ymd_and_hms(2026, 8, 7, 14, 0, 1).unwrap();
        let payload = b"raced remote payload";
        let mut retrieval = ScriptedRetrieval::new(
            [before, after],
            [Some(payload.len() as u64), Some(payload.len() as u64)],
            payload,
        );

        let error = retrieve_remote_file(&mut retrieval, "/remote/page.txt").unwrap_err();

        retrieval.assert_bracketed_one_download();
        assert!(
            format!("{error:#}").contains("remote changed while downloading /remote/page.txt"),
            "got: {error:#}"
        );
    }

    #[test]
    fn remote_snapshot_rejects_changed_available_size_after_one_download() {
        let mtime = Utc.with_ymd_and_hms(2026, 8, 7, 14, 0, 0).unwrap();
        let payload = b"size-raced remote payload";
        let mut retrieval = ScriptedRetrieval::new([mtime, mtime], [Some(10), Some(11)], payload);

        let error = retrieve_remote_file(&mut retrieval, "/remote/page.txt").unwrap_err();

        retrieval.assert_bracketed_one_download();
        assert!(
            format!("{error:#}").contains("remote changed while downloading /remote/page.txt"),
            "got: {error:#}"
        );
    }

    #[test]
    fn remote_snapshot_does_not_require_size_when_mtime_is_stable() {
        let mtime = Utc.with_ymd_and_hms(2026, 8, 7, 14, 0, 0).unwrap();
        let payload = b"stable payload without SIZE";
        let mut retrieval = ScriptedRetrieval::new([mtime, mtime], [None, None], payload);

        let remote = retrieve_remote_file(&mut retrieval, "/remote/page.txt").unwrap();

        retrieval.assert_bracketed_one_download();
        assert_eq!(remote.bytes, payload);
        assert_eq!(remote.sha256, hash_bytes(payload));
        assert_eq!(remote.size, payload.len() as u64);
        assert_eq!(remote.mtime, mtime);
    }

    fn cached_remote_state(rel: &str, cached_bytes: &[u8], mtime: DateTime<Utc>) -> StateFile {
        let mut state = StateFile::default();
        state.files.insert(
            rel.to_string(),
            FileRecord {
                sha256: hash_bytes(cached_bytes),
                size: cached_bytes.len() as u64,
                remote_mtime: mtime,
                last_synced: mtime,
            },
        );
        state
    }

    #[test]
    fn cache_hit_install_continues_the_probe_bracket_with_one_download() {
        let mtime = Utc.with_ymd_and_hms(2026, 8, 7, 15, 0, 0).unwrap();
        let cached_bytes = b"old cached payload";
        let downloaded = b"new remote payload";
        assert_eq!(cached_bytes.len(), downloaded.len());
        let mut remote = ScriptedRetrieval::new(
            [mtime, mtime],
            [Some(downloaded.len() as u64), Some(downloaded.len() as u64)],
            downloaded,
        );
        let mut state = cached_remote_state("page.txt", cached_bytes, mtime);
        let cached_hash = remote_hash::compute_with(
            &mut remote,
            &mut state,
            "page.txt",
            "/remote/page.txt",
            true,
        )
        .unwrap();

        let file = remote_file_for_install(&mut remote, "/remote/page.txt", cached_hash).unwrap();

        remote.assert_bracketed_one_download();
        assert_eq!(file.bytes, downloaded);
        assert_eq!(file.sha256, hash_bytes(downloaded));
        assert_eq!(file.size, downloaded.len() as u64);
        assert_eq!(file.mtime, mtime);
    }

    #[test]
    fn cache_hit_install_rejects_changed_metadata_after_one_download() {
        let before = Utc.with_ymd_and_hms(2026, 8, 7, 15, 0, 0).unwrap();
        let after = Utc.with_ymd_and_hms(2026, 8, 7, 15, 0, 1).unwrap();
        let cached_bytes = b"old cached payload";
        let downloaded = b"new remote payload";
        assert_eq!(cached_bytes.len(), downloaded.len());
        let mut remote = ScriptedRetrieval::new(
            [before, after],
            [Some(downloaded.len() as u64), Some(downloaded.len() as u64)],
            downloaded,
        );
        let mut state = cached_remote_state("page.txt", cached_bytes, before);
        let cached_hash = remote_hash::compute_with(
            &mut remote,
            &mut state,
            "page.txt",
            "/remote/page.txt",
            true,
        )
        .unwrap();

        let error =
            remote_file_for_install(&mut remote, "/remote/page.txt", cached_hash).unwrap_err();

        remote.assert_bracketed_one_download();
        assert!(
            format!("{error:#}").contains("remote changed while downloading /remote/page.txt"),
            "got: {error:#}"
        );
    }
}
