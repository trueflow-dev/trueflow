use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};
use trueflow::analysis::Language;
use trueflow::declaration::{
    Capability, DeclarationKind, DeclarationNode, FileDeclarationFacts, SourceComponent,
    SourceComponentRole, TypeUseRole, project_source,
};

fn project(source: &str) -> Result<FileDeclarationFacts> {
    project_source(Path::new("src/contracts.ts"), Language::TypeScript, source)
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
        .with_context(|| format!("missing projected TypeScript {kind:?} declaration {name}"))
}

fn exact_range(source: &str, text: &str) -> Result<Range<usize>> {
    let start = source
        .find(text)
        .with_context(|| format!("fixture does not contain {text:?}"))?;
    Ok(start..start + text.len())
}

fn overlaps(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn is_fully_owned(range: &Range<usize>, components: &[SourceComponent]) -> bool {
    let mut owned_ranges = components
        .iter()
        .map(|component| component.source_range.clone())
        .collect::<Vec<_>>();
    owned_ranges.sort_by_key(|owned| (owned.start, owned.end));

    let mut next_unowned_byte = range.start;
    for owned in owned_ranges {
        if owned.end <= next_unowned_byte {
            continue;
        }
        if owned.start > next_unowned_byte {
            return false;
        }
        next_unowned_byte = owned.end.min(range.end);
        if next_unowned_byte == range.end {
            return true;
        }
    }
    false
}

fn assert_complete_inventory(facts: &FileDeclarationFacts) {
    assert!(
        matches!(facts.capabilities.inventory, Capability::Complete),
        "TypeScript inventory must explicitly report Complete, got {:?}",
        facts.capabilities.inventory
    );
}

fn assert_non_overlapping_source_ownership(source: &str, facts: &FileDeclarationFacts) {
    for (left_index, left) in facts.declarations().iter().enumerate() {
        for right in &facts.declarations()[left_index + 1..] {
            for left_component in &left.components {
                for right_component in &right.components {
                    assert!(
                        !overlaps(&left_component.source_range, &right_component.source_range),
                        "{0:?} {1} and {2:?} {3} both own source bytes {4:?} ({5:?}) and {6:?} ({7:?})",
                        left.kind,
                        left.name,
                        right.kind,
                        right.name,
                        left_component.source_range,
                        source.get(left_component.source_range.clone()),
                        right_component.source_range,
                        source.get(right_component.source_range.clone()),
                    );
                }
            }
        }
    }
}

#[test]
fn interface_method_jsdoc_and_signature_have_one_child_owner() -> Result<()> {
    const SOURCE: &str = concat!(
        "export interface Codec {\n",
        "  readonly version: number;\n",
        "  /** Decodes wire text. */\n",
        "  decode(input: string): number;\n",
        "}\n",
    );
    const METHOD_SURFACE: &str = "/** Decodes wire text. */\n  decode(input: string): number;";

    let facts = project(SOURCE)?;
    assert_complete_inventory(&facts);
    let codec = declaration_named(&facts, "Codec", DeclarationKind::Interface)?;
    let decode = declaration_named(&facts, "decode", DeclarationKind::Method)?;

    assert_eq!(
        facts.declarations().len(),
        2,
        "the complete inventory must contain only Codec and its decode child"
    );
    assert_eq!(decode.parent_part.as_ref(), Some(&codec.id));
    assert_eq!(codec.children.as_slice(), [decode.id.clone()]);
    assert_eq!(decode.review_owner, decode.id);
    assert_eq!(codec.review_owner, codec.id);
    assert_eq!(decode.projection_text, METHOD_SURFACE);

    let method_range = exact_range(SOURCE, METHOD_SURFACE)?;
    assert_eq!(decode.source_span, method_range);
    assert!(
        is_fully_owned(&method_range, &decode.components),
        "decode must own every byte of its JSDoc and signature"
    );
    assert_non_overlapping_source_ownership(SOURCE, &facts);
    assert_eq!(
        codec.projection_text, "export interface Codec {\n  readonly version: number;\n}",
        "Codec must retain its property shape but exclude the independently owned method JSDoc and signature"
    );
    assert!(
        !codec.projection_text.contains("Decodes wire text")
            && !codec.projection_text.contains("decode("),
        "the interface aggregate must not duplicate its Method child's review surface"
    );

    Ok(())
}

#[test]
fn namespace_and_module_declarations_inventory_their_nested_functions() -> Result<()> {
    const SOURCE: &str = concat!(
        "export namespace API {\n",
        "  export function parse(input: string): number {\n",
        "    return Number(input);\n",
        "  }\n",
        "}\n",
        "\n",
        "export module Legacy {\n",
        "  export function serialize(value: number): string {\n",
        "    return String(value);\n",
        "  }\n",
        "}\n",
    );

    let facts = project(SOURCE)?;
    assert_complete_inventory(&facts);
    let inventory = facts
        .declarations()
        .iter()
        .map(|declaration| (declaration.name.as_str(), declaration.kind))
        .collect::<Vec<_>>();
    assert_eq!(
        inventory,
        [
            ("API", DeclarationKind::Module),
            ("parse", DeclarationKind::Function),
            ("Legacy", DeclarationKind::Module),
            ("serialize", DeclarationKind::Function),
        ],
        "Complete TypeScript inventory must include namespace/module containers and their nested functions"
    );

    let api = declaration_named(&facts, "API", DeclarationKind::Module)?;
    let parse = declaration_named(&facts, "parse", DeclarationKind::Function)?;
    let legacy = declaration_named(&facts, "Legacy", DeclarationKind::Module)?;
    let serialize = declaration_named(&facts, "serialize", DeclarationKind::Function)?;

    assert_eq!(api.projection_text, "export namespace API {\n}");
    assert_eq!(legacy.projection_text, "export module Legacy {\n}");
    assert_eq!(
        parse.projection_text,
        "export function parse(input: string): number"
    );
    assert_eq!(
        serialize.projection_text,
        "export function serialize(value: number): string"
    );
    assert_eq!(parse.parent_part.as_ref(), Some(&api.id));
    assert_eq!(serialize.parent_part.as_ref(), Some(&legacy.id));
    assert_eq!(api.children.as_slice(), [parse.id.clone()]);
    assert_eq!(legacy.children.as_slice(), [serialize.id.clone()]);
    assert_eq!(api.review_owner, api.id);
    assert_eq!(parse.review_owner, parse.id);
    assert_eq!(legacy.review_owner, legacy.id);
    assert_eq!(serialize.review_owner, serialize.id);
    assert_non_overlapping_source_ownership(SOURCE, &facts);
    assert!(
        !api.projection_text.contains("parse") && !legacy.projection_text.contains("serialize"),
        "module aggregates must not duplicate their nested Function review surfaces"
    );
    assert!(
        !parse.projection_text.contains("return") && !serialize.projection_text.contains("return"),
        "nested Function projections must exclude executable bodies"
    );

    Ok(())
}

#[test]
fn top_level_const_preserves_its_exact_typed_value_surface() -> Result<()> {
    const SOURCE: &str = "export const LIMIT: Limit = 4;\nexport type Limit = number;\n";
    const CONST_SURFACE: &str = "export const LIMIT: Limit = 4;";

    let facts = project(SOURCE)?;
    assert_complete_inventory(&facts);
    let inventory = facts
        .declarations()
        .iter()
        .map(|declaration| (declaration.name.as_str(), declaration.kind))
        .collect::<Vec<_>>();
    assert_eq!(
        inventory,
        [
            ("LIMIT", DeclarationKind::Constant),
            ("Limit", DeclarationKind::TypeAlias),
        ],
        "Complete TypeScript inventory must not silently omit a top-level const declaration"
    );

    let limit = declaration_named(&facts, "LIMIT", DeclarationKind::Constant)?;
    let const_range = exact_range(SOURCE, CONST_SURFACE)?;
    assert_eq!(limit.review_owner, limit.id);
    assert_eq!(limit.parent_part, None);
    assert!(limit.children.is_empty());
    assert_eq!(limit.source_span, const_range);
    assert_eq!(limit.projection_text, CONST_SURFACE);
    assert_eq!(
        limit
            .components
            .iter()
            .map(|component| (
                component.role,
                component.source_range.clone(),
                component.text.as_str(),
            ))
            .collect::<Vec<_>>(),
        [(SourceComponentRole::Value, const_range, CONST_SURFACE)],
        "the Constant must expose its full declaration as one exact typed-value surface"
    );

    let type_range = exact_range(SOURCE, "Limit")?;
    assert_eq!(
        limit
            .type_use_sites
            .iter()
            .map(|site| (site.name.as_str(), site.role, site.source_range.clone()))
            .collect::<Vec<_>>(),
        [("Limit", TypeUseRole::Other, type_range)],
        "the named type in the Constant surface must remain a declaration type-use site"
    );
    assert_non_overlapping_source_ownership(SOURCE, &facts);

    Ok(())
}
