use std::fmt;
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use async_lsp::lsp_types::{
    CallHierarchyIncomingCallsParams, CallHierarchyItem, CallHierarchyOutgoingCallsParams,
    PartialResultParams, Position, Range, Url, WorkDoneProgressParams,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::analysis::Language;
use crate::declaration::{DeclarationId, DeclarationKey};

#[path = "relationships/client.rs"]
mod client;

pub use client::{
    AsyncLspLauncher, CallHierarchyBundle, DocumentSnapshot, ProviderCallHierarchyState,
    ProviderError, RelationshipProvider, TextDocumentSync,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LspServerProfile {
    RustAnalyzer,
    TypeScriptLanguageServer,
    Pylsp,
    Gopls,
    Clangd,
}

impl LspServerProfile {
    pub const fn for_language(language: Language) -> Option<Self> {
        match language {
            Language::Rust => Some(Self::RustAnalyzer),
            Language::JavaScript | Language::TypeScript => Some(Self::TypeScriptLanguageServer),
            Language::Python => Some(Self::Pylsp),
            Language::Go => Some(Self::Gopls),
            Language::C | Language::Cpp => Some(Self::Clangd),
            _ => None,
        }
    }

    pub const fn executable(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust-analyzer",
            Self::TypeScriptLanguageServer => "typescript-language-server",
            Self::Pylsp => "pylsp",
            Self::Gopls => "gopls",
            Self::Clangd => "clangd",
        }
    }

    pub const fn argv(self) -> &'static [&'static str] {
        match self {
            Self::TypeScriptLanguageServer => &["--stdio"],
            Self::RustAnalyzer | Self::Pylsp | Self::Gopls | Self::Clangd => &[],
        }
    }

    pub const fn language_id(self, language: Language) -> Option<&'static str> {
        match (self, language) {
            (Self::RustAnalyzer, Language::Rust) => Some("rust"),
            (Self::TypeScriptLanguageServer, Language::JavaScript) => Some("javascript"),
            (Self::TypeScriptLanguageServer, Language::TypeScript) => Some("typescript"),
            (Self::Pylsp, Language::Python) => Some("python"),
            (Self::Gopls, Language::Go) => Some("go"),
            (Self::Clangd, Language::C) => Some("c"),
            (Self::Clangd, Language::Cpp) => Some("cpp"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkspaceTrust {
    Untrusted,
    TrustedForInvocation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchError {
    message: String,
}

impl LaunchError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for LaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LaunchError {}

pub trait LspServerLauncher {
    fn spawn(
        &mut self,
        profile: LspServerProfile,
        language: Language,
        workspace_root: &Path,
    ) -> std::result::Result<(), LaunchError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    Unconfigured,
    Untrusted,
    Active { profile: LspServerProfile },
    Failed { message: String },
}

pub fn start_session(
    profile: Option<LspServerProfile>,
    language: Language,
    workspace_root: &Path,
    trust: WorkspaceTrust,
    launcher: &mut impl LspServerLauncher,
) -> SessionState {
    let Some(profile) = profile else {
        return SessionState::Unconfigured;
    };
    if trust != WorkspaceTrust::TrustedForInvocation {
        return SessionState::Untrusted;
    }
    match launcher.spawn(profile, language, workspace_root) {
        Ok(()) => SessionState::Active { profile },
        Err(error) => SessionState::Failed {
            message: error.to_string(),
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerRequestDecision {
    RejectApplyEdit { applied: bool },
    RejectMethod,
    AllowReadOnly,
}

pub fn server_request_policy(method: &str) -> ServerRequestDecision {
    match method {
        "workspace/applyEdit" => ServerRequestDecision::RejectApplyEdit { applied: false },
        "workspace/configuration" | "workspace/workspaceFolders" => {
            ServerRequestDecision::AllowReadOnly
        }
        _ => ServerRequestDecision::RejectMethod,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
}

pub fn lsp_position_for_byte_offset(
    source: &str,
    byte_offset: usize,
    encoding: PositionEncoding,
) -> Result<Position> {
    if byte_offset > source.len() {
        bail!("byte offset {byte_offset} is past the end of the document");
    }
    if !source.is_char_boundary(byte_offset) {
        bail!("byte offset {byte_offset} splits a UTF-8 code point");
    }
    if byte_offset < source.len()
        && source.as_bytes()[byte_offset] == b'\n'
        && byte_offset > 0
        && source.as_bytes()[byte_offset - 1] == b'\r'
    {
        bail!("byte offset {byte_offset} splits a CRLF line ending");
    }

    let preceding = &source[..byte_offset];
    let line = preceding.bytes().filter(|byte| *byte == b'\n').count();
    let line_start = preceding
        .rfind('\n')
        .map_or(0, |newline| newline.saturating_add(1));
    let line_prefix = &source[line_start..byte_offset];
    let character = match encoding {
        PositionEncoding::Utf8 => line_prefix.len(),
        PositionEncoding::Utf16 => line_prefix.encode_utf16().count(),
    };

    Ok(Position::new(
        u32::try_from(line).map_err(|_| anyhow!("document has too many lines"))?,
        u32::try_from(character).map_err(|_| anyhow!("line is too long"))?,
    ))
}

pub fn byte_offset_for_lsp_position(
    source: &str,
    position: Position,
    encoding: PositionEncoding,
) -> Result<usize> {
    let target_line = usize::try_from(position.line).map_err(|_| anyhow!("invalid line"))?;
    let mut line_start = 0usize;
    for _ in 0..target_line {
        let Some(relative_newline) = source[line_start..].find('\n') else {
            bail!("line {} does not exist", position.line);
        };
        line_start = line_start
            .checked_add(relative_newline + 1)
            .ok_or_else(|| anyhow!("line offset overflow"))?;
    }

    let physical_end = source[line_start..]
        .find('\n')
        .map_or(source.len(), |relative| line_start + relative);
    let line_end = if physical_end > line_start && source.as_bytes()[physical_end - 1] == b'\r' {
        physical_end - 1
    } else {
        physical_end
    };
    let line = &source[line_start..line_end];
    let target = usize::try_from(position.character).map_err(|_| anyhow!("invalid character"))?;

    match encoding {
        PositionEncoding::Utf8 => {
            if target > line.len() {
                bail!("character {} is past the end of line {}", position.character, position.line);
            }
            if !line.is_char_boundary(target) {
                bail!("UTF-8 position splits a code point");
            }
            Ok(line_start + target)
        }
        PositionEncoding::Utf16 => {
            let mut units = 0usize;
            for (byte, character) in line.char_indices() {
                if units == target {
                    return Ok(line_start + byte);
                }
                let width = character.len_utf16();
                if target < units + width {
                    bail!("UTF-16 position splits a surrogate pair");
                }
                units += width;
            }
            if units == target {
                Ok(line_end)
            } else {
                bail!("character {} is past the end of line {}", position.character, position.line)
            }
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct SourceGeneration(u64);

impl SourceGeneration {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DocumentHash(String);

impl DocumentHash {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Self(format!("{:x}", hasher.finalize()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RelationshipRequestKey {
    pub source_generation: SourceGeneration,
    pub server_profile: LspServerProfile,
    pub declaration_id: DeclarationId,
    pub declaration_key: DeclarationKey,
    pub document_uri: Url,
    pub document_version: i32,
    pub document_hash: DocumentHash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RelationshipKind {
    Calls,
    CalledBy,
    UsesType,
    UsedBy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelationshipTarget {
    InReview(DeclarationId),
    External { uri: Url, range: Range },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipEdge {
    pub kind: RelationshipKind,
    pub source: DeclarationId,
    pub target: RelationshipTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RelationshipCapability {
    PrepareCallHierarchy,
    IncomingCalls,
    OutgoingCalls,
    TypeDefinition,
    References,
    DocumentSynchronization,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelationshipOutcome {
    Complete {
        edges: Vec<RelationshipEdge>,
    },
    Partial {
        edges: Vec<RelationshipEdge>,
        diagnostics: Vec<String>,
    },
    Unsupported {
        capability: RelationshipCapability,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipResult {
    pub key: RelationshipRequestKey,
    pub outcome: RelationshipOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelationshipState {
    Ready {
        key: RelationshipRequestKey,
        edges: Vec<RelationshipEdge>,
    },
    Partial {
        key: RelationshipRequestKey,
        edges: Vec<RelationshipEdge>,
        diagnostics: Vec<String>,
    },
    Unsupported {
        key: RelationshipRequestKey,
        capability: RelationshipCapability,
    },
    Failed {
        key: RelationshipRequestKey,
        message: String,
    },
    Stale {
        expected: RelationshipRequestKey,
        received: RelationshipRequestKey,
    },
}

pub fn reconcile_relationship_result(
    expected: &RelationshipRequestKey,
    result: RelationshipResult,
) -> RelationshipState {
    if *expected != result.key {
        return RelationshipState::Stale {
            expected: expected.clone(),
            received: result.key,
        };
    }

    match result.outcome {
        RelationshipOutcome::Complete { edges } => RelationshipState::Ready {
            key: result.key,
            edges,
        },
        RelationshipOutcome::Partial { edges, diagnostics } => RelationshipState::Partial {
            key: result.key,
            edges,
            diagnostics,
        },
        RelationshipOutcome::Unsupported { capability } => RelationshipState::Unsupported {
            key: result.key,
            capability,
        },
        RelationshipOutcome::Failed { message } => RelationshipState::Failed {
            key: result.key,
            message,
        },
    }
}

pub fn incoming_calls_params(item: CallHierarchyItem) -> CallHierarchyIncomingCallsParams {
    CallHierarchyIncomingCallsParams {
        item,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

pub fn outgoing_calls_params(item: CallHierarchyItem) -> CallHierarchyOutgoingCallsParams {
    CallHierarchyOutgoingCallsParams {
        item,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}
