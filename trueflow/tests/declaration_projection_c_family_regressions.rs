use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use trueflow::analysis::Language;
use trueflow::declaration::diff::{
    DeclarationChangeKind, DeclarationDiffUnit, MatchingEvidence, diff_declarations,
};
use trueflow::declaration::snapshot::{
    PathPairEvidence, SnapshotId, SnapshotPair, SnapshotPairId, SourceSnapshot,
};
use trueflow::declaration::{
    DeclarationKind, DeclarationNode, FileDeclarationFacts, SourceComponentRole, TypeUseRole,
    project_source,
};

fn project(path: &str, language: Language, source: &str) -> Result<FileDeclarationFacts> {
    project_source(Path::new(path), language, source)
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
        .with_context(|| format!("missing {kind:?} declaration named {name}"))
}

fn exact_range(source: &str, exact: &str) -> Result<Range<usize>> {
    let start = source
        .find(exact)
        .with_context(|| format!("fixture does not contain {exact:?}"))?;
    ensure!(
        !source[start + exact.len()..].contains(exact),
        "fixture contains {exact:?} more than once"
    );
    Ok(start..start + exact.len())
}

fn merged_component_ranges(declaration: &DeclarationNode) -> Vec<Range<usize>> {
    let mut merged = Vec::<Range<usize>>::new();
    for component in &declaration.components {
        if let Some(previous) = merged.last_mut()
            && previous.end == component.source_range.start
        {
            previous.end = component.source_range.end;
        } else {
            merged.push(component.source_range.clone());
        }
    }
    merged
}

fn assert_single_component(
    declaration: &DeclarationNode,
    source: &str,
    role: SourceComponentRole,
    exact: &str,
) -> Result<()> {
    let [component] = declaration.components.as_slice() else {
        anyhow::bail!(
            "expected one component for {} but got {:?}",
            declaration.name,
            declaration.components
        );
    };
    assert_eq!(component.role, role);
    assert_eq!(component.source_range, exact_range(source, exact)?);
    assert_eq!(component.text, exact);
    Ok(())
}

fn cpp_same_path_pair(pair_id: &str, base: &str, head: &str) -> SnapshotPair {
    let snapshot = |suffix: &str, source: &str| {
        SourceSnapshot::new(
            SnapshotId::new(format!("{pair_id}-{suffix}")),
            Path::new("src/conversion.cpp"),
            Language::Cpp,
            source,
        )
    };
    SnapshotPair::new(
        SnapshotPairId::new(pair_id),
        Some(snapshot("base", base)),
        Some(snapshot("head", head)),
        PathPairEvidence::SamePath,
    )
}

fn changed_unit_named<'a>(
    units: &'a [DeclarationDiffUnit],
    signature: &str,
) -> Result<&'a DeclarationDiffUnit> {
    units
        .iter()
        .find(|unit| {
            unit.head
                .as_ref()
                .is_some_and(|declaration| declaration.projection_text.ends_with(signature))
        })
        .with_context(|| format!("missing changed diff unit for {signature}"))
}

#[test]
fn cpp_method_prototype_is_owned_only_by_the_method_and_not_its_aggregate() -> Result<()> {
    const SOURCE: &str = concat!(
        "struct Codec {\n",
        "    int version;\n",
        "    Result decode(const Input *);\n",
        "};\n",
    );

    let facts = project("src/codec.cpp", Language::Cpp, SOURCE)?;
    let codec = declaration_named(&facts, "Codec", DeclarationKind::Struct)?;
    let decode = declaration_named(&facts, "decode", DeclarationKind::Method)?;

    assert_eq!(
        codec.projection_text,
        "struct Codec {\n    int version;\n};"
    );
    assert_eq!(
        codec.source_span,
        exact_range(
            SOURCE,
            "struct Codec {\n    int version;\n    Result decode(const Input *);\n};"
        )?
    );
    assert_eq!(
        merged_component_ranges(codec),
        [
            exact_range(SOURCE, "struct Codec {\n    int version;")?,
            exact_range(SOURCE, "\n};")?,
        ]
    );
    assert!(codec.type_use_sites.is_empty());

    assert_eq!(decode.projection_text, "Result decode(const Input *);");
    assert_eq!(
        decode.source_span,
        exact_range(SOURCE, "Result decode(const Input *);")?
    );
    assert_single_component(
        decode,
        SOURCE,
        SourceComponentRole::Signature,
        "Result decode(const Input *);",
    )?;
    assert_eq!(
        decode
            .type_use_sites
            .iter()
            .map(|site| (site.name.as_str(), site.role, site.source_range.clone()))
            .collect::<Vec<_>>(),
        [
            (
                "Result",
                TypeUseRole::Return,
                exact_range(SOURCE, "Result")?,
            ),
            (
                "Input",
                TypeUseRole::Parameter,
                exact_range(SOURCE, "Input")?,
            ),
        ]
    );

    assert_eq!(codec.children, [decode.id.clone()]);
    assert_eq!(decode.parent_part.as_ref(), Some(&codec.id));
    assert_eq!(codec.review_owner, codec.id);
    assert_eq!(decode.review_owner, decode.id);
    for aggregate_component in &codec.components {
        for method_component in &decode.components {
            assert!(
                aggregate_component.source_range.end <= method_component.source_range.start
                    || method_component.source_range.end <= aggregate_component.source_range.start,
                "aggregate and method both own source bytes: {aggregate_component:?} {method_component:?}"
            );
        }
    }
    assert!(facts.diagnostics.is_empty(), "{:?}", facts.diagnostics);

    Ok(())
}

#[test]
fn c_multi_declarator_prototypes_are_omitted_with_an_explicit_ownership_diagnostic() -> Result<()> {
    const SOURCE: &str = "int f(int), g(double);\n";

    let facts = project("include/callbacks.c", Language::C, SOURCE)?;

    assert!(
        facts.declarations().is_empty(),
        "one source statement must not be duplicated under several function owners: {:?}",
        facts.declarations()
    );
    assert_eq!(
        facts
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>(),
        ["multi-declarator callable declaration cannot be projected without duplicate ownership"]
    );
    assert_eq!(exact_range(SOURCE, "int f(int), g(double);")?, 0..22);

    Ok(())
}

#[test]
fn cpp_overload_documentation_edits_keep_signature_keys_and_pair_as_changed() -> Result<()> {
    const BASE: &str = concat!(
        "/// Integer v1.\n",
        "int convert(int);\n",
        "/// Real v1.\n",
        "double convert(double);\n",
    );
    const HEAD: &str = concat!(
        "/// Integer v2.\n",
        "int convert(int);\n",
        "/// Real v2.\n",
        "double convert(double);\n",
    );
    const CASES: [(&str, &str, &str, Range<usize>); 2] = [
        (
            "int convert(int);",
            "/// Integer v1.\nint convert(int);",
            "/// Integer v2.\nint convert(int);",
            0..33,
        ),
        (
            "double convert(double);",
            "/// Real v1.\ndouble convert(double);",
            "/// Real v2.\ndouble convert(double);",
            34..70,
        ),
    ];

    let base_facts = project("src/conversion.cpp", Language::Cpp, BASE)?;
    let head_facts = project("src/conversion.cpp", Language::Cpp, HEAD)?;
    for (signature, base_projection, head_projection, expected_span) in &CASES {
        let base = base_facts
            .declarations()
            .iter()
            .find(|declaration| declaration.projection_text.ends_with(signature))
            .with_context(|| format!("missing base overload {signature}"))?;
        let head = head_facts
            .declarations()
            .iter()
            .find(|declaration| declaration.projection_text.ends_with(signature))
            .with_context(|| format!("missing head overload {signature}"))?;
        assert_eq!(base.name, "convert");
        assert_eq!(base.kind, DeclarationKind::Function);
        assert_eq!(base.key, head.key, "documentation is not overload identity");
        assert_eq!(base.projection_text, *base_projection);
        assert_eq!(head.projection_text, *head_projection);
        assert_eq!(base.source_span, *expected_span);
        assert_eq!(head.source_span, *expected_span);
    }
    assert_ne!(
        base_facts.declarations()[0].key,
        base_facts.declarations()[1].key,
        "the two signatures must remain distinct overload identities"
    );

    let diff = diff_declarations(&[cpp_same_path_pair("overload-docs", BASE, HEAD)])?;
    assert_eq!(diff.matches.len(), 2, "{:#?}", diff);
    assert_eq!(diff.units.len(), 2, "{:#?}", diff);
    assert!(diff.diagnostics.is_empty(), "{:?}", diff.diagnostics);

    for (signature, base_projection, head_projection, expected_span) in CASES {
        let unit = changed_unit_named(&diff.units, signature)?;
        assert_eq!(unit.change_kind, DeclarationChangeKind::Changed);
        assert_eq!(unit.matching_evidence, Some(MatchingEvidence::ExactKey));
        let base = unit
            .base
            .as_ref()
            .context("changed overload missing base")?;
        let head = unit
            .head
            .as_ref()
            .context("changed overload missing head")?;
        assert_eq!(base.projection_text, base_projection);
        assert_eq!(head.projection_text, head_projection);
        assert_eq!(base.source_span, expected_span);
        assert_eq!(head.source_span, expected_span);
        assert_eq!(
            merged_component_ranges(base),
            [exact_range(BASE, base_projection)?]
        );
        assert_eq!(
            merged_component_ranges(head),
            [exact_range(HEAD, head_projection)?]
        );
    }

    Ok(())
}

#[test]
fn cpp_namespace_is_a_module_with_exact_child_lineage() -> Result<()> {
    const SOURCE: &str = "namespace api {\nint parse();\n}\n";

    let facts = project("src/api.cpp", Language::Cpp, SOURCE)?;
    assert_eq!(
        facts
            .declarations()
            .iter()
            .map(|declaration| (declaration.name.as_str(), declaration.kind))
            .collect::<Vec<_>>(),
        [
            ("api", DeclarationKind::Module),
            ("parse", DeclarationKind::Function)
        ]
    );

    let module = declaration_named(&facts, "api", DeclarationKind::Module)?;
    let parse = declaration_named(&facts, "parse", DeclarationKind::Function)?;
    assert_eq!(module.projection_text, "namespace api");
    assert_eq!(
        module.source_span,
        exact_range(SOURCE, "namespace api {\nint parse();\n}")?
    );
    assert_single_component(
        module,
        SOURCE,
        SourceComponentRole::Signature,
        "namespace api",
    )?;
    assert_eq!(module.children, [parse.id.clone()]);
    assert_eq!(parse.parent_part.as_ref(), Some(&module.id));

    assert_eq!(parse.projection_text, "int parse();");
    assert_eq!(parse.source_span, exact_range(SOURCE, "int parse();")?);
    assert_single_component(
        parse,
        SOURCE,
        SourceComponentRole::Signature,
        "int parse();",
    )?;
    assert_eq!(module.review_owner, module.id);
    assert_eq!(parse.review_owner, parse.id);
    assert!(facts.diagnostics.is_empty(), "{:?}", facts.diagnostics);

    Ok(())
}

struct InventoryCase {
    path: &'static str,
    language: Language,
    source: &'static str,
    expected: [(
        &'static str,
        DeclarationKind,
        SourceComponentRole,
        &'static str,
        TypeUseRole,
    ); 3],
}

#[test]
fn c_and_cpp_aliases_statics_and_constants_are_exact_inventory_targets() -> Result<()> {
    let cases = [
        InventoryCase {
            path: "src/identifiers.c",
            language: Language::C,
            source: concat!(
                "typedef External UserId;\n",
                "static const External Limit = {0};\n",
                "const External Capacity = {0};\n",
            ),
            expected: [
                (
                    "UserId",
                    DeclarationKind::TypeAlias,
                    SourceComponentRole::TypeAlias,
                    "typedef External UserId;",
                    TypeUseRole::AliasTarget,
                ),
                (
                    "Limit",
                    DeclarationKind::Static,
                    SourceComponentRole::Value,
                    "static const External Limit = {0};",
                    TypeUseRole::Other,
                ),
                (
                    "Capacity",
                    DeclarationKind::Constant,
                    SourceComponentRole::Value,
                    "const External Capacity = {0};",
                    TypeUseRole::Other,
                ),
            ],
        },
        InventoryCase {
            path: "src/identifiers.cpp",
            language: Language::Cpp,
            source: concat!(
                "using UserId = External;\n",
                "static const External Limit{};\n",
                "constexpr External Capacity{};\n",
            ),
            expected: [
                (
                    "UserId",
                    DeclarationKind::TypeAlias,
                    SourceComponentRole::TypeAlias,
                    "using UserId = External;",
                    TypeUseRole::AliasTarget,
                ),
                (
                    "Limit",
                    DeclarationKind::Static,
                    SourceComponentRole::Value,
                    "static const External Limit{};",
                    TypeUseRole::Other,
                ),
                (
                    "Capacity",
                    DeclarationKind::Constant,
                    SourceComponentRole::Value,
                    "constexpr External Capacity{};",
                    TypeUseRole::Other,
                ),
            ],
        },
    ];

    for case in cases {
        let facts = project(case.path, case.language, case.source)?;
        assert_eq!(
            facts
                .declarations()
                .iter()
                .map(|declaration| (declaration.name.as_str(), declaration.kind))
                .collect::<Vec<_>>(),
            case.expected
                .iter()
                .map(|(name, kind, _, _, _)| (*name, *kind))
                .collect::<Vec<_>>(),
            "{} inventory",
            case.path
        );
        assert!(
            facts.diagnostics.is_empty(),
            "{}: {:?}",
            case.path,
            facts.diagnostics
        );

        for (ordinal, (name, kind, role, projection, type_role)) in
            case.expected.into_iter().enumerate()
        {
            let declaration = declaration_named(&facts, name, kind)?;
            assert_eq!(declaration.source_ordinal, ordinal);
            assert_eq!(
                declaration.source_span,
                exact_range(case.source, projection)?
            );
            assert_eq!(declaration.projection_text, projection);
            assert_single_component(declaration, case.source, role, projection)?;
            assert_eq!(declaration.review_owner, declaration.id);
            assert!(declaration.parent_part.is_none());
            assert!(declaration.children.is_empty());
            let projection_range = exact_range(case.source, projection)?;
            let type_offset = projection
                .find("External")
                .context("inventory fixture projection is missing its named type")?;
            let expected_type_range = projection_range.start + type_offset
                ..projection_range.start + type_offset + "External".len();
            assert_eq!(
                declaration
                    .type_use_sites
                    .iter()
                    .map(|site| (site.name.as_str(), site.role, site.source_range.clone()))
                    .collect::<Vec<_>>(),
                [("External", type_role, expected_type_range)],
                "{} {name} type use",
                case.path
            );
        }
    }

    Ok(())
}
