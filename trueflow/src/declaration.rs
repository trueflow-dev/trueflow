use std::ops::Range;
use std::path::Path;

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::analysis::Language;

pub mod capture;
pub mod coverage;
pub mod diff;
pub mod relationships;
pub mod review;
pub mod snapshot;

mod c_family;
mod go;
mod python;

mod projection;
mod rust;
mod shell;
mod typescript;

pub use projection::projection_hash;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    Complete,
    Partial {
        missing_features: Vec<String>,
        diagnostics: Vec<ProjectionDiagnostic>,
    },
    NotApplicable {
        reason: String,
    },
    Unsupported {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclarationCapabilities {
    pub inventory: Capability,
    pub documentation_association: Capability,
    pub callable_projection: Capability,
    pub aggregate_projection: Capability,
    pub type_use_sites: Capability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionDiagnostic {
    pub message: String,
}

impl ProjectionDiagnostic {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeclarationKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Interface,
    Class,
    TypeAlias,
    AssociatedType,
    Constant,
    Static,
    Module,
    Constructor,
    Destructor,
    Operator,
    Property,
    Macro,
}

impl DeclarationKind {
    pub(crate) const fn protocol_tag(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Interface => "interface",
            Self::Class => "class",
            Self::TypeAlias => "type_alias",
            Self::AssociatedType => "associated_type",
            Self::Constant => "constant",
            Self::Static => "static",
            Self::Module => "module",
            Self::Constructor => "constructor",
            Self::Destructor => "destructor",
            Self::Operator => "operator",
            Self::Property => "property",
            Self::Macro => "macro",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Visibility {
    Public,
    Crate,
    Restricted(String),
    Protected,
    Package,
    Internal,
    Private,
    Implicit,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeclarationId(String);

impl DeclarationId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct DeclarationKey(String);

impl DeclarationKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct DeclarationProjectionHash(String);

impl DeclarationProjectionHash {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SourceComponentRole {
    Documentation,
    Attribute,
    Signature,
    AggregateShape,
    TypeAlias,
    Value,
    Terminator,
    Layout,
}

impl SourceComponentRole {
    pub(crate) const fn protocol_tag(self) -> &'static str {
        match self {
            Self::Documentation => "documentation",
            Self::Attribute => "attribute",
            Self::Signature => "signature",
            Self::AggregateShape => "aggregate_shape",
            Self::TypeAlias => "type_alias",
            Self::Value => "value",
            Self::Terminator => "terminator",
            Self::Layout => "layout",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceComponent {
    pub role: SourceComponentRole,
    pub source_range: Range<usize>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TypeUseRole {
    Parameter,
    Return,
    Field,
    Variant,
    Bound,
    AliasTarget,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeUseSite {
    pub name: String,
    pub role: TypeUseRole,
    pub source_range: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclarationNode {
    pub id: DeclarationId,
    pub key: DeclarationKey,
    pub name: String,
    pub kind: DeclarationKind,
    pub visibility: Visibility,
    pub parent_part: Option<DeclarationId>,
    pub source_ordinal: usize,
    pub source_span: Range<usize>,
    pub components: Vec<SourceComponent>,
    pub projection_text: String,
    pub projection_hash: DeclarationProjectionHash,
    pub review_owner: DeclarationId,
    pub children: Vec<DeclarationId>,
    pub type_use_sites: Vec<TypeUseSite>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDeclarationFacts {
    pub language: Language,
    pub capabilities: DeclarationCapabilities,
    pub diagnostics: Vec<ProjectionDiagnostic>,
    declarations: Vec<DeclarationNode>,
}

impl FileDeclarationFacts {
    pub(crate) fn from_parts(
        language: Language,
        declarations: Vec<DeclarationNode>,
        diagnostics: Vec<ProjectionDiagnostic>,
    ) -> Self {
        Self {
            language,
            capabilities: capabilities_for(language),
            diagnostics,
            declarations,
        }
    }

    pub fn declarations(&self) -> &[DeclarationNode] {
        &self.declarations
    }

    pub fn into_declarations(self) -> Vec<DeclarationNode> {
        self.declarations
    }
}

pub fn project_source(
    path: &Path,
    language: Language,
    source: &str,
) -> Result<FileDeclarationFacts> {
    let (declarations, diagnostics) = match language {
        Language::Rust => rust::project(path, source)?,
        Language::TypeScript => typescript::project(path, source)?,
        Language::Python => python::project(path, source)?,
        Language::Go => go::project(path, source)?,
        Language::C | Language::Cpp => c_family::project(path, language, source)?,
        Language::Shell => shell::project(path, source)?,
        _ => (
            Vec::new(),
            vec![ProjectionDiagnostic::new(format!(
                "{language:?} has no declaration projector"
            ))],
        ),
    };

    Ok(FileDeclarationFacts::from_parts(
        language,
        declarations,
        diagnostics,
    ))
}

pub fn capabilities_for(language: Language) -> DeclarationCapabilities {
    match language {
        Language::Rust => complete_capabilities(),
        Language::TypeScript | Language::Python | Language::Go | Language::C | Language::Cpp => {
            complete_capabilities()
        }
        Language::Json
        | Language::Yaml
        | Language::Html
        | Language::Css
        | Language::Markdown
        | Language::Toml
        | Language::Text => not_applicable_capabilities(language),
        Language::Unknown => unsupported_capabilities("the source language is unknown"),
        Language::Swift
        | Language::Elisp
        | Language::JavaScript
        | Language::Java
        | Language::Kotlin
        | Language::CSharp
        | Language::Ruby
        | Language::Php
        | Language::Zig
        | Language::Lua
        | Language::Dart
        | Language::Scala
        | Language::Haskell
        | Language::OCaml
        | Language::Elixir
        | Language::Clojure
        | Language::Sql
        | Language::Nix
        | Language::Just => partial_capabilities(language),
        Language::Shell => shell_capabilities(),
    }
}

fn complete_capabilities() -> DeclarationCapabilities {
    DeclarationCapabilities {
        inventory: Capability::Complete,
        documentation_association: Capability::Complete,
        callable_projection: Capability::Complete,
        aggregate_projection: Capability::Complete,
        type_use_sites: Capability::Complete,
    }
}

fn partial_capabilities(language: Language) -> DeclarationCapabilities {
    let missing = vec![format!("{language:?} declaration adapter")];
    let diagnostics = vec![ProjectionDiagnostic::new(format!(
        "{language:?} declaration extraction is not implemented"
    ))];
    let facet = || Capability::Partial {
        missing_features: missing.clone(),
        diagnostics: diagnostics.clone(),
    };
    DeclarationCapabilities {
        inventory: facet(),
        documentation_association: facet(),
        callable_projection: facet(),
        aggregate_projection: facet(),
        type_use_sites: facet(),
    }
}

fn shell_capabilities() -> DeclarationCapabilities {
    let not_applicable = || Capability::NotApplicable {
        reason: "shell has no aggregate or static type declaration surface".to_owned(),
    };
    DeclarationCapabilities {
        inventory: Capability::Complete,
        documentation_association: Capability::Complete,
        callable_projection: Capability::Complete,
        aggregate_projection: not_applicable(),
        type_use_sites: not_applicable(),
    }
}

fn not_applicable_capabilities(language: Language) -> DeclarationCapabilities {
    let facet = || Capability::NotApplicable {
        reason: format!("{language:?} does not define code declarations for this projector"),
    };
    DeclarationCapabilities {
        inventory: facet(),
        documentation_association: facet(),
        callable_projection: facet(),
        aggregate_projection: facet(),
        type_use_sites: facet(),
    }
}

fn unsupported_capabilities(reason: &str) -> DeclarationCapabilities {
    let facet = || Capability::Unsupported {
        reason: reason.to_owned(),
    };
    DeclarationCapabilities {
        inventory: facet(),
        documentation_association: facet(),
        callable_projection: facet(),
        aggregate_projection: facet(),
        type_use_sites: facet(),
    }
}
