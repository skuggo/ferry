//! Integration tests for `ferry init`. The non-ignored tests do NOT
//! require Docker — they cover the `--no-validate` flow which doesn't
//! touch FTP. The `#[ignore]`d test at the bottom exercises the full
//! validation flow against a vsftpd container; run with:
//!     cargo test --test init_integration -- --ignored

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn init_writes_config_and_updates_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join(".ferry.toml");
    let cwd = dir.path();

    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args([
            "init",
            "--no-validate",
            "--config",
            cfg_path.to_str().unwrap(),
        ])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Answers, in the order the prompts ask:
    // host, port (default 21), user, password, remote_root, local_root (default ".").
    let answers = "ftp.example.com\n\ndeploy\nsecret\n/var/www/site\n\n";
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(answers.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "init did not succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let cfg_text = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(cfg_text.contains("ftp.example.com"));
    assert!(cfg_text.contains("user = \"deploy\""));
    assert!(cfg_text.contains("remote_root = \"/var/www/site\""));

    let gi_path = cwd.join(".gitignore");
    let gi = std::fs::read_to_string(&gi_path).unwrap();
    assert!(gi.contains(".ferry.toml"));
    assert!(gi.contains(".ferry/"));
}

#[test]
fn init_dry_run_does_not_write_config_or_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join(".ferry.toml");
    let cwd = dir.path();

    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args([
            "init",
            "--no-validate",
            "--dry-run",
            "--config",
            cfg_path.to_str().unwrap(),
        ])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let answers = "ftp.example.com\n\ndeploy\nsecret\n/var/www/site\n\n";
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(answers.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "init did not succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(!cfg_path.exists(), "dry-run must not write the config");
    assert!(
        !cwd.join(".gitignore").exists(),
        "dry-run must not write .gitignore",
    );
    assert!(
        !cwd.join(".ferry/state.json").exists(),
        "dry-run must not write state",
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("would write"), "stdout: {stdout}");
    assert!(
        stdout.contains(cfg_path.to_str().unwrap()),
        "stdout: {stdout}",
    );
    assert!(stdout.contains(".gitignore"), "stdout: {stdout}");
    assert!(
        !stdout.contains("secret"),
        "stdout leaked password: {stdout}"
    );
}

#[allow(dead_code)]
mod support;

/// Exercises the full validating init flow against a real FTP server.
///
/// Layout under test:
/// - remote: `shared.txt` ("X"), `remote_only.txt` ("R"), `differs.txt` ("remote-version")
/// - local:  `shared.txt` ("X"), `local_only.txt` ("L"), `differs.txt` ("local-version")
///
/// We answer 'k' to the `differs.txt` prompt, so neither side is touched
/// and only `shared.txt` ends up seeded into `.ferry/state.json`.
#[test]
#[ignore]
fn validates_existing_local_against_remote() {
    use ferry::ftp::Ftp;
    use support::{REMOTE_ROOT, remote_path, start_ftp};

    let fixture = start_ftp();
    let host = &fixture.host;
    let port = fixture.control_port;

    // Pre-upload the remote fixtures.
    {
        let mut ftp = Ftp::connect(host, port, "test", "testpw", true).unwrap();
        ftp.upload_bytes(&remote_path("shared.txt"), b"X").unwrap();
        ftp.upload_bytes(&remote_path("remote_only.txt"), b"R")
            .unwrap();
        ftp.upload_bytes(&remote_path("differs.txt"), b"remote-version")
            .unwrap();
    }

    // Local mirror.
    let dir = tempfile::tempdir().unwrap();
    let local_root = dir.path();
    std::fs::write(local_root.join("shared.txt"), b"X").unwrap();
    std::fs::write(local_root.join("local_only.txt"), b"L").unwrap();
    std::fs::write(local_root.join("differs.txt"), b"local-version").unwrap();

    let cfg_path = local_root.join(".ferry.toml");

    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["init", "--config", cfg_path.to_str().unwrap()])
        .current_dir(local_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Prompt order: host, port (default 21), user, password, remote_root,
    // local_root (default "."), then a 'k' for the single differing file.
    // The local_root we pass is "." so the binary walks the cwd (= local_root).
    let answers = format!(
        "{host}\n{port}\ntest\ntestpw\n{REMOTE_ROOT}\n.\nk\n",
        host = host,
        port = port,
    );
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(answers.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "init did not succeed; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // Config landed on disk.
    assert!(cfg_path.exists(), ".ferry.toml should have been written");

    // State file should exist and contain ONLY shared.txt.
    let state_path = local_root.join(".ferry").join("state.json");
    let state_text =
        std::fs::read_to_string(&state_path).expect(".ferry/state.json should have been seeded");
    assert!(
        state_text.contains("shared.txt"),
        "state missing shared.txt: {state_text}"
    );
    assert!(
        !state_text.contains("local_only.txt"),
        "local-only entry must not be seeded: {state_text}",
    );
    assert!(
        !state_text.contains("remote_only.txt"),
        "remote-only entry must not be seeded: {state_text}",
    );
    assert!(
        !state_text.contains("differs.txt"),
        "kept differs entry must not be seeded: {state_text}",
    );

    // Local files unchanged.
    assert_eq!(std::fs::read(local_root.join("shared.txt")).unwrap(), b"X");
    assert_eq!(
        std::fs::read(local_root.join("local_only.txt")).unwrap(),
        b"L"
    );
    assert_eq!(
        std::fs::read(local_root.join("differs.txt")).unwrap(),
        b"local-version",
    );

    // Remote files unchanged.
    let mut ftp = Ftp::connect(host, port, "test", "testpw", true).unwrap();
    assert_eq!(ftp.download(&remote_path("shared.txt")).unwrap(), b"X");
    assert_eq!(ftp.download(&remote_path("remote_only.txt")).unwrap(), b"R");
    assert_eq!(
        ftp.download(&remote_path("differs.txt")).unwrap(),
        b"remote-version"
    );
}

#[test]
fn init_refuses_when_config_exists() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join(".ferry.toml");
    std::fs::write(&cfg_path, "junk").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args([
            "init",
            "--no-validate",
            "--config",
            cfg_path.to_str().unwrap(),
        ])
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!out.status.success(), "expected non-zero exit");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("already exists"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
