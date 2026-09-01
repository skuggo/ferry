use super::EntryKind;
use super::scope::SyncScope;
use anyhow::Result;

pub(super) trait PickerSource {
    fn list(&mut self, directory: &str) -> Result<Vec<PickerEntry>>;
}

pub(super) trait PickerIo {
    fn read_line(&mut self, prompt: &str) -> Result<String>;
    fn write_line(&mut self, line: &str) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Presence {
    Local,
    Remote,
    Both,
}

impl Presence {
    fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
            Self::Both => "both",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PickerEntry {
    pub name: String,
    pub kind: EntryKind,
    pub presence: Presence,
}

pub(super) struct ProjectPickerSource<'a> {
    remote: &'a mut dyn crate::ftp::StrictRemote,
    local_root: &'a std::path::Path,
    canonical_root: std::path::PathBuf,
    canonical_state_root: std::path::PathBuf,
    remote_root: String,
    matcher: &'a crate::ignored::Matcher,
    directories: std::collections::BTreeMap<String, Presence>,
}

impl<'a> ProjectPickerSource<'a> {
    pub(super) fn new(
        remote: &'a mut dyn crate::ftp::StrictRemote,
        local_root: &'a std::path::Path,
        remote_root: &str,
        matcher: &'a crate::ignored::Matcher,
    ) -> Result<Self> {
        let canonical_root = local_root
            .canonicalize()
            .map_err(|_| anyhow::anyhow!("failed to open local picker root"))?;
        let state_root = local_root.join(crate::names::STATE_DIR);
        let canonical_state_root = match state_root.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                canonical_root.join(crate::names::STATE_DIR)
            }
            Err(_) => anyhow::bail!("failed to inspect local picker state directory"),
        };
        let remote_root = super::inventory::normalize_remote_root(remote_root)
            .map_err(|_| anyhow::anyhow!("invalid remote picker root"))?;

        Ok(Self {
            remote,
            local_root,
            canonical_root,
            canonical_state_root,
            remote_root,
            matcher,
            directories: std::collections::BTreeMap::from([(String::new(), Presence::Both)]),
        })
    }

    fn list_local(&self, directory: &str) -> Result<Vec<(String, EntryKind)>> {
        let path = if directory.is_empty() {
            self.local_root.to_path_buf()
        } else {
            self.local_root.join(directory)
        };
        let canonical = path.canonicalize().map_err(|_| {
            anyhow::anyhow!(
                "failed to list local picker path {}",
                picker_path(directory)
            )
        })?;
        if !canonical.starts_with(&self.canonical_root)
            || canonical.starts_with(&self.canonical_state_root)
        {
            anyhow::bail!("unsafe local picker path {}", picker_path(directory));
        }
        let metadata = std::fs::metadata(&path).map_err(|_| {
            anyhow::anyhow!(
                "failed to inspect local picker path {}",
                picker_path(directory)
            )
        })?;
        if !metadata.is_dir() {
            anyhow::bail!(
                "local picker path {} is not a directory",
                picker_path(directory)
            );
        }

        let children = std::fs::read_dir(&path).map_err(|_| {
            anyhow::anyhow!(
                "failed to list local picker path {}",
                picker_path(directory)
            )
        })?;
        let mut entries = Vec::new();
        for child in children {
            let child = child.map_err(|_| {
                anyhow::anyhow!(
                    "failed to read local picker path {}",
                    picker_path(directory)
                )
            })?;
            let name = child.file_name().into_string().map_err(|_| {
                anyhow::anyhow!(
                    "unsafe local entry in picker path {}",
                    picker_path(directory)
                )
            })?;
            if !safe_child_name(&name) {
                anyhow::bail!(
                    "unsafe local entry in picker path {}",
                    picker_path(directory)
                );
            }
            let relative = join_relative(directory, &name);
            if super::inventory::is_state_path(&relative)
                || crate::commands::transfer_temp::is_reserved_local_transfer_temp(&relative)
            {
                continue;
            }

            let child_path = child.path();
            let canonical_child = child_path.canonicalize().map_err(|_| {
                anyhow::anyhow!(
                    "failed to inspect local entry in picker path {}",
                    picker_path(directory)
                )
            })?;
            if !canonical_child.starts_with(&self.canonical_root) {
                anyhow::bail!(
                    "unsafe local entry in picker path {}",
                    picker_path(directory)
                );
            }
            if canonical_child.starts_with(&self.canonical_state_root) {
                continue;
            }
            let metadata = std::fs::metadata(&child_path).map_err(|_| {
                anyhow::anyhow!(
                    "failed to inspect local entry in picker path {}",
                    picker_path(directory)
                )
            })?;
            let kind = if metadata.is_dir() {
                EntryKind::Directory
            } else if metadata.is_file() {
                EntryKind::File
            } else {
                anyhow::bail!(
                    "unsupported local entry in picker path {}",
                    picker_path(directory)
                );
            };
            if self.matcher.is_ignored(
                &self.local_root.join(&relative),
                kind == EntryKind::Directory,
            ) {
                continue;
            }
            entries.push((name, kind));
        }
        Ok(entries)
    }

    fn list_remote(&mut self, directory: &str) -> Result<Vec<(String, EntryKind)>> {
        let remote_directory = if directory.is_empty() {
            self.remote_root.clone()
        } else {
            super::inventory::remote_join(&self.remote_root, directory)
        };
        match super::inventory::list_remote_children(
            self.remote,
            self.local_root,
            &self.remote_root,
            &remote_directory,
            directory,
            self.matcher,
        ) {
            Ok(entries) => Ok(entries
                .into_iter()
                .map(|entry| {
                    (
                        entry.name,
                        if entry.is_dir {
                            EntryKind::Directory
                        } else {
                            EntryKind::File
                        },
                    )
                })
                .collect()),
            Err(error) if error.to_string().contains("unsafe remote entry") => {
                anyhow::bail!(
                    "unsafe remote entry in picker path {}",
                    picker_path(directory)
                )
            }
            Err(_) => anyhow::bail!(
                "failed to list remote picker path {}",
                picker_path(directory)
            ),
        }
    }
}

impl PickerSource for ProjectPickerSource<'_> {
    fn list(&mut self, directory: &str) -> Result<Vec<PickerEntry>> {
        if !directory.is_empty() {
            super::inventory::validate_relative_path(directory)
                .map_err(|_| anyhow::anyhow!("unsafe picker path"))?;
        }
        let presence = self
            .directories
            .get(directory)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("picker path was not discovered"))?;
        let mut merged = std::collections::BTreeMap::new();

        if matches!(presence, Presence::Local | Presence::Both) {
            for (name, kind) in self.list_local(directory)? {
                merge_entry(&mut merged, name, kind, Presence::Local);
            }
        }
        if matches!(presence, Presence::Remote | Presence::Both) {
            for (name, kind) in self.list_remote(directory)? {
                merge_entry(&mut merged, name, kind, Presence::Remote);
            }
        }

        let entries = merged.into_values().collect::<Vec<_>>();
        for entry in &entries {
            if entry.kind == EntryKind::Directory {
                self.directories
                    .insert(join_relative(directory, &entry.name), entry.presence);
            }
        }
        Ok(entries)
    }
}

fn merge_entry(
    entries: &mut std::collections::BTreeMap<String, PickerEntry>,
    name: String,
    kind: EntryKind,
    presence: Presence,
) {
    if let Some(existing) = entries.get_mut(&name) {
        existing.presence = Presence::Both;
        if existing.kind != kind {
            existing.kind = EntryKind::File;
        }
        return;
    }
    entries.insert(
        name.clone(),
        PickerEntry {
            name,
            kind,
            presence,
        },
    );
}

fn safe_child_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.chars().any(char::is_control)
}

fn picker_path(directory: &str) -> String {
    if directory.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", sanitize_display(directory))
    }
}

pub(super) struct StdioPickerIo;

impl PickerIo for StdioPickerIo {
    fn read_line(&mut self, prompt: &str) -> Result<String> {
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        std::io::Write::write_all(&mut stdout, prompt.as_bytes())?;
        std::io::Write::flush(&mut stdout)?;
        drop(stdout);

        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(line)
    }

    fn write_line(&mut self, line: &str) -> Result<()> {
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        std::io::Write::write_all(&mut stdout, line.as_bytes())?;
        std::io::Write::write_all(&mut stdout, b"\n")?;
        std::io::Write::flush(&mut stdout)?;
        Ok(())
    }
}

pub(super) fn select<S, I>(source: &mut S, io: &mut I) -> Result<Option<SyncScope>>
where
    S: PickerSource + ?Sized,
    I: PickerIo + ?Sized,
{
    let mut current = String::new();

    loop {
        let mut entries = source.list(&current)?;
        entries.sort_by(|left, right| left.name.cmp(&right.name));

        io.write_line(&format!(
            "Current path: {}",
            if current.is_empty() {
                "/".to_string()
            } else {
                format!("/{}", sanitize_display(&current))
            }
        ))?;
        io.write_line("0. Sync this folder")?;

        let parent_index = if current.is_empty() {
            None
        } else {
            io.write_line("1. Parent")?;
            Some(1)
        };
        let first_entry = if parent_index.is_some() { 2 } else { 1 };
        for (offset, entry) in entries.iter().enumerate() {
            let kind = match entry.kind {
                EntryKind::File => "file",
                EntryKind::Directory => "dir",
            };
            io.write_line(&format!(
                "{}. [{kind}] {} ({})",
                first_entry + offset,
                sanitize_display(&entry.name),
                entry.presence.label()
            ))?;
        }
        let cancel_index = first_entry + entries.len();
        io.write_line(&format!("{cancel_index}. Cancel"))?;

        loop {
            let answer = io.read_line("Selection: ")?;
            let trimmed = answer.trim();
            if trimmed.is_empty()
                || trimmed == "\u{1b}"
                || trimmed.eq_ignore_ascii_case("cancel")
                || trimmed.eq_ignore_ascii_case("q")
            {
                return Ok(None);
            }
            let Ok(choice) = trimmed.parse::<usize>() else {
                io.write_line("Please enter one of the displayed numbers.")?;
                continue;
            };
            if choice == 0 {
                return Ok(Some(if current.is_empty() {
                    SyncScope::RootDirectory
                } else {
                    SyncScope::Path(current.clone())
                }));
            }
            if Some(choice) == parent_index {
                current = current
                    .rsplit_once('/')
                    .map_or_else(String::new, |(parent, _)| parent.to_string());
                break;
            }
            if choice == cancel_index {
                return Ok(None);
            }
            let Some(entry) = choice
                .checked_sub(first_entry)
                .and_then(|offset| entries.get(offset))
            else {
                io.write_line("Please enter one of the displayed numbers.")?;
                continue;
            };
            let selected = join_relative(&current, &entry.name);
            match entry.kind {
                EntryKind::Directory => {
                    current = selected;
                    break;
                }
                EntryKind::File => return Ok(Some(SyncScope::Path(selected))),
            }
        }
    }
}

fn join_relative(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn sanitize_display(text: &str) -> String {
    let mut sanitized = String::new();
    for character in text.chars() {
        match character {
            '\n' => sanitized.push_str(r"\n"),
            '\r' => sanitized.push_str(r"\r"),
            '\t' => sanitized.push_str(r"\t"),
            '\u{1b}' => sanitized.push_str(r"\x1b"),
            character if character.is_control() => {
                sanitized.push_str(&format!("\\u{{{:x}}}", character as u32));
            }
            character => sanitized.push(character),
        }
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use super::{PickerEntry, PickerIo, PickerSource, Presence, select};
    use crate::commands::sync::EntryKind;
    use crate::commands::sync::scope::SyncScope;
    use anyhow::Result;
    use std::collections::{BTreeMap, VecDeque};

    #[derive(Default)]
    struct FakeSource {
        directories: BTreeMap<String, Vec<PickerEntry>>,
        listed: Vec<String>,
    }

    impl PickerSource for FakeSource {
        fn list(&mut self, directory: &str) -> Result<Vec<PickerEntry>> {
            self.listed.push(directory.to_string());
            Ok(self.directories.get(directory).cloned().unwrap_or_default())
        }
    }

    #[derive(Default)]
    struct FakeIo {
        input: VecDeque<String>,
        output: Vec<String>,
        prompts: Vec<String>,
    }

    impl FakeIo {
        fn with_input(lines: &[&str]) -> Self {
            Self {
                input: lines.iter().map(|line| (*line).to_string()).collect(),
                ..Self::default()
            }
        }
    }

    impl PickerIo for FakeIo {
        fn read_line(&mut self, prompt: &str) -> Result<String> {
            self.prompts.push(prompt.to_string());
            Ok(self.input.pop_front().unwrap_or_default())
        }

        fn write_line(&mut self, line: &str) -> Result<()> {
            self.output.push(line.to_string());
            Ok(())
        }
    }

    fn entry(name: &str, kind: EntryKind, presence: Presence) -> PickerEntry {
        PickerEntry {
            name: name.to_string(),
            kind,
            presence,
        }
    }

    #[test]
    fn choosing_exact_file_returns_one_path_selection() {
        let mut source = FakeSource {
            directories: BTreeMap::from([(
                String::new(),
                vec![entry("smoke.c", EntryKind::File, Presence::Both)],
            )]),
            ..FakeSource::default()
        };
        let mut io = FakeIo::with_input(&["1", "unused"]);

        assert_eq!(
            select(&mut source, &mut io).unwrap(),
            Some(SyncScope::Path("smoke.c".into()))
        );
        assert_eq!(source.listed, [""]);
        assert_eq!(io.input, VecDeque::from(["unused".to_string()]));
        assert!(io.output.iter().any(|line| line.contains("(both)")));
    }

    #[test]
    fn browses_remote_only_directory_then_syncs_that_folder() {
        let mut source = FakeSource {
            directories: BTreeMap::from([
                (
                    String::new(),
                    vec![entry("areas", EntryKind::Directory, Presence::Remote)],
                ),
                ("areas".into(), Vec::new()),
            ]),
            ..FakeSource::default()
        };
        let mut io = FakeIo::with_input(&["1", "0"]);

        assert_eq!(
            select(&mut source, &mut io).unwrap(),
            Some(SyncScope::Path("areas".into()))
        );
        assert_eq!(source.listed, ["", "areas"]);
        assert!(io.output.iter().any(|line| line.contains("areas (remote)")));
    }

    #[test]
    fn root_folder_selection_is_explicit_root_directory() {
        let mut source = FakeSource::default();
        let mut io = FakeIo::with_input(&["0"]);

        assert_eq!(
            select(&mut source, &mut io).unwrap(),
            Some(SyncScope::RootDirectory)
        );
    }

    #[test]
    fn parent_navigation_is_nested_only_and_never_escapes_root() {
        let mut source = FakeSource {
            directories: BTreeMap::from([
                (
                    String::new(),
                    vec![entry("one", EntryKind::Directory, Presence::Both)],
                ),
                (
                    "one".into(),
                    vec![entry("two", EntryKind::Directory, Presence::Both)],
                ),
                ("one/two".into(), Vec::new()),
            ]),
            ..FakeSource::default()
        };
        // root: one=1; one: parent=1, two=2; root again: one=1;
        // one again: two=2; one/two: parent=1; one: sync=0.
        let mut io = FakeIo::with_input(&["1", "1", "1", "2", "1", "0"]);

        assert_eq!(
            select(&mut source, &mut io).unwrap(),
            Some(SyncScope::Path("one".into()))
        );
        assert_eq!(source.listed, ["", "one", "", "one", "one/two", "one"]);
        assert_eq!(
            io.output
                .iter()
                .filter(|line| line.as_str() == "1. Parent")
                .count(),
            4
        );
        let root_visits = io
            .output
            .iter()
            .filter(|line| line.as_str() == "Current path: /")
            .count();
        assert_eq!(root_visits, 2);
    }

    #[test]
    fn cancel_returns_none_without_another_selection() {
        let mut source = FakeSource {
            directories: BTreeMap::from([(
                String::new(),
                vec![entry("smoke.c", EntryKind::File, Presence::Local)],
            )]),
            ..FakeSource::default()
        };
        // With one file at root, Cancel is item 2.
        let mut io = FakeIo::with_input(&["2", "1"]);

        assert_eq!(select(&mut source, &mut io).unwrap(), None);
        assert_eq!(source.listed, [""]);
        assert_eq!(io.input, VecDeque::from(["1".to_string()]));
        assert!(io.output.iter().any(|line| line.contains("(local)")));
    }

    #[test]
    fn malformed_number_reprompts_without_relisting() {
        let mut source = FakeSource {
            directories: BTreeMap::from([(
                String::new(),
                vec![entry("smoke.c", EntryKind::File, Presence::Local)],
            )]),
            ..FakeSource::default()
        };
        let mut io = FakeIo::with_input(&["not-a-number", "99", "1"]);

        assert_eq!(
            select(&mut source, &mut io).unwrap(),
            Some(SyncScope::Path("smoke.c".into()))
        );
        assert_eq!(source.listed, [""]);
        assert_eq!(io.prompts.len(), 3);
    }

    #[test]
    fn display_escapes_remote_control_characters_but_selection_keeps_name() {
        let hostile = "bad\n\u{1b}[31m.c";
        let mut source = FakeSource {
            directories: BTreeMap::from([(
                String::new(),
                vec![entry(hostile, EntryKind::File, Presence::Remote)],
            )]),
            ..FakeSource::default()
        };
        let mut io = FakeIo::with_input(&["1"]);

        assert_eq!(
            select(&mut source, &mut io).unwrap(),
            Some(SyncScope::Path(hostile.into()))
        );
        let rendered = io.output.join("\n");
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains("bad\n"));
        assert!(rendered.contains(r"bad\n\x1b[31m.c"), "{rendered:?}");
    }
}

#[cfg(test)]
mod source_tests {
    use super::{PickerEntry, PickerSource, Presence};
    use crate::commands::sync::EntryKind;
    use anyhow::Result;
    use std::collections::{BTreeMap, BTreeSet};

    #[derive(Default)]
    struct FakeRemote {
        listings: BTreeMap<String, Vec<crate::ftp::Entry>>,
        failures: BTreeSet<String>,
        listed: Vec<String>,
    }

    impl crate::ftp::Remote for FakeRemote {
        fn list_dir(&mut self, directory: &str) -> Result<Vec<crate::ftp::Entry>> {
            <Self as crate::ftp::StrictRemote>::list_dir_strict(self, directory)
        }

        fn file_size(&mut self, _path: &str) -> Result<u64> {
            anyhow::bail!("not a file probe")
        }
    }

    impl crate::ftp::StrictRemote for FakeRemote {
        fn list_dir_strict(&mut self, directory: &str) -> Result<Vec<crate::ftp::Entry>> {
            self.listed.push(directory.to_string());
            if self.failures.contains(directory) {
                anyhow::bail!("550 ATTACKER_REPLY\nsecond line")
            }
            Ok(self.listings.get(directory).cloned().unwrap_or_default())
        }
    }

    fn entry(name: &str, kind: EntryKind, presence: Presence) -> PickerEntry {
        PickerEntry {
            name: name.to_string(),
            kind,
            presence,
        }
    }

    fn remote_entry(name: &str, kind: EntryKind) -> crate::ftp::Entry {
        crate::ftp::Entry {
            name: name.to_string(),
            is_dir: kind == EntryKind::Directory,
            is_symlink: false,
            size: 0,
            modified: chrono::Utc::now(),
        }
    }

    #[test]
    fn real_source_merges_and_sorts_local_remote_and_both_children() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("beta.c"), b"local").unwrap();
        std::fs::create_dir(root.path().join("shared")).unwrap();
        std::fs::write(root.path().join("ignored-local.c"), b"ignored").unwrap();
        std::fs::create_dir(root.path().join(crate::names::STATE_DIR)).unwrap();

        let mut remote = FakeRemote {
            listings: BTreeMap::from([(
                "/remote".into(),
                vec![
                    remote_entry("shared", EntryKind::Directory),
                    remote_entry("ignored-remote.c", EntryKind::File),
                    remote_entry("alpha", EntryKind::Directory),
                    remote_entry(crate::names::STATE_DIR, EntryKind::Directory),
                ],
            )]),
            ..FakeRemote::default()
        };
        let matcher = crate::ignored::Matcher::new(&["ignored-*".into()], root.path()).unwrap();
        let mut source =
            super::ProjectPickerSource::new(&mut remote, root.path(), "/remote", &matcher).unwrap();

        assert_eq!(
            source.list("").unwrap(),
            vec![
                entry("alpha", EntryKind::Directory, Presence::Remote),
                entry("beta.c", EntryKind::File, Presence::Local),
                entry("shared", EntryKind::Directory, Presence::Both),
            ]
        );
    }

    #[test]
    fn real_source_lists_only_the_present_side_of_a_browsed_directory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("local-dir")).unwrap();
        std::fs::write(root.path().join("local-dir/local.c"), b"local").unwrap();

        let mut remote = FakeRemote {
            listings: BTreeMap::from([
                (
                    "/remote".into(),
                    vec![remote_entry("remote-dir", EntryKind::Directory)],
                ),
                (
                    "/remote/remote-dir".into(),
                    vec![remote_entry("remote.c", EntryKind::File)],
                ),
            ]),
            ..FakeRemote::default()
        };
        let matcher = crate::ignored::Matcher::new(&[], root.path()).unwrap();
        let mut source =
            super::ProjectPickerSource::new(&mut remote, root.path(), "/remote", &matcher).unwrap();

        let root_entries = source.list("").unwrap();
        assert!(
            root_entries
                .iter()
                .any(|entry| { entry.name == "local-dir" && entry.presence == Presence::Local })
        );
        assert!(
            root_entries
                .iter()
                .any(|entry| { entry.name == "remote-dir" && entry.presence == Presence::Remote })
        );
        assert_eq!(
            source.list("local-dir").unwrap(),
            vec![entry("local.c", EntryKind::File, Presence::Local)]
        );
        assert_eq!(
            source.list("remote-dir").unwrap(),
            vec![entry("remote.c", EntryKind::File, Presence::Remote)]
        );
        drop(source);

        assert_eq!(
            remote.listed,
            ["/remote".to_string(), "/remote/remote-dir".to_string()]
        );
    }

    #[test]
    fn real_source_rejects_unsafe_children_and_drops_raw_remote_failures() {
        let root = tempfile::tempdir().unwrap();
        let matcher = crate::ignored::Matcher::new(&[], root.path()).unwrap();

        let mut unsafe_remote = FakeRemote {
            listings: BTreeMap::from([(
                "/remote".into(),
                vec![remote_entry("bad\nname.c", EntryKind::File)],
            )]),
            ..FakeRemote::default()
        };
        let mut unsafe_source =
            super::ProjectPickerSource::new(&mut unsafe_remote, root.path(), "/remote", &matcher)
                .unwrap();
        let unsafe_error = unsafe_source.list("").unwrap_err();
        let rendered = format!("{unsafe_error:#}");
        assert!(!rendered.contains("bad\nname.c"), "{rendered:?}");
        assert!(rendered.contains("unsafe remote entry"), "{rendered:?}");
        drop(unsafe_source);

        let mut failing_remote = FakeRemote {
            failures: BTreeSet::from(["/remote".into()]),
            ..FakeRemote::default()
        };
        let mut failing_source =
            super::ProjectPickerSource::new(&mut failing_remote, root.path(), "/remote", &matcher)
                .unwrap();
        let failure = format!("{:#}", failing_source.list("").unwrap_err());
        assert!(!failure.contains("ATTACKER_REPLY"), "{failure:?}");
        assert!(!failure.contains("second line"), "{failure:?}");
        assert!(failure.contains("failed to list remote picker path /"));
    }
}
