use std::path::Path;

use anyhow::{Context, Result};
use trueflow::analysis::Language;
use trueflow::declaration::{DeclarationKind, project_source};

#[test]
fn abstract_property_surface_is_owned_only_by_the_class_aggregate() -> Result<()> {
    const SOURCE: &str = "from abc import ABC, abstractmethod\n\nclass Document(ABC):\n    @property\n    @abstractmethod\n    def title(self) -> str:\n        ...\n\n    def render(self, prefix: str) -> str:\n        return prefix + self.title\n";
    const ABSTRACT_PROPERTY_SURFACE: &str =
        "@property\n    @abstractmethod\n    def title(self) -> str:";

    let facts = project_source(Path::new("document.py"), Language::Python, SOURCE)?;
    let document = facts
        .declarations()
        .iter()
        .find(|declaration| {
            declaration.name == "Document" && declaration.kind == DeclarationKind::Class
        })
        .context("missing Document class projection")?;
    let render = facts
        .declarations()
        .iter()
        .find(|declaration| {
            declaration.name == "render" && declaration.kind == DeclarationKind::Method
        })
        .context("missing concrete render method projection")?;

    assert_eq!(
        document.projection_text,
        "class Document(ABC):\n    @property\n    @abstractmethod\n    def title(self) -> str:"
    );
    assert_eq!(
        render.projection_text, "def render(self, prefix: str) -> str:",
        "ordinary concrete methods must remain independent callable targets"
    );

    let property_start = SOURCE
        .find(ABSTRACT_PROPERTY_SURFACE)
        .context("abstract-property fixture surface")?;
    let property_end = property_start + ABSTRACT_PROPERTY_SURFACE.len();
    for offset in property_start..property_end {
        let owners = facts
            .declarations()
            .iter()
            .flat_map(|declaration| {
                declaration
                    .components
                    .iter()
                    .filter(move |component| component.source_range.contains(&offset))
                    .map(move |_| (declaration.kind, declaration.name.as_str()))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            owners,
            vec![(DeclarationKind::Class, "Document")],
            "abstract-property byte {offset} must be owned exactly once by the class aggregate"
        );
    }

    assert!(
        !facts.declarations().iter().any(|declaration| {
            declaration.name == "title" && declaration.kind == DeclarationKind::Method
        }),
        "the aggregate-owned abstract property must not also produce a Method review target"
    );

    Ok(())
}
