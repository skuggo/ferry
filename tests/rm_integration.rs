//! Requires Docker. Run with: cargo test --test rm_integration -- --ignored
use ferry::ftp::Ftp;
use std::process::Command;
mod support;
use support::{remote_path, start_ftp, write_config};

fn run_rm(cfg_path: &std::path::Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferry"));
    cmd.arg("--config").arg(cfg_path).arg("rm");
    for a in args {
        cmd.arg(a);
    }
    cmd.output().unwrap()
}

#[test]
#[ignore]
fn rm_deletes_file_remote_local_and_state() {
    let fixture = start_ftp();
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();

    let bytes: &[u8] = b"delete me\n";
    ftp.upload_bytes(&remote_path("notes.txt"), bytes).unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();
    std::fs::write(local_root.join("notes.txt"), bytes).unwrap();

    // Seed a state entry so we can prove rm drops it.
    let state_dir = local_root.join(".ferry");
    std::fs::create_dir_all(&state_dir).unwrap();
    let now = chrono::Utc::now();
    let mut state = ferry::state::StateFile::default();
    state.files.insert(
        "notes.txt".into(),
        ferry::state::FileRecord {
            sha256: "whatever".into(),
            size: bytes.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_dir.join("state.json")).unwrap();

    let cfg_path = write_config(local_root, &fixture);
    let out = run_rm(&cfg_path, &["notes.txt"]);
    assert!(
        out.status.success(),
        "rm failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    assert!(
        ftp.size(&remote_path("notes.txt")).is_err(),
        "remote file should be gone"
    );
    assert!(
        !local_root.join("notes.txt").exists(),
        "local file should be gone"
    );
    let new_state =
        ferry::state::StateFile::load_or_default(&state_dir.join("state.json")).unwrap();
    assert!(
        !new_state.files.contains_key("notes.txt"),
        "state entry should be dropped"
    );
}

#[test]
#[ignore]
fn rm_remote_only_file_succeeds() {
    let fixture = start_ftp();
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.upload_bytes(&remote_path("orphan.txt"), b"only on server\n")
        .unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();
    let cfg_path = write_config(local_root, &fixture);

    let out = run_rm(&cfg_path, &["orphan.txt"]);
    assert!(
        out.status.success(),
        "rm of remote-only file should succeed: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        ftp.size(&remote_path("orphan.txt")).is_err(),
        "remote-only file should be gone"
    );
}

#[test]
#[ignore]
fn rm_recursive_clears_subtree_and_removes_dirs() {
    let fixture = start_ftp();
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();

    ftp.mkdir(&remote_path("site")).unwrap();
    ftp.mkdir(&remote_path("site/sub")).unwrap();
    ftp.upload_bytes(&remote_path("site/a.txt"), b"a\n")
        .unwrap();
    ftp.upload_bytes(&remote_path("site/sub/b.txt"), b"b\n")
        .unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();
    std::fs::create_dir_all(local_root.join("site/sub")).unwrap();
    std::fs::write(local_root.join("site/a.txt"), b"a\n").unwrap();
    std::fs::write(local_root.join("site/sub/b.txt"), b"b\n").unwrap();

    let cfg_path = write_config(local_root, &fixture);
    let out = run_rm(&cfg_path, &["site", "--recursive"]);
    assert!(
        out.status.success(),
        "recursive rm failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    assert!(
        ftp.size(&remote_path("site/a.txt")).is_err(),
        "remote a.txt should be gone"
    );
    assert!(
        ftp.size(&remote_path("site/sub/b.txt")).is_err(),
        "remote b.txt should be gone"
    );
    // The emptied directories should be removed too.
    let remote_entries = ftp.list(support::REMOTE_ROOT).unwrap();
    assert!(remote_entries.iter().all(|entry| entry.name != "site"));
    assert!(
        !local_root.join("site").exists(),
        "local site/ should be removed"
    );
}

#[test]
#[ignore]
fn rm_on_directory_without_recursive_errors() {
    let fixture = start_ftp();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();
    std::fs::create_dir_all(local_root.join("stuff")).unwrap();
    std::fs::write(local_root.join("stuff/x.txt"), b"x\n").unwrap();

    let cfg_path = write_config(local_root, &fixture);
    let out = run_rm(&cfg_path, &["stuff"]);
    assert!(
        !out.status.success(),
        "rm of a directory without --recursive should fail"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refusing directory"),
        "expected 'refusing directory'; stderr={stderr}",
    );
    // The local directory must be left intact.
    assert!(
        local_root.join("stuff/x.txt").exists(),
        "local dir should be untouched"
    );
}
