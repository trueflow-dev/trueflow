use std::collections::{HashSet, VecDeque};
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};
use async_lsp::lsp_types::{
    CallHierarchyItem, Location, Position, Range as LspRange, ReferenceContext, ReferenceParams,
    SymbolKind, TextDocumentIdentifier, TextDocumentPositionParams, Url,
};
use serde_json::json;
use trueflow::analysis::Language;
use trueflow::declaration::relationships::{
    DocumentHash, LaunchError, LspServerLauncher, LspServerProfile, PositionEncoding,
    ProjectedDocument, ProviderError, RelationshipBackend, RelationshipCapability,
    RelationshipEdge, RelationshipKind, RelationshipLocation, RelationshipMethod,
    RelationshipOutcome, RelationshipProjectionIndex, RelationshipProvenance, RelationshipRequest,
    RelationshipRequestKey, RelationshipResult, RelationshipScope, RelationshipState,
    RelationshipTarget, ServerRequestDecision, SessionState, SourceGeneration, WorkspaceTrust,
    byte_offset_for_lsp_position, execute_relationship_plan, incoming_calls_params,
    lsp_position_for_byte_offset, outgoing_calls_params, plan_used_by, plan_uses_types,
    reconcile_relationship_execution, reconcile_relationship_result, server_request_policy,
    start_session,
};
use trueflow::declaration::{
    DeclarationId, DeclarationKey, DeclarationKind, DeclarationNode, TypeUseRole, project_source,
};

#[test]
fn common_five_profiles_pin_executable_argv_and_language_id() -> Result<()> {
    let cases = [
        (
            Language::Rust,
            LspServerProfile::RustAnalyzer,
            "rust-analyzer",
            &[][..],
            "rust",
        ),
        (
            Language::TypeScript,
            LspServerProfile::TypeScriptLanguageServer,
            "typescript-language-server",
            &["--stdio"][..],
            "typescript",
        ),
        (
            Language::Python,
            LspServerProfile::Pylsp,
            "pylsp",
            &[][..],
            "python",
        ),
        (
            Language::Go,
            LspServerProfile::Gopls,
            "gopls",
            &[][..],
            "go",
        ),
        (
            Language::C,
            LspServerProfile::Clangd,
            "clangd",
            &[][..],
            "c",
        ),
        (
            Language::Cpp,
            LspServerProfile::Clangd,
            "clangd",
            &[][..],
            "cpp",
        ),
    ];

    for (language, expected_profile, executable, argv, language_id) in cases {
        let profile = LspServerProfile::for_language(language)
            .with_context(|| format!("missing fixed LSP profile for {language:?}"))?;
        assert_eq!(profile, expected_profile, "wrong profile for {language:?}");
        assert_eq!(
            profile.executable(),
            executable,
            "wrong executable for {language:?}"
        );
        assert_eq!(profile.argv(), argv, "wrong argv for {language:?}");
        assert_eq!(
            profile.language_id(language),
            Some(language_id),
            "wrong languageId for {language:?}"
        );
    }

    assert_eq!(
        LspServerProfile::for_language(Language::Java),
        None,
        "languages outside the fixed profiles must not fall back to an arbitrary server"
    );
    Ok(())
}

struct PanicLauncher;

impl LspServerLauncher for PanicLauncher {
    fn spawn(
        &mut self,
        _profile: LspServerProfile,
        _language: Language,
        _workspace_root: &Path,
    ) -> std::result::Result<(), LaunchError> {
        panic!("an unconfigured or untrusted workspace reached process spawn")
    }
}

struct FailingLauncher;

impl LspServerLauncher for FailingLauncher {
    fn spawn(
        &mut self,
        _profile: LspServerProfile,
        _language: Language,
        _workspace_root: &Path,
    ) -> std::result::Result<(), LaunchError> {
        Err(LaunchError::new("server exited before initialize"))
    }
}

#[test]
fn unconfigured_and_untrusted_sessions_never_spawn_and_launch_failures_remain_distinct() {
    let workspace = Path::new("/review/workspace");
    let mut must_not_spawn = PanicLauncher;

    assert_eq!(
        start_session(
            None,
            Language::Rust,
            workspace,
            WorkspaceTrust::TrustedForInvocation,
            &mut must_not_spawn,
        ),
        SessionState::Unconfigured
    );
    assert_eq!(
        start_session(
            Some(LspServerProfile::RustAnalyzer),
            Language::Rust,
            workspace,
            WorkspaceTrust::Untrusted,
            &mut must_not_spawn,
        ),
        SessionState::Untrusted
    );

    let mut failing = FailingLauncher;
    assert_eq!(
        start_session(
            Some(LspServerProfile::RustAnalyzer),
            Language::Rust,
            workspace,
            WorkspaceTrust::TrustedForInvocation,
            &mut failing,
        ),
        SessionState::Failed {
            message: "server exited before initialize".to_owned(),
        }
    );
}

#[test]
fn server_request_policy_denies_workspace_mutation_and_command_execution() {
    assert_eq!(
        server_request_policy("workspace/applyEdit"),
        ServerRequestDecision::RejectApplyEdit { applied: false },
        "the provider must never accept a server-authored workspace edit"
    );
    assert_eq!(
        server_request_policy("workspace/executeCommand"),
        ServerRequestDecision::RejectMethod,
        "the provider must never execute a server-supplied workspace command"
    );
}

#[test]
fn negotiated_positions_round_trip_exact_utf8_source_boundaries() -> Result<()> {
    const SOURCE: &str = "aé😀\nβz";
    let cases = [
        (0, Position::new(0, 0), Position::new(0, 0)),
        (1, Position::new(0, 1), Position::new(0, 1)),
        (3, Position::new(0, 3), Position::new(0, 2)),
        (7, Position::new(0, 7), Position::new(0, 4)),
        (8, Position::new(1, 0), Position::new(1, 0)),
        (10, Position::new(1, 2), Position::new(1, 1)),
        (11, Position::new(1, 3), Position::new(1, 2)),
    ];

    for (byte_offset, utf8, utf16) in cases {
        for (encoding, expected) in [
            (PositionEncoding::Utf8, utf8),
            (PositionEncoding::Utf16, utf16),
        ] {
            assert_eq!(
                lsp_position_for_byte_offset(SOURCE, byte_offset, encoding)?,
                expected,
                "wrong {encoding:?} position for source byte {byte_offset}"
            );
            assert_eq!(
                byte_offset_for_lsp_position(SOURCE, expected, encoding)?,
                byte_offset,
                "{encoding:?} position did not round-trip source byte {byte_offset}"
            );
        }
    }

    Ok(())
}

#[test]
fn negotiated_positions_reject_split_code_points_and_surrogate_pairs() {
    const SOURCE: &str = "aé😀\nβz";

    assert!(
        lsp_position_for_byte_offset(SOURCE, 2, PositionEncoding::Utf8).is_err(),
        "a byte offset inside é must be rejected"
    );
    assert!(
        lsp_position_for_byte_offset(SOURCE, 4, PositionEncoding::Utf16).is_err(),
        "a byte offset inside 😀 must be rejected"
    );
    assert!(
        byte_offset_for_lsp_position(SOURCE, Position::new(0, 2), PositionEncoding::Utf8).is_err(),
        "a UTF-8 code-unit position inside é must be rejected"
    );
    assert!(
        byte_offset_for_lsp_position(SOURCE, Position::new(0, 3), PositionEncoding::Utf16).is_err(),
        "a UTF-16 position splitting 😀's surrogate pair must be rejected"
    );
    assert!(
        byte_offset_for_lsp_position(SOURCE, Position::new(0, 8), PositionEncoding::Utf8).is_err(),
        "a position past the line ending must be rejected"
    );
    assert!(
        byte_offset_for_lsp_position(SOURCE, Position::new(2, 0), PositionEncoding::Utf16).is_err(),
        "a position on a nonexistent line must be rejected"
    );
}

fn request_key(generation: u64, source: &str) -> Result<RelationshipRequestKey> {
    Ok(RelationshipRequestKey {
        source_generation: SourceGeneration::new(generation),
        server_profile: LspServerProfile::RustAnalyzer,
        declaration_id: DeclarationId::new("declaration:caller"),
        declaration_key: DeclarationKey::new("function:caller"),
        document_uri: Url::parse("file:///review/workspace/src/lib.rs")?,
        document_version: 7,
        document_hash: DocumentHash::from_bytes(source.as_bytes()),
    })
}

fn call_edge() -> RelationshipEdge {
    RelationshipEdge {
        kind: RelationshipKind::Calls,
        source: DeclarationId::new("declaration:caller"),
        target: RelationshipTarget::InReview(DeclarationId::new("declaration:callee")),
        locations: Vec::new(),
    }
}

#[test]
fn relationship_keys_change_with_source_generation_and_document_content() -> Result<()> {
    let original = request_key(41, "fn caller() { callee(); }")?;
    let next_generation = request_key(42, "fn caller() { callee(); }")?;
    let edited_document = request_key(41, "fn caller() { other(); }")?;

    assert_ne!(
        original, next_generation,
        "source generation must participate in request identity"
    );
    assert_ne!(
        original, edited_document,
        "exact document content hash must participate in request identity"
    );
    Ok(())
}

#[test]
fn stale_results_are_rejected_for_generation_or_document_hash_mismatch() -> Result<()> {
    let accepted_key = request_key(42, "fn caller() { current(); }")?;
    let stale_cases = [
        (
            "source generation",
            request_key(41, "fn caller() { current(); }")?,
        ),
        (
            "document hash",
            request_key(42, "fn caller() { previous(); }")?,
        ),
    ];

    for (reason, stale_key) in stale_cases {
        let result = RelationshipResult {
            key: stale_key.clone(),
            outcome: RelationshipOutcome::Complete {
                edges: vec![call_edge()],
            },
        };
        assert_eq!(
            reconcile_relationship_result(&accepted_key, result),
            RelationshipState::Stale {
                expected: accepted_key.clone(),
                received: stale_key,
            },
            "a stale {reason} result must not expose its graph"
        );
    }

    Ok(())
}

#[test]
fn unsupported_empty_failed_and_partial_relationship_results_remain_distinct() -> Result<()> {
    let key = request_key(42, "fn caller() { callee(); }")?;

    let unsupported = reconcile_relationship_result(
        &key,
        RelationshipResult {
            key: key.clone(),
            outcome: RelationshipOutcome::Unsupported {
                capability: RelationshipCapability::IncomingCalls,
            },
        },
    );
    let successful_empty = reconcile_relationship_result(
        &key,
        RelationshipResult {
            key: key.clone(),
            outcome: RelationshipOutcome::Complete { edges: Vec::new() },
        },
    );
    let partial = reconcile_relationship_result(
        &key,
        RelationshipResult {
            key: key.clone(),
            outcome: RelationshipOutcome::Partial {
                edges: vec![call_edge()],
                diagnostics: vec![
                    "one returned location no longer matches the document".to_owned(),
                ],
            },
        },
    );
    let failed = reconcile_relationship_result(
        &key,
        RelationshipResult {
            key: key.clone(),
            outcome: RelationshipOutcome::Failed {
                message: "server closed the request".to_owned(),
            },
        },
    );

    assert_eq!(
        unsupported,
        RelationshipState::Unsupported {
            key: key.clone(),
            capability: RelationshipCapability::IncomingCalls,
        }
    );
    assert_eq!(
        successful_empty,
        RelationshipState::Ready {
            key: key.clone(),
            edges: Vec::new(),
        },
        "a supported query with no edges is a successful empty result"
    );
    assert_eq!(
        partial,
        RelationshipState::Partial {
            key: key.clone(),
            edges: vec![call_edge()],
            diagnostics: vec!["one returned location no longer matches the document".to_owned()],
        },
        "reconciled edges must survive alongside diagnostics"
    );
    assert_eq!(
        failed,
        RelationshipState::Failed {
            key,
            message: "server closed the request".to_owned(),
        }
    );

    Ok(())
}

fn prepared_item(name: &str, data: Option<serde_json::Value>) -> Result<CallHierarchyItem> {
    Ok(CallHierarchyItem {
        name: name.to_owned(),
        kind: SymbolKind::FUNCTION,
        tags: None,
        detail: Some(format!("fn {name}()")),
        uri: Url::parse("file:///review/workspace/src/lib.rs")?,
        range: LspRange::new(Position::new(0, 0), Position::new(2, 1)),
        selection_range: LspRange::new(Position::new(0, 3), Position::new(0, 9)),
        data,
    })
}

#[test]
fn call_hierarchy_followups_preserve_the_prepared_item_including_opaque_data() -> Result<()> {
    let opaque = json!({
        "serverHandle": [17, "opaque"],
        "nested": { "mustRoundTrip": true }
    });
    let prepared = prepared_item("caller", Some(opaque))?;

    assert_eq!(
        incoming_calls_params(prepared.clone()).item,
        prepared,
        "incomingCalls must receive the exact item returned by prepareCallHierarchy"
    );
    assert_eq!(
        outgoing_calls_params(prepared.clone()).item,
        prepared,
        "outgoingCalls must receive the exact item returned by prepareCallHierarchy"
    );
    Ok(())
}

#[derive(Debug)]
struct FakeRelationshipBackend {
    capabilities: HashSet<RelationshipCapability>,
    replies: VecDeque<std::result::Result<Vec<Location>, ProviderError>>,
    requests: Vec<RelationshipRequest>,
}

impl FakeRelationshipBackend {
    fn new(
        capabilities: impl IntoIterator<Item = RelationshipCapability>,
        replies: impl IntoIterator<Item = Vec<Location>>,
    ) -> Self {
        Self {
            capabilities: capabilities.into_iter().collect(),
            replies: replies.into_iter().map(Ok).collect(),
            requests: Vec::new(),
        }
    }

    fn assert_exhausted(&self) {
        assert!(
            self.replies.is_empty(),
            "the planner skipped {} scripted relationship replies",
            self.replies.len()
        );
    }
}

impl RelationshipBackend for FakeRelationshipBackend {
    fn supports(&self, capability: RelationshipCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    fn request(
        &mut self,
        request: RelationshipRequest,
    ) -> std::result::Result<Vec<Location>, ProviderError> {
        self.requests.push(request);
        self.replies.pop_front().unwrap_or_else(|| {
            Err(ProviderError::Protocol(
                "relationship planner issued an unscripted request".to_owned(),
            ))
        })
    }
}

fn declaration_named<'a>(
    declarations: &'a [DeclarationNode],
    name: &str,
    kind: DeclarationKind,
) -> &'a DeclarationNode {
    declarations
        .iter()
        .find(|declaration| declaration.name == name && declaration.kind == kind)
        .unwrap_or_else(|| panic!("missing projected {kind:?} declaration {name}"))
}

fn nth_byte_range(source: &str, needle: &str, occurrence: usize) -> Range<usize> {
    let start = source
        .match_indices(needle)
        .nth(occurrence)
        .map(|(start, _)| start)
        .unwrap_or_else(|| panic!("missing occurrence {occurrence} of {needle:?}"));
    start..start + needle.len()
}

fn location_at(
    uri: &Url,
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
) -> Location {
    Location {
        uri: uri.clone(),
        range: LspRange::new(
            Position::new(start_line, start_character),
            Position::new(end_line, end_character),
        ),
    }
}

fn resolve_request(
    method: RelationshipMethod,
    uri: &Url,
    line: u32,
    character: u32,
) -> RelationshipRequest {
    RelationshipRequest::Resolve {
        method,
        params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: Position::new(line, character),
        },
    }
}

fn references_request(
    uri: &Url,
    line: u32,
    character: u32,
    include_declaration: bool,
) -> RelationshipRequest {
    RelationshipRequest::References(ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: Position::new(line, character),
        },
        context: ReferenceContext {
            include_declaration,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    })
}

#[test]
fn type_definition_only_server_resolves_projected_type_use() -> Result<()> {
    const SOURCE: &str = concat!(
        "export interface Payload {}\n",
        "export function decode(input: Payload) {}\n",
    );
    let uri = Url::parse("file:///review/workspace/src/payload.ts")?;
    let facts = project_source(Path::new("src/payload.ts"), Language::TypeScript, SOURCE)?;
    let payload =
        declaration_named(facts.declarations(), "Payload", DeclarationKind::Interface).clone();
    let decode =
        declaration_named(facts.declarations(), "decode", DeclarationKind::Function).clone();
    let document = ProjectedDocument::new(uri.clone(), SOURCE, facts);
    let index = RelationshipProjectionIndex::new(RelationshipScope::Workspace, [document]);
    let plan = plan_uses_types(&index, &decode.id, PositionEncoding::Utf16)?;

    let payload_declaration = location_at(&uri, 0, 17, 0, 24);
    let mut backend = FakeRelationshipBackend::new(
        [RelationshipCapability::TypeDefinition],
        [vec![payload_declaration.clone()]],
    );
    let execution = execute_relationship_plan(&plan, &mut backend)?;

    assert_eq!(
        backend.requests,
        vec![resolve_request(
            RelationshipMethod::TypeDefinition,
            &uri,
            1,
            30,
        )],
        "a TypeDefinition-only server must receive the projected type-use request"
    );
    backend.assert_exhausted();
    assert_eq!(
        reconcile_relationship_execution(&plan, execution, &index)?,
        RelationshipOutcome::Complete {
            edges: vec![RelationshipEdge {
                kind: RelationshipKind::UsesType,
                source: decode.id,
                target: RelationshipTarget::InReview(payload.id),
                locations: vec![RelationshipLocation {
                    origin: location_at(&uri, 1, 30, 1, 37),
                    target: Some(payload_declaration),
                    provenance: RelationshipProvenance::TypeUse {
                        method: RelationshipMethod::TypeDefinition,
                        role: TypeUseRole::Parameter,
                        scope: RelationshipScope::Workspace,
                    },
                }],
            }],
        },
        "TypeDefinition is a complete UsesTypes resolution path, not an unsupported fallback"
    );
    Ok(())
}

#[test]
fn uses_types_preserves_every_location_from_one_resolution_response_in_order() -> Result<()> {
    const SOURCE: &str = "export function decode(input: ExternalPayload) {}\n";
    let uri = Url::parse("file:///review/workspace/src/decode.ts")?;
    let facts = project_source(Path::new("src/decode.ts"), Language::TypeScript, SOURCE)?;
    let decode =
        declaration_named(facts.declarations(), "decode", DeclarationKind::Function).clone();
    let document = ProjectedDocument::new(uri.clone(), SOURCE, facts);
    let index = RelationshipProjectionIndex::new(RelationshipScope::Workspace, [document]);
    let plan = plan_uses_types(&index, &decode.id, PositionEncoding::Utf16)?;

    let first_uri = Url::parse("file:///registry/types-a.d.ts")?;
    let second_uri = Url::parse("file:///registry/types-b.d.ts")?;
    let first_declaration = location_at(&first_uri, 2, 4, 2, 19);
    let second_declaration = location_at(&second_uri, 7, 9, 7, 24);
    let mut backend = FakeRelationshipBackend::new(
        [RelationshipCapability::Declaration],
        [vec![first_declaration.clone(), second_declaration.clone()]],
    );
    let execution = execute_relationship_plan(&plan, &mut backend)?;

    assert_eq!(
        backend.requests,
        vec![resolve_request(
            RelationshipMethod::Declaration,
            &uri,
            0,
            30,
        )]
    );
    backend.assert_exhausted();
    let origin = location_at(&uri, 0, 30, 0, 45);
    assert_eq!(
        reconcile_relationship_execution(&plan, execution, &index)?,
        RelationshipOutcome::Complete {
            edges: vec![
                RelationshipEdge {
                    kind: RelationshipKind::UsesType,
                    source: decode.id.clone(),
                    target: RelationshipTarget::External {
                        uri: first_uri,
                        range: first_declaration.range,
                    },
                    locations: vec![RelationshipLocation {
                        origin: origin.clone(),
                        target: Some(first_declaration),
                        provenance: RelationshipProvenance::TypeUse {
                            method: RelationshipMethod::Declaration,
                            role: TypeUseRole::Parameter,
                            scope: RelationshipScope::Workspace,
                        },
                    }],
                },
                RelationshipEdge {
                    kind: RelationshipKind::UsesType,
                    source: decode.id,
                    target: RelationshipTarget::External {
                        uri: second_uri,
                        range: second_declaration.range,
                    },
                    locations: vec![RelationshipLocation {
                        origin,
                        target: Some(second_declaration),
                        provenance: RelationshipProvenance::TypeUse {
                            method: RelationshipMethod::Declaration,
                            role: TypeUseRole::Parameter,
                            scope: RelationshipScope::Workspace,
                        },
                    }],
                },
            ],
        },
        "every legal location in one type-use response must survive reconciliation in response order"
    );
    Ok(())
}

#[test]
fn uses_types_queries_only_projected_type_tokens_and_reconciles_exact_targets_with_method_provenance()
-> Result<()> {
    const SOURCE: &str = concat!(
        "export interface Input {}\n",
        "const sameSpelling = \"Input\";\n",
        "\n",
        "export class Mapper {\n",
        "    map(/*😀*/ first: Input, second: Input): ExternalType | Missing {\n",
        "        throw new Error();\n",
        "    }\n",
        "}\n",
    );
    let uri = Url::parse("file:///review/workspace/src/mapper.ts")?;
    let facts = project_source(Path::new("src/mapper.ts"), Language::TypeScript, SOURCE)?;
    let input =
        declaration_named(facts.declarations(), "Input", DeclarationKind::Interface).clone();
    let mapper = declaration_named(facts.declarations(), "map", DeclarationKind::Method).clone();
    let document = ProjectedDocument::new(uri.clone(), SOURCE, facts);
    let index = RelationshipProjectionIndex::new(RelationshipScope::Workspace, [document]);
    let plan = plan_uses_types(&index, &mapper.id, PositionEncoding::Utf16)?;

    assert_eq!(
        mapper
            .type_use_sites
            .iter()
            .map(|site| (site.name.as_str(), site.role, site.source_range.clone()))
            .collect::<Vec<_>>(),
        vec![
            (
                "Input",
                TypeUseRole::Parameter,
                nth_byte_range(SOURCE, "Input", 2)
            ),
            (
                "Input",
                TypeUseRole::Parameter,
                nth_byte_range(SOURCE, "Input", 3)
            ),
            (
                "ExternalType",
                TypeUseRole::Return,
                nth_byte_range(SOURCE, "ExternalType", 0),
            ),
            (
                "Missing",
                TypeUseRole::Return,
                nth_byte_range(SOURCE, "Missing", 0),
            ),
        ],
        "the request plan must originate only from projector-classified declaration-surface type uses"
    );

    let input_declaration = location_at(&uri, 0, 17, 0, 22);
    let same_spelling_value = location_at(&uri, 1, 22, 1, 27);
    let external_uri = Url::parse("file:///registry/types/index.d.ts")?;
    let external_declaration = location_at(&external_uri, 2, 4, 2, 16);
    let mut backend = FakeRelationshipBackend::new(
        [
            RelationshipCapability::Declaration,
            RelationshipCapability::Definition,
            RelationshipCapability::TypeDefinition,
        ],
        [
            vec![input_declaration.clone()],
            vec![same_spelling_value.clone()],
            vec![],
            vec![external_declaration.clone()],
            vec![],
            vec![],
        ],
    );
    let execution = execute_relationship_plan(&plan, &mut backend)?;

    assert_eq!(
        backend.requests,
        vec![
            resolve_request(RelationshipMethod::Declaration, &uri, 4, 22),
            resolve_request(RelationshipMethod::Declaration, &uri, 4, 37),
            resolve_request(RelationshipMethod::Declaration, &uri, 4, 45),
            resolve_request(RelationshipMethod::Definition, &uri, 4, 45),
            resolve_request(RelationshipMethod::Declaration, &uri, 4, 60),
            resolve_request(RelationshipMethod::Definition, &uri, 4, 60),
        ],
        "UTF-16 request positions must come from exact projected byte ranges; definition is only the declaration fallback"
    );
    backend.assert_exhausted();

    let input_use = location_at(&uri, 4, 22, 4, 27);
    let second_input_use = location_at(&uri, 4, 37, 4, 42);
    let external_use = location_at(&uri, 4, 45, 4, 57);
    let missing_use = location_at(&uri, 4, 60, 4, 67);
    assert_eq!(
        reconcile_relationship_execution(&plan, execution, &index)?,
        RelationshipOutcome::Complete {
            edges: vec![
                RelationshipEdge {
                    kind: RelationshipKind::UsesType,
                    source: mapper.id.clone(),
                    target: RelationshipTarget::InReview(input.id),
                    locations: vec![RelationshipLocation {
                        origin: input_use,
                        target: Some(input_declaration),
                        provenance: RelationshipProvenance::TypeUse {
                            method: RelationshipMethod::Declaration,
                            role: TypeUseRole::Parameter,
                            scope: RelationshipScope::Workspace,
                        },
                    }],
                },
                RelationshipEdge {
                    kind: RelationshipKind::UsesType,
                    source: mapper.id.clone(),
                    target: RelationshipTarget::Unresolved {
                        name: "Input".to_owned(),
                    },
                    locations: vec![RelationshipLocation {
                        origin: second_input_use,
                        target: Some(same_spelling_value),
                        provenance: RelationshipProvenance::TypeUse {
                            method: RelationshipMethod::Declaration,
                            role: TypeUseRole::Parameter,
                            scope: RelationshipScope::Workspace,
                        },
                    }],
                },
                RelationshipEdge {
                    kind: RelationshipKind::UsesType,
                    source: mapper.id.clone(),
                    target: RelationshipTarget::External {
                        uri: external_uri,
                        range: external_declaration.range,
                    },
                    locations: vec![RelationshipLocation {
                        origin: external_use,
                        target: Some(external_declaration),
                        provenance: RelationshipProvenance::TypeUse {
                            method: RelationshipMethod::Definition,
                            role: TypeUseRole::Return,
                            scope: RelationshipScope::Workspace,
                        },
                    }],
                },
                RelationshipEdge {
                    kind: RelationshipKind::UsesType,
                    source: mapper.id,
                    target: RelationshipTarget::Unresolved {
                        name: "Missing".to_owned(),
                    },
                    locations: vec![RelationshipLocation {
                        origin: missing_use,
                        target: None,
                        provenance: RelationshipProvenance::TypeUse {
                            method: RelationshipMethod::Definition,
                            role: TypeUseRole::Return,
                            scope: RelationshipScope::Workspace,
                        },
                    }],
                },
            ],
        },
        "same-spelled values must not reconcile as type declarations, while external and unresolved targets retain exact provenance"
    );
    Ok(())
}

#[test]
fn used_by_keeps_only_declaration_surface_type_references_and_labels_subset_scope() -> Result<()> {
    const SOURCE: &str = concat!(
        "export interface Payload {}\n",
        "\n",
        "export function decode(/*😀*/ input: Payload): Payload {\n",
        "    console.log(Payload);\n",
        "    const local: Payload = input;\n",
        "    return input;\n",
        "}\n",
    );
    let uri = Url::parse("file:///review/workspace/src/payload.ts")?;
    let facts = project_source(Path::new("src/payload.ts"), Language::TypeScript, SOURCE)?;
    let payload =
        declaration_named(facts.declarations(), "Payload", DeclarationKind::Interface).clone();
    let decode =
        declaration_named(facts.declarations(), "decode", DeclarationKind::Function).clone();
    let document = ProjectedDocument::new(uri.clone(), SOURCE, facts);
    let index = RelationshipProjectionIndex::new(RelationshipScope::ProjectedSubset, [document]);
    let plan = plan_used_by(&index, &payload.id, PositionEncoding::Utf16)?;

    let payload_declaration = location_at(&uri, 0, 17, 0, 24);
    let ordinary_value_reference = location_at(&uri, 3, 16, 3, 23);
    let parameter_type_use = location_at(&uri, 2, 37, 2, 44);
    let return_type_use = location_at(&uri, 2, 47, 2, 54);
    let executable_body_type_use = location_at(&uri, 4, 17, 4, 24);
    let mut backend = FakeRelationshipBackend::new(
        [RelationshipCapability::References],
        [vec![
            payload_declaration.clone(),
            ordinary_value_reference,
            parameter_type_use.clone(),
            return_type_use.clone(),
            executable_body_type_use,
        ]],
    );
    let execution = execute_relationship_plan(&plan, &mut backend)?;

    assert_eq!(
        backend.requests,
        vec![references_request(&uri, 0, 17, false)],
        "UsedBy must start at the reconciled type declaration and explicitly exclude declaration locations"
    );
    backend.assert_exhausted();
    assert_eq!(
        reconcile_relationship_execution(&plan, execution, &index)?,
        RelationshipOutcome::Complete {
            edges: vec![RelationshipEdge {
                kind: RelationshipKind::UsedBy,
                source: payload.id,
                target: RelationshipTarget::InReview(decode.id),
                locations: vec![
                    RelationshipLocation {
                        origin: payload_declaration.clone(),
                        target: Some(parameter_type_use),
                        provenance: RelationshipProvenance::TypeUse {
                            method: RelationshipMethod::References,
                            role: TypeUseRole::Parameter,
                            scope: RelationshipScope::ProjectedSubset,
                        },
                    },
                    RelationshipLocation {
                        origin: payload_declaration,
                        target: Some(return_type_use),
                        provenance: RelationshipProvenance::TypeUse {
                            method: RelationshipMethod::References,
                            role: TypeUseRole::Return,
                            scope: RelationshipScope::ProjectedSubset,
                        },
                    },
                ],
            }],
        },
        "references in value positions, the declaration itself, and executable bodies must be filtered even when the server returns them"
    );
    Ok(())
}

#[test]
fn absent_references_capability_is_unsupported_but_supported_empty_is_complete() -> Result<()> {
    const SOURCE: &str = "export interface Payload {}\n";
    let uri = Url::parse("file:///review/workspace/src/payload.ts")?;
    let facts = project_source(Path::new("src/payload.ts"), Language::TypeScript, SOURCE)?;
    let payload =
        declaration_named(facts.declarations(), "Payload", DeclarationKind::Interface).clone();
    let document = ProjectedDocument::new(uri.clone(), SOURCE, facts);
    let index = RelationshipProjectionIndex::new(RelationshipScope::Workspace, [document]);
    let plan = plan_used_by(&index, &payload.id, PositionEncoding::Utf16)?;

    let mut unsupported_backend = FakeRelationshipBackend::new([], []);
    let unsupported_execution = execute_relationship_plan(&plan, &mut unsupported_backend)?;
    assert!(
        unsupported_backend.requests.is_empty(),
        "an absent references capability must not be queried"
    );
    assert_eq!(
        reconcile_relationship_execution(&plan, unsupported_execution, &index)?,
        RelationshipOutcome::Unsupported {
            capability: RelationshipCapability::References,
        }
    );

    let mut empty_backend = FakeRelationshipBackend::new(
        [RelationshipCapability::References],
        [Vec::<Location>::new()],
    );
    let empty_execution = execute_relationship_plan(&plan, &mut empty_backend)?;
    assert_eq!(
        empty_backend.requests,
        vec![references_request(&uri, 0, 17, false)]
    );
    empty_backend.assert_exhausted();
    assert_eq!(
        reconcile_relationship_execution(&plan, empty_execution, &index)?,
        RelationshipOutcome::Complete { edges: Vec::new() },
        "a supported references request with no locations is a successful empty relationship result"
    );
    Ok(())
}
