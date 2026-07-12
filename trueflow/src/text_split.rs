use crate::block::Block;
use regex::Regex;
use std::sync::OnceLock;

static PARAGRAPH_BREAK: OnceLock<Regex> = OnceLock::new();

pub fn paragraph_break_regex() -> &'static Regex {
    PARAGRAPH_BREAK.get_or_init(|| {
        Regex::new(
            r"(?:\r\n[^\S\r\n]*(?:\r\n|\r|\n)|\r[^\S\r\n]*(?:\r\n|\r)|\n[^\S\r\n]*(?:\r\n|\r|\n))(?:[^\S\r\n]*(?:\r\n|\r|\n))*",
        )
            .unwrap_or_else(|error| panic!("invalid paragraph regex: {error}"))
    })
}

pub fn split_by_paragraph_breaks<F>(content: &str, mut make_block: F) -> Vec<Block>
where
    F: FnMut(&str, usize, usize, bool) -> Block,
{
    let re = paragraph_break_regex();
    let mut blocks = Vec::new();
    let mut start_offset = 0;

    for mat in re.find_iter(content) {
        let end_offset = mat.start();
        if start_offset < end_offset {
            let chunk = &content[start_offset..end_offset];
            if !chunk.is_empty() {
                blocks.push(make_block(chunk, start_offset, end_offset, false));
            }
        }

        let gap_chunk = &content[mat.start()..mat.end()];
        blocks.push(make_block(gap_chunk, mat.start(), mat.end(), true));

        start_offset = mat.end();
    }

    if start_offset < content.len() {
        let chunk = &content[start_offset..];
        if !chunk.is_empty() {
            blocks.push(make_block(chunk, start_offset, content.len(), false));
        }
    }

    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraph_break_consumes_complete_newline_sequences() {
        for (content, expected_gap) in
            [("alpha\r\n\r\nbeta", "\r\n\r\n"), ("alpha\r\rbeta", "\r\r")]
        {
            let paragraph_break = paragraph_break_regex()
                .find(content)
                .unwrap_or_else(|| panic!("expected paragraph break in {content:?}"));

            assert_eq!(&content[..paragraph_break.start()], "alpha");
            assert_eq!(&content[paragraph_break.range()], expected_gap);
            assert_eq!(&content[paragraph_break.end()..], "beta");
        }
    }
}
