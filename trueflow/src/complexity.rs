use crate::analysis::Language;
use tree_sitter::{Node, Parser};

pub fn calculate(content: &str, lang: Language) -> Option<u32> {
    if matches!(
        lang,
        Language::Unknown | Language::Text | Language::Markdown
    ) {
        return None;
    }

    let mut parser = Parser::new();
    let language = match lang {
        Language::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        Language::Swift => Some(tree_sitter_swift::LANGUAGE.into()),
        Language::Elisp => Some(tree_sitter_elisp::LANGUAGE.into()),
        Language::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
        Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        Language::Java => Some(tree_sitter_java::LANGUAGE.into()),
        Language::Kotlin => Some(tree_sitter_kotlin_ng::LANGUAGE.into()),
        Language::CSharp => Some(tree_sitter_c_sharp::LANGUAGE.into()),
        Language::Ruby => Some(tree_sitter_ruby::LANGUAGE.into()),
        Language::Php => Some(if content.contains("<?") {
            tree_sitter_php::LANGUAGE_PHP.into()
        } else {
            tree_sitter_php::LANGUAGE_PHP_ONLY.into()
        }),
        Language::Go => Some(tree_sitter_go::LANGUAGE.into()),
        Language::C => Some(tree_sitter_c::LANGUAGE.into()),
        Language::Cpp => Some(tree_sitter_cpp::LANGUAGE.into()),
        Language::Python => Some(tree_sitter_python::LANGUAGE.into()),
        Language::Shell => Some(tree_sitter_bash::LANGUAGE.into()),
        _ => None,
    }?;

    if parser.set_language(&language).is_err() {
        return None;
    }

    parser
        .parse(content, None)
        .map(|tree| calculate_node(tree.root_node(), 0, lang, content))
}

fn calculate_node(node: Node<'_>, nesting: u32, lang: Language, source: &str) -> u32 {
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
        Language::Elisp => {
            matches!(kind, "if" | "cond" | "while")
                || elisp_list_head_symbol(node, source).is_some_and(|head| {
                    matches!(head, "when" | "unless" | "dolist" | "dotimes" | "pcase")
                })
        }
        Language::Swift => matches!(
            kind,
            "if_statement"
                | "for_statement"
                | "while_statement"
                | "repeat_while_statement"
                | "guard_statement"
                | "switch_statement"
                | "do_statement"
                | "ternary_expression"
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
        Language::Java => matches!(
            kind,
            "if_statement"
                | "for_statement"
                | "enhanced_for_statement"
                | "while_statement"
                | "do_statement"
                | "switch_expression"
                | "catch_clause"
                | "ternary_expression"
        ),
        Language::Kotlin => matches!(
            kind,
            "if_expression"
                | "when_expression"
                | "for_statement"
                | "while_statement"
                | "do_while_statement"
                | "catch_block"
        ),
        Language::CSharp => matches!(
            kind,
            "if_statement"
                | "for_statement"
                | "foreach_statement"
                | "while_statement"
                | "do_statement"
                | "switch_statement"
                | "switch_expression"
                | "catch_clause"
                | "conditional_expression"
        ),
        Language::Ruby => {
            node.is_named()
                && matches!(
                    kind,
                    "if" | "if_modifier"
                        | "unless"
                        | "unless_modifier"
                        | "case"
                        | "for"
                        | "while"
                        | "until"
                        | "rescue"
                        | "conditional"
                )
        }
        Language::Php => matches!(
            kind,
            "if_statement"
                | "switch_statement"
                | "match_expression"
                | "for_statement"
                | "foreach_statement"
                | "while_statement"
                | "do_statement"
                | "catch_clause"
                | "conditional_expression"
        ),
        Language::Go => matches!(
            kind,
            "if_statement"
                | "for_statement"
                | "expression_switch_statement"
                | "type_switch_statement"
                | "select_statement"
        ),
        Language::C => matches!(
            kind,
            "if_statement"
                | "switch_statement"
                | "for_statement"
                | "while_statement"
                | "do_statement"
                | "conditional_expression"
                | "preproc_if"
                | "preproc_ifdef"
                | "preproc_elif"
                | "preproc_else"
        ),
        Language::Cpp => matches!(
            kind,
            "if_statement"
                | "switch_statement"
                | "for_statement"
                | "for_range_loop"
                | "while_statement"
                | "do_statement"
                | "conditional_expression"
                | "preproc_if"
                | "preproc_ifdef"
                | "preproc_elif"
                | "preproc_else"
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
        Language::Elisp => matches!(kind, "and" | "or"),
        Language::Swift => {
            matches!(
                kind,
                "conjunction_expression" | "disjunction_expression" | "nil_coalescing_expression"
            )
        }
        Language::JavaScript | Language::TypeScript => matches!(kind, "&&" | "||" | "??"),
        Language::Java => matches!(kind, "&&" | "||"),
        Language::Kotlin => matches!(kind, "&&" | "||" | "?:"),
        Language::CSharp => matches!(kind, "&&" | "||" | "??"),
        Language::Ruby => matches!(kind, "&&" | "||" | "and" | "or"),
        Language::Php => matches!(kind, "&&" | "||" | "??"),
        Language::Go => matches!(kind, "&&" | "||"),
        Language::C => matches!(kind, "&&" | "||"),
        Language::Cpp => matches!(kind, "&&" | "||"),
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
            score += calculate_node(child, nesting + 1, lang, source);
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
            score += calculate_node(child, nesting, lang, source);
        }
    }

    // Special case for 'else' and 'else if' - they pay nesting but don't increment it?
    // Simplified: Just +1 + nesting for now.

    score
}

fn elisp_list_head_symbol<'a>(node: Node<'a>, source: &'a str) -> Option<&'a str> {
    let head = node.named_child(0)?;
    (head.kind() == "symbol")
        .then(|| head.utf8_text(source.as_bytes()).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_complexity_rust() {
        let code = "fn foo() { if true { for x in 0..10 { } } }";
        // if (+1) + for (+1 + nesting 1) = 3
        let score = calculate(code, Language::Rust);
        assert_eq!(score, Some(3));
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
        assert_eq!(score, Some(6));
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
        assert_eq!(score, Some(6));
    }

    #[test]
    fn test_calculate_complexity_elisp() {
        let code = "
(defun run (items)
  (dolist (item items)
    (when item
      (message \"%s\" item)))
  (and items t))
";
        // dolist: +1
        // when inside dolist: +1 + nesting 1 = 2
        // and: +1
        // Total: 4
        let score = calculate(code, Language::Elisp);
        assert_eq!(score, Some(4));
    }

    #[test]
    fn test_calculate_complexity_swift() {
        let code = "
func run(_ values: [Int]) -> Int {
    var result = 0
    for value in values {
        if value > 0 {
            result += value
        }
    }
    return result
}";
        let score = calculate(code, Language::Swift);
        assert_eq!(score, Some(3));
    }

    #[test]
    fn test_calculate_complexity_kotlin() {
        let code = "
fun run(values: List<Int>): Int {
    for (value in values) {
        if (value > 0 && value < 10) {
            return value
        }
    }
    return 0
}
";
        // for: +1
        // if inside for: +1 + nesting 1 = 2
        // &&: +1
        // Total: 4
        let score = calculate(code, Language::Kotlin);
        assert_eq!(score, Some(4));
    }

    #[test]
    fn test_calculate_complexity_returns_none_for_textual_or_unsupported_languages() {
        assert_eq!(calculate("plain text", Language::Text), None);
        assert_eq!(calculate("# heading", Language::Markdown), None);
        assert_eq!(calculate("whatever", Language::Unknown), None);
        assert_eq!(calculate("key = \"value\"", Language::Toml), None);
    }

    #[test]
    fn test_calculate_complexity_csharp() {
        let code = "
int Build(int?[] values, bool ready) {
    for (var index = 0; index < values.Length; index++) {
        var current = values[index] ?? 0;
        if (current > 0 && ready) {
            return current;
        }
    }
    return 0;
}
";
        // for: +1
        // ??: +1
        // if inside for: +1 + nesting 1 = 2
        // &&: +1
        // Total: 5
        let score = calculate(code, Language::CSharp);
        assert_eq!(score, Some(5));
    }

    #[test]
    fn test_calculate_complexity_java() {
        let code = "
int process(int[] values) {
    int total = 0;
    for (int value : values) {
        if (value > 0) {
            total += value;
        }
    }
    return total;
}";
        let score = calculate(code, Language::Java);
        assert_eq!(score, Some(3));
    }

    #[test]
    fn test_calculate_complexity_ruby() {
        let code = "
def process(ready, value)
  if ready && value > 0
    if value.zero?
      return 0
    end
  end
end
";
        // outer if: +1
        // &&: +1
        // inner if inside outer if: +1 + nesting 1 = 2
        // Total: 4
        let score = calculate(code, Language::Ruby);
        assert_eq!(score, Some(4));
    }

    #[test]
    fn test_calculate_complexity_php() {
        let code = "
function processData(array $values, bool $ready): int {
    foreach ($values as $value) {
        $current = $value ?? 0;
        if ($ready && $current > 0) {
            return $current;
        }
    }
    return 0;
}
";
        // foreach: +1
        // ??: +1
        // if inside foreach: +1 + nesting 1 = 2
        // &&: +1
        // Total: 5
        let score = calculate(code, Language::Php);
        assert_eq!(score, Some(5));
    }

    #[test]
    fn test_calculate_complexity_c() {
        let code = "
#if ENABLE_FEATURE
int run(int flag, int ready) {
    if (flag && ready) {
        return 1;
    }
    return 0;
}
#endif
";
        // preproc_if: +1
        // if inside preproc_if: +1 + nesting 1 = 2
        // &&: +1
        // Total: 4
        let score = calculate(code, Language::C);
        assert_eq!(score, Some(4));
    }

    #[test]
    fn test_calculate_complexity_go() {
        let code = "
func process(values []int, ready bool) int {
    for _, value := range values {
        if ready && value > 0 {
            return value
        }
    }
    return 0
}
";
        // for: +1
        // if inside for: +1 + nesting 1 = 2
        // &&: +1
        // Total: 4
        let score = calculate(code, Language::Go);
        assert_eq!(score, Some(4));
    }

    #[test]
    fn test_calculate_complexity_cpp() {
        let code = "
int process(const std::vector<int>& values, bool ready) {
    for (int value : values) {
        if (ready && value > 0) {
            return value;
        }
    }
    return 0;
}
";
        // range-for: +1
        // if inside range-for: +1 + nesting 1 = 2
        // &&: +1
        // Total: 4
        let score = calculate(code, Language::Cpp);
        assert_eq!(score, Some(4));
    }
}
