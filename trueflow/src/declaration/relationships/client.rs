use std::collections::{HashMap, VecDeque};
use std::env;
use std::fmt;
use std::io;
use std::num::NonZeroUsize;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::task::{Context, Poll};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use async_lsp::concurrency::ConcurrencyLayer;
use async_lsp::lsp_types::notification::{
    DidCloseTextDocument, DidOpenTextDocument, LogMessage, PublishDiagnostics, ShowMessage,
};
use async_lsp::lsp_types::request::{
    ApplyWorkspaceEdit, CallHierarchyIncomingCalls, CallHierarchyOutgoingCalls,
    CallHierarchyPrepare, WorkspaceConfiguration, WorkspaceFoldersRequest,
};
use async_lsp::lsp_types::{
    ApplyWorkspaceEditResponse, CallHierarchyIncomingCall, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyPrepareParams, CallHierarchyServerCapability,
    ClientCapabilities, ClientInfo, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DynamicRegistrationClientCapabilities, GeneralClientCapabilities, InitializeParams,
    InitializedParams, Position, PositionEncodingKind, PublishDiagnosticsParams,
    TextDocumentClientCapabilities, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, TextDocumentSyncCapability, TextDocumentSyncKind, Url,
    WorkDoneProgressParams, WorkspaceFolder,
};
use async_lsp::router::Router;
use async_lsp::{Error, ErrorCode, MainLoop, ResponseError, ServerSocket};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};
use tokio::process::{Child, Command};
use tokio::runtime::Builder;
use tokio::sync::mpsc;
use tokio::task::JoinHandle as TokioJoinHandle;
use tokio::time::{Instant, timeout, timeout_at};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tower::ServiceBuilder;

use crate::analysis::Language;

use super::{
    DocumentHash, LaunchError, LspServerLauncher, LspServerProfile, PositionEncoding,
    RelationshipCapability, RelationshipRequestKey, WorkspaceTrust, incoming_calls_params,
    outgoing_calls_params,
};

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const COMMAND_REPLY_TIMEOUT: Duration = Duration::from_secs(7);
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_DOCUMENTS: usize = 32;
const MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_PREPARED_ITEMS: usize = 64;
const MAX_HIERARCHY_ITEMS: usize = 50_000;
const MAX_DIAGNOSTIC_DOCUMENTS: usize = 64;
const MAX_MESSAGES: usize = 128;
const MAX_MESSAGE_TEXT_BYTES: usize = 4 * 1024;
const COMMAND_QUEUE_DEPTH: usize = 8;
const INCOMING_REQUEST_LIMIT: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextDocumentSync {
    pub open_close: bool,
    pub change: TextDocumentSyncKind,
}

impl TextDocumentSync {
    fn negotiate(capability: Option<TextDocumentSyncCapability>) -> Self {
        match capability {
            Some(TextDocumentSyncCapability::Kind(change)) => Self {
                open_close: change != TextDocumentSyncKind::NONE,
                change,
            },
            Some(TextDocumentSyncCapability::Options(options)) => Self {
                open_close: options.open_close.unwrap_or(false),
                change: options.change.unwrap_or(TextDocumentSyncKind::NONE),
            },
            None => Self {
                open_close: false,
                change: TextDocumentSyncKind::NONE,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSnapshot {
    pub uri: Url,
    pub version: i32,
    pub language: Language,
    pub text: String,
}

impl DocumentSnapshot {
    pub fn new(uri: Url, version: i32, language: Language, text: impl Into<String>) -> Self {
        Self {
            uri,
            version,
            language,
            text: text.into(),
        }
    }

    pub fn hash(&self) -> DocumentHash {
        DocumentHash::from_bytes(self.text.as_bytes())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CallHierarchyBundle {
    pub key: RelationshipRequestKey,
    pub prepared: Vec<CallHierarchyItem>,
    pub incoming: Vec<CallHierarchyIncomingCall>,
    pub outgoing: Vec<CallHierarchyOutgoingCall>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderCallHierarchyState {
    Ready(CallHierarchyBundle),
    Partial {
        bundle: CallHierarchyBundle,
        diagnostics: Vec<String>,
    },
    Unsupported {
        key: RelationshipRequestKey,
        capability: RelationshipCapability,
    },
    Stale {
        expected: RelationshipRequestKey,
        received: RelationshipRequestKey,
    },
    Failed {
        key: RelationshipRequestKey,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    Untrusted,
    ProfileLanguageMismatch,
    ExecutableNotFound(String),
    InvalidWorkspace(String),
    InvalidDocument(String),
    DocumentSynchronizationUnsupported,
    SessionReplacementRequired,
    ResourceLimit(String),
    SessionClosed,
    Timeout,
    Protocol(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Untrusted => formatter.write_str("workspace is not trusted for this invocation"),
            Self::ProfileLanguageMismatch => {
                formatter.write_str("the fixed LSP profile does not support this language")
            }
            Self::ExecutableNotFound(executable) => {
                write!(formatter, "trusted LSP executable {executable:?} was not found")
            }
            Self::InvalidWorkspace(message) | Self::InvalidDocument(message) => {
                formatter.write_str(message)
            }
            Self::DocumentSynchronizationUnsupported => {
                formatter.write_str("the server does not support exact document open/close synchronization")
            }
            Self::SessionReplacementRequired => formatter.write_str(
                "the immutable document changed; start a replacement snapshot session",
            ),
            Self::ResourceLimit(message) => formatter.write_str(message),
            Self::SessionClosed => formatter.write_str("the LSP session is closed"),
            Self::Timeout => formatter.write_str("the LSP operation timed out"),
            Self::Protocol(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ProviderError {}

pub struct AsyncLspLauncher {
    trust: WorkspaceTrust,
    provider: Option<RelationshipProvider>,
}

impl AsyncLspLauncher {
    pub const fn new(trust: WorkspaceTrust) -> Self {
        Self {
            trust,
            provider: None,
        }
    }

    pub fn provider(&self) -> Option<&RelationshipProvider> {
        self.provider.as_ref()
    }

    pub fn provider_mut(&mut self) -> Option<&mut RelationshipProvider> {
        self.provider.as_mut()
    }

    pub fn take_provider(&mut self) -> Option<RelationshipProvider> {
        self.provider.take()
    }

    pub fn shutdown(&mut self) -> Result<(), ProviderError> {
        if let Some(mut provider) = self.provider.take() {
            provider.shutdown()
        } else {
            Ok(())
        }
    }
}

impl Default for AsyncLspLauncher {
    fn default() -> Self {
        Self::new(WorkspaceTrust::Untrusted)
    }
}

impl LspServerLauncher for AsyncLspLauncher {
    fn spawn(
        &mut self,
        profile: LspServerProfile,
        language: Language,
        workspace_root: &Path,
    ) -> Result<(), LaunchError> {
        if self.provider.is_some() {
            return Err(LaunchError::new("an LSP provider is already active"));
        }
        let provider = RelationshipProvider::launch(
            profile,
            language,
            workspace_root,
            self.trust,
        )
        .map_err(|error| LaunchError::new(error.to_string()))?;
        self.provider = Some(provider);
        Ok(())
    }
}

pub struct RelationshipProvider {
    profile: LspServerProfile,
    language: Language,
    position_encoding: PositionEncoding,
    text_sync: TextDocumentSync,
    command_tx: mpsc::Sender<WorkerCommand>,
    thread: Option<JoinHandle<()>>,
    closed: bool,
}

impl RelationshipProvider {
    pub fn launch(
        profile: LspServerProfile,
        language: Language,
        workspace_root: &Path,
        trust: WorkspaceTrust,
    ) -> Result<Self, ProviderError> {
        if trust != WorkspaceTrust::TrustedForInvocation {
            return Err(ProviderError::Untrusted);
        }
        if LspServerProfile::for_language(language) != Some(profile)
            || profile.language_id(language).is_none()
        {
            return Err(ProviderError::ProfileLanguageMismatch);
        }

        let root = workspace_root.canonicalize().map_err(|error| {
            ProviderError::InvalidWorkspace(format!(
                "cannot resolve workspace {}: {error}",
                workspace_root.display()
            ))
        })?;
        if !root.is_dir() {
            return Err(ProviderError::InvalidWorkspace(format!(
                "workspace {} is not a directory",
                root.display()
            )));
        }
        let executable = resolve_fixed_executable(profile, &root)?;
        let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE_DEPTH);
        let (startup_tx, startup_rx) = std_mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name(format!("trueflow-lsp-{:?}", profile))
            .spawn(move || {
                let runtime = match Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = startup_tx.send(Err(ProviderError::Protocol(format!(
                            "cannot create Tokio runtime: {error}"
                        ))));
                        return;
                    }
                };
                runtime.block_on(worker_main(
                    profile,
                    language,
                    root,
                    executable,
                    command_rx,
                    startup_tx,
                ));
            })
            .map_err(|error| ProviderError::Protocol(format!("cannot start LSP worker: {error}")))?;

        let startup = startup_rx
            .recv_timeout(INITIALIZE_TIMEOUT + SHUTDOWN_TIMEOUT)
            .map_err(|_| ProviderError::Timeout)?;
        let (position_encoding, text_sync) = match startup {
            Ok(metadata) => metadata,
            Err(error) => {
                let _ = thread.join();
                return Err(error);
            }
        };

        Ok(Self {
            profile,
            language,
            position_encoding,
            text_sync,
            command_tx,
            thread: Some(thread),
            closed: false,
        })
    }

    pub const fn profile(&self) -> LspServerProfile {
        self.profile
    }

    pub const fn language(&self) -> Language {
        self.language
    }

    pub const fn position_encoding(&self) -> PositionEncoding {
        self.position_encoding
    }

    pub const fn text_document_sync(&self) -> TextDocumentSync {
        self.text_sync
    }

    pub fn synchronize_document(&mut self, document: DocumentSnapshot) -> Result<(), ProviderError> {
        self.request(|reply| WorkerCommand::Synchronize { document, reply })?
    }

    pub fn close_document(&mut self, uri: Url) -> Result<(), ProviderError> {
        self.request(|reply| WorkerCommand::CloseDocument { uri, reply })?
    }

    pub fn call_hierarchy(
        &mut self,
        key: RelationshipRequestKey,
        position: Position,
    ) -> ProviderCallHierarchyState {
        let failure_key = key.clone();
        match self.request(|reply| WorkerCommand::CallHierarchy {
            key,
            position,
            reply,
        }) {
            Ok(state) => state,
            Err(error) => ProviderCallHierarchyState::Failed {
                key: failure_key,
                message: error.to_string(),
            },
        }
    }

    pub fn shutdown(&mut self) -> Result<(), ProviderError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let reply = self.request_even_if_closing(|reply| WorkerCommand::Shutdown { reply });
        if reply.is_ok() {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        } else {
            self.thread.take();
        }
        reply?
    }

    fn request<T>(
        &self,
        command: impl FnOnce(std_mpsc::SyncSender<T>) -> WorkerCommand,
    ) -> Result<T, ProviderError> {
        if self.closed {
            return Err(ProviderError::SessionClosed);
        }
        self.request_even_if_closing(command)
    }

    fn request_even_if_closing<T>(
        &self,
        command: impl FnOnce(std_mpsc::SyncSender<T>) -> WorkerCommand,
    ) -> Result<T, ProviderError> {
        let (reply_tx, reply_rx) = std_mpsc::sync_channel(1);
        self.command_tx
            .blocking_send(command(reply_tx))
            .map_err(|_| ProviderError::SessionClosed)?;
        reply_rx
            .recv_timeout(COMMAND_REPLY_TIMEOUT)
            .map_err(|error| match error {
                std_mpsc::RecvTimeoutError::Timeout => ProviderError::Timeout,
                std_mpsc::RecvTimeoutError::Disconnected => ProviderError::SessionClosed,
            })
    }
}

impl Drop for RelationshipProvider {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

enum WorkerCommand {
    Synchronize {
        document: DocumentSnapshot,
        reply: std_mpsc::SyncSender<Result<(), ProviderError>>,
    },
    CloseDocument {
        uri: Url,
        reply: std_mpsc::SyncSender<Result<(), ProviderError>>,
    },
    CallHierarchy {
        key: RelationshipRequestKey,
        position: Position,
        reply: std_mpsc::SyncSender<ProviderCallHierarchyState>,
    },
    Shutdown {
        reply: std_mpsc::SyncSender<Result<(), ProviderError>>,
    },
}

#[derive(Clone)]
struct OpenDocument {
    version: i32,
    hash: DocumentHash,
    byte_len: usize,
}

struct WorkerSession {
    profile: LspServerProfile,
    language: Language,
    root: PathBuf,
    server: ServerSocket,
    child: Child,
    mainloop: TokioJoinHandle<async_lsp::Result<()>>,
    stderr_drain: TokioJoinHandle<()>,
    position_encoding: PositionEncoding,
    text_sync: TextDocumentSync,
    call_hierarchy_supported: bool,
    documents: HashMap<Url, OpenDocument>,
    document_bytes: usize,
}

#[derive(Default)]
struct RouterState {
    diagnostics: VecDeque<DiagnosticSummary>,
    messages: VecDeque<String>,
    workspace_folder: Option<WorkspaceFolder>,
}

struct DiagnosticSummary {
    #[allow(dead_code)]
    uri: Url,
    #[allow(dead_code)]
    version: Option<i32>,
    #[allow(dead_code)]
    count: usize,
}

async fn worker_main(
    profile: LspServerProfile,
    language: Language,
    root: PathBuf,
    executable: PathBuf,
    mut command_rx: mpsc::Receiver<WorkerCommand>,
    startup_tx: std_mpsc::SyncSender<
        Result<(PositionEncoding, TextDocumentSync), ProviderError>,
    >,
) {
    let mut session = match start_worker_session(profile, language, root, executable).await {
        Ok(session) => session,
        Err(error) => {
            let _ = startup_tx.send(Err(error));
            return;
        }
    };
    let _ = startup_tx.send(Ok((session.position_encoding, session.text_sync)));

    while let Some(command) = command_rx.recv().await {
        match command {
            WorkerCommand::Synchronize { document, reply } => {
                let _ = reply.send(session.synchronize_document(document));
            }
            WorkerCommand::CloseDocument { uri, reply } => {
                let _ = reply.send(session.close_document(&uri));
            }
            WorkerCommand::CallHierarchy {
                key,
                position,
                reply,
            } => {
                let execution = session.call_hierarchy(key, position).await;
                let poison = execution.poison_session;
                let _ = reply.send(execution.state);
                if poison {
                    break;
                }
            }
            WorkerCommand::Shutdown { reply } => {
                let result = session.stop().await;
                let _ = reply.send(result);
                return;
            }
        }
    }
    let _ = session.stop().await;
}

async fn start_worker_session(
    profile: LspServerProfile,
    language: Language,
    root: PathBuf,
    executable: PathBuf,
) -> Result<WorkerSession, ProviderError> {
    let mut command = Command::new(executable);
    command
        .args(profile.argv())
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| ProviderError::Protocol(format!("cannot spawn LSP server: {error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProviderError::Protocol("LSP stdout pipe is unavailable".to_owned()))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| ProviderError::Protocol("LSP stdin pipe is unavailable".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ProviderError::Protocol("LSP stderr pipe is unavailable".to_owned()))?;

    let workspace_uri = Url::from_directory_path(&root).map_err(|()| {
        ProviderError::InvalidWorkspace(format!("workspace {} is not a file URI", root.display()))
    })?;
    let workspace_folder = WorkspaceFolder {
        uri: workspace_uri,
        name: root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace")
            .to_owned(),
    };
    let router_folder = workspace_folder.clone();

    let (mainloop, server) = MainLoop::new_client(move |_server| {
        let mut router = Router::new(RouterState {
            workspace_folder: Some(router_folder),
            ..RouterState::default()
        });
        router
            .request::<ApplyWorkspaceEdit, _>(|_, _| async {
                Ok(ApplyWorkspaceEditResponse {
                    applied: false,
                    failure_reason: Some(
                        "trueflow never applies language-server workspace edits".to_owned(),
                    ),
                    failed_change: None,
                })
            })
            .request::<WorkspaceConfiguration, _>(|_, params| async move {
                Ok(vec![Value::Null; params.items.len()])
            })
            .request::<WorkspaceFoldersRequest, _>(|state, ()| {
                let folders = state.workspace_folder.clone().map(|folder| vec![folder]);
                async move { Ok(folders) }
            })
            .notification::<PublishDiagnostics>(|state, params| {
                record_diagnostics(state, params);
                std::ops::ControlFlow::Continue(())
            })
            .notification::<ShowMessage>(|state, params| {
                record_message(state, params.message);
                std::ops::ControlFlow::Continue(())
            })
            .notification::<LogMessage>(|state, params| {
                record_message(state, params.message);
                std::ops::ControlFlow::Continue(())
            })
            .unhandled_request(|_, request| async move {
                Err(ResponseError::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    format!("server request {} is not permitted", request.method),
                ))
            })
            .unhandled_notification(|_, _| std::ops::ControlFlow::Continue(()));

        ServiceBuilder::new()
            .layer(ConcurrencyLayer::new(
                NonZeroUsize::new(INCOMING_REQUEST_LIMIT).expect("positive request limit"),
            ))
            .service(router)
    });

    let bounded_stdout = BoundedLspReader::new(stdout);
    let mainloop = tokio::spawn(mainloop.run_buffered(bounded_stdout.compat(), stdin.compat_write()));
    let stderr_buffer = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_STDERR_BYTES)));
    let stderr_drain = tokio::spawn(drain_stderr(stderr, Arc::clone(&stderr_buffer)));

    let capabilities = ClientCapabilities {
        general: Some(GeneralClientCapabilities {
            position_encodings: Some(vec![
                PositionEncodingKind::UTF8,
                PositionEncodingKind::UTF16,
            ]),
            ..GeneralClientCapabilities::default()
        }),
        text_document: Some(TextDocumentClientCapabilities {
            call_hierarchy: Some(DynamicRegistrationClientCapabilities {
                dynamic_registration: Some(false),
            }),
            ..TextDocumentClientCapabilities::default()
        }),
        ..ClientCapabilities::default()
    };
    let initialize = InitializeParams {
        capabilities,
        workspace_folders: Some(vec![workspace_folder]),
        client_info: Some(ClientInfo {
            name: "trueflow".to_owned(),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        }),
        ..InitializeParams::default()
    };
    let initialized = match timeout(INITIALIZE_TIMEOUT, server.request::<async_lsp::lsp_types::request::Initialize>(initialize)).await {
        Ok(Ok(initialized)) => initialized,
        Ok(Err(error)) => {
            let context =
                terminate_uninitialized(&mut child, mainloop, stderr_drain, stderr_buffer).await;
            return Err(ProviderError::Protocol(format!(
                "LSP initialize failed: {error}{context}"
            )));
        }
        Err(_) => {
            terminate_uninitialized(&mut child, mainloop, stderr_drain, stderr_buffer).await;
            return Err(ProviderError::Timeout);
        }
    };

    let position_encoding = match initialized.capabilities.position_encoding {
        Some(kind) if kind == PositionEncodingKind::UTF8 => PositionEncoding::Utf8,
        Some(kind) if kind == PositionEncodingKind::UTF16 => PositionEncoding::Utf16,
        Some(kind) => {
            terminate_uninitialized(&mut child, mainloop, stderr_drain, stderr_buffer).await;
            return Err(ProviderError::Protocol(format!(
                "server selected unadvertised position encoding {kind:?}"
            )));
        }
        None => PositionEncoding::Utf16,
    };
    let text_sync = TextDocumentSync::negotiate(initialized.capabilities.text_document_sync);
    let call_hierarchy_supported = matches!(
        initialized.capabilities.call_hierarchy_provider,
        Some(CallHierarchyServerCapability::Simple(true))
            | Some(CallHierarchyServerCapability::Options(_))
    );
    server
        .notify::<async_lsp::lsp_types::notification::Initialized>(InitializedParams {})
        .map_err(|error| ProviderError::Protocol(format!("cannot send initialized: {error}")))?;

    Ok(WorkerSession {
        profile,
        language,
        root,
        server,
        child,
        mainloop,
        stderr_drain,
        position_encoding,
        text_sync,
        call_hierarchy_supported,
        documents: HashMap::new(),
        document_bytes: 0,
    })
}

impl WorkerSession {
    fn synchronize_document(&mut self, document: DocumentSnapshot) -> Result<(), ProviderError> {
        if !self.text_sync.open_close {
            return Err(ProviderError::DocumentSynchronizationUnsupported);
        }
        if document.language != self.language
            || self.profile.language_id(document.language).is_none()
        {
            return Err(ProviderError::ProfileLanguageMismatch);
        }
        validate_document_uri(&self.root, &document.uri)?;
        if document.text.len() > MAX_DOCUMENT_BYTES {
            return Err(ProviderError::ResourceLimit(format!(
                "document exceeds the {MAX_DOCUMENT_BYTES}-byte synchronization limit"
            )));
        }
        let hash = document.hash();
        if let Some(open) = self.documents.get(&document.uri) {
            if open.version == document.version && open.hash == hash {
                return Ok(());
            }
            return Err(ProviderError::SessionReplacementRequired);
        }
        if self.documents.len() >= MAX_DOCUMENTS {
            return Err(ProviderError::ResourceLimit(format!(
                "session already has the maximum {MAX_DOCUMENTS} open documents"
            )));
        }
        let new_total = self
            .document_bytes
            .checked_add(document.text.len())
            .ok_or_else(|| ProviderError::ResourceLimit("document byte count overflow".to_owned()))?;
        if new_total > MAX_DOCUMENT_BYTES {
            return Err(ProviderError::ResourceLimit(format!(
                "open documents exceed the {MAX_DOCUMENT_BYTES}-byte session limit"
            )));
        }
        let language_id = self
            .profile
            .language_id(document.language)
            .ok_or(ProviderError::ProfileLanguageMismatch)?;
        self.server
            .notify::<DidOpenTextDocument>(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: document.uri.clone(),
                    language_id: language_id.to_owned(),
                    version: document.version,
                    text: document.text,
                },
            })
            .map_err(|error| ProviderError::Protocol(format!("cannot send didOpen: {error}")))?;
        self.documents.insert(
            document.uri,
            OpenDocument {
                version: document.version,
                hash,
                byte_len: new_total - self.document_bytes,
            },
        );
        self.document_bytes = new_total;
        Ok(())
    }

    fn close_document(&mut self, uri: &Url) -> Result<(), ProviderError> {
        if let Some(open) = self.documents.remove(uri) {
            self.server
                .notify::<DidCloseTextDocument>(DidCloseTextDocumentParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                })
                .map_err(|error| ProviderError::Protocol(format!("cannot send didClose: {error}")))?;
            self.document_bytes = self.document_bytes.saturating_sub(open.byte_len);
        }
        Ok(())
    }

    async fn call_hierarchy(
        &self,
        key: RelationshipRequestKey,
        position: Position,
    ) -> QueryExecution {
        if key.server_profile != self.profile || key.document_uri.scheme() != "file" {
            return QueryExecution::failed(key, "request key does not belong to this LSP session");
        }
        let Some(document) = self.documents.get(&key.document_uri) else {
            return QueryExecution::failed(key, "the exact request document is not synchronized");
        };
        if document.version != key.document_version || document.hash != key.document_hash {
            let received = RelationshipRequestKey {
                document_version: document.version,
                document_hash: document.hash.clone(),
                ..key.clone()
            };
            return QueryExecution {
                state: ProviderCallHierarchyState::Stale {
                    expected: key,
                    received,
                },
                poison_session: false,
            };
        }
        if !self.call_hierarchy_supported {
            return QueryExecution {
                state: ProviderCallHierarchyState::Unsupported {
                    key,
                    capability: RelationshipCapability::PrepareCallHierarchy,
                },
                poison_session: false,
            };
        }

        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let prepare_params = CallHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: key.document_uri.clone(),
                },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let mut prepared = loop {
            match timeout_at(
                deadline,
                self.server
                    .request::<CallHierarchyPrepare>(prepare_params.clone()),
            )
            .await
            {
                Ok(Ok(items)) => break items.unwrap_or_default(),
                Ok(Err(Error::Response(response)))
                    if response.code == ErrorCode::CONTENT_MODIFIED =>
                {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Ok(Err(Error::Response(response)))
                    if response.code == ErrorCode::METHOD_NOT_FOUND =>
                {
                    return QueryExecution::unsupported(
                        key,
                        RelationshipCapability::PrepareCallHierarchy,
                    );
                }
                Ok(Err(error)) => {
                    return QueryExecution::failed(
                        key,
                        format!("prepareCallHierarchy failed: {error}"),
                    );
                }
                Err(_) => return QueryExecution::timed_out(key),
            }
        };
        if prepared.len() > MAX_PREPARED_ITEMS {
            prepared.truncate(MAX_PREPARED_ITEMS);
            return QueryExecution::partial(
                key,
                prepared,
                Vec::new(),
                Vec::new(),
                format!("server returned more than {MAX_PREPARED_ITEMS} prepared items"),
            );
        }

        let mut incoming = Vec::new();
        let mut outgoing = Vec::new();
        for index in 0..prepared.len() {
            let item = prepared[index].clone();
            let incoming_result = timeout_at(
                deadline,
                self.server
                    .request::<CallHierarchyIncomingCalls>(incoming_calls_params(item.clone())),
            )
            .await;
            match incoming_result {
                Ok(Ok(calls)) => incoming.extend(calls.unwrap_or_default()),
                Ok(Err(Error::Response(response)))
                    if response.code == ErrorCode::METHOD_NOT_FOUND =>
                {
                    return QueryExecution::unsupported(
                        key,
                        RelationshipCapability::IncomingCalls,
                    );
                }
                Ok(Err(error)) => {
                    return QueryExecution::failed(
                        key,
                        format!("callHierarchy/incomingCalls failed: {error}"),
                    );
                }
                Err(_) => return QueryExecution::timed_out(key),
            }
            if incoming.len() > MAX_HIERARCHY_ITEMS {
                incoming.truncate(MAX_HIERARCHY_ITEMS);
                return QueryExecution::partial(
                    key,
                    prepared,
                    incoming,
                    outgoing,
                    format!("incoming calls exceeded the {MAX_HIERARCHY_ITEMS}-item limit"),
                );
            }

            let outgoing_result = timeout_at(
                deadline,
                self.server
                    .request::<CallHierarchyOutgoingCalls>(outgoing_calls_params(item)),
            )
            .await;
            match outgoing_result {
                Ok(Ok(calls)) => outgoing.extend(calls.unwrap_or_default()),
                Ok(Err(Error::Response(response)))
                    if response.code == ErrorCode::METHOD_NOT_FOUND =>
                {
                    return QueryExecution::unsupported(
                        key,
                        RelationshipCapability::OutgoingCalls,
                    );
                }
                Ok(Err(error)) => {
                    return QueryExecution::failed(
                        key,
                        format!("callHierarchy/outgoingCalls failed: {error}"),
                    );
                }
                Err(_) => return QueryExecution::timed_out(key),
            }
            if outgoing.len() > MAX_HIERARCHY_ITEMS {
                outgoing.truncate(MAX_HIERARCHY_ITEMS);
                return QueryExecution::partial(
                    key,
                    prepared,
                    incoming,
                    outgoing,
                    format!("outgoing calls exceeded the {MAX_HIERARCHY_ITEMS}-item limit"),
                );
            }
        }

        QueryExecution {
            state: ProviderCallHierarchyState::Ready(CallHierarchyBundle {
                key,
                prepared,
                incoming,
                outgoing,
            }),
            poison_session: false,
        }
    }

    async fn stop(&mut self) -> Result<(), ProviderError> {
        let uris: Vec<_> = self.documents.keys().cloned().collect();
        for uri in uris {
            let _ = self.close_document(&uri);
        }

        let shutdown_result = timeout(SHUTDOWN_TIMEOUT, self.server.request::<async_lsp::lsp_types::request::Shutdown>(())).await;
        if matches!(&shutdown_result, Ok(Ok(()))) {
            let _ = self
                .server
                .notify::<async_lsp::lsp_types::notification::Exit>(());
        }
        if timeout(SHUTDOWN_TIMEOUT, self.child.wait()).await.is_err() {
            let _ = self.child.kill().await;
            let _ = timeout(SHUTDOWN_TIMEOUT, self.child.wait()).await;
        }
        if timeout(SHUTDOWN_TIMEOUT, &mut self.mainloop).await.is_err() {
            self.mainloop.abort();
        }
        self.stderr_drain.abort();

        match shutdown_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(ProviderError::Protocol(format!("LSP shutdown failed: {error}"))),
            Err(_) => Err(ProviderError::Timeout),
        }
    }
}

struct QueryExecution {
    state: ProviderCallHierarchyState,
    poison_session: bool,
}

impl QueryExecution {
    fn failed(key: RelationshipRequestKey, message: impl Into<String>) -> Self {
        Self {
            state: ProviderCallHierarchyState::Failed {
                key,
                message: message.into(),
            },
            poison_session: false,
        }
    }

    fn unsupported(
        key: RelationshipRequestKey,
        capability: RelationshipCapability,
    ) -> Self {
        Self {
            state: ProviderCallHierarchyState::Unsupported { key, capability },
            poison_session: false,
        }
    }

    fn timed_out(key: RelationshipRequestKey) -> Self {
        Self {
            state: ProviderCallHierarchyState::Failed {
                key,
                message: "call hierarchy request timed out; the session was cancelled".to_owned(),
            },
            poison_session: true,
        }
    }

    fn partial(
        key: RelationshipRequestKey,
        prepared: Vec<CallHierarchyItem>,
        incoming: Vec<CallHierarchyIncomingCall>,
        outgoing: Vec<CallHierarchyOutgoingCall>,
        diagnostic: String,
    ) -> Self {
        Self {
            state: ProviderCallHierarchyState::Partial {
                bundle: CallHierarchyBundle {
                    key,
                    prepared,
                    incoming,
                    outgoing,
                },
                diagnostics: vec![diagnostic],
            },
            poison_session: false,
        }
    }
}

fn resolve_fixed_executable(
    profile: LspServerProfile,
    workspace_root: &Path,
) -> Result<PathBuf, ProviderError> {
    let executable = profile.executable();
    let Some(path) = env::var_os("PATH") else {
        return Err(ProviderError::ExecutableNotFound(executable.to_owned()));
    };
    for directory in env::split_paths(&path) {
        if !directory.is_absolute() || directory.components().any(|part| part == Component::CurDir) {
            continue;
        }
        let candidate = directory.join(executable);
        if !candidate.is_file() {
            continue;
        }
        let Ok(resolved) = candidate.canonicalize() else {
            continue;
        };
        if resolved.starts_with(workspace_root) {
            continue;
        }
        // Preserve the fixed basename for multi-call launchers such as rustup proxies.
        return Ok(candidate);
    }
    Err(ProviderError::ExecutableNotFound(executable.to_owned()))
}

fn validate_document_uri(root: &Path, uri: &Url) -> Result<(), ProviderError> {
    if uri.scheme() != "file" {
        return Err(ProviderError::InvalidDocument(
            "only file: document URIs are accepted".to_owned(),
        ));
    }
    let path = uri.to_file_path().map_err(|()| {
        ProviderError::InvalidDocument(format!("document URI {uri} is not a local file"))
    })?;
    if path.components().any(|component| component == Component::ParentDir)
        || !path.starts_with(root)
    {
        return Err(ProviderError::InvalidDocument(format!(
            "document URI {uri} is outside the trusted workspace"
        )));
    }
    Ok(())
}

fn record_diagnostics(state: &mut RouterState, params: PublishDiagnosticsParams) {
    if let Some(existing) = state
        .diagnostics
        .iter_mut()
        .find(|existing| existing.uri == params.uri)
    {
        existing.version = params.version;
        existing.count = params.diagnostics.len();
        return;
    }
    if state.diagnostics.len() == MAX_DIAGNOSTIC_DOCUMENTS {
        state.diagnostics.pop_front();
    }
    state.diagnostics.push_back(DiagnosticSummary {
        uri: params.uri,
        version: params.version,
        count: params.diagnostics.len(),
    });
}

fn record_message(state: &mut RouterState, mut message: String) {
    if message.len() > MAX_MESSAGE_TEXT_BYTES {
        let mut boundary = MAX_MESSAGE_TEXT_BYTES;
        while !message.is_char_boundary(boundary) {
            boundary -= 1;
        }
        message.truncate(boundary);
    }
    if state.messages.len() == MAX_MESSAGES {
        state.messages.pop_front();
    }
    state.messages.push_back(message);
}

async fn terminate_uninitialized(
    child: &mut Child,
    mut mainloop: TokioJoinHandle<async_lsp::Result<()>>,
    stderr_drain: TokioJoinHandle<()>,
    stderr: Arc<Mutex<VecDeque<u8>>>,
) -> String {
    let _ = child.kill().await;
    let _ = timeout(SHUTDOWN_TIMEOUT, child.wait()).await;
    let mainloop_failure = match timeout(SHUTDOWN_TIMEOUT, &mut mainloop).await {
        Ok(Ok(Err(error))) => Some(error.to_string()),
        Ok(Err(error)) => Some(format!("worker join failed: {error}")),
        Ok(Ok(Ok(()))) => None,
        Err(_) => {
            mainloop.abort();
            Some("protocol worker did not stop".to_owned())
        }
    };
    stderr_drain.abort();
    let stderr = stderr
        .lock()
        .ok()
        .map(|mut bytes| String::from_utf8_lossy(bytes.make_contiguous()).trim().to_owned())
        .filter(|message| !message.is_empty());
    match (mainloop_failure, stderr) {
        (None, None) => String::new(),
        (Some(protocol), None) => format!("; protocol worker: {protocol}"),
        (None, Some(stderr)) => format!("; server stderr: {stderr}"),
        (Some(protocol), Some(stderr)) => {
            format!("; protocol worker: {protocol}; server stderr: {stderr}")
        }
    }
}

async fn drain_stderr(
    mut stderr: tokio::process::ChildStderr,
    buffer: Arc<Mutex<VecDeque<u8>>>,
) {
    let mut chunk = [0u8; 4096];
    loop {
        let count = match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(count) => count,
        };
        let Ok(mut buffer) = buffer.lock() else {
            return;
        };
        for byte in &chunk[..count] {
            if buffer.len() == MAX_STDERR_BYTES {
                buffer.pop_front();
            }
            buffer.push_back(*byte);
        }
    }
}

struct BoundedLspReader<R> {
    inner: R,
    ready: VecDeque<u8>,
    header: Vec<u8>,
    body_remaining: usize,
}

impl<R> BoundedLspReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            ready: VecDeque::new(),
            header: Vec::new(),
            body_remaining: 0,
        }
    }

    fn consume(&mut self, bytes: &[u8]) -> io::Result<()> {
        for byte in bytes {
            if self.body_remaining > 0 {
                self.ready.push_back(*byte);
                self.body_remaining -= 1;
                continue;
            }

            self.header.push(*byte);
            if self.header.len() > MAX_HEADER_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "LSP header exceeds the configured limit",
                ));
            }
            if self.header.ends_with(b"\r\n\r\n") {
                let content_length = parse_content_length(&self.header)?;
                if content_length > MAX_MESSAGE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "LSP message exceeds the configured limit",
                    ));
                }
                self.ready.extend(self.header.drain(..));
                self.body_remaining = content_length;
            }
        }
        Ok(())
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for BoundedLspReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            while output.remaining() > 0 {
                let Some(byte) = self.ready.pop_front() else {
                    break;
                };
                output.put_slice(&[byte]);
            }
            if output.filled().len() > 0 {
                return Poll::Ready(Ok(()));
            }

            let mut storage = [0u8; 8192];
            let mut input = ReadBuf::new(&mut storage);
            match Pin::new(&mut self.inner).poll_read(context, &mut input) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) if input.filled().is_empty() => return Poll::Ready(Ok(())),
                Poll::Ready(Ok(())) => {
                    if let Err(error) = self.consume(input.filled()) {
                        return Poll::Ready(Err(error));
                    }
                }
            }
        }
    }
}

fn parse_content_length(header: &[u8]) -> io::Result<usize> {
    let header = std::str::from_utf8(header).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "LSP header is not valid UTF-8")
    })?;
    let mut content_length = None;
    for line in header.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("Content-Length") {
            if content_length.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "duplicate LSP Content-Length header",
                ));
            }
            content_length = Some(value.trim().parse::<usize>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid LSP Content-Length header")
            })?);
        }
    }
    content_length.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "missing LSP Content-Length header",
        )
    })
}
