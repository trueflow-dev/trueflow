use std::path::Path;

use anyhow::{Context, Result};
use trueflow::analysis::Language;
use trueflow::declaration::{
    Capability, DeclarationKind, FileDeclarationFacts, SourceComponentRole, Visibility,
    project_source,
};

fn project(path: &str, language: Language, source: &str) -> Result<FileDeclarationFacts> {
    project_source(Path::new(path), language, source)
}

fn assert_partial_mentions(capability: &Capability, expected: &str) {
    let Capability::Partial {
        missing_features,
        diagnostics,
    } = capability
    else {
        panic!("expected Partial capability mentioning {expected:?}, got {capability:?}");
    };
    assert!(
        missing_features
            .iter()
            .chain(diagnostics.iter().map(|diagnostic| &diagnostic.message))
            .any(|message| message.to_ascii_lowercase().contains(expected)),
        "Partial capability did not mention {expected:?}: {capability:?}"
    );
}

#[test]
fn rust_exact_source_inventory_is_broad_and_truthful_about_generated_declarations() -> Result<()> {
    const SOURCE: &str = r#"#![allow(dead_code)]
use std::fmt::Debug;

pub mod inline {
    pub type Identifier = u64;
    pub const LIMIT: usize = 8;
    pub static LABEL: &str = "inline";

    pub fn convert<T: Debug>(value: T) -> Result<T, Error>
    where
        T: Clone,
    {
        Ok(value)
    }
}

pub mod external;

#[derive(Debug, Clone)]
pub struct Config<T>
where
    T: Clone,
{
    pub value: T,
}

pub union Number {
    pub integer: i32,
    pub float: f32,
}

pub enum Message<T> {
    Data(T),
    Empty,
}

pub trait Repository<T> {
    type Error;
    const NAME: &'static str;
    fn load(&self, key: T) -> Result<T, Self::Error>;
}

impl<T: Clone> Config<T> {
    pub fn new(value: T) -> Self {
        Self { value }
    }
}

extern "C" {
    pub fn foreign(input: i32) -> i32;
}

macro_rules! declare_handlers {
    ($name:ident) => { fn $name() {} };
}

declare_handlers!(generated_handler);
"#;

    let facts = project("src/lib.rs", Language::Rust, SOURCE)?;
    let declarations = facts.declarations();
    let expected = [
        ("inline", DeclarationKind::Module),
        ("Identifier", DeclarationKind::TypeAlias),
        ("LIMIT", DeclarationKind::Constant),
        ("LABEL", DeclarationKind::Static),
        ("convert", DeclarationKind::Function),
        ("external", DeclarationKind::Module),
        ("Config", DeclarationKind::Struct),
        ("Number", DeclarationKind::Struct),
        ("Message", DeclarationKind::Enum),
        ("Repository", DeclarationKind::Trait),
        ("Error", DeclarationKind::AssociatedType),
        ("NAME", DeclarationKind::Constant),
        ("load", DeclarationKind::Method),
        ("new", DeclarationKind::Method),
        ("foreign", DeclarationKind::Function),
        ("declare_handlers", DeclarationKind::Macro),
    ];
    for (name, kind) in expected {
        assert!(
            declarations
                .iter()
                .any(|declaration| declaration.name == name && declaration.kind == kind),
            "missing common Rust declaration {kind:?} {name}; projected: {declarations:#?}"
        );
    }

    let convert = declarations
        .iter()
        .find(|declaration| declaration.name == "convert")
        .context("missing convert declaration")?;
    assert!(convert.projection_text.contains("where\n        T: Clone"));
    assert!(!convert.projection_text.contains("Ok(value)"));

    assert_partial_mentions(&facts.capabilities.inventory, "macro");
    assert!(
        facts.diagnostics.iter().any(|diagnostic| {
            let message = diagnostic.message.to_ascii_lowercase();
            message.contains("derive") || message.contains("macro invocation")
        }),
        "macro-generated omissions require an explicit per-file diagnostic: {:?}",
        facts.diagnostics
    );
    Ok(())
}

#[test]
fn nix_projects_root_let_and_output_bindings_without_splitting_nested_attrsets() -> Result<()> {
    const SOURCE: &str = include_str!("../example_repos/nix_support/default.nix");

    let facts = project("flake.nix", Language::Nix, SOURCE)?;
    let declarations = facts.declarations();
    let names = declarations
        .iter()
        .map(|declaration| declaration.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["defaults", "mkWorker", "selected", "worker"]);

    let defaults = declarations
        .iter()
        .find(|declaration| declaration.name == "defaults")
        .context("missing defaults binding")?;
    assert_eq!(defaults.kind, DeclarationKind::Constant);
    assert_eq!(defaults.visibility, Visibility::Private);
    assert!(defaults.projection_text.contains("labels = {"));
    assert!(defaults.projection_text.contains("packages = ["));

    let worker_factory = declarations
        .iter()
        .find(|declaration| declaration.name == "mkWorker")
        .context("missing mkWorker binding")?;
    assert_eq!(worker_factory.kind, DeclarationKind::Function);
    assert_eq!(worker_factory.projection_text.trim(), "mkWorker = name:");
    assert!(!worker_factory.projection_text.contains("packageSet"));

    let worker = declarations
        .iter()
        .find(|declaration| declaration.name == "worker")
        .context("missing output worker binding")?;
    assert_eq!(worker.visibility, Visibility::Public);

    assert_partial_mentions(&facts.capabilities.inventory, "nested");
    assert!(matches!(
        facts.capabilities.type_use_sites,
        Capability::NotApplicable { .. }
    ));
    assert!(
        facts
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("inherit defaults selected")),
        "multi-name inherit omission must be explicit: {:?}",
        facts.diagnostics
    );
    Ok(())
}

#[test]
fn just_projects_documented_recipe_signatures_and_aliases_without_bodies() -> Result<()> {
    const SOURCE: &str = r#"set shell := ["bash", "-eu", "-c"]

# Build one target.
[private]
build target="all": lint test
    cargo build --package {{target}}

# Short spelling.
alias b := build

project_name := "declarations"
"#;

    let facts = project("Justfile", Language::Just, SOURCE)?;
    let declarations = facts.declarations();
    assert_eq!(declarations.len(), 2, "projected: {declarations:#?}");

    let recipe = declarations
        .iter()
        .find(|declaration| declaration.name == "build")
        .context("missing build recipe")?;
    assert_eq!(recipe.kind, DeclarationKind::Function);
    assert_eq!(recipe.visibility, Visibility::Private);
    assert_eq!(
        recipe.projection_text,
        "# Build one target.\n[private]\nbuild target=\"all\": lint test"
    );
    assert!(!recipe.projection_text.contains("cargo build"));
    assert!(
        recipe
            .components
            .iter()
            .any(|component| component.role == SourceComponentRole::Documentation)
    );
    assert!(
        recipe
            .components
            .iter()
            .any(|component| component.role == SourceComponentRole::Attribute)
    );

    let alias = declarations
        .iter()
        .find(|declaration| declaration.name == "b")
        .context("missing b alias")?;
    assert_eq!(alias.kind, DeclarationKind::TypeAlias);
    assert_eq!(alias.projection_text, "# Short spelling.\nalias b := build");

    assert_partial_mentions(&facts.capabilities.inventory, "variable");
    assert!(matches!(
        facts.capabilities.aggregate_projection,
        Capability::NotApplicable { .. }
    ));
    assert!(
        facts.diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("project_name")
                && diagnostic.message.to_ascii_lowercase().contains("variable")
        }),
        "omitted Just variables require an explicit diagnostic: {:?}",
        facts.diagnostics
    );
    Ok(())
}
