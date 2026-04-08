use super::{LanguageRegistration, generic_tree_sitter_registration};
use tree_sitter::Language as TsLanguage;

pub(crate) fn registration() -> LanguageRegistration {
    generic_tree_sitter_registration(parser_language)
}

fn parser_language(_content: &str) -> TsLanguage {
    tree_sitter_html::LANGUAGE.into()
}
