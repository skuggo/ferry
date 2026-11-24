//! Requires Docker. Run with: cargo test --test status_integration -- --ignored
use ferry::ftp::Ftp;
use ferry::hash::hash_bytes;
use std::process::Command;
mod support;
use support::{remote_path, start_ftp, write_config};

#[test]
#[ignore]
fn status_reports_per_file_state() {
    let fixture = start_ftp();

    // Seed remote with three files.
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    let in_sync_contents: &[u8] = b"same on both sides\n";
    let remote_changed_contents: &[u8] = b"remote version\n";
    ftp.upload_bytes(&remote_path("in_sync.txt"), in_sync_contents)
        .unwrap();
    ftp.upload_bytes(&remote_path("remote_changed.txt"), remote_changed_contents)
        .unwrap();
    ftp.upload_bytes(&remote_path("remote_only.txt"), b"only on remote\n")
        .unwrap();

    // Local workdir mirroring most of remote, plus a local-only file.
    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();
    std::fs::write(local_root.join("in_sync.txt"), in_sync_contents).unwrap();
    // For "RemoteChanged" we need local == known and remote != known.
    // Set local to "original", store known = hash("original"); remote is "remote version" (differs).
    let original: &[u8] = b"original content\n";
    std::fs::write(local_root.join("remote_changed.txt"), original).unwrap();
    std::fs::write(local_root.join("local_only.txt"), b"only here\n").unwrap();

    // Pre-populate state with known-good entries.
    let state_dir = local_root.join(".ferry");
    std::fs::create_dir_all(&state_dir).unwrap();
    let now = chrono::Utc::now();
    let mut state = ferry::state::StateFile::default();
    state.files.insert(
        "in_sync.txt".into(),
        ferry::state::FileRecord {
            sha256: hash_bytes(in_sync_contents),
            size: in_sync_contents.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.files.insert(
        "remote_changed.txt".into(),
        ferry::state::FileRecord {
            sha256: hash_bytes(original),
            size: original.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_dir.join("state.json")).unwrap();

    // Write the config pointing at the container.
    let cfg_path = write_config(local_root, &fixture);

    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("status")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ferry status failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).unwrap();

    // The output line format is "{:>14}\t{rel}", e.g. "         InSync\tin_sync.txt".
    assert!(
        stdout.contains("InSync") && stdout.contains("in_sync.txt"),
        "expected InSync for in_sync.txt; got:\n{stdout}"
    );
    assert!(
        stdout.contains("RemoteChanged") && stdout.contains("remote_changed.txt"),
        "expected RemoteChanged for remote_changed.txt; got:\n{stdout}"
    );
    assert!(
        stdout.contains("LocalOnly") && stdout.contains("local_only.txt"),
        "expected LocalOnly for local_only.txt; got:\n{stdout}"
    );
    assert!(
        stdout.contains("RemoteOnly") && stdout.contains("remote_only.txt"),
        "expected RemoteOnly for remote_only.txt; got:\n{stdout}"
    );
}
