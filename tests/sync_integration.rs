//! Requires Docker. Run with: cargo test --test sync_integration -- --ignored
use ferry::ftp::Ftp;
use ferry::hash::hash_bytes;
use std::process::Command;
mod support;
use support::{FtpFixture, remote_path, start_ftp};

fn write_config(local_root: &std::path::Path, fixture: &FtpFixture) -> std::path::PathBuf {
    let config = support::write_config(local_root, fixture);
    let hidden_config = local_root.join(".ferry/config.toml");
    std::fs::rename(config, &hidden_config).unwrap();
    hidden_config
}

#[test]
#[ignore]
fn sync_noop_when_in_sync() {
    let fixture = start_ftp();

    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    let bytes: &[u8] = b"in sync on both sides\n";
    ftp.upload_bytes(&remote_path("agree.txt"), bytes).unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();
    std::fs::write(local_root.join("agree.txt"), bytes).unwrap();

    // Seed state so local == remote == known → InSync.
    let state_dir = local_root.join(".ferry");
    std::fs::create_dir_all(&state_dir).unwrap();
    let now = chrono::Utc::now();
    let mut state = ferry::state::StateFile {
        server_supports_mdtm: Some(true),
        ..Default::default()
    };
    state.files.insert(
        "agree.txt".into(),
        ferry::state::FileRecord {
            sha256: hash_bytes(bytes),
            size: bytes.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_dir.join("state.json")).unwrap();
    let state_before = std::fs::read(state_dir.join("state.json")).unwrap();

    let cfg_path = write_config(local_root, &fixture);

    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("sync")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "sync on a clean tree should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // Local file unchanged.
    let local_after = std::fs::read(local_root.join("agree.txt")).unwrap();
    assert_eq!(local_after, bytes, "local should be untouched on noop sync");

    // Remote file unchanged.
    let remote_after = ftp.download(&remote_path("agree.txt")).unwrap();
    assert_eq!(
        remote_after, bytes,
        "remote should be untouched on noop sync"
    );

    // State should still have the same single entry. We allow re-serialization
    // to change formatting in principle, but since neither files nor
    // server_supports_mdtm changed, the bytes should be identical.
    let state_after = std::fs::read(state_dir.join("state.json")).unwrap();
    assert_eq!(
        state_after, state_before,
        "state file should be byte-identical when nothing changed",
    );
}

#[test]
#[ignore]
fn sync_uploads_local_and_downloads_remote_in_one_pass() {
    let fixture = start_ftp();

    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();

    // Two files. For `local-changed.txt`: local diverged, remote == known.
    //                  For `remote-changed.txt`: remote diverged, local == known.
    let local_changed_known: &[u8] = b"original local-changed content\n";
    let local_changed_new: &[u8] = b"locally edited content\n";
    let remote_changed_known: &[u8] = b"original remote-changed content\n";
    let remote_changed_new: &[u8] = b"remote-edited content\n";

    // Seed remote: local-changed.txt still matches known; remote-changed.txt has new content.
    ftp.upload_bytes(&remote_path("local-changed.txt"), local_changed_known)
        .unwrap();
    ftp.upload_bytes(&remote_path("remote-changed.txt"), remote_changed_new)
        .unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();
    // Local: local-changed.txt has new content; remote-changed.txt matches known.
    std::fs::write(local_root.join("local-changed.txt"), local_changed_new).unwrap();
    std::fs::write(local_root.join("remote-changed.txt"), remote_changed_known).unwrap();

    // Seed state so:
    //   local-changed.txt:  known = local_changed_known → local != known, remote == known → LocalChanged
    //   remote-changed.txt: known = remote_changed_known → local == known, remote != known → RemoteChanged
    let state_dir = local_root.join(".ferry");
    std::fs::create_dir_all(&state_dir).unwrap();
    let now = chrono::Utc::now();
    let mut state = ferry::state::StateFile::default();
    state.files.insert(
        "local-changed.txt".into(),
        ferry::state::FileRecord {
            sha256: hash_bytes(local_changed_known),
            size: local_changed_known.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.files.insert(
        "remote-changed.txt".into(),
        ferry::state::FileRecord {
            sha256: hash_bytes(remote_changed_known),
            size: remote_changed_known.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_dir.join("state.json")).unwrap();

    let cfg_path = write_config(local_root, &fixture);

    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("sync")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "sync should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // Remote should now match the locally-edited local-changed.txt.
    let remote_lc = ftp.download(&remote_path("local-changed.txt")).unwrap();
    assert_eq!(
        remote_lc, local_changed_new,
        "local-changed.txt remote should be overwritten with local content",
    );

    // Local should now match the remote-edited remote-changed.txt.
    let local_rc = std::fs::read(local_root.join("remote-changed.txt")).unwrap();
    assert_eq!(
        local_rc, remote_changed_new,
        "remote-changed.txt local should be overwritten with remote content",
    );

    // State should now reflect the post-sync hashes for both files.
    let new_state =
        ferry::state::StateFile::load_or_default(&state_dir.join("state.json")).unwrap();
    let lc_rec = new_state
        .files
        .get("local-changed.txt")
        .expect("local-changed in state");
    assert_eq!(lc_rec.sha256, hash_bytes(local_changed_new));
    assert_eq!(lc_rec.size, local_changed_new.len() as u64);
    let rc_rec = new_state
        .files
        .get("remote-changed.txt")
        .expect("remote-changed in state");
    assert_eq!(rc_rec.sha256, hash_bytes(remote_changed_new));
    assert_eq!(rc_rec.size, remote_changed_new.len() as u64);
}

#[test]
#[ignore]
fn sync_refuses_both_changed_then_obeys_force() {
    let fixture = start_ftp();

    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();

    // Both sides diverged from the known hash.
    let known_bytes: &[u8] = b"originally synced content\n";
    let local_bytes: &[u8] = b"local edited content\n";
    let remote_bytes: &[u8] = b"remote edited content\n";

    ftp.upload_bytes(&remote_path("conflict.txt"), remote_bytes)
        .unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();
    std::fs::write(local_root.join("conflict.txt"), local_bytes).unwrap();

    // Seed state with the original "known" hash — neither side matches it now,
    // and they differ from each other → BothChanged.
    let state_dir = local_root.join(".ferry");
    std::fs::create_dir_all(&state_dir).unwrap();
    let now = chrono::Utc::now();
    let mut state = ferry::state::StateFile::default();
    state.files.insert(
        "conflict.txt".into(),
        ferry::state::FileRecord {
            sha256: hash_bytes(known_bytes),
            size: known_bytes.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_dir.join("state.json")).unwrap();

    let cfg_path = write_config(local_root, &fixture);

    // Without --force: sync should fail; neither side should be modified.
    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("sync")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "sync without --force should exit non-zero on BothChanged; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let local_after = std::fs::read(local_root.join("conflict.txt")).unwrap();
    assert_eq!(
        local_after, local_bytes,
        "local edits should be preserved when sync refuses",
    );
    let remote_after = ftp.download(&remote_path("conflict.txt")).unwrap();
    assert_eq!(
        remote_after, remote_bytes,
        "remote edits should be preserved when sync refuses",
    );

    // With --force: local should win — sync uploads local over remote.
    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("sync")
        .arg("--force")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "sync --force should succeed; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let remote_after_force = ftp.download(&remote_path("conflict.txt")).unwrap();
    assert_eq!(
        remote_after_force, local_bytes,
        "--force should overwrite remote with local content",
    );
    // Local should be unchanged (it's the winner).
    let local_after_force = std::fs::read(local_root.join("conflict.txt")).unwrap();
    assert_eq!(
        local_after_force, local_bytes,
        "local should still be local"
    );

    let new_state =
        ferry::state::StateFile::load_or_default(&state_dir.join("state.json")).unwrap();
    let rec = new_state.files.get("conflict.txt").expect("entry present");
    assert_eq!(rec.sha256, hash_bytes(local_bytes));
    assert_eq!(rec.size, local_bytes.len() as u64);
}
