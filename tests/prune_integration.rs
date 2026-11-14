//! `pull --prune` / `push --prune` against a real FTP server.
//! Requires Docker. Run with: cargo test --test prune_integration -- --ignored
mod support;

use ferry::ftp::Ftp;
use std::path::Path;
use std::process::{Command, Output};
use support::{remote_path, start_ftp, write_config};

fn ferry(cfg: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(cfg)
        .args(args)
        .output()
        .unwrap()
}

fn ok(out: &Output) {
    assert!(
        out.status.success(),
        "ferry failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn remote_has(ftp: &mut Ftp, rel: &str) -> bool {
    ftp.size(&remote_path(rel)).is_ok()
}

#[test]
#[ignore]
fn pull_prune_drops_only_untouched_copies_of_remote_deletions() {
    let fixture = start_ftp();
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("area")).unwrap();
    ftp.mkdir(&remote_path("area/objects")).unwrap();
    for f in ["area/keep.c", "area/gone.c", "area/edited.c", "area/objects/old.c"] {
        ftp.upload_bytes(&remote_path(f), b"original\n").unwrap();
    }
    let work = tempfile::tempdir().unwrap();
    let root = work.path();
    let cfg = write_config(root, &fixture);
    ok(&ferry(&cfg, &["pull", "area"]));

    // Deleted on the MUD: one file, one edited locally since, a whole folder.
    for f in ["area/gone.c", "area/edited.c", "area/objects/old.c"] {
        ftp.rm(&remote_path(f)).unwrap();
    }
    ftp.rmdir(&remote_path("area/objects")).unwrap();
    std::fs::write(root.join("area/edited.c"), b"my unpushed work\n").unwrap();
    std::fs::write(root.join("area/new.c"), b"never synced\n").unwrap();

    // A plain pull never deletes.
    ok(&ferry(&cfg, &["pull", "area"]));
    assert!(root.join("area/gone.c").exists());

    // A dry run reports and touches nothing.
    let dry = ferry(&cfg, &["--dry-run", "pull", "--prune", "area"]);
    ok(&dry);
    assert!(String::from_utf8_lossy(&dry.stdout).contains("would prune (local) area/gone.c"));
    assert!(root.join("area/gone.c").exists());

    let out = ferry(&cfg, &["pull", "--prune", "area"]);
    ok(&out);
    assert!(!root.join("area/gone.c").exists(), "untouched copy of a deleted file is pruned");
    assert!(!root.join("area/objects/old.c").exists(), "files of a deleted folder are pruned");
    assert!(root.join("area/keep.c").exists(), "a file still on the remote stays");
    assert!(root.join("area/edited.c").exists(), "a locally edited file is kept");
    assert!(root.join("area/new.c").exists(), "a never-synced file is kept");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("keeping local area/edited.c"), "{err}");
    assert!(err.contains("keeping local area/new.c"), "{err}");

    let state = ferry::state::StateFile::load_or_default(&root.join(".ferry/state.json")).unwrap();
    assert!(!state.files.contains_key("area/gone.c"));
    assert!(state.files.contains_key("area/keep.c"));
}

#[test]
#[ignore]
fn push_prune_drops_only_untouched_remote_copies_of_local_deletions() {
    let fixture = start_ftp();
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("area")).unwrap();
    for f in ["area/keep.c", "area/gone.c", "area/theirs.c"] {
        ftp.upload_bytes(&remote_path(f), b"original\n").unwrap();
    }
    ftp.upload_bytes(&remote_path("area/unknown.c"), b"remote only\n").unwrap();
    let work = tempfile::tempdir().unwrap();
    let root = work.path();
    let cfg = write_config(root, &fixture);
    ok(&ferry(&cfg, &["pull", "area/keep.c", "area/gone.c", "area/theirs.c"]));

    // Deleted locally; one of them was changed on the MUD meanwhile.
    std::fs::remove_file(root.join("area/gone.c")).unwrap();
    std::fs::remove_file(root.join("area/theirs.c")).unwrap();
    ftp.upload_bytes(&remote_path("area/theirs.c"), b"someone else's edit\n").unwrap();

    // A plain push never deletes.
    ok(&ferry(&cfg, &["push", "area"]));
    assert!(remote_has(&mut ftp, "area/gone.c"));

    let out = ferry(&cfg, &["push", "--prune", "area"]);
    ok(&out);
    assert!(!remote_has(&mut ftp, "area/gone.c"), "an untouched remote copy is pruned");
    assert!(remote_has(&mut ftp, "area/keep.c"), "a file still local stays");
    assert!(remote_has(&mut ftp, "area/theirs.c"), "a remote file changed since sync is kept");
    assert!(remote_has(&mut ftp, "area/unknown.c"), "a never-synced remote file is kept");

    // A folder missing locally is refused outright.
    let refused = ferry(&cfg, &["push", "--prune", "nothere"]);
    assert!(!refused.status.success());
}

#[test]
#[ignore]
fn prune_without_paths_is_refused() {
    let fixture = start_ftp();
    let work = tempfile::tempdir().unwrap();
    let cfg = write_config(work.path(), &fixture);
    assert!(!ferry(&cfg, &["pull", "--prune"]).status.success());
    assert!(!ferry(&cfg, &["push", "--prune"]).status.success());
}
