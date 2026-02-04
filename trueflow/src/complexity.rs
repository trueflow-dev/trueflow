use crate::analysis::Language;
use tree_sitter::{Node, Parser};

pub fn calculate(content: &str, lang: Language) -> u32 {
    if lang == Language::Unknown || lang == Language::Text || lang == Language::Markdown {
        return 0;
    }

    let mut parser = Parser::new();
    let language = match lang {
        Language::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        Language::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
        Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        Language::Python => Some(tree_sitter_python::LANGUAGE.into()),
        Language::Shell => Some(tree_sitter_bash::LANGUAGE.into()),
        _ => None,
    };

    let Some(language) = language else {
        return 0;
    };

    if parser.set_language(&language).is_err() {
        return 0;
    }

    match parser.parse(content, None) {
        Some(tree) => calculate_node(tree.root_node(), 0, &lang),
        None => 0,
    }
}

fn calculate_node(node: Node, nesting: u32, lang: &Language) -> u32 {
    let mut score = 0;
    let kind = node.kind();

    let is_control_flow = match lang {
        Language::Rust => matches!(
            kind,
            "if_expression"
                | "for_expression"
                | "while_expression"
                | "loop_expression"
                | "match_expression"
        ),
        Language::JavaScript | Language::TypeScript => matches!(
            kind,
            "if_statement"
                | "for_statement"
                | "while_statement"
                | "do_statement"
                | "switch_statement"
                | "catch_clause"
                | "ternary_expression"
        ),
        Language::Python => matches!(
            kind,
            "if_statement"
                | "for_statement"
                | "while_statement"
                | "try_statement"
                | "except_clause"
        ),
        Language::Shell => matches!(
            kind,
            "if_statement" | "for_statement" | "while_statement" | "case_statement"
        ),
        _ => false,
    };

    let is_logical_op = match lang {
        Language::Rust => matches!(kind, "&&" | "||"),
        Language::JavaScript | Language::TypeScript => matches!(kind, "&&" | "||" | "??"),
        Language::Python => matches!(kind, "and" | "or"), // Python uses 'boolean_operator' usually, need to check grammar
        Language::Shell => matches!(kind, "&&" | "||"),
        _ => false,
    };

    // Check specific logical operators for Python/others if nodes are named "boolean_operator"
    if (matches!(lang, Language::Python) && kind == "boolean_operator") || is_logical_op {
        score += 1;
    }

    if is_control_flow {
        score += 1 + nesting;
        // Increase nesting for children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            score += calculate_node(child, nesting + 1, lang);
        }
    } else {
        // Just recurse without increasing nesting, unless it's a function definition which resets nesting?
        // Cognitive complexity says functions nest but usually we start counting FROM the function.
        // Since we are analyzing a block which IS a function (mostly), we start at 0.
        // If we encounter a nested function, it should probably increment nesting or complexity?
        // Sonar says: "else", "catch" etc don't increment nesting level but pay for it.
        // This is a simplified implementation.

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            score += calculate_node(child, nesting, lang);
        }
    }

    // Special case for 'else' and 'else if' - they pay nesting but don't increment it?
    // Simplified: Just +1 + nesting for now.

    score
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_complexity_rust() {
        let code = "fn foo() { if true { for x in 0..10 { } } }";
        // if (+1) + for (+1 + nesting 1) = 3
        let score = calculate(code, Language::Rust);
        assert_eq!(score, 3);
    }

    #[test]
    fn test_calculate_complexity_nesting() {
        let code = "
        if a {
            if b {
                if c {
                }
            }
        }";
        // if a: +1
        // if b: +1 + 1 (nesting) = 2
        // if c: +1 + 2 (nesting) = 3
        // Total: 6
        let score = calculate(code, Language::Rust);
        assert_eq!(score, 6);
    }

    #[test]
    fn test_calculate_complexity_python() {
        let code = "
def foo():
    if True:
        try:
            pass
        except:
            pass
";
        // if: +1 (nesting 0) = 1
        // try: +1 + 1 (nesting 1, child of if) = 2
        // except: +1 + 2 (nesting 2, child of try) = 3
        // Total: 6
        let score = calculate(code, Language::Python);
        assert_eq!(score, 6);
    }
}
