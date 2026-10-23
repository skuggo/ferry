//! Running ferry from a subdirectory of the project, git-style: the config is
//! found upward, relative paths are taken from the working directory, and no
//! paths means "this subdirectory".
//!
//! Requires Docker. Run with: cargo test --test cwd_scope_integration -- --ignored
mod support;

use ferry::ftp::Ftp;
use std::path::Path;
use std::process::{Command, Output};
use support::{FtpFixture, remote_path, start_ftp, write_config};

/// A project with `top.c` at the root and `sub/inner.c`, `sub/deeper/leaf.c`
/// below it, present on the remote only.
fn project(fixture: &FtpFixture) -> tempfile::TempDir {
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("sub")).unwrap();
    ftp.mkdir(&remote_path("sub/deeper")).unwrap();
    ftp.upload_bytes(&remote_path("top.c"), b"top\n").unwrap();
    ftp.upload_bytes(&remote_path("sub/inner.c"), b"inner\n")
        .unwrap();
    ftp.upload_bytes(&remote_path("sub/deeper/leaf.c"), b"leaf\n")
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("sub/deeper")).unwrap();
    write_config(dir.path(), fixture);
    dir
}

fn ferry_in(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ferry"))
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
#[ignore = "requires Docker"]
fn status_in_a_subdirectory_reports_only_that_subtree() {
    let fixture = start_ftp();
    let dir = project(&fixture);

    let out = ferry_in(&dir.path().join("sub"), &["status"]);

    assert!(out.status.success(), "stderr={}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("sub/inner.c"), "stdout={stdout}");
    assert!(stdout.contains("sub/deeper/leaf.c"), "stdout={stdout}");
    assert!(!stdout.contains("top.c"), "stdout={stdout}");
}

#[test]
#[ignore = "requires Docker"]
fn status_takes_paths_relative_to_the_cwd() {
    let fixture = start_ftp();
    let dir = project(&fixture);

    let out = ferry_in(&dir.path().join("sub"), &["status", "deeper"]);

    assert!(out.status.success(), "stderr={}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("sub/deeper/leaf.c"), "stdout={stdout}");
    assert!(!stdout.contains("sub/inner.c"), "stdout={stdout}");
}

#[test]
#[ignore = "requires Docker"]
fn naming_the_root_from_a_subdirectory_covers_the_whole_project() {
    let fixture = start_ftp();
    let dir = project(&fixture);

    let out = ferry_in(&dir.path().join("sub/deeper"), &["status", "../.."]);

    assert!(out.status.success(), "stderr={}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("top.c"), "stdout={stdout}");
    assert!(stdout.contains("sub/deeper/leaf.c"), "stdout={stdout}");
}

#[test]
#[ignore = "requires Docker"]
fn pull_of_a_relative_file_resolves_from_the_cwd() {
    let fixture = start_ftp();
    let dir = project(&fixture);

    let out = ferry_in(&dir.path().join("sub"), &["pull", "inner.c"]);

    assert!(out.status.success(), "stderr={}", text(&out.stderr));
    assert_eq!(
        std::fs::read(dir.path().join("sub/inner.c")).unwrap(),
        b"inner\n"
    );
    assert!(!dir.path().join("inner.c").exists());
}

#[test]
#[ignore = "requires Docker"]
fn pull_without_paths_in_a_subdirectory_pulls_only_that_subtree() {
    let fixture = start_ftp();
    let dir = project(&fixture);

    let out = ferry_in(&dir.path().join("sub/deeper"), &["pull"]);

    assert!(out.status.success(), "stderr={}", text(&out.stderr));
    assert!(dir.path().join("sub/deeper/leaf.c").exists());
    assert!(!dir.path().join("sub/inner.c").exists());
    assert!(!dir.path().join("top.c").exists());
}

#[test]
#[ignore = "requires Docker"]
fn ls_without_a_path_lists_the_cwd_counterpart() {
    let fixture = start_ftp();
    let dir = project(&fixture);

    let out = ferry_in(&dir.path().join("sub"), &["ls"]);

    assert!(out.status.success(), "stderr={}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("inner.c"), "stdout={stdout}");
    assert!(!stdout.contains("top.c"), "stdout={stdout}");
}

#[test]
#[ignore = "requires Docker"]
fn rm_without_paths_still_refuses_in_a_subdirectory() {
    let fixture = start_ftp();
    let dir = project(&fixture);

    let out = ferry_in(&dir.path().join("sub"), &["rm", "--recursive"]);

    assert!(!out.status.success());
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    assert!(ftp.size(&remote_path("sub/inner.c")).is_ok());
}
