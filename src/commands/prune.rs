//! `--prune` for `pull` and `push`: mirror deletions, never lose work.
//!
//! Plain pull and push only copy. A file deleted on one side lingers on the
//! other forever, and anything reading the mirror (a code review, a grep)
//! treats the leftover as live. `--prune` removes such a file, under three
//! guards that all must hold:
//!
//! 1. The file was synced before and is unchanged since: its hash equals the
//!    state record. An edited or never-synced file is kept and reported, so a
//!    prune can only drop a copy ferry itself put there.
//! 2. The other side positively lacks it. For pull that is checked again by
//!    listing the nearest remote ancestor that lists cleanly; a walk that
//!    skipped a folder it could not list must never read as "deleted".
//! 3. The user named the paths. There is no whole-tree prune.

use crate::commands::ExecutionMode;
use crate::commands::walk::{child_name, listable_remote_root, remote_join};
use crate::ftp::{Ftp, Remote};
use crate::state::StateFile;
use anyhow::{Context, Result};
use std::path::Path;

/// True only when a listing proves `rel` is gone from the remote. Lists the
/// file's parent; if that fails (the folder may be gone too), the nearest
/// ancestor that lists, and checks the next path segment is absent there.
/// Any doubt -- no ancestor lists, or the segment is present -- is `false`.
pub fn remote_confirms_absent<R: Remote + ?Sized>(ftp: &mut R, remote_root: &str, rel: &str) -> bool {
    let root = listable_remote_root(remote_root);
    let segments: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    let mut reconnected = false;
    for depth in (0..segments.len()).rev() {
        let dir = if depth == 0 {
            root.clone()
        } else {
            remote_join(&root, &segments[..depth].join("/"))
        };
        let mut listed = ftp.list_dir(&dir);
        if listed.is_err() && !reconnected && ftp.try_reconnect() {
            reconnected = true;
            listed = ftp.list_dir(&dir);
        }
        if let Ok(entries) = listed {
            let child = segments[depth];
            return !entries
                .iter()
                .any(|e| child_name(&root, &dir, &e.name) == Some(child));
        }
    }
    false
}

/// Why a file that is missing on the other side is kept, or `None` when its
/// hash matches the state record and it may be pruned.
fn unchanged_since_sync(current: &str, known: Option<&str>) -> Option<&'static str> {
    match known {
        None => Some("never synced"),
        Some(k) if k != current => Some("changed since the last sync"),
        Some(_) => None,
    }
}

/// Pull side: `rel` exists only locally. Delete it when it is an untouched
/// copy of a file now gone from the remote.
pub fn prune_local(
    ftp: &mut Ftp,
    state: &mut StateFile,
    local_root: &Path,
    remote_root: &str,
    rel: &str,
    local_hash: &str,
    mode: ExecutionMode,
) -> Result<()> {
    let known = state.files.get(rel).map(|r| r.sha256.clone());
    if let Some(why) = unchanged_since_sync(local_hash, known.as_deref()) {
        eprintln!("keeping local {rel}: not on remote, but {why}");
        return Ok(());
    }
    if !remote_confirms_absent(ftp, remote_root, rel) {
        eprintln!("keeping local {rel}: could not confirm it is gone from remote");
        return Ok(());
    }
    if mode.is_dry_run() {
        println!("would prune (local) {rel}");
        return Ok(());
    }
    let full = local_root.join(rel);
    std::fs::remove_file(&full).with_context(|| format!("pruning local {}", full.display()))?;
    state.files.remove(rel);
    println!("pruned (local) {rel}: gone from remote");
    Ok(())
}

/// Push side: `rel` exists only on the remote. Delete it there when it is an
/// untouched copy of a file since deleted locally.
pub fn prune_remote(
    ftp: &mut Ftp,
    state: &mut StateFile,
    local_root: &Path,
    remote_path: &str,
    rel: &str,
    remote_hash: &str,
    mode: ExecutionMode,
) -> Result<()> {
    let known = state.files.get(rel).map(|r| r.sha256.clone());
    if let Some(why) = unchanged_since_sync(remote_hash, known.as_deref()) {
        eprintln!("keeping remote {rel}: not local, but {why}");
        return Ok(());
    }
    // symlink_metadata: a dangling local symlink still counts as present.
    if local_root.join(rel).symlink_metadata().is_ok() {
        eprintln!("keeping remote {rel}: a local entry exists after all");
        return Ok(());
    }
    if mode.is_dry_run() {
        println!("would prune (remote) {rel}");
        return Ok(());
    }
    ftp.rm(remote_path).with_context(|| format!("pruning remote {remote_path}"))?;
    state.files.remove(rel);
    println!("pruned (remote) {rel}: deleted locally");
    Ok(())
}

/// `--prune` needs explicit paths, and for push each must exist locally: a
/// folder missing from the mirror would otherwise read as "delete it all".
pub fn check_args(paths: &[String], local_root: &Path, push: bool) -> Result<()> {
    if paths.is_empty() {
        anyhow::bail!("--prune needs explicit paths; there is no whole-tree prune");
    }
    if push {
        for p in paths {
            if local_root.join(p).symlink_metadata().is_err() {
                anyhow::bail!(
                    "push --prune: {p} does not exist locally; refusing to prune its remote copy"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ftp::Entry;
    use chrono::TimeZone;
    use std::collections::HashMap;

    /// Directories that list (name -> children); any other path fails to list.
    struct Tree(HashMap<String, Vec<(&'static str, bool)>>);

    impl Remote for Tree {
        fn list_dir(&mut self, dir: &str) -> Result<Vec<Entry>> {
            let kids = self.0.get(dir).ok_or_else(|| anyhow::anyhow!("450 {dir}"))?;
            Ok(kids
                .iter()
                .map(|(n, d)| Entry {
                    name: (*n).to_string(),
                    is_dir: *d,
                    is_symlink: false,
                    size: 1,
                    modified: chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                })
                .collect())
        }
        fn file_size(&mut self, _path: &str) -> Result<u64> {
            anyhow::bail!("unused")
        }
    }

    fn tree(dirs: &[(&str, Vec<(&'static str, bool)>)]) -> Tree {
        Tree(dirs.iter().map(|(d, k)| (d.to_string(), k.clone())).collect())
    }

    #[test]
    fn a_file_missing_from_a_listed_parent_is_gone() {
        let mut t = tree(&[("/r/area", vec![("keep.c", false)])]);
        assert!(remote_confirms_absent(&mut t, "/r", "area/gone.c"));
    }

    #[test]
    fn a_file_still_listed_is_not_gone() {
        let mut t = tree(&[("/r/area", vec![("keep.c", false)])]);
        assert!(!remote_confirms_absent(&mut t, "/r", "area/keep.c"));
    }

    #[test]
    fn a_deleted_folder_is_proven_by_its_parent() {
        let mut t = tree(&[("/r/area", vec![("keep.c", false)])]);
        assert!(remote_confirms_absent(&mut t, "/r", "area/objects/gone.c"));
    }

    #[test]
    fn a_folder_that_exists_but_will_not_list_proves_nothing() {
        let mut t = tree(&[("/r/area", vec![("objects", true)])]);
        assert!(!remote_confirms_absent(&mut t, "/r", "area/objects/gone.c"));
    }

    #[test]
    fn nothing_lists_proves_nothing() {
        let mut t = tree(&[]);
        assert!(!remote_confirms_absent(&mut t, "/r", "area/gone.c"));
    }

    #[test]
    fn a_root_of_slash_lists_as_slash() {
        let mut t = tree(&[("/", vec![("players", true)])]);
        assert!(remote_confirms_absent(&mut t, "/", "gone/x.c"));
        assert!(!remote_confirms_absent(&mut t, "/", "players/x.c"));
    }

    #[test]
    fn only_untouched_synced_files_may_go() {
        assert_eq!(unchanged_since_sync("a", Some("a")), None);
        assert_eq!(unchanged_since_sync("a", Some("b")), Some("changed since the last sync"));
        assert_eq!(unchanged_since_sync("a", None), Some("never synced"));
    }

    #[test]
    fn prune_needs_paths_and_push_needs_them_local() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("area")).unwrap();
        assert!(check_args(&[], dir.path(), false).is_err());
        assert!(check_args(&["area".into()], dir.path(), true).is_ok());
        assert!(check_args(&["missing".into()], dir.path(), true).is_err());
        assert!(check_args(&["missing".into()], dir.path(), false).is_ok());
    }
}
