use std::ops::Range;
use std::path::Path;

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::analysis::Language;
use crate::repo_path::RepoPath;

pub mod capture;
pub mod coverage;
pub mod diff;
pub mod relationships;
pub mod review;
pub mod snapshot;

mod c_family;
mod go;
mod just;
mod nix;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationFileCapability {
    pub path: RepoPath,
    pub language: Language,
    pub inventory: Capability,
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
        capabilities: DeclarationCapabilities,
        declarations: Vec<DeclarationNode>,
        diagnostics: Vec<ProjectionDiagnostic>,
    ) -> Self {
        Self {
            language,
            capabilities,
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
    let capabilities = capabilities_for(language);
    let (declarations, mut diagnostics, capabilities) = match language {
        Language::Rust => {
            let (declarations, diagnostics, generated_declaration_gaps) =
                rust::project(path, source)?;
            (
                declarations,
                diagnostics,
                rust_capabilities(generated_declaration_gaps),
            )
        }
        Language::TypeScript => {
            let (declarations, diagnostics) = typescript::project(path, source)?;
            (declarations, diagnostics, capabilities)
        }
        Language::Python => {
            let (declarations, diagnostics) = python::project(path, source)?;
            (declarations, diagnostics, capabilities)
        }
        Language::Go => {
            let (declarations, diagnostics) = go::project(path, source)?;
            (declarations, diagnostics, capabilities)
        }
        Language::C | Language::Cpp => {
            let (declarations, diagnostics) = c_family::project(path, language, source)?;
            (declarations, diagnostics, capabilities)
        }
        Language::Nix => {
            let (declarations, diagnostics) = nix::project(path, source)?;
            (declarations, diagnostics, capabilities)
        }
        Language::Just => {
            let (declarations, diagnostics) = just::project(path, source)?;
            (declarations, diagnostics, capabilities)
        }
        Language::Shell => {
            let (declarations, diagnostics) = shell::project(path, source)?;
            (declarations, diagnostics, capabilities)
        }
        _ if matches!(capabilities.inventory, Capability::NotApplicable { .. }) => {
            (Vec::new(), Vec::new(), capabilities)
        }
        _ => (
            Vec::new(),
            vec![ProjectionDiagnostic::new(format!(
                "{language:?} has no declaration projector"
            ))],
            capabilities,
        ),
    };

    if let Capability::Partial {
        diagnostics: capability_diagnostics,
        ..
    } = &capabilities.inventory
    {
        for diagnostic in capability_diagnostics {
            if !diagnostics
                .iter()
                .any(|existing| existing.message == diagnostic.message)
            {
                diagnostics.push(diagnostic.clone());
            }
        }
    }

    Ok(FileDeclarationFacts::from_parts(
        language,
        capabilities,
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
        Language::Nix => nix_capabilities(),
        Language::Just => just_capabilities(),
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
        | Language::Sql => unsupported_capabilities(&format!(
            "{language:?} declaration extraction is not implemented"
        )),
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

fn rust_capabilities(generated_declaration_gaps: bool) -> DeclarationCapabilities {
    if !generated_declaration_gaps {
        return complete_capabilities();
    }
    let generated = || {
        partial_capability(
            "macro- and derive-generated declarations",
            "Rust projection is exact-source-only; declarations generated by macro invocations and derive attributes are not expanded",
        )
    };
    DeclarationCapabilities {
        inventory: generated(),
        documentation_association: Capability::Complete,
        callable_projection: generated(),
        aggregate_projection: generated(),
        type_use_sites: Capability::Complete,
    }
}

fn nix_capabilities() -> DeclarationCapabilities {
    DeclarationCapabilities {
        inventory: partial_capability(
            "nested and dynamic attribute declarations",
            "Nix projection inventories root let bindings and root output attributes; nested attributes, dynamic names, and multi-name inherit statements remain aggregate-owned or explicitly diagnosed",
        ),
        documentation_association: partial_capability(
            "non-contiguous comment association",
            "Nix projection associates only contiguous leading comments with a binding",
        ),
        callable_projection: partial_capability(
            "nested function bindings",
            "Nix projection excludes function bodies only for root bindings whose value is a direct function expression",
        ),
        aggregate_projection: partial_capability(
            "nested attribute member inventory",
            "Nested Nix attribute sets are projected as the exact value surface owned by their root binding",
        ),
        type_use_sites: Capability::NotApplicable {
            reason: "Nix has no static type-use declaration syntax".to_owned(),
        },
    }
}

fn just_capabilities() -> DeclarationCapabilities {
    DeclarationCapabilities {
        inventory: partial_capability(
            "variable, setting, import, and module declarations",
            "Just projection inventories recipes and aliases; variables, settings, imports, and modules are not review targets",
        ),
        documentation_association: partial_capability(
            "detached recipe comments",
            "Just projection associates only contiguous leading comments with recipes and aliases",
        ),
        callable_projection: Capability::Complete,
        aggregate_projection: Capability::NotApplicable {
            reason: "Just recipes do not define aggregate type shapes".to_owned(),
        },
        type_use_sites: Capability::NotApplicable {
            reason: "Just recipe parameters have no static type-use syntax".to_owned(),
        },
    }
}

fn partial_capability(missing_feature: &str, diagnostic: &str) -> Capability {
    Capability::Partial {
        missing_features: vec![missing_feature.to_owned()],
        diagnostics: vec![ProjectionDiagnostic::new(diagnostic)],
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
