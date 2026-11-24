//! Requires Docker. Run with:
//!   cargo test --test mdtm_fast_path_integration -- --ignored
//!
//! Verifies the MDTM/SIZE fast path: after a successful sync populates state
//! with (mtime, size, sha256), running `status` on the unchanged tree should
//! NOT re-download the file — it should trust the cached hash.
//!
//! Counting downloads from the outside is tricky. Instead of instrumenting
//! the Ftp wrapper with a #[cfg(test)] counter (which bleeds test concerns
//! into production), we verify the fast path by an indirect-but-robust
//! signal: after sync, we replace the remote file's contents with bytes that
//! produce a different hash but happen to leave SIZE unchanged AND we leave
//! MDTM untouched (impossible to guarantee on real servers — uploads always
//! bump mtime). So instead we exercise the converse: after sync, modify the
//! state.json on disk to set `remote_mtime` to a *clearly stale* value
//! relative to the server, then call status. With the fast path, status
//! should DOWNLOAD (because mtime doesn't match) and classify InSync. Without
//! the fast path, the same. That doesn't prove anything either way.
//!
//! Cleanest verifiable test: after the initial sync, replace the remote
//! file's contents with DIFFERENT bytes of the SAME length while spoofing
//! state to claim the new mtime. The fast path would then incorrectly
//! classify as InSync (cached hash mismatch). With a sufficiently invasive
//! test we'd catch this. But that's testing a known-incorrect-fast-path
//! invariant, not the speedup itself.
//!
//! Pragmatic test we actually run: sync once to populate state. Then call
//! status twice in quick succession on an unchanged tree. Assert that both
//! runs succeed and that the printed classification for our file is `InSync`
//! both times. This proves the fast path doesn't break correctness in the
//! "nothing changed" case — which is the case that actually fires it. A
//! follow-up integration test (out of scope for this task) could add a
//! download counter to instrument the speedup directly.

use ferry::ftp::Ftp;
use std::process::Command;
mod support;
use support::{remote_path, start_ftp, write_config};

#[test]
#[ignore]
fn mdtm_fast_path_preserves_in_sync_on_unchanged_tree() {
    let fixture = start_ftp();

    // Seed a single file.
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    let bytes: &[u8] = b"hello fast-path\n";
    ftp.upload_bytes(&remote_path("hello.txt"), bytes).unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();
    std::fs::write(local_root.join("hello.txt"), bytes).unwrap();

    let cfg_path = write_config(local_root, &fixture);

    // First sync: populates state.files["hello.txt"] with (sha256, size,
    // remote_mtime). After this, the fast path should fire on subsequent
    // status/sync runs against the same remote.
    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("sync")
        .arg("--force") // first run sees Untracked → needs --force
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "initial sync should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // Sanity: state recorded what we expect.
    let state =
        ferry::state::StateFile::load_or_default(&local_root.join(".ferry/state.json")).unwrap();
    let rec = state
        .files
        .get("hello.txt")
        .expect("hello.txt in state after sync");
    assert_eq!(rec.size, bytes.len() as u64);

    // Now run `status` — fast path territory. Nothing has changed; the
    // (mtime, size) recorded in state should match what the server reports,
    // so compute() skips the download and returns the cached hash. The
    // observable result is the InSync classification.
    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("status")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "post-sync status should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("InSync") && stdout.contains("hello.txt"),
        "expected InSync for hello.txt after sync; got:\n{stdout}"
    );

    // Run status again to make sure repeated fast-path hits stay stable.
    let out2 = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("status")
        .output()
        .unwrap();
    assert!(out2.status.success());
    let stdout2 = String::from_utf8(out2.stdout).unwrap();
    assert!(
        stdout2.contains("InSync") && stdout2.contains("hello.txt"),
        "expected InSync for hello.txt on second status run; got:\n{stdout2}"
    );

    // The server here (vsftpd via delfer/alpine-ftp-server) supports MDTM,
    // so after at least one fast-path attempt we expect the cached flag to
    // be Some(true). It may also still be None if the test set-up never
    // happened to need the fast path on this file (cache miss because
    // sync wrote then we read same mtime). Either Some(true) or None is
    // acceptable; Some(false) would indicate a regression.
    let state_after =
        ferry::state::StateFile::load_or_default(&local_root.join(".ferry/state.json")).unwrap();
    assert!(
        state_after.server_supports_mdtm != Some(false),
        "server_supports_mdtm should not be cached as false against a server that supports MDTM; got {:?}",
        state_after.server_supports_mdtm,
    );
}
