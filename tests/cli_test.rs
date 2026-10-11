use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ferry"))
}

const HOSTILE_FILE_BYTES: &[u8] = b"hostile remote bytes";
const HOSTILE_REPLY_MARKER: &str = "ATTACKER_MARKER";
const HOSTILE_REPLY_SECOND_LINE: &str = "attacker-second-line";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostileHashOperation {
    Mdtm,
    Size,
    Retr,
}

#[derive(Clone)]
enum FakeFtpScenario {
    Missing,
    DirectoryPrefix,
    TypeConflict,
    TypeConflictWithClean(Vec<u8>, Vec<u8>),
    FileConflict(Vec<u8>),
    TwoFiles(Vec<u8>, Vec<u8>),
    SharedDirectory {
        current: Arc<AtomicBool>,
        root_lists: Arc<AtomicUsize>,
    },
    BareLegacy(Vec<u8>),
    HostileHashReply(HostileHashOperation),
}

impl FakeFtpScenario {
    fn listing(&self, path: &str) -> String {
        match (self, path) {
            (Self::Missing, "/remote") => String::new(),
            (Self::DirectoryPrefix, "/remote") => {
                "drwxr-xr-x 1 owner group 0 Aug 10 12:00 area\r\n".into()
            }
            (Self::DirectoryPrefix, "/remote/area") => String::new(),
            (Self::TypeConflict, "/remote") => {
                "drwxr-xr-x 1 owner group 0 Aug 10 12:00 type.c\r\n".into()
            }
            (Self::TypeConflict, "/remote/type.c") => String::new(),
            (Self::TypeConflictWithClean(clean, _), "/remote") => format!(
                "drwxr-xr-x 1 owner group 0 Aug 10 12:00 type.c\r\n-rw-r--r-- 1 owner group {} Aug 10 12:00 clean.c\r\n",
                clean.len()
            ),
            (Self::TypeConflictWithClean(_, child), "/remote/type.c") => format!(
                "-rw-r--r-- 1 owner group {} Aug 10 12:00 child.c\r\n",
                child.len()
            ),
            (Self::FileConflict(bytes), "/remote") => format!(
                "-rw-r--r-- 1 owner group {} Aug 10 12:00 conflict.c\r\n",
                bytes.len()
            ),
            (Self::TwoFiles(first, second), "/remote") => format!(
                "-rw-r--r-- 1 owner group {} Aug 10 12:00 a.c\r\n-rw-r--r-- 1 owner group {} Aug 10 12:00 b.c\r\n",
                first.len(),
                second.len()
            ),
            (
                Self::SharedDirectory {
                    current,
                    root_lists,
                },
                "/remote",
            ) => {
                if root_lists.fetch_add(1, Ordering::SeqCst) > 3 {
                    current.store(false, Ordering::SeqCst);
                }
                "drwxr-xr-x 1 owner group 0 Aug 10 12:00 shared\r\n".into()
            }
            (Self::SharedDirectory { .. }, "/remote/shared") => String::new(),
            (Self::BareLegacy(_), "/remote") => {
                "drwxr-xr-x 1 owner group 0 Aug 10 12:00 legacy\r\n".into()
            }
            (Self::BareLegacy(bytes), "/remote/legacy") => format!(
                "-rw-r--r-- 1 owner group {} Aug 10 12:00 nested.c\r\n",
                bytes.len()
            ),
            (Self::HostileHashReply(_), "/remote") => format!(
                "-rw-r--r-- 1 owner group {} Aug 10 12:00 hostile.c\r\n",
                HOSTILE_FILE_BYTES.len()
            ),
            _ => String::new(),
        }
    }

    fn file(&self, path: &str) -> Option<&[u8]> {
        match (self, path) {
            (Self::FileConflict(bytes), "/remote/conflict.c") => Some(bytes),
            (Self::TwoFiles(first, _), "/remote/a.c") => Some(first),
            (Self::TwoFiles(_, second), "/remote/b.c") => Some(second),
            (Self::BareLegacy(bytes), "/remote/legacy/nested.c") => Some(bytes),
            (Self::TypeConflictWithClean(clean, _), "/remote/clean.c") => Some(clean),
            (Self::TypeConflictWithClean(_, child), "/remote/type.c/child.c") => Some(child),
            (Self::HostileHashReply(_), "/remote/hostile.c") => Some(HOSTILE_FILE_BYTES),
            _ => None,
        }
    }
}

struct FakeFtpServer {
    port: u16,
    handle: Option<JoinHandle<()>>,
}

impl FakeFtpServer {
    fn spawn(scenario: FakeFtpScenario) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut control, _) = listener.accept().unwrap();
            writeln!(control, "220 Ferry CLI test server\r").unwrap();
            control.flush().unwrap();
            let mut reader = BufReader::new(control.try_clone().unwrap());
            let mut data_listener = None;

            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
                        ) =>
                    {
                        break;
                    }
                    Err(error) => panic!("reading fake FTP command: {error}"),
                };
                let command = line.trim_end_matches(['\r', '\n']);
                let verb = command
                    .split_ascii_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_ascii_uppercase();
                match verb.as_str() {
                    "USER" => write_control(&mut control, "331 Password required"),
                    "PASS" => write_control(&mut control, "230 Logged in"),
                    "TYPE" | "OPTS" | "NOOP" => write_control(&mut control, "200 OK"),
                    "SYST" => write_control(&mut control, "215 UNIX Type: L8"),
                    "PWD" => write_control(&mut control, "257 \"/remote\""),
                    "EPSV" => {
                        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
                        let port = listener.local_addr().unwrap().port();
                        data_listener = Some(listener);
                        write_control(
                            &mut control,
                            &format!("229 Entering Extended Passive Mode (|||{port}|)"),
                        );
                    }
                    "PASV" => {
                        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
                        let port = listener.local_addr().unwrap().port();
                        data_listener = Some(listener);
                        write_control(
                            &mut control,
                            &format!(
                                "227 Entering Passive Mode (127,0,0,1,{},{})",
                                port / 256,
                                port % 256
                            ),
                        );
                    }
                    "LIST" => {
                        let path = command
                            .split_once(' ')
                            .map(|(_, path)| path)
                            .unwrap_or("/remote");
                        transfer_data(
                            &mut control,
                            data_listener.take().expect("LIST after passive command"),
                            scenario.listing(path).as_bytes(),
                        );
                    }
                    "MDTM" => {
                        let path = command.split_once(' ').unwrap().1;
                        if matches!(
                            scenario,
                            FakeFtpScenario::HostileHashReply(HostileHashOperation::Mdtm)
                        ) && path == "/remote/hostile.c"
                        {
                            write_hostile_control(&mut control);
                        } else if scenario.file(path).is_some() {
                            write_control(&mut control, "213 20260810120000");
                        } else {
                            write_control(&mut control, "550 Missing");
                        }
                    }
                    "SIZE" => {
                        let path = command.split_once(' ').unwrap().1;
                        if matches!(
                            scenario,
                            FakeFtpScenario::HostileHashReply(HostileHashOperation::Size)
                        ) && path == "/remote/hostile.c"
                        {
                            write_hostile_control(&mut control);
                        } else if let Some(bytes) = scenario.file(path) {
                            write_control(&mut control, &format!("213 {}", bytes.len()));
                        } else {
                            write_control(&mut control, "550 Missing");
                        }
                    }
                    "RETR" => {
                        let path = command.split_once(' ').unwrap().1;
                        if matches!(
                            scenario,
                            FakeFtpScenario::HostileHashReply(HostileHashOperation::Retr)
                        ) && path == "/remote/hostile.c"
                        {
                            let listener =
                                data_listener.take().expect("RETR after passive command");
                            let (data, _) = listener.accept().unwrap();
                            drop(data);
                            drop(listener);
                            write_hostile_control(&mut control);
                        } else {
                            transfer_data(
                                &mut control,
                                data_listener.take().expect("RETR after passive command"),
                                scenario.file(path).expect("known RETR path"),
                            );
                        }
                    }
                    "QUIT" => {
                        write_control(&mut control, "221 Goodbye");
                        break;
                    }
                    _ => write_control(&mut control, "200 OK"),
                }
            }
        });
        Self {
            port,
            handle: Some(handle),
        }
    }
}

impl Drop for FakeFtpServer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.join().unwrap();
        }
    }
}

fn write_control(stream: &mut TcpStream, line: &str) {
    write!(stream, "{line}\r\n").unwrap();
    stream.flush().unwrap();
}

fn write_hostile_control(stream: &mut TcpStream) {
    write!(
        stream,
        "550-\x1b[31m{HOSTILE_REPLY_MARKER}\r\n550 {HOSTILE_REPLY_SECOND_LINE}\r\n"
    )
    .unwrap();
    stream.flush().unwrap();
}

fn transfer_data(control: &mut TcpStream, listener: TcpListener, bytes: &[u8]) {
    write_control(control, "150 Opening data connection");
    let (mut data, _) = listener.accept().unwrap();
    data.write_all(bytes).unwrap();
    data.flush().unwrap();
    drop(data);
    write_control(control, "226 Transfer complete");
}

fn scoped_config(project: &std::path::Path, port: u16) -> std::path::PathBuf {
    let path = project.join(ferry::names::CONFIG_FILE);
    std::fs::write(
        &path,
        format!(
            r#"
[connection]
host = "127.0.0.1"
port = {port}
user = "u"
password = "p"
[paths]
local_root = "."
remote_root = "/remote"
[sync]
ignore = [".ferry.toml"]
"#
        ),
    )
    .unwrap();
    path
}

fn scoped_state_path(project: &std::path::Path) -> std::path::PathBuf {
    project.join(ferry::names::STATE_DIR).join("state.json")
}

fn write_compact_scoped_state(
    project: &std::path::Path,
    state: &ferry::state::StateFile,
) -> Vec<u8> {
    let path = scoped_state_path(project);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = serde_json::to_vec(state).unwrap();
    std::fs::write(path, &bytes).unwrap();
    bytes
}

struct SequenceGate {
    current: Mutex<VecDeque<bool>>,
    claims: AtomicUsize,
}

impl SequenceGate {
    fn new(current: impl IntoIterator<Item = bool>) -> Self {
        Self {
            current: Mutex::new(current.into_iter().collect()),
            claims: AtomicUsize::new(0),
        }
    }
}

impl ferry::commands::sync::CommitGate for SequenceGate {
    fn is_current(&self) -> bool {
        self.current.lock().unwrap().pop_front().unwrap_or(false)
    }

    fn commit(
        &self,
        mutation: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<ferry::commands::sync::CommitDecision> {
        self.claims.fetch_add(1, Ordering::SeqCst);
        mutation()?;
        Ok(ferry::commands::sync::CommitDecision::Committed)
    }
}

struct ErrorOnSecondClaim {
    claims: AtomicUsize,
}

impl ferry::commands::sync::CommitGate for ErrorOnSecondClaim {
    fn is_current(&self) -> bool {
        true
    }

    fn commit(
        &self,
        mutation: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<ferry::commands::sync::CommitDecision> {
        if self.claims.fetch_add(1, Ordering::SeqCst) == 1 {
            anyhow::bail!("injected second claim failure");
        }
        mutation()?;
        Ok(ferry::commands::sync::CommitDecision::Committed)
    }
}

struct LiveGate {
    current: Arc<AtomicBool>,
}

impl ferry::commands::sync::CommitGate for LiveGate {
    fn is_current(&self) -> bool {
        self.current.load(Ordering::SeqCst)
    }

    fn commit(
        &self,
        mutation: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<ferry::commands::sync::CommitDecision> {
        if !self.is_current() {
            return Ok(ferry::commands::sync::CommitDecision::Cancelled);
        }
        mutation()?;
        Ok(ferry::commands::sync::CommitDecision::Committed)
    }
}

#[test]
fn sync_cli_executes_bare_legacy_sync_and_rejects_invalid_scope_combinations() {
    let bytes = b"from legacy remote".to_vec();
    let server = FakeFtpServer::spawn(FakeFtpScenario::BareLegacy(bytes.clone()));
    let project = tempfile::tempdir().unwrap();
    let config = scoped_config(project.path(), server.port);

    let bare = bin()
        .args(["sync", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert!(
        bare.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&bare.stderr)
    );
    assert_eq!(
        std::fs::read(project.path().join("legacy/nested.c")).unwrap(),
        bytes
    );
    assert!(String::from_utf8_lossy(&bare.stdout).contains("downloaded legacy/nested.c"));
    let state = ferry::state::StateFile::load_or_default(
        &project
            .path()
            .join(ferry::names::STATE_DIR)
            .join("state.json"),
    )
    .unwrap();
    assert_eq!(
        state
            .files
            .get("legacy/nested.c")
            .map(|record| record.sha256.as_str()),
        Some(ferry::hash::hash_bytes(b"from legacy remote").as_str())
    );

    let multiple = bin().args(["sync", "one", "two"]).output().unwrap();
    assert_eq!(multiple.status.code(), Some(2));

    let path_and_select = bin().args(["sync", "one", "--select"]).output().unwrap();
    assert_eq!(path_and_select.status.code(), Some(2));
}

#[test]
fn scoped_sync_missing_path_is_exact_and_creates_no_state() {
    let server = FakeFtpServer::spawn(FakeFtpScenario::Missing);
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("unselected.c"), b"must stay local").unwrap();
    let config = scoped_config(project.path(), server.port);

    let output = bin()
        .args(["sync", "missing.c", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("path not found locally or remotely"),
        "stderr={stderr}"
    );
    assert!(project.path().join("unselected.c").is_file());
    assert!(
        !project.path().join(ferry::names::STATE_DIR).exists(),
        "missing explicit path must not create state"
    );
}

#[test]
fn scoped_sync_missing_leaf_below_local_directory_prefix_is_not_found() {
    let server = FakeFtpServer::spawn(FakeFtpScenario::Missing);
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir(project.path().join("area")).unwrap();
    let config = scoped_config(project.path(), server.port);

    let output = bin()
        .args(["sync", "area/missing.c", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("path not found locally or remotely"),
        "stderr={stderr}"
    );
    assert!(project.path().join("area").is_dir());
    assert!(
        !project.path().join(ferry::names::STATE_DIR).exists(),
        "missing descendant must not create state"
    );
}

#[test]
fn scoped_sync_missing_leaf_below_remote_directory_prefix_is_not_found() {
    let server = FakeFtpServer::spawn(FakeFtpScenario::DirectoryPrefix);
    let project = tempfile::tempdir().unwrap();
    let config = scoped_config(project.path(), server.port);

    let output = bin()
        .args(["sync", "area/missing.c", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("path not found locally or remotely"),
        "stderr={stderr}"
    );
    assert!(!project.path().join("area").exists());
    assert!(
        !project.path().join(ferry::names::STATE_DIR).exists(),
        "missing descendant must not create state"
    );
}

#[test]
fn scoped_sync_empty_root_noop_creates_no_state() {
    let server = FakeFtpServer::spawn(FakeFtpScenario::Missing);
    let project = tempfile::tempdir().unwrap();
    let config = scoped_config(project.path(), server.port);

    let output = bin()
        .args(["sync", ".", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !project.path().join(ferry::names::STATE_DIR).exists(),
        "empty zero-commit sync must not create state"
    );
}

#[test]
fn scoped_sync_public_cancellation_before_first_claim_creates_no_state() {
    let server = FakeFtpServer::spawn(FakeFtpScenario::TwoFiles(
        b"first remote".to_vec(),
        b"second remote".to_vec(),
    ));
    let project = tempfile::tempdir().unwrap();
    let config = scoped_config(project.path(), server.port);
    let gate = SequenceGate::new([false]);

    let outcome = ferry::commands::sync::run_scoped(
        &config,
        ferry::commands::sync::scope::SyncScope::RootDirectory,
        false,
        ferry::commands::ExecutionMode::Apply,
        &gate,
    )
    .unwrap();

    assert!(outcome.cancelled);
    assert_eq!(gate.claims.load(Ordering::SeqCst), 0);
    assert!(!project.path().join("a.c").exists());
    assert!(!project.path().join("b.c").exists());
    assert!(!project.path().join(ferry::names::STATE_DIR).exists());
}

#[test]
fn scoped_sync_public_final_validation_cancellation_creates_no_state() {
    let current = Arc::new(AtomicBool::new(true));
    let root_lists = Arc::new(AtomicUsize::new(0));
    let server = FakeFtpServer::spawn(FakeFtpScenario::SharedDirectory {
        current: Arc::clone(&current),
        root_lists: Arc::clone(&root_lists),
    });
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir(project.path().join("shared")).unwrap();
    let config = scoped_config(project.path(), server.port);
    let gate = LiveGate { current };

    let outcome = ferry::commands::sync::run_scoped(
        &config,
        ferry::commands::sync::scope::SyncScope::RootDirectory,
        false,
        ferry::commands::ExecutionMode::Apply,
        &gate,
    )
    .unwrap();

    assert!(outcome.cancelled);
    assert!(outcome.events.is_empty());
    assert!(outcome.issues.is_empty());
    assert_eq!(root_lists.load(Ordering::SeqCst), 6);
    assert!(!project.path().join(ferry::names::STATE_DIR).exists());
}

#[test]
fn scoped_sync_public_partial_cancellation_saves_first_commit() {
    let first = b"first remote".to_vec();
    let server = FakeFtpServer::spawn(FakeFtpScenario::TwoFiles(
        first.clone(),
        b"second remote".to_vec(),
    ));
    let project = tempfile::tempdir().unwrap();
    let config = scoped_config(project.path(), server.port);
    let gate = SequenceGate::new([true, true, true, true, true, true, true, true, false]);

    let outcome = ferry::commands::sync::run_scoped(
        &config,
        ferry::commands::sync::scope::SyncScope::RootDirectory,
        false,
        ferry::commands::ExecutionMode::Apply,
        &gate,
    )
    .unwrap();

    assert!(outcome.cancelled);
    assert_eq!(gate.claims.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(project.path().join("a.c")).unwrap(), first);
    assert!(!project.path().join("b.c").exists());
    let state =
        ferry::state::StateFile::load_or_default(&scoped_state_path(project.path())).unwrap();
    assert!(state.files.contains_key("a.c"));
    assert!(!state.files.contains_key("b.c"));
}

#[test]
fn scoped_sync_public_partial_error_saves_first_commit() {
    let first = b"first remote".to_vec();
    let server = FakeFtpServer::spawn(FakeFtpScenario::TwoFiles(
        first.clone(),
        b"second remote".to_vec(),
    ));
    let project = tempfile::tempdir().unwrap();
    let config = scoped_config(project.path(), server.port);
    let gate = ErrorOnSecondClaim {
        claims: AtomicUsize::new(0),
    };

    let error = ferry::commands::sync::run_scoped(
        &config,
        ferry::commands::sync::scope::SyncScope::RootDirectory,
        false,
        ferry::commands::ExecutionMode::Apply,
        &gate,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("injected second claim failure"));
    assert_eq!(gate.claims.load(Ordering::SeqCst), 2);
    assert_eq!(std::fs::read(project.path().join("a.c")).unwrap(), first);
    assert!(!project.path().join("b.c").exists());
    let state =
        ferry::state::StateFile::load_or_default(&scoped_state_path(project.path())).unwrap();
    assert!(state.files.contains_key("a.c"));
    assert!(!state.files.contains_key("b.c"));
}

#[test]
fn scoped_sync_type_conflict_exits_one() {
    let server = FakeFtpServer::spawn(FakeFtpScenario::TypeConflict);
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("type.c"), b"local file").unwrap();
    let config = scoped_config(project.path(), server.port);

    let output = bin()
        .args(["sync", "type.c", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("type conflict"), "stderr={stderr}");
    assert_eq!(
        std::fs::read(project.path().join("type.c")).unwrap(),
        b"local file"
    );
    assert!(
        !project.path().join(ferry::names::STATE_DIR).exists(),
        "type-only conflict must not create state"
    );
}

#[test]
fn scoped_sync_type_conflict_keeps_clean_sibling_and_saves_progress() {
    let clean = b"clean remote sibling".to_vec();
    let expected_clean_hash = ferry::hash::hash_bytes(&clean);
    let server = FakeFtpServer::spawn(FakeFtpScenario::TypeConflictWithClean(
        clean.clone(),
        b"blocked descendant".to_vec(),
    ));
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("type.c"), b"local file").unwrap();
    let config = scoped_config(project.path(), server.port);

    let output = bin()
        .args(["sync", ".", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("type conflict"), "stderr={stderr}");
    assert_eq!(
        std::fs::read(project.path().join("type.c")).unwrap(),
        b"local file"
    );
    assert_eq!(
        std::fs::read(project.path().join("clean.c")).unwrap(),
        clean
    );
    assert!(!project.path().join("type.c").join("child.c").exists());

    let state = ferry::state::StateFile::load_or_default(
        &project
            .path()
            .join(ferry::names::STATE_DIR)
            .join("state.json"),
    )
    .unwrap();
    assert_eq!(
        state
            .files
            .get("clean.c")
            .map(|record| record.sha256.as_str()),
        Some(expected_clean_hash.as_str())
    );
    assert!(!state.files.contains_key("type.c"));
    assert!(!state.files.contains_key("type.c/child.c"));
}

#[test]
fn scoped_sync_file_conflict_exits_two() {
    let remote_bytes = b"remote bytes".to_vec();
    let server = FakeFtpServer::spawn(FakeFtpScenario::FileConflict(remote_bytes));
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("conflict.c"), b"local bytes").unwrap();
    let config = scoped_config(project.path(), server.port);
    let mut state = ferry::state::StateFile::default();
    state.files.insert(
        "conflict.c".into(),
        ferry::state::FileRecord {
            sha256: ferry::hash::hash_bytes(b"known bytes"),
            size: b"known bytes".len() as u64,
            remote_mtime: "2026-08-09T12:00:00Z".parse().unwrap(),
            last_synced: "2026-08-09T12:01:00Z".parse().unwrap(),
        },
    );
    state
        .save(
            &project
                .path()
                .join(ferry::names::STATE_DIR)
                .join("state.json"),
        )
        .unwrap();

    let output = bin()
        .args(["sync", "conflict.c", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("conflict"), "stderr={stderr}");
}

#[test]
fn scoped_sync_untracked_conflict_does_not_persist_capability_probe() {
    let remote_bytes = b"remote bytes".to_vec();
    let server = FakeFtpServer::spawn(FakeFtpScenario::FileConflict(remote_bytes));
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("conflict.c"), b"local bytes").unwrap();
    let config = scoped_config(project.path(), server.port);

    let output = bin()
        .args(["sync", "conflict.c", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(
        !project.path().join(ferry::names::STATE_DIR).exists(),
        "capability-only probe must not create state"
    );
}

#[test]
fn scoped_sync_unchanged_file_does_not_rewrite_state_or_capability() {
    let bytes = b"same bytes".to_vec();
    let server = FakeFtpServer::spawn(FakeFtpScenario::FileConflict(bytes.clone()));
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("conflict.c"), &bytes).unwrap();
    let config = scoped_config(project.path(), server.port);
    let mut state = ferry::state::StateFile::default();
    state.files.insert(
        "conflict.c".into(),
        ferry::state::FileRecord {
            sha256: ferry::hash::hash_bytes(&bytes),
            size: bytes.len() as u64,
            remote_mtime: "2026-08-10T12:00:00Z".parse().unwrap(),
            last_synced: "2026-08-10T12:01:00Z".parse().unwrap(),
        },
    );
    let before = write_compact_scoped_state(project.path(), &state);

    let output = bin()
        .args(["sync", "conflict.c", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(scoped_state_path(project.path())).unwrap(),
        before
    );
    let after =
        ferry::state::StateFile::load_or_default(&scoped_state_path(project.path())).unwrap();
    assert_eq!(after.server_supports_mdtm, None);
}

#[test]
fn scoped_sync_exact_stale_state_only_renders_skip_and_does_not_rewrite_state() {
    let server = FakeFtpServer::spawn(FakeFtpScenario::Missing);
    let project = tempfile::tempdir().unwrap();
    let config = scoped_config(project.path(), server.port);
    let mut state = ferry::state::StateFile::default();
    state.files.insert(
        "stale.c".into(),
        ferry::state::FileRecord {
            sha256: ferry::hash::hash_bytes(b"stale"),
            size: 5,
            remote_mtime: "2026-08-10T12:00:00Z".parse().unwrap(),
            last_synced: "2026-08-10T12:01:00Z".parse().unwrap(),
        },
    );
    let before = write_compact_scoped_state(project.path(), &state);

    let output = bin()
        .args(["sync", "stale.c", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "skip (not on local or remote): stale.c\n"
    );
    assert!(output.stdout.is_empty());
    assert!(!project.path().join("stale.c").exists());
    assert_eq!(
        std::fs::read(scoped_state_path(project.path())).unwrap(),
        before
    );
}

#[test]
fn scoped_sync_dry_run_does_not_create_state_or_local_file() {
    let remote_bytes = b"dry remote".to_vec();
    let server = FakeFtpServer::spawn(FakeFtpScenario::FileConflict(remote_bytes));
    let project = tempfile::tempdir().unwrap();
    let config = scoped_config(project.path(), server.port);

    let output = bin()
        .args(["sync", "conflict.c", "--dry-run", "--config"])
        .arg(&config)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!project.path().join("conflict.c").exists());
    assert!(!project.path().join(ferry::names::STATE_DIR).exists());
}

#[test]
fn scoped_sync_hostile_hash_replies_are_not_rendered() {
    for operation in [
        HostileHashOperation::Mdtm,
        HostileHashOperation::Size,
        HostileHashOperation::Retr,
    ] {
        let server = FakeFtpServer::spawn(FakeFtpScenario::HostileHashReply(operation));
        let project = tempfile::tempdir().unwrap();
        let config = scoped_config(project.path(), server.port);

        let output = bin()
            .args(["sync", "hostile.c", "--config"])
            .arg(&config)
            .output()
            .unwrap();
        let rendered = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        assert!(
            !rendered.contains(HOSTILE_REPLY_MARKER),
            "operation={operation:?}, rendered={rendered:?}"
        );
        assert!(
            !rendered.contains(HOSTILE_REPLY_SECOND_LINE),
            "operation={operation:?}, rendered={rendered:?}"
        );
        assert!(
            !rendered.contains('\u{1b}'),
            "operation={operation:?}, rendered={rendered:?}"
        );
        if operation == HostileHashOperation::Retr {
            assert_eq!(output.status.code(), Some(1), "rendered={rendered:?}");
            assert!(rendered.contains("ftp scoped download /remote/hostile.c"));
            assert!(
                !project.path().join(ferry::names::STATE_DIR).exists(),
                "zero-commit retrieval error must not create state"
            );
        } else {
            assert!(output.status.success(), "rendered={rendered:?}");
        }
    }
}

#[test]
fn help_lists_subcommands() {
    let out = bin().arg("--help").output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    for cmd in ["init", "status", "pull", "push", "sync", "rm"] {
        assert!(stdout.contains(cmd), "missing subcommand: {cmd}");
    }
}

#[test]
fn rm_requires_at_least_one_path() {
    // Bare `rm` must refuse rather than delete anything. The distinctive
    // message (not clap's "unrecognized subcommand") proves our own guard
    // fired, and it must happen before any config/connection work.
    let out = bin().arg("rm").output().unwrap();
    assert!(!out.status.success(), "bare rm should exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("at least one path"),
        "expected 'at least one path' guard; stderr={stderr}",
    );
}

#[test]
fn bare_rm_does_not_migrate_legacy_project_files() {
    let project = tempfile::tempdir().unwrap();
    let legacy_config = project.path().join(ferry::names::LEGACY_CONFIG_FILE);
    let legacy_state = project
        .path()
        .join(ferry::names::LEGACY_STATE_DIR)
        .join("state.json");
    std::fs::write(
        &legacy_config,
        r#"
[connection]
host = "127.0.0.1"
port = 1
user = "u"
password = "p"

[paths]
remote_root = "/"
"#,
    )
    .unwrap();
    ferry::state::StateFile::default()
        .save(&legacy_state)
        .unwrap();
    let config_before = std::fs::read(&legacy_config).unwrap();
    let state_before = std::fs::read(&legacy_state).unwrap();

    let out = bin()
        .arg("rm")
        .current_dir(project.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "bare rm should exit non-zero");
    assert!(stderr.contains("at least one path"), "stderr={stderr}");
    assert_eq!(std::fs::read(&legacy_config).unwrap(), config_before);
    assert_eq!(std::fs::read(&legacy_state).unwrap(), state_before);
    assert!(!project.path().join(ferry::names::CONFIG_FILE).exists());
    assert!(!project.path().join(ferry::names::STATE_DIR).exists());
}

#[test]
fn malformed_hook_json_does_not_echo_input() {
    let marker = "SENSITIVE-HOOK-MARKER-9dff6c";
    let mut child = bin()
        .arg("hook")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    write!(child.stdin.as_mut().unwrap(), "{{invalid:{marker}}}").unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr={stderr}");
    assert!(!stderr.contains(marker), "hook echoed input: {stderr}");
    assert!(stderr.contains("parsing hook envelope"), "stderr={stderr}");
}

#[test]
fn rm_rejects_unsafe_paths() {
    // A `..` path must be refused before we touch the server. Provide a valid
    // config so we get past config-load and reach path validation.
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join(".ferry.toml");
    std::fs::write(
        &cfg_path,
        r#"
[connection]
host = "127.0.0.1"
port = 1
user = "u"
password = "p"
[paths]
remote_root = "/"
"#,
    )
    .unwrap();
    let out = bin()
        .args(["rm", "../escape", "--config"])
        .arg(&cfg_path)
        .output()
        .unwrap();
    assert!(!out.status.success(), "rm ../escape should exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refusing path"),
        "expected 'refusing path' guard; stderr={stderr}",
    );
}

#[test]
fn missing_config_exits_3() {
    // A non-existent config path must surface as exit code 3 (config/auth
    // category) so Zed's tasks.json can prompt the user to fix .ferry.toml
    // rather than retry. Use a temp-dir path that's guaranteed not to exist.
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.toml");
    let out = bin()
        .args(["status", "--config"])
        .arg(&missing)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "expected exit 3 for missing config; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
}

#[test]
fn unknown_subcommand_is_non_zero() {
    // clap exits with its own non-zero code on parse errors; we don't try to
    // remap it. Just sanity-check we don't accidentally exit 0.
    let out = bin().args(["bogus"]).output().unwrap();
    assert_ne!(out.status.code(), Some(0));
}

#[test]
fn dry_run_does_not_migrate_legacy_names() {
    let dir = tempfile::tempdir().unwrap();
    let legacy_config = dir.path().join(ferry::names::LEGACY_CONFIG_FILE);
    let legacy_state = dir
        .path()
        .join(ferry::names::LEGACY_STATE_DIR)
        .join("state.json");
    std::fs::write(
        &legacy_config,
        r#"
[connection]
host = "127.0.0.1"
port = 1
user = "u"
password = "p"

[paths]
local_root = "."
remote_root = "/"
"#,
    )
    .unwrap();
    let state = ferry::state::StateFile::default();
    state.save(&legacy_state).unwrap();
    let config_before = std::fs::read(&legacy_config).unwrap();
    let state_before = std::fs::read(&legacy_state).unwrap();

    let out = bin()
        .args(["status", "--dry-run"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(1),
        "expected connection exit after loading legacy config; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ftp connect 127.0.0.1:1"),
        "legacy config was not read through; stderr={stderr}",
    );
    assert_eq!(std::fs::read(&legacy_config).unwrap(), config_before);
    assert_eq!(std::fs::read(&legacy_state).unwrap(), state_before);
    assert!(!dir.path().join(ferry::names::CONFIG_FILE).exists());
    assert!(!dir.path().join(ferry::names::STATE_DIR).exists());
}

#[test]
fn hook_dry_run_cooldown_reads_legacy_state_past_empty_current_dir() {
    let project = tempfile::tempdir().unwrap();
    let local_root = project.path().join("mirror");
    std::fs::create_dir(&local_root).unwrap();
    let target = local_root.join("target.txt");
    std::fs::write(&target, b"local bytes\n").unwrap();

    let config_path = project.path().join(ferry::names::CONFIG_FILE);
    std::fs::write(
        &config_path,
        r#"
[connection]
host = "127.0.0.1"
port = 1
user = "u"
password = "p"

[paths]
local_root = "mirror"
remote_root = "/"
"#,
    )
    .unwrap();

    let legacy_state_path = local_root
        .join(ferry::names::LEGACY_STATE_DIR)
        .join("state.json");
    let now = chrono::Utc::now();
    let mut state = ferry::state::StateFile::default();
    state.files.insert(
        "target.txt".into(),
        ferry::state::FileRecord {
            sha256: "known".into(),
            size: 12,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&legacy_state_path).unwrap();
    let current_state_dir = local_root.join(ferry::names::STATE_DIR);
    std::fs::create_dir(&current_state_dir).unwrap();

    let config_before = std::fs::read(&config_path).unwrap();
    let state_before = std::fs::read(&legacy_state_path).unwrap();
    let target_before = std::fs::read(&target).unwrap();

    let mut child = bin()
        .args(["hook", "--cooldown", "3600", "--dry-run"])
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    serde_json::to_writer(
        child.stdin.as_mut().unwrap(),
        &serde_json::json!({
            "tool_name": "Read",
            "tool_input": {"file_path": target},
        }),
    )
    .unwrap();
    child.stdin.as_mut().unwrap().flush().unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("within 3600s cooldown, skipping pull"),
        "legacy cooldown state was ignored and FTP was attempted; stderr={stderr}",
    );
    assert!(!stderr.contains("pull failed"), "stderr={stderr}");
    assert_eq!(std::fs::read(&config_path).unwrap(), config_before);
    assert_eq!(std::fs::read(&legacy_state_path).unwrap(), state_before);
    assert_eq!(std::fs::read(&target).unwrap(), target_before);
    assert!(!current_state_dir.join("state.json").exists());
    assert_eq!(std::fs::read_dir(&current_state_dir).unwrap().count(), 0);
}

#[test]
fn finds_config_upward() {
    let project = tempfile::tempdir().unwrap();
    let nested = project.path().join("nested/deeper");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        project.path().join(ferry::names::CONFIG_FILE),
        r#"
[connection]
host = "127.0.0.1"
port = 1
user = "u"
password = "p"

[paths]
remote_root = "/"
"#,
    )
    .unwrap();

    let out = bin().arg("status").current_dir(&nested).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ftp connect 127.0.0.1:1"),
        "ancestor config was not used; stderr={stderr}",
    );
    assert!(
        !stderr.contains("reading .ferry.toml"),
        "status looked only in the nested directory; stderr={stderr}",
    );
}

#[test]
fn explicit_legacy_config_refreshes_after_apply_migration() {
    let project = tempfile::tempdir().unwrap();
    let unrelated_cwd = tempfile::tempdir().unwrap();
    std::fs::write(
        unrelated_cwd.path().join(ferry::names::CONFIG_FILE),
        r#"
[connection]
host = "127.0.0.1"
port = 2
user = "unrelated"
password = "p"

[paths]
remote_root = "/"
"#,
    )
    .unwrap();
    let legacy_config = project.path().join(ferry::names::LEGACY_CONFIG_FILE);
    std::fs::write(
        &legacy_config,
        r#"
[connection]
host = "127.0.0.1"
port = 1
user = "u"
password = "p"

[paths]
remote_root = "/"
"#,
    )
    .unwrap();

    let output = bin()
        .args(["status", "--config"])
        .arg(&legacy_config)
        .current_dir(unrelated_cwd.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ftp connect 127.0.0.1:1"),
        "explicit migrated config was not refreshed; stderr={stderr}",
    );
    assert!(
        project.path().join(ferry::names::CONFIG_FILE).exists(),
        "legacy config was not migrated",
    );
    assert!(!legacy_config.exists(), "legacy config was not removed");
}

#[test]
fn explicit_legacy_config_wins_when_current_config_coexists() {
    let project = tempfile::tempdir().unwrap();
    let legacy_config = project.path().join(ferry::names::LEGACY_CONFIG_FILE);
    let current_config = project.path().join(ferry::names::CONFIG_FILE);
    std::fs::write(
        &legacy_config,
        r#"
[connection]
host = "127.0.0.1"
port = 1
user = "legacy"
password = "p"

[paths]
remote_root = "/"
"#,
    )
    .unwrap();
    std::fs::write(
        &current_config,
        r#"
[connection]
host = "127.0.0.1"
port = 2
user = "current"
password = "p"

[paths]
remote_root = "/"
"#,
    )
    .unwrap();

    let output = bin()
        .args(["status", "--config"])
        .arg(&legacy_config)
        .current_dir(project.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ftp connect 127.0.0.1:1"),
        "explicit legacy config lost authority; stderr={stderr}",
    );
    assert!(
        !stderr.contains("ftp connect 127.0.0.1:2"),
        "current config replaced explicit legacy config; stderr={stderr}",
    );
    assert!(legacy_config.exists(), "legacy config should remain");
    assert!(current_config.exists(), "current config should remain");
}

fn recent_state(target: &str) -> ferry::state::StateFile {
    let now = chrono::Utc::now();
    let mut state = ferry::state::StateFile::default();
    state.files.insert(
        target.into(),
        ferry::state::FileRecord {
            sha256: "known".into(),
            size: 12,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state
}

fn hook_with_target(project: &std::path::Path, target: &std::path::Path) -> std::process::Output {
    let mut child = bin()
        .args(["hook", "--cooldown", "3600"])
        .current_dir(project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    serde_json::to_writer(
        child.stdin.as_mut().unwrap(),
        &serde_json::json!({
            "tool_name": "Read",
            "tool_input": {"file_path": target},
        }),
    )
    .unwrap();
    child.stdin.as_mut().unwrap().flush().unwrap();
    drop(child.stdin.take());
    child.wait_with_output().unwrap()
}

#[test]
fn hook_migrates_descendant_local_root_state() {
    let project = tempfile::tempdir().unwrap();
    let local_root = project.path().join("mirror");
    std::fs::create_dir(&local_root).unwrap();
    let target = local_root.join("target.txt");
    std::fs::write(&target, b"local bytes\n").unwrap();
    std::fs::write(
        project.path().join(ferry::names::CONFIG_FILE),
        r#"
[connection]
host = "127.0.0.1"
port = 1
user = "u"
password = "p"

[paths]
local_root = "mirror"
remote_root = "/"
"#,
    )
    .unwrap();
    let legacy = local_root
        .join(ferry::names::LEGACY_STATE_DIR)
        .join("state.json");
    recent_state("target.txt").save(&legacy).unwrap();
    std::fs::create_dir(local_root.join(ferry::names::STATE_DIR)).unwrap();

    let output = hook_with_target(project.path(), &target);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr={stderr}");
    assert!(
        stderr.contains("within 3600s cooldown, skipping pull"),
        "stderr={stderr}"
    );
    assert!(!stderr.contains("pull failed"), "stderr={stderr}");
    assert!(
        local_root
            .join(ferry::names::STATE_DIR)
            .join("state.json")
            .exists()
    );
    assert!(!legacy.exists());
}

#[test]
fn hook_reads_legacy_state_when_migration_fails() {
    let project = tempfile::tempdir().unwrap();
    let local_root = project.path().join("mirror");
    std::fs::create_dir(&local_root).unwrap();
    let target = local_root.join("target.txt");
    std::fs::write(&target, b"local bytes\n").unwrap();
    std::fs::write(
        project.path().join(ferry::names::CONFIG_FILE),
        r#"
[connection]
host = "127.0.0.1"
port = 1
user = "u"
password = "p"

[paths]
local_root = "mirror"
remote_root = "/"
"#,
    )
    .unwrap();
    let legacy = local_root
        .join(ferry::names::LEGACY_STATE_DIR)
        .join("state.json");
    recent_state("target.txt").save(&legacy).unwrap();
    std::fs::write(
        local_root.join(ferry::names::STATE_DIR),
        b"blocks migration",
    )
    .unwrap();
    let legacy_before = std::fs::read(&legacy).unwrap();

    let output = hook_with_target(project.path(), &target);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr={stderr}");
    assert!(stderr.contains("warning"), "stderr={stderr}");
    assert!(
        stderr.contains("within 3600s cooldown, skipping pull"),
        "stderr={stderr}"
    );
    assert!(!stderr.contains("pull failed"), "stderr={stderr}");
    assert_eq!(std::fs::read(&legacy).unwrap(), legacy_before);
}
#[test]
fn select_requires_a_tty_before_config_migration_or_network_access() {
    let project = tempfile::tempdir().unwrap();
    let legacy_config = project.path().join(ferry::names::LEGACY_CONFIG_FILE);
    let legacy_state = project.path().join(ferry::names::LEGACY_STATE_DIR);
    std::fs::write(
        &legacy_config,
        r#"
[connection]
host = "127.0.0.1"
port = 1
user = "u"
password = "p"
[paths]
local_root = "."
remote_root = "/remote"
"#,
    )
    .unwrap();
    std::fs::create_dir(&legacy_state).unwrap();
    std::fs::write(legacy_state.join("marker"), b"unchanged").unwrap();
    let config_before = std::fs::read(&legacy_config).unwrap();

    let output = bin()
        .args(["sync", "--select"])
        .current_dir(project.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "error: ferry sync --select requires an interactive terminal; pass PATH directly\n"
    );
    assert!(output.stdout.is_empty());
    assert_eq!(std::fs::read(&legacy_config).unwrap(), config_before);
    assert_eq!(
        std::fs::read(legacy_state.join("marker")).unwrap(),
        b"unchanged"
    );
    assert!(!project.path().join(ferry::names::CONFIG_FILE).exists());
    assert!(!project.path().join(ferry::names::STATE_DIR).exists());
}
