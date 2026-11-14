use crate::config::Config;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelativeToRoot {
    Root,
    Path(String),
}

#[derive(Debug)]
pub struct ProjectLocation {
    pub config_dir: PathBuf,
    pub config_path: PathBuf,
}

#[derive(Debug)]
pub struct ResolvedFile {
    pub config_dir: PathBuf,
    pub config_path: PathBuf,
    pub config: Config,
    pub relative_path: String,
}

/// Test guard: a test that needs "no ferry project here" can only mean it if
/// no config sits above its temp directory. A stray `.ferry.toml` in /tmp
/// once made three tests fail with nothing pointing at the cause; this names
/// the file instead.
#[cfg(test)]
pub(crate) fn assert_no_config_above(dir: &Path) {
    if let Some(found) = find_config_upward(dir) {
        panic!(
            "a stray ferry config at {} sits above the test's temp directory; \
             remove it (or set TMPDIR elsewhere) and rerun",
            found.config_path.display()
        );
    }
}

pub fn find_config_upward(start: &Path) -> Option<ProjectLocation> {
    let mut dir = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };

    loop {
        let config_path = crate::names::config_path_for_read(&dir);
        if crate::names::entry_is_present(&config_path) {
            return Some(ProjectLocation {
                config_dir: dir,
                config_path,
            });
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub fn resolve_file(path: &Path, migrate_legacy: bool) -> Result<Option<ResolvedFile>> {
    let Some(location) = find_config_upward(path) else {
        return Ok(None);
    };

    if migrate_legacy && let Err(error) = crate::names::migrate_legacy(&location.config_dir) {
        eprintln!(
            "ferry: warning: migrating legacy paths in {}: {error:#}",
            location.config_dir.display()
        );
    }

    let config_path = crate::names::config_path_for_read(&location.config_dir);
    let config = Config::load(&config_path)?;

    if migrate_legacy && let Err(error) = crate::names::migrate_legacy(&config.paths.local_root) {
        eprintln!(
            "ferry: warning: migrating legacy paths in {}: {error:#}",
            config.paths.local_root.display()
        );
    }

    let relative_path = relative_to_local_root(&config.paths.local_root, path)?;
    Ok(Some(ResolvedFile {
        config_dir: location.config_dir,
        config_path,
        config,
        relative_path,
    }))
}

fn canonicalize_path_or_new_file(path: &Path) -> Result<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    let target = std::fs::read_link(path)
                        .with_context(|| format!("reading symlink {}", path.display()))?;
                    let target = if target.is_absolute() {
                        target
                    } else {
                        path.parent()
                            .context("symlink path has no parent")?
                            .join(target)
                    };
                    canonicalize_path_or_new_file(&target)
                }
                Ok(_) => Err(error)
                    .with_context(|| format!("canonicalizing existing path {}", path.display())),
                Err(metadata_error) if metadata_error.kind() == std::io::ErrorKind::NotFound => {
                    let parent = path.parent().context("new file path has no parent")?;
                    let name = path.file_name().context("new file path has no file name")?;
                    Ok(canonicalize_path_or_new_file(parent)?.join(name))
                }
                Err(metadata_error) => {
                    Err(metadata_error).with_context(|| format!("checking path {}", path.display()))
                }
            }
        }
        Err(error) => Err(error).with_context(|| format!("canonicalizing file {}", path.display())),
    }
}

pub fn relative_to_local_root_or_root(local_root: &Path, path: &Path) -> Result<RelativeToRoot> {
    let local_root = local_root
        .canonicalize()
        .with_context(|| format!("canonicalizing local_root {}", local_root.display()))?;
    let path = canonicalize_path_or_new_file(path)?;

    let relative = path.strip_prefix(&local_root).map_err(|_| {
        anyhow::anyhow!(
            "file {} is outside local_root {}",
            path.display(),
            local_root.display()
        )
    })?;
    if relative.as_os_str().is_empty() {
        return Ok(RelativeToRoot::Root);
    }
    let relative = relative
        .to_str()
        .context("relative path is not valid UTF-8")?;
    #[cfg(windows)]
    let relative = relative.replace('\\', "/");
    #[cfg(not(windows))]
    let relative = relative.to_owned();
    crate::commands::walk::safe_rel(&relative).map(RelativeToRoot::Path)
}

pub fn relative_to_local_root(local_root: &Path, path: &Path) -> Result<String> {
    match relative_to_local_root_or_root(local_root, path)? {
        RelativeToRoot::Path(path) => Ok(path),
        RelativeToRoot::Root => anyhow::bail!(
            "file {} resolves to local_root itself; expected a descendant path",
            path.display()
        ),
    }
}

/// Where `cwd` sits inside `local_root`, as a project-relative path: `""` at
/// the root itself, `None` when `cwd` is not inside the project at all.
pub fn cwd_offset(local_root: &Path, cwd: &Path) -> Option<String> {
    let local_root = local_root.canonicalize().ok()?;
    let cwd = cwd.canonicalize().ok()?;
    let relative = cwd.strip_prefix(&local_root).ok()?;
    let relative = relative.to_str()?;
    #[cfg(windows)]
    let relative = relative.replace('\\', "/");
    Some(relative.to_owned())
}

/// Rebase CLI path arguments the way git does: a relative argument names a
/// path relative to the working directory (`offset`, from [`cwd_offset`]),
/// and comes back relative to the project root. Absolute arguments pass
/// through for the commands to contain as before.
///
/// An empty result means "the whole project". With `default_to_cwd`, no
/// arguments in a subdirectory scope to that subdirectory; an argument that
/// resolves to the root itself widens the result to the whole project.
/// Without an offset (working directory outside `local_root`) arguments keep
/// their historical root-relative meaning.
pub fn rebase_cli_paths(
    offset: Option<&str>,
    args: &[String],
    default_to_cwd: bool,
) -> Result<Vec<String>> {
    let Some(offset) = offset else {
        return Ok(args.to_vec());
    };
    if args.is_empty() {
        return Ok(if default_to_cwd && !offset.is_empty() {
            vec![offset.to_string()]
        } else {
            Vec::new()
        });
    }

    let mut rebased = Vec::with_capacity(args.len());
    for arg in args {
        if Path::new(arg).is_absolute() {
            rebased.push(arg.clone());
            continue;
        }
        let mut segments: Vec<&str> = offset.split('/').filter(|s| !s.is_empty()).collect();
        for segment in arg.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    if segments.pop().is_none() {
                        anyhow::bail!("refusing path {arg:?}: it resolves outside the project");
                    }
                }
                name => segments.push(name),
            }
        }
        if segments.is_empty() {
            return Ok(Vec::new());
        }
        rebased.push(segments.join("/"));
    }
    Ok(rebased)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names::{CONFIG_FILE, LEGACY_CONFIG_FILE, LEGACY_STATE_DIR, STATE_DIR};

    fn write_config(dir: &Path, local_root: &str) {
        std::fs::write(
            dir.join(CONFIG_FILE),
            format!(
                "[connection]\nhost = \"h\"\nuser = \"u\"\npassword = \"p\"\n\
                 [paths]\nlocal_root = \"{local_root}\"\nremote_root = \"/\"\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn nearest_nested_config_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path();
        let inner = outer.join("nested");
        std::fs::create_dir_all(inner.join("mirror")).unwrap();
        write_config(outer, ".");
        write_config(&inner, "mirror");
        let file = inner.join("mirror/file.c");
        std::fs::write(&file, "").unwrap();

        let resolved = resolve_file(&file, false).unwrap().unwrap();
        assert_eq!(resolved.config_dir, inner);
        assert_eq!(resolved.relative_path, "file.c");
    }

    #[test]
    fn maps_file_through_descendant_local_root() {
        let tmp = tempfile::tempdir().unwrap();
        let mirror = tmp.path().join("mirror/sub");
        std::fs::create_dir_all(&mirror).unwrap();
        write_config(tmp.path(), "mirror");
        let file = mirror.join("room.c");
        std::fs::write(&file, "").unwrap();

        let resolved = resolve_file(&file, false).unwrap().unwrap();
        assert_eq!(resolved.relative_path, "sub/room.c");
    }

    #[test]
    fn rejects_file_outside_configured_local_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("mirror")).unwrap();
        write_config(tmp.path(), "mirror");
        let file = tmp.path().join("outside.c");
        std::fs::write(&file, "").unwrap();

        let error = resolve_file(&file, false).unwrap_err();
        assert!(error.to_string().contains("outside local_root"));
    }

    #[test]
    fn discovers_legacy_config() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("one/two");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(tmp.path().join(LEGACY_CONFIG_FILE), "legacy").unwrap();

        let location = find_config_upward(&nested).unwrap();
        assert_eq!(location.config_dir, tmp.path());
        assert_eq!(location.config_path, tmp.path().join(LEGACY_CONFIG_FILE));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_current_config_stops_upward_discovery() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        write_config(tmp.path(), ".");
        let current = nested.join(CONFIG_FILE);
        symlink(nested.join("missing.toml"), &current).unwrap();
        let file = nested.join("file.c");
        std::fs::write(&file, "").unwrap();

        let location = find_config_upward(&nested).unwrap();
        assert_eq!(location.config_dir, nested);
        assert_eq!(location.config_path, current);
        let error = resolve_file(&file, false).unwrap_err();
        assert!(error.to_string().contains("reading"));
        assert!(error.to_string().contains(CONFIG_FILE));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_legacy_config_stops_upward_discovery() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        write_config(tmp.path(), ".");
        let legacy = nested.join(LEGACY_CONFIG_FILE);
        symlink(nested.join("missing.toml"), &legacy).unwrap();

        let location = find_config_upward(&nested).unwrap();
        assert_eq!(location.config_dir, nested);
        assert_eq!(location.config_path, legacy);
    }

    #[test]
    fn returns_none_without_a_config() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file.c");
        std::fs::write(&file, "").unwrap();
        assert_no_config_above(tmp.path());

        assert!(resolve_file(&file, false).unwrap().is_none());
    }

    #[test]
    fn resolves_a_new_file_through_its_existing_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let mirror = tmp.path().join("mirror");
        std::fs::create_dir(&mirror).unwrap();
        write_config(tmp.path(), "mirror");

        let resolved = resolve_file(&mirror.join("new.c"), false).unwrap().unwrap();
        assert_eq!(resolved.relative_path, "new.c");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape_from_local_root() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let mirror = tmp.path().join("mirror");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&mirror).unwrap();
        std::fs::create_dir(&outside).unwrap();
        write_config(tmp.path(), "mirror");
        let escaped = outside.join("escaped.c");
        std::fs::write(&escaped, "").unwrap();
        symlink(&escaped, mirror.join("link.c")).unwrap();

        let error = resolve_file(&mirror.join("link.c"), false).unwrap_err();
        assert!(error.to_string().contains("outside local_root"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_dangling_symlink_escape_from_local_root() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let mirror = tmp.path().join("mirror");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&mirror).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let dangling_target = outside.join("new.c");
        symlink(&dangling_target, mirror.join("link.c")).unwrap();

        let error = relative_to_local_root(&mirror, &mirror.join("link.c")).unwrap_err();
        assert!(error.to_string().contains("outside local_root"));
    }

    #[test]
    fn root_aware_absolute_root_returns_root() {
        let root = tempfile::tempdir().unwrap();

        assert_eq!(
            relative_to_local_root_or_root(root.path(), root.path()).unwrap(),
            RelativeToRoot::Root
        );
    }

    #[test]
    fn root_aware_absolute_descendant_returns_path() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let file = nested.join("file.c");
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            relative_to_local_root_or_root(root.path(), &file).unwrap(),
            RelativeToRoot::Path("nested/file.c".into())
        );
    }

    #[test]
    fn root_aware_accepts_new_descendant_via_existing_ancestor() {
        let root = tempfile::tempdir().unwrap();

        assert_eq!(
            relative_to_local_root_or_root(root.path(), &root.path().join("new/nested/file.c"))
                .unwrap(),
            RelativeToRoot::Path("new/nested/file.c".into())
        );
    }

    #[test]
    fn root_aware_rejects_absolute_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside.c");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(&outside, "").unwrap();

        let error = relative_to_local_root_or_root(&root, &outside).unwrap_err();
        assert!(error.to_string().contains("outside local_root"));
    }

    #[cfg(unix)]
    #[test]
    fn root_aware_rejects_dangling_symlink_escape() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(outside.join("new.c"), root.join("link.c")).unwrap();

        let error = relative_to_local_root_or_root(&root, &root.join("link.c")).unwrap_err();
        assert!(error.to_string().contains("outside local_root"));
    }

    #[test]
    fn root_aware_legacy_resolver_still_rejects_root() {
        let root = tempfile::tempdir().unwrap();

        let error = relative_to_local_root(root.path(), root.path()).unwrap_err();
        assert!(error.to_string().contains("local_root itself"));
    }

    #[cfg(unix)]
    #[test]
    fn root_aware_rejects_non_utf8_descendant() {
        use std::os::unix::ffi::OsStringExt;

        let root = tempfile::tempdir().unwrap();
        let name = std::ffi::OsString::from_vec(b"bad-\xff.c".to_vec());
        let path = root.path().join(name);
        std::fs::write(&path, "").unwrap();

        let error = relative_to_local_root_or_root(root.path(), &path).unwrap_err();
        assert!(error.to_string().contains("UTF-8"));
    }

    #[test]
    fn migration_moves_legacy_state_from_descendant_local_root() {
        let tmp = tempfile::tempdir().unwrap();
        let mirror = tmp.path().join("mirror");
        std::fs::create_dir(&mirror).unwrap();
        write_config(tmp.path(), "mirror");
        std::fs::create_dir(mirror.join(STATE_DIR)).unwrap();
        std::fs::create_dir(mirror.join(LEGACY_STATE_DIR)).unwrap();
        let legacy_state = mirror.join(LEGACY_STATE_DIR).join("state.json");
        std::fs::write(&legacy_state, "legacy-state").unwrap();
        let file = mirror.join("file.c");
        std::fs::write(&file, "").unwrap();

        resolve_file(&file, true).unwrap().unwrap();

        assert_eq!(
            std::fs::read_to_string(mirror.join(STATE_DIR).join("state.json")).unwrap(),
            "legacy-state"
        );
        assert!(!legacy_state.exists());
        assert!(!mirror.join(LEGACY_STATE_DIR).exists());
    }

    #[cfg(unix)]
    #[test]
    fn preserves_literal_backslash_in_unix_relative_path() {
        use std::os::unix::ffi::OsStringExt;

        let tmp = tempfile::tempdir().unwrap();
        let name = std::ffi::OsString::from_vec(b"a\\b.c".to_vec());
        let path = tmp.path().join(&name);
        std::fs::write(&path, "").unwrap();

        assert_eq!(relative_to_local_root(tmp.path(), &path).unwrap(), "a\\b.c");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_relative_path() {
        use std::os::unix::ffi::OsStringExt;

        let tmp = tempfile::tempdir().unwrap();
        let name = std::ffi::OsString::from_vec(b"bad-\xff.c".to_vec());
        let path = tmp.path().join(name);
        std::fs::write(&path, "").unwrap();

        let error = relative_to_local_root(tmp.path(), &path).unwrap_err();
        assert!(error.to_string().contains("UTF-8"));
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    fn rebased(offset: Option<&str>, list: &[&str], default_to_cwd: bool) -> Vec<String> {
        rebase_cli_paths(offset, &args(list), default_to_cwd).unwrap()
    }

    #[test]
    fn cwd_offset_locates_the_cwd_inside_local_root() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("players/knights");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            cwd_offset(tmp.path(), &nested),
            Some("players/knights".to_string())
        );
        assert_eq!(cwd_offset(tmp.path(), tmp.path()), Some(String::new()));
    }

    #[test]
    fn cwd_offset_is_none_outside_local_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("mirror");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();

        assert_eq!(cwd_offset(&root, &elsewhere), None);
        assert_eq!(cwd_offset(&root, tmp.path()), None);
    }

    #[cfg(unix)]
    #[test]
    fn cwd_offset_sees_through_a_symlinked_local_root() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(
            cwd_offset(&link, &real.join("sub")),
            Some("sub".to_string())
        );
    }

    #[test]
    fn relative_args_are_taken_from_the_cwd() {
        let offset = Some("players/knights");
        assert_eq!(
            rebased(offset, &["castle.c"], true),
            ["players/knights/castle.c"]
        );
        assert_eq!(rebased(offset, &["."], true), ["players/knights"]);
        assert_eq!(
            rebased(offset, &["./gobj/"], true),
            ["players/knights/gobj"]
        );
        assert_eq!(rebased(offset, &[".."], true), ["players"]);
        assert_eq!(rebased(offset, &["../../NEWS"], true), ["NEWS"]);
        assert_eq!(
            rebased(offset, &["a/./b/../c"], true),
            ["players/knights/a/c"]
        );
    }

    #[test]
    fn no_args_in_a_subdirectory_scope_to_it_when_defaulting() {
        assert_eq!(
            rebased(Some("players/knights"), &[], true),
            ["players/knights"]
        );
        assert!(rebased(Some("players/knights"), &[], false).is_empty());
        assert!(rebased(Some(""), &[], true).is_empty());
    }

    // Empty means "the whole project" to every scoped command.
    #[test]
    fn an_arg_naming_the_root_means_the_whole_project() {
        assert!(rebased(Some("players/knights"), &["../.."], true).is_empty());
        assert!(rebased(Some("players/knights"), &["../..", "castle.c"], true).is_empty());
        assert!(rebased(Some(""), &["."], true).is_empty());
    }

    #[test]
    fn args_escaping_the_project_are_refused() {
        let error =
            rebase_cli_paths(Some("players"), &args(&["../../etc/passwd"]), true).unwrap_err();
        assert!(
            format!("{error:#}").contains("outside the project"),
            "{error:#}"
        );
    }

    #[test]
    fn absolute_args_pass_through_untouched() {
        assert_eq!(
            rebased(Some("players"), &["/home/simon/code/3s/NEWS"], true),
            ["/home/simon/code/3s/NEWS"]
        );
    }

    // Outside local_root there is no offset to apply: keep the historical
    // root-relative meaning rather than guess.
    #[test]
    fn without_an_offset_args_keep_their_root_relative_meaning() {
        assert_eq!(rebased(None, &["players/x.c"], true), ["players/x.c"]);
        assert!(rebased(None, &[], true).is_empty());
    }
}
