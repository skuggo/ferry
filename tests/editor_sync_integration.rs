//! Docker cases run with: cargo test --test editor_sync_integration -- --ignored
mod support;

use ferry::commands::file_transfer::{TransferOutcome, TransferStatus};
use ferry::commands::{
    ExecutionMode,
    pull::{apply_prepared_pull, fetch_remote_one, prepare_force_pull_one, pull_one},
    push::push_one,
};
use ferry::error::Exit;
use ferry::ftp::Ftp;
use ferry::hash::hash_bytes;
use ferry::state::{FileRecord, StateFile};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};
use support::{remote_path, start_ftp, write_config};

fn seed_state(local_root: &std::path::Path, rel: &str, known_bytes: &[u8]) {
    let state_path = local_root.join(".ferry/state.json");
    let now = chrono::Utc::now();
    let mut state = StateFile::load_or_default(&state_path).unwrap();
    state.files.insert(
        rel.to_string(),
        FileRecord {
            sha256: hash_bytes(known_bytes),
            size: known_bytes.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_path).unwrap();
}

fn unreachable_config(local_root: &std::path::Path) -> std::path::PathBuf {
    let config_path = local_root.join(".ferry.toml");
    std::fs::write(
        &config_path,
        format!(
            "[connection]\nhost = \"127.0.0.1\"\nport = 1\nuser = \"test\"\npassword = \"testpw\"\npassive = true\n\n[paths]\nlocal_root = {:?}\nremote_root = \"/home/test\"\n",
            local_root.display().to_string(),
        ),
    )
    .unwrap();
    config_path
}

#[test]
#[ignore]
fn pull_one_transfers_remote_bytes_without_printing() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let remote_bytes = b"remote bytes\n";
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("nested")).unwrap();
    ftp.upload_bytes(&remote_path("nested/pull.txt"), remote_bytes)
        .unwrap();

    let outcome = pull_one(&config, "nested/pull.txt", false, ExecutionMode::Apply).unwrap();

    assert_eq!(
        outcome,
        TransferOutcome {
            path: "nested/pull.txt".into(),
            status: TransferStatus::Transferred,
        }
    );
    assert_eq!(
        std::fs::read(local.path().join("nested/pull.txt")).unwrap(),
        remote_bytes
    );
    assert_eq!(
        StateFile::load_or_default(&local.path().join(".ferry/state.json"))
            .unwrap()
            .files
            .get("nested/pull.txt")
            .unwrap()
            .sha256,
        hash_bytes(remote_bytes)
    );
}

#[test]
#[ignore]
fn pull_one_conflict_names_relative_path_and_preserves_local_file() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "nested/pull-conflict.txt";
    let known = b"original\n";
    let local_bytes = b"local edit\n";
    let remote_bytes = b"remote edit\n";
    std::fs::create_dir_all(local.path().join("nested")).unwrap();
    std::fs::write(local.path().join(rel), local_bytes).unwrap();
    seed_state(local.path(), rel, known);
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("nested")).unwrap();
    ftp.upload_bytes(&remote_path(rel), remote_bytes).unwrap();

    let error = pull_one(&config, rel, false, ExecutionMode::Apply).unwrap_err();

    assert!(error.downcast_ref::<Exit>().is_some());
    assert!(format!("{error:#}").contains(rel));
    assert_eq!(std::fs::read(local.path().join(rel)).unwrap(), local_bytes);
}

#[test]
#[ignore]
fn push_one_transfers_local_bytes_without_printing() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "nested/push.txt";
    let local_bytes = b"local bytes\n";
    std::fs::create_dir_all(local.path().join("nested")).unwrap();
    std::fs::write(local.path().join(rel), local_bytes).unwrap();

    let outcome = push_one(&config, rel, false, ExecutionMode::Apply).unwrap();

    assert_eq!(
        outcome,
        TransferOutcome {
            path: rel.into(),
            status: TransferStatus::Transferred,
        }
    );
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    assert_eq!(ftp.download(&remote_path(rel)).unwrap(), local_bytes);
    assert_eq!(
        StateFile::load_or_default(&local.path().join(".ferry/state.json"))
            .unwrap()
            .files
            .get(rel)
            .unwrap()
            .sha256,
        hash_bytes(local_bytes)
    );
}

#[test]
#[ignore]
fn push_one_conflict_names_relative_path_and_preserves_remote_file() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "nested/push-conflict.txt";
    let known = b"original\n";
    let local_bytes = b"local edit\n";
    let remote_bytes = b"remote edit\n";
    std::fs::create_dir_all(local.path().join("nested")).unwrap();
    std::fs::write(local.path().join(rel), local_bytes).unwrap();
    seed_state(local.path(), rel, known);
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("nested")).unwrap();
    ftp.upload_bytes(&remote_path(rel), remote_bytes).unwrap();

    let error = push_one(&config, rel, false, ExecutionMode::Apply).unwrap_err();

    assert!(error.downcast_ref::<Exit>().is_some());
    assert!(format!("{error:#}").contains(rel));
    assert_eq!(ftp.download(&remote_path(rel)).unwrap(), remote_bytes);
}

#[test]
fn pull_one_transport_error_names_relative_path() {
    let local = tempfile::tempdir().unwrap();
    let rel = "nested/pull-transport.txt";
    let error = pull_one(
        &unreachable_config(local.path()),
        rel,
        false,
        ExecutionMode::Apply,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains(rel));
}

#[test]
fn push_one_transport_error_names_relative_path() {
    let local = tempfile::tempdir().unwrap();
    let rel = "nested/push-transport.txt";
    let error = push_one(
        &unreachable_config(local.path()),
        rel,
        false,
        ExecutionMode::Apply,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains(rel));
}

#[test]
fn pull_one_rejects_an_escaping_relative_path_before_connecting() {
    let local = tempfile::tempdir().unwrap();
    let error = pull_one(
        &unreachable_config(local.path()),
        "../escape",
        false,
        ExecutionMode::Apply,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("../escape"));
    assert!(format!("{error:#}").contains("refusing path"));
}

#[test]
fn push_one_rejects_an_absolute_path_before_connecting() {
    let local = tempfile::tempdir().unwrap();
    let error = push_one(
        &unreachable_config(local.path()),
        "/escape",
        false,
        ExecutionMode::Apply,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("/escape"));
    assert!(format!("{error:#}").contains("refusing path"));
}

const OUTPUT_PROBE_TOKEN: &str = "ferry-editor-sync-output-v2";
const STDOUT_BEGIN: &[u8] = b"FERRY_API_STDOUT_BEGIN\n";
const STDOUT_END: &[u8] = b"FERRY_API_STDOUT_END\n";
const STDERR_BEGIN: &[u8] = b"FERRY_API_STDERR_BEGIN\n";
const STDERR_END: &[u8] = b"FERRY_API_STDERR_END\n";

fn output_child_mode() -> Option<String> {
    let mode = std::env::var("FERRY_OUTPUT_PROBE").ok()?;
    (std::env::var("FERRY_OUTPUT_PROBE_TOKEN").ok().as_deref() == Some(OUTPUT_PROBE_TOKEN))
        .then_some(mode)
}

fn api_bytes_between(output: &[u8], begin: &[u8], end: &[u8]) -> Vec<u8> {
    let start = output
        .windows(begin.len())
        .position(|window| window == begin)
        .expect("begin marker")
        + begin.len();
    let end_offset = output[start..]
        .windows(end.len())
        .position(|window| window == end)
        .expect("end marker");
    output[start..start + end_offset].to_vec()
}

fn run_output_child(test: &str, mode: &str) -> std::process::Output {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--include-ignored"])
        .env("FERRY_OUTPUT_PROBE", mode)
        .env("FERRY_OUTPUT_PROBE_TOKEN", OUTPUT_PROBE_TOKEN)
        .output()
        .unwrap()
}

fn assert_silent_api_bytes(output: std::process::Output) {
    assert!(
        output.status.success(),
        "output probe exited with {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        api_bytes_between(&output.stdout, STDOUT_BEGIN, STDOUT_END),
        Vec::<u8>::new(),
        "single-file API wrote to stdout"
    );
    assert_eq!(
        api_bytes_between(&output.stderr, STDERR_BEGIN, STDERR_END),
        Vec::<u8>::new(),
        "single-file API wrote to stderr"
    );
}

fn emit_markers() {
    print!("{}", String::from_utf8_lossy(STDOUT_BEGIN));
    eprint!("{}", String::from_utf8_lossy(STDERR_BEGIN));
}

fn emit_end_markers() {
    print!("{}", String::from_utf8_lossy(STDOUT_END));
    eprint!("{}", String::from_utf8_lossy(STDERR_END));
}

#[test]
fn single_file_transport_errors_are_silent() {
    if output_child_mode().as_deref() == Some("transport") {
        let local = tempfile::tempdir().unwrap();
        let config = unreachable_config(local.path());
        emit_markers();
        assert!(pull_one(&config, "transport-pull.txt", false, ExecutionMode::Apply).is_err());
        assert!(push_one(&config, "transport-push.txt", false, ExecutionMode::Apply).is_err());
        emit_end_markers();
        return;
    }

    assert_silent_api_bytes(run_output_child(
        "single_file_transport_errors_are_silent",
        "transport",
    ));
}

#[test]
#[ignore]
fn single_file_successful_transfers_are_silent() {
    if output_child_mode().as_deref() == Some("success") {
        let fixture = start_ftp();
        let local = tempfile::tempdir().unwrap();
        let config = write_config(local.path(), &fixture);
        let pull_rel = "output-pull.txt";
        let push_rel = "output-push.txt";
        std::fs::write(local.path().join(push_rel), b"push output probe\n").unwrap();
        let mut ftp =
            Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
        ftp.upload_bytes(&remote_path(pull_rel), b"pull output probe\n")
            .unwrap();

        emit_markers();
        assert_eq!(
            pull_one(&config, pull_rel, false, ExecutionMode::Apply)
                .unwrap()
                .status,
            TransferStatus::Transferred
        );
        assert_eq!(
            push_one(&config, push_rel, false, ExecutionMode::Apply)
                .unwrap()
                .status,
            TransferStatus::Transferred
        );
        emit_end_markers();
        assert_eq!(
            std::fs::read(local.path().join(pull_rel)).unwrap(),
            b"pull output probe\n"
        );
        assert_eq!(
            ftp.download(&remote_path(push_rel)).unwrap(),
            b"push output probe\n"
        );
        return;
    }

    assert_silent_api_bytes(run_output_child(
        "single_file_successful_transfers_are_silent",
        "success",
    ));
}

#[test]
#[ignore]
fn single_file_unchanged_and_missing_source_outcomes() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "unchanged.txt";
    let bytes = b"unchanged\n";
    let state_path = local.path().join(".ferry/state.json");
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.upload_bytes(&remote_path(rel), bytes).unwrap();
    ftp.upload_bytes(&remote_path("remote-only.txt"), b"remote\n")
        .unwrap();
    std::fs::write(local.path().join("local-only.txt"), b"local\n").unwrap();

    assert_eq!(
        pull_one(&config, rel, false, ExecutionMode::Apply)
            .unwrap()
            .status,
        TransferStatus::Transferred
    );
    assert_eq!(
        pull_one(&config, rel, false, ExecutionMode::Apply)
            .unwrap()
            .status,
        TransferStatus::Unchanged
    );
    assert_eq!(
        push_one(&config, rel, false, ExecutionMode::Apply)
            .unwrap()
            .status,
        TransferStatus::Unchanged
    );
    // An unchanged pull can refresh the synchronization timestamp and MDTM
    // capability. Missing sources should leave that updated state alone.
    let state_before_missing = std::fs::read(&state_path).unwrap();
    assert_eq!(
        pull_one(&config, "local-only.txt", false, ExecutionMode::Apply)
            .unwrap()
            .status,
        TransferStatus::SkippedMissingSource
    );
    assert_eq!(
        push_one(&config, "remote-only.txt", false, ExecutionMode::Apply)
            .unwrap()
            .status,
        TransferStatus::SkippedMissingSource
    );
    assert_eq!(std::fs::read(&state_path).unwrap(), state_before_missing);
}

#[test]
#[ignore]
fn single_file_force_resolves_both_conflict_directions() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let known = b"known\n";
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();

    std::fs::write(local.path().join("pull-force.txt"), b"local edit\n").unwrap();
    seed_state(local.path(), "pull-force.txt", known);
    ftp.upload_bytes(&remote_path("pull-force.txt"), b"remote edit\n")
        .unwrap();
    assert_eq!(
        pull_one(&config, "pull-force.txt", true, ExecutionMode::Apply)
            .unwrap()
            .status,
        TransferStatus::Transferred
    );
    assert_eq!(
        std::fs::read(local.path().join("pull-force.txt")).unwrap(),
        b"remote edit\n"
    );

    std::fs::write(local.path().join("push-force.txt"), b"local edit\n").unwrap();
    seed_state(local.path(), "push-force.txt", known);
    ftp.upload_bytes(&remote_path("push-force.txt"), b"remote edit\n")
        .unwrap();
    assert_eq!(
        push_one(&config, "push-force.txt", true, ExecutionMode::Apply)
            .unwrap()
            .status,
        TransferStatus::Transferred
    );
    assert_eq!(
        ftp.download(&remote_path("push-force.txt")).unwrap(),
        b"local edit\n"
    );
}

#[test]
#[ignore]
fn single_file_dry_run_transfers_do_not_mutate_files_or_state() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let push_rel = "push-preview.txt";
    let pull_rel = "pull-preview.txt";
    std::fs::write(local.path().join(push_rel), b"local preview\n").unwrap();
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.upload_bytes(&remote_path(pull_rel), b"remote preview\n")
        .unwrap();

    assert_eq!(
        pull_one(&config, pull_rel, false, ExecutionMode::DryRun)
            .unwrap()
            .status,
        TransferStatus::Transferred
    );
    assert_eq!(
        push_one(&config, push_rel, false, ExecutionMode::DryRun)
            .unwrap()
            .status,
        TransferStatus::Transferred
    );
    assert!(!local.path().join(pull_rel).exists());
    assert!(ftp.size(&remote_path(push_rel)).is_err());
    assert!(!local.path().join(".ferry/state.json").exists());
}

#[test]
#[ignore]
fn single_file_clean_changed_branches_transfer_and_persist_state() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let known = b"known\n";
    let remote_changed = b"remote changed\n";
    let local_changed = b"local changed\n";
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();

    std::fs::write(local.path().join("remote-changed.txt"), known).unwrap();
    seed_state(local.path(), "remote-changed.txt", known);
    ftp.upload_bytes(&remote_path("remote-changed.txt"), remote_changed)
        .unwrap();
    assert_eq!(
        pull_one(&config, "remote-changed.txt", false, ExecutionMode::Apply)
            .unwrap()
            .status,
        TransferStatus::Transferred
    );
    assert_eq!(
        std::fs::read(local.path().join("remote-changed.txt")).unwrap(),
        remote_changed
    );

    std::fs::write(local.path().join("local-changed.txt"), local_changed).unwrap();
    seed_state(local.path(), "local-changed.txt", known);
    ftp.upload_bytes(&remote_path("local-changed.txt"), known)
        .unwrap();
    assert_eq!(
        push_one(&config, "local-changed.txt", false, ExecutionMode::Apply)
            .unwrap()
            .status,
        TransferStatus::Transferred
    );
    assert_eq!(
        ftp.download(&remote_path("local-changed.txt")).unwrap(),
        local_changed
    );
    let state = StateFile::load_or_default(&local.path().join(".ferry/state.json")).unwrap();
    assert_eq!(
        state.files["remote-changed.txt"].sha256,
        hash_bytes(remote_changed)
    );
    assert_eq!(
        state.files["local-changed.txt"].sha256,
        hash_bytes(local_changed)
    );
}

#[test]
#[ignore]
fn single_file_untracked_conflict_preserves_both_sides_and_state() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "untracked.txt";
    let local_bytes = b"local untracked\n";
    let remote_bytes = b"remote untracked\n";
    std::fs::write(local.path().join(rel), local_bytes).unwrap();
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.upload_bytes(&remote_path(rel), remote_bytes).unwrap();

    let pull_error = pull_one(&config, rel, false, ExecutionMode::Apply).unwrap_err();
    let push_error = push_one(&config, rel, false, ExecutionMode::Apply).unwrap_err();

    assert!(pull_error.downcast_ref::<Exit>().is_some());
    assert!(push_error.downcast_ref::<Exit>().is_some());
    assert_eq!(std::fs::read(local.path().join(rel)).unwrap(), local_bytes);
    assert_eq!(ftp.download(&remote_path(rel)).unwrap(), remote_bytes);
    assert!(!local.path().join(".ferry/state.json").exists());
}

#[test]
#[ignore]
fn fetch_remote_one_returns_bytes_without_mutating_local_or_state() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "prepared/fetch-only.txt";
    let local_bytes = b"local bytes must remain exact\n\0";
    let remote_bytes = b"controlled remote payload\n\0\xff";
    let local_path = local.path().join(rel);
    let state_path = local.path().join(".ferry/state.json");
    std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
    std::fs::write(&local_path, local_bytes).unwrap();
    seed_state(local.path(), rel, b"previously synchronized bytes\n");
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("prepared")).unwrap();
    ftp.upload_bytes(&remote_path(rel), remote_bytes).unwrap();
    let remote_mtime = ftp.mtime(&remote_path(rel)).unwrap();
    let local_before = std::fs::read(&local_path).unwrap();
    let state_before = std::fs::read(&state_path).unwrap();

    let remote = fetch_remote_one(&config, rel).unwrap();

    assert_eq!(remote.bytes, remote_bytes);
    assert_eq!(remote.sha256, hash_bytes(remote_bytes));
    assert_eq!(remote.size, remote_bytes.len() as u64);
    assert_eq!(remote.mtime, remote_mtime);
    assert_eq!(std::fs::read(&local_path).unwrap(), local_before);
    assert_eq!(std::fs::read(&state_path).unwrap(), state_before);
}

#[test]
fn fetch_remote_one_rejects_an_unsafe_path_before_loading_config() {
    let local = tempfile::tempdir().unwrap();
    let config = local.path().join(".ferry.toml");
    let rel = "../escape";
    std::fs::write(&config, b"this is not valid TOML = [").unwrap();

    let error = fetch_remote_one(&config, rel).unwrap_err();
    let diagnostic = format!("{error:#}");

    assert!(diagnostic.contains(rel), "got: {diagnostic}");
    assert!(
        diagnostic.contains("must be a relative path under local_root with no '..' segments"),
        "got: {diagnostic}"
    );
}

#[test]
#[ignore]
fn prepared_force_pull_installs_the_fetched_remote_and_updates_state() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "prepared/force-install.txt";
    let equal_rel = "prepared/force-install-equal.txt";
    let local_bytes = b"local bytes before forced install\n\0";
    let remote_bytes = b"fetched remote bytes for forced install\n\0\xff";
    let equal_bytes = b"same bytes still require a forced install\n\0";
    let local_path = local.path().join(rel);
    let equal_local_path = local.path().join(equal_rel);
    let retained_equal_link_path = local.path().join("prepared/retained-equal-link.txt");
    let state_path = local.path().join(".ferry/state.json");
    std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
    std::fs::write(&local_path, local_bytes).unwrap();
    std::fs::write(&equal_local_path, equal_bytes).unwrap();
    std::fs::hard_link(&equal_local_path, &retained_equal_link_path).unwrap();
    seed_state(local.path(), rel, local_bytes);
    seed_state(local.path(), equal_rel, equal_bytes);
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("prepared")).unwrap();
    ftp.upload_bytes(&remote_path(rel), remote_bytes).unwrap();
    ftp.upload_bytes(&remote_path(equal_rel), equal_bytes)
        .unwrap();
    let remote_mtime = ftp.mtime(&remote_path(rel)).unwrap();
    let equal_remote_mtime = ftp.mtime(&remote_path(equal_rel)).unwrap();
    let local_before = std::fs::read(&local_path).unwrap();
    let equal_local_before = std::fs::read(&equal_local_path).unwrap();
    let state_before = std::fs::read(&state_path).unwrap();

    let prepared = prepare_force_pull_one(&config, rel).unwrap();
    let equal_prepared = prepare_force_pull_one(&config, equal_rel).unwrap();

    assert_eq!(std::fs::read(&local_path).unwrap(), local_before);
    assert_eq!(
        std::fs::read(&equal_local_path).unwrap(),
        equal_local_before
    );
    assert_eq!(std::fs::read(&state_path).unwrap(), state_before);

    let outcome = apply_prepared_pull(prepared, ExecutionMode::Apply).unwrap();
    let equal_outcome = apply_prepared_pull(equal_prepared, ExecutionMode::Apply).unwrap();

    assert_eq!(
        outcome,
        TransferOutcome::new(rel, TransferStatus::Transferred)
    );
    assert_eq!(
        equal_outcome,
        TransferOutcome::new(equal_rel, TransferStatus::Transferred)
    );
    assert_eq!(std::fs::read(&local_path).unwrap(), remote_bytes);
    assert_eq!(std::fs::read(&equal_local_path).unwrap(), equal_bytes);
    let state = StateFile::load_or_default(&state_path).unwrap();
    let record = state.files.get(rel).unwrap();
    assert_eq!(record.sha256, hash_bytes(remote_bytes));
    assert_eq!(record.size, remote_bytes.len() as u64);
    assert_eq!(record.remote_mtime, remote_mtime);
    let equal_record = state.files.get(equal_rel).unwrap();
    assert_eq!(equal_record.sha256, hash_bytes(equal_bytes));
    assert_eq!(equal_record.size, equal_bytes.len() as u64);
    assert_eq!(equal_record.remote_mtime, equal_remote_mtime);

    let post_install_bytes = b"target mutation after physical install\n";
    std::fs::write(&equal_local_path, post_install_bytes).unwrap();
    assert_eq!(
        std::fs::read(&equal_local_path).unwrap(),
        post_install_bytes
    );
    assert_eq!(
        std::fs::read(&retained_equal_link_path).unwrap(),
        equal_bytes,
        "force apply must replace the equal-content target, not leave the original hard link"
    );
}

#[test]
#[ignore]
fn prepared_force_pull_rejects_a_local_change_after_preparation() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "prepared/stale-local.txt";
    let local_path = local.path().join(rel);
    let state_path = local.path().join(".ferry/state.json");
    let edited_bytes = b"edited after preparation\n\0";
    std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
    std::fs::write(&local_path, b"local before preparation\n").unwrap();
    seed_state(local.path(), rel, b"previously synchronized bytes\n");
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("prepared")).unwrap();
    ftp.upload_bytes(&remote_path(rel), b"remote fetched during preparation\n")
        .unwrap();
    let prepared = prepare_force_pull_one(&config, rel).unwrap();
    let state_before_apply = std::fs::read(&state_path).unwrap();
    std::fs::write(&local_path, edited_bytes).unwrap();

    let error = apply_prepared_pull(prepared, ExecutionMode::Apply).unwrap_err();

    assert!(
        error
            .downcast_ref::<Exit>()
            .is_some_and(|exit| matches!(exit, Exit::Conflict(_))),
        "got: {error:#}"
    );
    assert_eq!(std::fs::read(&local_path).unwrap(), edited_bytes);
    assert_eq!(std::fs::read(&state_path).unwrap(), state_before_apply);
}

#[test]
#[ignore]
fn prepared_force_pull_requires_a_remote_file() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "prepared/missing-remote.txt";
    let absent_rel = "prepared/also-missing-remotely.txt";
    let local_path = local.path().join(rel);
    let absent_local_path = local.path().join(absent_rel);
    let state_path = local.path().join(".ferry/state.json");
    let local_bytes = b"existing local bytes must remain\n\0";
    std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
    std::fs::write(&local_path, local_bytes).unwrap();
    seed_state(local.path(), rel, b"previously synchronized bytes\n");
    let state_before = std::fs::read(&state_path).unwrap();

    let error = prepare_force_pull_one(&config, rel).unwrap_err();

    assert!(
        format!("{error:#}").contains(&format!("remote has no {rel}")),
        "got: {error:#}"
    );
    assert_eq!(std::fs::read(&local_path).unwrap(), local_bytes);
    assert_eq!(std::fs::read(&state_path).unwrap(), state_before);

    let absent_error = prepare_force_pull_one(&config, absent_rel).unwrap_err();

    assert!(
        format!("{absent_error:#}").contains(&format!("remote has no {absent_rel}")),
        "got: {absent_error:#}"
    );
    assert!(!absent_local_path.exists());
    assert_eq!(std::fs::read(&state_path).unwrap(), state_before);
}

fn write_lsp_frame(writer: &mut impl Write, value: &serde_json::Value) {
    let payload = serde_json::to_vec(value).unwrap();
    write!(writer, "Content-Length: {}\r\n\r\n", payload.len()).unwrap();
    writer.write_all(&payload).unwrap();
    writer.flush().unwrap();
}

fn valid_lsp_content_type(value: &str) -> bool {
    let mut parts = value.split(';').map(str::trim);
    if !parts
        .next()
        .is_some_and(|mime| mime.eq_ignore_ascii_case("application/vscode-jsonrpc"))
    {
        return false;
    }
    let Some(charset) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    let Some((name, value)) = charset.split_once('=') else {
        return false;
    };
    name.trim().eq_ignore_ascii_case("charset")
        && matches!(value.trim().to_ascii_lowercase().as_str(), "utf-8" | "utf8")
}

fn read_lsp_frame(reader: &mut impl BufRead) -> Result<Option<serde_json::Value>, String> {
    let mut content_length = None;
    let mut content_type = None;
    let mut saw_header = false;
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| format!("reading LSP stdout header: {error}"))?;
        if read == 0 {
            if saw_header {
                return Err("truncated LSP header at stdout EOF".to_string());
            }
            return Ok(None);
        }
        saw_header = true;
        if line == "\r\n" {
            break;
        }
        let line = line
            .strip_suffix("\r\n")
            .ok_or_else(|| "every stdout header line must use CRLF framing".to_string())?;
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| "unframed stdout byte or malformed LSP header".to_string())?;
        if name.eq_ignore_ascii_case("Content-Length") {
            if content_length.is_some() {
                return Err("duplicate Content-Length header".to_string());
            }
            let value = value.trim();
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(format!("invalid Content-Length header: {value:?}"));
            }
            content_length = Some(
                value
                    .parse::<usize>()
                    .map_err(|error| format!("invalid Content-Length header: {error}"))?,
            );
        } else if name.eq_ignore_ascii_case("Content-Type") {
            if content_type.is_some() {
                return Err("duplicate Content-Type header".to_string());
            }
            let value = value.trim();
            if !valid_lsp_content_type(value) {
                return Err(format!("invalid Content-Type header: {value:?}"));
            }
            content_type = Some(value.to_string());
        } else {
            return Err(format!("unsupported LSP stdout header: {name}"));
        }
    }
    let content_length =
        content_length.ok_or_else(|| "LSP stdout frame must have Content-Length".to_string())?;
    let mut payload = vec![0; content_length];
    reader
        .read_exact(&mut payload)
        .map_err(|error| format!("reading complete LSP stdout payload: {error}"))?;
    let value = serde_json::from_slice(&payload)
        .map_err(|error| format!("LSP stdout payload must be JSON: {error}"))?;
    Ok(Some(value))
}

#[test]
fn lsp_frame_parser_rejects_unknown_colon_header() {
    let bytes = b"X-Hostile: uploaded\r\nContent-Length: 2\r\n\r\n{}";
    let error = read_lsp_frame(&mut BufReader::new(&bytes[..])).unwrap_err();

    assert!(error.contains("unsupported LSP stdout header: X-Hostile"));
}

#[test]
fn lsp_frame_parser_rejects_invalid_content_type() {
    let bytes = b"Content-Type: text/plain\r\nContent-Length: 2\r\n\r\n{}";
    let error = read_lsp_frame(&mut BufReader::new(&bytes[..])).unwrap_err();

    assert!(error.contains("invalid Content-Type header"));
}

#[test]
fn lsp_frame_parser_accepts_valid_optional_content_type() {
    let bytes =
        b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 2\r\n\r\n{}";
    let frame = read_lsp_frame(&mut BufReader::new(&bytes[..]))
        .expect("valid frame")
        .expect("framed payload");

    assert_eq!(frame, serde_json::json!({}));
}

const LSP_TEST_TIMEOUT: Duration = Duration::from_secs(10);

enum LspStdoutEvent {
    Frame(serde_json::Value),
    Eof,
    Error(String),
}

fn spawn_lsp_stdout_reader(stdout: impl Read + Send + 'static) -> Receiver<LspStdoutEvent> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let event = match read_lsp_frame(&mut reader) {
                Ok(Some(frame)) => LspStdoutEvent::Frame(frame),
                Ok(None) => LspStdoutEvent::Eof,
                Err(error) => LspStdoutEvent::Error(error),
            };
            let finished = !matches!(event, LspStdoutEvent::Frame(_));
            if sender.send(event).is_err() || finished {
                return;
            }
        }
    });
    receiver
}

fn receive_lsp_frame(receiver: &Receiver<LspStdoutEvent>, context: &str) -> serde_json::Value {
    match receiver.recv_timeout(LSP_TEST_TIMEOUT) {
        Ok(LspStdoutEvent::Frame(frame)) => frame,
        Ok(LspStdoutEvent::Eof) => panic!("stdout closed while waiting for {context}"),
        Ok(LspStdoutEvent::Error(error)) => {
            panic!("invalid LSP stdout while waiting for {context}: {error}")
        }
        Err(error) => panic!("timed out waiting for {context}: {error}"),
    }
}

fn receive_lsp_eof(receiver: &Receiver<LspStdoutEvent>) {
    match receiver.recv_timeout(LSP_TEST_TIMEOUT) {
        Ok(LspStdoutEvent::Eof) => {}
        Ok(LspStdoutEvent::Frame(frame)) => panic!("unexpected framed message after exit: {frame}"),
        Ok(LspStdoutEvent::Error(error)) => panic!("invalid LSP stdout before exit: {error}"),
        Err(error) => panic!("timed out waiting for LSP stdout EOF: {error}"),
    }
}

fn spawn_output_drain(mut output: impl Read + Send + 'static) -> Receiver<Result<Vec<u8>, String>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = output
            .read_to_end(&mut bytes)
            .map(|_| bytes)
            .map_err(|error| format!("reading child output: {error}"));
        let _ = sender.send(result);
    });
    receiver
}

struct BoundedChild {
    child: Child,
    reaped: bool,
}

impl BoundedChild {
    fn new(child: Child) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    fn wait(&mut self, timeout: Duration) -> Result<ExitStatus, String> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    return Ok(status);
                }
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    let _ = self.child.kill();
                    let status = self
                        .child
                        .wait()
                        .map_err(|error| format!("reaping timed-out ferry-lsp: {error}"))?;
                    self.reaped = true;
                    return Err(format!(
                        "ferry-lsp did not exit within {timeout:?}; killed with {status}"
                    ));
                }
                Err(error) => return Err(format!("polling ferry-lsp exit: {error}")),
            }
        }
    }
}

impl Drop for BoundedChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
#[ignore]
fn ferry_lsp_scoped_sync_stdout_is_only_content_length_framed_json_rpc() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let _config = write_config(local.path(), &fixture);
    let file_path = local.path().join("lsp-stdout-probe.txt");
    let file_text = "scoped sync stdout probe\n";
    std::fs::write(&file_path, file_text).unwrap();
    let uri = format!("file://{}", file_path.display());

    let mut child = BoundedChild::new(
        Command::new(env!("CARGO_BIN_EXE_ferry-lsp"))
            .current_dir(local.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdin = child.child.stdin.take().unwrap();
    let stdout = spawn_lsp_stdout_reader(child.child.stdout.take().unwrap());
    let stderr = spawn_output_drain(child.child.stderr.take().unwrap());

    write_lsp_frame(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": null,
                "rootUri": format!("file://{}", local.path().display()),
                "capabilities": {}
            }
        }),
    );
    let initialize = receive_lsp_frame(&stdout, "initialize response");
    assert_eq!(initialize["id"], 1);
    assert!(initialize.get("error").is_none());

    write_lsp_frame(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }),
    );
    write_lsp_frame(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "text",
                    "version": 1,
                    "text": file_text
                }
            }
        }),
    );
    write_lsp_frame(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "workspace/executeCommand",
            "params": {
                "command": "ferry.syncFile",
                "arguments": [uri]
            }
        }),
    );

    let mut acknowledged = false;
    let mut feedback = None;
    for _ in 0..8 {
        let message = receive_lsp_frame(&stdout, "scoped-sync response or feedback");
        if message.get("id") == Some(&serde_json::json!(2)) {
            assert!(message.get("error").is_none());
            acknowledged = true;
        } else if message.get("method") == Some(&serde_json::json!("window/showMessage")) {
            feedback = message["params"]["message"].as_str().map(str::to_string);
        }
        if acknowledged && feedback.is_some() {
            break;
        }
    }
    assert!(acknowledged, "scoped-sync command acknowledgement");
    let feedback = feedback.expect("typed scoped-sync feedback notification");
    assert!(feedback.starts_with("ferry: sync complete:"));
    assert!(feedback.contains("uploaded"));

    write_lsp_frame(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "shutdown",
            "params": null
        }),
    );
    let shutdown = receive_lsp_frame(&stdout, "shutdown response");
    assert_eq!(shutdown["id"], 3);
    assert!(shutdown.get("error").is_none());
    write_lsp_frame(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "exit",
            "params": null
        }),
    );
    drop(stdin);

    let status = child
        .wait(LSP_TEST_TIMEOUT)
        .unwrap_or_else(|error| panic!("{error}"));
    receive_lsp_eof(&stdout);
    let stderr = stderr
        .recv_timeout(LSP_TEST_TIMEOUT)
        .unwrap_or_else(|error| panic!("timed out waiting for ferry-lsp stderr EOF: {error}"))
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        status.success(),
        "ferry-lsp failed: {}",
        String::from_utf8_lossy(&stderr)
    );

    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    assert_eq!(
        ftp.download(&remote_path("lsp-stdout-probe.txt")).unwrap(),
        file_text.as_bytes()
    );
}
