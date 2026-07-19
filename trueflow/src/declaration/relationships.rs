use std::fmt;
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use async_lsp::lsp_types::{
    CallHierarchyIncomingCallsParams, CallHierarchyItem, CallHierarchyOutgoingCallsParams,
    Location, PartialResultParams, Position, Range, ReferenceContext, ReferenceParams,
    TextDocumentIdentifier, TextDocumentPositionParams, Url, WorkDoneProgressParams,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::analysis::Language;
use crate::declaration::{
    DeclarationId, DeclarationKey, DeclarationKind, DeclarationNode, FileDeclarationFacts,
    TypeUseRole,
};
use crate::hashing::hex_digest;

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
        u32::try_from(line).map_err(|_error| anyhow!("document has too many lines"))?,
        u32::try_from(character).map_err(|_error| anyhow!("line is too long"))?,
    ))
}

pub fn byte_offset_for_lsp_position(
    source: &str,
    position: Position,
    encoding: PositionEncoding,
) -> Result<usize> {
    let target_line = usize::try_from(position.line).map_err(|_error| anyhow!("invalid line"))?;
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
    let target =
        usize::try_from(position.character).map_err(|_error| anyhow!("invalid character"))?;

    match encoding {
        PositionEncoding::Utf8 => {
            if target > line.len() {
                bail!(
                    "character {} is past the end of line {}",
                    position.character,
                    position.line
                );
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
                bail!(
                    "character {} is past the end of line {}",
                    position.character,
                    position.line
                )
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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
        Self(hex_digest(hasher.finalize()))
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

#[derive(Debug, Clone)]
pub struct ProjectedDocument {
    uri: Url,
    source: String,
    facts: FileDeclarationFacts,
}

impl ProjectedDocument {
    pub fn new(uri: Url, source: impl Into<String>, facts: FileDeclarationFacts) -> Self {
        Self {
            uri,
            source: source.into(),
            facts,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RelationshipScope {
    Workspace,
    ProjectedSubset,
}

#[derive(Debug, Clone)]
pub struct RelationshipProjectionIndex {
    scope: RelationshipScope,
    documents: Vec<ProjectedDocument>,
}

impl RelationshipProjectionIndex {
    pub fn new(
        scope: RelationshipScope,
        documents: impl IntoIterator<Item = ProjectedDocument>,
    ) -> Self {
        Self {
            scope,
            documents: documents.into_iter().collect(),
        }
    }

    pub const fn scope(&self) -> RelationshipScope {
        self.scope
    }

    fn declaration(&self, id: &DeclarationId) -> Option<(&ProjectedDocument, &DeclarationNode)> {
        self.documents.iter().find_map(|document| {
            document
                .facts
                .declarations()
                .iter()
                .find(|declaration| declaration.id == *id)
                .map(|declaration| (document, declaration))
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RelationshipMethod {
    Declaration,
    Definition,
    TypeDefinition,
    References,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RelationshipRequest {
    Resolve {
        method: RelationshipMethod,
        params: TextDocumentPositionParams,
    },
    References(ReferenceParams),
}

pub trait RelationshipBackend {
    fn supports(&self, capability: RelationshipCapability) -> bool;

    fn request(
        &mut self,
        request: RelationshipRequest,
    ) -> std::result::Result<Vec<Location>, ProviderError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelationshipProvenance {
    TypeUse {
        method: RelationshipMethod,
        role: TypeUseRole,
        scope: RelationshipScope,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipLocation {
    pub origin: Location,
    pub target: Option<Location>,
    pub provenance: RelationshipProvenance,
}

#[derive(Debug, Clone)]
pub struct RelationshipPlan {
    scope: RelationshipScope,
    encoding: PositionEncoding,
    kind: RelationshipPlanKind,
}

#[derive(Debug, Clone)]
enum RelationshipPlanKind {
    UsesTypes {
        source: DeclarationId,
        sites: Vec<PlannedTypeUse>,
    },
    UsedBy {
        source: DeclarationId,
        declaration: Location,
    },
}

#[derive(Debug, Clone)]
struct PlannedTypeUse {
    name: String,
    role: TypeUseRole,
    origin: Location,
    params: TextDocumentPositionParams,
}

#[derive(Debug, Clone)]
pub struct RelationshipExecution {
    kind: RelationshipExecutionKind,
}

#[derive(Debug, Clone)]
enum RelationshipExecutionKind {
    Unsupported(RelationshipCapability),
    UsesTypes(Vec<ExecutedTypeUse>),
    UsedBy(Vec<Location>),
}

#[derive(Debug, Clone)]
struct ExecutedTypeUse {
    method: RelationshipMethod,
    locations: Vec<Location>,
}

pub fn plan_uses_types(
    index: &RelationshipProjectionIndex,
    source: &DeclarationId,
    encoding: PositionEncoding,
) -> Result<RelationshipPlan> {
    let (document, declaration) = index
        .declaration(source)
        .ok_or_else(|| anyhow!("relationship source declaration is not projected"))?;
    let mut sites = Vec::with_capacity(declaration.type_use_sites.len());
    for site in &declaration.type_use_sites {
        let origin = location_for_byte_range(
            &document.uri,
            &document.source,
            site.source_range.clone(),
            encoding,
        )?;
        sites.push(PlannedTypeUse {
            name: site.name.clone(),
            role: site.role,
            params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: document.uri.clone(),
                },
                position: origin.range.start,
            },
            origin,
        });
    }
    Ok(RelationshipPlan {
        scope: index.scope,
        encoding,
        kind: RelationshipPlanKind::UsesTypes {
            source: source.clone(),
            sites,
        },
    })
}

pub fn plan_used_by(
    index: &RelationshipProjectionIndex,
    source: &DeclarationId,
    encoding: PositionEncoding,
) -> Result<RelationshipPlan> {
    let (document, declaration) = index
        .declaration(source)
        .ok_or_else(|| anyhow!("relationship source declaration is not projected"))?;
    let declaration = declaration_name_location(document, declaration, encoding)?;
    Ok(RelationshipPlan {
        scope: index.scope,
        encoding,
        kind: RelationshipPlanKind::UsedBy {
            source: source.clone(),
            declaration,
        },
    })
}

pub fn execute_relationship_plan(
    plan: &RelationshipPlan,
    backend: &mut impl RelationshipBackend,
) -> std::result::Result<RelationshipExecution, ProviderError> {
    let kind = match &plan.kind {
        RelationshipPlanKind::UsesTypes { sites, .. } => {
            let declaration_supported = backend.supports(RelationshipCapability::Declaration);
            let definition_supported = backend.supports(RelationshipCapability::Definition);
            let type_definition_supported =
                backend.supports(RelationshipCapability::TypeDefinition);
            if !declaration_supported && !definition_supported && !type_definition_supported {
                RelationshipExecutionKind::Unsupported(RelationshipCapability::Declaration)
            } else {
                let mut executed = Vec::with_capacity(sites.len());
                for site in sites {
                    let mut method = if declaration_supported {
                        RelationshipMethod::Declaration
                    } else if definition_supported {
                        RelationshipMethod::Definition
                    } else {
                        RelationshipMethod::TypeDefinition
                    };
                    let mut locations = backend.request(RelationshipRequest::Resolve {
                        method,
                        params: site.params.clone(),
                    })?;
                    if locations.is_empty()
                        && method == RelationshipMethod::Declaration
                        && definition_supported
                    {
                        method = RelationshipMethod::Definition;
                        locations = backend.request(RelationshipRequest::Resolve {
                            method,
                            params: site.params.clone(),
                        })?;
                    }
                    executed.push(ExecutedTypeUse { method, locations });
                }
                RelationshipExecutionKind::UsesTypes(executed)
            }
        }
        RelationshipPlanKind::UsedBy { declaration, .. } => {
            if !backend.supports(RelationshipCapability::References) {
                RelationshipExecutionKind::Unsupported(RelationshipCapability::References)
            } else {
                let locations =
                    backend.request(RelationshipRequest::References(ReferenceParams {
                        text_document_position: TextDocumentPositionParams {
                            text_document: TextDocumentIdentifier {
                                uri: declaration.uri.clone(),
                            },
                            position: declaration.range.start,
                        },
                        context: ReferenceContext {
                            include_declaration: false,
                        },
                        work_done_progress_params: WorkDoneProgressParams::default(),
                        partial_result_params: PartialResultParams::default(),
                    }))?;
                RelationshipExecutionKind::UsedBy(locations)
            }
        }
    };
    Ok(RelationshipExecution { kind })
}

pub fn reconcile_relationship_execution(
    plan: &RelationshipPlan,
    execution: RelationshipExecution,
    index: &RelationshipProjectionIndex,
) -> Result<RelationshipOutcome> {
    let RelationshipExecution { kind } = execution;
    if let RelationshipExecutionKind::Unsupported(capability) = kind {
        return Ok(RelationshipOutcome::Unsupported { capability });
    }
    match (&plan.kind, kind) {
        (
            RelationshipPlanKind::UsesTypes { source, sites },
            RelationshipExecutionKind::UsesTypes(executed),
        ) => reconcile_uses_types(plan, source, sites, executed, index),
        (
            RelationshipPlanKind::UsedBy {
                source,
                declaration,
            },
            RelationshipExecutionKind::UsedBy(locations),
        ) => reconcile_used_by(plan, source, declaration, locations, index),
        _ => bail!("relationship execution does not match its request plan"),
    }
}

fn reconcile_uses_types(
    plan: &RelationshipPlan,
    source: &DeclarationId,
    sites: &[PlannedTypeUse],
    executed: Vec<ExecutedTypeUse>,
    index: &RelationshipProjectionIndex,
) -> Result<RelationshipOutcome> {
    if sites.len() != executed.len() {
        bail!("relationship execution returned the wrong number of type-use results");
    }
    let mut edges: Vec<RelationshipEdge> = Vec::with_capacity(sites.len());
    for (site, executed) in sites.iter().zip(executed) {
        let method = executed.method;
        let locations = executed.locations;
        let unresolved = locations.is_empty().then_some(None);
        for location in locations.into_iter().map(Some).chain(unresolved) {
            let target = match location.as_ref() {
                Some(location) => reconcile_type_target(index, location, plan.encoding)?
                    .map(RelationshipTarget::InReview)
                    .unwrap_or_else(|| {
                        if index
                            .documents
                            .iter()
                            .any(|document| document.uri == location.uri)
                        {
                            RelationshipTarget::Unresolved {
                                name: site.name.clone(),
                            }
                        } else {
                            RelationshipTarget::External {
                                uri: location.uri.clone(),
                                range: location.range,
                            }
                        }
                    }),
                None => RelationshipTarget::Unresolved {
                    name: site.name.clone(),
                },
            };
            let relationship_location = RelationshipLocation {
                origin: site.origin.clone(),
                target: location,
                provenance: RelationshipProvenance::TypeUse {
                    method,
                    role: site.role,
                    scope: plan.scope,
                },
            };
            if let Some(edge) = edges.iter_mut().find(|edge| edge.target == target) {
                if !edge.locations.contains(&relationship_location) {
                    edge.locations.push(relationship_location);
                }
            } else {
                edges.push(RelationshipEdge {
                    kind: RelationshipKind::UsesType,
                    source: source.clone(),
                    target,
                    locations: vec![relationship_location],
                });
            }
        }
    }
    Ok(RelationshipOutcome::Complete { edges })
}

fn reconcile_used_by(
    plan: &RelationshipPlan,
    source: &DeclarationId,
    declaration: &Location,
    locations: Vec<Location>,
    index: &RelationshipProjectionIndex,
) -> Result<RelationshipOutcome> {
    let mut edges: Vec<RelationshipEdge> = Vec::new();
    for location in locations {
        let Some((owner, role)) = matching_type_use(index, &location, plan.encoding)? else {
            continue;
        };
        let relationship_location = RelationshipLocation {
            origin: declaration.clone(),
            target: Some(location),
            provenance: RelationshipProvenance::TypeUse {
                method: RelationshipMethod::References,
                role,
                scope: plan.scope,
            },
        };
        if let Some(edge) = edges
            .iter_mut()
            .find(|edge| matches!(&edge.target, RelationshipTarget::InReview(id) if id == &owner))
        {
            edge.locations.push(relationship_location);
        } else {
            edges.push(RelationshipEdge {
                kind: RelationshipKind::UsedBy,
                source: source.clone(),
                target: RelationshipTarget::InReview(owner),
                locations: vec![relationship_location],
            });
        }
    }
    Ok(RelationshipOutcome::Complete { edges })
}

fn reconcile_type_target(
    index: &RelationshipProjectionIndex,
    location: &Location,
    encoding: PositionEncoding,
) -> Result<Option<DeclarationId>> {
    for document in &index.documents {
        if document.uri != location.uri {
            continue;
        }
        for declaration in document.facts.declarations() {
            if is_type_declaration(declaration.kind)
                && declaration_name_location(document, declaration, encoding)? == *location
            {
                return Ok(Some(declaration.id.clone()));
            }
        }
    }
    Ok(None)
}

fn matching_type_use(
    index: &RelationshipProjectionIndex,
    location: &Location,
    encoding: PositionEncoding,
) -> Result<Option<(DeclarationId, TypeUseRole)>> {
    for document in &index.documents {
        if document.uri != location.uri {
            continue;
        }
        for declaration in document.facts.declarations() {
            for site in &declaration.type_use_sites {
                if location_for_byte_range(
                    &document.uri,
                    &document.source,
                    site.source_range.clone(),
                    encoding,
                )? == *location
                {
                    return Ok(Some((declaration.id.clone(), site.role)));
                }
            }
        }
    }
    Ok(None)
}

fn declaration_name_location(
    document: &ProjectedDocument,
    declaration: &DeclarationNode,
    encoding: PositionEncoding,
) -> Result<Location> {
    let source = document
        .source
        .get(declaration.source_span.clone())
        .ok_or_else(|| anyhow!("projected declaration span is outside its source document"))?;
    let relative = source
        .find(&declaration.name)
        .ok_or_else(|| anyhow!("projected declaration name is absent from its source span"))?;
    let start = declaration.source_span.start + relative;
    location_for_byte_range(
        &document.uri,
        &document.source,
        start..start + declaration.name.len(),
        encoding,
    )
}

fn location_for_byte_range(
    uri: &Url,
    source: &str,
    range: std::ops::Range<usize>,
    encoding: PositionEncoding,
) -> Result<Location> {
    if range.start > range.end {
        bail!("relationship source range is reversed");
    }
    Ok(Location {
        uri: uri.clone(),
        range: Range::new(
            lsp_position_for_byte_offset(source, range.start, encoding)?,
            lsp_position_for_byte_offset(source, range.end, encoding)?,
        ),
    })
}

const fn is_type_declaration(kind: DeclarationKind) -> bool {
    matches!(
        kind,
        DeclarationKind::Struct
            | DeclarationKind::Enum
            | DeclarationKind::Trait
            | DeclarationKind::Interface
            | DeclarationKind::Class
            | DeclarationKind::TypeAlias
            | DeclarationKind::AssociatedType
    )
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
    Unresolved { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipEdge {
    pub kind: RelationshipKind,
    pub source: DeclarationId,
    pub target: RelationshipTarget,
    pub locations: Vec<RelationshipLocation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RelationshipCapability {
    PrepareCallHierarchy,
    IncomingCalls,
    OutgoingCalls,
    Declaration,
    Definition,
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
