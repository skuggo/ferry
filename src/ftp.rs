use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::io::Cursor;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;
use suppaftp::FtpStream;
use suppaftp::list::ListParser;

// MDTM is always UTC per RFC 3659 §3.
fn mdtm_utc(civil: jiff::civil::DateTime) -> Result<DateTime<Utc>> {
    let timestamp = jiff::tz::Offset::UTC.to_timestamp(civil)?;
    DateTime::from_timestamp(timestamp.as_second(), timestamp.subsec_nanosecond() as u32)
        .context("FTP modification time outside the supported range")
}

/// How long a connect, read or write may stall before it fails. Without it a
/// data connection the server drops can leave ferry waiting forever (the
/// socket sits in CLOSE-WAIT). It limits each stall, not a whole transfer, so
/// a slow but moving download is unaffected. Override with
/// `FERRY_TIMEOUT_SECS`.
const DEFAULT_IO_TIMEOUT_SECS: u64 = 15;

/// Pauses between reconnect attempts after a failure. A refused login (the
/// server's per-host connection cap, a restart) is usually gone within
/// seconds; retrying turns it into a short wait instead of a failed run.
const RECONNECT_BACKOFF_SECS: [u64; 2] = [1, 3];

fn io_timeout() -> Duration {
    let secs = std::env::var("FERRY_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_IO_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Everything needed to open the session again after a transport failure.
#[derive(Clone)]
struct ConnectParams {
    host: String,
    port: u16,
    user: String,
    pass: String,
    passive: bool,
}

pub struct Ftp {
    inner: FtpStream,
    params: ConnectParams,
    // Size and modification time of every plain file seen in a LIST during
    // this session, keyed by full remote path. Lets the hash step skip the
    // per-file MDTM/SIZE round trips when the listing already proves a file
    // unchanged (see `remote_hash::listing_proves_unchanged`).
    listed: HashMap<String, (u64, DateTime<Utc>)>,
    // Directories this session has seen exist (listed, or seen as a
    // directory entry in a listing, or created). Lets uploads skip the
    // MKD-and-confirm round trips for parents that are already there.
    known_dirs: std::collections::HashSet<String>,
    // The most recent listing of each directory this session, reused only by
    // the single-leaf symlink probe (`Remote::list_dir_reuse`) so one path
    // argument does not list its parent twice.
    recent_lists: HashMap<String, Vec<Entry>>,
    // Explicit pull arguments may ask about several files in one directory.
    // A symlink check only needs the parent LIST, so retain the result for the
    // lifetime of this command and avoid repeating the same network round-trip.
    symlink_targets: HashMap<String, Option<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    /// A POSIX symlink record. Kept out of `is_dir`/`is_file` so consumers
    /// must decide a policy explicitly: enumeration skips these (they are not
    /// syncable), and every write path refuses them, because the target can
    /// resolve outside the configured remote root.
    pub is_symlink: bool,
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
    /// Walk the queued `(relative path, remote dir)` subdirectories with extra
    /// sessions, if this remote can open them, adding their files and
    /// symlinks to `out` and `symlinks` exactly as the sequential walk would.
    /// Directories it does not finish stay in `pending` for the caller. The
    /// default walks nothing, so fakes and plain remotes stay sequential.
    /// Reopen the session after a failure, so the next request is not sent
    /// down a connection that has died. Returns false when this remote
    /// cannot (fakes) or the reconnect failed.
    fn try_reconnect(&mut self) -> bool {
        false
    }
    /// A listing of `dir` that may come from earlier in the same session.
    /// Only for the single-leaf symlink probe, which would otherwise list a
    /// path argument's parent a second time. Defaults to a fresh listing.
    fn list_dir_reuse(&mut self, dir: &str) -> Result<Vec<Entry>> {
        self.list_dir(dir)
    }
    fn walk_dirs_parallel(
        &mut self,
        _root: &str,
        _pending: &mut Vec<(String, String)>,
        _out: &mut std::collections::BTreeSet<String>,
        _symlinks: &mut std::collections::BTreeSet<String>,
    ) {
    }
}

/// Parallel FTP sessions for walks and prefetching (`FERRY_PARALLEL`, at
/// most 8). Off unless asked for: the server caps connections per host, and
/// tools that already run several ferry processes at once would otherwise
/// multiply their connection count and lock everyone out.
pub fn parallel_workers() -> usize {
    std::env::var("FERRY_PARALLEL")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(1)
        .min(8)
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
    fn list_dir_reuse(&mut self, dir: &str) -> Result<Vec<Entry>> {
        self.list_reuse(dir)
    }
    fn try_reconnect(&mut self) -> bool {
        self.reconnect().is_ok()
    }
    fn walk_dirs_parallel(
        &mut self,
        root: &str,
        pending: &mut Vec<(String, String)>,
        out: &mut std::collections::BTreeSet<String>,
        symlinks: &mut std::collections::BTreeSet<String>,
    ) {
        Ftp::walk_dirs_parallel(self, root, pending, out, symlinks);
    }
}

impl StrictRemote for Ftp {
    fn list_dir_strict(&mut self, dir: &str) -> Result<Vec<Entry>> {
        self.list_strict(dir)
    }
}

impl Ftp {
    pub fn connect(host: &str, port: u16, user: &str, pass: &str, passive: bool) -> Result<Self> {
        let params = ConnectParams {
            host: host.to_string(),
            port,
            user: user.to_string(),
            pass: pass.to_string(),
            passive,
        };
        let inner = Self::open_retrying(&params)?;
        Ok(Self {
            inner,
            params,
            listed: HashMap::new(),
            known_dirs: std::collections::HashSet::new(),
            recent_lists: HashMap::new(),
            symlink_targets: HashMap::new(),
        })
    }

    /// Drop the current session and log in again with the same settings.
    /// Used to recover from a timed-out or broken connection; the listing
    /// and symlink caches stay valid because they describe the server, not
    /// the session.
    pub fn reconnect(&mut self) -> Result<()> {
        // After a failure, trust nothing remembered about directories: the
        // retry re-checks parents and re-lists, exactly as a fresh run would.
        self.known_dirs.clear();
        self.recent_lists.clear();
        let _ = self.inner.quit();
        self.inner = Self::open_retrying(&self.params)?;
        Ok(())
    }

    /// `open`, retried after the `RECONNECT_BACKOFF_SECS` pauses while the
    /// failure is the server's (refused, dropped). A config or credential
    /// problem (`Exit::Auth`) is returned at once: waiting will not fix it.
    fn open_retrying(params: &ConnectParams) -> Result<FtpStream> {
        let mut result = Self::open(params);
        for pause in RECONNECT_BACKOFF_SECS {
            match &result {
                Err(e) if e.downcast_ref::<crate::error::Exit>().is_none() => {
                    std::thread::sleep(Duration::from_secs(pause));
                    result = Self::open(params);
                }
                _ => break,
            }
        }
        result
    }

    fn open(params: &ConnectParams) -> Result<FtpStream> {
        let ConnectParams {
            host,
            port,
            user,
            pass,
            passive,
        } = params;
        let (host, port, passive) = (host.as_str(), *port, *passive);
        let timeout = io_timeout();
        // Connect + login failures become `Exit::Auth` so the process exits 3
        // (config/auth) rather than 1. The underlying suppaftp message is
        // preserved in the payload so the user still sees the real cause.
        let addr: SocketAddr = (host, port)
            .to_socket_addrs()
            .map_err(|e| crate::error::Exit::Auth(format!("ftp connect {host}:{port}: {e}")))?
            .next()
            .ok_or_else(|| {
                crate::error::Exit::Auth(format!("ftp connect {host}:{port}: no address"))
            })?;
        // A refused, dropped or garbled greeting is the server being busy (its
        // per-host connection cap) or restarting -- not a config problem, so
        // it is an ordinary error, which `connect` and `reconnect` retry.
        let s = FtpStream::connect_timeout(addr, timeout).map_err(|e| {
            anyhow::anyhow!(
                "ftp connect {host}:{port}: the server refused or dropped the connection \
                 (busy, at its connection limit, or restarting): {e}"
            )
        })?;
        s.get_ref()
            .set_read_timeout(Some(timeout))
            .context("ftp set control read timeout")?;
        s.get_ref()
            .set_write_timeout(Some(timeout))
            .context("ftp set control write timeout")?;
        // Data connections (LIST/RETR/STOR) get the same limits, so a transfer
        // the server abandons fails instead of hanging.
        let mut s = s.passive_stream_builder(move |addr| {
            let data = TcpStream::connect_timeout(&addr, timeout)
                .map_err(suppaftp::FtpError::ConnectionError)?;
            data.set_read_timeout(Some(timeout))
                .map_err(suppaftp::FtpError::ConnectionError)?;
            data.set_write_timeout(Some(timeout))
                .map_err(suppaftp::FtpError::ConnectionError)?;
            Ok(data)
        });
        // Only a real rejection of the credentials (530) is an auth problem;
        // a session dropped during login is the server again.
        s.login(user, pass).map_err(|e| match &e {
            suppaftp::FtpError::UnexpectedResponse(r)
                if r.status == suppaftp::Status::NotLoggedIn =>
            {
                anyhow::Error::from(crate::error::Exit::Auth(format!("ftp login as {user}: {e}")))
            }
            _ => anyhow::anyhow!("ftp login as {user}: the server dropped the session: {e}"),
        })?;
        s.transfer_type(suppaftp::types::FileType::Binary)
            .context("ftp set binary transfer type")?;
        s.set_mode(if passive {
            suppaftp::Mode::Passive
        } else {
            suppaftp::Mode::Active
        });
        Ok(s)
    }

    /// Keep an idle session alive (and find out whether it still is).
    pub fn noop(&mut self) -> Result<()> {
        self.inner.noop().context("ftp noop")?;
        Ok(())
    }

    /// Forget everything remembered from earlier listings. For a session that
    /// outlives one command (the hook helper): a listing from minutes ago
    /// must not vouch for a file now.
    pub fn forget_session_caches(&mut self) {
        self.listed.clear();
        self.known_dirs.clear();
        self.recent_lists.clear();
        self.symlink_targets.clear();
    }

    /// Size and modification time of `path` as the last LIST of its parent
    /// directory reported them, if this session listed it.
    pub fn listed_meta(&self, path: &str) -> Option<(u64, DateTime<Utc>)> {
        self.listed.get(path.trim_end_matches('/')).copied()
    }

    fn remember_listing(&mut self, dir: &str, entries: &[Entry]) {
        let key = dir_key(dir);
        let dir = dir.trim_end_matches('/');
        self.known_dirs.insert(key.clone());
        for entry in entries {
            if entry.is_symlink || entry.name.contains('/') {
                continue;
            }
            if entry.name == "." || entry.name == ".." {
                continue;
            }
            if entry.is_dir {
                self.known_dirs.insert(format!("{dir}/{}", entry.name));
                continue;
            }
            self.listed.insert(
                format!("{dir}/{}", entry.name),
                (entry.size, entry.modified),
            );
        }
        self.recent_lists.insert(key, entries.to_vec());
    }

    /// True when this session has seen `path` exist as a directory.
    pub fn dir_known(&self, path: &str) -> bool {
        self.known_dirs.contains(&dir_key(path))
    }

    /// Record that `path` exists as a directory (e.g. just created).
    pub fn note_dir(&mut self, path: &str) {
        self.known_dirs.insert(dir_key(path));
    }

    /// The listing of `dir` from earlier in this session, or a fresh one.
    pub fn list_reuse(&mut self, dir: &str) -> Result<Vec<Entry>> {
        if let Some(entries) = self.recent_lists.get(&dir_key(dir)) {
            return Ok(entries.clone());
        }
        self.list(dir)
    }

    /// Raw LIST response for `dir`, including dotfiles: the 3k FTP server
    /// hides `.*` entries from a plain `LIST <dir>` but honors `LIST -a`.
    /// Servers that reject the flag outright get a plain `LIST` retry, and
    /// servers that silently swallow it (an empty `-a` reply for a dir that
    /// is not actually empty) are caught by comparing the two replies.
    fn list_lines(&mut self, dir: &str) -> Result<Vec<String>> {
        let flagged = self.inner.list(Some(&format!("-a {dir}")));
        let flagged_lines = match flagged {
            Ok(lines) if !lines.is_empty() => return Ok(lines),
            _ => self
                .inner
                .list(Some(dir))
                .with_context(|| format!("ftp list {dir}"))?,
        };
        Ok(flagged_lines)
    }

    pub fn list(&mut self, dir: &str) -> Result<Vec<Entry>> {
        let lines = self.list_lines(dir)?;
        let entries = parse_listing_tolerant(&lines);
        self.remember_listing(dir, &entries);
        Ok(entries)
    }

    pub fn list_strict(&mut self, dir: &str) -> Result<Vec<Entry>> {
        let lines = self
            .list_lines(dir)
            .map_err(|error| strict_list_transport_error(dir, error))?;

        let entries = parse_listing_strict(dir, &lines)?;
        self.remember_listing(dir, &entries);
        Ok(entries)
    }

    /// Resolve a symlink leaf from one parent LIST response. This is used only
    /// by an explicit pull argument; normal walks and all write operations keep
    /// refusing remote symlinks.
    pub fn symlink_target(&mut self, path: &str) -> Result<Option<String>> {
        let trimmed = path.trim_end_matches('/');
        if let Some(target) = self.symlink_targets.get(trimmed) {
            return Ok(target.clone());
        }
        let (parent, leaf) = trimmed.rsplit_once('/').unwrap_or(("/", trimmed));
        let parent = if parent.is_empty() { "/" } else { parent };
        let lines = self.list_lines(parent)?;
        let entries = parse_listing_tolerant(&lines);
        self.remember_listing(parent, &entries);
        let target = lines.into_iter().find_map(|line| {
            let file = match ListParser::parse_posix(&line) {
                Ok(file) => file,
                Err(_) => return None,
            };
            if file.name() == leaf && file.is_symlink() {
                return file
                    .symlink()
                    .map(|target| target.to_string_lossy().into_owned());
            }
            None
        });
        self.symlink_targets
            .insert(trimmed.to_string(), target.clone());
        Ok(target)
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

/// Work queue shared by parallel walk sessions: directories still to list,
/// and how many are being listed right now (their subdirectories may still
/// arrive, so an empty queue alone does not mean the walk is over).
struct WalkQueue {
    pending: Vec<(String, String)>,
    in_flight: usize,
}

impl Ftp {
    /// See [`Remote::walk_dirs_parallel`]. Each worker opens its own session;
    /// a directory that fails to list is retried once on a fresh connection
    /// before it is warned about and skipped, as the sequential walk does.
    /// Workers that cannot connect simply do not take part.
    fn walk_dirs_parallel(
        &mut self,
        root: &str,
        pending: &mut Vec<(String, String)>,
        out: &mut std::collections::BTreeSet<String>,
        symlinks: &mut std::collections::BTreeSet<String>,
    ) {
        let workers = parallel_workers();
        if workers < 2 || pending.is_empty() {
            return;
        }
        let queue = std::sync::Mutex::new(WalkQueue {
            pending: std::mem::take(pending),
            in_flight: 0,
        });
        let wake = std::sync::Condvar::new();
        let params = self.params.clone();
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    let (queue, wake, params) = (&queue, &wake, params.clone());
                    scope.spawn(move || walk_worker(&params, root, queue, wake))
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().ok().flatten())
                .collect::<Vec<_>>()
        });
        for (worker_out, worker_syms, listed, dirs) in results {
            out.extend(worker_out);
            symlinks.extend(worker_syms);
            self.listed.extend(listed);
            self.known_dirs.extend(dirs);
        }
        // Anything no worker got to (all failed to connect) goes back.
        if let Ok(q) = queue.into_inner() {
            pending.extend(q.pending);
        }
    }
}

type WalkResult = (
    std::collections::BTreeSet<String>,
    std::collections::BTreeSet<String>,
    HashMap<String, (u64, DateTime<Utc>)>,
    std::collections::HashSet<String>,
);

/// One parallel walk session: take directories off the queue, list them,
/// queue their subdirectories, until nothing is queued or in flight.
fn walk_worker(
    params: &ConnectParams,
    root: &str,
    queue: &std::sync::Mutex<WalkQueue>,
    wake: &std::sync::Condvar,
) -> Option<WalkResult> {
    let mut ftp = Ftp {
        inner: Ftp::open(params).ok()?,
        params: params.clone(),
        listed: HashMap::new(),
        known_dirs: std::collections::HashSet::new(),
        recent_lists: HashMap::new(),
        symlink_targets: HashMap::new(),
    };
    let mut out = std::collections::BTreeSet::new();
    let mut syms = std::collections::BTreeSet::new();
    loop {
        let job = {
            let mut q = queue.lock().ok()?;
            loop {
                if let Some(job) = q.pending.pop() {
                    q.in_flight += 1;
                    break Some(job);
                }
                if q.in_flight == 0 {
                    break None;
                }
                q = wake.wait(q).ok()?;
            }
        };
        let Some((sub, dir)) = job else { break };
        let mut listed = crate::commands::walk::walk_one_dir(
            &mut ftp, root, &sub, &dir, &mut out, &mut syms, false,
        );
        if listed.is_err() && ftp.reconnect().is_ok() {
            listed = crate::commands::walk::walk_one_dir(
                &mut ftp, root, &sub, &dir, &mut out, &mut syms, false,
            );
        }
        let subdirs = match listed {
            Ok(subdirs) => subdirs,
            Err(e) => {
                eprintln!("warning: skipping remote dir {dir}: {e:#}");
                Vec::new()
            }
        };
        let mut q = queue.lock().ok()?;
        q.pending.extend(subdirs);
        q.in_flight -= 1;
        wake.notify_all();
    }
    // Wake any worker still waiting so it can see the walk is finished.
    wake.notify_all();
    Some((out, syms, ftp.listed, ftp.known_dirs))
}

/// True when `error` carries a 450/550 reply whose text says the path does
/// not exist (as opposed to e.g. "Permission denied", which shares 550).
pub(crate) fn is_no_such_file(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<suppaftp::FtpError>(),
            Some(suppaftp::FtpError::UnexpectedResponse(response))
                if matches!(
                    response.status,
                    suppaftp::Status::RequestFileActionIgnored | suppaftp::Status::FileUnavailable
                ) && String::from_utf8_lossy(&response.body)
                    .to_ascii_lowercase()
                    .contains("no such file")
        )
    })
}

/// True when `error` carries a reply saying the server does not implement
/// the command at all (500/502/504) -- a property of the server, unlike a
/// per-path 550.
pub(crate) fn is_command_unsupported(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<suppaftp::FtpError>(),
            Some(suppaftp::FtpError::UnexpectedResponse(response))
                if matches!(
                    response.status,
                    suppaftp::Status::BadCommand
                        | suppaftp::Status::NotImplemented
                        | suppaftp::Status::NotImplementedParameter
                )
        )
    })
}

fn strict_list_transport_error(dir: &str, _error: anyhow::Error) -> anyhow::Error {
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
            let file = ListParser::parse_posix(line).ok()?;
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
        let file = ListParser::parse_posix(line).map_err(|_| {
            anyhow::anyhow!(
                "ftp list {}: invalid record {index}",
                sanitize_for_message(dir)
            )
        })?;
        // Symlinks are carried through as entries marked `is_symlink` rather
        // than dropped. Dropping them here made a symlink indistinguishable
        // from an absent path, and the guarded write pre-check reads absence
        // as "safe to create" -- so a STOR followed the link out of the root.
        // Enumeration skips them and writes refuse them; both need the record.
        //
        // Forward-compatibility guard: `suppaftp::list::FileType` has exactly
        // three variants today (directory, file, symlink), so this bail is
        // unreachable with the current suppaftp. It stays because `entry_from_posix_file`
        // derives `is_dir` from `is_directory()`, which would silently present
        // any future variant (socket, device, ...) as a regular file. Refusing
        // an unknown record type is the fail-closed choice.
        if !file.is_directory() && !file.is_file() && !file.is_symlink() {
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
        is_symlink: file.is_symlink(),
        size: u64::try_from(file.size()).unwrap_or(0),
        modified: DateTime::<Utc>::from(file.modified()),
    }
}

/// Canonical cache key for a remote directory: no trailing slash, except
/// that the root stays "/".
fn dir_key(dir: &str) -> String {
    let trimmed = dir.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
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
        let civil = self
            .inner
            .mdtm(remote_path)
            .map_err(|error| scoped_mtime_transport_error(remote_path, error))?;
        mdtm_utc(civil)
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

    pub fn mtime(&mut self, remote_path: &str) -> Result<DateTime<Utc>> {
        let civil = self
            .inner
            .mdtm(remote_path)
            .with_context(|| format!("ftp mtime {remote_path}"))?;
        mdtm_utc(civil)
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
                        ListParser::parse_posix(line)
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
    /// A symlink record whose target carries an ANSI escape, so tests can
    /// prove the target never reaches a user-visible message.
    const ATTACKER_LINK: &str = "lrwxrwxrwx 1 owner group 8 Jan 1 2000 link -> \u{1b}[31mtarget";

    #[test]
    fn strict_listing_transport_error_omits_server_response() {
        const ATTACKER_REPLY: &str = "\u{1b}[31mattacker-reply";
        let transport_error =
            suppaftp::FtpError::UnexpectedResponse(suppaftp::types::Response::new(
                suppaftp::Status::FileUnavailable,
                ATTACKER_REPLY.as_bytes().to_vec(),
            ));

        let error = strict_list_transport_error("/root", transport_error.into());
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
    fn strict_listing_marks_a_symlink_record_rather_than_dropping_it() {
        let lines = vec![ATTACKER_LINK.to_string()];

        let entries = parse_listing_strict("/root", &lines).unwrap();

        // Dropping the record would make a symlink indistinguishable from an
        // absent path, which is what lets a guarded write STOR through it.
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "link");
        assert!(entries[0].is_symlink);
        assert!(!entries[0].is_dir);
        // The attacker-controlled link target never enters the data model, so
        // it cannot reach a message, a path, or a transfer decision.
        assert!(!entries[0].name.contains("target"));
        assert!(!entries[0].name.chars().any(char::is_control));
    }

    #[test]
    fn tolerant_listing_marks_a_symlink_record() {
        let lines = vec![ATTACKER_LINK.to_string()];

        let entries = parse_listing_tolerant(&lines);

        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_symlink);
        assert!(!entries[0].is_dir);
    }

    #[test]
    fn a_symlink_among_valid_records_leaves_every_other_record_intact() {
        // A single-line fixture cannot tell "handles the symlink" apart from
        // "drops everything", so pin the survivors explicitly.
        let lines = vec![
            VALID_POSIX_FILE.to_string(),
            ATTACKER_LINK.to_string(),
            VALID_POSIX_DIRECTORY.to_string(),
        ];

        let entries = parse_listing_strict("/root", &lines).unwrap();

        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.name.as_str(), entry.is_dir, entry.is_symlink))
                .collect::<Vec<_>>(),
            vec![
                ("file.txt", false, false),
                ("link", false, true),
                ("subdir", true, false),
            ]
        );
    }

    #[test]
    fn listings_do_not_mark_plain_files_or_directories_as_symlinks() {
        let lines = vec![
            VALID_POSIX_FILE.to_string(),
            VALID_POSIX_DIRECTORY.to_string(),
        ];

        let entries = parse_listing_strict("/root", &lines).unwrap();

        assert!(entries.iter().all(|entry| !entry.is_symlink));
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
