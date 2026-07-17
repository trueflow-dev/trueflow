use std::path::Path;

use anyhow::{Context, Result};
use trueflow::analysis::Language;
use trueflow::declaration::{
    project_source, Capability, DeclarationKind, DeclarationNode, FileDeclarationFacts, Visibility,
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

fn assert_complete(capability: &Capability, facet: &str, language: Language) {
    assert!(
        matches!(capability, Capability::Complete),
        "{language:?} {facet} capability should be complete, got {capability:?}"
    );
}

#[test]
fn rust_callable_hash_ignores_body_and_tracks_contract_surface() -> Result<()> {
    const BASE: &str = "// ordinary comment\n/// Adds one.\n#[inline]\npub fn add(left: i32, right: i32) -> i32 {\n    left + right\n}\n";
    const BODY_EDIT: &str = "// ordinary comment\n/// Adds one.\n#[inline]\npub fn add(left: i32, right: i32) -> i32 {\n    left.saturating_add(right)\n}\n";
    const SIGNATURE_EDIT: &str = "// ordinary comment\n/// Adds one.\n#[inline]\npub fn add(left: i32, right: i64) -> i32 {\n    left + right as i32\n}\n";
    const DOC_EDIT: &str = "// ordinary comment\n/// Adds two values.\n#[inline]\npub fn add(left: i32, right: i32) -> i32 {\n    left + right\n}\n";
    const VISIBILITY_EDIT: &str = "// ordinary comment\n/// Adds one.\n#[inline]\npub(crate) fn add(left: i32, right: i32) -> i32 {\n    left + right\n}\n";
    const ATTRIBUTE_EDIT: &str = "// ordinary comment\n/// Adds one.\n#[cold]\npub fn add(left: i32, right: i32) -> i32 {\n    left + right\n}\n";

    let base = project("src/math.rs", Language::Rust, BASE)?;
    let body_edit = project("src/math.rs", Language::Rust, BODY_EDIT)?;
    let signature_edit = project("src/math.rs", Language::Rust, SIGNATURE_EDIT)?;
    let doc_edit = project("src/math.rs", Language::Rust, DOC_EDIT)?;
    let visibility_edit = project("src/math.rs", Language::Rust, VISIBILITY_EDIT)?;
    let attribute_edit = project("src/math.rs", Language::Rust, ATTRIBUTE_EDIT)?;

    let base_add = declaration_named(&base, "add", DeclarationKind::Function)?;
    assert_eq!(base_add.visibility, Visibility::Public);
    assert_eq!(
        base_add.projection_text,
        "/// Adds one.\n#[inline]\npub fn add(left: i32, right: i32) -> i32"
    );
    assert!(!base_add.projection_text.contains("ordinary comment"));
    assert!(!base_add.projection_text.contains("left + right"));
    assert_eq!(base_add.review_owner, base_add.id);

    assert_eq!(
        base_add.projection_hash,
        declaration_named(&body_edit, "add", DeclarationKind::Function)?.projection_hash,
        "an executable-body-only edit must preserve approval identity"
    );

    for (label, changed) in [
        ("signature", &signature_edit),
        ("documentation", &doc_edit),
        ("visibility", &visibility_edit),
        ("attribute", &attribute_edit),
    ] {
        assert_ne!(
            base_add.projection_hash,
            declaration_named(changed, "add", DeclarationKind::Function)?.projection_hash,
            "a {label} edit must reopen the callable declaration"
        );
    }

    Ok(())
}

#[test]
fn rust_aggregate_hash_tracks_shape_not_method_bodies() -> Result<()> {
    const BASE: &str = "pub struct Account {\n    pub id: u64,\n    name: String,\n}\nimpl Account {\n    pub fn label(&self) -> &str {\n        &self.name\n    }\n}\npub enum Status {\n    Ready,\n    Failed(u16),\n}\n";
    const METHOD_BODY_EDIT: &str = "pub struct Account {\n    pub id: u64,\n    name: String,\n}\nimpl Account {\n    pub fn label(&self) -> &str {\n        self.name.as_str()\n    }\n}\npub enum Status {\n    Ready,\n    Failed(u16),\n}\n";
    const FIELD_SHAPE_EDIT: &str = "pub struct Account {\n    pub id: u128,\n    name: String,\n}\nimpl Account {\n    pub fn label(&self) -> &str {\n        &self.name\n    }\n}\npub enum Status {\n    Ready,\n    Failed(u16),\n}\n";
    const ENUM_SHAPE_EDIT: &str = "pub struct Account {\n    pub id: u64,\n    name: String,\n}\nimpl Account {\n    pub fn label(&self) -> &str {\n        &self.name\n    }\n}\npub enum Status {\n    Ready,\n    Failed { code: u16 },\n}\n";

    let base = project("src/model.rs", Language::Rust, BASE)?;
    let method_body_edit = project("src/model.rs", Language::Rust, METHOD_BODY_EDIT)?;
    let field_shape_edit = project("src/model.rs", Language::Rust, FIELD_SHAPE_EDIT)?;
    let enum_shape_edit = project("src/model.rs", Language::Rust, ENUM_SHAPE_EDIT)?;

    let account = declaration_named(&base, "Account", DeclarationKind::Struct)?;
    let status = declaration_named(&base, "Status", DeclarationKind::Enum)?;
    assert_eq!(
        account.projection_text,
        "pub struct Account {\n    pub id: u64,\n    name: String,\n}"
    );
    assert_eq!(
        status.projection_text,
        "pub enum Status {\n    Ready,\n    Failed(u16),\n}"
    );
    assert!(!account.projection_text.contains("label"));

    assert_eq!(
        account.projection_hash,
        declaration_named(&method_body_edit, "Account", DeclarationKind::Struct)?.projection_hash
    );
    assert_eq!(
        status.projection_hash,
        declaration_named(&method_body_edit, "Status", DeclarationKind::Enum)?.projection_hash
    );
    assert_ne!(
        account.projection_hash,
        declaration_named(&field_shape_edit, "Account", DeclarationKind::Struct)?.projection_hash
    );
    assert_eq!(
        status.projection_hash,
        declaration_named(&field_shape_edit, "Status", DeclarationKind::Enum)?.projection_hash
    );
    assert_ne!(
        status.projection_hash,
        declaration_named(&enum_shape_edit, "Status", DeclarationKind::Enum)?.projection_hash
    );
    assert_eq!(
        account.projection_hash,
        declaration_named(&enum_shape_edit, "Account", DeclarationKind::Struct)?.projection_hash
    );

    Ok(())
}

#[test]
fn rust_inventory_includes_all_visibilities_and_excludes_comments_and_locals() -> Result<()> {
    const SOURCE: &str = "// ordinary module comment\npub fn visible() {}\nfn hidden() {}\nfn outer() {\n    // ordinary body comment\n    fn local() {}\n    local();\n}\n";

    let facts = project("src/scope.rs", Language::Rust, SOURCE)?;
    let mut names = facts
        .declarations()
        .iter()
        .map(|declaration| declaration.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(names, ["hidden", "outer", "visible"]);

    let visible = declaration_named(&facts, "visible", DeclarationKind::Function)?;
    let hidden = declaration_named(&facts, "hidden", DeclarationKind::Function)?;
    assert_eq!(visible.visibility, Visibility::Public);
    assert_eq!(hidden.visibility, Visibility::Private);
    assert!(
        facts
            .declarations()
            .iter()
            .all(|declaration| !declaration.projection_text.contains("ordinary"))
    );
    assert!(facts.declarations().iter().all(|declaration| {
        declaration.review_owner == declaration.id && declaration.name != "local"
    }));

    Ok(())
}

struct ProjectionCase {
    path: &'static str,
    language: Language,
    source: &'static str,
    callable: &'static str,
    callable_projection: &'static str,
    aggregate: &'static str,
    aggregate_kind: DeclarationKind,
    aggregate_projection: &'static str,
}

#[test]
fn common_languages_extract_exact_body_free_callable_and_aggregate_shapes() -> Result<()> {
    let cases = [
        ProjectionCase {
            path: "model.ts",
            language: Language::TypeScript,
            source: "export function greet(name: string): string {\n  return `Hi ${name}`;\n}\nexport interface User {\n  readonly id: number;\n  name: string;\n}\n",
            callable: "greet",
            callable_projection: "export function greet(name: string): string",
            aggregate: "User",
            aggregate_kind: DeclarationKind::Interface,
            aggregate_projection: "export interface User {\n  readonly id: number;\n  name: string;\n}",
        },
        ProjectionCase {
            path: "model.py",
            language: Language::Python,
            source: "def greet(name: str) -> str:\n    return f\"Hi {name}\"\n\nclass User:\n    id: int\n    def label(self) -> str:\n        return str(self.id)\n",
            callable: "greet",
            callable_projection: "def greet(name: str) -> str:",
            aggregate: "User",
            aggregate_kind: DeclarationKind::Class,
            aggregate_projection: "class User:\n    id: int",
        },
        ProjectionCase {
            path: "model.go",
            language: Language::Go,
            source: "package model\n\nfunc greet(name string) string {\n\treturn \"Hi \" + name\n}\n\ntype User struct {\n\tID int\n\tName string\n}\n",
            callable: "greet",
            callable_projection: "func greet(name string) string",
            aggregate: "User",
            aggregate_kind: DeclarationKind::Struct,
            aggregate_projection: "type User struct {\n\tID int\n\tName string\n}",
        },
        ProjectionCase {
            path: "model.c",
            language: Language::C,
            source: "int greet(const char *name) {\n    return name[0];\n}\n\ntypedef struct User {\n    int id;\n    const char *name;\n} User;\n",
            callable: "greet",
            callable_projection: "int greet(const char *name)",
            aggregate: "User",
            aggregate_kind: DeclarationKind::Struct,
            aggregate_projection: "typedef struct User {\n    int id;\n    const char *name;\n} User;",
        },
        ProjectionCase {
            path: "model.cpp",
            language: Language::Cpp,
            source: "std::string greet(const std::string& name) {\n    return name;\n}\n\nstruct User {\n    int id;\n    std::string name;\n    std::string label() const { return name; }\n};\n",
            callable: "greet",
            callable_projection: "std::string greet(const std::string& name)",
            aggregate: "User",
            aggregate_kind: DeclarationKind::Struct,
            aggregate_projection: "struct User {\n    int id;\n    std::string name;\n};",
        },
    ];

    for case in cases {
        let facts = project(case.path, case.language, case.source)?;
        assert_complete(
            &facts.capabilities.callable_projection,
            "callable projection",
            case.language,
        );
        assert_complete(
            &facts.capabilities.aggregate_projection,
            "aggregate projection",
            case.language,
        );

        let callable = declaration_named(&facts, case.callable, DeclarationKind::Function)?;
        let aggregate = declaration_named(&facts, case.aggregate, case.aggregate_kind)?;
        assert_eq!(callable.projection_text, case.callable_projection);
        assert_eq!(aggregate.projection_text, case.aggregate_projection);
        assert!(
            !callable.projection_text.contains("return"),
            "{:?} callable leaked its body",
            case.language
        );
        assert!(
            !aggregate.projection_text.contains("return"),
            "{:?} aggregate leaked a method body",
            case.language
        );
        assert_eq!(callable.review_owner, callable.id);
        assert_eq!(aggregate.review_owner, aggregate.id);
    }

    Ok(())
}

#[test]
fn unicode_component_spans_are_valid_utf8_boundaries() -> Result<()> {
    const SOURCE: &str = "/// Résumé 🦀\npub fn café(名前: &str) -> String {\n    format!(\"Olá, {名前}\")\n}\n";

    let facts = project("src/unicode.rs", Language::Rust, SOURCE)?;
    let declaration = declaration_named(&facts, "café", DeclarationKind::Function)?;
    assert_eq!(
        declaration.projection_text,
        "/// Résumé 🦀\npub fn café(名前: &str) -> String"
    );
    assert_eq!(
        SOURCE.get(declaration.source_span.clone()),
        Some("/// Résumé 🦀\npub fn café(名前: &str) -> String {\n    format!(\"Olá, {名前}\")\n}")
    );
    for component in &declaration.components {
        assert_eq!(
            SOURCE.get(component.source_range.clone()),
            Some(component.text.as_str()),
            "component {:?} must use in-bounds UTF-8 byte boundaries",
            component.role
        );
    }

    Ok(())
}

#[test]
fn overloaded_declarations_receive_distinct_snapshot_local_ids() -> Result<()> {
    const SOURCE: &str = "int convert(int value) { return value; }\ndouble convert(double value) { return value; }\n";

    let facts = project("convert.cpp", Language::Cpp, SOURCE)?;
    let overloads = facts
        .declarations()
        .iter()
        .filter(|declaration| {
            declaration.name == "convert" && declaration.kind == DeclarationKind::Function
        })
        .collect::<Vec<_>>();
    assert_eq!(overloads.len(), 2);
    assert_ne!(overloads[0].id, overloads[1].id);
    assert_ne!(overloads[0].projection_hash, overloads[1].projection_hash);

    Ok(())
}

fn all_languages() -> [Language; 34] {
    [
        Language::Rust,
        Language::Swift,
        Language::Elisp,
        Language::JavaScript,
        Language::TypeScript,
        Language::Java,
        Language::Kotlin,
        Language::CSharp,
        Language::Python,
        Language::Ruby,
        Language::Php,
        Language::Go,
        Language::C,
        Language::Cpp,
        Language::Zig,
        Language::Lua,
        Language::Dart,
        Language::Scala,
        Language::Haskell,
        Language::OCaml,
        Language::Elixir,
        Language::Clojure,
        Language::Sql,
        Language::Yaml,
        Language::Json,
        Language::Html,
        Language::Css,
        Language::Shell,
        Language::Markdown,
        Language::Toml,
        Language::Nix,
        Language::Just,
        Language::Text,
        Language::Unknown,
    ]
}

fn representative_path(language: Language) -> &'static str {
    match language {
        Language::Rust => "empty.rs",
        Language::Swift => "empty.swift",
        Language::Elisp => "empty.el",
        Language::JavaScript => "empty.js",
        Language::TypeScript => "empty.ts",
        Language::Java => "empty.java",
        Language::Kotlin => "empty.kt",
        Language::CSharp => "empty.cs",
        Language::Python => "empty.py",
        Language::Ruby => "empty.rb",
        Language::Php => "empty.php",
        Language::Go => "empty.go",
        Language::C => "empty.c",
        Language::Cpp => "empty.cpp",
        Language::Zig => "empty.zig",
        Language::Lua => "empty.lua",
        Language::Dart => "empty.dart",
        Language::Scala => "empty.scala",
        Language::Haskell => "empty.hs",
        Language::OCaml => "empty.ml",
        Language::Elixir => "empty.ex",
        Language::Clojure => "empty.clj",
        Language::Sql => "empty.sql",
        Language::Yaml => "empty.yaml",
        Language::Json => "empty.json",
        Language::Html => "empty.html",
        Language::Css => "empty.css",
        Language::Shell => "empty.sh",
        Language::Markdown => "empty.md",
        Language::Toml => "empty.toml",
        Language::Nix => "empty.nix",
        Language::Just => "justfile",
        Language::Text => "empty.txt",
        Language::Unknown => "empty.unknown",
    }
}

fn assert_explicit_capability(capability: &Capability, language: Language, facet: &str) {
    match capability {
        Capability::Complete => {}
        Capability::Partial {
            missing_features,
            diagnostics,
        } => {
            assert!(
                !missing_features.is_empty(),
                "{language:?} {facet} partial support must name its missing features"
            );
            assert!(
                !diagnostics.is_empty(),
                "{language:?} {facet} partial support must explain its limitations"
            );
        }
        Capability::NotApplicable { reason } | Capability::Unsupported { reason } => assert!(
            !reason.trim().is_empty(),
            "{language:?} {facet} unavailable support must have an explicit reason"
        ),
    }
}

#[test]
fn every_language_reports_explicit_capability_for_every_declaration_facet() -> Result<()> {
    for language in all_languages() {
        let facts = project(representative_path(language), language, "")?;
        for (facet, capability) in [
            ("inventory", &facts.capabilities.inventory),
            (
                "documentation association",
                &facts.capabilities.documentation_association,
            ),
            (
                "callable projection",
                &facts.capabilities.callable_projection,
            ),
            (
                "aggregate projection",
                &facts.capabilities.aggregate_projection,
            ),
            ("type-use classification", &facts.capabilities.type_use_sites),
        ] {
            assert_explicit_capability(capability, language, facet);
        }
        assert!(
            facts
                .diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.message.trim().is_empty()),
            "{language:?} returned an empty projection diagnostic"
        );
    }

    Ok(())
}
