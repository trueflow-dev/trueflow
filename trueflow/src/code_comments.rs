pub fn line_is_c_style_comment(trimmed_line: &str) -> bool {
    trimmed_line.starts_with("//")
        || trimmed_line.starts_with("/*")
        || trimmed_line.starts_with('*')
        || trimmed_line.starts_with("*/")
}

pub fn chunk_is_comment_only(chunk: &str) -> bool {
    let mut in_block_comment = false;
    let mut saw_comment = false;

    for source_line in chunk.lines() {
        let mut line = source_line.trim_start();
        loop {
            if line.is_empty() {
                break;
            }

            if in_block_comment {
                saw_comment = true;
                if let Some(end) = line.find("*/") {
                    line = line[end + 2..].trim_start();
                    in_block_comment = false;
                    continue;
                }
                break;
            }

            if line.starts_with('#') || line.starts_with("//") {
                saw_comment = true;
                break;
            }

            if let Some(comment) = line.strip_prefix("/*") {
                saw_comment = true;
                in_block_comment = true;
                line = comment;
                continue;
            }

            // Split chunks can begin inside a block comment, before a leading `*`.
            if line.starts_with('*') {
                saw_comment = true;
                if let Some(end) = line.find("*/") {
                    line = line[end + 2..].trim_start();
                    continue;
                }
                break;
            }

            return false;
        }
    }

    saw_comment
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_is_comment_only_accepts_comment_lines() {
        assert!(chunk_is_comment_only("# one\n// two\n/* three\n* four\n*/"));
    }

    #[test]
    fn chunk_is_comment_only_rejects_code_lines() {
        assert!(!chunk_is_comment_only("// note\nlet value = 1;"));
    }

    #[test]
    fn chunk_is_comment_only_rejects_code_after_block_comment() {
        assert!(!chunk_is_comment_only("/* rationale */ dangerous_call();"));
    }

    #[test]
    fn line_is_c_style_comment_accepts_closing_block_markers() {
        assert!(line_is_c_style_comment("*/"));
    }
}
