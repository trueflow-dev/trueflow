use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};
use trueflow::analysis::Language;
use trueflow::declaration::{
    DeclarationKind, SourceComponent, SourceComponentRole, project_source,
};

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

#[test]
fn named_interface_methods_are_independent_children_while_embedded_types_stay_aggregate_owned()
-> Result<()> {
    const SOURCE: &str = concat!(
        "package api\n",
        "\n",
        "type Reader interface {\n",
        "\tRead(buf []byte) (int, error)\n",
        "\tio.Closer\n",
        "}\n",
    );
    const METHOD_SURFACE: &str = "Read(buf []byte) (int, error)";
    const EMBEDDED_TYPE: &str = "io.Closer";

    let facts = project_source(Path::new("api/reader.go"), Language::Go, SOURCE)?;
    let reader = facts
        .declarations()
        .iter()
        .find(|declaration| {
            declaration.kind == DeclarationKind::Interface && declaration.name == "Reader"
        })
        .context("missing projected Go interface Reader")?;
    let methods = facts
        .declarations()
        .iter()
        .filter(|declaration| declaration.kind == DeclarationKind::Method)
        .collect::<Vec<_>>();

    assert_eq!(
        methods.len(),
        1,
        "the named interface operation must be projected as one Method child; declarations={:#?}",
        facts.declarations()
    );
    let method = methods[0];
    assert_eq!(method.name, "Read");
    assert_eq!(method.projection_text, METHOD_SURFACE);
    assert_eq!(method.parent_part.as_ref(), Some(&reader.id));
    assert_eq!(method.review_owner, method.id);
    assert_eq!(reader.children.as_slice(), std::slice::from_ref(&method.id));
    assert_eq!(reader.review_owner, reader.id);
    assert_eq!(
        facts.declarations().len(),
        2,
        "the embedded type element must remain part of Reader, not become a declaration"
    );

    let method_range = exact_range(SOURCE, METHOD_SURFACE)?;
    let embedded_range = exact_range(SOURCE, EMBEDDED_TYPE)?;
    assert!(
        is_fully_owned(&method_range, &method.components),
        "the Method child must own its complete callable surface"
    );
    assert!(
        reader
            .components
            .iter()
            .all(|component| !overlaps(&component.source_range, &method_range)),
        "Reader's aggregate components must not also own the named method range"
    );
    assert!(
        !reader.projection_text.contains(METHOD_SURFACE),
        "Reader's aggregate projection must exclude the independently reviewable method"
    );
    assert!(
        is_fully_owned(&embedded_range, &reader.components),
        "Reader must retain ownership of the complete embedded type element"
    );
    assert!(reader.projection_text.contains(EMBEDDED_TYPE));

    Ok(())
}

#[test]
fn multi_name_const_spec_has_one_exact_syntactic_review_owner() -> Result<()> {
    const SOURCE: &str = "package values\n\nconst A, B = 1, 2\n";
    const CONST_SPEC: &str = "const A, B = 1, 2";

    let facts = project_source(Path::new("values/constants.go"), Language::Go, SOURCE)?;
    let constants = facts
        .declarations()
        .iter()
        .filter(|declaration| declaration.kind == DeclarationKind::Constant)
        .collect::<Vec<_>>();

    assert_eq!(
        constants.len(),
        1,
        "one Go const_spec must produce one syntactic review owner, not one duplicate owner per name; constants={constants:#?}"
    );
    let owner = constants[0];
    let spec_range = exact_range(SOURCE, CONST_SPEC)?;
    assert_eq!(owner.review_owner, owner.id);
    assert_eq!(owner.source_span, spec_range);
    assert_eq!(owner.projection_text, CONST_SPEC);
    assert_eq!(
        owner
            .components
            .iter()
            .map(|component| (
                component.role,
                component.source_range.clone(),
                component.text.as_str(),
            ))
            .collect::<Vec<_>>(),
        [(SourceComponentRole::Value, spec_range, CONST_SPEC)],
        "the syntactic owner must expose the const spec's exact range and projection once"
    );

    Ok(())
}
