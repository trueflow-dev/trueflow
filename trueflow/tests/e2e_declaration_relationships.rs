use std::path::Path;

use anyhow::{Context, Result};
use async_lsp::lsp_types::{CallHierarchyItem, Position, Range as LspRange, SymbolKind, Url};
use serde_json::json;
use trueflow::analysis::Language;
use trueflow::declaration::relationships::{
    byte_offset_for_lsp_position, incoming_calls_params, lsp_position_for_byte_offset,
    outgoing_calls_params, reconcile_relationship_result, server_request_policy, start_session,
    DocumentHash, LaunchError, LspServerLauncher, LspServerProfile, PositionEncoding,
    RelationshipCapability, RelationshipEdge, RelationshipKind, RelationshipOutcome,
    RelationshipRequestKey, RelationshipResult, RelationshipState, RelationshipTarget,
    ServerRequestDecision, SessionState, SourceGeneration, WorkspaceTrust,
};
use trueflow::declaration::{DeclarationId, DeclarationKey};

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
        assert_eq!(profile.executable(), executable, "wrong executable for {language:?}");
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
        byte_offset_for_lsp_position(SOURCE, Position::new(0, 2), PositionEncoding::Utf8)
            .is_err(),
        "a UTF-8 code-unit position inside é must be rejected"
    );
    assert!(
        byte_offset_for_lsp_position(SOURCE, Position::new(0, 3), PositionEncoding::Utf16)
            .is_err(),
        "a UTF-16 position splitting 😀's surrogate pair must be rejected"
    );
    assert!(
        byte_offset_for_lsp_position(SOURCE, Position::new(0, 8), PositionEncoding::Utf8)
            .is_err(),
        "a position past the line ending must be rejected"
    );
    assert!(
        byte_offset_for_lsp_position(SOURCE, Position::new(2, 0), PositionEncoding::Utf16)
            .is_err(),
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
                diagnostics: vec!["one returned location no longer matches the document".to_owned()],
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
