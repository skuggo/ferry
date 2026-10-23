//! Pulling files that exist on both sides but have no `.ferry/state.json`
//! entry -- e.g. a mirror seeded from a git checkout rather than by ferry.
//!
//! Requires Docker. Run with: cargo test --test untracked_pull_integration -- --ignored
mod support;

use ferry::ftp::Ftp;
use ferry::hash::hash_bytes;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use support::{remote_path, start_ftp, write_config};

fn state_of(local_root: &Path) -> ferry::state::StateFile {
    ferry::state::StateFile::load_or_default(&local_root.join(".ferry/state.json")).unwrap()
}

fn ferry(cfg: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(cfg)
        .args(args)
        .output()
        .unwrap()
}

fn hook(target: &Path) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["hook", "--cooldown", "0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    serde_json::to_writer(
        child.stdin.as_mut().unwrap(),
        &serde_json::json!({"tool_name": "Read", "tool_input": {"file_path": target}}),
    )
    .unwrap();
    child.stdin.as_mut().unwrap().flush().unwrap();
    drop(child.stdin.take());
    child.wait_with_output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
#[ignore = "requires Docker"]
fn single_file_pull_adopts_an_identical_untracked_file() {
    let fixture = start_ftp();
    let bytes: &[u8] = b"same on both sides\n";
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.upload_bytes(&remote_path("same.c"), bytes).unwrap();

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("same.c"), bytes).unwrap();
    let cfg = write_config(dir.path(), &fixture);

    let out = ferry(&cfg, &["pull", "same.c"]);

    assert!(out.status.success(), "stderr={}", text(&out.stderr));
    let record = state_of(dir.path()).files.get("same.c").cloned();
    assert_eq!(record.map(|r| r.sha256), Some(hash_bytes(bytes)));
}

#[test]
#[ignore = "requires Docker"]
fn folder_pull_adopts_an_identical_untracked_file() {
    let fixture = start_ftp();
    let bytes: &[u8] = b"same on both sides\n";
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("sub")).unwrap();
    ftp.upload_bytes(&remote_path("sub/same.c"), bytes).unwrap();

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/same.c"), bytes).unwrap();
    let cfg = write_config(dir.path(), &fixture);

    let out = ferry(&cfg, &["pull", "sub"]);

    assert!(out.status.success(), "stderr={}", text(&out.stderr));
    assert!(state_of(dir.path()).files.contains_key("sub/same.c"));
}

#[test]
#[ignore = "requires Docker"]
fn single_file_pull_of_a_differing_untracked_file_says_it_may_be_stale() {
    let fixture = start_ftp();
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.upload_bytes(&remote_path("old.c"), b"the server's newer copy\n")
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("old.c"), b"old\n").unwrap();
    let cfg = write_config(dir.path(), &fixture);

    let out = ferry(&cfg, &["pull", "old.c"]);

    assert_eq!(out.status.code(), Some(2), "stderr={}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("not tracked"), "stderr={stderr}");
    assert!(stderr.contains("local 4 bytes"), "stderr={stderr}");
    assert!(stderr.contains("remote 24 bytes"), "stderr={stderr}");
    assert_eq!(std::fs::read(dir.path().join("old.c")).unwrap(), b"old\n");
}

#[test]
#[ignore = "requires Docker"]
fn hook_tells_the_agent_when_it_declines_an_untracked_file() {
    let fixture = start_ftp();
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.upload_bytes(&remote_path("old.c"), b"the server's newer copy\n")
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("old.c");
    std::fs::write(&target, b"old\n").unwrap();
    write_config(dir.path(), &fixture);

    let out = hook(&target);

    assert!(out.status.success(), "stderr={}", text(&out.stderr));
    let envelope: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout is not hook JSON ({e}): {}", text(&out.stdout)));
    let context = envelope["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_default();
    assert_eq!(
        envelope["hookSpecificOutput"]["hookEventName"],
        "PreToolUse"
    );
    assert!(context.contains("old.c"), "context={context}");
    assert!(context.contains("stale"), "context={context}");
    assert!(
        context.contains("ferry pull --force old.c"),
        "context={context}"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"old\n");
}

#[test]
#[ignore = "requires Docker"]
fn hook_adopts_an_identical_untracked_file_silently() {
    let fixture = start_ftp();
    let bytes: &[u8] = b"same on both sides\n";
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.upload_bytes(&remote_path("same.c"), bytes).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("same.c");
    std::fs::write(&target, bytes).unwrap();
    write_config(dir.path(), &fixture);

    let out = hook(&target);

    assert!(out.status.success(), "stderr={}", text(&out.stderr));
    assert!(out.stdout.is_empty(), "stdout={}", text(&out.stdout));
    assert!(state_of(dir.path()).files.contains_key("same.c"));
}
