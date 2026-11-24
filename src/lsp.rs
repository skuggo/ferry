mod diff;
mod document_state;

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    CodeActionOrCommand, CodeActionParams, CodeActionProviderCapability, Command,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, ExecuteCommandOptions, ExecuteCommandParams, MessageActionItem,
    MessageType, ServerCapabilities, ShowMessageParams, ShowMessageRequestParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, WorkDoneProgressOptions,
};

use crate::commands::ExecutionMode;
use crate::commands::cc::{self, FileCheckResult};
use crate::commands::file_transfer::TransferOutcome;
use crate::commands::pull::{LocalIdentity, PreparedPull as CorePreparedPull, fetch_remote_one};
use crate::commands::sync::scope::SyncScope;
use crate::commands::sync::{CommitGate, SyncEventKind, SyncOutcome};
use crate::config::Config;
use document_state::{DocumentScope, DocumentTracker, OperationGuard};
use lsp_types::request::{Request as LspRequest, ShowMessageRequest};

pub const PULL_COMMAND: &str = "ferry.pull";
pub const COMPARE_COMMAND: &str = "ferry.compare";
pub const PUSH_COMMAND: &str = "ferry.push";
pub const FORCE_PULL_COMMAND: &str = "ferry.forcePull";
pub const SYNC_FILE_COMMAND: &str = "ferry.syncFile";
pub const SYNC_FOLDER_COMMAND: &str = "ferry.syncFolder";
pub const COMPILE_COMMAND: &str = "ferry.compile";
pub const ACTION_COMMANDS: &[&str] = &[
    PULL_COMMAND,
    COMPARE_COMMAND,
    FORCE_PULL_COMMAND,
    PUSH_COMMAND,
    SYNC_FILE_COMMAND,
    SYNC_FOLDER_COMMAND,
    COMPILE_COMMAND,
];

const CODE_ACTION_METHOD: &str = "textDocument/codeAction";
const EXECUTE_COMMAND_METHOD: &str = "workspace/executeCommand";

pub trait FileOperations {
    type PreparedPull;

    fn prepare_pull(
        &mut self,
        config_path: &Path,
        rel: &str,
        force: bool,
    ) -> Result<Self::PreparedPull>;

    fn prepare_force_pull(
        &mut self,
        _config_path: &Path,
        _rel: &str,
    ) -> Result<Self::PreparedPull> {
        anyhow::bail!("Force Pull preparation is unsupported")
    }

    /// Synchronously applies a prepared pull at its final commit boundary.
    ///
    /// Implementations must not retain or otherwise let `request` escape this
    /// call. They must perform no commit mutation before exactly one successful
    /// [`PullRequest::try_claim`] call, and must invoke that authorization exactly
    /// once, immediately before the commit mutation.
    fn apply_pull(
        &mut self,
        prepared: Self::PreparedPull,
        request: PullRequest,
    ) -> Result<TransferOutcome>;

    fn push(&mut self, config_path: &Path, rel: &str, force: bool) -> Result<TransferOutcome>;

    fn compile(&mut self, config_path: &Path, rel: &str) -> Result<FileCheckResult>;

    fn compare(&mut self, _request: CompareRequest) -> Result<CompareOutcome> {
        anyhow::bail!("native diff is unavailable")
    }

    fn sync(&mut self, _request: SyncRequest) -> Result<SyncOutcome> {
        anyhow::bail!("scoped sync is unavailable")
    }

    fn shutdown_callback(&self) -> Arc<dyn Fn() -> Result<()> + Send + Sync> {
        Arc::new(|| Ok(()))
    }
}

pub struct SyncRequest {
    pub config: Config,
    pub scope: SyncScope,
    pub gate: Arc<dyn CommitGate>,
}

const PULL_AUTH_PENDING: u8 = 0;
const PULL_AUTHORIZED: u8 = 1;
const PULL_AUTH_DENIED: u8 = 2;

pub struct PullRequest {
    guard: OperationGuard,
    decision: Arc<AtomicU8>,
}

impl PullRequest {
    /// Claim authorization immediately before committing the prepared Pull.
    ///
    /// This request is one-shot. Repeated calls return `false`, as does a
    /// document edit or shutdown that cancelled the pending operation.
    pub fn try_claim(&self) -> bool {
        if self
            .decision
            .compare_exchange(
                PULL_AUTH_PENDING,
                PULL_AUTH_DENIED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        if self.guard.try_claim() {
            self.decision.store(PULL_AUTHORIZED, Ordering::Release);
            true
        } else {
            false
        }
    }
}

struct PullAuthorization {
    decision: Arc<AtomicU8>,
}

impl PullAuthorization {
    fn request(guard: OperationGuard) -> (PullRequest, Self) {
        let decision = Arc::new(AtomicU8::new(PULL_AUTH_PENDING));
        (
            PullRequest {
                guard,
                decision: Arc::clone(&decision),
            },
            Self { decision },
        )
    }

    fn decision(&self) -> u8 {
        self.decision.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompareOutcome {
    Opened,
    Cancelled,
}

pub struct CompareRequest {
    config_path: PathBuf,
    relative_path: String,
    local_path: PathBuf,
    guard: OperationGuard,
}

impl CompareRequest {
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }

    pub fn local_path(&self) -> &Path {
        &self.local_path
    }

    /// Claim authorization immediately before launching the comparison.
    ///
    /// Returns `false` when an editor change or shutdown cancelled the work.
    pub fn try_claim(&self) -> bool {
        self.guard.try_claim()
    }
}

pub struct FerryOperations {
    snapshots: diff::SharedSnapshotStore,
    launcher: Box<dyn diff::DiffLauncher>,
}

impl FerryOperations {
    pub fn new() -> Result<Self> {
        Ok(Self {
            snapshots: diff::SharedSnapshotStore::new()?,
            launcher: Box::new(diff::ZedDiffLauncher),
        })
    }
}

impl FileOperations for FerryOperations {
    type PreparedPull = CorePreparedPull;

    fn prepare_pull(
        &mut self,
        config_path: &Path,
        rel: &str,
        force: bool,
    ) -> Result<Self::PreparedPull> {
        crate::commands::pull::prepare_pull_one(config_path, rel, force)
    }

    fn prepare_force_pull(&mut self, config_path: &Path, rel: &str) -> Result<Self::PreparedPull> {
        crate::commands::pull::prepare_force_pull_one(config_path, rel)
    }

    fn apply_pull(
        &mut self,
        prepared: Self::PreparedPull,
        request: PullRequest,
    ) -> Result<TransferOutcome> {
        crate::commands::pull::apply_prepared_pull_if(prepared, ExecutionMode::Apply, || {
            request.try_claim()
        })
    }

    fn push(&mut self, config_path: &Path, rel: &str, force: bool) -> Result<TransferOutcome> {
        crate::commands::push::push_one(config_path, rel, force, ExecutionMode::Apply)
    }

    fn compile(&mut self, config_path: &Path, rel: &str) -> Result<FileCheckResult> {
        cc::check_files(config_path, &[rel.to_string()])?
            .into_iter()
            .next()
            .context("compile check returned no result")
    }

    fn compare(&mut self, request: CompareRequest) -> Result<CompareOutcome> {
        let CompareRequest {
            config_path,
            relative_path,
            local_path,
            guard,
        } = request;
        compare_file_with(
            &self.snapshots,
            self.launcher.as_mut(),
            &config_path,
            &relative_path,
            &local_path,
            guard,
            |config_path, relative_path| Ok(fetch_remote_one(config_path, relative_path)?.bytes),
        )
    }

    fn sync(&mut self, request: SyncRequest) -> Result<SyncOutcome> {
        crate::commands::sync::run_scoped_from_config(
            &request.config,
            request.scope,
            false,
            ExecutionMode::Apply,
            request.gate.as_ref(),
        )
    }

    fn shutdown_callback(&self) -> Arc<dyn Fn() -> Result<()> + Send + Sync> {
        let shutdown = self.snapshots.shutdown_handle();
        Arc::new(move || shutdown.shutdown())
    }
}

#[derive(Clone)]
struct ShutdownBoundary {
    shutdown: Arc<dyn Fn() -> Result<()> + Send + Sync>,
}

impl ShutdownBoundary {
    #[cfg(test)]
    fn noop() -> Self {
        Self {
            shutdown: Arc::new(|| Ok(())),
        }
    }

    fn shutdown(&self) -> Result<()> {
        (self.shutdown)()
    }
}

fn compare_file_with<F>(
    snapshots: &diff::SharedSnapshotStore,
    launcher: &mut dyn diff::DiffLauncher,
    config_path: &Path,
    relative_path: &str,
    local_path: &Path,
    guard: OperationGuard,
    fetch: F,
) -> Result<CompareOutcome>
where
    F: FnOnce(&Path, &str) -> Result<Vec<u8>>,
{
    let saved_local = LocalIdentity::capture(local_path)?;
    anyhow::ensure!(
        matches!(saved_local, LocalIdentity::Present(_)),
        "Compare requires a saved local file"
    );
    let remote_bytes = fetch(config_path, relative_path)?;
    let snapshot = snapshots.prepare_snapshot(local_path, &remote_bytes)?;
    if LocalIdentity::capture(local_path)? != saved_local {
        return Ok(CompareOutcome::Cancelled);
    }
    match snapshots.launch_and_retain(local_path, snapshot, guard, launcher)? {
        diff::LaunchOutcome::Launched => Ok(CompareOutcome::Opened),
        diff::LaunchOutcome::Cancelled => Ok(CompareOutcome::Cancelled),
    }
}

pub struct Server<O: FileOperations> {
    operations: O,
    shutdown: ShutdownBoundary,
}

impl<O: FileOperations> Server<O> {
    pub fn new(operations: O) -> Self {
        let shutdown = ShutdownBoundary {
            shutdown: operations.shutdown_callback(),
        };
        Self {
            operations,
            shutdown,
        }
    }

    #[cfg(test)]
    fn with_shutdown<F>(operations: O, shutdown: F) -> Self
    where
        F: Fn() -> Result<()> + Send + Sync + 'static,
    {
        Self {
            operations,
            shutdown: ShutdownBoundary {
                shutdown: Arc::new(shutdown),
            },
        }
    }

    fn shutdown_handle(&self) -> ShutdownBoundary {
        self.shutdown.clone()
    }

    #[cfg(test)]
    fn handle_notification(&mut self, connection: &Connection, notification: Notification) {
        let message = match notification.method.as_str() {
            "textDocument/didOpen" => {
                serde_json::from_value::<DidOpenTextDocumentParams>(notification.params)
                    .ok()
                    .and_then(|params| {
                        let path = uri_to_path(params.text_document.uri.as_str())?;
                        let mut tracker = DocumentTracker::default();
                        tracker
                            .open(path.clone(), &params.text_document.text)
                            .ok()?;
                        let guard = tracker.begin_clean_operation(&path).ok();
                        self.handle_file_event(&path, Event::Open, guard, None)
                    })
            }
            "textDocument/didSave" => {
                serde_json::from_value::<DidSaveTextDocumentParams>(notification.params)
                    .ok()
                    .and_then(|params| {
                        let path = uri_to_path(params.text_document.uri.as_str())?;
                        self.handle_file_event(&path, Event::Save, None, None)
                    })
            }
            _ => None,
        };
        if let Some(message) = message {
            let _ = connection.sender.send(message);
        }
    }

    #[cfg(test)]
    fn process_request(&mut self, request: Request) -> Vec<Message> {
        match request.method.as_str() {
            CODE_ACTION_METHOD => vec![Message::Response(code_actions(request))],
            EXECUTE_COMMAND_METHOD => match prepare_execute_command(request) {
                PreparedRequest::Ready { id, command } => {
                    let mut tracker = DocumentTracker::default();
                    let guard = if matches!(command.action, ActionCommand::Pull) {
                        std::fs::read_to_string(&command.absolute_path)
                            .ok()
                            .and_then(|text| {
                                tracker.open(command.absolute_path.clone(), &text).ok()?;
                                tracker
                                    .begin_clean_operation(&command.absolute_path)
                                    .ok()
                                    .map(CommandGuard::File)
                            })
                    } else if matches!(
                        command.action,
                        ActionCommand::SyncFile | ActionCommand::SyncFolder
                    ) {
                        sync_document_scope_at_ack(&command)
                            .ok()
                            .and_then(|scope| tracker.begin_clean_scope(scope).ok())
                            .map(|guard| CommandGuard::Scope(Arc::new(guard)))
                    } else {
                        None
                    };
                    vec![
                        Message::Response(Response::new_ok(id, ())),
                        self.process_command(command, guard, None),
                    ]
                }
                PreparedRequest::Immediate(messages) => messages,
            },
            _ => vec![Message::Response(Response::new_err(
                request.id,
                ErrorCode::MethodNotFound as i32,
                "unsupported request".to_string(),
            ))],
        }
    }

    fn process_command(
        &mut self,
        command: PreparedCommand,
        guard: Option<CommandGuard>,
        running: Option<&AtomicBool>,
    ) -> Message {
        let resolved = match crate::project::resolve_file(&command.absolute_path, true) {
            Ok(Some(resolved)) => resolved,
            Ok(None) => {
                return warning_message(format!(
                    "ferry: {}: file is no longer in a Ferry project",
                    command.initial_relative_path
                ));
            }
            Err(error) => {
                return warning_message(format!(
                    "ferry: {}: {}; run a Ferry task for details",
                    command.initial_relative_path,
                    safe_error_summary(&error)
                ));
            }
        };
        match command.action {
            ActionCommand::Pull => {
                let Some(CommandGuard::File(guard)) = guard else {
                    return save_first_warning(&resolved.relative_path);
                };
                match self.pull_with_guard(
                    &resolved.config_path,
                    &resolved.relative_path,
                    guard,
                    running,
                ) {
                    GuardedPullResult::Completed(result) => {
                        transfer_feedback(&resolved.relative_path, result)
                    }
                    GuardedPullResult::Cancelled => save_first_warning(&resolved.relative_path),
                }
            }
            ActionCommand::ForcePull => warning_message(format!(
                "ferry: {}: Force Pull requires native confirmation",
                resolved.relative_path
            )),
            ActionCommand::Push => transfer_feedback(
                &resolved.relative_path,
                self.operations
                    .push(&resolved.config_path, &resolved.relative_path, false),
            ),
            ActionCommand::Compile => compile_feedback(
                &resolved.relative_path,
                self.operations
                    .compile(&resolved.config_path, &resolved.relative_path),
            ),
            ActionCommand::Compare => {
                let Some(CommandGuard::File(guard)) = guard else {
                    return save_first_warning(&resolved.relative_path);
                };
                match self.operations.compare(CompareRequest {
                    config_path: resolved.config_path,
                    relative_path: resolved.relative_path.clone(),
                    local_path: command.absolute_path,
                    guard,
                }) {
                    Ok(CompareOutcome::Opened) => info_message(format!(
                        "ferry: {}: opened native diff",
                        resolved.relative_path
                    )),
                    Ok(CompareOutcome::Cancelled) => save_first_warning(&resolved.relative_path),
                    Err(error) => warning_message(format!(
                        "ferry: {}: {}; run a Ferry task for details",
                        resolved.relative_path,
                        safe_error_summary(&error)
                    )),
                }
            }
            ActionCommand::SyncFile | ActionCommand::SyncFolder => {
                let Some(CommandGuard::Scope(gate)) = guard else {
                    return save_scope_warning();
                };
                let scope = sync_scope_for(command.action, &resolved.relative_path)
                    .expect("sync action must derive a scope");
                sync_feedback(self.operations.sync(SyncRequest {
                    config: resolved.config,
                    scope,
                    gate,
                }))
            }
        }
    }

    fn handle_file_event(
        &mut self,
        path: &Path,
        event: Event,
        guard: Option<OperationGuard>,
        running: Option<&AtomicBool>,
    ) -> Option<Message> {
        let resolved = match crate::project::resolve_file(path, true) {
            Ok(Some(resolved)) => resolved,
            Ok(None) => return None,
            Err(error) => {
                return Some(warning_message(format!(
                    "ferry: {}; run a Ferry task for details",
                    safe_error_summary(&error)
                )));
            }
        };

        let enabled = match event {
            Event::Open => resolved.config.editor.pull_on_open,
            Event::Save => resolved.config.editor.push_on_save,
        };
        if !enabled {
            return None;
        }

        let result = match event {
            Event::Open => {
                let Some(guard) = guard else {
                    return Some(save_first_warning(&resolved.relative_path));
                };
                match self.pull_with_guard(
                    &resolved.config_path,
                    &resolved.relative_path,
                    guard,
                    running,
                ) {
                    GuardedPullResult::Completed(result) => result,
                    GuardedPullResult::Cancelled => {
                        return Some(save_first_warning(&resolved.relative_path));
                    }
                }
            }
            Event::Save => {
                self.operations
                    .push(&resolved.config_path, &resolved.relative_path, false)
            }
        };
        if let Err(error) = result {
            return Some(warning_message(format!(
                "ferry: {}: {}; run a Ferry task for details",
                resolved.relative_path,
                safe_error_summary(&error)
            )));
        }
        None
    }

    fn pull_with_guard(
        &mut self,
        config_path: &Path,
        relative_path: &str,
        guard: OperationGuard,
        running: Option<&AtomicBool>,
    ) -> GuardedPullResult {
        let prepared = match self
            .operations
            .prepare_pull(config_path, relative_path, false)
        {
            Ok(prepared) => prepared,
            Err(error) => return GuardedPullResult::Completed(Err(error)),
        };
        if running.is_some_and(|running| !running.load(Ordering::Acquire)) {
            return GuardedPullResult::Cancelled;
        }
        let (request, authorization) = PullAuthorization::request(guard);
        let result = self.operations.apply_pull(prepared, request);
        match authorization.decision() {
            PULL_AUTH_DENIED => GuardedPullResult::Cancelled,
            PULL_AUTHORIZED => GuardedPullResult::Completed(result),
            PULL_AUTH_PENDING => match result {
                Err(error) => GuardedPullResult::Completed(Err(error)),
                Ok(_) => GuardedPullResult::Completed(Err(anyhow::anyhow!(
                    "Pull apply completed without final authorization"
                ))),
            },
            _ => unreachable!("invalid Pull authorization state"),
        }
    }

    fn prepare_force_pull(
        &mut self,
        preparation: ForcePullPreparation,
    ) -> WorkerEvent<O::PreparedPull> {
        let ForcePullPreparation {
            operation_id,
            absolute_path,
            initial_relative_path,
            guard,
        } = preparation;
        let resolved = match crate::project::resolve_file(&absolute_path, true) {
            Ok(Some(resolved)) => resolved,
            Ok(None) => {
                return WorkerEvent::ForcePullPreparationFailed {
                    operation_id,
                    absolute_path,
                    relative_path: initial_relative_path,
                    error: anyhow::anyhow!("file is no longer in a Ferry project"),
                };
            }
            Err(error) => {
                return WorkerEvent::ForcePullPreparationFailed {
                    operation_id,
                    absolute_path,
                    relative_path: initial_relative_path,
                    error,
                };
            }
        };
        match self
            .operations
            .prepare_force_pull(&resolved.config_path, &resolved.relative_path)
        {
            Ok(prepared) => WorkerEvent::ForcePullReady(PendingForcePull {
                operation_id,
                absolute_path,
                relative_path: resolved.relative_path,
                prepared,
                guard,
            }),
            Err(error) => WorkerEvent::ForcePullPreparationFailed {
                operation_id,
                absolute_path,
                relative_path: resolved.relative_path,
                error,
            },
        }
    }

    fn apply_force_pull(
        &mut self,
        pending: PendingForcePull<O::PreparedPull>,
    ) -> WorkerEvent<O::PreparedPull> {
        let PendingForcePull {
            operation_id,
            absolute_path,
            relative_path,
            prepared,
            guard,
        } = pending;
        let (request, authorization) = PullAuthorization::request(guard);
        let result = self.operations.apply_pull(prepared, request);
        let result = match authorization.decision() {
            PULL_AUTH_DENIED => GuardedPullResult::Cancelled,
            PULL_AUTHORIZED => GuardedPullResult::Completed(result),
            PULL_AUTH_PENDING => match result {
                Err(error) => GuardedPullResult::Completed(Err(error)),
                Ok(_) => GuardedPullResult::Completed(Err(anyhow::anyhow!(
                    "Pull apply completed without final authorization"
                ))),
            },
            _ => unreachable!("invalid Pull authorization state"),
        };
        WorkerEvent::ForcePullApplied {
            operation_id,
            absolute_path,
            relative_path,
            result,
        }
    }
}

enum GuardedPullResult {
    Completed(Result<TransferOutcome>),
    Cancelled,
}

fn code_actions(request: Request) -> Response {
    let id = request.id;
    let params = match serde_json::from_value::<CodeActionParams>(request.params) {
        Ok(params) => params,
        Err(_) => return invalid_params(id, "invalid code-action parameters"),
    };
    let uri = params.text_document.uri;
    let actions = uri_to_path(uri.as_str())
        .and_then(|path| crate::project::resolve_file(&path, true).ok().flatten())
        .map(|_| {
            [
                ("Ferry: Pull", PULL_COMMAND),
                ("Ferry: Compare with Remote", COMPARE_COMMAND),
                ("Ferry: Force Pull (overwrite local)", FORCE_PULL_COMMAND),
                ("Ferry: Push", PUSH_COMMAND),
                ("Ferry: Sync Current File", SYNC_FILE_COMMAND),
                ("Ferry: Sync Current Folder", SYNC_FOLDER_COMMAND),
                ("Ferry: Compile-check", COMPILE_COMMAND),
            ]
            .into_iter()
            .map(|(title, command)| {
                CodeActionOrCommand::Command(Command {
                    title: title.to_string(),
                    command: command.to_string(),
                    arguments: Some(vec![serde_json::Value::String(uri.as_str().to_string())]),
                })
            })
            .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Response::new_ok(id, actions)
}

#[derive(Clone, Copy)]
enum ActionCommand {
    Pull,
    Compare,
    ForcePull,
    Push,
    SyncFile,
    SyncFolder,
    Compile,
}

enum CommandGuard {
    File(OperationGuard),
    Scope(Arc<dyn CommitGate>),
}

struct PreparedCommand {
    action: ActionCommand,
    absolute_path: PathBuf,
    initial_relative_path: String,
}

enum PreparedRequest {
    Ready {
        id: lsp_server::RequestId,
        command: PreparedCommand,
    },
    Immediate(Vec<Message>),
}

fn prepare_execute_command(request: Request) -> PreparedRequest {
    let id = request.id;
    let params = match serde_json::from_value::<ExecuteCommandParams>(request.params) {
        Ok(params) => params,
        Err(_) => {
            return PreparedRequest::Immediate(vec![Message::Response(invalid_params(
                id,
                "invalid execute-command parameters",
            ))]);
        }
    };
    let action = match params.command.as_str() {
        PULL_COMMAND => ActionCommand::Pull,
        COMPARE_COMMAND => ActionCommand::Compare,
        FORCE_PULL_COMMAND => ActionCommand::ForcePull,
        PUSH_COMMAND => ActionCommand::Push,
        SYNC_FILE_COMMAND => ActionCommand::SyncFile,
        SYNC_FOLDER_COMMAND => ActionCommand::SyncFolder,
        COMPILE_COMMAND => ActionCommand::Compile,
        _ => {
            return PreparedRequest::Immediate(vec![Message::Response(invalid_params(
                id,
                "invalid Ferry command or arguments",
            ))]);
        }
    };
    if params.arguments.len() != 1 {
        return PreparedRequest::Immediate(vec![Message::Response(invalid_params(
            id,
            "invalid Ferry command or arguments",
        ))]);
    }
    let Some(uri) = params.arguments[0].as_str() else {
        return PreparedRequest::Immediate(vec![Message::Response(invalid_params(
            id,
            "expected one file URI argument",
        ))]);
    };
    let Some(path) = uri_to_path(uri) else {
        return PreparedRequest::Immediate(vec![Message::Response(invalid_params(
            id,
            "expected one file URI argument",
        ))]);
    };
    let resolved = match crate::project::resolve_file(&path, true) {
        Ok(Some(resolved)) => resolved,
        Ok(None) => {
            return PreparedRequest::Immediate(vec![Message::Response(invalid_params(
                id,
                "file is outside a Ferry project",
            ))]);
        }
        Err(error) => {
            return PreparedRequest::Immediate(operation_response(
                id,
                warning_message(format!(
                    "ferry: {}; run a Ferry task for details",
                    safe_error_summary(&error)
                )),
            ));
        }
    };
    PreparedRequest::Ready {
        id,
        command: PreparedCommand {
            action,
            absolute_path: path,
            initial_relative_path: resolved.relative_path,
        },
    }
}

fn sync_scope_for(action: ActionCommand, relative_path: &str) -> Option<SyncScope> {
    match action {
        ActionCommand::SyncFile => Some(SyncScope::Path(relative_path.to_string())),
        ActionCommand::SyncFolder => {
            let parent = Path::new(relative_path).parent()?;
            if parent.as_os_str().is_empty() {
                Some(SyncScope::RootDirectory)
            } else {
                Some(SyncScope::Path(parent.to_string_lossy().into_owned()))
            }
        }
        _ => None,
    }
}

fn sync_document_scope_at_ack(
    command: &PreparedCommand,
) -> std::result::Result<DocumentScope, Message> {
    let resolved = crate::project::resolve_file(&command.absolute_path, true)
        .map_err(|error| {
            warning_message(format!(
                "ferry: {}; run a Ferry task for details",
                safe_error_summary(&error)
            ))
        })?
        .ok_or_else(|| {
            warning_message("ferry: file is no longer in a Ferry project".to_string())
        })?;
    let scope = sync_scope_for(command.action, &resolved.relative_path)
        .expect("only sync commands request a document scope");
    let document_scope = match scope {
        SyncScope::Path(_) if matches!(command.action, ActionCommand::SyncFile) => {
            let path = command.absolute_path.canonicalize().map_err(|_| {
                warning_message("ferry: operation failed; run a Ferry task for details".to_string())
            })?;
            DocumentScope::Exact(path)
        }
        SyncScope::RootDirectory => {
            let root = resolved
                .config
                .paths
                .local_root
                .canonicalize()
                .map_err(|_| {
                    warning_message(
                        "ferry: configuration error; run a Ferry task for details".to_string(),
                    )
                })?;
            DocumentScope::Directory(root)
        }
        SyncScope::Path(_) => {
            let parent = command.absolute_path.parent().ok_or_else(|| {
                warning_message("ferry: operation failed; run a Ferry task for details".to_string())
            })?;
            let parent = parent.canonicalize().map_err(|_| {
                warning_message("ferry: operation failed; run a Ferry task for details".to_string())
            })?;
            DocumentScope::Directory(parent)
        }
        SyncScope::LegacyProject => unreachable!("editor actions never use legacy scope"),
    };
    Ok(document_scope)
}

#[derive(Clone, Copy)]
enum Event {
    Open,
    Save,
}

pub fn capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                will_save: None,
                will_save_wait_until: None,
                save: Some(TextDocumentSyncSaveOptions::Supported(true)),
            },
        )),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: ACTION_COMMANDS
                .iter()
                .map(|command| (*command).to_string())
                .collect(),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        ..ServerCapabilities::default()
    }
}

struct ForcePullPreparation {
    operation_id: u64,
    absolute_path: PathBuf,
    initial_relative_path: String,
    guard: OperationGuard,
}

struct PendingForcePull<P> {
    operation_id: u64,
    absolute_path: PathBuf,
    relative_path: String,
    prepared: P,
    guard: OperationGuard,
}

enum Work<P> {
    Open {
        path: PathBuf,
        guard: Option<OperationGuard>,
    },
    Save {
        path: PathBuf,
    },
    Command {
        command: PreparedCommand,
        guard: Option<CommandGuard>,
    },
    ForcePullPrepare(ForcePullPreparation),
    ForcePullApply(PendingForcePull<P>),
}

enum WorkerEvent<P> {
    Message(Message),
    ForcePullReady(PendingForcePull<P>),
    ForcePullPreparationFailed {
        operation_id: u64,
        absolute_path: PathBuf,
        relative_path: String,
        error: anyhow::Error,
    },
    ForcePullApplied {
        operation_id: u64,
        absolute_path: PathBuf,
        relative_path: String,
        result: GuardedPullResult,
    },
}

#[derive(Clone)]
struct ForcePullSlot {
    operation_id: u64,
    guard: OperationGuard,
    relative_path: String,
    request_id: Option<RequestId>,
}

impl ForcePullSlot {
    fn cancel(&self) {
        self.guard.cancel();
    }
}

struct PendingConfirmation<P> {
    pending: PendingForcePull<P>,
}

#[derive(Debug)]
struct ShutdownFailures {
    protocol_error: anyhow::Error,
    cleanup_error: anyhow::Error,
}

impl fmt::Display for ShutdownFailures {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Ferry shutdown cleanup also failed: {:#}",
            self.cleanup_error
        )
    }
}

impl Error for ShutdownFailures {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.protocol_error.as_ref())
    }
}

struct Coordinator<P> {
    documents: DocumentTracker,
    running: Arc<AtomicBool>,
    shutdown: ShutdownBoundary,
    shutdown_complete: bool,
    cleanup_error: Option<anyhow::Error>,
    next_force_operation_id: u64,
    next_force_request_id: u64,
    force_slots: HashMap<PathBuf, ForcePullSlot>,
    pending_confirmations: HashMap<RequestId, PendingConfirmation<P>>,
}

impl<P> Coordinator<P> {
    fn new(running: Arc<AtomicBool>, shutdown: ShutdownBoundary) -> Self {
        Self {
            documents: DocumentTracker::default(),
            running,
            shutdown,
            shutdown_complete: false,
            cleanup_error: None,
            next_force_operation_id: 1,
            next_force_request_id: 1,
            force_slots: HashMap::new(),
            pending_confirmations: HashMap::new(),
        }
    }

    fn begin_force_pull(
        &mut self,
        command: PreparedCommand,
    ) -> Result<ForcePullPreparation, Message> {
        let absolute_path = command.absolute_path;
        let guard = self
            .documents
            .begin_clean_operation(&absolute_path)
            .map_err(|_| save_first_warning(&command.initial_relative_path))?;
        if let Some(previous) = self.force_slots.remove(&absolute_path) {
            previous.cancel();
            if let Some(request_id) = previous.request_id {
                self.pending_confirmations.remove(&request_id);
            }
        }
        let operation_id = self.next_force_operation_id;
        self.next_force_operation_id = self
            .next_force_operation_id
            .checked_add(1)
            .expect("Force Pull operation id exhausted");
        self.force_slots.insert(
            absolute_path.clone(),
            ForcePullSlot {
                operation_id,
                guard: guard.clone(),
                relative_path: command.initial_relative_path.clone(),
                request_id: None,
            },
        );
        Ok(ForcePullPreparation {
            operation_id,
            absolute_path,
            initial_relative_path: command.initial_relative_path,
            guard,
        })
    }

    fn enqueue_force_pull_preparation(
        &mut self,
        work_sender: &mpsc::Sender<Work<P>>,
        preparation: ForcePullPreparation,
    ) -> Result<(), String> {
        let operation_id = preparation.operation_id;
        let absolute_path = preparation.absolute_path.clone();
        let relative_path = preparation.initial_relative_path.clone();
        if work_sender
            .send(Work::ForcePullPrepare(preparation))
            .is_err()
        {
            self.remove_current_force_slot(&absolute_path, operation_id);
            return Err(relative_path);
        }
        Ok(())
    }

    fn current_force_slot(&self, path: &Path, operation_id: u64) -> Option<&ForcePullSlot> {
        self.force_slots
            .get(path)
            .filter(|slot| slot.operation_id == operation_id)
    }

    fn remove_current_force_slot(&mut self, path: &Path, operation_id: u64) {
        if self.current_force_slot(path, operation_id).is_some()
            && let Some(slot) = self.force_slots.remove(path)
        {
            slot.cancel();
            if let Some(request_id) = slot.request_id {
                self.pending_confirmations.remove(&request_id);
            }
        }
    }

    fn cancel_force_for_path(&mut self, path: &Path) -> Option<String> {
        if let Some(slot) = self.force_slots.remove(path) {
            slot.cancel();
            if let Some(request_id) = slot.request_id {
                self.pending_confirmations.remove(&request_id);
            }
            return Some(slot.relative_path);
        }
        None
    }

    fn allocate_force_request_id(&mut self) -> RequestId {
        let counter = self.next_force_request_id;
        self.next_force_request_id = self
            .next_force_request_id
            .checked_add(1)
            .expect("Force Pull request id exhausted");
        RequestId::from(format!("ferry-force-pull-{counter}"))
    }

    fn begin_shutdown(&mut self) {
        for slot in self.force_slots.values() {
            slot.cancel();
        }
        self.force_slots.clear();
        self.pending_confirmations.clear();
        self.documents.cancel_all();
        self.running.store(false, Ordering::Release);
        if self.shutdown_complete {
            return;
        }
        match self.shutdown.shutdown() {
            Ok(()) => {
                self.shutdown_complete = true;
                self.cleanup_error = None;
            }
            Err(error) => self.cleanup_error = Some(error),
        }
    }

    fn finish(mut self, result: Result<()>) -> Result<()> {
        self.begin_shutdown();
        if self.shutdown_complete {
            return result;
        }
        match (result, self.cleanup_error.take()) {
            (Err(protocol_error), Some(cleanup_error)) => {
                Err(anyhow::Error::new(ShutdownFailures {
                    protocol_error,
                    cleanup_error,
                }))
            }
            (Err(protocol_error), None) => Err(protocol_error),
            (Ok(()), Some(cleanup_error)) => Err(cleanup_error),
            (Ok(()), None) => Err(anyhow::anyhow!("Ferry shutdown cleanup did not complete")),
        }
    }
}

pub fn main_loop<O: FileOperations + Send + 'static>(
    connection: Connection,
    mut server: Server<O>,
) -> Result<()>
where
    O::PreparedPull: Send + 'static,
{
    let (work_sender, work_receiver) = mpsc::channel::<Work<O::PreparedPull>>();
    let (outbound_sender, outbound_receiver) = mpsc::channel::<WorkerEvent<O::PreparedPull>>();
    let running = Arc::new(AtomicBool::new(true));
    let worker_running = Arc::clone(&running);
    let shutdown = server.shutdown_handle();

    // Deliberately detached: a transport can block indefinitely. The worker owns no
    // clone of the LSP sender, so it cannot keep the stdio writer alive after the
    // protocol loop exits. Shutdown instead makes queued work inert immediately.
    let _worker = thread::spawn(move || {
        while worker_running.load(Ordering::Acquire) {
            let Ok(work) = work_receiver.recv() else {
                return;
            };
            if !worker_running.load(Ordering::Acquire) {
                return;
            }
            let event = match work {
                Work::Open { path, guard } => server
                    .handle_file_event(&path, Event::Open, guard, Some(&worker_running))
                    .map(WorkerEvent::Message),
                Work::Save { path } => server
                    .handle_file_event(&path, Event::Save, None, Some(&worker_running))
                    .map(WorkerEvent::Message),
                Work::Command { command, guard } => Some(WorkerEvent::Message(
                    server.process_command(command, guard, Some(&worker_running)),
                )),
                Work::ForcePullPrepare(preparation) => Some(server.prepare_force_pull(preparation)),
                Work::ForcePullApply(pending) => Some(server.apply_force_pull(pending)),
            };
            if !worker_running.load(Ordering::Acquire) {
                return;
            }
            if let Some(event) = event
                && outbound_sender.send(event).is_err()
            {
                return;
            }
        }
    });

    let result = protocol_loop(
        &connection,
        &work_sender,
        &outbound_receiver,
        Arc::clone(&running),
        shutdown,
    );
    drop(work_sender);
    result
}

fn protocol_loop<P: Send + 'static>(
    connection: &Connection,
    work_sender: &mpsc::Sender<Work<P>>,
    outbound_receiver: &mpsc::Receiver<WorkerEvent<P>>,
    running: Arc<AtomicBool>,
    shutdown: ShutdownBoundary,
) -> Result<()> {
    let mut coordinator = Coordinator::new(running, shutdown);
    let result = protocol_loop_inner(connection, work_sender, outbound_receiver, &mut coordinator);
    coordinator.finish(result)
}

fn protocol_loop_inner<P: Send + 'static>(
    connection: &Connection,
    work_sender: &mpsc::Sender<Work<P>>,
    outbound_receiver: &mpsc::Receiver<WorkerEvent<P>>,
    coordinator: &mut Coordinator<P>,
) -> Result<()> {
    loop {
        while let Ok(event) = outbound_receiver.try_recv() {
            if handle_worker_event(connection, coordinator, event) {
                return Ok(());
            }
        }

        match connection.receiver.try_recv() {
            Ok(Message::Request(request)) => {
                if handle_request(connection, work_sender, coordinator, request)? {
                    return Ok(());
                }
            }
            Ok(Message::Notification(notification)) => {
                if handle_notification(connection, work_sender, coordinator, notification) {
                    return Ok(());
                }
            }
            Ok(Message::Response(response)) => {
                if handle_response(connection, work_sender, coordinator, response) {
                    return Ok(());
                }
            }
            Err(error) if error.is_empty() => thread::sleep(Duration::from_millis(5)),
            Err(_) => return Ok(()),
        }
    }
}

fn handle_worker_event<P: Send + 'static>(
    connection: &Connection,
    coordinator: &mut Coordinator<P>,
    event: WorkerEvent<P>,
) -> bool {
    match event {
        WorkerEvent::Message(message) => connection.sender.send(message).is_err(),
        WorkerEvent::ForcePullReady(pending) => {
            let operation_id = pending.operation_id;
            let absolute_path = pending.absolute_path.clone();
            if coordinator
                .current_force_slot(&absolute_path, operation_id)
                .is_none()
            {
                return false;
            }
            let request_id = coordinator.allocate_force_request_id();
            let relative_path = pending.relative_path.clone();
            coordinator
                .force_slots
                .get_mut(&absolute_path)
                .expect("current Force Pull slot disappeared")
                .request_id = Some(request_id.clone());
            coordinator
                .pending_confirmations
                .insert(request_id.clone(), PendingConfirmation { pending });
            let request = Request::new(
                request_id,
                ShowMessageRequest::METHOD.to_string(),
                ShowMessageRequestParams {
                    typ: MessageType::WARNING,
                    message: format!(
                        "ferry: {}: Force Pull will overwrite the saved local file. Continue?",
                        relative_path
                    ),
                    actions: Some(vec![
                        MessageActionItem {
                            title: "Overwrite local file".to_string(),
                            properties: HashMap::new(),
                        },
                        MessageActionItem {
                            title: "Cancel".to_string(),
                            properties: HashMap::new(),
                        },
                    ]),
                },
            );
            let failed = connection.sender.send(Message::Request(request)).is_err();
            if failed {
                coordinator.remove_current_force_slot(&absolute_path, operation_id);
            }
            failed
        }
        WorkerEvent::ForcePullPreparationFailed {
            operation_id,
            absolute_path,
            relative_path,
            error,
        } => {
            if coordinator
                .current_force_slot(&absolute_path, operation_id)
                .is_none()
            {
                return false;
            }
            let feedback = warning_message(format!(
                "ferry: {relative_path}: {}; run a Ferry task for details",
                safe_error_summary(&error)
            ));
            coordinator.remove_current_force_slot(&absolute_path, operation_id);
            connection.sender.send(feedback).is_err()
        }
        WorkerEvent::ForcePullApplied {
            operation_id,
            absolute_path,
            relative_path,
            result,
        } => {
            if coordinator
                .current_force_slot(&absolute_path, operation_id)
                .is_none()
            {
                return false;
            }
            let feedback = match result {
                GuardedPullResult::Completed(result) => transfer_feedback(&relative_path, result),
                GuardedPullResult::Cancelled => save_first_warning(&relative_path),
            };
            coordinator.remove_current_force_slot(&absolute_path, operation_id);
            connection.sender.send(feedback).is_err()
        }
    }
}

fn handle_response<P: Send + 'static>(
    connection: &Connection,
    work_sender: &mpsc::Sender<Work<P>>,
    coordinator: &mut Coordinator<P>,
    response: Response,
) -> bool {
    let Some(confirmation) = coordinator.pending_confirmations.remove(&response.id) else {
        return false;
    };
    let pending = confirmation.pending;
    let operation_id = pending.operation_id;
    let absolute_path = pending.absolute_path.clone();
    if coordinator
        .current_force_slot(&absolute_path, operation_id)
        .is_none()
    {
        return false;
    }
    if !pending.guard.is_pending() {
        coordinator.remove_current_force_slot(&absolute_path, operation_id);
        return false;
    }
    if let Some(slot) = coordinator.force_slots.get_mut(&absolute_path) {
        slot.request_id = None;
    }
    let affirmative = response
        .response_result
        .ok()
        .and_then(|value| serde_json::from_value::<Option<MessageActionItem>>(value).ok())
        .flatten()
        .is_some_and(|action| action.title == "Overwrite local file");
    if !affirmative {
        coordinator.remove_current_force_slot(&absolute_path, operation_id);
        return false;
    }
    let still_resolved = crate::project::resolve_file(&absolute_path, true)
        .ok()
        .flatten()
        .is_some_and(|resolved| resolved.relative_path == pending.relative_path);
    if !still_resolved {
        let feedback = warning_message(format!(
            "ferry: {}: file is no longer in the same Ferry project",
            pending.relative_path
        ));
        coordinator.remove_current_force_slot(&absolute_path, operation_id);
        return connection.sender.send(feedback).is_err();
    }
    let relative_path = pending.relative_path.clone();
    if work_sender.send(Work::ForcePullApply(pending)).is_err() {
        let feedback = warning_message(format!(
            "ferry: {relative_path}: operation worker unavailable"
        ));
        coordinator.remove_current_force_slot(&absolute_path, operation_id);
        return connection.sender.send(feedback).is_err();
    }
    false
}

fn handle_notification<P>(
    connection: &Connection,
    work_sender: &mpsc::Sender<Work<P>>,
    coordinator: &mut Coordinator<P>,
    notification: Notification,
) -> bool {
    match notification.method.as_str() {
        "textDocument/didOpen" => {
            let Ok(params) =
                serde_json::from_value::<DidOpenTextDocumentParams>(notification.params)
            else {
                return false;
            };
            let Some(path) = uri_to_path(params.text_document.uri.as_str()) else {
                return false;
            };
            let _ = coordinator.cancel_force_for_path(&path);
            if coordinator
                .documents
                .open(path.clone(), &params.text_document.text)
                .is_err()
            {
                return false;
            }
            let guard = coordinator.documents.begin_clean_operation(&path).ok();
            work_sender.send(Work::Open { path, guard }).is_err()
        }
        "textDocument/didChange" => {
            if let Ok(params) =
                serde_json::from_value::<DidChangeTextDocumentParams>(notification.params)
                && let Some(path) = uri_to_path(params.text_document.uri.as_str())
            {
                coordinator.documents.change(&path);
                if let Some(relative_path) = coordinator.cancel_force_for_path(&path) {
                    return connection
                        .sender
                        .send(save_first_warning(&relative_path))
                        .is_err();
                }
            }
            false
        }
        "textDocument/didSave" => {
            let Ok(params) =
                serde_json::from_value::<DidSaveTextDocumentParams>(notification.params)
            else {
                return false;
            };
            let Some(path) = uri_to_path(params.text_document.uri.as_str()) else {
                return false;
            };
            coordinator.documents.save(&path);
            let _ = coordinator.cancel_force_for_path(&path);
            work_sender.send(Work::Save { path }).is_err()
        }
        "textDocument/didClose" => {
            if let Ok(params) =
                serde_json::from_value::<DidCloseTextDocumentParams>(notification.params)
                && let Some(path) = uri_to_path(params.text_document.uri.as_str())
            {
                coordinator.documents.close(&path);
                let _ = coordinator.cancel_force_for_path(&path);
            }
            false
        }
        _ => false,
    }
}

fn handle_request<P>(
    connection: &Connection,
    work_sender: &mpsc::Sender<Work<P>>,
    coordinator: &mut Coordinator<P>,
    request: Request,
) -> Result<bool> {
    if request.method == "shutdown" {
        coordinator.begin_shutdown();
        return Ok(connection.handle_shutdown(&request)?);
    }

    if request.method == CODE_ACTION_METHOD {
        let response = code_actions(request);
        let _ = connection.sender.send(Message::Response(response));
        return Ok(false);
    }

    if request.method == EXECUTE_COMMAND_METHOD {
        match prepare_execute_command(request) {
            PreparedRequest::Ready { id, command } => {
                if matches!(command.action, ActionCommand::ForcePull) {
                    let preparation = match coordinator.begin_force_pull(command) {
                        Ok(preparation) => preparation,
                        Err(feedback) => {
                            for message in operation_response(id, feedback) {
                                if connection.sender.send(message).is_err() {
                                    return Ok(true);
                                }
                            }
                            return Ok(false);
                        }
                    };
                    if connection
                        .sender
                        .send(Message::Response(Response::new_ok(id, ())))
                        .is_err()
                    {
                        return Ok(true);
                    }
                    if let Err(relative_path) =
                        coordinator.enqueue_force_pull_preparation(work_sender, preparation)
                    {
                        return Ok(connection
                            .sender
                            .send(warning_message(format!(
                                "ferry: {relative_path}: operation worker unavailable"
                            )))
                            .is_err());
                    }
                    return Ok(false);
                }

                let guard =
                    if matches!(command.action, ActionCommand::Pull | ActionCommand::Compare) {
                        match coordinator
                            .documents
                            .begin_clean_operation(&command.absolute_path)
                        {
                            Ok(guard) => Some(CommandGuard::File(guard)),
                            Err(_) => {
                                for message in operation_response(
                                    id,
                                    save_first_warning(&command.initial_relative_path),
                                ) {
                                    if connection.sender.send(message).is_err() {
                                        return Ok(true);
                                    }
                                }
                                return Ok(false);
                            }
                        }
                    } else if matches!(
                        command.action,
                        ActionCommand::SyncFile | ActionCommand::SyncFolder
                    ) {
                        let scope = match sync_document_scope_at_ack(&command) {
                            Ok(scope) => scope,
                            Err(feedback) => {
                                for message in operation_response(id, feedback) {
                                    if connection.sender.send(message).is_err() {
                                        return Ok(true);
                                    }
                                }
                                return Ok(false);
                            }
                        };
                        match coordinator.documents.begin_clean_scope(scope) {
                            Ok(guard) => Some(CommandGuard::Scope(Arc::new(guard))),
                            Err(_) => {
                                for message in operation_response(id, save_scope_warning()) {
                                    if connection.sender.send(message).is_err() {
                                        return Ok(true);
                                    }
                                }
                                return Ok(false);
                            }
                        }
                    } else {
                        None
                    };
                let response = if work_sender.send(Work::Command { command, guard }).is_ok() {
                    Response::new_ok(id, ())
                } else {
                    Response::new_err(
                        id,
                        ErrorCode::InternalError as i32,
                        "Ferry operation worker unavailable".to_string(),
                    )
                };
                return Ok(connection.sender.send(Message::Response(response)).is_err());
            }
            PreparedRequest::Immediate(messages) => {
                for message in messages {
                    if connection.sender.send(message).is_err() {
                        return Ok(true);
                    }
                }
                return Ok(false);
            }
        }
    }

    let response = Response::new_err(
        request.id,
        ErrorCode::MethodNotFound as i32,
        "unsupported request".to_string(),
    );
    let _ = connection.sender.send(Message::Response(response));
    Ok(false)
}

fn invalid_params(id: lsp_server::RequestId, message: &str) -> Response {
    Response::new_err(id, ErrorCode::InvalidParams as i32, message.to_string())
}

fn operation_response(id: lsp_server::RequestId, feedback: Message) -> Vec<Message> {
    vec![Message::Response(Response::new_ok(id, ())), feedback]
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let stripped = uri.strip_prefix("file://")?;
    let path_part = if let Some(rest) = stripped.strip_prefix("localhost") {
        rest
    } else {
        stripped
    };
    if !path_part.starts_with('/') {
        return None;
    }
    let decoded = percent_encoding::percent_decode_str(path_part)
        .decode_utf8()
        .ok()?;
    let absolute_path = PathBuf::from(decoded.as_ref());
    Some(std::fs::canonicalize(&absolute_path).unwrap_or(absolute_path))
}

fn warning_message(message: String) -> Message {
    let notification = Notification::new(
        "window/showMessage".to_string(),
        ShowMessageParams {
            typ: MessageType::WARNING,
            message,
        },
    );
    Message::Notification(notification)
}

fn save_first_warning(relative_path: &str) -> Message {
    warning_message(format!("ferry: {relative_path}: save the file and retry"))
}

fn save_scope_warning() -> Message {
    warning_message(
        "ferry: folder changed in Zed; save all files in this folder and retry".to_string(),
    )
}

fn info_message(message: String) -> Message {
    let notification = Notification::new(
        "window/showMessage".to_string(),
        ShowMessageParams {
            typ: MessageType::INFO,
            message,
        },
    );
    Message::Notification(notification)
}

fn scope_cancellation_feedback(outcome: &crate::commands::sync::SyncOutcome) -> Option<Message> {
    outcome.cancelled.then(|| {
        warning_message("ferry: folder changed in Zed; save all files and retry".to_string())
    })
}

fn sync_feedback(result: Result<SyncOutcome>) -> Message {
    match result {
        Err(error) => warning_message(format!(
            "ferry: {}; run a Ferry task for details",
            safe_error_summary(&error)
        )),
        Ok(outcome) if outcome.cancelled => {
            scope_cancellation_feedback(&outcome).expect("cancelled sync must emit feedback")
        }
        Ok(outcome) if !outcome.issues.is_empty() => {
            warning_message("ferry: conflict; run a Ferry task for details".to_string())
        }
        Ok(outcome) => {
            let mut uploaded = 0usize;
            let mut downloaded = 0usize;
            let mut unchanged = 0usize;
            let mut directories = 0usize;
            let mut skipped = 0usize;
            let mut forced = 0usize;
            for event in outcome.events {
                match event.kind {
                    SyncEventKind::Uploaded => uploaded += 1,
                    SyncEventKind::Downloaded => downloaded += 1,
                    SyncEventKind::Unchanged => unchanged += 1,
                    SyncEventKind::CreatedLocalDirectory
                    | SyncEventKind::CreatedRemoteDirectory => directories += 1,
                    SyncEventKind::SkippedAbsent => skipped += 1,
                    SyncEventKind::ForcedRemoteOverwrite => forced += 1,
                }
            }
            info_message(format!(
                "ferry: sync complete: {uploaded} uploaded, {downloaded} downloaded, {unchanged} unchanged, {directories} directories created, {skipped} skipped, {forced} forced"
            ))
        }
    }
}

fn transfer_feedback(relative_path: &str, result: Result<TransferOutcome>) -> Message {
    match result {
        Ok(outcome) => {
            let summary = match outcome.status {
                crate::commands::file_transfer::TransferStatus::Transferred => "transferred",
                crate::commands::file_transfer::TransferStatus::Unchanged => "unchanged",
                crate::commands::file_transfer::TransferStatus::SkippedMissingSource => {
                    "skipped: source missing"
                }
            };
            info_message(format!("ferry: {relative_path}: {summary}"))
        }
        Err(error) => warning_message(format!(
            "ferry: {relative_path}: {}; run a Ferry task for details",
            safe_error_summary(&error)
        )),
    }
}

fn compile_feedback(relative_path: &str, result: Result<FileCheckResult>) -> Message {
    match result {
        Ok(result) => match result.status {
            crate::commands::cc::FileCheckStatus::Passed => {
                info_message(format!("ferry: {relative_path}: compile-check passed"))
            }
            crate::commands::cc::FileCheckStatus::Failed => warning_message(format!(
                "ferry: {relative_path}: compile-check failed: {}",
                result.diagnostics
            )),
            crate::commands::cc::FileCheckStatus::TransportError(_) => warning_message(format!(
                "ferry: {relative_path}: compile-check transport error; run a Ferry task for details"
            )),
        },
        Err(error) => warning_message(format!(
            "ferry: {relative_path}: {}; run a Ferry task for details",
            safe_error_summary(&error)
        )),
    }
}

fn safe_error_summary(error: &anyhow::Error) -> &'static str {
    for source in error.chain() {
        if let Some(exit) = source.downcast_ref::<crate::error::Exit>() {
            return match exit {
                crate::error::Exit::Conflict(_) => "conflict",
                crate::error::Exit::Config(_) => "configuration error",
                crate::error::Exit::Auth(_) => "connection/authentication error",
            };
        }
    }
    "operation failed"
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::{Result, anyhow};
    use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
    use lsp_types::{
        DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
        DidSaveTextDocumentParams, MessageType, ShowMessageParams, TextDocumentContentChangeEvent,
        TextDocumentIdentifier, TextDocumentItem, Uri, VersionedTextDocumentIdentifier,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::commands::cc::{FileCheckResult, FileCheckStatus};
    use crate::commands::file_transfer::{TransferOutcome, TransferStatus};

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        PullPrepare {
            config_path: PathBuf,
            rel: String,
            force: bool,
        },
        ForcePullPrepare {
            config_path: PathBuf,
            rel: String,
        },
        PullApply {
            rel: String,
        },
        Push {
            config_path: PathBuf,
            rel: String,
            force: bool,
        },
        Compile {
            config_path: PathBuf,
            rel: String,
        },
        Sync {
            host: String,
            local_root: PathBuf,
            remote_root: String,
            scope: SyncScope,
            gate_current: bool,
        },
    }

    #[derive(Clone, Copy)]
    enum Failure {
        Conflict,
        Generic,
        SensitiveAuth,
    }

    struct FakeOperations {
        calls: Rc<RefCell<Vec<Call>>>,
        failure: Option<Failure>,
        status: TransferStatus,
        compile_status: FileCheckStatus,
        diagnostics: String,
    }

    #[derive(Default)]
    struct TestDiffLauncher {
        calls: Arc<Mutex<Vec<(PathBuf, PathBuf)>>>,
        fail: bool,
    }

    impl diff::DiffLauncher for TestDiffLauncher {
        fn launch(&mut self, local: &Path, remote: &Path) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push((local.to_path_buf(), remote.to_path_buf()));
            if self.fail {
                anyhow::bail!("launcher sentinel")
            }
            Ok(())
        }
    }

    impl FakeOperations {
        fn successful(calls: Rc<RefCell<Vec<Call>>>) -> Self {
            Self {
                calls,
                failure: None,
                status: TransferStatus::Transferred,
                compile_status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            }
        }

        fn with_status(calls: Rc<RefCell<Vec<Call>>>, status: TransferStatus) -> Self {
            Self {
                calls,
                failure: None,
                status,
                compile_status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            }
        }

        fn failing(calls: Rc<RefCell<Vec<Call>>>, failure: Failure) -> Self {
            Self {
                calls,
                failure: Some(failure),
                status: TransferStatus::Transferred,
                compile_status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            }
        }

        fn compiling(
            calls: Rc<RefCell<Vec<Call>>>,
            status: FileCheckStatus,
            diagnostics: &str,
        ) -> Self {
            Self {
                calls,
                failure: None,
                status: TransferStatus::Transferred,
                compile_status: status,
                diagnostics: diagnostics.to_string(),
            }
        }

        fn result(&self, rel: &str) -> Result<TransferOutcome> {
            match self.failure {
                Some(Failure::Conflict) => {
                    Err(crate::error::Exit::Conflict("changed".into()).into())
                }
                Some(Failure::Generic) => Err(anyhow!("transport unavailable")),
                Some(Failure::SensitiveAuth) => Err(crate::error::Exit::Auth(format!(
                    "login rejected for {REVIEW_SECRET}"
                ))
                .into()),
                None => Ok(TransferOutcome::new(rel, self.status)),
            }
        }
    }

    impl FileOperations for FakeOperations {
        type PreparedPull = String;

        fn prepare_pull(
            &mut self,
            config_path: &Path,
            rel: &str,
            force: bool,
        ) -> Result<Self::PreparedPull> {
            self.calls.borrow_mut().push(Call::PullPrepare {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
                force,
            });
            match self.failure {
                Some(_) => self.result(rel).map(|_| unreachable!()),
                None => Ok(rel.to_string()),
            }
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            request: PullRequest,
        ) -> Result<TransferOutcome> {
            self.calls.borrow_mut().push(Call::PullApply {
                rel: prepared.clone(),
            });
            if !request.try_claim() {
                return Err(crate::error::Exit::Conflict("cancelled".into()).into());
            }
            self.result(&prepared)
        }

        fn push(&mut self, config_path: &Path, rel: &str, force: bool) -> Result<TransferOutcome> {
            self.calls.borrow_mut().push(Call::Push {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
                force,
            });
            self.result(rel)
        }

        fn compile(&mut self, config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            self.calls.borrow_mut().push(Call::Compile {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
            });
            match self.failure {
                Some(Failure::Conflict) => {
                    Err(crate::error::Exit::Conflict("changed".into()).into())
                }
                Some(Failure::Generic) => Err(anyhow!("transport unavailable")),
                Some(Failure::SensitiveAuth) => Err(crate::error::Exit::Auth(format!(
                    "login rejected for {REVIEW_SECRET}"
                ))
                .into()),
                None => Ok(FileCheckResult {
                    path: rel.to_string(),
                    status: self.compile_status.clone(),
                    diagnostics: self.diagnostics.clone(),
                }),
            }
        }
    }

    struct Fixture {
        _temp: TempDir,
        config_path: PathBuf,
        file_path: PathBuf,
    }

    impl Fixture {
        fn new(editor: &str) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let nested = root.join("src/nested");
            fs::create_dir_all(&nested).unwrap();
            let file_path = nested.join("hello world.c");
            fs::write(&file_path, "int main(void) {}\n").unwrap();
            let config_path = root.join(crate::names::CONFIG_FILE);
            let fixture = Self {
                _temp: temp,
                config_path,
                file_path,
            };
            fixture.set_editor(editor);
            fixture
        }

        fn uri(&self) -> Uri {
            let encoded = self.file_path.to_string_lossy().replace(' ', "%20");
            Uri::from_str(&format!("file://{encoded}")).unwrap()
        }

        fn set_editor(&self, editor: &str) {
            fs::write(
                &self.config_path,
                format!(
                    "[connection]\nhost = \"example.invalid\"\nuser = \"u\"\npassword = \"p\"\n\
                     [paths]\nlocal_root = \".\"\nremote_root = \"/project\"\n{editor}"
                ),
            )
            .unwrap();
        }

        fn set_raw_config(&self, config: &str) {
            fs::write(&self.config_path, config).unwrap();
        }

        fn expected_sync_call(&self, scope: SyncScope) -> Call {
            Call::Sync {
                host: "example.invalid".to_string(),
                local_root: self._temp.path().join("."),
                remote_root: "/project".to_string(),
                scope,
                gate_current: true,
            }
        }
    }

    fn did_open(uri: Uri) -> Notification {
        did_open_with_text(uri, "int main(void) {}\n")
    }

    fn did_open_with_text(uri: Uri, text: &str) -> Notification {
        Notification::new(
            "textDocument/didOpen".to_string(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri,
                    language_id: "c".to_string(),
                    version: 1,
                    text: text.to_string(),
                },
            },
        )
    }

    fn did_change(uri: Uri) -> Notification {
        Notification::new(
            "textDocument/didChange".to_string(),
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier { uri, version: 2 },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "changed in editor".to_string(),
                }],
            },
        )
    }

    fn did_save(uri: Uri) -> Notification {
        Notification::new(
            "textDocument/didSave".to_string(),
            DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier::new(uri),
                text: None,
            },
        )
    }

    fn did_close(uri: Uri) -> Notification {
        Notification::new(
            "textDocument/didClose".to_string(),
            DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier::new(uri),
            },
        )
    }

    fn warning(client: &Connection) -> Option<ShowMessageParams> {
        match client.receiver.try_recv().ok()? {
            Message::Notification(notification) if notification.method == "window/showMessage" => {
                serde_json::from_value(notification.params).ok()
            }
            other => panic!("unexpected server message: {other:?}"),
        }
    }

    #[test]
    fn automatic_open_with_default_config_calls_nothing() {
        let fixture = Fixture::new("");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, _client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        server.handle_notification(&server_connection, did_open(fixture.uri()));

        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn automatic_open_enabled_pulls_once_without_force() {
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, _client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        server.handle_notification(&server_connection, did_open(fixture.uri()));

        assert_eq!(
            *calls.borrow(),
            vec![
                Call::PullPrepare {
                    config_path: fixture.config_path,
                    rel: "src/nested/hello world.c".to_string(),
                    force: false,
                },
                Call::PullApply {
                    rel: "src/nested/hello world.c".to_string(),
                },
            ]
        );
    }

    #[test]
    fn automatic_editor_settings_are_reloaded_for_each_event() {
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, _client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        server.handle_notification(&server_connection, did_open(fixture.uri()));
        fixture.set_editor("[editor]\npull_on_open = false\n");
        server.handle_notification(&server_connection, did_open(fixture.uri()));

        assert_eq!(calls.borrow().len(), 2);
    }

    #[test]
    fn automatic_save_with_default_config_calls_nothing() {
        let fixture = Fixture::new("");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, _client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        server.handle_notification(&server_connection, did_save(fixture.uri()));

        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn automatic_save_enabled_pushes_once_without_force() {
        let fixture = Fixture::new("[editor]\npush_on_save = true\n");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, _client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        server.handle_notification(&server_connection, did_save(fixture.uri()));

        assert_eq!(
            *calls.borrow(),
            vec![Call::Push {
                config_path: fixture.config_path,
                rel: "src/nested/hello world.c".to_string(),
                force: false,
            }]
        );
    }

    #[test]
    fn automatic_non_file_uri_calls_nothing() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, _client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));
        let uri = Uri::from_str("untitled:buffer").unwrap();

        server.handle_notification(&server_connection, did_open(uri));

        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn automatic_file_outside_project_calls_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("outside.c");
        fs::write(&path, "").unwrap();
        let uri = Uri::from_str(&format!("file://{}", path.display())).unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, _client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        server.handle_notification(&server_connection, did_open(uri));

        assert!(calls.borrow().is_empty());
    }

    fn automatic_failure_emits_warning(failure: Failure) {
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::failing(calls, failure));

        server.handle_notification(&server_connection, did_open(fixture.uri()));

        let warning = warning(&client_connection).expect("expected warning notification");
        assert_eq!(warning.typ, MessageType::WARNING);
        assert!(warning.message.contains("src/nested/hello world.c"));
    }

    #[test]
    fn automatic_conflict_emits_path_warning() {
        automatic_failure_emits_warning(Failure::Conflict);
    }

    #[test]
    fn automatic_generic_failure_emits_path_warning() {
        automatic_failure_emits_warning(Failure::Generic);
    }

    const REVIEW_SECRET: &str = "REVIEW_SECRET_SENTINEL";

    #[test]
    fn automatic_resolution_config_error_never_discloses_source_text() {
        let fixture = Fixture::new("");
        fixture.set_raw_config(&format!(
            "[connection]\nhost = \"example.invalid\"\nuser = \"u\"\n\
             password = \"{REVIEW_SECRET}\n[paths]\nlocal_root = \".\"\nremote_root = \"/\"\n"
        ));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::successful(calls));

        server.handle_notification(&server_connection, did_open(fixture.uri()));

        let warning = warning(&client_connection).expect("expected warning notification");
        assert_eq!(warning.typ, MessageType::WARNING);
        assert!(warning.message.contains("configuration error"));
        assert!(!warning.message.contains(REVIEW_SECRET));
    }

    #[test]
    fn automatic_operation_auth_error_never_discloses_details() {
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::failing(calls, Failure::SensitiveAuth));

        server.handle_notification(&server_connection, did_open(fixture.uri()));

        let warning = warning(&client_connection).expect("expected warning notification");
        assert_eq!(warning.typ, MessageType::WARNING);
        assert!(warning.message.contains("src/nested/hello world.c"));
        assert!(warning.message.contains("connection/authentication error"));
        assert!(!warning.message.contains(REVIEW_SECRET));
    }

    struct CorruptingConfigOperations;

    impl FileOperations for CorruptingConfigOperations {
        type PreparedPull = String;

        fn prepare_pull(
            &mut self,
            config_path: &Path,
            _rel: &str,
            _force: bool,
        ) -> Result<Self::PreparedPull> {
            fs::write(
                config_path,
                format!(
                    "[connection]\nhost = \"example.invalid\"\nuser = \"u\"\n\
                     password = \"{REVIEW_SECRET}\n[paths]\nremote_root = \"/\"\n"
                ),
            )?;
            crate::config::Config::load(config_path)?;
            unreachable!("malformed config must fail")
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            request: PullRequest,
        ) -> Result<TransferOutcome> {
            anyhow::ensure!(request.try_claim(), "cancelled");
            Ok(TransferOutcome::new(&prepared, TransferStatus::Transferred))
        }

        fn push(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }
    }

    #[test]
    fn automatic_second_config_load_error_never_discloses_source_text() {
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
        let (server_connection, client_connection) = Connection::memory();
        let mut server = Server::new(CorruptingConfigOperations);

        server.handle_notification(&server_connection, did_open(fixture.uri()));

        let warning = warning(&client_connection).expect("expected warning notification");
        assert_eq!(warning.typ, MessageType::WARNING);
        assert!(warning.message.contains("src/nested/hello world.c"));
        assert!(warning.message.contains("configuration error"));
        assert!(!warning.message.contains(REVIEW_SECRET));
    }

    #[test]
    fn automatic_success_outcomes_are_silent() {
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
        for status in [
            TransferStatus::Transferred,
            TransferStatus::Unchanged,
            TransferStatus::SkippedMissingSource,
        ] {
            let calls = Rc::new(RefCell::new(Vec::new()));
            let (server_connection, client_connection) = Connection::memory();
            let mut server = Server::new(FakeOperations::with_status(calls, status));

            server.handle_notification(&server_connection, did_open(fixture.uri()));

            assert!(client_connection.receiver.try_recv().is_err());
        }
    }

    #[test]
    fn capabilities_advertise_text_sync_and_exact_ferry_actions() {
        assert_eq!(
            capabilities(),
            ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        will_save: None,
                        will_save_wait_until: None,
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                    },
                )),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        PULL_COMMAND.to_string(),
                        COMPARE_COMMAND.to_string(),
                        FORCE_PULL_COMMAND.to_string(),
                        PUSH_COMMAND.to_string(),
                        "ferry.syncFile".to_string(),
                        "ferry.syncFolder".to_string(),
                        COMPILE_COMMAND.to_string(),
                    ],
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                ..ServerCapabilities::default()
            }
        );
    }

    struct SendOperations;

    struct UnauthorizedSuccessOperations;

    impl FileOperations for UnauthorizedSuccessOperations {
        type PreparedPull = String;

        fn prepare_pull(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<Self::PreparedPull> {
            Ok(rel.to_string())
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            _request: PullRequest,
        ) -> Result<TransferOutcome> {
            Ok(TransferOutcome::new(&prepared, TransferStatus::Unchanged))
        }

        fn push(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }
    }

    impl FileOperations for SendOperations {
        type PreparedPull = String;

        fn prepare_pull(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<Self::PreparedPull> {
            Ok(rel.to_string())
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            request: PullRequest,
        ) -> Result<TransferOutcome> {
            anyhow::ensure!(request.try_claim(), "cancelled");
            Ok(TransferOutcome::new(&prepared, TransferStatus::Transferred))
        }

        fn push(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }
    }

    fn send_shutdown_request(client: &Connection, id: i32) {
        client
            .sender
            .send(Message::Request(Request::new(
                RequestId::from(id),
                "shutdown".to_string(),
                (),
            )))
            .unwrap();
    }

    fn send_exit(client: &Connection) {
        client
            .sender
            .send(Message::Notification(Notification::new(
                "exit".to_string(),
                (),
            )))
            .unwrap();
    }

    fn send_shutdown(client: &Connection, id: i32) {
        send_shutdown_request(client, id);
        send_exit(client);
    }

    fn response_with_id(message: Message, id: i32) -> Response {
        match message {
            Message::Response(response) if response.id == RequestId::from(id) => response,
            other => panic!("expected response {id}, got {other:?}"),
        }
    }

    fn code_action_request(id: i32, uri: &Uri) -> Request {
        Request::new(
            RequestId::from(id),
            "textDocument/codeAction".to_string(),
            serde_json::json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 0 }
                },
                "context": { "diagnostics": [] }
            }),
        )
    }

    fn request_code_actions(uri: Uri) -> Response {
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(SendOperations)));
        client_connection
            .sender
            .send(Message::Request(code_action_request(61, &uri)))
            .unwrap();

        let response = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("code-action response");
        send_shutdown(&client_connection, 62);
        let shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("shutdown response");
        loop_thread.join().unwrap().unwrap();
        assert!(response_with_id(shutdown, 62).response_result.is_ok());
        response_with_id(response, 61)
    }

    #[test]
    fn code_action_returns_exact_ferry_commands_for_project_file() {
        let fixture = Fixture::new("");
        let uri = fixture.uri();

        let response = request_code_actions(uri.clone());

        assert!(response.response_result.is_ok());
        assert_eq!(
            response.response_result.unwrap(),
            serde_json::json!([
                {
                    "title": "Ferry: Pull",
                    "command": "ferry.pull",
                    "arguments": [uri]
                },
                {
                    "title": "Ferry: Compare with Remote",
                    "command": "ferry.compare",
                    "arguments": [uri]
                },
                {
                    "title": "Ferry: Force Pull (overwrite local)",
                    "command": "ferry.forcePull",
                    "arguments": [uri]
                },
                {
                    "title": "Ferry: Push",
                    "command": "ferry.push",
                    "arguments": [uri]
                },
                {
                    "title": "Ferry: Sync Current File",
                    "command": "ferry.syncFile",
                    "arguments": [uri]
                },
                {
                    "title": "Ferry: Sync Current Folder",
                    "command": "ferry.syncFolder",
                    "arguments": [uri]
                },
                {
                    "title": "Ferry: Compile-check",
                    "command": "ferry.compile",
                    "arguments": [uri]
                }
            ])
        );
    }

    fn receive_acknowledgement(client: &Connection, id: i32) {
        let message = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|_| panic!("request {id} acknowledgement"));
        let response = response_with_id(message, id);
        assert!(response.response_result.is_ok());
        assert_eq!(response.response_result.unwrap(), serde_json::Value::Null);
    }

    fn receive_server_request(client: &Connection, context: &str) -> Request {
        match client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|_| panic!("timed out waiting for {context}"))
        {
            Message::Request(request) => request,
            other => panic!("expected {context}, got {other:?}"),
        }
    }

    fn receive_force_prompt(client: &Connection, command_id: i32) -> Request {
        receive_acknowledgement(client, command_id);
        receive_server_request(client, "Force Pull confirmation request")
    }

    fn respond_to_force_prompt(client: &Connection, prompt: &Request, result: serde_json::Value) {
        client
            .sender
            .send(Message::Response(Response::new_ok(
                prompt.id.clone(),
                result,
            )))
            .unwrap();
    }

    fn assert_exact_force_prompt(prompt: &Request, relative_path: &str, counter: u64) {
        assert_eq!(prompt.method, ShowMessageRequest::METHOD);
        assert_eq!(
            prompt.id,
            RequestId::from(format!("ferry-force-pull-{counter}"))
        );
        assert_eq!(
            prompt.params,
            serde_json::json!({
                "type": MessageType::WARNING,
                "message": format!(
                    "ferry: {relative_path}: Force Pull will overwrite the saved local file. Continue?"
                ),
                "actions": [
                    { "title": "Overwrite local file" },
                    { "title": "Cancel" }
                ]
            })
        );
    }

    #[test]
    fn force_pull_clean_tracked_file_is_acknowledged_immediately_once() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        client
            .sender
            .send(Message::Request(execute_command_request(
                63,
                "ferry.forcePull",
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();

        let prompt = receive_force_prompt(&client, 63);

        assert_exact_force_prompt(&prompt, "src/nested/hello world.c", 1);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[Call::ForcePullPrepare {
                config_path: fixture.config_path.clone(),
                rel: "src/nested/hello world.c".to_string(),
            }],
            "preparation must use the dedicated seam and never apply before confirmation"
        );
        respond_to_force_prompt(&client, &prompt, serde_json::json!({ "title": "Cancel" }));
        finish_loop(&client, loop_thread, 64);
        assert_eq!(calls.lock().unwrap().len(), 1, "Cancel must not apply");
    }

    fn send_force_pull(client: &Connection, id: i32, uri: &Uri) -> Request {
        client
            .sender
            .send(Message::Request(execute_command_request(
                id,
                FORCE_PULL_COMMAND,
                vec![serde_json::to_value(uri).unwrap()],
            )))
            .unwrap();
        receive_force_prompt(client, id)
    }

    fn direct_force_coordinator(fixture: &Fixture) -> Coordinator<String> {
        let mut coordinator =
            Coordinator::new(Arc::new(AtomicBool::new(true)), ShutdownBoundary::noop());
        coordinator
            .documents
            .open(fixture.file_path.clone(), "int main(void) {}\n")
            .unwrap();
        coordinator
    }

    fn begin_direct_force(
        coordinator: &mut Coordinator<String>,
        fixture: &Fixture,
    ) -> (PendingForcePull<String>, OperationGuard) {
        let preparation = coordinator
            .begin_force_pull(PreparedCommand {
                action: ActionCommand::ForcePull,
                absolute_path: fixture.file_path.clone(),
                initial_relative_path: "src/nested/hello world.c".to_string(),
            })
            .unwrap();
        let guard = preparation.guard.clone();
        (
            PendingForcePull {
                operation_id: preparation.operation_id,
                absolute_path: preparation.absolute_path,
                relative_path: preparation.initial_relative_path,
                prepared: "prepared".to_string(),
                guard: preparation.guard,
            },
            guard,
        )
    }

    #[test]
    fn force_pull_affirmative_response_applies_exactly_once_and_old_id_is_ignored() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        let prompt = send_force_pull(&client, 65, &fixture.uri());

        respond_to_force_prompt(
            &client,
            &prompt,
            serde_json::json!({ "title": "Overwrite local file" }),
        );
        let feedback = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("Force Pull apply feedback");
        assert!(matches!(
            feedback,
            Message::Notification(notification) if notification.method == "window/showMessage"
        ));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[
                Call::ForcePullPrepare {
                    config_path: fixture.config_path.clone(),
                    rel: "src/nested/hello world.c".to_string(),
                },
                Call::PullApply {
                    rel: "src/nested/hello world.c".to_string(),
                },
            ]
        );

        respond_to_force_prompt(
            &client,
            &prompt,
            serde_json::json!({ "title": "Overwrite local file" }),
        );
        client
            .sender
            .send(Message::Request(code_action_request(66, &fixture.uri())))
            .unwrap();
        let barrier = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("old-response protocol barrier");
        assert!(response_with_id(barrier, 66).response_result.is_ok());
        assert_eq!(
            calls.lock().unwrap().len(),
            2,
            "old response must not reapply"
        );
        finish_loop(&client, loop_thread, 67);
    }

    #[test]
    fn force_pull_cancel_null_malformed_unknown_action_and_unknown_id_never_apply() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();

        for (offset, response) in [
            serde_json::json!({ "title": "Cancel" }),
            serde_json::Value::Null,
            serde_json::json!({ "notTitle": "Overwrite local file" }),
            serde_json::json!({ "title": "Something else" }),
        ]
        .into_iter()
        .enumerate()
        {
            let prompt = send_force_pull(&client, 70 + offset as i32, &fixture.uri());
            assert_exact_force_prompt(&prompt, "src/nested/hello world.c", offset as u64 + 1);
            respond_to_force_prompt(&client, &prompt, response);
        }

        let prompt = send_force_pull(&client, 74, &fixture.uri());
        client
            .sender
            .send(Message::Response(Response::new_ok(
                RequestId::from("ferry-force-pull-unknown".to_string()),
                Some(MessageActionItem {
                    title: "Overwrite local file".to_string(),
                    properties: HashMap::new(),
                }),
            )))
            .unwrap();
        client
            .sender
            .send(Message::Request(code_action_request(75, &fixture.uri())))
            .unwrap();
        assert!(
            response_with_id(
                client
                    .receiver
                    .recv_timeout(Duration::from_secs(2))
                    .expect("unknown response-id barrier"),
                75,
            )
            .response_result
            .is_ok()
        );
        respond_to_force_prompt(&client, &prompt, serde_json::json!({ "title": "Cancel" }));
        finish_loop(&client, loop_thread, 76);

        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            5,
            "each command prepares once and none applies"
        );
        assert!(
            calls
                .iter()
                .all(|call| matches!(call, Call::ForcePullPrepare { .. }))
        );
    }

    #[test]
    fn force_pull_edit_while_prompting_invalidates_request_and_ignores_old_affirmative() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        let prompt = send_force_pull(&client, 77, &fixture.uri());
        client
            .sender
            .send(Message::Notification(did_change(fixture.uri())))
            .unwrap();
        client
            .sender
            .send(Message::Request(code_action_request(78, &fixture.uri())))
            .unwrap();
        assert_save_retry_warning_with_response(receive_request_messages(&client, 78, 2), 78);
        respond_to_force_prompt(
            &client,
            &prompt,
            serde_json::json!({ "title": "Overwrite local file" }),
        );
        client
            .sender
            .send(Message::Request(execute_command_request(
                79,
                COMPILE_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        let barrier = receive_request_messages(&client, 79, 2);
        assert!(barrier.iter().all(|message| !matches!(
            message,
            Message::Notification(notification)
                if serde_json::from_value::<ShowMessageParams>(notification.params.clone())
                    .is_ok_and(|params| params.typ == MessageType::WARNING)
        )));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[
                Call::ForcePullPrepare {
                    config_path: fixture.config_path.clone(),
                    rel: "src/nested/hello world.c".to_string(),
                },
                Call::Compile {
                    config_path: fixture.config_path.clone(),
                    rel: "src/nested/hello world.c".to_string(),
                },
            ],
            "old prompt response must be unknown and never queue apply"
        );
        finish_loop(&client, loop_thread, 80);
    }

    #[test]
    fn force_pull_repeated_matching_did_open_invalidates_pending_confirmation() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        let prompt = send_force_pull(&client, 81, &fixture.uri());
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        client
            .sender
            .send(Message::Request(code_action_request(82, &fixture.uri())))
            .unwrap();
        assert!(
            response_with_id(client.receiver.recv().unwrap(), 82)
                .response_result
                .is_ok()
        );
        respond_to_force_prompt(
            &client,
            &prompt,
            serde_json::json!({ "title": "Overwrite local file" }),
        );
        client
            .sender
            .send(Message::Request(execute_command_request(
                83,
                COMPILE_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        let barrier = receive_request_messages(&client, 83, 2);
        assert!(barrier.iter().all(|message| !matches!(
            message,
            Message::Notification(notification)
                if serde_json::from_value::<ShowMessageParams>(notification.params.clone())
                    .is_ok_and(|params| params.typ == MessageType::WARNING)
        )));
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|call| !matches!(call, Call::PullApply { .. })),
            "repeated didOpen must remove the slot/request before old response"
        );
        finish_loop(&client, loop_thread, 84);
    }

    #[test]
    fn force_pull_failed_valid_reopen_invalidates_live_confirmation_before_tracking() {
        let fixture = Fixture::new("");
        let mut coordinator = direct_force_coordinator(&fixture);
        let (pending, guard) = begin_direct_force(&mut coordinator, &fixture);
        let (server_connection, client) = Connection::memory();
        assert!(!handle_worker_event(
            &server_connection,
            &mut coordinator,
            WorkerEvent::ForcePullReady(pending),
        ));
        let prompt = receive_server_request(&client, "failed-reopen Force Pull prompt");
        let (work_sender, work_receiver) = mpsc::channel();

        assert!(!handle_notification(
            &server_connection,
            &work_sender,
            &mut coordinator,
            Notification::new(
                "textDocument/didOpen".to_string(),
                serde_json::json!({ "textDocument": {} }),
            ),
        ));
        assert!(!handle_notification(
            &server_connection,
            &work_sender,
            &mut coordinator,
            did_open_with_text(Uri::from_str("untitled:buffer").unwrap(), "unsaved"),
        ));
        assert_eq!(coordinator.force_slots.len(), 1);
        assert_eq!(coordinator.pending_confirmations.len(), 1);
        assert!(guard.is_pending());

        fs::remove_file(&fixture.file_path).unwrap();
        assert!(!handle_notification(
            &server_connection,
            &work_sender,
            &mut coordinator,
            did_open_with_text(fixture.uri(), "int main(void) {}\n"),
        ));
        assert!(!handle_response(
            &server_connection,
            &work_sender,
            &mut coordinator,
            Response::new_ok(
                prompt.id,
                serde_json::json!({ "title": "Overwrite local file" }),
            ),
        ));

        assert!(work_receiver.try_recv().is_err());
        assert!(coordinator.force_slots.is_empty());
        assert!(coordinator.pending_confirmations.is_empty());
        assert!(!guard.is_pending());
        assert!(!fixture.file_path.exists());
        assert!(client.receiver.try_recv().is_err());
    }

    #[test]
    fn force_pull_save_and_close_invalidate_confirmation_and_ignore_late_affirmative() {
        for (label, close, base_id) in [("didSave", false, 401), ("didClose", true, 405)] {
            let fixture = Fixture::new("");
            let calls = Arc::new(Mutex::new(Vec::new()));
            let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
            client
                .sender
                .send(Message::Notification(did_open(fixture.uri())))
                .unwrap();
            let prompt = send_force_pull(&client, base_id, &fixture.uri());
            let lifecycle = if close {
                did_close(fixture.uri())
            } else {
                did_save(fixture.uri())
            };
            client
                .sender
                .send(Message::Notification(lifecycle))
                .unwrap();
            client
                .sender
                .send(Message::Request(code_action_request(
                    base_id + 1,
                    &fixture.uri(),
                )))
                .unwrap();
            assert!(
                response_with_id(client.receiver.recv().unwrap(), base_id + 1)
                    .response_result
                    .is_ok(),
                "{label} protocol barrier"
            );

            respond_to_force_prompt(
                &client,
                &prompt,
                serde_json::json!({ "title": "Overwrite local file" }),
            );
            client
                .sender
                .send(Message::Request(execute_command_request(
                    base_id + 2,
                    COMPILE_COMMAND,
                    vec![serde_json::to_value(fixture.uri()).unwrap()],
                )))
                .unwrap();
            let barrier = receive_request_messages(&client, base_id + 2, 2);
            assert!(
                barrier.iter().all(|message| !matches!(
                    message,
                    Message::Notification(notification)
                        if serde_json::from_value::<ShowMessageParams>(notification.params.clone())
                            .is_ok_and(|params| params.typ == MessageType::WARNING)
                )),
                "{label} must not emit late Force feedback"
            );
            assert!(
                calls
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|call| !matches!(call, Call::PullApply { .. })),
                "{label} must make the old response unknown"
            );
            finish_loop(&client, loop_thread, base_id + 3);
        }
    }

    #[test]
    fn force_pull_json_rpc_error_response_cancels_and_cleans_confirmation() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        let prompt = send_force_pull(&client, 85, &fixture.uri());
        client
            .sender
            .send(Message::Response(Response::new_err(
                prompt.id,
                ErrorCode::InternalError as i32,
                "client rejected request".to_string(),
            )))
            .unwrap();
        let next = send_force_pull(&client, 86, &fixture.uri());
        assert_exact_force_prompt(&next, "src/nested/hello world.c", 2);
        respond_to_force_prompt(&client, &next, serde_json::json!({ "title": "Cancel" }));
        finish_loop(&client, loop_thread, 87);
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|call| !matches!(call, Call::PullApply { .. }))
        );
    }

    #[test]
    fn force_pull_untracked_file_is_acknowledged_once_and_never_prepared() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Request(execute_command_request(
                88,
                FORCE_PULL_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();

        assert_acknowledged_with_warning(receive_request_messages(&client, 88, 2), 88);
        assert!(calls.lock().unwrap().is_empty());
        finish_loop(&client, loop_thread, 89);
    }

    #[test]
    fn force_pull_prompt_send_failure_cleans_confirmation_and_guard() {
        let fixture = Fixture::new("");
        let mut coordinator = direct_force_coordinator(&fixture);
        let (pending, guard) = begin_direct_force(&mut coordinator, &fixture);
        let (failed_connection, failed_client) = Connection::memory();
        drop(failed_client);

        assert!(handle_worker_event(
            &failed_connection,
            &mut coordinator,
            WorkerEvent::ForcePullReady(pending),
        ));
        assert!(coordinator.force_slots.is_empty());
        assert!(coordinator.pending_confirmations.is_empty());
        assert!(!guard.is_pending());

        let (response_connection, _response_client) = Connection::memory();
        let (work_sender, work_receiver) = mpsc::channel();
        assert!(!handle_response(
            &response_connection,
            &work_sender,
            &mut coordinator,
            Response::new_ok(
                RequestId::from("ferry-force-pull-1".to_string()),
                serde_json::json!({ "title": "Overwrite local file" }),
            ),
        ));
        assert!(work_receiver.try_recv().is_err());
    }

    #[test]
    fn force_pull_preparation_enqueue_failure_cleans_guard_and_warns_safely() {
        let fixture = Fixture::new("");
        let mut coordinator = direct_force_coordinator(&fixture);
        let preparation = coordinator
            .begin_force_pull(PreparedCommand {
                action: ActionCommand::ForcePull,
                absolute_path: fixture.file_path.clone(),
                initial_relative_path: "src/nested/hello world.c".to_string(),
            })
            .unwrap();
        let guard = preparation.guard.clone();
        let (failed_sender, failed_receiver) = mpsc::channel();
        drop(failed_receiver);

        assert_eq!(
            coordinator.enqueue_force_pull_preparation(&failed_sender, preparation),
            Err("src/nested/hello world.c".to_string())
        );
        assert!(coordinator.force_slots.is_empty());
        assert!(coordinator.pending_confirmations.is_empty());
        assert!(!guard.is_pending());

        let mut coordinator = direct_force_coordinator(&fixture);
        let (server_connection, client) = Connection::memory();
        let (failed_sender, failed_receiver) = mpsc::channel();
        drop(failed_receiver);
        assert!(
            !handle_request(
                &server_connection,
                &failed_sender,
                &mut coordinator,
                execute_command_request(
                    400,
                    FORCE_PULL_COMMAND,
                    vec![serde_json::to_value(fixture.uri()).unwrap()],
                ),
            )
            .unwrap()
        );

        let messages = receive_request_messages(&client, 400, 2);
        let warnings = messages
            .into_iter()
            .filter_map(|message| match message {
                Message::Notification(notification) => {
                    serde_json::from_value::<ShowMessageParams>(notification.params).ok()
                }
                Message::Response(response) => {
                    assert_eq!(response.response_result.unwrap(), serde_json::Value::Null);
                    None
                }
                other => panic!("unexpected enqueue-failure message: {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].typ, MessageType::WARNING);
        assert_eq!(
            warnings[0].message,
            "ferry: src/nested/hello world.c: operation worker unavailable"
        );
        assert!(coordinator.force_slots.is_empty());
        assert!(coordinator.pending_confirmations.is_empty());
    }

    #[test]
    fn force_pull_apply_enqueue_failure_cleans_guard_and_warns_safely() {
        let fixture = Fixture::new("");
        let mut coordinator = direct_force_coordinator(&fixture);
        let (pending, guard) = begin_direct_force(&mut coordinator, &fixture);
        let (server_connection, client) = Connection::memory();
        assert!(!handle_worker_event(
            &server_connection,
            &mut coordinator,
            WorkerEvent::ForcePullReady(pending),
        ));
        let prompt = receive_server_request(&client, "direct Force Pull prompt");
        let prompt_id = prompt.id.clone();
        let (failed_sender, failed_receiver) = mpsc::channel();
        drop(failed_receiver);

        assert!(!handle_response(
            &server_connection,
            &failed_sender,
            &mut coordinator,
            Response::new_ok(
                prompt_id.clone(),
                serde_json::json!({ "title": "Overwrite local file" }),
            ),
        ));
        let warning = match client.receiver.recv().unwrap() {
            Message::Notification(notification) => {
                serde_json::from_value::<ShowMessageParams>(notification.params).unwrap()
            }
            other => panic!("expected apply-enqueue warning, got {other:?}"),
        };
        assert_eq!(warning.typ, MessageType::WARNING);
        assert_eq!(
            warning.message,
            "ferry: src/nested/hello world.c: operation worker unavailable"
        );
        assert!(coordinator.force_slots.is_empty());
        assert!(coordinator.pending_confirmations.is_empty());
        assert!(!guard.is_pending());

        let (work_sender, work_receiver) = mpsc::channel();
        assert!(!handle_response(
            &server_connection,
            &work_sender,
            &mut coordinator,
            Response::new_ok(
                prompt_id,
                serde_json::json!({ "title": "Overwrite local file" }),
            ),
        ));
        assert!(work_receiver.try_recv().is_err());
        assert!(client.receiver.try_recv().is_err());
    }

    #[test]
    fn force_pull_cancelled_live_confirmation_is_rejected_before_apply_enqueue() {
        let fixture = Fixture::new("");
        let mut coordinator = direct_force_coordinator(&fixture);
        let (pending, guard) = begin_direct_force(&mut coordinator, &fixture);
        let (server_connection, client) = Connection::memory();
        assert!(!handle_worker_event(
            &server_connection,
            &mut coordinator,
            WorkerEvent::ForcePullReady(pending),
        ));
        let prompt = receive_server_request(&client, "live-guard Force Pull prompt");
        guard.cancel();
        let (work_sender, work_receiver) = mpsc::channel();

        assert!(!handle_response(
            &server_connection,
            &work_sender,
            &mut coordinator,
            Response::new_ok(
                prompt.id,
                serde_json::json!({ "title": "Overwrite local file" }),
            ),
        ));
        assert!(work_receiver.try_recv().is_err());
        assert!(coordinator.force_slots.is_empty());
        assert!(coordinator.pending_confirmations.is_empty());
        assert!(!guard.is_pending());
        assert!(client.receiver.try_recv().is_err());
    }

    #[test]
    fn force_pull_dirty_file_is_acknowledged_and_never_prepared() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "unsaved\n",
            )))
            .unwrap();
        client
            .sender
            .send(Message::Request(execute_command_request(
                80,
                FORCE_PULL_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        assert_acknowledged_with_warning(receive_request_messages(&client, 80, 2), 80);
        assert!(calls.lock().unwrap().is_empty());
        finish_loop(&client, loop_thread, 81);
    }

    #[test]
    fn force_pull_same_file_supersedes_existing_prompt_before_second_preparation_finishes() {
        let fixture = Fixture::new("");
        let first_gate = test_gate(true);
        let second_gate = test_gate(false);
        let trace = Arc::new(Mutex::new(Vec::new()));
        let (prepare_started_tx, prepare_started_rx) = mpsc::channel();
        let (apply_started_tx, _apply_started_rx) = mpsc::channel();
        let operations = BlockingForceOperations {
            trace: Arc::clone(&trace),
            prepare_started: prepare_started_tx,
            prepare_gates: vec![first_gate, Arc::clone(&second_gate)],
            prepare_failures: vec![],
            next_prepare: 0,
            apply_started: apply_started_tx,
            apply_gate: test_gate(true),
            apply_failure: false,
            next_apply: 0,
            stopped: None,
        };
        let (server_connection, client) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        let first_prompt = send_force_pull(&client, 82, &fixture.uri());
        assert_eq!(prepare_started_rx.recv().unwrap(), 0);
        client
            .sender
            .send(Message::Request(execute_command_request(
                83,
                FORCE_PULL_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        receive_acknowledgement(&client, 83);
        assert_eq!(prepare_started_rx.recv().unwrap(), 1);

        respond_to_force_prompt(
            &client,
            &first_prompt,
            serde_json::json!({ "title": "Overwrite local file" }),
        );
        client
            .sender
            .send(Message::Request(code_action_request(84, &fixture.uri())))
            .unwrap();
        assert!(
            response_with_id(
                client
                    .receiver
                    .recv_timeout(Duration::from_secs(2))
                    .expect("supersession protocol barrier"),
                84,
            )
            .response_result
            .is_ok()
        );
        assert!(
            trace
                .lock()
                .unwrap()
                .iter()
                .all(|event| !matches!(event, ForceTrace::ApplyStarted(..))),
            "superseded prompt response must not queue apply"
        );

        release_barrier(&second_gate);
        let second_prompt = receive_server_request(&client, "second Force Pull prompt");
        assert_exact_force_prompt(&second_prompt, "src/nested/hello world.c", 2);
        respond_to_force_prompt(
            &client,
            &second_prompt,
            serde_json::json!({ "title": "Cancel" }),
        );
        finish_loop(&client, loop_thread, 85);
    }

    #[test]
    fn force_pull_same_file_supersedes_blocked_preparation_at_second_acceptance() {
        let fixture = Fixture::new("");
        let first_gate = test_gate(false);
        let trace = Arc::new(Mutex::new(Vec::new()));
        let (prepare_started_tx, prepare_started_rx) = mpsc::channel();
        let (apply_started_tx, _apply_started_rx) = mpsc::channel();
        let operations = BlockingForceOperations {
            trace: Arc::clone(&trace),
            prepare_started: prepare_started_tx,
            prepare_gates: vec![Arc::clone(&first_gate), test_gate(true)],
            prepare_failures: vec![true, false],
            next_prepare: 0,
            apply_started: apply_started_tx,
            apply_gate: test_gate(true),
            apply_failure: false,
            next_apply: 0,
            stopped: None,
        };
        let (server_connection, client) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        client
            .sender
            .send(Message::Request(execute_command_request(
                86,
                FORCE_PULL_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        receive_acknowledgement(&client, 86);
        assert_eq!(prepare_started_rx.recv().unwrap(), 0);
        client
            .sender
            .send(Message::Request(execute_command_request(
                87,
                FORCE_PULL_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        receive_acknowledgement(&client, 87);

        release_barrier(&first_gate);
        assert_eq!(prepare_started_rx.recv().unwrap(), 1);
        let second_prompt = receive_server_request(&client, "superseding Force Pull prompt");
        assert_exact_force_prompt(&second_prompt, "src/nested/hello world.c", 1);
        assert!(
            trace
                .lock()
                .unwrap()
                .iter()
                .all(|event| !matches!(event, ForceTrace::ApplyStarted(..))),
            "stale preparation failure must not warn or apply"
        );
        respond_to_force_prompt(
            &client,
            &second_prompt,
            serde_json::json!({ "title": "Cancel" }),
        );
        finish_loop(&client, loop_thread, 88);
    }

    #[test]
    fn force_pull_slot_survives_blocked_apply_and_new_acceptance_cancels_unclaimed_commit() {
        let fixture = Fixture::new("");
        let apply_gate = test_gate(false);
        let trace = Arc::new(Mutex::new(Vec::new()));
        let (prepare_started_tx, prepare_started_rx) = mpsc::channel();
        let (apply_started_tx, apply_started_rx) = mpsc::channel();
        let operations = BlockingForceOperations {
            trace: Arc::clone(&trace),
            prepare_started: prepare_started_tx,
            prepare_gates: vec![test_gate(true), test_gate(true)],
            prepare_failures: vec![],
            next_prepare: 0,
            apply_started: apply_started_tx,
            apply_gate: Arc::clone(&apply_gate),
            apply_failure: false,
            next_apply: 0,
            stopped: None,
        };
        let (server_connection, client) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        let first_prompt = send_force_pull(&client, 89, &fixture.uri());
        assert_eq!(prepare_started_rx.recv().unwrap(), 0);
        respond_to_force_prompt(
            &client,
            &first_prompt,
            serde_json::json!({ "title": "Overwrite local file" }),
        );
        assert_eq!(apply_started_rx.recv().unwrap(), 0);

        client
            .sender
            .send(Message::Request(code_action_request(90, &fixture.uri())))
            .unwrap();
        assert!(
            response_with_id(
                client
                    .receiver
                    .recv_timeout(Duration::from_secs(2))
                    .expect("code action while apply blocked"),
                90,
            )
            .response_result
            .is_ok()
        );
        client
            .sender
            .send(Message::Request(execute_command_request(
                91,
                FORCE_PULL_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        receive_acknowledgement(&client, 91);

        release_barrier(&apply_gate);
        assert_eq!(prepare_started_rx.recv().unwrap(), 1);
        let second_prompt = receive_server_request(&client, "prompt after cancelled blocked apply");
        assert_exact_force_prompt(&second_prompt, "src/nested/hello world.c", 2);
        assert!(
            trace
                .lock()
                .unwrap()
                .iter()
                .all(|event| !matches!(event, ForceTrace::ApplyCommitted(..))),
            "superseded in-flight apply must fail its final guard claim"
        );
        respond_to_force_prompt(
            &client,
            &second_prompt,
            serde_json::json!({ "title": "Cancel" }),
        );
        finish_loop(&client, loop_thread, 92);
    }

    #[test]
    fn force_pull_different_files_keep_independent_prompts_and_apply_independently() {
        let fixture = Fixture::new("");
        let second_path = fixture
            .config_path
            .parent()
            .unwrap()
            .join("src/nested/second.c");
        fs::write(&second_path, b"second\n").unwrap();
        let second_uri = Uri::from_str(&format!("file://{}", second_path.display())).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                second_uri.clone(),
                "second\n",
            )))
            .unwrap();

        let first_prompt = send_force_pull(&client, 93, &fixture.uri());
        let second_prompt = send_force_pull(&client, 94, &second_uri);
        assert_exact_force_prompt(&first_prompt, "src/nested/hello world.c", 1);
        assert_exact_force_prompt(&second_prompt, "src/nested/second.c", 2);
        respond_to_force_prompt(
            &client,
            &second_prompt,
            serde_json::json!({ "title": "Overwrite local file" }),
        );
        assert!(matches!(
            client
                .receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            Message::Notification(_)
        ));
        respond_to_force_prompt(
            &client,
            &first_prompt,
            serde_json::json!({ "title": "Overwrite local file" }),
        );
        assert!(matches!(
            client
                .receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            Message::Notification(_)
        ));
        finish_loop(&client, loop_thread, 95);

        let calls = calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|call| matches!(call, Call::ForcePullPrepare { .. }))
                .count(),
            2
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| matches!(call, Call::PullApply { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn force_pull_edit_while_preparing_cancels_without_prompt_or_apply() {
        let fixture = Fixture::new("");
        let prepare_gate = test_gate(false);
        let trace = Arc::new(Mutex::new(Vec::new()));
        let (prepare_started_tx, prepare_started_rx) = mpsc::channel();
        let (apply_started_tx, _apply_started_rx) = mpsc::channel();
        let operations = BlockingForceOperations {
            trace: Arc::clone(&trace),
            prepare_started: prepare_started_tx,
            prepare_gates: vec![Arc::clone(&prepare_gate)],
            prepare_failures: vec![],
            next_prepare: 0,
            apply_started: apply_started_tx,
            apply_gate: test_gate(true),
            apply_failure: false,
            next_apply: 0,
            stopped: None,
        };
        let (server_connection, client) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        client
            .sender
            .send(Message::Request(execute_command_request(
                96,
                FORCE_PULL_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        receive_acknowledgement(&client, 96);
        assert_eq!(prepare_started_rx.recv().unwrap(), 0);
        client
            .sender
            .send(Message::Notification(did_change(fixture.uri())))
            .unwrap();
        client
            .sender
            .send(Message::Request(code_action_request(97, &fixture.uri())))
            .unwrap();
        assert_save_retry_warning_with_response(receive_request_messages(&client, 97, 2), 97);
        client
            .sender
            .send(Message::Request(execute_command_request(
                98,
                COMPILE_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        receive_acknowledgement(&client, 98);
        release_barrier(&prepare_gate);

        let feedback = match client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
        {
            Message::Notification(notification) => {
                serde_json::from_value::<ShowMessageParams>(notification.params).unwrap()
            }
            other => panic!("expected compile barrier feedback, got {other:?}"),
        };
        assert_eq!(feedback.typ, MessageType::INFO);
        assert!(
            client.receiver.try_recv().is_err(),
            "cancelled preparation must not emit late duplicate feedback"
        );
        assert!(
            trace
                .lock()
                .unwrap()
                .iter()
                .all(|event| !matches!(event, ForceTrace::ApplyStarted(..)))
        );
        finish_loop(&client, loop_thread, 99);
    }

    #[test]
    fn force_pull_shutdown_cancels_blocked_preparation_before_exit_and_suppresses_late_output() {
        let fixture = Fixture::new("");
        let prepare_gate = test_gate(false);
        let trace = Arc::new(Mutex::new(Vec::new()));
        let (prepare_started_tx, prepare_started_rx) = mpsc::channel();
        let (apply_started_tx, _apply_started_rx) = mpsc::channel();
        let (stopped_tx, stopped_rx) = mpsc::sync_channel(1);
        let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
        let operations = BlockingForceOperations {
            trace: Arc::clone(&trace),
            prepare_started: prepare_started_tx,
            prepare_gates: vec![Arc::clone(&prepare_gate)],
            prepare_failures: vec![],
            next_prepare: 0,
            apply_started: apply_started_tx,
            apply_gate: test_gate(true),
            apply_failure: false,
            next_apply: 0,
            stopped: Some(stopped_tx),
        };
        let server = Server::with_shutdown(operations, move || {
            shutdown_tx.send(()).unwrap();
            Ok(())
        });
        let (server_connection, client) = Connection::memory();
        let loop_thread = thread::spawn(move || main_loop(server_connection, server));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        client
            .sender
            .send(Message::Request(execute_command_request(
                99,
                FORCE_PULL_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        receive_acknowledgement(&client, 99);
        assert_eq!(prepare_started_rx.recv().unwrap(), 0);

        send_shutdown_request(&client, 100);
        assert!(
            response_with_id(client.receiver.recv().unwrap(), 100)
                .response_result
                .is_ok()
        );
        shutdown_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("begin_shutdown callback before preparation release");
        release_barrier(&prepare_gate);
        stopped_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("Force worker stops after released preparation");
        assert!(
            client.receiver.try_recv().is_err(),
            "shutdown suppresses late prompt/write/feedback"
        );
        assert!(
            trace
                .lock()
                .unwrap()
                .iter()
                .all(|event| !matches!(event, ForceTrace::ApplyStarted(..)))
        );
        send_exit(&client);
        loop_thread.join().unwrap().unwrap();
    }

    #[test]
    fn force_pull_current_preparation_and_application_failures_warn_once_safely() {
        for (prepare_failure, apply_failure, base_id) in [(true, false, 101), (false, true, 104)] {
            let fixture = Fixture::new("");
            let trace = Arc::new(Mutex::new(Vec::new()));
            let (prepare_started_tx, _prepare_started_rx) = mpsc::channel();
            let (apply_started_tx, _apply_started_rx) = mpsc::channel();
            let operations = BlockingForceOperations {
                trace: Arc::clone(&trace),
                prepare_started: prepare_started_tx,
                prepare_gates: vec![test_gate(true)],
                prepare_failures: vec![prepare_failure],
                next_prepare: 0,
                apply_started: apply_started_tx,
                apply_gate: test_gate(true),
                apply_failure,
                next_apply: 0,
                stopped: None,
            };
            let (server_connection, client) = Connection::memory();
            let loop_thread =
                thread::spawn(move || main_loop(server_connection, Server::new(operations)));
            client
                .sender
                .send(Message::Notification(did_open_with_text(
                    fixture.uri(),
                    "int main(void) {}\n",
                )))
                .unwrap();
            client
                .sender
                .send(Message::Request(execute_command_request(
                    base_id,
                    FORCE_PULL_COMMAND,
                    vec![serde_json::to_value(fixture.uri()).unwrap()],
                )))
                .unwrap();
            receive_acknowledgement(&client, base_id);
            if !prepare_failure {
                let prompt = receive_server_request(&client, "failure-case Force prompt");
                respond_to_force_prompt(
                    &client,
                    &prompt,
                    serde_json::json!({ "title": "Overwrite local file" }),
                );
            }
            let warning = match client
                .receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
            {
                Message::Notification(notification) => {
                    serde_json::from_value::<ShowMessageParams>(notification.params).unwrap()
                }
                other => panic!("expected safe Force failure warning, got {other:?}"),
            };
            assert_eq!(warning.typ, MessageType::WARNING);
            assert!(warning.message.contains("src/nested/hello world.c"));
            assert!(warning.message.contains("operation failed"));
            assert!(!warning.message.contains("injected"));
            assert!(
                client.receiver.try_recv().is_err(),
                "failure warns exactly once"
            );
            assert!(
                trace
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|event| !matches!(event, ForceTrace::ApplyCommitted(..)))
            );
            finish_loop(&client, loop_thread, base_id + 1);
        }
    }

    #[test]
    fn code_action_returns_empty_for_non_file_uri() {
        let uri = Uri::from_str("untitled:buffer").unwrap();

        let response = request_code_actions(uri);

        assert!(response.response_result.is_ok());
        assert_eq!(response.response_result.unwrap(), serde_json::json!([]));
    }

    #[test]
    fn code_action_returns_empty_for_file_outside_project() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("outside.c");
        fs::write(&path, "").unwrap();
        crate::project::assert_no_config_above(temp.path());
        let uri = Uri::from_str(&format!("file://{}", path.display())).unwrap();

        let response = request_code_actions(uri);

        assert!(response.response_result.is_ok());
        assert_eq!(response.response_result.unwrap(), serde_json::json!([]));
    }

    #[test]
    fn code_action_rejects_malformed_parameters() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut server = Server::new(FakeOperations::successful(calls));
        let request = Request::new(
            RequestId::from(63),
            CODE_ACTION_METHOD.to_string(),
            serde_json::json!({ "textDocument": {} }),
        );

        let (response, messages) = process_server_request(&mut server, request);

        assert_eq!(
            response.response_result.unwrap_err().code,
            ErrorCode::InvalidParams as i32
        );
        assert!(messages.is_empty());
    }

    fn execute_command_request(
        id: i32,
        command: &str,
        arguments: Vec<serde_json::Value>,
    ) -> Request {
        Request::new(
            RequestId::from(id),
            EXECUTE_COMMAND_METHOD.to_string(),
            serde_json::json!({
                "command": command,
                "arguments": arguments
            }),
        )
    }

    fn process_server_request<O: FileOperations>(
        server: &mut Server<O>,
        request: Request,
    ) -> (Response, Vec<ShowMessageParams>) {
        let mut response = None;
        let mut notifications = Vec::new();
        for message in server.process_request(request) {
            match message {
                Message::Response(item) => {
                    assert!(response.replace(item).is_none(), "duplicate response");
                }
                Message::Notification(item) if item.method == "window/showMessage" => {
                    notifications.push(serde_json::from_value(item.params).unwrap());
                }
                other => panic!("unexpected server message: {other:?}"),
            }
        }
        (response.expect("request response"), notifications)
    }

    #[test]
    fn execute_command_invalid_inputs_return_invalid_params_without_operations() {
        let fixture = Fixture::new("");
        let uri = serde_json::to_value(fixture.uri()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside.c");
        fs::write(&outside, "").unwrap();
        crate::project::assert_no_config_above(temp.path());
        let outside_uri = serde_json::json!(format!("file://{}", outside.display()));
        let requests = vec![
            execute_command_request(70, "ferry.unknown", vec![uri.clone()]),
            execute_command_request(71, PULL_COMMAND, vec![]),
            execute_command_request(72, PUSH_COMMAND, vec![uri.clone(), uri.clone()]),
            execute_command_request(73, COMPILE_COMMAND, vec![serde_json::json!(42)]),
            Request::new(
                RequestId::from(74),
                EXECUTE_COMMAND_METHOD.to_string(),
                serde_json::json!({ "arguments": [uri.clone()] }),
            ),
            execute_command_request(75, PULL_COMMAND, vec![serde_json::json!("untitled:buffer")]),
            execute_command_request(76, PUSH_COMMAND, vec![outside_uri]),
        ];
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        for request in requests {
            let (response, messages) = process_server_request(&mut server, request);
            assert_eq!(
                response
                    .response_result
                    .expect_err("InvalidParams response")
                    .code,
                ErrorCode::InvalidParams as i32
            );
            assert!(messages.is_empty());
        }
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn execute_command_manual_pull_and_push_run_once_without_force() {
        let fixture = Fixture::new("");
        let uri = serde_json::to_value(fixture.uri()).unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        for (id, command) in [(80, PULL_COMMAND), (81, PUSH_COMMAND)] {
            let (response, messages) = process_server_request(
                &mut server,
                execute_command_request(id, command, vec![uri.clone()]),
            );
            assert!(response.response_result.is_ok());
            assert_eq!(response.response_result.unwrap(), serde_json::Value::Null);
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].typ, MessageType::INFO);
        }
        assert_eq!(
            *calls.borrow(),
            vec![
                Call::PullPrepare {
                    config_path: fixture.config_path.clone(),
                    rel: "src/nested/hello world.c".to_string(),
                    force: false,
                },
                Call::PullApply {
                    rel: "src/nested/hello world.c".to_string(),
                },
                Call::Push {
                    config_path: fixture.config_path,
                    rel: "src/nested/hello world.c".to_string(),
                    force: false,
                }
            ]
        );
    }

    #[test]
    fn execute_command_recognizes_both_scoped_sync_commands() {
        let fixture = Fixture::new("");
        let uri = serde_json::to_value(fixture.uri()).unwrap();

        for (id, command) in [(182, "ferry.syncFile"), (183, "ferry.syncFolder")] {
            let calls = Rc::new(RefCell::new(Vec::new()));
            let mut server = Server::new(FakeOperations::successful(calls));
            let (response, messages) = process_server_request(
                &mut server,
                execute_command_request(id, command, vec![uri.clone()]),
            );

            assert!(
                response.response_result.is_ok(),
                "{command} must be executable"
            );
            assert_eq!(response.response_result.unwrap(), serde_json::Value::Null);
            assert_eq!(messages.len(), 1);
        }
    }

    #[test]
    fn scoped_sync_commands_pass_exact_file_nested_folder_and_root_folder_scopes() {
        let fixture = Fixture::new("");
        let root_file = fixture._temp.path().join("root.c");
        fs::write(&root_file, "root\n").unwrap();
        let root_uri = Uri::from_str(&format!("file://{}", root_file.display())).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));

        for uri in [fixture.uri(), root_uri.clone()] {
            let text = if uri == root_uri {
                "root\n"
            } else {
                "int main(void) {}\n"
            };
            client
                .sender
                .send(Message::Notification(did_open_with_text(uri, text)))
                .unwrap();
        }

        for (id, command, uri) in [
            (184, SYNC_FILE_COMMAND, fixture.uri()),
            (185, SYNC_FOLDER_COMMAND, fixture.uri()),
            (186, SYNC_FOLDER_COMMAND, root_uri),
        ] {
            client
                .sender
                .send(Message::Request(execute_command_request(
                    id,
                    command,
                    vec![serde_json::to_value(uri).unwrap()],
                )))
                .unwrap();
            let messages = receive_request_messages(&client, id, 2);
            assert!(messages.iter().any(
                |message| matches!(message, Message::Response(response) if response.response_result.is_ok())
            ));
        }

        finish_loop(&client, loop_thread, 187);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[
                fixture.expected_sync_call(SyncScope::Path("src/nested/hello world.c".to_string())),
                fixture.expected_sync_call(SyncScope::Path("src/nested".to_string())),
                fixture.expected_sync_call(SyncScope::RootDirectory),
            ]
        );
    }

    fn assert_scope_save_all_warning(messages: Vec<Message>, id: i32) {
        let mut saw_response = false;
        let mut warnings = Vec::new();
        for message in messages {
            match message {
                Message::Response(response) if response.id == RequestId::from(id) => {
                    assert!(response.response_result.is_ok());
                    saw_response = true;
                }
                Message::Notification(notification)
                    if notification.method == "window/showMessage" =>
                {
                    warnings.push(
                        serde_json::from_value::<ShowMessageParams>(notification.params).unwrap(),
                    );
                }
                other => panic!("unexpected scoped-sync admission message: {other:?}"),
            }
        }
        assert!(saw_response);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].typ, MessageType::WARNING);
        assert_eq!(
            warnings[0].message,
            "ferry: folder changed in Zed; save all files in this folder and retry"
        );
    }

    #[test]
    fn dirty_current_file_refuses_both_scoped_sync_actions() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        client
            .sender
            .send(Message::Notification(did_change(fixture.uri())))
            .unwrap();

        for (id, command) in [(188, SYNC_FILE_COMMAND), (189, SYNC_FOLDER_COMMAND)] {
            client
                .sender
                .send(Message::Request(execute_command_request(
                    id,
                    command,
                    vec![serde_json::to_value(fixture.uri()).unwrap()],
                )))
                .unwrap();
            assert_scope_save_all_warning(receive_request_messages(&client, id, 2), id);
        }

        finish_loop(&client, loop_thread, 190);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn folder_sync_refuses_dirty_descendant_but_not_dirty_sibling_outside_scope() {
        let fixture = Fixture::new("");
        let inside_path = fixture.file_path.parent().unwrap().join("inside.c");
        let outside_dir = fixture._temp.path().join("src/other");
        fs::create_dir_all(&outside_dir).unwrap();
        let outside_path = outside_dir.join("outside.c");
        fs::write(&inside_path, "inside\n").unwrap();
        fs::write(&outside_path, "outside\n").unwrap();
        let inside_uri = Uri::from_str(&format!("file://{}", inside_path.display())).unwrap();
        let outside_uri = Uri::from_str(&format!("file://{}", outside_path.display())).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));

        client
            .sender
            .send(Message::Notification(did_open_with_text(
                inside_uri.clone(),
                "inside\n",
            )))
            .unwrap();
        client
            .sender
            .send(Message::Notification(did_change(inside_uri)))
            .unwrap();
        client
            .sender
            .send(Message::Request(execute_command_request(
                191,
                SYNC_FOLDER_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        assert_scope_save_all_warning(receive_request_messages(&client, 191, 2), 191);
        assert!(calls.lock().unwrap().is_empty());

        client
            .sender
            .send(Message::Notification(did_save(fixture.uri())))
            .unwrap();
        client
            .sender
            .send(Message::Notification(did_save(
                Uri::from_str(&format!(
                    "file://{}",
                    fixture
                        .file_path
                        .parent()
                        .unwrap()
                        .join("inside.c")
                        .display()
                ))
                .unwrap(),
            )))
            .unwrap();
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                outside_uri.clone(),
                "outside\n",
            )))
            .unwrap();
        client
            .sender
            .send(Message::Notification(did_change(outside_uri)))
            .unwrap();
        client
            .sender
            .send(Message::Request(execute_command_request(
                192,
                SYNC_FOLDER_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        let messages = receive_request_messages(&client, 192, 2);
        assert!(messages.iter().any(
            |message| matches!(message, Message::Response(response) if response.response_result.is_ok())
        ));

        finish_loop(&client, loop_thread, 193);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[fixture.expected_sync_call(SyncScope::Path("src/nested".to_string()))]
        );
    }

    #[test]
    fn pull_preparation_failure_never_applies_and_emits_safe_warning() {
        let fixture = Fixture::new("");
        let uri = serde_json::to_value(fixture.uri()).unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut server = Server::new(FakeOperations::failing(Rc::clone(&calls), Failure::Generic));

        let (response, messages) = process_server_request(
            &mut server,
            execute_command_request(83, PULL_COMMAND, vec![uri]),
        );

        assert!(response.response_result.is_ok());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].typ, MessageType::WARNING);
        assert!(messages[0].message.contains("operation failed"));
        assert!(!messages[0].message.contains("transport unavailable"));
        assert_eq!(
            calls.borrow().as_slice(),
            &[Call::PullPrepare {
                config_path: fixture.config_path,
                rel: "src/nested/hello world.c".to_string(),
                force: false,
            }]
        );
    }

    #[test]
    fn pull_apply_success_without_authorization_is_rejected_safely() {
        let fixture = Fixture::new("");
        let uri = serde_json::to_value(fixture.uri()).unwrap();
        let mut server = Server::new(UnauthorizedSuccessOperations);

        let (response, messages) = process_server_request(
            &mut server,
            execute_command_request(84, PULL_COMMAND, vec![uri]),
        );

        assert!(response.response_result.is_ok());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].typ, MessageType::WARNING);
        assert!(messages[0].message.contains("operation failed"));
        assert!(!messages[0].message.contains("authorization"));
    }

    #[test]
    fn compare_command_is_acknowledged_without_pull_push_or_compile() {
        let fixture = Fixture::new("");
        let uri = serde_json::to_value(fixture.uri()).unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        let (response, messages) = process_server_request(
            &mut server,
            execute_command_request(82, "ferry.compare", vec![uri]),
        );

        assert!(response.response_result.is_ok());
        assert_eq!(response.response_result.unwrap(), serde_json::Value::Null);
        assert_eq!(messages.len(), 1);
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn compare_request_public_api_exposes_paths_and_one_shot_authorization() {
        let directory = tempfile::tempdir().unwrap();
        let local = directory.path().join("local.c");
        let config = directory.path().join("config.toml");
        fs::write(&local, b"saved local bytes").unwrap();
        let mut documents = DocumentTracker::default();
        documents.open(local.clone(), "saved local bytes").unwrap();
        let request = CompareRequest {
            config_path: config.clone(),
            relative_path: "local.c".to_string(),
            local_path: local.clone(),
            guard: documents.begin_clean_operation(&local).unwrap(),
        };

        assert_eq!(request.config_path(), config);
        assert_eq!(request.relative_path(), "local.c");
        assert_eq!(request.local_path(), local);
        assert!(request.try_claim());
        assert!(!request.try_claim());
    }

    #[test]
    fn pull_request_public_api_is_object_safe_and_one_shot() {
        fn accepts_trait_object(_operations: &mut dyn FileOperations<PreparedPull = String>) {}

        let directory = tempfile::tempdir().unwrap();
        let local = directory.path().join("local.c");
        fs::write(&local, b"saved local bytes").unwrap();
        let mut documents = DocumentTracker::default();
        documents.open(local.clone(), "saved local bytes").unwrap();
        let (request, authorization) =
            PullAuthorization::request(documents.begin_clean_operation(&local).unwrap());
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut operations = FakeOperations::successful(calls);

        accepts_trait_object(&mut operations);
        assert!(request.try_claim());
        assert!(!request.try_claim());
        assert_eq!(authorization.decision(), PULL_AUTHORIZED);
    }

    #[test]
    fn compare_success_fetches_once_launches_local_then_remote_and_retains_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let local = directory.path().join("local.c");
        let config = directory.path().join("config.toml");
        fs::write(&local, b"saved local bytes").unwrap();
        fs::write(&config, b"config sentinel").unwrap();
        let mut documents = DocumentTracker::default();
        documents.open(local.clone(), "saved local bytes").unwrap();
        let guard = documents.begin_clean_operation(&local).unwrap();
        let snapshots = diff::SharedSnapshotStore::new().unwrap();
        let shutdown = snapshots.shutdown_handle();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut launcher = TestDiffLauncher {
            calls: Arc::clone(&calls),
            fail: false,
        };
        let mut fetches = 0;

        let outcome = compare_file_with(
            &snapshots,
            &mut launcher,
            &config,
            "local.c",
            &local,
            guard,
            |observed_config, observed_rel| {
                fetches += 1;
                assert_eq!(observed_config, config);
                assert_eq!(observed_rel, "local.c");
                Ok(b"remote bytes\0\xff".to_vec())
            },
        )
        .unwrap();

        assert_eq!(outcome, CompareOutcome::Opened);
        assert_eq!(fetches, 1);
        let launched = calls.lock().unwrap().clone();
        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].0, local);
        assert!(launched[0].1.is_absolute());
        assert_eq!(fs::read(&launched[0].1).unwrap(), b"remote bytes\0\xff");
        assert!(
            fs::metadata(&launched[0].1)
                .unwrap()
                .permissions()
                .readonly()
        );
        assert_eq!(fs::read(&local).unwrap(), b"saved local bytes");
        assert_eq!(fs::read(&config).unwrap(), b"config sentinel");
        assert!(
            launched[0].1.exists(),
            "successful snapshot must be retained"
        );

        shutdown.shutdown().unwrap();
        assert!(!launched[0].1.exists(), "shutdown must remove the snapshot");
    }

    #[test]
    fn compare_retrieval_failure_does_not_create_or_launch_a_diff() {
        let directory = tempfile::tempdir().unwrap();
        let local = directory.path().join("local.c");
        fs::write(&local, b"saved local bytes").unwrap();
        let mut documents = DocumentTracker::default();
        documents.open(local.clone(), "saved local bytes").unwrap();
        let guard = documents.begin_clean_operation(&local).unwrap();
        let snapshots = diff::SharedSnapshotStore::new().unwrap();
        let shutdown = snapshots.shutdown_handle();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut launcher = TestDiffLauncher {
            calls: Arc::clone(&calls),
            fail: false,
        };
        let mut fetches = 0;

        let error = compare_file_with(
            &snapshots,
            &mut launcher,
            Path::new("unused.toml"),
            "local.c",
            &local,
            guard,
            |_, _| {
                fetches += 1;
                Err(anyhow!("retrieval sentinel"))
            },
        )
        .unwrap_err();

        assert_eq!(fetches, 1);
        assert!(format!("{error:#}").contains("retrieval sentinel"));
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(fs::read(&local).unwrap(), b"saved local bytes");
        shutdown.shutdown().unwrap();
    }

    #[test]
    fn compare_snapshot_creation_failure_is_independent_and_does_not_launch() {
        let directory = tempfile::tempdir().unwrap();
        let local = directory.path().join("local.c");
        fs::write(&local, b"saved local bytes").unwrap();
        let mut documents = DocumentTracker::default();
        documents.open(local.clone(), "saved local bytes").unwrap();
        let guard = documents.begin_clean_operation(&local).unwrap();
        let snapshots = diff::SharedSnapshotStore::new().unwrap();
        snapshots.shutdown_handle().shutdown().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut launcher = TestDiffLauncher {
            calls: Arc::clone(&calls),
            fail: false,
        };
        let mut fetches = 0;

        let error = compare_file_with(
            &snapshots,
            &mut launcher,
            Path::new("unused.toml"),
            "local.c",
            &local,
            guard,
            |_, _| {
                fetches += 1;
                Ok(b"remote bytes".to_vec())
            },
        )
        .unwrap_err();

        assert_eq!(fetches, 1, "retrieval succeeds before snapshot injection");
        assert!(format!("{error:#}").contains("closed"));
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(fs::read(&local).unwrap(), b"saved local bytes");
    }

    #[test]
    fn compare_launcher_failure_drops_snapshot_and_does_not_mutate_local() {
        let directory = tempfile::tempdir().unwrap();
        let local = directory.path().join("local.c");
        fs::write(&local, b"saved local bytes").unwrap();
        let mut documents = DocumentTracker::default();
        documents.open(local.clone(), "saved local bytes").unwrap();
        let guard = documents.begin_clean_operation(&local).unwrap();
        let snapshots = diff::SharedSnapshotStore::new().unwrap();
        let shutdown = snapshots.shutdown_handle();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut launcher = TestDiffLauncher {
            calls: Arc::clone(&calls),
            fail: true,
        };

        let error = compare_file_with(
            &snapshots,
            &mut launcher,
            Path::new("unused.toml"),
            "local.c",
            &local,
            guard,
            |_, _| Ok(b"remote bytes".to_vec()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("launcher sentinel"));
        let launched = calls.lock().unwrap().clone();
        assert_eq!(launched.len(), 1);
        assert!(
            !launched[0].1.exists(),
            "failed snapshot must not be retained"
        );
        assert_eq!(fs::read(&local).unwrap(), b"saved local bytes");
        shutdown.shutdown().unwrap();
    }

    #[test]
    fn compare_local_identity_change_cancels_before_launch() {
        let directory = tempfile::tempdir().unwrap();
        let local = directory.path().join("local.c");
        fs::write(&local, b"saved local bytes").unwrap();
        let mut documents = DocumentTracker::default();
        documents.open(local.clone(), "saved local bytes").unwrap();
        let guard = documents.begin_clean_operation(&local).unwrap();
        let snapshots = diff::SharedSnapshotStore::new().unwrap();
        let shutdown = snapshots.shutdown_handle();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut launcher = TestDiffLauncher {
            calls: Arc::clone(&calls),
            fail: false,
        };

        let outcome = compare_file_with(
            &snapshots,
            &mut launcher,
            Path::new("unused.toml"),
            "local.c",
            &local,
            guard,
            |_, _| {
                fs::write(&local, b"changed local bytes").unwrap();
                Ok(b"remote bytes".to_vec())
            },
        )
        .unwrap();

        assert_eq!(outcome, CompareOutcome::Cancelled);
        assert!(calls.lock().unwrap().is_empty());
        shutdown.shutdown().unwrap();
    }

    #[test]
    fn compare_missing_saved_local_file_fails_before_retrieval() {
        let directory = tempfile::tempdir().unwrap();
        let tracked = directory.path().join("tracked.c");
        let missing = directory.path().join("missing.c");
        fs::write(&tracked, b"saved local bytes").unwrap();
        let mut documents = DocumentTracker::default();
        documents
            .open(tracked.clone(), "saved local bytes")
            .unwrap();
        let guard = documents.begin_clean_operation(&tracked).unwrap();
        let snapshots = diff::SharedSnapshotStore::new().unwrap();
        let shutdown = snapshots.shutdown_handle();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut launcher = TestDiffLauncher {
            calls: Arc::clone(&calls),
            fail: false,
        };
        let mut fetches = 0;

        let error = compare_file_with(
            &snapshots,
            &mut launcher,
            Path::new("unused.toml"),
            "missing.c",
            &missing,
            guard,
            |_, _| {
                fetches += 1;
                Ok(b"remote bytes".to_vec())
            },
        )
        .unwrap_err();

        assert_eq!(fetches, 0);
        assert!(format!("{error:#}").contains("saved local file"));
        assert!(calls.lock().unwrap().is_empty());
        shutdown.shutdown().unwrap();
    }

    #[test]
    fn compare_ferry_operations_constructor_is_fallible_and_shutdown_is_real() {
        let operations = FerryOperations::new().unwrap();
        (operations.shutdown_callback())().unwrap();
    }

    #[test]
    fn execute_command_transfer_success_feedback_distinguishes_every_outcome() {
        let fixture = Fixture::new("");
        let uri = serde_json::to_value(fixture.uri()).unwrap();
        for (status, expected) in [
            (TransferStatus::Transferred, "transferred"),
            (TransferStatus::Unchanged, "unchanged"),
            (TransferStatus::SkippedMissingSource, "source missing"),
        ] {
            let calls = Rc::new(RefCell::new(Vec::new()));
            let mut server = Server::new(FakeOperations::with_status(calls, status));

            let (response, messages) = process_server_request(
                &mut server,
                execute_command_request(90, PULL_COMMAND, vec![uri.clone()]),
            );

            assert!(response.response_result.is_ok());
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].typ, MessageType::INFO);
            assert!(messages[0].message.contains("src/nested/hello world.c"));
            assert!(messages[0].message.contains(expected));
        }
    }

    #[test]
    fn execute_command_transfer_failures_emit_safe_path_warnings() {
        let fixture = Fixture::new("");
        let uri = serde_json::to_value(fixture.uri()).unwrap();
        for failure in [Failure::Conflict, Failure::Generic, Failure::SensitiveAuth] {
            let calls = Rc::new(RefCell::new(Vec::new()));
            let mut server = Server::new(FakeOperations::failing(calls, failure));

            let (response, messages) = process_server_request(
                &mut server,
                execute_command_request(100, PUSH_COMMAND, vec![uri.clone()]),
            );

            assert!(response.response_result.is_ok());
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].typ, MessageType::WARNING);
            assert!(messages[0].message.contains("src/nested/hello world.c"));
            assert!(!messages[0].message.contains(REVIEW_SECRET));
            assert!(!messages[0].message.contains("transport unavailable"));
        }
    }

    #[test]
    fn execute_command_compile_pass_and_failure_emit_detailed_feedback() {
        let fixture = Fixture::new("");
        let uri = serde_json::to_value(fixture.uri()).unwrap();
        for (status, diagnostics, expected_type) in [
            (FileCheckStatus::Passed, "", MessageType::INFO),
            (
                FileCheckStatus::Failed,
                "line 9: expected semicolon",
                MessageType::WARNING,
            ),
        ] {
            let calls = Rc::new(RefCell::new(Vec::new()));
            let mut server = Server::new(FakeOperations::compiling(
                Rc::clone(&calls),
                status,
                diagnostics,
            ));

            let (response, messages) = process_server_request(
                &mut server,
                execute_command_request(110, COMPILE_COMMAND, vec![uri.clone()]),
            );

            assert!(response.response_result.is_ok());
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].typ, expected_type);
            assert!(messages[0].message.contains("src/nested/hello world.c"));
            if !diagnostics.is_empty() {
                assert!(messages[0].message.contains(diagnostics));
            }
            assert_eq!(calls.borrow().len(), 1);
        }
    }

    #[test]
    fn execute_command_compile_transport_cases_emit_safe_warnings() {
        let fixture = Fixture::new("");
        let uri = serde_json::to_value(fixture.uri()).unwrap();
        let cases = [
            FakeOperations::compiling(
                Rc::new(RefCell::new(Vec::new())),
                FileCheckStatus::TransportError(REVIEW_SECRET.to_string()),
                "",
            ),
            FakeOperations::failing(Rc::new(RefCell::new(Vec::new())), Failure::SensitiveAuth),
        ];
        for operations in cases {
            let mut server = Server::new(operations);

            let (response, messages) = process_server_request(
                &mut server,
                execute_command_request(120, COMPILE_COMMAND, vec![uri.clone()]),
            );

            assert!(response.response_result.is_ok());
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].typ, MessageType::WARNING);
            assert!(messages[0].message.contains("src/nested/hello world.c"));
            assert!(!messages[0].message.contains(REVIEW_SECRET));
        }
    }

    struct RecordingSendOperations {
        calls: Arc<Mutex<Vec<Call>>>,
    }

    impl FileOperations for RecordingSendOperations {
        type PreparedPull = String;

        fn prepare_pull(
            &mut self,
            config_path: &Path,
            rel: &str,
            force: bool,
        ) -> Result<Self::PreparedPull> {
            self.calls.lock().unwrap().push(Call::PullPrepare {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
                force,
            });
            Ok(rel.to_string())
        }

        fn prepare_force_pull(
            &mut self,
            config_path: &Path,
            rel: &str,
        ) -> Result<Self::PreparedPull> {
            self.calls.lock().unwrap().push(Call::ForcePullPrepare {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
            });
            Ok(rel.to_string())
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            request: PullRequest,
        ) -> Result<TransferOutcome> {
            self.calls.lock().unwrap().push(Call::PullApply {
                rel: prepared.clone(),
            });
            anyhow::ensure!(request.try_claim(), "cancelled");
            Ok(TransferOutcome::new(&prepared, TransferStatus::Transferred))
        }

        fn push(&mut self, config_path: &Path, rel: &str, force: bool) -> Result<TransferOutcome> {
            self.calls.lock().unwrap().push(Call::Push {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
                force,
            });
            Ok(TransferOutcome::new(rel, TransferStatus::Unchanged))
        }

        fn compile(&mut self, config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            self.calls.lock().unwrap().push(Call::Compile {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
            });
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }

        fn sync(&mut self, request: SyncRequest) -> Result<SyncOutcome> {
            let config = request.config;
            self.calls.lock().unwrap().push(Call::Sync {
                host: config.connection.host,
                local_root: config.paths.local_root,
                remote_root: config.paths.remote_root,
                scope: request.scope,
                gate_current: request.gate.is_current(),
            });
            Ok(SyncOutcome::default())
        }
    }

    struct BlockingSyncOperations {
        started: mpsc::SyncSender<SyncScope>,
        release: mpsc::Receiver<()>,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct ResolvedSyncSnapshot {
        host: String,
        local_root: PathBuf,
        remote_root: String,
        scope: SyncScope,
    }

    struct ConfigReplacingSyncOperations {
        config_path: PathBuf,
        replacement: String,
        calls: Arc<AtomicUsize>,
        observed: mpsc::SyncSender<ResolvedSyncSnapshot>,
    }

    impl FileOperations for ConfigReplacingSyncOperations {
        type PreparedPull = String;

        fn prepare_pull(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<Self::PreparedPull> {
            Ok(rel.to_string())
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            request: PullRequest,
        ) -> Result<TransferOutcome> {
            anyhow::ensure!(request.try_claim(), "cancelled");
            Ok(TransferOutcome::new(&prepared, TransferStatus::Unchanged))
        }

        fn push(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            Ok(TransferOutcome::new(rel, TransferStatus::Unchanged))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }

        fn sync(&mut self, request: SyncRequest) -> Result<SyncOutcome> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            fs::write(&self.config_path, &self.replacement)?;
            let config = request.config;
            self.observed
                .send(ResolvedSyncSnapshot {
                    host: config.connection.host,
                    local_root: config.paths.local_root,
                    remote_root: config.paths.remote_root,
                    scope: request.scope,
                })
                .map_err(|_| anyhow!("snapshot observer disconnected"))?;
            Ok(SyncOutcome::default())
        }
    }

    impl FileOperations for BlockingSyncOperations {
        type PreparedPull = String;

        fn prepare_pull(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<Self::PreparedPull> {
            Ok(rel.to_string())
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            request: PullRequest,
        ) -> Result<TransferOutcome> {
            anyhow::ensure!(request.try_claim(), "cancelled");
            Ok(TransferOutcome::new(&prepared, TransferStatus::Unchanged))
        }

        fn push(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            Ok(TransferOutcome::new(rel, TransferStatus::Unchanged))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }

        fn sync(&mut self, request: SyncRequest) -> Result<SyncOutcome> {
            self.started.send(request.scope).unwrap();
            self.release
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| anyhow!("timed out waiting to release sync"))?;
            let mut mutation = || Ok(());
            let decision = request.gate.commit(&mut mutation)?;
            Ok(SyncOutcome {
                cancelled: decision == crate::commands::sync::CommitDecision::Cancelled,
                ..SyncOutcome::default()
            })
        }
    }

    fn start_blocking_sync_loop() -> (
        Connection,
        thread::JoinHandle<Result<()>>,
        mpsc::Receiver<SyncScope>,
        mpsc::SyncSender<()>,
    ) {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let operations = BlockingSyncOperations {
            started: started_tx,
            release: release_rx,
        };
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        (client_connection, loop_thread, started_rx, release_tx)
    }

    fn receive_show_message(client: &Connection, context: &str) -> ShowMessageParams {
        match client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|_| panic!("timed out waiting for {context}"))
        {
            Message::Notification(notification) if notification.method == "window/showMessage" => {
                serde_json::from_value(notification.params).unwrap()
            }
            other => panic!("expected {context}, got {other:?}"),
        }
    }

    #[test]
    fn both_scoped_sync_commands_acknowledge_before_completion_and_keep_actions_responsive() {
        for (offset, command, expected_scope) in [
            (
                0,
                SYNC_FILE_COMMAND,
                SyncScope::Path("src/nested/hello world.c".to_string()),
            ),
            (
                1,
                SYNC_FOLDER_COMMAND,
                SyncScope::Path("src/nested".to_string()),
            ),
        ] {
            let fixture = Fixture::new("");
            let (client, loop_thread, started, release) = start_blocking_sync_loop();
            client
                .sender
                .send(Message::Notification(did_open(fixture.uri())))
                .unwrap();
            let command_id = 194 + offset * 3;
            client
                .sender
                .send(Message::Request(execute_command_request(
                    command_id,
                    command,
                    vec![serde_json::to_value(fixture.uri()).unwrap()],
                )))
                .unwrap();

            receive_acknowledgement(&client, command_id);
            assert_eq!(
                started.recv_timeout(Duration::from_secs(2)).unwrap(),
                expected_scope,
                "{command}"
            );
            assert!(
                client.receiver.try_recv().is_err(),
                "{command} is still blocked"
            );

            let action_id = command_id + 1;
            client
                .sender
                .send(Message::Request(code_action_request(
                    action_id,
                    &fixture.uri(),
                )))
                .unwrap();
            let response = response_with_id(
                client
                    .receiver
                    .recv_timeout(Duration::from_secs(2))
                    .expect("responsive Code Action response"),
                action_id,
            );
            assert!(response.response_result.is_ok(), "{command}");
            assert_eq!(
                response.response_result.unwrap().as_array().unwrap().len(),
                7
            );

            release.send(()).unwrap();
            let feedback = receive_show_message(&client, "scoped-sync completion");
            assert_eq!(feedback.typ, MessageType::INFO, "{command}");
            assert!(
                feedback.message.starts_with("ferry: sync complete:"),
                "{command}"
            );
            finish_loop(&client, loop_thread, command_id + 2);
        }
    }

    #[test]
    fn scoped_sync_keeps_the_config_snapshot_resolved_by_the_worker() {
        let fixture = Fixture::new("");
        let calls = Arc::new(AtomicUsize::new(0));
        let (observed_tx, observed_rx) = mpsc::sync_channel(1);
        let operations = ConfigReplacingSyncOperations {
            config_path: fixture.config_path.clone(),
            replacement: "[connection]\nhost = \"replacement.invalid\"\nuser = \"other\"\npassword = \"other\"\n\
                          [paths]\nlocal_root = \"replacement\"\nremote_root = \"/replacement\"\n"
                .to_string(),
            calls: Arc::clone(&calls),
            observed: observed_tx,
        };
        let (server_connection, client) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));

        client
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        client
            .sender
            .send(Message::Request(execute_command_request(
                203,
                SYNC_FILE_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        receive_acknowledgement(&client, 203);

        assert_eq!(
            observed_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("resolved sync snapshot"),
            ResolvedSyncSnapshot {
                host: "example.invalid".to_string(),
                local_root: fixture._temp.path().join("."),
                remote_root: "/project".to_string(),
                scope: SyncScope::Path("src/nested/hello world.c".to_string()),
            }
        );
        receive_show_message(&client, "scoped sync completion");
        finish_loop(&client, loop_thread, 204);
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
    }

    #[derive(Clone, Copy, Debug)]
    enum SyncLifecycleNotification {
        Open,
        Change,
        Save,
        Close,
    }

    #[test]
    fn open_change_save_and_close_beneath_in_flight_folder_cancel_before_commit() {
        for (offset, lifecycle) in [
            SyncLifecycleNotification::Open,
            SyncLifecycleNotification::Change,
            SyncLifecycleNotification::Save,
            SyncLifecycleNotification::Close,
        ]
        .into_iter()
        .enumerate()
        {
            let fixture = Fixture::new("");
            let sibling_path = fixture.file_path.parent().unwrap().join("sibling.c");
            fs::write(&sibling_path, "sibling\n").unwrap();
            let sibling_uri = Uri::from_str(&format!("file://{}", sibling_path.display())).unwrap();
            let (client, loop_thread, started, release) = start_blocking_sync_loop();
            client
                .sender
                .send(Message::Notification(did_open(fixture.uri())))
                .unwrap();
            let command_id = 200 + i32::try_from(offset).unwrap() * 3;
            client
                .sender
                .send(Message::Request(execute_command_request(
                    command_id,
                    SYNC_FOLDER_COMMAND,
                    vec![serde_json::to_value(fixture.uri()).unwrap()],
                )))
                .unwrap();
            receive_acknowledgement(&client, command_id);
            started
                .recv_timeout(Duration::from_secs(2))
                .expect("folder sync started");

            let notification = match lifecycle {
                SyncLifecycleNotification::Open => did_open_with_text(sibling_uri, "sibling\n"),
                SyncLifecycleNotification::Change => did_change(fixture.uri()),
                SyncLifecycleNotification::Save => did_save(fixture.uri()),
                SyncLifecycleNotification::Close => did_close(fixture.uri()),
            };
            client
                .sender
                .send(Message::Notification(notification))
                .unwrap();

            let barrier_id = command_id + 1;
            client
                .sender
                .send(Message::Request(code_action_request(
                    barrier_id,
                    &fixture.uri(),
                )))
                .unwrap();
            let barrier = response_with_id(
                client
                    .receiver
                    .recv_timeout(Duration::from_secs(2))
                    .expect("lifecycle barrier response"),
                barrier_id,
            );
            assert!(barrier.response_result.is_ok(), "{lifecycle:?}");

            release.send(()).unwrap();
            let feedback = receive_show_message(&client, "scope cancellation feedback");
            assert_eq!(feedback.typ, MessageType::WARNING, "{lifecycle:?}");
            assert_eq!(
                feedback.message, "ferry: folder changed in Zed; save all files and retry",
                "{lifecycle:?}"
            );
            finish_loop(&client, loop_thread, command_id + 2);
        }
    }

    struct QueueBlockingSyncOperations {
        calls: Arc<Mutex<Vec<Call>>>,
        started: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
        pushed: mpsc::SyncSender<()>,
    }

    impl FileOperations for QueueBlockingSyncOperations {
        type PreparedPull = String;

        fn prepare_pull(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<Self::PreparedPull> {
            Ok(rel.to_string())
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            request: PullRequest,
        ) -> Result<TransferOutcome> {
            anyhow::ensure!(request.try_claim(), "cancelled");
            Ok(TransferOutcome::new(&prepared, TransferStatus::Unchanged))
        }

        fn push(&mut self, config_path: &Path, rel: &str, force: bool) -> Result<TransferOutcome> {
            self.calls.lock().unwrap().push(Call::Push {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
                force,
            });
            self.pushed.send(()).unwrap();
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }

        fn sync(&mut self, request: SyncRequest) -> Result<SyncOutcome> {
            let config = request.config;
            self.calls.lock().unwrap().push(Call::Sync {
                host: config.connection.host,
                local_root: config.paths.local_root,
                remote_root: config.paths.remote_root,
                scope: request.scope,
                gate_current: request.gate.is_current(),
            });
            self.started.send(()).unwrap();
            self.release
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| anyhow!("timed out releasing queued sync"))?;
            let mut mutation = || Ok(());
            let decision = request.gate.commit(&mut mutation)?;
            Ok(SyncOutcome {
                cancelled: decision == crate::commands::sync::CommitDecision::Cancelled,
                ..SyncOutcome::default()
            })
        }
    }

    #[test]
    fn queued_save_runs_after_folder_cancellation_and_re_resolves_project_root() {
        let fixture = Fixture::new("[editor]\npush_on_save = true\n");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (pushed_tx, pushed_rx) = mpsc::sync_channel(1);
        let operations = QueueBlockingSyncOperations {
            calls: Arc::clone(&calls),
            started: started_tx,
            release: release_rx,
            pushed: pushed_tx,
        };
        let (server_connection, client) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        client
            .sender
            .send(Message::Request(execute_command_request(
                212,
                SYNC_FOLDER_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        receive_acknowledgement(&client, 212);
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("folder sync started");

        fixture.set_raw_config(
            "[connection]\nhost = \"example.invalid\"\nuser = \"u\"\npassword = \"p\"\n\
             [paths]\nlocal_root = \"src\"\nremote_root = \"/changed\"\n\
             [editor]\npush_on_save = true\n",
        );
        client
            .sender
            .send(Message::Notification(did_save(fixture.uri())))
            .unwrap();
        client
            .sender
            .send(Message::Request(code_action_request(213, &fixture.uri())))
            .unwrap();
        let barrier = response_with_id(
            client
                .receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("queued save protocol barrier"),
            213,
        );
        assert!(barrier.response_result.is_ok());
        assert!(
            pushed_rx.try_recv().is_err(),
            "save stays behind folder sync"
        );

        release_tx.send(()).unwrap();
        pushed_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("queued save processed after sync");
        let feedback = receive_show_message(&client, "cancelled folder sync");
        assert_eq!(feedback.typ, MessageType::WARNING);
        assert_eq!(
            feedback.message,
            "ferry: folder changed in Zed; save all files and retry"
        );
        finish_loop(&client, loop_thread, 214);

        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[
                fixture.expected_sync_call(SyncScope::Path("src/nested".to_string())),
                Call::Push {
                    config_path: fixture.config_path,
                    rel: "nested/hello world.c".to_string(),
                    force: false,
                },
            ]
        );
    }

    struct StagedSyncOperations {
        destination: PathBuf,
        state_path: PathBuf,
        staged: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
        stopped: mpsc::SyncSender<()>,
    }

    impl Drop for StagedSyncOperations {
        fn drop(&mut self) {
            let _ = self.stopped.send(());
        }
    }

    impl FileOperations for StagedSyncOperations {
        type PreparedPull = String;

        fn prepare_pull(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<Self::PreparedPull> {
            Ok(rel.to_string())
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            request: PullRequest,
        ) -> Result<TransferOutcome> {
            anyhow::ensure!(request.try_claim(), "cancelled");
            Ok(TransferOutcome::new(&prepared, TransferStatus::Unchanged))
        }

        fn push(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            Ok(TransferOutcome::new(rel, TransferStatus::Unchanged))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }

        fn sync(&mut self, request: SyncRequest) -> Result<SyncOutcome> {
            self.staged.send(()).unwrap();
            self.release
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| anyhow!("timed out releasing staged sync"))?;
            let destination = self.destination.clone();
            let state_path = self.state_path.clone();
            let mut mutation = || {
                fs::write(&destination, b"remote replacement")?;
                fs::write(&state_path, b"mutated state")?;
                Ok(())
            };
            let decision = request.gate.commit(&mut mutation)?;
            Ok(SyncOutcome {
                cancelled: decision == crate::commands::sync::CommitDecision::Cancelled,
                ..SyncOutcome::default()
            })
        }
    }

    #[test]
    fn shutdown_after_sync_staging_is_prompt_and_denies_replacement_state_and_feedback() {
        let fixture = Fixture::new("");
        let original = fs::read(&fixture.file_path).unwrap();
        let state_path = fixture._temp.path().join("staged-state.json");
        let (staged_tx, staged_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (stopped_tx, stopped_rx) = mpsc::sync_channel(1);
        let operations = StagedSyncOperations {
            destination: fixture.file_path.clone(),
            state_path: state_path.clone(),
            staged: staged_tx,
            release: release_rx,
            stopped: stopped_tx,
        };
        let (server_connection, client) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        client
            .sender
            .send(Message::Request(execute_command_request(
                215,
                SYNC_FILE_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        receive_acknowledgement(&client, 215);
        staged_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("sync reached staged commit boundary");

        let shutdown_started = Instant::now();
        send_shutdown_request(&client, 216);
        let shutdown = client
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("prompt shutdown response while sync is staged");
        assert!(response_with_id(shutdown, 216).response_result.is_ok());
        assert!(shutdown_started.elapsed() < Duration::from_secs(1));
        assert_eq!(fs::read(&fixture.file_path).unwrap(), original);
        assert!(!state_path.exists());

        send_exit(&client);
        loop_thread.join().unwrap().unwrap();
        assert!(
            client
                .receiver
                .try_recv()
                .expect_err("LSP writer closed")
                .is_disconnected(),
            "detached worker must not retain the LSP writer"
        );

        release_tx.send(()).unwrap();
        stopped_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("staged sync worker stopped after release");
        assert_eq!(fs::read(&fixture.file_path).unwrap(), original);
        assert!(!state_path.exists());
        assert!(
            client.receiver.try_recv().is_err(),
            "no late worker feedback"
        );
    }

    fn show_message_params(message: Message) -> ShowMessageParams {
        match message {
            Message::Notification(notification) if notification.method == "window/showMessage" => {
                serde_json::from_value(notification.params).unwrap()
            }
            other => panic!("expected showMessage notification, got {other:?}"),
        }
    }

    #[test]
    fn sync_feedback_uses_typed_counts_and_redacts_paths_secrets_and_raw_transport_text() {
        let hostile_path = format!("/absolute/{REVIEW_SECRET}/raw LIST record");
        let events = [
            SyncEventKind::Uploaded,
            SyncEventKind::Downloaded,
            SyncEventKind::Unchanged,
            SyncEventKind::CreatedLocalDirectory,
            SyncEventKind::CreatedRemoteDirectory,
            SyncEventKind::SkippedAbsent,
            SyncEventKind::ForcedRemoteOverwrite,
        ]
        .into_iter()
        .map(|kind| crate::commands::sync::SyncEvent {
            path: hostile_path.clone(),
            kind,
        })
        .collect();
        let success = show_message_params(sync_feedback(Ok(SyncOutcome {
            events,
            issues: Vec::new(),
            cancelled: false,
        })));
        assert_eq!(success.typ, MessageType::INFO);
        assert_eq!(
            success.message,
            "ferry: sync complete: 1 uploaded, 1 downloaded, 1 unchanged, 2 directories created, 1 skipped, 1 forced"
        );

        let conflict = show_message_params(sync_feedback(Ok(SyncOutcome {
            events: Vec::new(),
            issues: vec![crate::commands::sync::SyncIssue::FileConflict {
                path: hostile_path.clone(),
                state: crate::state::FileState::BothChanged,
            }],
            cancelled: false,
        })));
        assert_eq!(conflict.typ, MessageType::WARNING);
        assert_eq!(
            conflict.message,
            "ferry: conflict; run a Ferry task for details"
        );

        let cancelled = show_message_params(sync_feedback(Ok(SyncOutcome {
            events: vec![crate::commands::sync::SyncEvent {
                path: hostile_path.clone(),
                kind: SyncEventKind::Uploaded,
            }],
            issues: Vec::new(),
            cancelled: true,
        })));
        assert_eq!(cancelled.typ, MessageType::WARNING);
        assert_eq!(
            cancelled.message,
            "ferry: folder changed in Zed; save all files and retry"
        );

        let auth_error = show_message_params(sync_feedback(Err(crate::error::Exit::Auth(
            format!("login rejected for {REVIEW_SECRET}"),
        )
        .into())));
        assert_eq!(auth_error.typ, MessageType::WARNING);
        assert_eq!(
            auth_error.message,
            "ferry: connection/authentication error; run a Ferry task for details"
        );

        for feedback in [success, conflict, cancelled, auth_error] {
            assert!(!feedback.message.contains(REVIEW_SECRET));
            assert!(!feedback.message.contains("/absolute"));
            assert!(!feedback.message.contains("raw LIST"));
        }
    }

    type TestGate = Arc<(Mutex<bool>, Condvar)>;

    fn test_gate(open: bool) -> TestGate {
        Arc::new((Mutex::new(open), Condvar::new()))
    }

    fn wait_for_gate(gate: &TestGate) {
        let (lock, wake) = &**gate;
        let mut open = lock.lock().unwrap();
        while !*open {
            open = wake.wait(open).unwrap();
        }
    }

    #[derive(Debug, Eq, PartialEq)]
    enum ForceTrace {
        PrepareStarted(usize, String),
        PrepareFinished(usize, String),
        ApplyStarted(usize, String),
        ApplyCommitted(usize, String),
    }

    struct BlockingForceOperations {
        trace: Arc<Mutex<Vec<ForceTrace>>>,
        prepare_started: mpsc::Sender<usize>,
        prepare_gates: Vec<TestGate>,
        prepare_failures: Vec<bool>,
        next_prepare: usize,
        apply_started: mpsc::Sender<usize>,
        apply_gate: TestGate,
        apply_failure: bool,
        next_apply: usize,
        stopped: Option<mpsc::SyncSender<()>>,
    }

    impl Drop for BlockingForceOperations {
        fn drop(&mut self) {
            if let Some(stopped) = &self.stopped {
                let _ = stopped.send(());
            }
        }
    }

    impl FileOperations for BlockingForceOperations {
        type PreparedPull = String;

        fn prepare_pull(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<Self::PreparedPull> {
            Ok(rel.to_string())
        }

        fn prepare_force_pull(
            &mut self,
            _config_path: &Path,
            rel: &str,
        ) -> Result<Self::PreparedPull> {
            let index = self.next_prepare;
            self.next_prepare += 1;
            self.trace
                .lock()
                .unwrap()
                .push(ForceTrace::PrepareStarted(index, rel.to_string()));
            self.prepare_started.send(index).unwrap();
            if let Some(gate) = self.prepare_gates.get(index) {
                wait_for_gate(gate);
            }
            if self.prepare_failures.get(index).copied().unwrap_or(false) {
                anyhow::bail!("injected Force Pull preparation failure")
            }
            self.trace
                .lock()
                .unwrap()
                .push(ForceTrace::PrepareFinished(index, rel.to_string()));
            Ok(rel.to_string())
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            request: PullRequest,
        ) -> Result<TransferOutcome> {
            let index = self.next_apply;
            self.next_apply += 1;
            self.trace
                .lock()
                .unwrap()
                .push(ForceTrace::ApplyStarted(index, prepared.clone()));
            self.apply_started.send(index).unwrap();
            wait_for_gate(&self.apply_gate);
            if self.apply_failure {
                anyhow::bail!("injected Force Pull application failure")
            }
            anyhow::ensure!(request.try_claim(), "cancelled");
            self.trace
                .lock()
                .unwrap()
                .push(ForceTrace::ApplyCommitted(index, prepared.clone()));
            Ok(TransferOutcome::new(&prepared, TransferStatus::Transferred))
        }

        fn push(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            Ok(TransferOutcome::new(rel, TransferStatus::Unchanged))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }
    }

    fn receive_request_messages(client: &Connection, id: i32, count: usize) -> Vec<Message> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut messages = Vec::new();
        while messages.len() < count {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for request {id}");
            messages.push(
                client
                    .receiver
                    .recv_timeout(remaining)
                    .unwrap_or_else(|_| panic!("timed out waiting for request {id}")),
            );
        }
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(message, Message::Response(response) if response.id == RequestId::from(id)))
                .count(),
            1,
            "request must receive exactly one correlated response"
        );
        messages
    }

    fn assert_acknowledged_with_warning(messages: Vec<Message>, id: i32) {
        let mut responses = 0;
        let mut warnings = Vec::new();
        for message in messages {
            match message {
                Message::Response(response) if response.id == RequestId::from(id) => {
                    responses += 1;
                    assert!(response.response_result.is_ok());
                    assert_eq!(response.response_result.unwrap(), serde_json::Value::Null);
                }
                Message::Notification(notification)
                    if notification.method == "window/showMessage" =>
                {
                    warnings.push(
                        serde_json::from_value::<ShowMessageParams>(notification.params).unwrap(),
                    );
                }
                other => panic!("unexpected protocol message: {other:?}"),
            }
        }
        assert_eq!(responses, 1, "request must be acknowledged exactly once");
        assert_eq!(warnings.len(), 1, "request must emit exactly one warning");
        assert_eq!(warnings[0].typ, MessageType::WARNING);
        assert_eq!(
            warnings[0].message,
            "ferry: src/nested/hello world.c: save the file and retry"
        );
    }

    fn assert_save_retry_warning_with_response(messages: Vec<Message>, id: i32) {
        let mut responses = 0;
        let mut warnings = Vec::new();
        for message in messages {
            match message {
                Message::Response(response) if response.id == RequestId::from(id) => {
                    responses += 1;
                    assert!(response.response_result.is_ok());
                }
                Message::Notification(notification)
                    if notification.method == "window/showMessage" =>
                {
                    warnings.push(
                        serde_json::from_value::<ShowMessageParams>(notification.params).unwrap(),
                    );
                }
                other => panic!("unexpected lifecycle message: {other:?}"),
            }
        }
        assert_eq!(responses, 1, "barrier must respond exactly once");
        assert_eq!(warnings.len(), 1, "dirtying must warn exactly once");
        assert_eq!(warnings[0].typ, MessageType::WARNING);
        assert_eq!(
            warnings[0].message,
            "ferry: src/nested/hello world.c: save the file and retry"
        );
    }

    fn start_recording_loop(
        calls: Arc<Mutex<Vec<Call>>>,
    ) -> (Connection, thread::JoinHandle<Result<()>>) {
        let operations = RecordingSendOperations { calls };
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        (client_connection, loop_thread)
    }

    fn send_pull(client: &Connection, id: i32, uri: &Uri) -> Vec<Message> {
        client
            .sender
            .send(Message::Request(execute_command_request(
                id,
                PULL_COMMAND,
                vec![serde_json::to_value(uri).unwrap()],
            )))
            .unwrap();
        receive_request_messages(client, id, 2)
    }

    fn finish_loop(client: &Connection, loop_thread: thread::JoinHandle<Result<()>>, id: i32) {
        send_shutdown(client, id);
        let shutdown = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("shutdown response");
        assert!(response_with_id(shutdown, id).response_result.is_ok());
        loop_thread.join().unwrap().unwrap();
    }

    #[test]
    fn dirty_document_matching_did_open_content_permits_pull() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();

        let messages = send_pull(&client, 201, &fixture.uri());

        assert!(messages.iter().any(|message| matches!(
            message,
            Message::Notification(notification) if notification.method == "window/showMessage"
        )));
        assert_eq!(calls.lock().unwrap().len(), 2);
        finish_loop(&client, loop_thread, 202);
    }

    #[test]
    fn dirty_document_differing_did_open_content_refuses_pull_without_operations() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "unsaved editor contents\n",
            )))
            .unwrap();

        let messages = send_pull(&client, 211, &fixture.uri());

        assert_acknowledged_with_warning(messages, 211);
        assert!(calls.lock().unwrap().is_empty());
        finish_loop(&client, loop_thread, 212);
    }

    #[test]
    fn dirty_document_did_change_refuses_pull_without_operations() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        client
            .sender
            .send(Message::Notification(did_change(fixture.uri())))
            .unwrap();

        let messages = send_pull(&client, 221, &fixture.uri());

        assert_acknowledged_with_warning(messages, 221);
        assert!(calls.lock().unwrap().is_empty());
        finish_loop(&client, loop_thread, 222);
    }

    #[test]
    fn dirty_document_did_save_marks_clean_and_permits_a_new_pull() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        client
            .sender
            .send(Message::Notification(did_change(fixture.uri())))
            .unwrap();
        assert_acknowledged_with_warning(send_pull(&client, 231, &fixture.uri()), 231);
        client
            .sender
            .send(Message::Notification(did_save(fixture.uri())))
            .unwrap();

        let messages = send_pull(&client, 232, &fixture.uri());

        assert!(messages.iter().any(|message| matches!(
            message,
            Message::Notification(notification) if notification.method == "window/showMessage"
        )));
        assert_eq!(calls.lock().unwrap().len(), 2);
        finish_loop(&client, loop_thread, 233);
    }

    #[test]
    fn automatic_restored_dirty_buffer_warns_and_skips_enabled_pull() {
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "restored unsaved contents\n",
            )))
            .unwrap();

        let warning = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("dirty pull-on-open warning");

        let warning = match warning {
            Message::Notification(notification) if notification.method == "window/showMessage" => {
                serde_json::from_value::<ShowMessageParams>(notification.params).unwrap()
            }
            other => panic!("expected dirty pull-on-open warning, got {other:?}"),
        };
        assert_eq!(warning.typ, MessageType::WARNING);
        assert_eq!(
            warning.message,
            "ferry: src/nested/hello world.c: save the file and retry"
        );
        assert!(calls.lock().unwrap().is_empty());
        finish_loop(&client, loop_thread, 243);
    }

    #[test]
    fn automatic_restored_dirty_buffer_is_silent_when_pull_on_open_is_disabled() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "restored unsaved contents\n",
            )))
            .unwrap();

        client
            .sender
            .send(Message::Request(execute_command_request(
                247,
                COMPILE_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        let barrier_messages = receive_request_messages(&client, 247, 2);

        assert!(barrier_messages.iter().all(|message| !matches!(
            message,
            Message::Notification(notification)
                if serde_json::from_value::<ShowMessageParams>(notification.params.clone())
                    .is_ok_and(|params| params.typ == MessageType::WARNING)
        )));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[Call::Compile {
                config_path: fixture.config_path.clone(),
                rel: "src/nested/hello world.c".to_string(),
            }],
            "compile feedback is a FIFO barrier behind the disabled open"
        );
        finish_loop(&client, loop_thread, 248);
    }

    #[test]
    fn dirty_document_malformed_and_non_file_notifications_do_not_clear_dirty_state() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        client
            .sender
            .send(Message::Notification(did_change(fixture.uri())))
            .unwrap();
        client
            .sender
            .send(Message::Notification(Notification::new(
                "textDocument/didSave".to_string(),
                serde_json::json!({ "textDocument": {} }),
            )))
            .unwrap();
        for method in [
            "textDocument/didOpen",
            "textDocument/didChange",
            "textDocument/didClose",
        ] {
            client
                .sender
                .send(Message::Notification(Notification::new(
                    method.to_string(),
                    serde_json::json!({ "textDocument": {} }),
                )))
                .unwrap();
        }
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                Uri::from_str("untitled:buffer").unwrap(),
                "not a file",
            )))
            .unwrap();
        let non_file_uri = Uri::from_str("untitled:buffer").unwrap();
        for notification in [
            did_change(non_file_uri.clone()),
            did_save(non_file_uri.clone()),
            did_close(non_file_uri),
        ] {
            client
                .sender
                .send(Message::Notification(notification))
                .unwrap();
        }

        let messages = send_pull(&client, 245, &fixture.uri());

        assert_acknowledged_with_warning(messages, 245);
        assert!(calls.lock().unwrap().is_empty());
        finish_loop(&client, loop_thread, 246);
    }

    #[test]
    fn dirty_document_did_close_removes_tracking_so_stale_pull_fails_safely() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        client
            .sender
            .send(Message::Notification(did_close(fixture.uri())))
            .unwrap();

        let messages = send_pull(&client, 241, &fixture.uri());

        assert_acknowledged_with_warning(messages, 241);
        assert!(calls.lock().unwrap().is_empty());
        finish_loop(&client, loop_thread, 242);
    }

    #[test]
    fn main_loop_shutdown_cancels_immediately_and_retries_only_incomplete_cleanup() {
        let fixture = Fixture::new("");
        let running = Arc::new(AtomicBool::new(true));
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_shutdown = Arc::clone(&attempts);
        let shutdown = ShutdownBoundary {
            shutdown: Arc::new(move || {
                if attempts_for_shutdown.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                    Err(anyhow!("injected cleanup failure"))
                } else {
                    Ok(())
                }
            }),
        };
        let mut coordinator = Coordinator::<String>::new(Arc::clone(&running), shutdown);
        coordinator
            .documents
            .open(fixture.file_path.clone(), "int main(void) {}\n")
            .unwrap();
        let guard = coordinator
            .documents
            .begin_clean_operation(&fixture.file_path)
            .unwrap();
        let scope_guard = coordinator
            .documents
            .begin_clean_scope(document_state::DocumentScope::Exact(
                fixture.file_path.clone(),
            ))
            .unwrap();

        coordinator.begin_shutdown();

        assert!(!running.load(Ordering::Acquire));
        assert!(!guard.try_claim());
        assert!(!crate::commands::sync::CommitGate::is_current(&scope_guard));
        assert!(!coordinator.shutdown_complete);
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 1);

        coordinator.begin_shutdown();
        assert!(coordinator.shutdown_complete);
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2);

        coordinator.begin_shutdown();
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2);
    }

    #[test]
    fn main_loop_finish_preserves_both_protocol_and_cleanup_errors() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_shutdown = Arc::clone(&attempts);
        let shutdown = ShutdownBoundary {
            shutdown: Arc::new(move || {
                attempts_for_shutdown.fetch_add(1, AtomicOrdering::SeqCst);
                Err(anyhow!("snapshot cleanup failed"))
            }),
        };
        let mut coordinator = Coordinator::<String>::new(Arc::new(AtomicBool::new(true)), shutdown);
        coordinator.begin_shutdown();

        let error = coordinator
            .finish(Err(anyhow!("protocol loop failed")))
            .unwrap_err();
        let rendered = format!("{error:#}");
        let combined = error
            .downcast_ref::<ShutdownFailures>()
            .expect("both failures must remain structurally accessible");

        assert_eq!(
            format!("{:#}", combined.protocol_error),
            "protocol loop failed"
        );
        assert_eq!(
            format!("{:#}", combined.cleanup_error),
            "snapshot cleanup failed"
        );
        assert!(rendered.contains("protocol loop failed"), "{rendered}");
        assert!(rendered.contains("snapshot cleanup failed"), "{rendered}");
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2);
    }

    #[test]
    fn main_loop_failing_shutdown_preserves_protocol_and_cleanup_errors() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_shutdown = Arc::clone(&attempts);
        let server = Server::with_shutdown(SendOperations, move || {
            attempts_for_shutdown.fetch_add(1, AtomicOrdering::SeqCst);
            Err(anyhow!("snapshot cleanup failed"))
        });
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread = thread::spawn(move || main_loop(server_connection, server));
        send_shutdown_request(&client_connection, 249);
        let shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("shutdown response");
        assert!(response_with_id(shutdown, 249).response_result.is_ok());
        client_connection
            .sender
            .send(Message::Request(Request::new(
                RequestId::from(250),
                "unexpected/duringShutdown".to_string(),
                (),
            )))
            .unwrap();

        let error = loop_thread.join().unwrap().unwrap_err();
        let rendered = format!("{error:#}");

        assert!(
            rendered.contains("unexpected message during shutdown"),
            "{rendered}"
        );
        assert!(rendered.contains("snapshot cleanup failed"), "{rendered}");
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2);
    }

    #[test]
    fn main_loop_finish_preserves_single_protocol_or_cleanup_error() {
        let protocol_error =
            Coordinator::<String>::new(Arc::new(AtomicBool::new(true)), ShutdownBoundary::noop())
                .finish(Err(anyhow!("protocol only")))
                .unwrap_err();
        assert_eq!(format!("{protocol_error:#}"), "protocol only");

        let cleanup_error = Coordinator::<String>::new(
            Arc::new(AtomicBool::new(true)),
            ShutdownBoundary {
                shutdown: Arc::new(|| Err(anyhow!("cleanup only"))),
            },
        )
        .finish(Ok(()))
        .unwrap_err();
        assert_eq!(format!("{cleanup_error:#}"), "cleanup only");
    }

    #[test]
    fn main_loop_cleanup_error_does_not_delay_shutdown_response_and_retries_after_exit() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_shutdown = Arc::clone(&attempts);
        let server = Server::with_shutdown(SendOperations, move || {
            if attempts_for_shutdown.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                Err(anyhow!("injected cleanup failure"))
            } else {
                Ok(())
            }
        });
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread = thread::spawn(move || main_loop(server_connection, server));

        send_shutdown_request(&client_connection, 254);
        let shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("cleanup error must not delay shutdown response");

        assert!(response_with_id(shutdown, 254).response_result.is_ok());
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 1);

        send_exit(&client_connection);
        loop_thread.join().unwrap().unwrap();
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2);
    }

    #[test]
    fn main_loop_processes_actions_and_commands_with_correlated_feedback() {
        let fixture = Fixture::new("");
        let uri = fixture.uri();
        let uri_value = serde_json::to_value(&uri).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let operations = RecordingSendOperations {
            calls: Arc::clone(&calls),
        };
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client_connection
            .sender
            .send(Message::Notification(did_open(uri.clone())))
            .unwrap();

        client_connection
            .sender
            .send(Message::Request(code_action_request(130, &uri)))
            .unwrap();
        let action_messages = receive_request_messages(&client_connection, 130, 1);
        let actions = response_with_id(action_messages.into_iter().next().unwrap(), 130);
        assert!(actions.response_result.is_ok());
        assert_eq!(
            actions.response_result.unwrap().as_array().unwrap().len(),
            7
        );

        let mut feedback = Vec::new();
        for (id, command) in [
            (131, PULL_COMMAND),
            (132, PUSH_COMMAND),
            (133, COMPILE_COMMAND),
        ] {
            client_connection
                .sender
                .send(Message::Request(execute_command_request(
                    id,
                    command,
                    vec![uri_value.clone()],
                )))
                .unwrap();
            let messages = receive_request_messages(&client_connection, id, 2);
            for message in messages {
                match message {
                    Message::Response(response) => {
                        assert_eq!(response.id, RequestId::from(id));
                        assert!(response.response_result.is_ok());
                        assert_eq!(response.response_result.unwrap(), serde_json::Value::Null);
                    }
                    Message::Notification(notification)
                        if notification.method == "window/showMessage" =>
                    {
                        feedback.push(
                            serde_json::from_value::<ShowMessageParams>(notification.params)
                                .unwrap(),
                        );
                    }
                    other => panic!("unexpected protocol message: {other:?}"),
                }
            }
        }

        send_shutdown(&client_connection, 134);
        let shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("shutdown response");
        loop_thread.join().unwrap().unwrap();

        assert!(response_with_id(shutdown, 134).response_result.is_ok());
        assert_eq!(feedback.len(), 3);
        assert!(feedback.iter().all(|message| {
            message.typ == MessageType::INFO && message.message.contains("src/nested/hello world.c")
        }));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                Call::PullPrepare {
                    config_path: fixture.config_path.clone(),
                    rel: "src/nested/hello world.c".to_string(),
                    force: false,
                },
                Call::PullApply {
                    rel: "src/nested/hello world.c".to_string(),
                },
                Call::Push {
                    config_path: fixture.config_path.clone(),
                    rel: "src/nested/hello world.c".to_string(),
                    force: false,
                },
                Call::Compile {
                    config_path: fixture.config_path,
                    rel: "src/nested/hello world.c".to_string(),
                },
            ]
        );
    }

    #[test]
    fn main_loop_replies_method_not_found_to_unsupported_requests() {
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(SendOperations)));
        client_connection
            .sender
            .send(Message::Request(Request::new(
                RequestId::from(41),
                "workspace/unsupported".to_string(),
                (),
            )))
            .unwrap();

        let unsupported = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .ok();
        send_shutdown(&client_connection, 42);
        let shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        loop_thread.join().unwrap().unwrap();

        let unsupported = response_with_id(unsupported.expect("unsupported request response"), 41);
        assert_eq!(
            unsupported
                .response_result
                .expect_err("JSON-RPC error")
                .code,
            ErrorCode::MethodNotFound as i32
        );
        assert!(response_with_id(shutdown, 42).response_result.is_ok());
    }

    struct BlockingOperations {
        started: mpsc::SyncSender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
        finished: mpsc::SyncSender<()>,
    }

    struct BlockingNoopPullOperations {
        started: mpsc::SyncSender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
        finished: mpsc::SyncSender<()>,
        apply_attempts: Arc<AtomicUsize>,
        successful_applies: Arc<AtomicUsize>,
    }

    struct IdentityPreparedPull {
        relative_path: String,
        expected_local: LocalIdentity,
    }

    struct IdentityCheckingOperations {
        local_path: PathBuf,
        state_path: PathBuf,
        started: mpsc::SyncSender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
        finished: mpsc::SyncSender<()>,
        apply_attempts: Arc<AtomicUsize>,
        successful_applies: Arc<AtomicUsize>,
    }

    impl FileOperations for IdentityCheckingOperations {
        type PreparedPull = IdentityPreparedPull;

        fn prepare_pull(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<Self::PreparedPull> {
            let prepared = IdentityPreparedPull {
                relative_path: rel.to_string(),
                expected_local: LocalIdentity::capture(&self.local_path)?,
            };
            self.started.send(()).unwrap();
            let (lock, wake) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            self.finished.send(()).unwrap();
            Ok(prepared)
        }

        fn prepare_force_pull(
            &mut self,
            config_path: &Path,
            rel: &str,
        ) -> Result<Self::PreparedPull> {
            self.prepare_pull(config_path, rel, false)
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            request: PullRequest,
        ) -> Result<TransferOutcome> {
            self.apply_attempts.fetch_add(1, AtomicOrdering::SeqCst);
            if LocalIdentity::capture(&self.local_path)? != prepared.expected_local {
                return Err(crate::error::Exit::Conflict(
                    "local file changed while preparing pull".into(),
                )
                .into());
            }
            anyhow::ensure!(request.try_claim(), "cancelled");
            fs::write(&self.local_path, b"remote replacement")?;
            fs::create_dir_all(self.state_path.parent().unwrap())?;
            fs::write(&self.state_path, b"mutated state")?;
            self.successful_applies.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(TransferOutcome::new(
                &prepared.relative_path,
                TransferStatus::Transferred,
            ))
        }

        fn push(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }
    }

    struct ShutdownDuringPullPreparationOperations {
        started: mpsc::SyncSender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
        finished: mpsc::SyncSender<()>,
        stopped: mpsc::SyncSender<()>,
        apply_attempts: Arc<AtomicUsize>,
    }

    impl Drop for ShutdownDuringPullPreparationOperations {
        fn drop(&mut self) {
            let _ = self.stopped.send(());
        }
    }

    impl FileOperations for ShutdownDuringPullPreparationOperations {
        type PreparedPull = String;

        fn prepare_pull(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<Self::PreparedPull> {
            self.started.send(()).unwrap();
            let (lock, wake) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            self.finished.send(()).unwrap();
            Ok(rel.to_string())
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            request: PullRequest,
        ) -> Result<TransferOutcome> {
            self.apply_attempts.fetch_add(1, AtomicOrdering::SeqCst);
            anyhow::ensure!(request.try_claim(), "cancelled");
            Ok(TransferOutcome::new(&prepared, TransferStatus::Transferred))
        }

        fn push(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }
    }

    impl FileOperations for BlockingNoopPullOperations {
        type PreparedPull = String;

        fn prepare_pull(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<Self::PreparedPull> {
            self.started.send(()).unwrap();
            let (lock, wake) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            self.finished.send(()).unwrap();
            Ok(rel.to_string())
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            request: PullRequest,
        ) -> Result<TransferOutcome> {
            self.apply_attempts.fetch_add(1, AtomicOrdering::SeqCst);
            if !request.try_claim() {
                return Err(crate::error::Exit::Conflict("cancelled".into()).into());
            }
            self.successful_applies.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(TransferOutcome::new(&prepared, TransferStatus::Unchanged))
        }

        fn push(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }
    }

    #[test]
    fn pull_edit_during_preparation_cancels_commit() {
        let fixture = Fixture::new("");
        let original_local = fs::read(&fixture.file_path).unwrap();
        let state_path = fixture
            .config_path
            .parent()
            .unwrap()
            .join(crate::names::STATE_DIR)
            .join("state.json");
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let apply_attempts = Arc::new(AtomicUsize::new(0));
        let successful_applies = Arc::new(AtomicUsize::new(0));
        let operations = BlockingNoopPullOperations {
            started: started_tx,
            release: Arc::clone(&release),
            finished: finished_tx,
            apply_attempts: Arc::clone(&apply_attempts),
            successful_applies: Arc::clone(&successful_applies),
        };
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client_connection
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        client_connection
            .sender
            .send(Message::Request(execute_command_request(
                271,
                PULL_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        let response = receive_request_messages(&client_connection, 271, 1);
        assert!(
            response_with_id(response.into_iter().next().unwrap(), 271)
                .response_result
                .is_ok()
        );
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("pull preparation should start");

        client_connection
            .sender
            .send(Message::Notification(did_change(fixture.uri())))
            .unwrap();
        client_connection
            .sender
            .send(Message::Request(code_action_request(272, &fixture.uri())))
            .unwrap();
        let barrier = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("protocol barrier response");
        assert!(response_with_id(barrier, 272).response_result.is_ok());
        release_barrier(&release);
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("pull preparation should finish");
        let feedback = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("pull feedback");

        let warning = match feedback {
            Message::Notification(notification) if notification.method == "window/showMessage" => {
                serde_json::from_value::<ShowMessageParams>(notification.params).unwrap()
            }
            other => panic!("expected one save-and-retry warning, got {other:?}"),
        };
        assert_eq!(warning.typ, MessageType::WARNING);
        assert!(warning.message.contains("save the file and retry"));
        assert_eq!(fs::read(&fixture.file_path).unwrap(), original_local);
        assert!(!state_path.exists());
        assert_eq!(apply_attempts.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(successful_applies.load(AtomicOrdering::SeqCst), 0);
        assert!(client_connection.receiver.try_recv().is_err());

        finish_loop(&client_connection, loop_thread, 273);
    }

    #[test]
    fn force_pull_saved_local_identity_mismatch_after_prompt_is_core_conflict_without_mutation() {
        let fixture = Fixture::new("");
        let state_path = fixture
            .config_path
            .parent()
            .unwrap()
            .join(crate::names::STATE_DIR)
            .join("state.json");
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let apply_attempts = Arc::new(AtomicUsize::new(0));
        let successful_applies = Arc::new(AtomicUsize::new(0));
        let operations = IdentityCheckingOperations {
            local_path: fixture.file_path.clone(),
            state_path: state_path.clone(),
            started: started_tx,
            release: Arc::clone(&release),
            finished: finished_tx,
            apply_attempts: Arc::clone(&apply_attempts),
            successful_applies: Arc::clone(&successful_applies),
        };
        let (server_connection, client) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        client
            .sender
            .send(Message::Request(execute_command_request(
                107,
                FORCE_PULL_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        receive_acknowledgement(&client, 107);
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("Force Pull captures saved local identity before prompt");
        release_barrier(&release);
        finished_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("Force Pull preparation completion");
        let prompt = receive_server_request(&client, "identity-backstop Force Pull prompt");

        fs::write(&fixture.file_path, b"saved disk edit after prompt").unwrap();
        respond_to_force_prompt(
            &client,
            &prompt,
            serde_json::json!({ "title": "Overwrite local file" }),
        );
        let warning = match client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
        {
            Message::Notification(notification) => {
                serde_json::from_value::<ShowMessageParams>(notification.params).unwrap()
            }
            other => panic!("expected identity conflict warning, got {other:?}"),
        };
        assert_eq!(warning.typ, MessageType::WARNING);
        assert!(warning.message.contains("conflict"));
        assert_eq!(
            fs::read(&fixture.file_path).unwrap(),
            b"saved disk edit after prompt",
            "core conflict backstop preserves saved local file"
        );
        assert!(
            !state_path.exists(),
            "core conflict backstop preserves state"
        );
        assert_eq!(apply_attempts.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(successful_applies.load(AtomicOrdering::SeqCst), 0);
        finish_loop(&client, loop_thread, 108);
    }

    #[test]
    fn saved_disk_identity_change_between_prepare_and_apply_is_a_conflict() {
        let fixture = Fixture::new("");
        let state_path = fixture
            .config_path
            .parent()
            .unwrap()
            .join(crate::names::STATE_DIR)
            .join("state.json");
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let apply_attempts = Arc::new(AtomicUsize::new(0));
        let successful_applies = Arc::new(AtomicUsize::new(0));
        let operations = IdentityCheckingOperations {
            local_path: fixture.file_path.clone(),
            state_path: state_path.clone(),
            started: started_tx,
            release: Arc::clone(&release),
            finished: finished_tx,
            apply_attempts: Arc::clone(&apply_attempts),
            successful_applies: Arc::clone(&successful_applies),
        };
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client_connection
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        client_connection
            .sender
            .send(Message::Request(execute_command_request(
                274,
                PULL_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        receive_request_messages(&client_connection, 274, 1);
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("pull preparation should start");
        fs::write(&fixture.file_path, b"saved disk edit").unwrap();
        release_barrier(&release);
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("pull preparation should finish");
        let feedback = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("conflict feedback");
        let warning = match feedback {
            Message::Notification(notification) if notification.method == "window/showMessage" => {
                serde_json::from_value::<ShowMessageParams>(notification.params).unwrap()
            }
            other => panic!("expected conflict warning, got {other:?}"),
        };

        assert_eq!(warning.typ, MessageType::WARNING);
        assert!(warning.message.contains("conflict"));
        assert!(!warning.message.contains("save the file and retry"));
        assert_eq!(fs::read(&fixture.file_path).unwrap(), b"saved disk edit");
        assert!(!state_path.exists());
        assert_eq!(apply_attempts.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(successful_applies.load(AtomicOrdering::SeqCst), 0);
        finish_loop(&client_connection, loop_thread, 275);
    }

    #[test]
    fn shutdown_during_blocked_pull_preparation_skips_apply_and_late_feedback() {
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let (stopped_tx, stopped_rx) = mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let apply_attempts = Arc::new(AtomicUsize::new(0));
        let operations = ShutdownDuringPullPreparationOperations {
            started: started_tx,
            release: Arc::clone(&release),
            finished: finished_tx,
            stopped: stopped_tx,
            apply_attempts: Arc::clone(&apply_attempts),
        };
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client_connection
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("pull preparation should start");

        send_shutdown_request(&client_connection, 276);
        let shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("prompt shutdown response");
        assert!(response_with_id(shutdown, 276).response_result.is_ok());
        release_barrier(&release);
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("pull preparation should finish");
        stopped_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker should stop after preparation");
        assert_eq!(apply_attempts.load(AtomicOrdering::SeqCst), 0);
        assert!(client_connection.receiver.try_recv().is_err());

        send_exit(&client_connection);
        loop_thread.join().unwrap().unwrap();
    }

    impl FileOperations for BlockingOperations {
        type PreparedPull = String;

        fn prepare_pull(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<Self::PreparedPull> {
            self.started.send(()).unwrap();
            let (lock, wake) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            self.finished.send(()).unwrap();
            Ok(rel.to_string())
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            request: PullRequest,
        ) -> Result<TransferOutcome> {
            anyhow::ensure!(request.try_claim(), "cancelled");
            Ok(TransferOutcome::new(&prepared, TransferStatus::Transferred))
        }

        fn push(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }
    }

    struct QueueBlockingOperations {
        calls: Arc<Mutex<Vec<Call>>>,
        started: mpsc::SyncSender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
        finished: mpsc::SyncSender<()>,
        processed: Option<mpsc::SyncSender<()>>,
    }

    struct BlockingCompareOperations {
        snapshots: diff::SharedSnapshotStore,
        launcher: TestDiffLauncher,
        started: mpsc::SyncSender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
        finished: mpsc::SyncSender<()>,
        stopped: Option<mpsc::SyncSender<()>>,
        guard_probe: Option<mpsc::SyncSender<OperationGuard>>,
        fetches: Arc<AtomicUsize>,
        non_compare_calls: Arc<AtomicUsize>,
    }

    impl Drop for BlockingCompareOperations {
        fn drop(&mut self) {
            if let Some(stopped) = &self.stopped {
                let _ = stopped.send(());
            }
        }
    }

    impl FileOperations for BlockingCompareOperations {
        type PreparedPull = String;

        fn prepare_pull(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<Self::PreparedPull> {
            self.non_compare_calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(rel.to_string())
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            request: PullRequest,
        ) -> Result<TransferOutcome> {
            self.non_compare_calls.fetch_add(1, AtomicOrdering::SeqCst);
            anyhow::ensure!(request.try_claim(), "cancelled");
            Ok(TransferOutcome::new(&prepared, TransferStatus::Transferred))
        }

        fn push(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            self.non_compare_calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            self.non_compare_calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }

        fn compare(&mut self, request: CompareRequest) -> Result<CompareOutcome> {
            let CompareRequest {
                config_path,
                relative_path,
                local_path,
                guard,
            } = request;
            if let Some(guard_probe) = &self.guard_probe {
                guard_probe.send(guard.clone()).unwrap();
            }
            let started = self.started.clone();
            let release = Arc::clone(&self.release);
            let finished = self.finished.clone();
            let fetches = Arc::clone(&self.fetches);
            compare_file_with(
                &self.snapshots,
                &mut self.launcher,
                &config_path,
                &relative_path,
                &local_path,
                guard,
                move |_, _| {
                    fetches.fetch_add(1, AtomicOrdering::SeqCst);
                    started.send(()).unwrap();
                    let (lock, wake) = &*release;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = wake.wait(released).unwrap();
                    }
                    finished.send(()).unwrap();
                    Ok(b"remote compare bytes".to_vec())
                },
            )
        }

        fn shutdown_callback(&self) -> Arc<dyn Fn() -> Result<()> + Send + Sync> {
            let shutdown = self.snapshots.shutdown_handle();
            Arc::new(move || shutdown.shutdown())
        }
    }

    fn release_barrier(release: &Arc<(Mutex<bool>, Condvar)>) {
        let (lock, wake) = &**release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }

    #[test]
    fn compare_blocked_retrieval_did_change_cancels_with_one_save_retry_warning() {
        let fixture = Fixture::new("");
        let original_local = fs::read(&fixture.file_path).unwrap();
        let original_config = fs::read(&fixture.config_path).unwrap();
        let snapshots = diff::SharedSnapshotStore::new().unwrap();
        let launcher_calls = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let fetches = Arc::new(AtomicUsize::new(0));
        let non_compare_calls = Arc::new(AtomicUsize::new(0));
        let operations = BlockingCompareOperations {
            snapshots,
            launcher: TestDiffLauncher {
                calls: Arc::clone(&launcher_calls),
                fail: false,
            },
            started: started_tx,
            release: Arc::clone(&release),
            finished: finished_tx,
            stopped: None,
            guard_probe: None,
            fetches: Arc::clone(&fetches),
            non_compare_calls: Arc::clone(&non_compare_calls),
        };
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client_connection
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        client_connection
            .sender
            .send(Message::Request(execute_command_request(
                301,
                COMPARE_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("compare retrieval must block");

        let acknowledgement = client_connection
            .receiver
            .recv_timeout(Duration::from_millis(500))
            .expect("command response must not wait for retrieval");
        let acknowledgement = response_with_id(acknowledgement, 301);
        assert!(acknowledgement.response_result.is_ok());
        assert_eq!(
            acknowledgement.response_result.unwrap(),
            serde_json::Value::Null
        );

        client_connection
            .sender
            .send(Message::Notification(did_change(fixture.uri())))
            .unwrap();
        client_connection
            .sender
            .send(Message::Request(code_action_request(302, &fixture.uri())))
            .unwrap();
        let responsive_action = client_connection
            .receiver
            .recv_timeout(Duration::from_millis(500))
            .expect("code action proves didChange was processed while retrieval blocked");
        let responsive_action = response_with_id(responsive_action, 302);
        assert_eq!(
            responsive_action
                .response_result
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            7
        );

        release_barrier(&release);
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("retrieval must finish after release");
        let feedback = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("cancelled Compare feedback");
        let feedback = match feedback {
            Message::Notification(notification) if notification.method == "window/showMessage" => {
                serde_json::from_value::<ShowMessageParams>(notification.params).unwrap()
            }
            other => panic!("expected Compare warning, got {other:?}"),
        };
        assert_eq!(feedback.typ, MessageType::WARNING);
        assert_eq!(
            feedback.message,
            "ferry: src/nested/hello world.c: save the file and retry"
        );
        assert!(client_connection.receiver.try_recv().is_err());
        assert_eq!(fetches.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(non_compare_calls.load(AtomicOrdering::SeqCst), 0);
        assert!(launcher_calls.lock().unwrap().is_empty());
        assert_eq!(fs::read(&fixture.file_path).unwrap(), original_local);
        assert_eq!(fs::read(&fixture.config_path).unwrap(), original_config);

        finish_loop(&client_connection, loop_thread, 303);
    }

    #[test]
    fn compare_shutdown_while_retrieval_blocked_cleans_root_and_prevents_late_launch() {
        let fixture = Fixture::new("");
        let snapshots = diff::SharedSnapshotStore::new().unwrap();
        let root = snapshots.root_path().unwrap();
        let snapshot_shutdown = snapshots.shutdown_handle();
        let launcher_calls = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let (stopped_tx, stopped_rx) = mpsc::sync_channel(1);
        let (guard_tx, guard_rx) = mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let fetches = Arc::new(AtomicUsize::new(0));
        let non_compare_calls = Arc::new(AtomicUsize::new(0));
        let operations = BlockingCompareOperations {
            snapshots,
            launcher: TestDiffLauncher {
                calls: Arc::clone(&launcher_calls),
                fail: false,
            },
            started: started_tx,
            release: Arc::clone(&release),
            finished: finished_tx,
            stopped: Some(stopped_tx),
            guard_probe: Some(guard_tx),
            fetches: Arc::clone(&fetches),
            non_compare_calls: Arc::clone(&non_compare_calls),
        };
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client_connection
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        client_connection
            .sender
            .send(Message::Request(execute_command_request(
                311,
                COMPARE_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("compare retrieval must block");
        let guard = guard_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("Compare must expose its still-pending guard to the test");
        let acknowledgement = client_connection
            .receiver
            .recv_timeout(Duration::from_millis(500))
            .expect("prompt Compare acknowledgement");
        assert!(
            response_with_id(acknowledgement, 311)
                .response_result
                .is_ok()
        );

        send_shutdown_request(&client_connection, 312);
        let shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("shutdown response while exit is withheld");
        assert!(response_with_id(shutdown, 312).response_result.is_ok());
        assert!(!guard.try_claim(), "shutdown must cancel the pending guard");
        assert!(snapshot_shutdown.is_closed().unwrap());
        assert!(
            !root.exists(),
            "shutdown must remove the exact snapshot root"
        );

        release_barrier(&release);
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("retrieval must finish after release");
        stopped_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker must stop after blocked retrieval returns");
        assert_eq!(fetches.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(non_compare_calls.load(AtomicOrdering::SeqCst), 0);
        assert!(launcher_calls.lock().unwrap().is_empty());
        assert!(
            client_connection.receiver.try_recv().is_err(),
            "released worker must not emit late feedback"
        );

        send_exit(&client_connection);
        loop_thread.join().unwrap().unwrap();
    }

    #[test]
    fn compare_success_feedback_retains_snapshot_until_real_protocol_shutdown() {
        let fixture = Fixture::new("");
        let original_local = fs::read(&fixture.file_path).unwrap();
        let original_config = fs::read(&fixture.config_path).unwrap();
        let state_path = fixture
            .config_path
            .parent()
            .unwrap()
            .join(crate::names::STATE_DIR);
        let snapshots = diff::SharedSnapshotStore::new().unwrap();
        let root = snapshots.root_path().unwrap();
        let launcher_calls = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, _started_rx) = mpsc::sync_channel(1);
        let (finished_tx, _finished_rx) = mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(true), Condvar::new()));
        let fetches = Arc::new(AtomicUsize::new(0));
        let non_compare_calls = Arc::new(AtomicUsize::new(0));
        let operations = BlockingCompareOperations {
            snapshots,
            launcher: TestDiffLauncher {
                calls: Arc::clone(&launcher_calls),
                fail: false,
            },
            started: started_tx,
            release,
            finished: finished_tx,
            stopped: None,
            guard_probe: None,
            fetches: Arc::clone(&fetches),
            non_compare_calls: Arc::clone(&non_compare_calls),
        };
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client_connection
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        client_connection
            .sender
            .send(Message::Request(execute_command_request(
                321,
                COMPARE_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        let messages = receive_request_messages(&client_connection, 321, 2);
        let mut info = None;
        for message in messages {
            match message {
                Message::Response(response) => {
                    assert!(response.response_result.is_ok());
                    assert_eq!(response.response_result.unwrap(), serde_json::Value::Null);
                }
                Message::Notification(notification)
                    if notification.method == "window/showMessage" =>
                {
                    assert!(
                        info.replace(
                            serde_json::from_value::<ShowMessageParams>(notification.params)
                                .unwrap()
                        )
                        .is_none()
                    );
                }
                other => panic!("unexpected Compare message: {other:?}"),
            }
        }
        let info = info.expect("Compare success feedback");
        assert_eq!(info.typ, MessageType::INFO);
        assert_eq!(
            info.message,
            "ferry: src/nested/hello world.c: opened native diff"
        );
        let launched = launcher_calls.lock().unwrap().clone();
        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].0, fixture.file_path);
        assert!(launched[0].1.exists(), "snapshot retained after launch");
        assert_eq!(fetches.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(non_compare_calls.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(fs::read(&fixture.file_path).unwrap(), original_local);
        assert_eq!(fs::read(&fixture.config_path).unwrap(), original_config);
        assert!(!state_path.exists(), "Compare must not create Ferry state");

        send_shutdown_request(&client_connection, 322);
        let shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("shutdown response while exit withheld");
        assert!(response_with_id(shutdown, 322).response_result.is_ok());
        assert!(!launched[0].1.exists());
        assert!(!root.exists());
        send_exit(&client_connection);
        loop_thread.join().unwrap().unwrap();
    }

    #[derive(Clone, Copy)]
    enum CompareFailureStage {
        Retrieval,
        SnapshotCreation,
        Launcher,
    }

    struct FailingCompareOperations {
        snapshots: diff::SharedSnapshotStore,
        launcher: TestDiffLauncher,
        stage: CompareFailureStage,
    }

    impl FileOperations for FailingCompareOperations {
        type PreparedPull = String;

        fn prepare_pull(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<Self::PreparedPull> {
            Ok(rel.to_string())
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            request: PullRequest,
        ) -> Result<TransferOutcome> {
            anyhow::ensure!(request.try_claim(), "cancelled");
            Ok(TransferOutcome::new(&prepared, TransferStatus::Transferred))
        }

        fn push(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }

        fn compare(&mut self, request: CompareRequest) -> Result<CompareOutcome> {
            let CompareRequest {
                config_path,
                relative_path,
                local_path,
                guard,
            } = request;
            let stage = self.stage;
            compare_file_with(
                &self.snapshots,
                &mut self.launcher,
                &config_path,
                &relative_path,
                &local_path,
                guard,
                move |_, _| match stage {
                    CompareFailureStage::Retrieval => {
                        Err(anyhow!("{REVIEW_SECRET} retrieval sentinel"))
                    }
                    CompareFailureStage::SnapshotCreation | CompareFailureStage::Launcher => {
                        Ok(b"remote bytes".to_vec())
                    }
                },
            )
        }

        fn shutdown_callback(&self) -> Arc<dyn Fn() -> Result<()> + Send + Sync> {
            let shutdown = self.snapshots.shutdown_handle();
            Arc::new(move || shutdown.shutdown())
        }
    }

    #[test]
    fn compare_runtime_failures_each_emit_one_safe_warning() {
        for stage in [
            CompareFailureStage::Retrieval,
            CompareFailureStage::SnapshotCreation,
            CompareFailureStage::Launcher,
        ] {
            let fixture = Fixture::new("");
            let original_local = fs::read(&fixture.file_path).unwrap();
            let original_config = fs::read(&fixture.config_path).unwrap();
            let state_path = fixture
                .config_path
                .parent()
                .unwrap()
                .join(crate::names::STATE_DIR);
            let snapshots = diff::SharedSnapshotStore::new().unwrap();
            if matches!(stage, CompareFailureStage::SnapshotCreation) {
                snapshots.shutdown_handle().shutdown().unwrap();
            }
            let operations = FailingCompareOperations {
                snapshots,
                launcher: TestDiffLauncher {
                    calls: Arc::new(Mutex::new(Vec::new())),
                    fail: matches!(stage, CompareFailureStage::Launcher),
                },
                stage,
            };
            let (server_connection, client_connection) = Connection::memory();
            let loop_thread =
                thread::spawn(move || main_loop(server_connection, Server::new(operations)));
            client_connection
                .sender
                .send(Message::Notification(did_open(fixture.uri())))
                .unwrap();
            client_connection
                .sender
                .send(Message::Request(execute_command_request(
                    331,
                    COMPARE_COMMAND,
                    vec![serde_json::to_value(fixture.uri()).unwrap()],
                )))
                .unwrap();

            let messages = receive_request_messages(&client_connection, 331, 2);
            let warnings = messages
                .into_iter()
                .filter_map(|message| match message {
                    Message::Response(response) => {
                        assert!(response.response_result.is_ok());
                        assert_eq!(response.response_result.unwrap(), serde_json::Value::Null);
                        None
                    }
                    Message::Notification(notification)
                        if notification.method == "window/showMessage" =>
                    {
                        Some(
                            serde_json::from_value::<ShowMessageParams>(notification.params)
                                .unwrap(),
                        )
                    }
                    other => panic!("unexpected Compare failure message: {other:?}"),
                })
                .collect::<Vec<_>>();
            assert_eq!(warnings.len(), 1);
            assert_eq!(warnings[0].typ, MessageType::WARNING);
            assert_eq!(
                warnings[0].message,
                "ferry: src/nested/hello world.c: operation failed; run a Ferry task for details"
            );
            assert!(!warnings[0].message.contains(REVIEW_SECRET));
            assert!(!warnings[0].message.contains("sentinel"));
            assert_eq!(fs::read(&fixture.file_path).unwrap(), original_local);
            assert_eq!(fs::read(&fixture.config_path).unwrap(), original_config);
            assert!(!state_path.exists());

            finish_loop(&client_connection, loop_thread, 332);
        }
    }

    impl FileOperations for QueueBlockingOperations {
        type PreparedPull = String;

        fn prepare_pull(
            &mut self,
            config_path: &Path,
            rel: &str,
            force: bool,
        ) -> Result<Self::PreparedPull> {
            let is_first = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(Call::PullPrepare {
                    config_path: config_path.to_path_buf(),
                    rel: rel.to_string(),
                    force,
                });
                calls.len() == 1
            };
            if is_first {
                self.started.send(()).unwrap();
                let (lock, wake) = &*self.release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = wake.wait(released).unwrap();
                }
                self.finished.send(()).unwrap();
            }
            Ok(rel.to_string())
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            request: PullRequest,
        ) -> Result<TransferOutcome> {
            self.calls.lock().unwrap().push(Call::PullApply {
                rel: prepared.clone(),
            });
            anyhow::ensure!(request.try_claim(), "cancelled");
            Ok(TransferOutcome::new(&prepared, TransferStatus::Transferred))
        }

        fn push(&mut self, config_path: &Path, rel: &str, force: bool) -> Result<TransferOutcome> {
            self.calls.lock().unwrap().push(Call::Push {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
                force,
            });
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn compile(&mut self, config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            self.calls.lock().unwrap().push(Call::Compile {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
            });
            if let Some(processed) = &self.processed {
                processed.send(()).unwrap();
            }
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }
    }

    #[test]
    fn dirty_document_shutdown_cancels_queued_pull_and_closes_snapshot_boundary_before_exit() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let (stopped_tx, stopped_rx) = mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let operations = ShutdownBlockingOperations {
            calls: Arc::clone(&calls),
            started: started_tx,
            release: Arc::clone(&release),
            finished: finished_tx,
            stopped: stopped_tx,
        };
        let shutdown_count = Arc::new(AtomicUsize::new(0));
        let shutdown_count_for_loop = Arc::clone(&shutdown_count);
        let (snapshot_closed_tx, snapshot_closed_rx) = mpsc::sync_channel(1);
        let server = Server::with_shutdown(operations, move || {
            shutdown_count_for_loop.fetch_add(1, AtomicOrdering::SeqCst);
            snapshot_closed_tx.send(()).unwrap();
            Ok(())
        });
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread = thread::spawn(move || main_loop(server_connection, server));
        client_connection
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        client_connection
            .sender
            .send(Message::Request(execute_command_request(
                251,
                PUSH_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        receive_request_messages(&client_connection, 251, 1);
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("preparation blocker should start");
        client_connection
            .sender
            .send(Message::Request(execute_command_request(
                252,
                PULL_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        receive_request_messages(&client_connection, 252, 1);

        send_shutdown_request(&client_connection, 253);
        let shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("shutdown response must arrive before exit");

        assert!(response_with_id(shutdown, 253).response_result.is_ok());
        snapshot_closed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("snapshot boundary must close during shutdown handshake");
        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocked preparation should be released");
        stopped_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker should exit after the blocked operation returns");
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[Call::Push {
                config_path: fixture.config_path.clone(),
                rel: "src/nested/hello world.c".to_string(),
                force: false,
            }],
            "cancelled queued pull must never launch"
        );
        assert!(
            client_connection.receiver.try_recv().is_err(),
            "released worker must not write after shutdown begins"
        );
        assert_eq!(shutdown_count.load(AtomicOrdering::SeqCst), 1);

        send_exit(&client_connection);
        loop_thread.join().unwrap().unwrap();
        assert_eq!(shutdown_count.load(AtomicOrdering::SeqCst), 1);
    }

    struct ShutdownBlockingOperations {
        calls: Arc<Mutex<Vec<Call>>>,
        started: mpsc::SyncSender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
        finished: mpsc::SyncSender<()>,
        stopped: mpsc::SyncSender<()>,
    }

    impl Drop for ShutdownBlockingOperations {
        fn drop(&mut self) {
            let _ = self.stopped.send(());
        }
    }

    impl FileOperations for ShutdownBlockingOperations {
        type PreparedPull = String;

        fn prepare_pull(
            &mut self,
            config_path: &Path,
            rel: &str,
            force: bool,
        ) -> Result<Self::PreparedPull> {
            self.calls.lock().unwrap().push(Call::PullPrepare {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
                force,
            });
            Ok(rel.to_string())
        }

        fn apply_pull(
            &mut self,
            prepared: Self::PreparedPull,
            request: PullRequest,
        ) -> Result<TransferOutcome> {
            self.calls.lock().unwrap().push(Call::PullApply {
                rel: prepared.clone(),
            });
            anyhow::ensure!(request.try_claim(), "cancelled");
            Ok(TransferOutcome::new(&prepared, TransferStatus::Transferred))
        }

        fn push(&mut self, config_path: &Path, rel: &str, force: bool) -> Result<TransferOutcome> {
            self.calls.lock().unwrap().push(Call::Push {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
                force,
            });
            self.started.send(()).unwrap();
            let (lock, wake) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            self.finished.send(()).unwrap();
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }
    }

    #[test]
    fn main_loop_shutdown_does_not_wait_for_blocked_file_operation() {
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let operations = BlockingOperations {
            started: started_tx,
            release: Arc::clone(&release),
            finished: finished_tx,
        };
        let (server_connection, client_connection) = Connection::memory();
        let (loop_done_tx, loop_done_rx) = mpsc::sync_channel(1);
        let loop_thread = thread::spawn(move || {
            let result = main_loop(server_connection, Server::new(operations));
            loop_done_tx.send(()).unwrap();
            result
        });
        client_connection
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("pull should start");

        send_shutdown(&client_connection, 51);
        let prompt_shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .ok();
        let prompt_loop_exit = loop_done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        let writer_disconnected = prompt_loop_exit
            && client_connection
                .receiver
                .try_recv()
                .expect_err("worker must not retain the LSP sender")
                .is_disconnected();

        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocked operation should be released");
        loop_thread.join().unwrap().unwrap();

        let shutdown = response_with_id(
            prompt_shutdown.expect("shutdown response must not wait for file operation"),
            51,
        );
        assert!(shutdown.response_result.is_ok());
        assert!(prompt_loop_exit, "main loop should terminate promptly");
        assert!(writer_disconnected, "worker must not retain the LSP sender");
    }

    #[test]
    fn code_action_remains_responsive_while_transfer_worker_is_blocked() {
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let operations = BlockingOperations {
            started: started_tx,
            release: Arc::clone(&release),
            finished: finished_tx,
        };
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client_connection
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("pull should start");
        client_connection
            .sender
            .send(Message::Request(code_action_request(141, &fixture.uri())))
            .unwrap();

        let prompt_action = client_connection
            .receiver
            .recv_timeout(Duration::from_millis(500))
            .ok();
        send_shutdown(&client_connection, 142);
        let prompt_shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("shutdown response");
        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocked operation should be released");
        loop_thread.join().unwrap().unwrap();

        let action = response_with_id(
            prompt_action.expect("code action must not wait for transfer worker"),
            141,
        );
        assert!(action.response_result.is_ok());
        assert_eq!(action.response_result.unwrap().as_array().unwrap().len(), 7);
        assert!(
            response_with_id(prompt_shutdown, 142)
                .response_result
                .is_ok()
        );
    }

    #[test]
    fn execute_command_responds_before_blocked_operation_and_shutdown() {
        let fixture = Fixture::new("");
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let operations = BlockingOperations {
            started: started_tx,
            release: Arc::clone(&release),
            finished: finished_tx,
        };
        let (server_connection, client_connection) = Connection::memory();
        let (loop_done_tx, loop_done_rx) = mpsc::sync_channel(1);
        let loop_thread = thread::spawn(move || {
            let result = main_loop(server_connection, Server::new(operations));
            loop_done_tx.send(()).unwrap();
            result
        });
        client_connection
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        client_connection
            .sender
            .send(Message::Request(execute_command_request(
                151,
                PULL_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("manual pull should start");

        let prompt_command = client_connection
            .receiver
            .recv_timeout(Duration::from_millis(500))
            .ok();
        send_shutdown(&client_connection, 152);
        let prompt_shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("shutdown response");
        let prompt_loop_exit = loop_done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        let writer_disconnected = prompt_loop_exit
            && client_connection
                .receiver
                .try_recv()
                .expect_err("worker must not retain the LSP sender")
                .is_disconnected();

        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocked operation should be released");
        loop_thread.join().unwrap().unwrap();

        let command = response_with_id(
            prompt_command.expect("command response must precede blocked operation"),
            151,
        );
        assert!(command.response_result.is_ok());
        assert_eq!(command.response_result.unwrap(), serde_json::Value::Null);
        assert!(
            response_with_id(prompt_shutdown, 152)
                .response_result
                .is_ok()
        );
        assert!(prompt_loop_exit, "main loop should terminate promptly");
        assert!(writer_disconnected, "worker must not retain the LSP sender");
    }

    #[test]
    fn queued_command_re_resolves_after_project_roots_change() {
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let operations = QueueBlockingOperations {
            calls: Arc::clone(&calls),
            started: started_tx,
            release: Arc::clone(&release),
            finished: finished_tx,
            processed: None,
        };
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client_connection
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("automatic pull should block first");
        client_connection
            .sender
            .send(Message::Request(execute_command_request(
                161,
                PULL_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        let prompt_command = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .ok();

        let other_root = fixture.config_path.parent().unwrap().join("other");
        fs::create_dir(&other_root).unwrap();
        fixture.set_raw_config(
            "[connection]\nhost = \"example.invalid\"\nuser = \"u\"\npassword = \"p\"\n\
             [paths]\nlocal_root = \"other\"\nremote_root = \"/changed\"\n",
        );
        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocked operation should be released");
        let eventual_feedback = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .ok();
        let no_duplicate = client_connection.receiver.try_recv().is_err();
        send_shutdown(&client_connection, 162);
        let shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("shutdown response");
        loop_thread.join().unwrap().unwrap();

        let command = response_with_id(prompt_command.expect("prompt command response"), 161);
        assert!(command.response_result.is_ok());
        assert_eq!(command.response_result.unwrap(), serde_json::Value::Null);
        let warning = match eventual_feedback.expect("safe stale-command warning") {
            Message::Notification(notification) if notification.method == "window/showMessage" => {
                serde_json::from_value::<ShowMessageParams>(notification.params).unwrap()
            }
            other => panic!("expected warning feedback, got {other:?}"),
        };
        assert_eq!(warning.typ, MessageType::WARNING);
        assert!(warning.message.contains("src/nested/hello world.c"));
        assert!(!warning.message.contains("/changed"));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[
                Call::PullPrepare {
                    config_path: fixture.config_path.clone(),
                    rel: "src/nested/hello world.c".to_string(),
                    force: false,
                },
                Call::PullApply {
                    rel: "src/nested/hello world.c".to_string(),
                },
            ],
            "stale target must not run",
        );
        assert!(no_duplicate, "command must receive exactly one response");
        assert!(response_with_id(shutdown, 162).response_result.is_ok());
    }

    #[test]
    fn queued_open_re_resolves_after_project_roots_change() {
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
        let second_path = fixture
            .config_path
            .parent()
            .unwrap()
            .join("src/nested/second.c");
        fs::write(&second_path, b"second\n").unwrap();
        let second_uri = Uri::from_str(&format!("file://{}", second_path.display())).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let (processed_tx, processed_rx) = mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let operations = QueueBlockingOperations {
            calls: Arc::clone(&calls),
            started: started_tx,
            release: Arc::clone(&release),
            finished: finished_tx,
            processed: Some(processed_tx),
        };
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client_connection
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first automatic pull should block");
        client_connection
            .sender
            .send(Message::Notification(did_open_with_text(
                second_uri.clone(),
                "second\n",
            )))
            .unwrap();
        client_connection
            .sender
            .send(Message::Request(code_action_request(163, &second_uri)))
            .unwrap();
        let queued = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("queued-open protocol barrier");
        assert!(response_with_id(queued, 163).response_result.is_ok());

        let other_root = fixture.config_path.parent().unwrap().join("other");
        fs::create_dir(&other_root).unwrap();
        let probe_path = other_root.join("probe.c");
        fs::write(&probe_path, b"probe\n").unwrap();
        let probe_uri = Uri::from_str(&format!("file://{}", probe_path.display())).unwrap();
        fixture.set_raw_config(
            "[connection]\nhost = \"example.invalid\"\nuser = \"u\"\npassword = \"p\"\n\
             [paths]\nlocal_root = \"other\"\nremote_root = \"/changed\"\n",
        );
        client_connection
            .sender
            .send(Message::Request(execute_command_request(
                164,
                COMPILE_COMMAND,
                vec![serde_json::to_value(probe_uri).unwrap()],
            )))
            .unwrap();
        let compile_response = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("compile acknowledgement");
        assert!(
            response_with_id(compile_response, 164)
                .response_result
                .is_ok()
        );
        release_barrier(&release);
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first preparation should finish");
        processed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("compile barrier after queued open");
        let feedback = (0..2)
            .map(|_| {
                client_connection
                    .receiver
                    .recv_timeout(Duration::from_secs(1))
                    .expect("queued open and compile feedback")
            })
            .collect::<Vec<_>>();

        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[
                Call::PullPrepare {
                    config_path: fixture.config_path.clone(),
                    rel: "src/nested/hello world.c".to_string(),
                    force: false,
                },
                Call::PullApply {
                    rel: "src/nested/hello world.c".to_string(),
                },
                Call::Compile {
                    config_path: fixture.config_path.clone(),
                    rel: "probe.c".to_string(),
                },
            ],
            "queued open must not prepare its stale target",
        );
        assert!(feedback.iter().all(|message| matches!(
            message,
            Message::Notification(notification) if notification.method == "window/showMessage"
        )));
        finish_loop(&client_connection, loop_thread, 165);
    }

    #[test]
    fn uri_to_path_roundtrip_simple() {
        assert_eq!(
            uri_to_path("file:///home/user/foo.c"),
            Some(PathBuf::from("/home/user/foo.c"))
        );
    }

    #[test]
    fn uri_to_path_percent_decodes_spaces() {
        assert_eq!(
            uri_to_path("file:///home/user/my%20file.c"),
            Some(PathBuf::from("/home/user/my file.c"))
        );
    }

    #[test]
    fn uri_to_path_rejects_non_file_scheme() {
        assert!(uri_to_path("http://example.com/foo").is_none());
    }

    #[test]
    fn uri_to_path_handles_localhost_authority() {
        assert_eq!(
            uri_to_path("file://localhost/home/user/foo.c"),
            Some(PathBuf::from("/home/user/foo.c"))
        );
    }

    struct ScopeFinalValidationRemote {
        list_calls: usize,
        reached: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
        events: Vec<String>,
    }

    impl crate::ftp::Remote for ScopeFinalValidationRemote {
        fn list_dir(&mut self, _dir: &str) -> anyhow::Result<Vec<crate::ftp::Entry>> {
            anyhow::bail!("tolerant LIST must not be used")
        }

        fn file_size(&mut self, path: &str) -> anyhow::Result<u64> {
            anyhow::bail!("unexpected file size probe for {path}")
        }

        fn exact_file_presence(
            &mut self,
            _path: &str,
        ) -> anyhow::Result<crate::ftp::ExactFilePresence> {
            Ok(crate::ftp::ExactFilePresence::Missing)
        }
    }

    impl crate::ftp::StrictRemote for ScopeFinalValidationRemote {
        fn list_dir_strict(&mut self, dir: &str) -> anyhow::Result<Vec<crate::ftp::Entry>> {
            self.events.push(format!("list {dir}"));
            self.list_calls += 1;
            if self.list_calls == 3 {
                self.reached
                    .send(())
                    .map_err(|_| anyhow!("final-validation receiver disappeared"))?;
                self.release
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(|_| anyhow!("timed out releasing final validation"))?;
            }
            Ok(Vec::new())
        }
    }

    impl crate::commands::remote_hash::RemoteFileRetrieval for ScopeFinalValidationRemote {
        fn mtime(&mut self, path: &str) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
            anyhow::bail!("unexpected remote mtime probe for {path}")
        }

        fn size(&mut self, path: &str) -> anyhow::Result<u64> {
            anyhow::bail!("unexpected remote size probe for {path}")
        }

        fn download(&mut self, path: &str) -> anyhow::Result<Vec<u8>> {
            anyhow::bail!("unexpected remote download for {path}")
        }
    }

    impl crate::commands::file_transfer::RemoteWrite for ScopeFinalValidationRemote {
        fn upload_bytes(&mut self, path: &str, _bytes: &[u8]) -> anyhow::Result<()> {
            self.events.push(format!("upload {path}"));
            Ok(())
        }

        fn rename(&mut self, from: &str, to: &str) -> anyhow::Result<()> {
            self.events.push(format!("rename {from} {to}"));
            Ok(())
        }

        fn rm(&mut self, path: &str) -> anyhow::Result<()> {
            self.events.push(format!("rm {path}"));
            Ok(())
        }

        fn mkdir(&mut self, path: &str) -> anyhow::Result<()> {
            self.events.push(format!("mkdir {path}"));
            Ok(())
        }

        fn mkdir_scoped_strict(&mut self, path: &str) -> anyhow::Result<()> {
            self.events.push(format!("mkdir_strict {path}"));
            Ok(())
        }

        fn mtime(&mut self, path: &str) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
            anyhow::bail!("unexpected exact remote mtime probe for {path}")
        }

        fn destination_snapshot(
            &mut self,
            _remote_root: &str,
            path: &str,
        ) -> anyhow::Result<crate::commands::file_transfer::RemoteDestinationSnapshot> {
            self.events.push(format!("snapshot {path}"));
            Ok(crate::commands::file_transfer::RemoteDestinationSnapshot::Missing)
        }
    }

    #[test]
    fn scope_commit_empty_final_validation_invalidation_emits_retry_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        let mut tracker = document_state::DocumentTracker::default();
        let guard = tracker
            .begin_clean_scope(document_state::DocumentScope::Directory(
                root.path().to_path_buf(),
            ))
            .unwrap();
        let (reached_tx, reached_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let worker_root = root.path().to_path_buf();

        let worker = thread::spawn(move || {
            let mut remote = ScopeFinalValidationRemote {
                list_calls: 0,
                reached: reached_tx,
                release: release_rx,
                events: Vec::new(),
            };
            let mut state = crate::state::StateFile::default();
            let matcher = crate::ignored::Matcher::new(&[], &worker_root).unwrap();
            let outcome = crate::commands::sync::run_scoped_with_for_test(
                &mut remote,
                &mut state,
                &worker_root,
                "/remote",
                &matcher,
                crate::commands::sync::scope::SyncScope::RootDirectory,
                false,
                crate::commands::ExecutionMode::Apply,
                &guard,
            )
            .unwrap();
            (outcome, state, remote)
        });

        reached_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("empty scope reached final validation");
        tracker.cancel_all();
        release_tx.send(()).unwrap();

        let (outcome, state, remote) = worker.join().unwrap();
        assert!(outcome.cancelled);
        assert!(outcome.events.is_empty());
        assert!(outcome.issues.is_empty());
        assert_eq!(state, crate::state::StateFile::default());
        assert!(scope_local_transfer_temps(root.path()).is_empty());
        assert_eq!(
            remote.events,
            vec![
                "list /remote".to_string(),
                "list /remote".to_string(),
                "list /remote".to_string(),
            ],
            "final validation must remain read-only"
        );

        let message =
            scope_cancellation_feedback(&outcome).expect("cancelled scopes must emit feedback");
        let feedback = match message {
            Message::Notification(notification) if notification.method == "window/showMessage" => {
                serde_json::from_value::<ShowMessageParams>(notification.params).unwrap()
            }
            other => panic!("expected cancellation warning, got {other:?}"),
        };
        assert_eq!(feedback.typ, MessageType::WARNING);
        assert_eq!(
            feedback.message,
            "ferry: folder changed in Zed; save all files and retry"
        );
        assert!(
            scope_cancellation_feedback(&crate::commands::sync::SyncOutcome::default()).is_none()
        );
    }

    #[derive(Clone, Copy, Debug)]
    enum ScopeInvalidation {
        Change,
        Save,
        CancelAll,
    }

    impl ScopeInvalidation {
        fn apply(self, tracker: &mut document_state::DocumentTracker, path: &Path) {
            match self {
                Self::Change => tracker.change(path),
                Self::Save => tracker.save(path),
                Self::CancelAll => tracker.cancel_all(),
            }
        }
    }

    struct ScopePauseGate {
        inner: document_state::ScopeOperationGuard,
        staged: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl crate::commands::sync::CommitGate for ScopePauseGate {
        fn is_current(&self) -> bool {
            crate::commands::sync::CommitGate::is_current(&self.inner)
        }

        fn commit(
            &self,
            mutation: &mut dyn FnMut() -> anyhow::Result<()>,
        ) -> anyhow::Result<crate::commands::sync::CommitDecision> {
            self.staged
                .send(())
                .map_err(|_| anyhow!("staging probe receiver disappeared"))?;
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| anyhow!("timed out releasing staged transfer"))?;
            crate::commands::sync::CommitGate::commit(&self.inner, mutation)
        }
    }

    fn scope_remote_hash(bytes: &[u8]) -> crate::commands::remote_hash::RemoteHash {
        crate::commands::remote_hash::RemoteHash {
            sha256: crate::hash::hash_bytes(bytes),
            size: bytes.len() as u64,
            mtime: chrono::Utc::now(),
            from_cache: false,
            metadata_stable: true,
            bytes: Some(bytes.to_vec()),
            pre_download: None,
        }
    }

    fn scope_local_transfer_temps(directory: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(directory)
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(crate::commands::transfer_temp::is_reserved_local_transfer_temp)
                    .then_some(path)
            })
            .collect()
    }

    #[test]
    fn scope_commit_local_post_staging_change_save_and_shutdown_deny_install() {
        for invalidation in [
            ScopeInvalidation::Change,
            ScopeInvalidation::Save,
            ScopeInvalidation::CancelAll,
        ] {
            let root = tempfile::tempdir().unwrap();
            let destination = root.path().join("file.c");
            std::fs::write(&destination, b"old local").unwrap();
            let expected =
                crate::commands::pull::ExpectedLocalDestination::capture(root.path(), &destination)
                    .unwrap();
            let remote = scope_remote_hash(b"new remote");
            let mut tracker = document_state::DocumentTracker::default();
            tracker.open(destination.clone(), "old local").unwrap();
            let guard = tracker
                .begin_clean_scope(document_state::DocumentScope::Exact(destination.clone()))
                .unwrap();
            let (staged_tx, staged_rx) = mpsc::sync_channel(1);
            let (release_tx, release_rx) = mpsc::sync_channel(1);
            let gate = ScopePauseGate {
                inner: guard,
                staged: staged_tx,
                release: Mutex::new(release_rx),
            };
            let worker_path = destination.clone();

            let worker = thread::spawn(move || {
                let mut state = crate::state::StateFile::default();
                let decision = crate::commands::pull::download_one_guarded(
                    &mut state,
                    &worker_path,
                    "file.c",
                    &remote,
                    &expected,
                    crate::commands::ExecutionMode::Apply,
                    &gate,
                )
                .unwrap();
                (decision, state)
            });

            staged_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("local replacement staged");
            assert_eq!(scope_local_transfer_temps(root.path()).len(), 1);
            invalidation.apply(&mut tracker, &destination);
            release_tx.send(()).unwrap();

            let (decision, state) = worker.join().unwrap();
            assert_eq!(
                decision,
                crate::commands::sync::CommitDecision::Cancelled,
                "{invalidation:?}"
            );
            assert_eq!(std::fs::read(&destination).unwrap(), b"old local");
            assert!(state.files.is_empty());
            assert!(scope_local_transfer_temps(root.path()).is_empty());
        }
    }

    #[derive(Clone)]
    struct ScopeRemoteFile {
        bytes: Vec<u8>,
        modified: chrono::DateTime<chrono::Utc>,
    }

    #[derive(Default)]
    struct ScopeTestRemote {
        files: std::collections::BTreeMap<String, ScopeRemoteFile>,
        events: Vec<String>,
    }

    impl crate::commands::file_transfer::RemoteWrite for ScopeTestRemote {
        fn upload_bytes(&mut self, path: &str, bytes: &[u8]) -> anyhow::Result<()> {
            self.events.push(format!("upload {path}"));
            self.files.insert(
                path.to_string(),
                ScopeRemoteFile {
                    bytes: bytes.to_vec(),
                    modified: chrono::Utc::now(),
                },
            );
            Ok(())
        }

        fn rename(&mut self, from: &str, to: &str) -> anyhow::Result<()> {
            self.events.push(format!("rename {from} {to}"));
            let file = self
                .files
                .remove(from)
                .ok_or_else(|| anyhow!("missing remote rename source"))?;
            self.files.insert(to.to_string(), file);
            Ok(())
        }

        fn rm(&mut self, path: &str) -> anyhow::Result<()> {
            self.events.push(format!("rm {path}"));
            self.files.remove(path);
            Ok(())
        }

        fn mkdir(&mut self, _path: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn mkdir_scoped_strict(&mut self, _path: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn mtime(&mut self, path: &str) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
            self.files
                .get(path)
                .map(|file| file.modified)
                .ok_or_else(|| anyhow!("missing remote mtime target"))
        }

        fn destination_snapshot(
            &mut self,
            _remote_root: &str,
            path: &str,
        ) -> anyhow::Result<crate::commands::file_transfer::RemoteDestinationSnapshot> {
            Ok(self.files.get(path).map_or(
                crate::commands::file_transfer::RemoteDestinationSnapshot::Missing,
                |file| crate::commands::file_transfer::RemoteDestinationSnapshot::File {
                    size: file.bytes.len() as u64,
                    modified: file.modified,
                    sha256: crate::hash::hash_bytes(&file.bytes),
                },
            ))
        }
    }

    #[test]
    fn scope_commit_remote_post_staging_change_save_and_shutdown_deny_rename() {
        for invalidation in [
            ScopeInvalidation::Change,
            ScopeInvalidation::Save,
            ScopeInvalidation::CancelAll,
        ] {
            let root = tempfile::tempdir().unwrap();
            let source_path = root.path().join("file.c");
            let bytes = b"new local";
            std::fs::write(&source_path, bytes).unwrap();
            let source =
                crate::commands::push::ExpectedLocalSource::capture(root.path(), &source_path)
                    .unwrap();
            let destination = crate::commands::push::ExpectedRemoteDestination {
                snapshot: crate::commands::file_transfer::RemoteDestinationSnapshot::Missing,
            };
            let mut tracker = document_state::DocumentTracker::default();
            tracker.open(source_path.clone(), "new local").unwrap();
            let guard = tracker
                .begin_clean_scope(document_state::DocumentScope::Exact(source_path.clone()))
                .unwrap();
            let (staged_tx, staged_rx) = mpsc::sync_channel(1);
            let (release_tx, release_rx) = mpsc::sync_channel(1);
            let gate = ScopePauseGate {
                inner: guard,
                staged: staged_tx,
                release: Mutex::new(release_rx),
            };
            let hash = crate::hash::hash_bytes(bytes);

            let worker = thread::spawn(move || {
                let mut state = crate::state::StateFile::default();
                let mut remote = ScopeTestRemote::default();
                let decision = crate::commands::push::upload_one_guarded(
                    &mut remote,
                    &mut state,
                    "file.c",
                    "/remote",
                    "/remote/file.c",
                    bytes,
                    &hash,
                    &source,
                    &destination,
                    crate::commands::ExecutionMode::Apply,
                    &gate,
                )
                .unwrap();
                (decision, state, remote)
            });

            staged_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("remote replacement staged");
            invalidation.apply(&mut tracker, &source_path);
            release_tx.send(()).unwrap();

            let (decision, state, remote) = worker.join().unwrap();
            assert_eq!(
                decision,
                crate::commands::sync::CommitDecision::Cancelled,
                "{invalidation:?}"
            );
            assert!(state.files.is_empty());
            assert!(!remote.files.contains_key("/remote/file.c"));
            assert!(remote.files.keys().all(|path| {
                !crate::commands::transfer_temp::is_reserved_remote_transfer_temp(path)
            }));
            assert!(
                remote.events.iter().any(|event| event.starts_with("rm ")),
                "owned temp cleanup must be attempted"
            );
            assert!(
                remote
                    .events
                    .iter()
                    .all(|event| !event.starts_with("rename ")),
                "destination rename must not begin"
            );
        }
    }

    struct PauseAfterFirstCommitGate {
        inner: document_state::ScopeOperationGuard,
        committed: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        commits: AtomicUsize,
    }

    impl crate::commands::sync::CommitGate for PauseAfterFirstCommitGate {
        fn is_current(&self) -> bool {
            crate::commands::sync::CommitGate::is_current(&self.inner)
        }

        fn commit(
            &self,
            mutation: &mut dyn FnMut() -> anyhow::Result<()>,
        ) -> anyhow::Result<crate::commands::sync::CommitDecision> {
            let decision = crate::commands::sync::CommitGate::commit(&self.inner, mutation)?;
            if decision == crate::commands::sync::CommitDecision::Committed
                && self.commits.fetch_add(1, AtomicOrdering::SeqCst) == 0
            {
                self.committed
                    .send(())
                    .map_err(|_| anyhow!("first-commit receiver disappeared"))?;
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(|_| anyhow!("timed out releasing first commit"))?;
            }
            Ok(decision)
        }
    }

    #[test]
    fn scope_commit_two_entries_keep_first_progress_and_never_stage_second() {
        let root = tempfile::tempdir().unwrap();
        let first_path = root.path().join("a.c");
        let second_path = root.path().join("b.c");
        std::fs::write(&first_path, b"first").unwrap();
        std::fs::write(&second_path, b"second").unwrap();
        let first_source =
            crate::commands::push::ExpectedLocalSource::capture(root.path(), &first_path).unwrap();
        let second_source =
            crate::commands::push::ExpectedLocalSource::capture(root.path(), &second_path).unwrap();
        let missing = crate::commands::file_transfer::RemoteDestinationSnapshot::Missing;
        let first_destination = crate::commands::push::ExpectedRemoteDestination {
            snapshot: missing.clone(),
        };
        let second_destination =
            crate::commands::push::ExpectedRemoteDestination { snapshot: missing };
        let mut tracker = document_state::DocumentTracker::default();
        tracker.open(first_path.clone(), "first").unwrap();
        tracker.open(second_path.clone(), "second").unwrap();
        let guard = tracker
            .begin_clean_scope(document_state::DocumentScope::Directory(
                root.path().to_path_buf(),
            ))
            .unwrap();
        let (committed_tx, committed_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let gate = PauseAfterFirstCommitGate {
            inner: guard,
            committed: committed_tx,
            release: Mutex::new(release_rx),
            commits: AtomicUsize::new(0),
        };

        let worker = thread::spawn(move || {
            let mut state = crate::state::StateFile::default();
            let mut remote = ScopeTestRemote::default();
            let first = crate::commands::push::upload_one_guarded(
                &mut remote,
                &mut state,
                "a.c",
                "/remote",
                "/remote/a.c",
                b"first",
                &crate::hash::hash_bytes(b"first"),
                &first_source,
                &first_destination,
                crate::commands::ExecutionMode::Apply,
                &gate,
            )
            .unwrap();
            let second = crate::commands::push::upload_one_guarded(
                &mut remote,
                &mut state,
                "b.c",
                "/remote",
                "/remote/b.c",
                b"second",
                &crate::hash::hash_bytes(b"second"),
                &second_source,
                &second_destination,
                crate::commands::ExecutionMode::Apply,
                &gate,
            )
            .unwrap();
            (first, second, state, remote)
        });

        committed_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first entry committed");
        tracker.save(&first_path);
        release_tx.send(()).unwrap();

        let (first, second, state, remote) = worker.join().unwrap();
        assert_eq!(first, crate::commands::sync::CommitDecision::Committed);
        assert_eq!(second, crate::commands::sync::CommitDecision::Cancelled);
        assert!(state.files.contains_key("a.c"));
        assert!(!state.files.contains_key("b.c"));
        assert!(remote.files.contains_key("/remote/a.c"));
        assert!(!remote.files.contains_key("/remote/b.c"));
        assert_eq!(
            remote
                .events
                .iter()
                .filter(|event| event.starts_with("upload "))
                .count(),
            1,
            "second transfer must not stage"
        );
    }
}
