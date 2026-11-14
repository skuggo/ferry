//! MDTM/SIZE fast-path for computing remote file hashes.
//!
//! Every command that calls `classify()` needs a remote hash. The naive path is
//! "download and SHA-256 the bytes" — which is fine for a small project but
//! expensive on every status check.
//!
//! Optimization: before downloading, compare the remote file's `MDTM` (mtime)
//! and `SIZE` against the cached values in `state.files[rel]`. If both match
//! we trust the cached `sha256` and skip the download entirely.
//!
//! Falls back to always-hashing when the server doesn't support MDTM (it
//! answers MDTM with 500/502/504). The fallback decision is cached in
//! `state.server_supports_mdtm` so we don't re-probe every run.

use crate::commands::walk::remote_join;
use crate::config::Config;
use crate::ftp::Ftp;
use crate::hash::hash_bytes;
use crate::state::FileRecord;
use crate::state::StateFile;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Timelike, Utc};
use std::collections::{BTreeSet, HashMap};

pub(crate) trait RemoteFileRetrieval {
    fn mtime(&mut self, remote_path: &str) -> Result<DateTime<Utc>>;
    fn size(&mut self, remote_path: &str) -> Result<u64>;
    fn download(&mut self, remote_path: &str) -> Result<Vec<u8>>;
    /// Size and mtime from a directory listing already fetched this session,
    /// if any. Fakes and other remotes without a listing cache keep `None`.
    fn listed(&self, _remote_path: &str) -> Option<(u64, DateTime<Utc>)> {
        None
    }
}

impl RemoteFileRetrieval for Ftp {
    fn mtime(&mut self, remote_path: &str) -> Result<DateTime<Utc>> {
        Ftp::mtime(self, remote_path)
    }

    fn size(&mut self, remote_path: &str) -> Result<u64> {
        Ftp::size(self, remote_path)
    }

    fn download(&mut self, remote_path: &str) -> Result<Vec<u8>> {
        Ftp::download(self, remote_path)
    }

    fn listed(&self, remote_path: &str) -> Option<(u64, DateTime<Utc>)> {
        self.listed_meta(remote_path)
    }
}

/// True when a LIST entry alone proves the remote file is the one `known`
/// recorded, so the MDTM/SIZE round trips can be skipped.
///
/// LIST is coarser than MDTM: recent files show HH:MM, older ones only a
/// date (parsed as midnight). So the listing time names a period, a minute or
/// a whole day, and the entry is trusted only when:
/// - the size matches the record;
/// - the recorded MDTM falls inside that same period;
/// - the record was last verified after the period ended, so no later edit
///   inside the period could be hiding behind an identical size and time.
///
/// A midnight time is read as date-only: stricter for the rare recent file
/// saved at exactly 00:00, never looser. Anything else falls back to MDTM.
pub(crate) fn listing_proves_unchanged(
    known: &FileRecord,
    listed_size: u64,
    listed_mtime: DateTime<Utc>,
) -> bool {
    let date_only = listed_mtime.hour() == 0 && listed_mtime.minute() == 0;
    let period = if date_only {
        Duration::days(1)
    } else {
        Duration::minutes(1)
    };
    let Some(start) = listed_mtime
        .with_second(0)
        .and_then(|t| t.with_nanosecond(0))
    else {
        return false;
    };
    let end = start + period;
    listed_size == known.size
        && known.remote_mtime >= start
        && known.remote_mtime < end
        && known.last_synced >= end
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RemoteMetadata {
    mtime: DateTime<Utc>,
    size: Option<u64>,
}

/// Result of computing a remote hash, either by trusting cached state or by
/// downloading and hashing.
#[derive(Debug)]
pub struct RemoteHash {
    pub sha256: String,
    pub size: u64,
    pub mtime: DateTime<Utc>,
    /// True if we skipped the download because (mtime, size) matched state.
    pub from_cache: bool,
    /// Whether `mtime` was observed and stable across this retrieval.
    pub(crate) metadata_stable: bool,
    /// The downloaded bytes — populated only when we actually downloaded AND
    /// the caller asked for them via `want_bytes`. `pull`/`sync` need these
    /// for the local write; `status` doesn't.
    pub bytes: Option<Vec<u8>>,
    /// The cache probe that must be reused if classification later requires
    /// an install. `None` means the bytes were already retrieved, or MDTM was
    /// unavailable and the single retrieval could not be metadata-validated.
    pub(crate) pre_download: Option<RemoteMetadata>,
}

/// Compute the remote hash for `remote_path`, using the MDTM/SIZE fast path
/// when the server supports MDTM and the cached state agrees.
///
/// - `want_bytes`: if true, the caller wants the downloaded bytes in the
///   non-cached case (pull/sync need them for the local write). When the
///   fast path fires we still return `bytes: None` — and that's fine, because
///   `from_cache == true` means the file is `InSync` so no write is needed.
///   When the fast path can't fire (no cached entry, mismatch, MDTM
///   unsupported) we download anyway, and `want_bytes` decides whether we
///   hand the bytes back or drop them.
pub fn compute(
    ftp: &mut Ftp,
    state: &mut StateFile,
    rel: &str,
    remote_path: &str,
    want_bytes: bool,
) -> Result<RemoteHash> {
    compute_with(ftp, state, rel, remote_path, want_bytes)
}

/// [`compute`] for a caller that has just probed the remote `SIZE` itself
/// (to prove the file exists), so the fast path does not ask again. `None`
/// (the server gave no size) asks as usual.
pub fn compute_after_probe(
    ftp: &mut Ftp,
    state: &mut StateFile,
    rel: &str,
    remote_path: &str,
    want_bytes: bool,
    probed_size: Option<u64>,
) -> Result<RemoteHash> {
    compute_inner(ftp, state, rel, remote_path, want_bytes, probed_size)
}

pub(crate) fn compute_with<R: RemoteFileRetrieval>(
    remote: &mut R,
    state: &mut StateFile,
    rel: &str,
    remote_path: &str,
    want_bytes: bool,
) -> Result<RemoteHash> {
    compute_inner(remote, state, rel, remote_path, want_bytes, None)
}

fn compute_inner<R: RemoteFileRetrieval>(
    remote: &mut R,
    state: &mut StateFile,
    rel: &str,
    remote_path: &str,
    want_bytes: bool,
    known_size: Option<u64>,
) -> Result<RemoteHash> {
    // Listing fast path: no round trips at all when this session's LIST
    // already proves the file unchanged since the recorded sync.
    if let Some(known) = state.files.get(rel)
        && let Some((listed_size, listed_mtime)) = remote.listed(remote_path)
        && listing_proves_unchanged(known, listed_size, listed_mtime)
    {
        let observed = RemoteMetadata {
            mtime: known.remote_mtime,
            size: Some(known.size),
        };
        return Ok(RemoteHash {
            sha256: known.sha256.clone(),
            size: known.size,
            mtime: known.remote_mtime,
            from_cache: true,
            metadata_stable: true,
            bytes: None,
            pre_download: Some(observed),
        });
    }

    let pre_download = if state.server_supports_mdtm.unwrap_or(true) {
        match remote.mtime(remote_path) {
            Ok(mtime) => {
                let size = match known_size {
                    Some(size) => Some(size),
                    None => remote.size(remote_path).ok(),
                };
                if state.server_supports_mdtm.is_none() {
                    state.server_supports_mdtm = Some(true);
                }
                Some(RemoteMetadata { mtime, size })
            }
            // Only a server that rejects the command itself loses the fast
            // path. Any other failure (typically a 550 for one path it cannot
            // stat) is about this file: hash it the slow way and move on.
            Err(error) => {
                if crate::ftp::is_command_unsupported(&error) {
                    state.server_supports_mdtm = Some(false);
                }
                None
            }
        }
    } else {
        None
    };

    if let (Some(known), Some(observed)) = (state.files.get(rel), pre_download)
        && observed.size == Some(known.size)
        && observed.mtime == known.remote_mtime
    {
        return Ok(RemoteHash {
            sha256: known.sha256.clone(),
            size: known.size,
            mtime: observed.mtime,
            from_cache: true,
            metadata_stable: true,
            bytes: None,
            pre_download: Some(observed),
        });
    }

    match pre_download {
        Some(observed) => download_after_observation(remote, remote_path, observed, want_bytes),
        None => download_unvalidated(remote, remote_path, want_bytes),
    }
}

pub(crate) fn complete_for_install<R: RemoteFileRetrieval>(
    remote: &mut R,
    remote_path: &str,
    remote_hash: RemoteHash,
) -> Result<RemoteHash> {
    if remote_hash.bytes.is_some() {
        return Ok(remote_hash);
    }
    let observed = remote_hash.pre_download.ok_or_else(|| {
        anyhow::anyhow!("remote hash for {remote_path} has no bytes or pre-download metadata")
    })?;
    download_after_observation(remote, remote_path, observed, true)
}

pub(crate) fn retrieve_stable<R: RemoteFileRetrieval>(
    remote: &mut R,
    remote_path: &str,
) -> Result<RemoteHash> {
    let observed = observe(remote, remote_path)?;
    download_after_observation(remote, remote_path, observed, true)
}

pub(crate) fn retrieve_fresh<R: RemoteFileRetrieval>(
    remote: &mut R,
    remote_path: &str,
) -> Result<RemoteHash> {
    match observe(remote, remote_path) {
        Ok(observed) => download_after_observation(remote, remote_path, observed, true),
        Err(_) => download_unvalidated(remote, remote_path, true),
    }
}

fn observe<R: RemoteFileRetrieval>(remote: &mut R, remote_path: &str) -> Result<RemoteMetadata> {
    let mtime = remote
        .mtime(remote_path)
        .with_context(|| format!("fetching mtime for {remote_path} before download"))?;
    let size = remote.size(remote_path).ok();
    Ok(RemoteMetadata { mtime, size })
}

fn download_after_observation<R: RemoteFileRetrieval>(
    remote: &mut R,
    remote_path: &str,
    before: RemoteMetadata,
    want_bytes: bool,
) -> Result<RemoteHash> {
    let bytes = remote
        .download(remote_path)
        .with_context(|| format!("downloading {remote_path}"))?;
    let after = RemoteMetadata {
        mtime: remote
            .mtime(remote_path)
            .with_context(|| format!("fetching mtime for {remote_path} after download"))?,
        size: remote.size(remote_path).ok(),
    };
    let size_changed = matches!((before.size, after.size), (Some(a), Some(b)) if a != b);
    if before.mtime != after.mtime || size_changed {
        anyhow::bail!("remote changed while downloading {remote_path}");
    }

    Ok(hash_download(bytes, before.mtime, want_bytes, true))
}

/// Servers known not to support MDTM retain the historical always-hash
/// fallback. The single RETR is intentionally marked unvalidated through
/// `pre_download: None`: no stable metadata snapshot can be claimed when
/// there is nothing observable to bracket it.
fn download_unvalidated<R: RemoteFileRetrieval>(
    remote: &mut R,
    remote_path: &str,
    want_bytes: bool,
) -> Result<RemoteHash> {
    let bytes = remote
        .download(remote_path)
        .with_context(|| format!("downloading {remote_path}"))?;
    Ok(hash_download(bytes, Utc::now(), want_bytes, false))
}

fn hash_download(
    bytes: Vec<u8>,
    mtime: DateTime<Utc>,
    want_bytes: bool,
    metadata_stable: bool,
) -> RemoteHash {
    let sha256 = hash_bytes(&bytes);
    let size = bytes.len() as u64;
    RemoteHash {
        sha256,
        size,
        mtime,
        from_cache: false,
        metadata_stable,
        bytes: if want_bytes { Some(bytes) } else { None },
        pre_download: None,
    }
}

/// Targets are prefetched and then pulled this many at a time, which bounds
/// how many downloaded files are held in memory at once.
pub(crate) const PREFETCH_CHUNK: usize = 200;

/// Compute remote hashes (and, with `want_bytes`, fetch the bytes) for the
/// remote files in `chunk` that the session listing cannot prove unchanged,
/// using `workers` connections of their own. Shared by pull and push. Best effort: a file a worker fails on is simply absent from the
/// result, and the sequential loop handles it the ordinary way.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prefetch(
    ftp: &Ftp,
    cfg: &Config,
    state: &StateFile,
    remote_paths: &BTreeSet<String>,
    chunk: &[String],
    workers: usize,
    want_bytes: bool,
    pool: &mut Vec<Option<Ftp>>,
) -> HashMap<String, RemoteHash> {
    // Each worker gets a private state holding just the records it needs.
    let mut records = std::collections::BTreeMap::new();
    let mut todo: Vec<&String> = Vec::new();
    for rel in chunk {
        if !remote_paths.contains(rel) {
            continue;
        }
        if let Some(known) = state.files.get(rel) {
            // The main connection's listing already proves this one: the
            // sequential loop settles it with no network traffic at all.
            let remote_path = remote_join(&cfg.paths.remote_root, rel);
            if let Some((size, mtime)) = ftp.listed_meta(&remote_path)
                && listing_proves_unchanged(known, size, mtime)
            {
                continue;
            }
            records.insert(rel.clone(), known.clone());
        }
        todo.push(rel);
    }
    if todo.len() < 2 {
        return HashMap::new();
    }
    let supports_mdtm = state.server_supports_mdtm;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(HashMap::new());
    // Worker connections live in `pool` and are reused by later chunks; a
    // slot is (re)connected only when empty.
    let wanted = workers.min(todo.len());
    while pool.len() < wanted {
        pool.push(None);
    }
    {
        let (records, todo, next, results) = (&records, &todo, &next, &results);
        std::thread::scope(|scope| {
            for slot in pool.iter_mut().take(wanted) {
                scope.spawn(move || {
                    if slot.is_none() {
                        *slot = Ftp::connect(
                            &cfg.connection.host,
                            cfg.connection.port,
                            &cfg.connection.user,
                            &cfg.connection.password,
                            cfg.connection.passive,
                        )
                        .ok();
                    }
                    let Some(ftp) = slot.as_mut() else {
                        return;
                    };
                    let mut local = StateFile {
                        files: records.clone(),
                        server_supports_mdtm: supports_mdtm,
                        ..StateFile::default()
                    };
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(rel) = todo.get(i) else { break };
                        let remote_path = remote_join(&cfg.paths.remote_root, rel);
                        for attempt in 0..2 {
                            match compute(ftp, &mut local, rel, &remote_path, want_bytes) {
                                Ok(rh) => {
                                    results.lock().unwrap().insert((*rel).clone(), rh);
                                    break;
                                }
                                Err(_) if attempt == 0 => {
                                    if ftp.reconnect().is_err() {
                                        return;
                                    }
                                }
                                Err(_) => {}
                            }
                        }
                    }
                });
            }
        });
    }
    results.into_inner().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    mod listing_fast_path {
        use super::super::listing_proves_unchanged;
        use crate::state::FileRecord;
        use chrono::{DateTime, TimeZone, Utc};

        fn at(h: u32, m: u32, sec: u32) -> DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 10, 8, h, m, sec).unwrap()
        }

        fn rec(mtime: DateTime<Utc>, synced: DateTime<Utc>) -> FileRecord {
            FileRecord {
                sha256: "x".into(),
                size: 100,
                remote_mtime: mtime,
                last_synced: synced,
            }
        }

        #[test]
        fn same_minute_same_size_synced_after_the_minute_is_trusted() {
            let r = rec(at(11, 30, 48), at(11, 35, 0));
            assert!(listing_proves_unchanged(&r, 100, at(11, 30, 0)));
        }

        #[test]
        fn size_change_is_not_trusted() {
            let r = rec(at(11, 30, 48), at(11, 35, 0));
            assert!(!listing_proves_unchanged(&r, 101, at(11, 30, 0)));
        }

        #[test]
        fn different_minute_is_not_trusted() {
            let r = rec(at(11, 30, 48), at(11, 35, 0));
            assert!(!listing_proves_unchanged(&r, 100, at(11, 31, 0)));
        }

        #[test]
        fn date_only_listing_needs_verification_after_that_day() {
            let day = Utc.with_ymd_and_hms(2004, 5, 24, 0, 0, 0).unwrap();
            let mtime = Utc.with_ymd_and_hms(2004, 5, 24, 17, 44, 31).unwrap();
            let same_day = Utc.with_ymd_and_hms(2004, 5, 24, 20, 0, 0).unwrap();
            let later = Utc.with_ymd_and_hms(2026, 10, 8, 9, 0, 0).unwrap();
            assert!(listing_proves_unchanged(&rec(mtime, later), 100, day));
            assert!(!listing_proves_unchanged(&rec(mtime, same_day), 100, day));
        }

        #[test]
        fn recorded_mtime_outside_the_listed_day_is_not_trusted() {
            let day = Utc.with_ymd_and_hms(2004, 5, 24, 0, 0, 0).unwrap();
            let mtime = Utc.with_ymd_and_hms(2004, 5, 25, 1, 0, 0).unwrap();
            assert!(!listing_proves_unchanged(
                &rec(mtime, at(9, 0, 0)),
                100,
                day
            ));
        }

        #[test]
        fn sync_inside_the_same_minute_is_not_trusted() {
            // A second same-size edit later in 11:30 would look identical.
            let r = rec(at(11, 30, 10), at(11, 30, 40));
            assert!(!listing_proves_unchanged(&r, 100, at(11, 30, 0)));
        }
    }

    use super::*;
    use crate::state::FileRecord;
    use chrono::TimeZone;
    use std::collections::VecDeque;

    struct ScriptedRetrieval {
        mtimes: VecDeque<Option<DateTime<Utc>>>,
        sizes: VecDeque<Option<u64>>,
        payload: Option<Vec<u8>>,
        events: Vec<&'static str>,
    }

    impl ScriptedRetrieval {
        fn new(
            mtimes: impl IntoIterator<Item = Option<DateTime<Utc>>>,
            sizes: impl IntoIterator<Item = Option<u64>>,
            payload: &[u8],
        ) -> Self {
            Self {
                mtimes: mtimes.into_iter().collect(),
                sizes: sizes.into_iter().collect(),
                payload: Some(payload.to_vec()),
                events: Vec::new(),
            }
        }

        fn assert_full_bracket(&self) {
            assert_eq!(self.events, ["MDTM", "SIZE", "RETR", "MDTM", "SIZE"]);
            assert_eq!(
                self.events.iter().filter(|event| **event == "RETR").count(),
                1
            );
        }
    }

    impl RemoteFileRetrieval for ScriptedRetrieval {
        fn mtime(&mut self, _remote_path: &str) -> Result<DateTime<Utc>> {
            self.events.push("MDTM");
            self.mtimes
                .pop_front()
                .expect("scripted MDTM call")
                .ok_or_else(|| anyhow::anyhow!("MDTM unsupported"))
        }

        fn size(&mut self, _remote_path: &str) -> Result<u64> {
            self.events.push("SIZE");
            self.sizes
                .pop_front()
                .expect("scripted SIZE call")
                .ok_or_else(|| anyhow::anyhow!("SIZE unsupported"))
        }

        fn download(&mut self, _remote_path: &str) -> Result<Vec<u8>> {
            self.events.push("RETR");
            Ok(self.payload.take().expect("exactly one RETR call"))
        }
    }

    fn timestamp(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 7, 14, 0, second).unwrap()
    }

    fn cached_state(rel: &str, bytes: &[u8], mtime: DateTime<Utc>) -> StateFile {
        let mut state = StateFile::default();
        state.files.insert(
            rel.to_string(),
            FileRecord {
                sha256: hash_bytes(bytes),
                size: bytes.len() as u64,
                remote_mtime: mtime,
                last_synced: mtime,
            },
        );
        state
    }

    #[test]
    fn default_supports_mdtm_is_unknown() {
        let s = StateFile::default();
        assert_eq!(s.server_supports_mdtm, None);
    }

    #[test]
    fn known_unsupported_mdtm_uses_one_unvalidated_download() {
        let payload = b"payload from a server without MDTM";
        let mut remote = ScriptedRetrieval::new([], [], payload);
        let mut state = StateFile {
            server_supports_mdtm: Some(false),
            ..StateFile::default()
        };

        let hash = compute_with(
            &mut remote,
            &mut state,
            "page.txt",
            "/remote/page.txt",
            true,
        )
        .unwrap();

        assert_eq!(remote.events, ["RETR"]);
        assert_eq!(hash.bytes.as_deref(), Some(payload.as_slice()));
        assert_eq!(hash.sha256, hash_bytes(payload));
        assert_eq!(hash.size, payload.len() as u64);
        assert!(hash.pre_download.is_none());
        assert_eq!(state.server_supports_mdtm, Some(false));
    }

    #[test]
    fn cache_miss_hash_brackets_exactly_one_download() {
        let mtime = timestamp(0);
        let payload = b"downloaded remote payload";
        let mut remote = ScriptedRetrieval::new(
            [Some(mtime), Some(mtime)],
            [Some(payload.len() as u64), Some(payload.len() as u64)],
            payload,
        );
        let mut state = StateFile::default();

        let hash = compute_with(
            &mut remote,
            &mut state,
            "page.txt",
            "/remote/page.txt",
            true,
        )
        .unwrap();

        remote.assert_full_bracket();
        assert_eq!(hash.bytes.as_deref(), Some(payload.as_slice()));
        assert_eq!(hash.sha256, hash_bytes(payload));
        assert_eq!(hash.size, payload.len() as u64);
        assert_eq!(hash.mtime, mtime);
        assert!(!hash.from_cache);
    }

    #[test]
    fn cache_miss_rejects_changed_mtime_after_one_download() {
        let payload = b"raced payload";
        let mut remote = ScriptedRetrieval::new(
            [Some(timestamp(0)), Some(timestamp(1))],
            [Some(payload.len() as u64), Some(payload.len() as u64)],
            payload,
        );

        let error = compute_with(
            &mut remote,
            &mut StateFile::default(),
            "page.txt",
            "/remote/page.txt",
            true,
        )
        .unwrap_err();

        remote.assert_full_bracket();
        assert!(
            format!("{error:#}").contains("remote changed while downloading /remote/page.txt"),
            "got: {error:#}"
        );
    }

    #[test]
    fn cache_miss_rejects_changed_available_size_after_one_download() {
        let mtime = timestamp(0);
        let payload = b"size-raced payload";
        let mut remote =
            ScriptedRetrieval::new([Some(mtime), Some(mtime)], [Some(10), Some(11)], payload);

        let error = compute_with(
            &mut remote,
            &mut StateFile::default(),
            "page.txt",
            "/remote/page.txt",
            true,
        )
        .unwrap_err();

        remote.assert_full_bracket();
        assert!(
            format!("{error:#}").contains("remote changed while downloading /remote/page.txt"),
            "got: {error:#}"
        );
    }

    #[test]
    fn cache_hit_returns_hash_after_metadata_probe_without_download() {
        let mtime = timestamp(0);
        let payload = b"cached remote payload";
        let mut remote = ScriptedRetrieval::new(
            [Some(mtime)],
            [Some(payload.len() as u64)],
            b"must not download",
        );
        let mut state = cached_state("page.txt", payload, mtime);

        let hash = compute_with(
            &mut remote,
            &mut state,
            "page.txt",
            "/remote/page.txt",
            true,
        )
        .unwrap();

        assert_eq!(remote.events, ["MDTM", "SIZE"]);
        assert!(hash.from_cache);
        assert!(hash.bytes.is_none());
        assert_eq!(hash.sha256, hash_bytes(payload));
        assert_eq!(hash.size, payload.len() as u64);
        assert_eq!(hash.mtime, mtime);
    }

    #[test]
    fn cache_miss_allows_unsupported_size_when_mtime_is_stable() {
        let mtime = timestamp(0);
        let payload = b"stable payload without SIZE";
        let mut remote = ScriptedRetrieval::new([Some(mtime), Some(mtime)], [None, None], payload);

        let hash = compute_with(
            &mut remote,
            &mut StateFile::default(),
            "page.txt",
            "/remote/page.txt",
            true,
        )
        .unwrap();

        remote.assert_full_bracket();
        assert_eq!(hash.bytes.as_deref(), Some(payload.as_slice()));
        assert_eq!(hash.sha256, hash_bytes(payload));
        assert_eq!(hash.size, payload.len() as u64);
        assert_eq!(hash.mtime, mtime);
    }

    /// MDTM answers with a fixed FTP reply; everything else succeeds.
    struct MdtmReplies {
        code: u32,
        body: &'static str,
        payload: Vec<u8>,
    }

    impl RemoteFileRetrieval for MdtmReplies {
        fn mtime(&mut self, _remote_path: &str) -> Result<DateTime<Utc>> {
            Err(anyhow::Error::new(suppaftp::FtpError::UnexpectedResponse(
                suppaftp::types::Response::new(
                    suppaftp::Status::from(self.code),
                    self.body.as_bytes().to_vec(),
                ),
            ))
            .context("ftp mtime"))
        }

        fn size(&mut self, _remote_path: &str) -> Result<u64> {
            Ok(self.payload.len() as u64)
        }

        fn download(&mut self, _remote_path: &str) -> Result<Vec<u8>> {
            Ok(self.payload.clone())
        }
    }

    fn mdtm_flag_after(code: u32, body: &'static str, before: Option<bool>) -> Option<bool> {
        let mut remote = MdtmReplies {
            code,
            body,
            payload: b"bytes".to_vec(),
        };
        let mut state = StateFile {
            server_supports_mdtm: before,
            ..StateFile::default()
        };
        let hash = compute_with(&mut remote, &mut state, "a.c", "/a.c", true).unwrap();
        assert_eq!(hash.sha256, hash_bytes(b"bytes"));
        state.server_supports_mdtm
    }

    // One path the server cannot stat (3k.org lists /all_explorer_rooms but
    // answers 550 for it) says nothing about MDTM support for every other
    // file. Recording `false` here used to switch the fast path off for the
    // whole project, permanently.
    #[test]
    fn a_per_file_550_does_not_disable_mdtm() {
        let body = "550 /all_explorer_rooms: No such file or directory";
        assert_eq!(mdtm_flag_after(550, body, None), None);
        assert_eq!(mdtm_flag_after(550, body, Some(true)), Some(true));
    }

    #[test]
    fn an_unimplemented_command_reply_disables_mdtm() {
        assert_eq!(
            mdtm_flag_after(500, "500 MDTM not understood", None),
            Some(false)
        );
        assert_eq!(
            mdtm_flag_after(502, "502 Command not implemented", Some(true)),
            Some(false)
        );
    }

    // The single-file paths have just asked SIZE to prove the file exists;
    // asking again is one more ~100 ms round trip on every hook call.
    #[test]
    fn a_known_size_skips_the_size_probe_on_a_cache_hit() {
        let mtime = timestamp(0);
        let payload = b"cached remote payload";
        let mut remote = ScriptedRetrieval::new([Some(mtime)], [], b"must not download");
        let mut state = cached_state("page.txt", payload, mtime);

        let hash = compute_inner(
            &mut remote,
            &mut state,
            "page.txt",
            "/remote/page.txt",
            true,
            Some(payload.len() as u64),
        )
        .unwrap();

        assert_eq!(remote.events, ["MDTM"]);
        assert!(hash.from_cache);
    }

    #[test]
    fn a_known_size_that_disagrees_with_state_still_downloads() {
        let mtime = timestamp(0);
        let payload = b"new remote payload, longer";
        let mut remote = ScriptedRetrieval::new(
            [Some(mtime), Some(mtime)],
            [Some(payload.len() as u64)],
            payload,
        );
        let mut state = cached_state("page.txt", b"old", mtime);

        let hash = compute_inner(
            &mut remote,
            &mut state,
            "page.txt",
            "/remote/page.txt",
            true,
            Some(payload.len() as u64),
        )
        .unwrap();

        assert_eq!(remote.events, ["MDTM", "RETR", "MDTM", "SIZE"]);
        assert!(!hash.from_cache);
        assert_eq!(hash.sha256, hash_bytes(payload));
    }
}
