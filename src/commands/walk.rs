use crate::ftp::Remote;
use crate::ignored::Matcher;
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::Path;

/// Join a remote root with a relative path, trimming any trailing slash on the
/// root so we never produce `//foo`.
pub fn remote_join(root: &str, rel: &str) -> String {
    let root = root.trim_end_matches('/');
    format!("{}/{}", root, rel)
}

/// Normalize and validate a user-supplied path argument into the relative form
/// used as state-file keys: forward slashes, no leading `./`, no trailing `/`.
///
/// Rejects paths that are empty, absolute, or contain a `..` segment — these
/// could escape the sync roots. Shared by `push` and `rm` so both enforce the
/// same containment rule.
pub fn safe_rel(p: &str) -> Result<String> {
    #[cfg(windows)]
    let s = p.replace('\\', "/");
    #[cfg(not(windows))]
    let s = p.to_owned();
    let rel = s.trim_start_matches("./").to_string();
    if rel.is_empty() || Path::new(&rel).is_absolute() || rel.split('/').any(|c| c == "..") {
        anyhow::bail!(
            "refusing path {p:?}: must be a relative path under local_root with no '..' segments"
        );
    }
    Ok(rel.trim_end_matches('/').to_string())
}

/// Normalize a path argument relative to `local_root`. Absolute paths are
/// accepted only when their canonical location is contained by that root.
pub fn safe_arg(local_root: &Path, input: &str) -> Result<String> {
    let path = Path::new(input);
    if path.is_absolute() {
        return crate::project::relative_to_local_root(local_root, path);
    }
    safe_rel(input)
}

/// Walk the local mirror, populating `out` with relative paths (forward-slash
/// separated). Skips files matched by the ignore matcher and the `.ferry`
/// state directory.
pub fn walk_local(
    root: &Path,
    dir: &Path,
    matcher: &Matcher,
    out: &mut BTreeSet<String>,
) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("reading local dir {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("walking local dir {}", dir.display()))?;
        let path = entry.path();
        let is_dir = path.is_dir();
        if matcher.is_ignored(&path, is_dir) {
            continue;
        }
        if is_dir {
            // Skip the state directory itself.
            if path.file_name().and_then(|s| s.to_str()) == Some(crate::names::STATE_DIR) {
                continue;
            }
            walk_local(root, &path, matcher, out)?;
        } else {
            let rel = path.strip_prefix(root)?.to_string_lossy().into_owned();
            // normalize separators on windows; not strictly needed on linux but keeps state portable
            #[cfg(windows)]
            let rel = rel.replace('\\', "/");
            out.insert(rel);
        }
    }
    Ok(())
}

/// Walk the remote tree, populating `out` with relative paths beneath `root`.
///
/// Per-directory listing failures are logged to stderr but do not abort the
/// walk. This is defensive against decades-old FTP trees with dangling
/// symlinks, permission-denied subfolders, and other cruft — one bad
/// subdirectory shouldn't kill the whole operation.
///
/// The top-level call still returns Err if the starting directory itself
/// fails to list — that's a real "the target doesn't exist" signal that
/// callers need.
pub fn walk_remote<R: Remote + ?Sized>(
    ftp: &mut R,
    root: &str,
    sub: &str,
    out: &mut BTreeSet<String>,
) -> Result<()> {
    let mut symlinks = BTreeSet::new();
    walk_remote_with_symlinks(ftp, root, sub, out, &mut symlinks)
}

/// As [`walk_remote`], but also reports the relative path of every entry the
/// walk skipped because the server described it as a symlink.
///
/// Callers that write must consult this. A skipped symlink is absent from
/// `out`, and "absent" otherwise classifies as `LocalOnly` — which uploads,
/// and the server resolves the link, putting the write outside the configured
/// remote root. Read-only callers can use [`walk_remote`] and drop the set.
pub fn walk_remote_with_symlinks<R: Remote + ?Sized>(
    ftp: &mut R,
    root: &str,
    sub: &str,
    out: &mut BTreeSet<String>,
    symlinks: &mut BTreeSet<String>,
) -> Result<()> {
    let root_dir = listable_remote_root(root);
    let dir = if sub.is_empty() {
        root_dir
    } else {
        // `root_dir` is "/" or already trimmed, so this trim only ever turns
        // "/" back into "" -- which is exactly what keeps "/" + "a/b" from
        // becoming "//a/b". A doubled slash survives LIST on some servers but
        // then fails `child_name`'s absolute-echo prefix check, silently
        // dropping every child of the walk.
        format!("{}/{}", root_dir.trim_end_matches('/'), sub)
    };
    // `sub` may name a single file rather than a directory — `pull`/`push`/`rm`
    // all pass user path args straight through. Settle that with SIZE before
    // listing, because servers disagree about how to answer `LIST <file>` and
    // every answer is ambiguous by the time it reaches the walk: ProFTPD echoes
    // the argument back as the filename, vsftpd replies with the bare basename
    // (indistinguishable from a real child), and others refuse outright.
    if !sub.is_empty() && ftp.file_size(&dir).is_ok() {
        // SIZE succeeds on a symlink to a file, so it cannot tell a real file
        // from a link. One listing of the parent settles it. This costs a round
        // trip only for an explicit path argument, never per file in a walk.
        match leaf_is_symlink(ftp, root, &dir) {
            Some(true) => {
                warn_skipping_symlink(sub);
                symlinks.insert(sub.to_string());
                return Ok(());
            }
            Some(false) => {}
            None => eprintln!(
                "warning: cannot confirm whether remote {sub:?} is a symlink: \
                 its parent directory would not list"
            ),
        }
        out.insert(sub.to_string());
        return Ok(());
    }
    walk_remote_inner(
        ftp, root, sub, &dir, out, symlinks, /* top_level = */ true,
    )
}

/// The directory to hand `LIST` for a configured `remote_root`.
///
/// Preserving "/" matters: trimming it to an empty string makes `LIST` use the
/// server's current directory, which on a non-chrooted server is the login home
/// rather than the root — so the top level came from one subtree while every
/// child path was built as `/name` from another.
///
/// An empty `remote_root` normalizes to "/" for the same reason
/// `remote_join("", rel)` already yields `/rel`. It is not rejected here
/// because this is also the read path; `sync` and `init` reject it outright.
///
/// Note: `file_transfer::normalize_remote_root` and
/// `sync::inventory::normalize_remote_root` are two further copies of this
/// logic. Consolidating all three (ideally by normalizing once in
/// `Config::load`, which already does this for `local_root`) is worth a
/// follow-up, but is deliberately out of scope here.
fn listable_remote_root(root: &str) -> String {
    let trimmed = root.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn warn_skipping_symlink(relative: &str) {
    eprintln!(
        "warning: skipping remote symlink {relative:?}: not following it, the target can resolve outside the remote root"
    );
}

/// Ask the server whether the leaf of `dir` is a symlink, using one listing of
/// its parent directory.
///
/// `None` means the question could not be answered — the parent would not list,
/// or it does not name the leaf at all. Callers keep their previous behaviour in
/// that case rather than turning a listing hiccup into a refusal.
pub fn leaf_is_symlink<R: Remote + ?Sized>(ftp: &mut R, root: &str, dir: &str) -> Option<bool> {
    let trimmed = dir.trim_end_matches('/');
    let (parent, leaf) = trimmed.rsplit_once('/')?;
    if leaf.is_empty() {
        return None;
    }
    let parent = if parent.is_empty() { "/" } else { parent };
    let entries = ftp.list_dir(parent).ok()?;
    for entry in entries {
        if child_name(root, parent, &entry.name) == Some(leaf) {
            return Some(entry.is_symlink);
        }
    }
    None
}

/// Resolve one user-supplied path arg into `out`, covering both the directory
/// case (walk the subtree) and the single-file case, and return how many
/// entries were added.
///
/// The SIZE fallback fires when the walk *finds nothing*, not merely when it
/// errors. A server that happily lists a file returns `Ok` with no usable
/// entries, which is indistinguishable here from an empty directory — gating
/// the fallback on `Err` alone left `pull` reporting "not on local or remote"
/// for files that plainly exist, and left `push` treating an existing remote
/// file as `LocalOnly` and uploading over it without a conflict check.
///
/// Shared by `pull` and `push` so the two can't drift apart on this.
pub fn collect_remote_arg<R: Remote + ?Sized>(
    ftp: &mut R,
    root: &str,
    rel: &str,
    out: &mut BTreeSet<String>,
) -> usize {
    let mut symlinks = BTreeSet::new();
    collect_remote_arg_with_symlinks(ftp, root, rel, out, &mut symlinks)
}

/// As [`collect_remote_arg`], but reports skipped symlinks the way
/// [`walk_remote_with_symlinks`] does, and never lets the `SIZE` fallback
/// resurrect one as a transfer target.
pub fn collect_remote_arg_with_symlinks<R: Remote + ?Sized>(
    ftp: &mut R,
    root: &str,
    rel: &str,
    out: &mut BTreeSet<String>,
    symlinks: &mut BTreeSet<String>,
) -> usize {
    let before = out.len();
    let walked = walk_remote_with_symlinks(ftp, root, rel, out, symlinks);
    // `SIZE` cannot distinguish a file from a symlink to one, so a path the
    // walk already identified as a link must not come back through here.
    if (walked.is_err() || out.len() == before) && !symlinks.contains(rel) {
        let remote_path = remote_join(root, rel);
        if ftp.file_size(&remote_path).is_ok() {
            out.insert(rel.to_string());
        }
    }
    out.len() - before
}

/// True when `path` is `root` itself or sits beneath it. Compares on segment
/// boundaries so `/rootless` doesn't count as being under `/root`. An empty
/// root (i.e. `remote_root = "/"`) contains everything.
fn is_under(root: &str, path: &str) -> bool {
    let root = root.trim_end_matches('/');
    root.is_empty() || path == root || path.strip_prefix(root).is_some_and(|r| r.starts_with('/'))
}

/// Resolve a server-supplied listing name into the name of a direct child of
/// `dir`, or `None` if it can't be trusted.
///
/// Most servers reply with a bare filename, but some echo a full path — either
/// absolute or relative to the listed directory. Those are accepted as long as
/// they still denote a direct child inside `root`; anything that would steer
/// the walk elsewhere (a sibling tree, a parent, a deeper grandchild) is
/// rejected so a corrupt or hostile listing can't escape the sync root.
fn child_name<'a>(root: &str, dir: &str, name: &'a str) -> Option<&'a str> {
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    if !name.contains('/') {
        return Some(name);
    }
    let dir = dir.trim_end_matches('/');
    let rest = if name.starts_with('/') {
        // Absolute: must be exactly `dir` plus one more segment. Stripping the
        // separator separately is what stops `/rootless/x` from passing as a
        // child of `/root`.
        name.strip_prefix(dir)?.strip_prefix('/')?
    } else {
        name
    };
    if rest.is_empty() || rest.contains('/') || rest == "." || rest == ".." {
        return None;
    }
    is_under(root, &format!("{dir}/{rest}")).then_some(rest)
}

#[allow(clippy::too_many_arguments)]
fn walk_remote_inner<R: Remote + ?Sized>(
    ftp: &mut R,
    root: &str,
    sub: &str,
    dir: &str,
    out: &mut BTreeSet<String>,
    symlinks: &mut BTreeSet<String>,
    top_level: bool,
) -> Result<()> {
    let entries = match ftp.list_dir(dir) {
        Ok(e) => e,
        Err(e) if top_level => {
            return Err(e).with_context(|| format!("walking remote dir {dir}"));
        }
        Err(e) => {
            eprintln!("warning: skipping remote dir {dir}: {e:#}");
            return Ok(());
        }
    };
    for entry in entries {
        if entry.name == "." || entry.name == ".." {
            continue;
        }
        // A symlink is not syncable: FTP carries no link records, and the
        // server resolves the target, which can sit outside the remote root.
        // Record it so write paths refuse the path instead of reading its
        // absence from `out` as "safe to create".
        if entry.is_symlink {
            if let Some(name) = child_name(root, dir, &entry.name) {
                let child_sub = if sub.is_empty() {
                    name.to_string()
                } else {
                    format!("{}/{}", sub, name)
                };
                warn_skipping_symlink(&child_sub);
                symlinks.insert(child_sub);
            } else {
                eprintln!(
                    "warning: skipping remote symlink {:?} in {dir}: not a path under the sync root",
                    entry.name
                );
            }
            continue;
        }
        // A server that answers `LIST <file>` by describing the file itself.
        // Normally the SIZE probe in `walk_remote` resolves a file arg before
        // we get here; this covers servers without SIZE support. Only the
        // absolute-echo form is detectable — a bare basename is
        // indistinguishable from a genuine child of the same name, so it is
        // left to the SIZE probe rather than guessed at.
        if top_level
            && !sub.is_empty()
            && entry.name.trim_end_matches('/') == dir.trim_end_matches('/')
        {
            out.insert(sub.to_string());
            continue;
        }
        let Some(name) = child_name(root, dir, &entry.name) else {
            eprintln!(
                "warning: skipping remote entry {:?} in {dir}: not a path under the sync root",
                entry.name
            );
            continue;
        };
        let child_sub = if sub.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", sub, name)
        };
        if entry.is_dir {
            let child_dir = format!("{}/{}", dir.trim_end_matches('/'), name);
            let _ = walk_remote_inner(ftp, root, &child_sub, &child_dir, out, symlinks, false);
        } else {
            out.insert(child_sub);
        }
    }
    Ok(())
}

#[cfg(test)]
mod walk_remote_tests {
    use super::*;
    use crate::ftp::{Entry, Remote};
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;

    /// A fake server built from a path -> children map. Paths present as keys
    /// are directories; paths listed in `files` are files. `echo` selects how
    /// `LIST <file>` answers, which is the behaviour real servers differ on.
    #[derive(Clone, Copy, PartialEq)]
    enum Echo {
        /// ProFTPD: echoes the argument it was given, verbatim.
        AbsolutePath,
        /// vsftpd: replies with the file's bare basename.
        Basename,
        /// A server that refuses to LIST a non-directory at all.
        Error,
    }

    struct Fake {
        dirs: HashMap<String, Vec<(String, bool)>>,
        files: Vec<String>,
        echo: Echo,
        supports_size: bool,
        /// Extra entries injected into a directory's listing, used to exercise
        /// the containment check.
        inject: HashMap<String, Vec<(String, bool)>>,
        /// Per-directory child names the server describes as POSIX symlinks.
        symlinks: HashMap<String, Vec<String>>,
        pub listed: Vec<String>,
    }

    fn entry(name: &str, is_dir: bool) -> Entry {
        Entry {
            name: name.to_string(),
            is_dir,
            is_symlink: false,
            size: 1,
            modified: Utc.with_ymd_and_hms(2026, 7, 31, 0, 0, 0).unwrap(),
        }
    }

    fn symlink_entry(name: &str) -> Entry {
        Entry {
            name: name.to_string(),
            is_dir: false,
            is_symlink: true,
            size: 1,
            modified: Utc.with_ymd_and_hms(2026, 7, 31, 0, 0, 0).unwrap(),
        }
    }

    impl Fake {
        fn new(echo: Echo) -> Self {
            let mut dirs: HashMap<String, Vec<(String, bool)>> = HashMap::new();
            // /root is the sync root; /root/sub is a real directory.
            dirs.insert(
                "/root".into(),
                vec![
                    ("a.txt".into(), false),
                    ("sub".into(), true),
                    ("finger_d.20260729".into(), false),
                ],
            );
            dirs.insert("/root/sub".into(), vec![("b.txt".into(), false)]);
            Self {
                dirs,
                files: vec![
                    "/root/a.txt".into(),
                    "/root/finger_d.20260729".into(),
                    "/root/sub/b.txt".into(),
                ],
                echo,
                supports_size: true,
                inject: HashMap::new(),
                symlinks: HashMap::new(),
                listed: Vec::new(),
            }
        }
    }

    impl Remote for Fake {
        fn list_dir(&mut self, dir: &str) -> Result<Vec<Entry>> {
            self.listed.push(dir.to_string());
            if let Some(children) = self.dirs.get(dir) {
                let mut out: Vec<Entry> = children.iter().map(|(n, d)| entry(n, *d)).collect();
                if let Some(extra) = self.inject.get(dir) {
                    out.extend(extra.iter().map(|(n, d)| entry(n, *d)));
                }
                if let Some(links) = self.symlinks.get(dir) {
                    out.extend(links.iter().map(|n| symlink_entry(n)));
                }
                return Ok(out);
            }
            if self.files.iter().any(|f| f == dir) {
                // LIST against a file: each server answers differently.
                return match self.echo {
                    Echo::AbsolutePath => Ok(vec![entry(dir, false)]),
                    Echo::Basename => {
                        let base = dir.rsplit('/').next().unwrap_or(dir);
                        Ok(vec![entry(base, false)])
                    }
                    Echo::Error => anyhow::bail!("550 {dir}: Not a directory"),
                };
            }
            anyhow::bail!("550 {dir}: No such file or directory")
        }

        fn file_size(&mut self, path: &str) -> Result<u64> {
            if !self.supports_size {
                anyhow::bail!("500 SIZE not understood");
            }
            if self.files.iter().any(|f| f == path) {
                Ok(1)
            } else {
                anyhow::bail!("550 {path}: not a regular file")
            }
        }
    }

    fn walk(f: &mut Fake, sub: &str) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        walk_remote(f, "/root", sub, &mut out)?;
        Ok(out)
    }

    #[test]
    fn walks_whole_tree_from_root() {
        let mut f = Fake::new(Echo::AbsolutePath);
        let out = walk(&mut f, "").unwrap();
        assert_eq!(
            out.into_iter().collect::<Vec<_>>(),
            vec!["a.txt", "finger_d.20260729", "sub/b.txt"]
        );
    }

    #[test]
    fn preserves_slash_as_the_ftp_root() {
        let mut f = Fake::new(Echo::AbsolutePath);
        f.dirs.clear();
        f.dirs.insert("/".into(), vec![("players".into(), true)]);
        f.dirs
            .insert("/players".into(), vec![("skuggis".into(), true)]);
        f.dirs
            .insert("/players/skuggis".into(), vec![("castle.c".into(), false)]);
        f.files = vec!["/players/skuggis/castle.c".into()];

        let mut out = BTreeSet::new();
        walk_remote(&mut f, "/", "", &mut out).unwrap();

        assert_eq!(
            out.into_iter().collect::<Vec<_>>(),
            vec!["players/skuggis/castle.c"]
        );
        assert_eq!(f.listed.first().map(String::as_str), Some("/"));
    }

    #[test]
    fn slash_root_joins_a_subpath_without_doubling_the_separator() {
        let mut f = Fake::new(Echo::AbsolutePath);
        f.dirs.clear();
        f.dirs
            .insert("/players/skuggis".into(), vec![("castle.c".into(), false)]);

        let mut out = BTreeSet::new();
        walk_remote(&mut f, "/", "players/skuggis", &mut out).unwrap();

        // `//players/skuggis` would survive LIST on some servers but then fail
        // `child_name`'s absolute-echo prefix check, silently dropping every
        // child of every subpath walk under `remote_root = "/"`.
        assert_eq!(f.listed, ["/players/skuggis"]);
        assert_eq!(
            out.into_iter().collect::<Vec<_>>(),
            vec!["players/skuggis/castle.c"]
        );
    }

    #[test]
    fn leaf_is_symlink_answers_from_one_listing_of_the_parent() {
        let mut f = Fake::new(Echo::AbsolutePath);
        f.symlinks.insert("/root".into(), vec!["link.c".into()]);

        assert_eq!(leaf_is_symlink(&mut f, "/root", "/root/link.c"), Some(true));
        assert_eq!(leaf_is_symlink(&mut f, "/root", "/root/a.txt"), Some(false));
        assert_eq!(f.listed, ["/root", "/root"]);
    }

    #[test]
    fn leaf_is_symlink_is_unknown_when_the_parent_will_not_list() {
        let mut f = Fake::new(Echo::AbsolutePath);

        // No answer is not the same as "safe": callers must keep their prior
        // behaviour rather than read `None` as `Some(false)`.
        assert_eq!(leaf_is_symlink(&mut f, "/root", "/gone/link.c"), None);
    }

    #[test]
    fn leaf_is_symlink_is_unknown_when_the_parent_does_not_name_the_leaf() {
        let mut f = Fake::new(Echo::AbsolutePath);

        assert_eq!(leaf_is_symlink(&mut f, "/root", "/root/absent.c"), None);
    }

    #[test]
    fn walk_skips_a_remote_symlink_but_keeps_its_siblings() {
        let mut f = Fake::new(Echo::AbsolutePath);
        f.symlinks.insert("/root".into(), vec!["link.c".into()]);

        let out = walk(&mut f, "").unwrap();

        // The link is not syncable; the real files beside it still are.
        assert!(!out.contains("link.c"));
        assert!(out.contains("a.txt"));
        assert!(out.contains("sub/b.txt"));
    }

    #[test]
    fn walk_reports_the_relative_path_of_every_skipped_symlink() {
        let mut f = Fake::new(Echo::AbsolutePath);
        f.symlinks.insert("/root".into(), vec!["link.c".into()]);
        f.symlinks.insert("/root/sub".into(), vec!["deep.c".into()]);

        let mut out = BTreeSet::new();
        let mut symlinks = BTreeSet::new();
        walk_remote_with_symlinks(&mut f, "/root", "", &mut out, &mut symlinks).unwrap();

        // Write paths need these to refuse the upload. Treating them as merely
        // absent is what lets a STOR follow the link out of the remote root.
        assert_eq!(
            symlinks.into_iter().collect::<Vec<_>>(),
            vec!["link.c", "sub/deep.c"]
        );
    }

    #[test]
    fn walk_does_not_descend_through_a_symlinked_directory() {
        let mut f = Fake::new(Echo::AbsolutePath);
        f.symlinks.insert("/root".into(), vec!["elsewhere".into()]);
        f.dirs
            .insert("/root/elsewhere".into(), vec![("escaped.c".into(), false)]);

        let out = walk(&mut f, "").unwrap();

        assert!(!out.iter().any(|path| path.starts_with("elsewhere")));
        assert!(!f.listed.iter().any(|dir| dir == "/root/elsewhere"));
    }

    #[test]
    fn a_single_symlink_path_arg_is_refused_rather_than_sized_into_a_target() {
        let mut f = Fake::new(Echo::AbsolutePath);
        f.symlinks.insert("/root".into(), vec!["link.c".into()]);
        // SIZE succeeds on a symlink to a file, which is exactly how the
        // single-path fast path used to smuggle one in as a normal target.
        f.files.push("/root/link.c".into());

        let mut out = BTreeSet::new();
        let mut symlinks = BTreeSet::new();
        walk_remote_with_symlinks(&mut f, "/root", "link.c", &mut out, &mut symlinks).unwrap();

        assert!(out.is_empty());
        assert_eq!(symlinks.into_iter().collect::<Vec<_>>(), vec!["link.c"]);
    }

    #[test]
    fn collect_remote_arg_does_not_resurrect_a_symlink_through_the_size_fallback() {
        let mut f = Fake::new(Echo::AbsolutePath);
        f.symlinks.insert("/root".into(), vec!["link.c".into()]);
        f.files.push("/root/link.c".into());

        let mut out = BTreeSet::new();
        let mut symlinks = BTreeSet::new();
        let added =
            collect_remote_arg_with_symlinks(&mut f, "/root", "link.c", &mut out, &mut symlinks);

        assert_eq!(added, 0);
        assert!(out.is_empty());
        assert_eq!(symlinks.into_iter().collect::<Vec<_>>(), vec!["link.c"]);
    }

    #[test]
    fn walks_a_subdirectory_arg() {
        let mut f = Fake::new(Echo::AbsolutePath);
        let out = walk(&mut f, "sub").unwrap();
        assert_eq!(out.into_iter().collect::<Vec<_>>(), vec!["sub/b.txt"]);
    }

    // The regression this whole change is about: pointing the walk at a single
    // file must yield that file, on every server flavour.

    #[test]
    fn file_arg_yields_the_file_on_proftpd() {
        let mut f = Fake::new(Echo::AbsolutePath);
        let out = walk(&mut f, "finger_d.20260729").unwrap();
        assert_eq!(
            out.into_iter().collect::<Vec<_>>(),
            vec!["finger_d.20260729"]
        );
    }

    #[test]
    fn file_arg_yields_the_file_on_vsftpd() {
        let mut f = Fake::new(Echo::Basename);
        let out = walk(&mut f, "a.txt").unwrap();
        assert_eq!(out.into_iter().collect::<Vec<_>>(), vec!["a.txt"]);
    }

    #[test]
    fn file_arg_yields_the_file_when_list_refuses_non_directories() {
        let mut f = Fake::new(Echo::Error);
        let out = walk(&mut f, "a.txt").unwrap();
        assert_eq!(out.into_iter().collect::<Vec<_>>(), vec!["a.txt"]);
    }

    #[test]
    fn nested_file_arg_yields_the_file() {
        let mut f = Fake::new(Echo::AbsolutePath);
        let out = walk(&mut f, "sub/b.txt").unwrap();
        assert_eq!(out.into_iter().collect::<Vec<_>>(), vec!["sub/b.txt"]);
    }

    /// The doubled-path bug: `sub/b.txt` must never appear as
    /// `sub/b.txt/b.txt`, which is what naive basename normalisation produces.
    #[test]
    fn file_arg_never_produces_a_doubled_path() {
        for echo in [Echo::AbsolutePath, Echo::Basename, Echo::Error] {
            let mut f = Fake::new(echo);
            let out = walk(&mut f, "sub/b.txt").unwrap();
            assert!(
                !out.iter().any(|p| p.contains("b.txt/b.txt")),
                "doubled path in {out:?}"
            );
        }
    }

    /// A file arg is never listed *as a directory*, so the self-description
    /// case never arises. Its parent is listed once, which is what tells a real
    /// file apart from a symlink to one — `SIZE` succeeds for both.
    #[test]
    fn file_arg_is_resolved_by_size_probe_without_listing_the_file() {
        let mut f = Fake::new(Echo::AbsolutePath);
        let _ = walk(&mut f, "a.txt").unwrap();
        assert_eq!(f.listed, ["/root"]);
        assert!(
            !f.listed.iter().any(|dir| dir == "/root/a.txt"),
            "should not have listed the file itself, listed: {:?}",
            f.listed
        );
    }

    /// Servers without SIZE fall back to the listing path, and must still not
    /// emit a doubled path or lose the file.
    #[test]
    fn file_arg_still_resolves_without_size_support() {
        let mut f = Fake::new(Echo::AbsolutePath);
        f.supports_size = false;
        let out = walk(&mut f, "finger_d.20260729").unwrap();
        assert_eq!(
            out.into_iter().collect::<Vec<_>>(),
            vec!["finger_d.20260729"]
        );
    }

    #[test]
    fn missing_path_is_an_error_at_top_level() {
        let mut f = Fake::new(Echo::AbsolutePath);
        assert!(walk(&mut f, "nope.txt").is_err());
    }

    // Containment: the guard must still reject names that would steer the walk
    // outside the sync root, while tolerating server-supplied absolute paths
    // that stay inside it.

    #[test]
    fn absolute_names_inside_root_are_accepted() {
        let mut f = Fake::new(Echo::AbsolutePath);
        f.dirs.insert(
            "/root".into(),
            vec![("/root/a.txt".into(), false), ("/root/sub".into(), true)],
        );
        let out = walk(&mut f, "").unwrap();
        assert_eq!(
            out.into_iter().collect::<Vec<_>>(),
            vec!["a.txt", "sub/b.txt"]
        );
    }

    #[test]
    fn names_escaping_the_root_are_rejected() {
        let mut f = Fake::new(Echo::AbsolutePath);
        f.inject.insert(
            "/root".into(),
            vec![
                ("/etc/passwd".into(), false),
                ("../outside.txt".into(), false),
            ],
        );
        let out = walk(&mut f, "").unwrap();
        assert!(
            !out.iter()
                .any(|p| p.contains("passwd") || p.contains("outside")),
            "escaped the root: {out:?}"
        );
        // The legitimate entries are still walked.
        assert!(out.contains("a.txt"));
    }

    #[test]
    fn parent_traversal_via_absolute_prefix_is_rejected() {
        let mut f = Fake::new(Echo::AbsolutePath);
        f.inject
            .insert("/root".into(), vec![("/rootless/evil.txt".into(), false)]);
        let out = walk(&mut f, "").unwrap();
        assert!(
            !out.iter().any(|p| p.contains("evil")),
            "prefix-matched its way out of the root: {out:?}"
        );
    }

    // collect_remote_arg: the caller-side resolution shared by pull and push.

    fn collect(f: &mut Fake, rel: &str) -> (usize, BTreeSet<String>) {
        let mut out = BTreeSet::new();
        let n = collect_remote_arg(f, "/root", rel, &mut out);
        (n, out)
    }

    #[test]
    fn collect_resolves_a_directory_arg() {
        let mut f = Fake::new(Echo::AbsolutePath);
        let (n, out) = collect(&mut f, "sub");
        assert_eq!(n, 1);
        assert_eq!(out.into_iter().collect::<Vec<_>>(), vec!["sub/b.txt"]);
    }

    #[test]
    fn collect_resolves_a_file_arg_on_every_server_flavour() {
        for echo in [Echo::AbsolutePath, Echo::Basename, Echo::Error] {
            let mut f = Fake::new(echo);
            let (n, out) = collect(&mut f, "finger_d.20260729");
            assert_eq!(n, 1, "echo variant lost the file");
            assert_eq!(
                out.into_iter().collect::<Vec<_>>(),
                vec!["finger_d.20260729"]
            );
        }
    }

    /// The regression that made `push <file>` clobber remote edits: a walk that
    /// returns Ok having found nothing must still fall through to the SIZE
    /// probe, or `on_remote` stays false for a file that does exist.
    #[test]
    fn collect_falls_back_when_the_walk_succeeds_but_finds_nothing() {
        struct EmptyOkWalk {
            inner: Fake,
        }
        impl Remote for EmptyOkWalk {
            fn list_dir(&mut self, _dir: &str) -> Result<Vec<Entry>> {
                Ok(Vec::new()) // Ok, but nothing usable.
            }
            fn file_size(&mut self, path: &str) -> Result<u64> {
                self.inner.file_size(path)
            }
        }
        let mut f = EmptyOkWalk {
            inner: Fake::new(Echo::AbsolutePath),
        };
        let mut out = BTreeSet::new();
        let n = collect_remote_arg(&mut f, "/root", "a.txt", &mut out);
        assert_eq!(n, 1, "SIZE fallback did not fire on an empty Ok walk");
        assert!(out.contains("a.txt"));
    }

    #[test]
    fn collect_reports_nothing_for_a_path_on_neither_side() {
        let mut f = Fake::new(Echo::AbsolutePath);
        let (n, out) = collect(&mut f, "nope.txt");
        assert_eq!(n, 0);
        assert!(out.is_empty());
    }

    /// An empty remote directory is genuinely empty — the fallback must not
    /// invent an entry for it.
    #[test]
    fn collect_does_not_invent_an_entry_for_an_empty_directory() {
        let mut f = Fake::new(Echo::AbsolutePath);
        f.dirs.insert("/root/empty".into(), Vec::new());
        let (n, out) = collect(&mut f, "empty");
        assert_eq!(n, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn broken_subdirectory_does_not_abort_the_walk() {
        let mut f = Fake::new(Echo::AbsolutePath);
        f.inject
            .insert("/root".into(), vec![("brokendir".into(), true)]);
        let out = walk(&mut f, "").unwrap();
        assert!(out.contains("a.txt"), "walk aborted early: {out:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_rel_normalizes_and_strips_dot_slash_and_trailing_slash() {
        assert_eq!(safe_rel("./src/x.html").unwrap(), "src/x.html");
        assert_eq!(safe_rel("src/old/").unwrap(), "src/old");
        assert_eq!(safe_rel("notes.txt").unwrap(), "notes.txt");
    }

    #[test]
    fn safe_rel_rejects_empty() {
        assert!(safe_rel("").is_err());
        assert!(safe_rel("./").is_err());
    }

    #[test]
    fn safe_rel_rejects_absolute() {
        assert!(safe_rel("/etc/passwd").is_err());
    }

    #[test]
    fn absolute_arg_inside_root_becomes_relative() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("sub/file.c");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "").unwrap();
        assert_eq!(
            safe_arg(tmp.path(), file.to_str().unwrap()).unwrap(),
            "sub/file.c"
        );
    }

    #[test]
    fn absolute_arg_outside_root_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let file = other.path().join("file.c");
        std::fs::write(&file, "").unwrap();
        assert!(safe_arg(root.path(), file.to_str().unwrap()).is_err());
    }

    #[test]
    fn safe_rel_rejects_parent_segments() {
        assert!(safe_rel("../escape").is_err());
        assert!(safe_rel("a/../../b").is_err());
    }
}
