pub fn line_is_c_style_comment(trimmed_line: &str) -> bool {
    trimmed_line.starts_with("//")
        || trimmed_line.starts_with("/*")
        || trimmed_line.starts_with('*')
        || trimmed_line.starts_with("*/")
}

pub fn chunk_is_comment_only(chunk: &str) -> bool {
    let trimmed = chunk.trim();
    !trimmed.is_empty()
        && trimmed
            .lines()
            .all(|line| line_is_c_style_comment_or_hash(line.trim_start()))
}

fn line_is_c_style_comment_or_hash(trimmed_line: &str) -> bool {
    trimmed_line.starts_with('#') || line_is_c_style_comment(trimmed_line)
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
    fn line_is_c_style_comment_accepts_closing_block_markers() {
        assert!(line_is_c_style_comment("*/"));
    }
}
