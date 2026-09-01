use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::io::Cursor;
use suppaftp::FtpStream;

pub struct Ftp {
    inner: FtpStream,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExactFilePresence {
    Present,
    Missing,
}

/// The subset of remote operations the tree walk needs. Exists so `walk_remote`
/// can be exercised against fake servers in unit tests — real FTP servers
/// disagree about how to answer `LIST <file>`, and those disagreements are
/// exactly what the walk has to handle.
pub trait Remote {
    fn list_dir(&mut self, dir: &str) -> Result<Vec<Entry>>;
    /// `SIZE` is defined for files but not directories, so a successful reply
    /// is how we tell the two apart. Mirrors the probe in `rm` and `pull_one`.
    fn file_size(&mut self, path: &str) -> Result<u64>;
    /// An exact, completeness-aware file lookup used only for single-file
    /// transfer safety. The tolerant directory walk deliberately does not use
    /// this method.
    fn exact_file_presence(&mut self, _path: &str) -> Result<ExactFilePresence> {
        anyhow::bail!("exact remote presence lookup unavailable")
    }
}

pub trait StrictRemote: Remote {
    fn list_dir_strict(&mut self, dir: &str) -> Result<Vec<Entry>>;
}

impl Remote for Ftp {
    fn list_dir(&mut self, dir: &str) -> Result<Vec<Entry>> {
        self.list(dir)
    }
    fn file_size(&mut self, path: &str) -> Result<u64> {
        self.size(path)
    }
    fn exact_file_presence(&mut self, path: &str) -> Result<ExactFilePresence> {
        self.exact_file_presence(path)
    }
}

impl StrictRemote for Ftp {
    fn list_dir_strict(&mut self, dir: &str) -> Result<Vec<Entry>> {
        self.list_strict(dir)
    }
}

impl Ftp {
    pub fn connect(host: &str, port: u16, user: &str, pass: &str, passive: bool) -> Result<Self> {
        // Connect + login failures become `Exit::Auth` so the process exits 3
        // (config/auth) rather than 1. The underlying suppaftp message is
        // preserved in the payload so the user still sees the real cause.
        let mut s = FtpStream::connect((host, port))
            .map_err(|e| crate::error::Exit::Auth(format!("ftp connect {host}:{port}: {e}")))?;
        s.login(user, pass)
            .map_err(|e| crate::error::Exit::Auth(format!("ftp login as {user}: {e}")))?;
        s.transfer_type(suppaftp::types::FileType::Binary)
            .context("ftp set binary transfer type")?;
        s.set_mode(if passive {
            suppaftp::Mode::Passive
        } else {
            suppaftp::Mode::Active
        });
        Ok(Self { inner: s })
    }

    pub fn list(&mut self, dir: &str) -> Result<Vec<Entry>> {
        let lines = self
            .inner
            .list(Some(dir))
            .with_context(|| format!("ftp list {dir}"))?;
        Ok(parse_listing_tolerant(&lines))
    }

    pub fn list_strict(&mut self, dir: &str) -> Result<Vec<Entry>> {
        let lines = self
            .inner
            .list(Some(dir))
            .map_err(|error| strict_list_transport_error(dir, error))?;

        parse_listing_strict(dir, &lines)
    }

    /// Probe exactly one remote pathname through `NLST`. Unlike [`Self::list`]
    /// this is intentionally strict: every returned line must name the
    /// requested path, so malformed, partial, or unrelated replies cannot be
    /// mistaken for authoritative absence.
    pub fn exact_file_presence(&mut self, path: &str) -> Result<ExactFilePresence> {
        let lines = self
            .inner
            .nlst(Some(path))
            .with_context(|| format!("ftp nlst {path}"))?;
        exact_nlst_presence(path, &lines)
    }
}

fn strict_list_transport_error(dir: &str, _error: suppaftp::FtpError) -> anyhow::Error {
    anyhow::anyhow!(
        "ftp list {}: remote listing failed",
        sanitize_for_message(dir)
    )
}

#[allow(dead_code)]
fn strict_mkdir_transport_error(path: &str, _error: suppaftp::FtpError) -> anyhow::Error {
    anyhow::anyhow!(
        "ftp mkdir {}: remote create failed",
        sanitize_for_message(path)
    )
}

fn scoped_download_transport_error(path: &str, _error: suppaftp::FtpError) -> anyhow::Error {
    anyhow::anyhow!(
        "ftp scoped download {}: remote read failed",
        sanitize_for_message(path)
    )
}

fn scoped_upload_transport_error(path: &str, _error: suppaftp::FtpError) -> anyhow::Error {
    anyhow::anyhow!(
        "ftp scoped upload {}: remote write failed",
        sanitize_for_message(path)
    )
}

fn scoped_mtime_transport_error(path: &str, _error: suppaftp::FtpError) -> anyhow::Error {
    anyhow::anyhow!(
        "ftp scoped mtime {}: remote metadata read failed",
        sanitize_for_message(path)
    )
}

fn scoped_size_transport_error(path: &str, _error: suppaftp::FtpError) -> anyhow::Error {
    anyhow::anyhow!(
        "ftp scoped size {}: remote metadata read failed",
        sanitize_for_message(path)
    )
}

fn scoped_rename_transport_error(
    from: &str,
    to: &str,
    _error: suppaftp::FtpError,
) -> anyhow::Error {
    anyhow::anyhow!(
        "ftp scoped rename {} -> {}: remote rename failed",
        sanitize_for_message(from),
        sanitize_for_message(to)
    )
}

fn scoped_rm_transport_error(path: &str, _error: suppaftp::FtpError) -> anyhow::Error {
    anyhow::anyhow!(
        "ftp scoped rm {}: remote remove failed",
        sanitize_for_message(path)
    )
}

fn parse_listing_tolerant(lines: &[String]) -> Vec<Entry> {
    lines
        .iter()
        .filter_map(|line| {
            let file = suppaftp::list::File::from_posix_line(line).ok()?;
            Some(entry_from_posix_file(&file))
        })
        .collect()
}

fn parse_listing_strict(dir: &str, lines: &[String]) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let file = suppaftp::list::File::from_posix_line(line).map_err(|_| {
            anyhow::anyhow!(
                "ftp list {}: invalid record {index}",
                sanitize_for_message(dir)
            )
        })?;
        if file.is_symlink() {
            // Do not follow remote symlinks: their targets may escape the
            // configured remote root and cannot be synchronized safely.
            continue;
        }
        if !file.is_directory() && !file.is_file() {
            anyhow::bail!(
                "ftp list {}: unsupported record type at record {index}",
                sanitize_for_message(dir)
            );
        }
        entries.push(entry_from_posix_file(&file));
    }
    Ok(entries)
}

fn entry_from_posix_file(file: &suppaftp::list::File) -> Entry {
    Entry {
        name: file.name().to_string(),
        is_dir: file.is_directory(),
        size: u64::try_from(file.size()).unwrap_or(0),
        modified: DateTime::<Utc>::from(file.modified()),
    }
}

fn sanitize_for_message(value: &str) -> String {
    value.chars().flat_map(char::escape_default).collect()
}

fn exact_nlst_presence(path: &str, lines: &[String]) -> Result<ExactFilePresence> {
    if lines.is_empty() {
        return Ok(ExactFilePresence::Missing);
    }
    let requested = path.trim_end_matches('/');
    let leaf = requested.rsplit('/').next().unwrap_or(requested);
    for line in lines {
        let name = line.trim().trim_end_matches('/');
        if name.is_empty() || (name != requested && name != leaf) {
            anyhow::bail!("ftp nlst {path}: unexpected exact-listing line {line:?}");
        }
    }
    Ok(ExactFilePresence::Present)
}

impl Ftp {
    pub fn upload_bytes(&mut self, remote_path: &str, data: &[u8]) -> Result<()> {
        let mut r = Cursor::new(data);
        self.inner
            .put_file(remote_path, &mut r)
            .with_context(|| format!("ftp upload {remote_path}"))?;
        Ok(())
    }

    pub fn download(&mut self, remote_path: &str) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.download_to(remote_path, &mut buf)?;
        Ok(buf)
    }

    pub fn download_to<W: std::io::Write>(&mut self, remote_path: &str, w: &mut W) -> Result<u64> {
        let mut copied: u64 = 0;
        self.inner
            .retr(remote_path, |r| {
                copied = std::io::copy(r, w).map_err(suppaftp::FtpError::ConnectionError)?;
                Ok(())
            })
            .with_context(|| format!("ftp download {remote_path}"))?;
        Ok(copied)
    }

    pub(crate) fn upload_bytes_scoped(&mut self, remote_path: &str, data: &[u8]) -> Result<()> {
        let mut reader = Cursor::new(data);
        self.inner
            .put_file(remote_path, &mut reader)
            .map_err(|error| scoped_upload_transport_error(remote_path, error))?;
        Ok(())
    }

    pub(crate) fn download_scoped(&mut self, remote_path: &str) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.inner
            .retr(remote_path, |reader| {
                std::io::copy(reader, &mut bytes).map_err(suppaftp::FtpError::ConnectionError)?;
                Ok(())
            })
            .map_err(|error| scoped_download_transport_error(remote_path, error))?;
        Ok(bytes)
    }

    pub(crate) fn mtime_scoped(&mut self, remote_path: &str) -> Result<DateTime<Utc>> {
        let naive = self
            .inner
            .mdtm(remote_path)
            .map_err(|error| scoped_mtime_transport_error(remote_path, error))?;
        Ok(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
    }

    pub(crate) fn size_scoped(&mut self, remote_path: &str) -> Result<u64> {
        let size = self
            .inner
            .size(remote_path)
            .map_err(|error| scoped_size_transport_error(remote_path, error))?;
        Ok(size as u64)
    }

    pub(crate) fn rename_scoped(&mut self, from: &str, to: &str) -> Result<()> {
        self.inner
            .rename(from, to)
            .map_err(|error| scoped_rename_transport_error(from, to, error))?;
        Ok(())
    }

    pub(crate) fn rm_scoped(&mut self, path: &str) -> Result<()> {
        self.inner
            .rm(path)
            .map_err(|error| scoped_rm_transport_error(path, error))?;
        Ok(())
    }

    pub fn size(&mut self, remote_path: &str) -> Result<u64> {
        let n = self
            .inner
            .size(remote_path)
            .with_context(|| format!("ftp size {remote_path}"))?;
        Ok(n as u64)
    }

    // MDTM is always UTC per RFC 3659 §3.
    pub fn mtime(&mut self, remote_path: &str) -> Result<DateTime<Utc>> {
        let naive = self
            .inner
            .mdtm(remote_path)
            .with_context(|| format!("ftp mtime {remote_path}"))?;
        Ok(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
    }

    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.inner
            .rename(from, to)
            .with_context(|| format!("ftp rename {from} -> {to}"))?;
        Ok(())
    }

    pub fn rm(&mut self, path: &str) -> Result<()> {
        self.inner
            .rm(path)
            .with_context(|| format!("ftp rm {path}"))?;
        Ok(())
    }

    /// Remove a remote directory. The server requires it to be empty; callers
    /// that want a recursive delete must remove the contents first and invoke
    /// `rmdir` bottom-up.
    pub fn rmdir(&mut self, path: &str) -> Result<()> {
        self.inner
            .rmdir(path)
            .with_context(|| format!("ftp rmdir {path}"))?;
        Ok(())
    }

    /// Issue exactly one `MKD` command and propagate every server failure.
    ///
    /// Scoped commits must not use [`Self::mkdir`]'s tolerant LIST fallback:
    /// a generic 550 is never evidence that a directory already exists.
    #[allow(dead_code)]
    pub(crate) fn mkdir_scoped_strict(&mut self, path: &str) -> Result<()> {
        self.inner
            .mkdir(path)
            .map_err(|error| strict_mkdir_transport_error(path, error))
    }

    /// Create a remote directory. Returns Ok if the directory was created OR
    /// already exists. Other errors are propagated.
    ///
    /// FTP servers reply 550 for both "already exists" and real failures, and
    /// suppaftp does not distinguish them. To make this idempotent we fall back
    /// to listing the parent directory after a failed mkdir: if the leaf is
    /// present we treat the call as success, otherwise we surface the original
    /// error with context.
    pub fn mkdir(&mut self, path: &str) -> Result<()> {
        match self.inner.mkdir(path) {
            Ok(_) => Ok(()),
            Err(e) => {
                let (parent, leaf) = match path.rsplit_once('/') {
                    Some((p, l)) => (if p.is_empty() { "/" } else { p }, l),
                    None => ("/", path),
                };
                if let Ok(lines) = self.inner.list(Some(parent)) {
                    let exists = lines.iter().any(|line| {
                        suppaftp::list::File::from_posix_line(line)
                            .map(|f| f.is_directory() && f.name() == leaf)
                            .unwrap_or(false)
                    });
                    if exists {
                        return Ok(());
                    }
                }
                Err(anyhow::Error::from(e)).with_context(|| format!("ftp mkdir {path}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ExactFilePresence, exact_nlst_presence, parse_listing_strict, parse_listing_tolerant,
        scoped_download_transport_error, scoped_mtime_transport_error,
        scoped_rename_transport_error, scoped_rm_transport_error, scoped_size_transport_error,
        scoped_upload_transport_error, strict_list_transport_error, strict_mkdir_transport_error,
    };

    const VALID_POSIX_FILE: &str = "-rw-r--r-- 1 owner group 42 Jan 1 2000 file.txt";
    const VALID_POSIX_DIRECTORY: &str = "drwxr-xr-x 2 owner group 4096 Jan 1 2000 subdir";

    #[test]
    fn strict_listing_transport_error_omits_server_response() {
        const ATTACKER_REPLY: &str = "\u{1b}[31mattacker-reply";
        let transport_error =
            suppaftp::FtpError::UnexpectedResponse(suppaftp::types::Response::new(
                suppaftp::Status::FileUnavailable,
                ATTACKER_REPLY.as_bytes().to_vec(),
            ));

        let error = strict_list_transport_error("/root", transport_error);
        let message = format!("{error:#}");

        assert!(message.contains("ftp list /root"));
        assert!(!message.contains('\u{1b}'));
        assert!(!message.contains("attacker-reply"));
        assert!(message.contains("remote listing failed"));
    }

    #[test]
    fn strict_mkdir_transport_error_omits_server_response_and_escapes_path() {
        const ATTACKER_REPLY: &str = "\u{1b}[31mattacker-reply";
        let transport_error =
            suppaftp::FtpError::UnexpectedResponse(suppaftp::types::Response::new(
                suppaftp::Status::FileUnavailable,
                ATTACKER_REPLY.as_bytes().to_vec(),
            ));

        let error = strict_mkdir_transport_error("/root/unsafe\nname", transport_error);
        let message = format!("{error:#}");

        assert!(message.contains("ftp mkdir /root/unsafe\\nname"));
        assert!(!message.contains('\n'));
        assert!(!message.contains('\u{1b}'));
        assert!(!message.contains("attacker-reply"));
        assert!(message.contains("remote create failed"));
    }

    fn attacker_reply() -> suppaftp::FtpError {
        suppaftp::FtpError::UnexpectedResponse(suppaftp::types::Response::new(
            suppaftp::Status::FileUnavailable,
            b"\x1b[31mattacker-reply\nsecond-line".to_vec(),
        ))
    }

    #[test]
    fn scoped_transfer_errors_drop_every_raw_server_reply_and_escape_paths() {
        let errors = [
            scoped_download_transport_error("/root/unsafe\nname", attacker_reply()),
            scoped_upload_transport_error("/root/unsafe\nname", attacker_reply()),
            scoped_mtime_transport_error("/root/unsafe\nname", attacker_reply()),
            scoped_size_transport_error("/root/unsafe\nname", attacker_reply()),
            scoped_rename_transport_error("/root/from\nname", "/root/to\nname", attacker_reply()),
            scoped_rm_transport_error("/root/unsafe\nname", attacker_reply()),
        ];

        for error in errors {
            let message = format!("{error:#}");
            assert!(message.contains("ftp scoped"), "{message}");
            assert!(message.contains("\\n"), "{message}");
            assert!(!message.contains('\n'), "{message}");
            assert!(!message.contains('\u{1b}'), "{message}");
            assert!(!message.contains("attacker-reply"), "{message}");
            assert!(!message.contains("second-line"), "{message}");
        }
    }

    #[test]
    fn strict_listing_rejects_one_malformed_line_among_valid_entries() {
        let lines = vec![
            VALID_POSIX_FILE.to_string(),
            "\u{1b}[31mmalformed".to_string(),
            VALID_POSIX_DIRECTORY.to_string(),
        ];

        let error = parse_listing_strict("/root", &lines).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("ftp list /root"));
        assert!(message.contains("record 1"));
        assert!(!message.contains('\u{1b}'));
        assert!(!message.contains("malformed"));
    }

    #[test]
    fn strict_listing_accounts_for_blank_dot_and_dotdot_records() {
        let lines = vec![
            String::new(),
            " \t".to_string(),
            "drwxr-xr-x 2 owner group 4096 Jan 1 2000 .".to_string(),
            "drwxr-xr-x 2 owner group 4096 Jan 1 2000 ..".to_string(),
            VALID_POSIX_FILE.to_string(),
        ];

        let entries = parse_listing_strict("/root", &lines).unwrap();

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec![".", "..", "file.txt"]
        );
    }

    #[test]
    fn tolerant_listing_drops_malformed_records_for_legacy_callers() {
        let lines = vec![
            VALID_POSIX_FILE.to_string(),
            "\u{1b}[31mmalformed".to_string(),
            VALID_POSIX_DIRECTORY.to_string(),
        ];

        let entries = parse_listing_tolerant(&lines);

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["file.txt", "subdir"]
        );
    }

    #[test]
    fn strict_listing_skips_symlinks_while_tolerant_listing_stays_compatible() {
        const ATTACKER_LINK: &str =
            "lrwxrwxrwx 1 owner group 8 Jan 1 2000 link -> \u{1b}[31mtarget";
        let lines = vec![ATTACKER_LINK.to_string()];

        let strict_entries = parse_listing_strict("/root", &lines).unwrap();
        assert!(strict_entries.is_empty());

        let tolerant_entries = parse_listing_tolerant(&lines);
        assert_eq!(tolerant_entries.len(), 1);
        assert!(!tolerant_entries[0].is_dir);
    }

    #[test]
    fn exact_nlst_recognizes_a_hidden_requested_name() {
        assert_eq!(
            exact_nlst_presence("/home/test/.hidden", &[".hidden".to_string()]).unwrap(),
            ExactFilePresence::Present
        );
    }

    #[test]
    fn exact_nlst_empty_response_proves_absence() {
        assert_eq!(
            exact_nlst_presence("/home/test/missing", &[]).unwrap(),
            ExactFilePresence::Missing
        );
    }

    #[test]
    fn exact_nlst_rejects_unexpected_raw_lines() {
        let error =
            exact_nlst_presence("/home/test/target", &["not-the-target".to_string()]).unwrap_err();

        assert!(format!("{error:#}").contains("unexpected exact-listing line"));
    }
}
