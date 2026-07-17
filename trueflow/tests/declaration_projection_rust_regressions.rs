use std::path::Path;

use anyhow::{Context, Result};
use trueflow::analysis::Language;
use trueflow::declaration::diff::{DeclarationChangeKind, diff_declarations};
use trueflow::declaration::snapshot::{
    PathPairEvidence, SnapshotId, SnapshotPair, SnapshotPairId, SourceSnapshot,
};
use trueflow::declaration::{
    Capability, DeclarationKind, DeclarationNode, FileDeclarationFacts, TypeUseRole, project_source,
};

fn project(source: &str) -> Result<FileDeclarationFacts> {
    project_source(Path::new("src/lib.rs"), Language::Rust, source)
}

fn declaration_named<'a>(
    facts: &'a FileDeclarationFacts,
    name: &str,
    kind: DeclarationKind,
) -> Result<&'a DeclarationNode> {
    facts
        .declarations()
        .iter()
        .find(|declaration| declaration.name == name && declaration.kind == kind)
        .with_context(|| format!("missing projected Rust {kind:?} declaration {name}"))
}

fn snapshot(id: &str, source: &str) -> SourceSnapshot {
    SourceSnapshot::new(
        SnapshotId::new(id),
        Path::new("src/lib.rs"),
        Language::Rust,
        source,
    )
}

fn same_path_pair(base: &str, head: &str) -> SnapshotPair {
    SnapshotPair::new(
        SnapshotPairId::new("trait-impl-documentation"),
        Some(snapshot("trait-impl-base", base)),
        Some(snapshot("trait-impl-head", head)),
        PathPairEvidence::SamePath,
    )
}

#[test]
fn trait_impl_methods_with_identical_signatures_have_distinct_qualified_keys() -> Result<()> {
    const SOURCE: &str = r#"pub struct S;
impl A for S {
fn run(&self) {}
}
impl B for S {
fn run(&self) {}
}
"#;

    let facts = project(SOURCE)?;
    let methods = facts
        .declarations()
        .iter()
        .filter(|declaration| {
            declaration.kind == DeclarationKind::Method && declaration.name == "run"
        })
        .collect::<Vec<_>>();

    assert_eq!(
        methods.len(),
        2,
        "both trait implementations must be inventoried"
    );
    assert_ne!(
        methods[0].key, methods[1].key,
        "impl A for S and impl B for S must qualify the same method signature with different declaration keys"
    );

    Ok(())
}

#[test]
fn trait_impl_documentation_edits_pair_with_their_qualified_methods() -> Result<()> {
    const BASE: &str = r#"pub struct S;
impl A for S {
/// A behavior before.
fn run(&self) {}
}
impl B for S {
/// B behavior before.
fn run(&self) {}
}
"#;
    const HEAD: &str = r#"pub struct S;
impl A for S {
/// A behavior after.
fn run(&self) {}
}
impl B for S {
/// B behavior after.
fn run(&self) {}
}
"#;

    let diff = diff_declarations(&[same_path_pair(BASE, HEAD)])?;
    let method_units = diff
        .units
        .iter()
        .filter(|unit| {
            unit.base
                .as_ref()
                .or(unit.head.as_ref())
                .is_some_and(|declaration| {
                    declaration.kind == DeclarationKind::Method && declaration.name == "run"
                })
        })
        .collect::<Vec<_>>();
    let mut pairings = method_units
        .iter()
        .filter_map(|unit| {
            Some((
                unit.change_kind,
                unit.base.as_ref()?.projection_text.as_str(),
                unit.head.as_ref()?.projection_text.as_str(),
            ))
        })
        .collect::<Vec<_>>();
    pairings.sort_unstable_by_key(|(_, base, _)| *base);

    assert_eq!(
        pairings,
        [
            (
                DeclarationChangeKind::Changed,
                "/// A behavior before.\nfn run(&self)",
                "/// A behavior after.\nfn run(&self)",
            ),
            (
                DeclarationChangeKind::Changed,
                "/// B behavior before.\nfn run(&self)",
                "/// B behavior after.\nfn run(&self)",
            ),
        ],
        "each documentation edit must remain paired with its trait-qualified method; method units: {method_units:#?}; diagnostics: {:#?}",
        diff.diagnostics
    );

    Ok(())
}

#[test]
fn rust_type_use_roles_reflect_each_declaration_surface_position() -> Result<()> {
    const SOURCE: &str = r#"pub fn convert(input: Input) -> Output {
    unreachable!()
}
pub struct Row {
    cell: Cell,
}
pub enum Message {
    Data(Payload),
}
pub trait Repository: ParentBound {}
"#;

    let facts = project(SOURCE)?;
    let declarations = [
        declaration_named(&facts, "convert", DeclarationKind::Function)?,
        declaration_named(&facts, "Row", DeclarationKind::Struct)?,
        declaration_named(&facts, "Message", DeclarationKind::Enum)?,
        declaration_named(&facts, "Repository", DeclarationKind::Trait)?,
    ];
    let actual = declarations
        .into_iter()
        .flat_map(|declaration| {
            declaration
                .type_use_sites
                .iter()
                .map(move |site| (declaration.name.as_str(), site.name.as_str(), site.role))
        })
        .collect::<Vec<_>>();

    assert_eq!(
        actual,
        [
            ("convert", "Input", TypeUseRole::Parameter),
            ("convert", "Output", TypeUseRole::Return),
            ("Row", "Cell", TypeUseRole::Field),
            ("Message", "Payload", TypeUseRole::Variant),
            ("Repository", "ParentBound", TypeUseRole::Bound),
        ],
        "Rust type uses must expose their syntactic role rather than collapsing to Other"
    );

    Ok(())
}

#[test]
fn named_rust_union_is_projected_or_explicitly_marks_aggregate_projection_partial() -> Result<()> {
    const SOURCE: &str = r#"pub union Number {
    integer: i32,
    float: f32,
}
"#;
    const EXPECTED_PROJECTION: &str = "pub union Number {\n    integer: i32,\n    float: f32,\n}";

    let facts = project(SOURCE)?;
    match &facts.capabilities.aggregate_projection {
        Capability::Complete => {
            let number = facts
                .declarations()
                .iter()
                .find(|declaration| declaration.name == "Number")
                .context(
                    "Rust aggregate projection is Complete but silently omitted named union Number",
                )?;
            assert!(
                matches!(
                    number.kind,
                    DeclarationKind::Struct
                        | DeclarationKind::Enum
                        | DeclarationKind::Interface
                        | DeclarationKind::Class
                ),
                "named union Number must be represented as an aggregate, got {:?}",
                number.kind
            );
            assert_eq!(
                number.projection_text, EXPECTED_PROJECTION,
                "a projected union must preserve its exact declaration surface"
            );
        }
        Capability::Partial {
            missing_features,
            diagnostics,
        } => {
            let explicitly_mentions_union = missing_features
                .iter()
                .chain(diagnostics.iter().map(|diagnostic| &diagnostic.message))
                .chain(
                    facts
                        .diagnostics
                        .iter()
                        .map(|diagnostic| &diagnostic.message),
                )
                .any(|message| message.to_ascii_lowercase().contains("union"));
            assert!(
                explicitly_mentions_union,
                "Partial Rust aggregate capability must explicitly diagnose omitted unions"
            );
        }
        capability => panic!(
            "Rust aggregate projection for a named union must be Complete with an exact aggregate or explicitly Partial, got {capability:?}"
        ),
    }

    Ok(())
}
