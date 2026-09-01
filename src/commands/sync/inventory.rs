use super::scope::SyncScope;
use crate::commands::transfer_temp::{
    is_reserved_local_transfer_temp, is_reserved_remote_transfer_temp,
};
use crate::ftp::{Entry, StrictRemote};
use crate::ignored::Matcher;
use crate::state::StateFile;
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::Metadata;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct InventoryEntry {
    pub(crate) local: Option<EntryKind>,
    pub(crate) remote: Option<EntryKind>,
    pub(crate) in_state: bool,
}

/// A non-authoritative snapshot of the selected local, remote, and state paths.
///
/// This inventory can become stale immediately after collection and is never
/// authorization to mutate a path. Consumers MUST revalidate applicable path
/// identities and entry types inside the final `CommitGate` claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopedInventory {
    pub(crate) scope: SyncScope,
    pub(crate) entries: BTreeMap<String, InventoryEntry>,
    pub(crate) implicit_ancestors: BTreeMap<String, InventoryEntry>,
    pub(crate) selected_local: bool,
    pub(crate) selected_remote: bool,
}

/// Collects a read-only, non-authoritative snapshot for planning.
///
/// No result from this function authorizes mutation. Consumers MUST revalidate
/// applicable path identities and entry types inside the final `CommitGate`
/// claim before making changes.
pub(crate) fn collect<R: StrictRemote + ?Sized>(
    remote: &mut R,
    local_root: &Path,
    remote_root: &str,
    matcher: &Matcher,
    state: &StateFile,
    scope: SyncScope,
) -> Result<ScopedInventory> {
    if scope == SyncScope::LegacyProject {
        bail!("scoped inventory requires an explicit sync scope");
    }

    let mut entries = BTreeMap::new();
    let mut implicit_ancestors = BTreeMap::new();
    let selected_local = collect_local(
        local_root,
        matcher,
        &scope,
        &mut entries,
        &mut implicit_ancestors,
    )?;
    let selected_remote = collect_remote(
        remote,
        local_root,
        remote_root,
        matcher,
        &scope,
        &mut entries,
        &mut implicit_ancestors,
    )?;

    for path in state.files.keys() {
        if !scope_contains(&scope, path) {
            continue;
        }
        validate_state_path_key(path)?;
        if is_state_path(path) || is_reserved_remote_transfer_temp(path) {
            continue;
        }
        if matcher.is_ignored(&local_root.join(path), false) {
            continue;
        }
        entries.entry(path.clone()).or_default().in_state = true;
    }

    if let SyncScope::Path(selected) = &scope
        && !selected_local
        && !selected_remote
        && !entries.get(selected).is_some_and(|entry| entry.in_state)
    {
        bail!("path not found locally or remotely");
    }

    Ok(ScopedInventory {
        scope,
        entries,
        implicit_ancestors,
        selected_local,
        selected_remote,
    })
}

struct LocalContext<'a> {
    local_root: &'a Path,
    canonical_root: &'a Path,
    canonical_state_root: &'a Path,
    matcher: &'a Matcher,
}

fn collect_local(
    local_root: &Path,
    matcher: &Matcher,
    scope: &SyncScope,
    entries: &mut BTreeMap<String, InventoryEntry>,
    implicit_ancestors: &mut BTreeMap<String, InventoryEntry>,
) -> Result<bool> {
    let canonical_root = local_root
        .canonicalize()
        .with_context(|| format!("canonicalizing local_root {}", local_root.display()))?;
    let state_root = local_root.join(crate::names::STATE_DIR);
    let canonical_state_root = match state_root.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            canonical_root.join(crate::names::STATE_DIR)
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("canonicalizing state dir {}", state_root.display()));
        }
    };
    let mut ancestors = BTreeSet::from([canonical_root.clone()]);
    let context = LocalContext {
        local_root,
        canonical_root: &canonical_root,
        canonical_state_root: &canonical_state_root,
        matcher,
    };

    match scope {
        SyncScope::LegacyProject => unreachable!("legacy scope rejected by collect"),
        SyncScope::RootDirectory => {
            collect_local_children(local_root, "", &context, entries, &mut ancestors)?;
            Ok(true)
        }
        SyncScope::Path(relative) => {
            validate_relative_path(relative)?;
            let Some((path, metadata)) =
                resolve_local_selection(&context, relative, implicit_ancestors)?
            else {
                return Ok(false);
            };
            collect_local_node(&path, relative, metadata, &context, entries, &mut ancestors)?;
            Ok(true)
        }
    }
}

fn resolve_local_selection(
    context: &LocalContext<'_>,
    relative: &str,
    implicit_ancestors: &mut BTreeMap<String, InventoryEntry>,
) -> Result<Option<(PathBuf, Metadata)>> {
    let segments = relative.split('/').collect::<Vec<_>>();
    let mut path = context.local_root.to_path_buf();
    let mut relative_prefix = String::new();

    for (index, segment) in segments.iter().enumerate() {
        path.push(segment);
        relative_prefix = join_relative(&relative_prefix, segment);
        if is_state_path(&relative_prefix) {
            bail!("refusing scoped path inside Ferry state directory");
        }

        let symlink_metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading local metadata {}", path.display()));
            }
        };
        let canonical = path
            .canonicalize()
            .with_context(|| format!("canonicalizing local path {}", path.display()))?;
        if !canonical.starts_with(context.canonical_root) {
            bail!(
                "local path {} resolves outside local_root {}",
                path.display(),
                context.local_root.display()
            );
        }
        if canonical.starts_with(context.canonical_state_root) {
            bail!("refusing scoped path inside Ferry state directory");
        }

        let metadata = if symlink_metadata.file_type().is_symlink() {
            std::fs::metadata(&path)
                .with_context(|| format!("following local symlink {}", path.display()))?
        } else {
            symlink_metadata.clone()
        };
        let is_dir = metadata.is_dir();
        if is_reserved_local_transfer_temp(&relative_prefix)
            || context
                .matcher
                .is_ignored(&context.local_root.join(&relative_prefix), is_dir)
        {
            return Ok(None);
        }
        let is_selected = index + 1 == segments.len();
        if is_selected {
            return Ok(Some((path, symlink_metadata)));
        }
        let kind = if is_dir {
            EntryKind::Directory
        } else if metadata.is_file() {
            EntryKind::File
        } else {
            bail!("unsupported local entry type at {}", path.display());
        };
        implicit_ancestors
            .entry(relative_prefix.clone())
            .or_default()
            .local = Some(kind);
        if !is_dir {
            return Ok(None);
        }
    }

    Ok(None)
}

fn collect_local_children(
    directory: &Path,
    relative_directory: &str,
    context: &LocalContext<'_>,
    entries: &mut BTreeMap<String, InventoryEntry>,
    ancestors: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let children = std::fs::read_dir(directory)
        .with_context(|| format!("reading local dir {}", directory.display()))?;
    for child in children {
        let child = child.with_context(|| format!("walking local dir {}", directory.display()))?;
        let name = child.file_name().into_string().map_err(|_| {
            anyhow::anyhow!(
                "local path under {} is not valid UTF-8",
                directory.display()
            )
        })?;
        let relative = join_relative(relative_directory, &name);
        let path = child.path();
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("reading local metadata {}", path.display()))?;
        collect_local_node(&path, &relative, metadata, context, entries, ancestors)?;
    }
    Ok(())
}

fn collect_local_node(
    path: &Path,
    relative: &str,
    symlink_metadata: Metadata,
    context: &LocalContext<'_>,
    entries: &mut BTreeMap<String, InventoryEntry>,
    ancestors: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if is_state_path(relative) || is_reserved_local_transfer_temp(relative) {
        return Ok(());
    }

    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalizing local path {}", path.display()))?;
    if !canonical.starts_with(context.canonical_root) {
        bail!(
            "local path {} resolves outside local_root {}",
            path.display(),
            context.local_root.display()
        );
    }
    if canonical.starts_with(context.canonical_state_root) {
        return Ok(());
    }

    let metadata = if symlink_metadata.file_type().is_symlink() {
        std::fs::metadata(path)
            .with_context(|| format!("following local symlink {}", path.display()))?
    } else {
        symlink_metadata
    };
    let file_type = metadata.file_type();
    let is_dir = if file_type.is_dir() {
        true
    } else if file_type.is_file() {
        false
    } else {
        bail!("unsupported local entry type at {}", path.display());
    };
    if context
        .matcher
        .is_ignored(&context.local_root.join(relative), is_dir)
    {
        return Ok(());
    }

    let kind = if is_dir {
        EntryKind::Directory
    } else {
        EntryKind::File
    };
    entries.entry(relative.to_string()).or_default().local = Some(kind);

    if !is_dir {
        return Ok(());
    }
    if !ancestors.insert(canonical.clone()) {
        bail!("local directory cycle encountered at {}", path.display());
    }
    let result = collect_local_children(path, relative, context, entries, ancestors);
    ancestors.remove(&canonical);
    result
}

fn collect_remote<R: StrictRemote + ?Sized>(
    remote: &mut R,
    local_root: &Path,
    remote_root: &str,
    matcher: &Matcher,
    scope: &SyncScope,
    entries: &mut BTreeMap<String, InventoryEntry>,
    implicit_ancestors: &mut BTreeMap<String, InventoryEntry>,
) -> Result<bool> {
    let remote_root = normalize_remote_root(remote_root)?;
    match scope {
        SyncScope::LegacyProject => unreachable!("legacy scope rejected by collect"),
        SyncScope::RootDirectory => {
            collect_remote_children(
                remote,
                local_root,
                &remote_root,
                &remote_root,
                "",
                matcher,
                entries,
            )?;
            Ok(true)
        }
        SyncScope::Path(relative) => {
            validate_relative_path(relative)?;
            collect_remote_path(
                remote,
                local_root,
                &remote_root,
                matcher,
                relative,
                entries,
                implicit_ancestors,
            )
        }
    }
}

fn collect_remote_path<R: StrictRemote + ?Sized>(
    remote: &mut R,
    local_root: &Path,
    remote_root: &str,
    matcher: &Matcher,
    selected: &str,
    entries: &mut BTreeMap<String, InventoryEntry>,
    implicit_ancestors: &mut BTreeMap<String, InventoryEntry>,
) -> Result<bool> {
    let segments = selected.split('/').collect::<Vec<_>>();
    let mut directory = remote_root.to_string();
    let mut relative_directory = String::new();

    for (index, expected) in segments.iter().enumerate() {
        let children = list_remote_children(
            remote,
            local_root,
            remote_root,
            &directory,
            &relative_directory,
            matcher,
        )?;
        let Some(child) = children.into_iter().find(|child| child.name == *expected) else {
            return Ok(false);
        };
        let child_relative = join_relative(&relative_directory, &child.name);
        let is_selected = index + 1 == segments.len();
        if is_selected {
            record_remote(entries, &child_relative, child.is_dir);
            if child.is_dir {
                let child_directory = remote_join(&directory, &child.name);
                collect_remote_children(
                    remote,
                    local_root,
                    remote_root,
                    &child_directory,
                    &child_relative,
                    matcher,
                    entries,
                )?;
            }
            return Ok(true);
        }
        record_remote(implicit_ancestors, &child_relative, child.is_dir);
        if !child.is_dir {
            return Ok(false);
        }
        directory = remote_join(&directory, &child.name);
        relative_directory = child_relative;
    }
    Ok(false)
}

fn collect_remote_children<R: StrictRemote + ?Sized>(
    remote: &mut R,
    local_root: &Path,
    remote_root: &str,
    directory: &str,
    relative_directory: &str,
    matcher: &Matcher,
    entries: &mut BTreeMap<String, InventoryEntry>,
) -> Result<()> {
    for child in list_remote_children(
        remote,
        local_root,
        remote_root,
        directory,
        relative_directory,
        matcher,
    )? {
        let relative = join_relative(relative_directory, &child.name);
        record_remote(entries, &relative, child.is_dir);
        if child.is_dir {
            let child_directory = remote_join(directory, &child.name);
            collect_remote_children(
                remote,
                local_root,
                remote_root,
                &child_directory,
                &relative,
                matcher,
                entries,
            )?;
        }
    }
    Ok(())
}

pub(super) fn list_remote_children<R: StrictRemote + ?Sized>(
    remote: &mut R,
    local_root: &Path,
    remote_root: &str,
    directory: &str,
    relative_directory: &str,
    matcher: &Matcher,
) -> Result<Vec<RemoteChild>> {
    let listed = remote
        .list_dir_strict(directory)
        .with_context(|| format!("listing remote directory {directory:?}"))?;
    let mut children = Vec::new();
    let mut names = BTreeSet::new();
    for entry in listed {
        if entry.name == "." || entry.name == ".." {
            continue;
        }
        // Name validation and the duplicate guard run for every record type,
        // symlinks included: an unsafe name or a self-contradictory listing is
        // a server problem regardless of what the record claims to be, and
        // skipping first would quietly exempt links from both checks.
        let name = remote_child_name(remote_root, directory, &entry)?;
        if !names.insert(name.clone()) {
            bail!("duplicate remote entry {name:?} in {directory:?}");
        }
        // Symlinks are not syncable and must not be descended into: the server
        // resolves the target, which can sit outside the configured remote
        // root. Skipping keeps one link from aborting the whole inventory --
        // the write paths refuse these paths independently.
        if entry.is_symlink {
            eprintln!(
                "warning: skipping remote symlink {:?} in {directory:?}: not following it, the target can resolve outside the remote root",
                sanitize_entry_name(&name)
            );
            continue;
        }
        let relative = join_relative(relative_directory, &name);
        if is_state_path(&relative) || is_reserved_remote_transfer_temp(&relative) {
            continue;
        }
        if matcher.is_ignored(&local_root.join(&relative), entry.is_dir) {
            continue;
        }
        children.push(RemoteChild {
            name,
            is_dir: entry.is_dir,
        });
    }
    Ok(children)
}

pub(super) struct RemoteChild {
    pub(super) name: String,
    pub(super) is_dir: bool,
}

/// Escape a server-supplied name for a warning. Unlike `remote_child_name`,
/// which refuses control characters outright, a skipped symlink is only
/// reported -- so the name still has to be rendered safely.
fn sanitize_entry_name(name: &str) -> String {
    name.chars().flat_map(char::escape_default).collect()
}

fn remote_child_name(remote_root: &str, directory: &str, entry: &Entry) -> Result<String> {
    let supplied = entry.name.as_str();
    if supplied.chars().any(char::is_control) {
        bail!("unsafe remote entry in {directory:?}");
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

    let child_path = remote_join(directory, name);
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || !is_under_remote_root(remote_root, &child_path)
    {
        bail!("unsafe remote entry in {directory:?}");
    }
    Ok(name.to_string())
}

fn record_remote(entries: &mut BTreeMap<String, InventoryEntry>, relative: &str, is_dir: bool) {
    entries.entry(relative.to_string()).or_default().remote = Some(if is_dir {
        EntryKind::Directory
    } else {
        EntryKind::File
    });
}

pub(super) fn validate_relative_path(relative: &str) -> Result<()> {
    if relative.is_empty()
        || relative.starts_with('/')
        || relative
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
        || relative.contains('\\')
    {
        bail!("unsafe scoped path {relative:?}");
    }
    Ok(())
}

fn scope_contains(scope: &SyncScope, path: &str) -> bool {
    match scope {
        SyncScope::LegacyProject => false,
        SyncScope::RootDirectory => true,
        SyncScope::Path(selected) => {
            path == selected
                || path
                    .strip_prefix(selected)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }
    }
}

fn validate_state_path_key(path: &str) -> Result<()> {
    let bytes = path.as_bytes();
    let has_windows_drive_prefix = bytes.get(1) == Some(&b':')
        && bytes
            .first()
            .is_some_and(|first| first.is_ascii_alphabetic());
    let has_noncanonical_segment = path
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..");
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || has_windows_drive_prefix
        || has_noncanonical_segment
    {
        bail!("state contains unsafe path key");
    }
    Ok(())
}

pub(super) fn normalize_remote_root(root: &str) -> Result<String> {
    if root.is_empty() {
        bail!("remote_root must not be empty");
    }
    let trimmed = root.trim_end_matches('/');
    Ok(if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    })
}

pub(super) fn remote_join(directory: &str, name: &str) -> String {
    if directory == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", directory.trim_end_matches('/'))
    }
}

fn is_under_remote_root(root: &str, path: &str) -> bool {
    root == "/"
        || path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn join_relative(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

pub(super) fn is_state_path(relative: &str) -> bool {
    relative == crate::names::STATE_DIR
        || relative
            .strip_prefix(crate::names::STATE_DIR)
            .is_some_and(|suffix| suffix.starts_with('/'))
}
#[cfg(test)]
mod tests {
    use super::{EntryKind, InventoryEntry, ScopedInventory, collect, sanitize_entry_name};
    use crate::commands::sync::scope::SyncScope;
    use crate::ftp::{Entry, Remote, StrictRemote};
    use crate::ignored::Matcher;
    use crate::state::{FileRecord, StateFile};
    use anyhow::Result;
    use chrono::{TimeZone, Utc};
    use std::collections::BTreeMap;
    use std::path::Path;

    #[derive(Default)]
    struct FakeRemote {
        directories: BTreeMap<String, Vec<Entry>>,
        failures: BTreeMap<String, String>,
        listed: Vec<String>,
        tolerant_calls: usize,
        size_calls: usize,
    }

    impl FakeRemote {
        fn directory(mut self, path: &str, entries: Vec<Entry>) -> Self {
            self.directories.insert(path.to_string(), entries);
            self
        }

        fn failure(mut self, path: &str, message: &str) -> Self {
            self.failures.insert(path.to_string(), message.to_string());
            self
        }
    }

    impl Remote for FakeRemote {
        fn list_dir(&mut self, _dir: &str) -> Result<Vec<Entry>> {
            self.tolerant_calls += 1;
            anyhow::bail!("tolerant listing must not be called")
        }

        fn file_size(&mut self, _path: &str) -> Result<u64> {
            self.size_calls += 1;
            anyhow::bail!("SIZE must not be called")
        }
    }

    impl StrictRemote for FakeRemote {
        fn list_dir_strict(&mut self, dir: &str) -> Result<Vec<Entry>> {
            self.listed.push(dir.to_string());
            if let Some(message) = self.failures.get(dir) {
                anyhow::bail!(message.clone());
            }
            self.directories
                .get(dir)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unexpected strict LIST {dir}"))
        }
    }

    fn file(name: &str) -> Entry {
        entry(name, false)
    }

    fn directory(name: &str) -> Entry {
        entry(name, true)
    }

    fn symlink(name: &str) -> Entry {
        Entry {
            is_symlink: true,
            ..entry(name, false)
        }
    }

    fn entry(name: &str, is_dir: bool) -> Entry {
        Entry {
            name: name.to_string(),
            is_dir,
            is_symlink: false,
            size: 1,
            modified: Utc.with_ymd_and_hms(2026, 8, 10, 0, 0, 0).unwrap(),
        }
    }

    fn state_with(paths: &[&str]) -> StateFile {
        let mut state = StateFile::default();
        for path in paths {
            state.files.insert(
                (*path).to_string(),
                FileRecord {
                    sha256: "known".to_string(),
                    size: 1,
                    remote_mtime: Utc.with_ymd_and_hms(2026, 8, 10, 0, 0, 0).unwrap(),
                    last_synced: Utc.with_ymd_and_hms(2026, 8, 10, 0, 0, 0).unwrap(),
                },
            );
        }
        state
    }

    fn matcher(root: &Path, patterns: &[&str]) -> Matcher {
        Matcher::new(
            &patterns
                .iter()
                .map(|pattern| (*pattern).to_string())
                .collect::<Vec<_>>(),
            root,
        )
        .unwrap()
    }

    fn inventory(
        remote: &mut FakeRemote,
        local_root: &Path,
        state: &StateFile,
        scope: SyncScope,
    ) -> Result<ScopedInventory> {
        collect(
            remote,
            local_root,
            "/remote",
            &matcher(local_root, &[]),
            state,
            scope,
        )
    }

    fn presence(
        local: Option<EntryKind>,
        remote: Option<EntryKind>,
        in_state: bool,
    ) -> InventoryEntry {
        InventoryEntry {
            local,
            remote,
            in_state,
        }
    }

    #[test]
    fn collects_an_exact_local_file() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("only-local.c"), "local").unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("only-local.c".into()),
        )
        .unwrap();

        assert_eq!(
            actual.entries.get("only-local.c"),
            Some(&presence(Some(EntryKind::File), None, false))
        );
        assert_eq!(remote.listed, vec!["/remote"]);
        assert_eq!(remote.tolerant_calls, 0);
        assert_eq!(remote.size_calls, 0);
    }

    #[test]
    fn collects_an_exact_remote_only_file() {
        let root = tempfile::tempdir().unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![file("only-remote.c")]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("only-remote.c".into()),
        )
        .unwrap();

        assert_eq!(
            actual.entries.get("only-remote.c"),
            Some(&presence(None, Some(EntryKind::File), false))
        );
        assert_eq!(remote.listed, vec!["/remote"]);
    }

    #[test]
    fn collects_local_only_directory_with_nested_and_empty_descendants() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("area/nested/empty")).unwrap();
        std::fs::write(root.path().join("area/nested/file.c"), "local").unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("area".into()),
        )
        .unwrap();

        assert_eq!(
            actual.entries,
            BTreeMap::from([
                (
                    "area".into(),
                    presence(Some(EntryKind::Directory), None, false),
                ),
                (
                    "area/nested".into(),
                    presence(Some(EntryKind::Directory), None, false),
                ),
                (
                    "area/nested/empty".into(),
                    presence(Some(EntryKind::Directory), None, false),
                ),
                (
                    "area/nested/file.c".into(),
                    presence(Some(EntryKind::File), None, false),
                ),
            ])
        );
        assert_eq!(remote.listed, vec!["/remote"]);
    }

    #[test]
    fn collects_remote_only_directory_with_nested_and_empty_descendants() {
        let root = tempfile::tempdir().unwrap();
        let mut remote = FakeRemote::default()
            .directory("/remote", vec![directory("area")])
            .directory("/remote/area", vec![directory("nested"), file("top.c")])
            .directory(
                "/remote/area/nested",
                vec![directory("empty"), file("inner.c")],
            )
            .directory("/remote/area/nested/empty", vec![]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("area".into()),
        )
        .unwrap();

        assert_eq!(
            actual.entries,
            BTreeMap::from([
                (
                    "area".into(),
                    presence(None, Some(EntryKind::Directory), false),
                ),
                (
                    "area/nested".into(),
                    presence(None, Some(EntryKind::Directory), false),
                ),
                (
                    "area/nested/empty".into(),
                    presence(None, Some(EntryKind::Directory), false),
                ),
                (
                    "area/nested/inner.c".into(),
                    presence(None, Some(EntryKind::File), false),
                ),
                (
                    "area/top.c".into(),
                    presence(None, Some(EntryKind::File), false),
                ),
            ])
        );
        assert_eq!(
            remote.listed,
            vec![
                "/remote",
                "/remote/area",
                "/remote/area/nested",
                "/remote/area/nested/empty"
            ]
        );
    }

    #[test]
    fn root_directory_collects_descendants_without_an_empty_key() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("local-empty")).unwrap();
        let mut remote = FakeRemote::default()
            .directory("/remote", vec![directory("remote-empty")])
            .directory("/remote/remote-empty", vec![]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::RootDirectory,
        )
        .unwrap();

        assert_eq!(actual.scope, SyncScope::RootDirectory);
        assert!(!actual.entries.contains_key(""));
        assert_eq!(
            actual.entries.get("local-empty"),
            Some(&presence(Some(EntryKind::Directory), None, false))
        );
        assert_eq!(
            actual.entries.get("remote-empty"),
            Some(&presence(None, Some(EntryKind::Directory), false))
        );
    }

    #[test]
    fn empty_root_directory_is_still_a_valid_selection() {
        let root = tempfile::tempdir().unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::RootDirectory,
        )
        .unwrap();

        assert!(actual.entries.is_empty());
    }

    #[test]
    fn root_scope_excludes_exact_local_transfer_temps_but_keeps_near_misses() {
        let root = tempfile::tempdir().unwrap();
        let exact = ".page.txt.ferry-tmp.0123456789abcdef0123456789abcdef";
        let near_misses = [
            ".page.txt.ferry-tmp.0123456789abcdef0123456789abcde",
            ".page.txt.ferry-tmp.0123456789abcdef0123456789abcdeF",
            "page.txt.ferry-tmp.0123456789abcdef0123456789abcdef",
        ];
        std::fs::write(root.path().join(exact), "stale temp").unwrap();
        for name in near_misses {
            std::fs::write(root.path().join(name), "user file").unwrap();
        }
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &state_with(&[exact]),
            SyncScope::RootDirectory,
        )
        .unwrap();

        assert!(!actual.entries.contains_key(exact));
        for name in near_misses {
            assert_eq!(
                actual.entries.get(name),
                Some(&presence(Some(EntryKind::File), None, false))
            );
        }
    }

    #[test]
    fn root_scope_excludes_exact_remote_transfer_temps_but_keeps_near_misses() {
        let root = tempfile::tempdir().unwrap();
        let exact = ".page.txt.ferry-tmp.0123456789abcdef0123456789abcdef";
        let near_misses = [
            ".page.txt.ferry-tmp.0123456789abcdef0123456789abcdef0",
            "..ferry-tmp.0123456789abcdef0123456789abcdef",
            ".page.txt.ferry-tmp.0123456789abcdef0123456789ABCDEf",
        ];
        let mut entries = vec![file(exact)];
        entries.extend(near_misses.into_iter().map(file));
        let mut remote = FakeRemote::default().directory("/remote", entries);

        let actual = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::RootDirectory,
        )
        .unwrap();

        assert!(!actual.entries.contains_key(exact));
        for name in near_misses {
            assert_eq!(
                actual.entries.get(name),
                Some(&presence(None, Some(EntryKind::File), false))
            );
        }
    }

    #[test]
    fn adds_state_only_records_beneath_scope_on_segment_boundaries() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("area")).unwrap();
        let state = state_with(&["area", "area/old.c", "area/nested/x.c", "area-old/x.c"]);
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &state,
            SyncScope::Path("area".into()),
        )
        .unwrap();

        assert!(actual.entries.get("area").unwrap().in_state);
        assert_eq!(
            actual.entries.get("area/old.c"),
            Some(&presence(None, None, true))
        );
        assert_eq!(
            actual.entries.get("area/nested/x.c"),
            Some(&presence(None, None, true))
        );
        assert!(!actual.entries.contains_key("area-old/x.c"));
    }

    #[test]
    fn root_scope_excludes_state_only_ferry_paths_on_segment_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let state = state_with(&[".ferry", ".ferry/state.json", ".ferry-old/x.c"]);
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let actual = inventory(&mut remote, root.path(), &state, SyncScope::RootDirectory).unwrap();

        assert_eq!(
            actual.entries,
            BTreeMap::from([(".ferry-old/x.c".into(), presence(None, None, true))])
        );
    }

    #[test]
    fn applies_ignore_rules_to_local_and_remote_entries_with_directory_types() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("area/ignored/local-child")).unwrap();
        std::fs::write(root.path().join("area/drop.tmp"), "drop").unwrap();
        std::fs::write(root.path().join("area/keep.c"), "keep").unwrap();
        let mut remote = FakeRemote::default()
            .directory("/remote", vec![directory("area")])
            .directory(
                "/remote/area",
                vec![directory("ignored"), file("remote.tmp"), file("remote.c")],
            );
        let matcher = matcher(root.path(), &["ignored/", "*.tmp"]);

        let actual = collect(
            &mut remote,
            root.path(),
            "/remote",
            &matcher,
            &StateFile::default(),
            SyncScope::Path("area".into()),
        )
        .unwrap();

        assert_eq!(
            actual
                .entries
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["area", "area/keep.c", "area/remote.c"]
        );
        assert_eq!(remote.listed, vec!["/remote", "/remote/area"]);
    }

    #[test]
    fn retains_file_versus_directory_presence_for_conflict_reporting() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("clash"), "file").unwrap();
        let mut remote = FakeRemote::default()
            .directory("/remote", vec![directory("clash")])
            .directory("/remote/clash", vec![file("child.c")]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("clash".into()),
        )
        .unwrap();

        assert_eq!(
            actual.entries.get("clash"),
            Some(&presence(
                Some(EntryKind::File),
                Some(EntryKind::Directory),
                false
            ))
        );
        assert_eq!(
            actual.entries.get("clash/child.c"),
            Some(&presence(None, Some(EntryKind::File), false))
        );
    }

    #[test]
    fn rejects_a_path_missing_locally_remotely_and_from_state() {
        let root = tempfile::tempdir().unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let error = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("missing.c".into()),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("path not found locally or remotely"),
            "got: {error:#}"
        );
    }

    #[test]
    fn stale_state_only_exact_path_counts_as_selected_presence() {
        let root = tempfile::tempdir().unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &state_with(&["gone.c"]),
            SyncScope::Path("gone.c".into()),
        )
        .unwrap();

        assert!(!actual.selected_local);
        assert!(!actual.selected_remote);
        assert_eq!(
            actual.entries,
            BTreeMap::from([("gone.c".into(), presence(None, None, true))])
        );
    }

    #[test]
    fn state_only_descendant_does_not_validate_its_absent_parent_selection() {
        let root = tempfile::tempdir().unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let error = inventory(
            &mut remote,
            root.path(),
            &state_with(&["gone/child.c"]),
            SyncScope::Path("gone".into()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("path not found locally or remotely"));
    }

    #[test]
    fn strict_nested_list_failure_aborts_the_entire_inventory() {
        let root = tempfile::tempdir().unwrap();
        let mut remote = FakeRemote::default()
            .directory("/remote", vec![directory("area")])
            .directory("/remote/area", vec![directory("nested")])
            .failure("/remote/area/nested", "nested LIST failed");

        let error = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("area".into()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("nested LIST failed"));
        assert_eq!(
            remote.listed,
            vec!["/remote", "/remote/area", "/remote/area/nested"]
        );
    }

    #[test]
    fn unsafe_server_supplied_child_name_aborts_strict_traversal() {
        let root = tempfile::tempdir().unwrap();
        let mut remote =
            FakeRemote::default().directory("/remote", vec![file("/outside/attacker.c")]);

        let error = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::RootDirectory,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("unsafe remote entry"));
    }

    #[test]
    fn local_nested_file_is_inventoried_when_first_remote_ancestor_is_absent() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("a/b")).unwrap();
        std::fs::write(root.path().join("a/b/file.c"), "local").unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("a/b/file.c".into()),
        )
        .unwrap();

        assert_eq!(
            actual.entries.get("a/b/file.c"),
            Some(&presence(Some(EntryKind::File), None, false))
        );
        assert_eq!(remote.listed, vec!["/remote"]);
    }

    #[test]
    fn local_nested_directory_is_inventoried_when_inner_remote_ancestor_is_absent() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("a/b/selected/empty")).unwrap();
        let mut remote = FakeRemote::default()
            .directory("/remote", vec![directory("a")])
            .directory("/remote/a", vec![]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("a/b/selected".into()),
        )
        .unwrap();

        assert_eq!(
            actual.entries.get("a/b/selected"),
            Some(&presence(Some(EntryKind::Directory), None, false))
        );
        assert_eq!(
            actual.entries.get("a/b/selected/empty"),
            Some(&presence(Some(EntryKind::Directory), None, false))
        );
        assert_eq!(remote.listed, vec!["/remote", "/remote/a"]);
    }

    #[test]
    fn failure_listing_any_proven_remote_ancestor_is_fatal() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("a/b")).unwrap();
        std::fs::write(root.path().join("a/b/file.c"), "local").unwrap();
        let mut remote = FakeRemote::default()
            .directory("/remote", vec![directory("a")])
            .failure(
                "/remote/a",
                "permission denied while listing proven directory",
            );

        let error = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("a/b/file.c".into()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("permission denied"));
        assert_eq!(remote.listed, vec!["/remote", "/remote/a"]);
    }

    #[test]
    fn generic_ftp_550_listing_error_is_never_reinterpreted_as_absence() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("a/b")).unwrap();
        std::fs::write(root.path().join("a/b/file.c"), "local").unwrap();
        let mut remote = FakeRemote::default()
            .directory("/remote", vec![directory("a")])
            .failure("/remote/a", "550 file unavailable");

        let error = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("a/b/file.c".into()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("550 file unavailable"));
        assert_eq!(remote.size_calls, 0);
        assert_eq!(remote.tolerant_calls, 0);
    }

    #[test]
    fn dot_entries_are_accounted_for_and_skipped() {
        let root = tempfile::tempdir().unwrap();
        let mut remote = FakeRemote::default()
            .directory(
                "/remote",
                vec![directory("."), directory(".."), directory("area")],
            )
            .directory(
                "/remote/area",
                vec![directory("."), directory(".."), file("file.c")],
            );

        let actual = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("area".into()),
        )
        .unwrap();

        assert_eq!(
            actual
                .entries
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["area", "area/file.c"]
        );
    }

    #[test]
    fn ferry_state_directory_is_not_inventoried_or_descended_into() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".ferry/nested")).unwrap();
        std::fs::write(root.path().join(".ferry/nested/state.json"), "secret").unwrap();
        std::fs::write(root.path().join("keep.c"), "keep").unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::RootDirectory,
        )
        .unwrap();

        assert_eq!(
            actual.entries,
            BTreeMap::from([(
                "keep.c".into(),
                presence(Some(EntryKind::File), None, false)
            )])
        );
    }

    #[test]
    fn remote_ferry_state_directory_is_not_inventoried_or_descended_into() {
        let root = tempfile::tempdir().unwrap();
        let mut remote = FakeRemote::default()
            .directory("/remote", vec![directory(".ferry"), file("keep-remote.c")])
            .directory("/remote/.ferry", vec![file("state.json")]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::RootDirectory,
        )
        .unwrap();

        assert_eq!(
            actual.entries,
            BTreeMap::from([(
                "keep-remote.c".into(),
                presence(None, Some(EntryKind::File), false)
            )])
        );
        assert_eq!(remote.listed, vec!["/remote"]);
    }

    #[cfg(unix)]
    #[test]
    fn selected_symlink_that_escapes_local_root_is_rejected() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside.c");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(&outside, "outside").unwrap();
        symlink(&outside, root.join("escape.c")).unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let error = inventory(
            &mut remote,
            &root,
            &StateFile::default(),
            SyncScope::Path("escape.c".into()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("outside local_root"));
    }

    #[cfg(unix)]
    #[test]
    fn descendant_symlink_that_escapes_local_root_is_rejected() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(root.join("area")).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, root.join("area/escape")).unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let error = inventory(
            &mut remote,
            &root,
            &StateFile::default(),
            SyncScope::Path("area".into()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("outside local_root"));
    }

    #[cfg(unix)]
    #[test]
    fn missing_leaf_beneath_escaping_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        let mut remote = FakeRemote::default()
            .directory("/remote", vec![directory("escape")])
            .directory("/remote/escape", vec![file("new.c")]);

        let error = inventory(
            &mut remote,
            &root,
            &StateFile::default(),
            SyncScope::Path("escape/new.c".into()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("outside local_root"));
        assert!(remote.listed.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn root_scope_does_not_descend_through_an_alias_to_the_state_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".ferry")).unwrap();
        std::fs::write(root.path().join(".ferry/state.json"), "secret").unwrap();
        std::fs::write(root.path().join("keep.c"), "keep").unwrap();
        symlink(root.path().join(".ferry"), root.path().join("state-alias")).unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::RootDirectory,
        )
        .unwrap();

        assert_eq!(
            actual.entries,
            BTreeMap::from([(
                "keep.c".into(),
                presence(Some(EntryKind::File), None, false)
            )])
        );
    }

    #[cfg(unix)]
    #[test]
    fn exact_alias_to_the_state_directory_is_not_inventoried() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".ferry")).unwrap();
        std::fs::write(root.path().join(".ferry/state.json"), "secret").unwrap();
        symlink(root.path().join(".ferry"), root.path().join("state-alias")).unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let error = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("state-alias".into()),
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("Ferry state directory"),
            "got: {error:#}"
        );
    }

    #[test]
    fn root_scope_rejects_noncanonical_state_keys_without_echoing_controls() {
        let unsafe_paths = [
            "",
            "../escape",
            "/absolute",
            "./dot",
            "area/./file.c",
            "area/../file.c",
            "area//file.c",
            "area/",
            r"area\file.c",
            "C:/windows/file.c",
            "C:drive-relative.c",
            ".ferry/../escape.c",
            ".ferry//state.json",
            "control\u{1b}key",
            "control\rkey",
            "control\nkey",
            "control\0key",
        ];

        for unsafe_path in unsafe_paths {
            let root = tempfile::tempdir().unwrap();
            let mut remote = FakeRemote::default().directory("/remote", vec![]);

            let error = inventory(
                &mut remote,
                root.path(),
                &state_with(&[unsafe_path]),
                SyncScope::RootDirectory,
            )
            .unwrap_err();
            let message = format!("{error:#}");

            assert!(message.contains("unsafe path key"), "got: {message:?}");
            assert!(
                !message.chars().any(char::is_control),
                "error contains a raw control character: {message:?}"
            );
        }
    }

    #[test]
    fn path_scope_rejects_in_scope_noncanonical_state_keys() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("area")).unwrap();
        let unsafe_paths = [
            "area/../escape.c",
            "area/./file.c",
            "area//file.c",
            "area/",
            r"area/\file.c",
            "area/control\u{1b}key",
            "area/control\rkey",
            "area/control\nkey",
            "area/control\0key",
        ];

        for unsafe_path in unsafe_paths {
            let mut remote = FakeRemote::default().directory("/remote", vec![]);
            let error = inventory(
                &mut remote,
                root.path(),
                &state_with(&[unsafe_path]),
                SyncScope::Path("area".into()),
            )
            .unwrap_err();
            let message = format!("{error:#}");

            assert!(message.contains("unsafe path key"), "got: {message:?}");
            assert!(
                !message.chars().any(char::is_control),
                "error contains a raw control character: {message:?}"
            );
        }
    }

    #[test]
    fn path_scope_ignores_noncanonical_out_of_scope_state_keys() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("area")).unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &state_with(&["other/../escape.c"]),
            SyncScope::Path("area".into()),
        )
        .unwrap();

        assert_eq!(
            actual.entries,
            BTreeMap::from([(
                "area".into(),
                presence(Some(EntryKind::Directory), None, false)
            )])
        );
    }

    #[test]
    fn a_skipped_symlink_name_is_escaped_before_it_reaches_a_warning() {
        // `remote_child_name` refuses control characters outright, but a
        // skipped symlink is only reported -- so the reporting path has to
        // escape the name itself.
        let escaped = sanitize_entry_name("link\u{1b}[31m\r\n");

        assert!(!escaped.chars().any(char::is_control));
        assert!(escaped.starts_with("link"));
    }

    #[test]
    fn a_symlink_with_a_control_character_name_still_aborts_the_listing() {
        let root = tempfile::tempdir().unwrap();
        let mut remote = FakeRemote::default()
            .directory("/remote", vec![file("real.c"), symlink("link\u{1b}name")]);

        // Skipping symlinks must not weaken the unsafe-name check: a control
        // character in a server-supplied name is a server problem whatever the
        // record type, and it is refused before the type is even consulted.
        let error = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::RootDirectory,
        )
        .unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("unsafe remote entry"), "got: {message:?}");
        assert!(!message.chars().any(char::is_control));
    }

    #[test]
    fn a_symlink_still_counts_against_the_duplicate_name_guard() {
        let root = tempfile::tempdir().unwrap();
        // A server contradicting itself: one name, two record types. Dropping
        // the symlink before the guard would let the second record through as
        // an ordinary single-file listing.
        let mut remote =
            FakeRemote::default().directory("/remote", vec![symlink("conf"), file("conf")]);

        let error = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::RootDirectory,
        )
        .unwrap_err();

        assert!(
            format!("{error:#}").contains("duplicate remote entry"),
            "got: {:#}",
            error
        );
    }

    #[test]
    fn inventory_skips_a_remote_symlink_instead_of_aborting() {
        let root = tempfile::tempdir().unwrap();
        let mut remote =
            FakeRemote::default().directory("/remote", vec![file("real.c"), symlink("link.c")]);

        let inventoried = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::RootDirectory,
        )
        .unwrap();

        // Aborting here is the bug the PR set out to fix: one symlink anywhere
        // in the tree took down the whole scoped sync / picker operation.
        assert!(inventoried.entries.contains_key("real.c"));
        assert!(!inventoried.entries.contains_key("link.c"));
    }

    #[test]
    fn inventory_does_not_descend_through_a_symlinked_directory() {
        let root = tempfile::tempdir().unwrap();
        let mut remote = FakeRemote::default()
            .directory("/remote", vec![symlink("elsewhere")])
            .directory("/remote/elsewhere", vec![file("escaped.c")]);

        let inventoried = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::RootDirectory,
        )
        .unwrap();

        assert!(
            inventoried
                .entries
                .keys()
                .all(|key| !key.starts_with("elsewhere"))
        );
        assert_eq!(remote.listed, vec!["/remote"]);
    }

    #[test]
    fn unsafe_remote_names_abort_before_recording_or_listing_children() {
        let unsafe_names = [
            "control\u{1b}name",
            "control\rname",
            "control\nname",
            "control\0name",
            r"protocol\name",
        ];

        for unsafe_name in unsafe_names {
            let root = tempfile::tempdir().unwrap();
            let mut remote =
                FakeRemote::default().directory("/remote", vec![directory(unsafe_name)]);

            let error = inventory(
                &mut remote,
                root.path(),
                &StateFile::default(),
                SyncScope::RootDirectory,
            )
            .unwrap_err();
            let message = format!("{error:#}");

            assert!(message.contains("unsafe remote entry"), "got: {message:?}");
            assert!(
                !message.chars().any(char::is_control),
                "error contains a raw control character: {message:?}"
            );
            assert_eq!(remote.listed, vec!["/remote"]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn local_socket_is_rejected_as_an_unsupported_entry_type() {
        use std::os::unix::net::UnixListener;

        let root = tempfile::tempdir().unwrap();
        let _listener = UnixListener::bind(root.path().join("special.sock")).unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let error = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("special.sock".into()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("unsupported local entry type"));
    }

    #[test]
    fn local_directory_prefix_does_not_make_a_missing_selected_leaf_exist() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("area")).unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let error = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("area/missing.c".into()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("path not found locally or remotely"));
        assert_eq!(remote.listed, vec!["/remote"]);
    }

    #[test]
    fn remote_directory_prefix_does_not_make_a_missing_selected_leaf_exist() {
        let root = tempfile::tempdir().unwrap();
        let mut remote = FakeRemote::default()
            .directory("/remote", vec![directory("area")])
            .directory("/remote/area", vec![]);

        let error = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("area/missing.c".into()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("path not found locally or remotely"));
        assert_eq!(remote.listed, vec!["/remote", "/remote/area"]);
    }

    #[test]
    fn local_file_prefix_is_not_classified_as_the_missing_selected_descendant() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("area"), "ancestor bytes").unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![]);

        let error = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("area/missing.c".into()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("path not found locally or remotely"));
        assert_eq!(
            std::fs::read(root.path().join("area")).unwrap(),
            b"ancestor bytes"
        );
        assert_eq!(remote.size_calls, 0);
    }

    #[test]
    fn remote_file_prefix_is_not_classified_as_the_missing_selected_descendant() {
        let root = tempfile::tempdir().unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![file("area")]);

        let error = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("area/missing.c".into()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("path not found locally or remotely"));
        assert_eq!(remote.listed, vec!["/remote"]);
        assert_eq!(remote.size_calls, 0);
    }

    #[test]
    fn ignored_selected_path_and_state_under_ignored_ancestor_leave_no_inventory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("ignored")).unwrap();
        std::fs::write(root.path().join("ignored/selected.c"), "ignored local").unwrap();
        let mut remote = FakeRemote::default()
            .directory("/remote", vec![directory("ignored")])
            .directory("/remote/ignored", vec![file("selected.c")]);
        let ignored = matcher(root.path(), &["ignored/"]);

        let error = collect(
            &mut remote,
            root.path(),
            "/remote",
            &ignored,
            &state_with(&["ignored/selected.c"]),
            SyncScope::Path("ignored/selected.c".into()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("path not found locally or remotely"));
        assert_eq!(remote.listed, vec!["/remote"]);
        assert_eq!(remote.size_calls, 0);
    }

    #[test]
    fn ignored_directory_name_near_miss_remains_selectable_without_ancestor_entries() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("ignored-old")).unwrap();
        std::fs::write(root.path().join("ignored-old/selected.c"), "kept").unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![]);
        let ignored = matcher(root.path(), &["ignored/"]);

        let actual = collect(
            &mut remote,
            root.path(),
            "/remote",
            &ignored,
            &StateFile::default(),
            SyncScope::Path("ignored-old/selected.c".into()),
        )
        .unwrap();

        assert_eq!(
            actual.entries,
            BTreeMap::from([(
                "ignored-old/selected.c".into(),
                presence(Some(EntryKind::File), None, false),
            )])
        );
    }

    #[test]
    fn remote_selected_leaf_with_local_file_prefix_keeps_only_the_leaf_entry() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("area"), "local ancestor").unwrap();
        let mut remote = FakeRemote::default()
            .directory("/remote", vec![directory("area")])
            .directory("/remote/area", vec![file("selected.c")]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("area/selected.c".into()),
        )
        .unwrap();

        assert_eq!(
            actual.entries,
            BTreeMap::from([(
                "area/selected.c".into(),
                presence(None, Some(EntryKind::File), false),
            )])
        );
    }

    #[test]
    fn local_selected_leaf_with_remote_file_prefix_keeps_only_the_leaf_entry() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("area")).unwrap();
        std::fs::write(root.path().join("area/selected.c"), "local selected").unwrap();
        let mut remote = FakeRemote::default().directory("/remote", vec![file("area")]);

        let actual = inventory(
            &mut remote,
            root.path(),
            &StateFile::default(),
            SyncScope::Path("area/selected.c".into()),
        )
        .unwrap();

        assert_eq!(
            actual.entries,
            BTreeMap::from([(
                "area/selected.c".into(),
                presence(Some(EntryKind::File), None, false),
            )])
        );
        assert_eq!(remote.size_calls, 0);
    }
}
