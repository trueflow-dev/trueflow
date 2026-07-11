use crate::block::BlockKind;
use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TomlSpan {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) kind: BlockKind,
}

impl TomlSpan {
    fn new(start: usize, end: usize, kind: BlockKind) -> Self {
        Self { start, end, kind }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StringMode {
    Basic,
    Literal,
    MultilineBasic,
    MultilineLiteral,
}

pub(crate) fn split_document(source: &str) -> Result<Vec<TomlSpan>> {
    let headers = find_document_header_starts(source);
    let mut spans = Vec::new();

    let prefix_end = headers.first().copied().unwrap_or(source.len());
    spans.extend(scan_scope_entries(&source[..prefix_end], 0));

    for (index, start) in headers.iter().copied().enumerate() {
        let end = headers.get(index + 1).copied().unwrap_or(source.len());
        spans.push(TomlSpan::new(start, end, BlockKind::Section));
    }

    Ok(spans)
}

pub(crate) fn split_section_children(source: &str) -> Result<Vec<TomlSpan>> {
    if line_starts_with_header_at(source, 0) {
        let body_start = next_line_start(source, 0);
        return Ok(scan_scope_entries(&source[body_start..], body_start));
    }

    if let Some((inline_start, inline_end)) = find_inline_table_value_span(source) {
        return Ok(scan_inline_table_entries(source, inline_start, inline_end));
    }

    Ok(Vec::new())
}

pub(crate) fn split_list_children(source: &str) -> Result<Vec<TomlSpan>> {
    let Some((array_start, array_end)) = find_array_value_span(source) else {
        return Ok(Vec::new());
    };

    Ok(scan_array_entries(source, array_start, array_end))
}

fn find_document_header_starts(source: &str) -> Vec<usize> {
    let mut headers = Vec::new();
    let mut position = 0usize;

    while position < source.len() {
        if line_is_blank(source, position) {
            position = next_line_start(source, position);
            continue;
        }

        if line_is_comment_only(source, position) {
            position = next_line_start(source, position);
            continue;
        }

        if line_starts_with_header_at(source, position) {
            headers.push(position);
            position = next_line_start(source, position);
            continue;
        }

        let end = scan_key_value_block(source, position);
        if end <= position {
            position = next_line_start(source, position);
        } else {
            position = end;
        }
    }

    headers
}

fn scan_scope_entries(scope: &str, base_offset: usize) -> Vec<TomlSpan> {
    let mut spans = Vec::new();
    let mut position = 0usize;

    while position < scope.len() {
        if line_is_blank(scope, position) {
            position = next_line_start(scope, position);
            continue;
        }

        if line_is_comment_only(scope, position) {
            let comment_start = position;
            let mut comment_end = next_line_start(scope, position);
            position = comment_end;
            while position < scope.len()
                && (line_is_comment_only(scope, position) || line_is_blank(scope, position))
            {
                comment_end = next_line_start(scope, position);
                position = comment_end;
            }
            spans.push(TomlSpan::new(
                base_offset + comment_start,
                base_offset + comment_end,
                BlockKind::Comment,
            ));
            continue;
        }

        let end = scan_key_value_block(scope, position);
        if end <= position {
            position = next_line_start(scope, position);
            continue;
        }

        spans.push(TomlSpan::new(
            base_offset + position,
            base_offset + end,
            classify_assignment_chunk(&scope[position..end]),
        ));
        position = end;
    }

    spans
}

fn scan_inline_table_entries(
    source: &str,
    inline_start: usize,
    inline_end: usize,
) -> Vec<TomlSpan> {
    scan_comma_separated_items(
        &source[inline_start + 1..inline_end - 1],
        inline_start + 1,
        classify_assignment_chunk,
    )
}

fn scan_array_entries(source: &str, array_start: usize, array_end: usize) -> Vec<TomlSpan> {
    scan_comma_separated_items(
        &source[array_start + 1..array_end - 1],
        array_start + 1,
        classify_value_chunk,
    )
}

fn scan_comma_separated_items(
    source: &str,
    base_offset: usize,
    classify: fn(&str) -> BlockKind,
) -> Vec<TomlSpan> {
    let bytes = source.as_bytes();
    let mut spans = Vec::new();
    let mut item_start = 0usize;
    let mut index = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut line_comment = false;
    let mut string_mode = None;

    while index < bytes.len() {
        if line_comment {
            if bytes[index] == b'\n' {
                line_comment = false;
            }
            index += 1;
            continue;
        }

        if let Some(mode) = string_mode {
            if let Some(next) = advance_string(bytes, index, mode) {
                index = next;
                continue;
            }
            string_mode = None;
            index += closing_quote_len(mode);
            continue;
        }

        match bytes[index] {
            b'#' => {
                line_comment = true;
                index += 1;
            }
            b'"' | b'\'' => {
                string_mode = Some(start_string_mode(bytes, index));
                index += opening_quote_len(bytes, index);
            }
            b'[' => {
                bracket_depth += 1;
                index += 1;
            }
            b']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                index += 1;
            }
            b'{' => {
                brace_depth += 1;
                index += 1;
            }
            b'}' => {
                brace_depth = brace_depth.saturating_sub(1);
                index += 1;
            }
            b',' if bracket_depth == 0 && brace_depth == 0 => {
                push_trimmed_item_span(
                    &mut spans,
                    source,
                    base_offset,
                    item_start,
                    index,
                    classify,
                );
                item_start = index + 1;
                index += 1;
            }
            _ => {
                index += 1;
            }
        }
    }

    push_trimmed_item_span(
        &mut spans,
        source,
        base_offset,
        item_start,
        source.len(),
        classify,
    );
    spans
}

fn push_trimmed_item_span(
    spans: &mut Vec<TomlSpan>,
    source: &str,
    base_offset: usize,
    start: usize,
    end: usize,
    classify: fn(&str) -> BlockKind,
) {
    let Some((trimmed_start, trimmed_end)) = trim_ascii_whitespace_range(source, start, end) else {
        return;
    };

    spans.push(TomlSpan::new(
        base_offset + trimmed_start,
        base_offset + trimmed_end,
        classify(&source[trimmed_start..trimmed_end]),
    ));
}

fn classify_assignment_chunk(chunk: &str) -> BlockKind {
    find_assignment_value_start(chunk)
        .map(|value_start| classify_value_chunk(&chunk[value_start..]))
        .unwrap_or(BlockKind::Content)
}

fn classify_value_chunk(chunk: &str) -> BlockKind {
    match chunk.trim_start().as_bytes().first().copied() {
        Some(b'[') => BlockKind::List,
        Some(b'{') => BlockKind::Section,
        _ => BlockKind::Content,
    }
}

fn find_inline_table_value_span(source: &str) -> Option<(usize, usize)> {
    let value_start = find_assignment_value_start(source)?;
    let start = skip_ascii_whitespace(source, value_start);
    (source.as_bytes().get(start) == Some(&b'{'))
        .then(|| find_matching_delimiter(source, start, b'{', b'}'))?
}

fn find_array_value_span(source: &str) -> Option<(usize, usize)> {
    let value_start = find_assignment_value_start(source)?;
    let start = skip_ascii_whitespace(source, value_start);
    (source.as_bytes().get(start) == Some(&b'['))
        .then(|| find_matching_delimiter(source, start, b'[', b']'))?
}

fn find_assignment_value_start(source: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut index = 0usize;
    let mut line_comment = false;
    let mut string_mode = None;

    while index < bytes.len() {
        if line_comment {
            if bytes[index] == b'\n' {
                line_comment = false;
            }
            index += 1;
            continue;
        }

        if let Some(mode) = string_mode {
            if let Some(next) = advance_string(bytes, index, mode) {
                index = next;
                continue;
            }
            string_mode = None;
            index += closing_quote_len(mode);
            continue;
        }

        match bytes[index] {
            b'#' => {
                line_comment = true;
                index += 1;
            }
            b'"' | b'\'' => {
                string_mode = Some(start_string_mode(bytes, index));
                index += opening_quote_len(bytes, index);
            }
            b'=' => return Some(skip_ascii_whitespace(source, index + 1)),
            _ => index += 1,
        }
    }

    None
}

fn find_matching_delimiter(
    source: &str,
    open_index: usize,
    open: u8,
    _close: u8,
) -> Option<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut index = open_index + 1;
    let mut bracket_depth = usize::from(open == b'[');
    let mut brace_depth = usize::from(open == b'{');
    let mut line_comment = false;
    let mut string_mode = None;

    while index < bytes.len() {
        if line_comment {
            if bytes[index] == b'\n' {
                line_comment = false;
            }
            index += 1;
            continue;
        }

        if let Some(mode) = string_mode {
            if let Some(next) = advance_string(bytes, index, mode) {
                index = next;
                continue;
            }
            string_mode = None;
            index += closing_quote_len(mode);
            continue;
        }

        match bytes[index] {
            b'#' => {
                line_comment = true;
                index += 1;
            }
            b'"' | b'\'' => {
                string_mode = Some(start_string_mode(bytes, index));
                index += opening_quote_len(bytes, index);
            }
            b'[' => {
                bracket_depth += 1;
                index += 1;
            }
            b']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                index += 1;
                if open == b'[' && bracket_depth == 0 && brace_depth == 0 {
                    return Some((open_index, index));
                }
            }
            b'{' => {
                brace_depth += 1;
                index += 1;
            }
            b'}' => {
                brace_depth = brace_depth.saturating_sub(1);
                index += 1;
                if open == b'{' && brace_depth == 0 && bracket_depth == 0 {
                    return Some((open_index, index));
                }
            }
            _ => {
                index += 1;
            }
        }
    }

    None
}

fn scan_key_value_block(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut index = start;
    let mut seen_equals = false;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut line_comment = false;
    let mut string_mode = None;

    while index < bytes.len() {
        if line_comment {
            if bytes[index] == b'\n' {
                line_comment = false;
                if seen_equals && bracket_depth == 0 && brace_depth == 0 {
                    return index + 1;
                }
            }
            index += 1;
            continue;
        }

        if let Some(mode) = string_mode {
            if let Some(next) = advance_string(bytes, index, mode) {
                index = next;
                continue;
            }
            string_mode = None;
            index += closing_quote_len(mode);
            continue;
        }

        match bytes[index] {
            b'#' => {
                line_comment = true;
                index += 1;
            }
            b'"' | b'\'' => {
                string_mode = Some(start_string_mode(bytes, index));
                index += opening_quote_len(bytes, index);
            }
            b'=' if !seen_equals => {
                seen_equals = true;
                index += 1;
            }
            b'[' if seen_equals => {
                bracket_depth += 1;
                index += 1;
            }
            b']' if seen_equals => {
                bracket_depth = bracket_depth.saturating_sub(1);
                index += 1;
            }
            b'{' if seen_equals => {
                brace_depth += 1;
                index += 1;
            }
            b'}' if seen_equals => {
                brace_depth = brace_depth.saturating_sub(1);
                index += 1;
            }
            b'\n' if seen_equals && bracket_depth == 0 && brace_depth == 0 => {
                return index + 1;
            }
            b'\n' => {
                index += 1;
                if !seen_equals {
                    return index;
                }
            }
            _ => {
                index += 1;
            }
        }
    }

    source.len()
}

fn start_string_mode(bytes: &[u8], index: usize) -> StringMode {
    match bytes[index] {
        b'"' if starts_with(bytes, index, b"\"\"\"") => StringMode::MultilineBasic,
        b'"' => StringMode::Basic,
        b'\'' if starts_with(bytes, index, b"'''") => StringMode::MultilineLiteral,
        b'\'' => StringMode::Literal,
        _ => StringMode::Basic,
    }
}

fn opening_quote_len(bytes: &[u8], index: usize) -> usize {
    if starts_with(bytes, index, b"\"\"\"") || starts_with(bytes, index, b"'''") {
        3
    } else {
        1
    }
}

fn closing_quote_len(mode: StringMode) -> usize {
    match mode {
        StringMode::Basic | StringMode::Literal => 1,
        StringMode::MultilineBasic | StringMode::MultilineLiteral => 3,
    }
}

fn advance_string(bytes: &[u8], index: usize, mode: StringMode) -> Option<usize> {
    match mode {
        StringMode::Basic => match bytes[index] {
            b'\\' => Some((index + 2).min(bytes.len())),
            b'"' => None,
            _ => Some(index + 1),
        },
        StringMode::Literal => match bytes[index] {
            b'\'' => None,
            _ => Some(index + 1),
        },
        StringMode::MultilineBasic => {
            if starts_with(bytes, index, b"\"\"\"") {
                None
            } else if bytes[index] == b'\\' {
                Some((index + 2).min(bytes.len()))
            } else {
                Some(index + 1)
            }
        }
        StringMode::MultilineLiteral => {
            if starts_with(bytes, index, b"'''") {
                None
            } else {
                Some(index + 1)
            }
        }
    }
}

fn trim_ascii_whitespace_range(source: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut trimmed_start = start;
    while trimmed_start < end && bytes[trimmed_start].is_ascii_whitespace() {
        trimmed_start += 1;
    }

    let mut trimmed_end = end;
    while trimmed_end > trimmed_start && bytes[trimmed_end - 1].is_ascii_whitespace() {
        trimmed_end -= 1;
    }

    (trimmed_start < trimmed_end).then_some((trimmed_start, trimmed_end))
}

fn skip_ascii_whitespace(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut index = start;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    index
}

fn line_starts_with_header_at(source: &str, line_start: usize) -> bool {
    let line_end = next_line_start(source, line_start);
    let line = &source[line_start..line_end];
    let trimmed = line.trim_start();
    !trimmed.is_empty()
        && !trimmed.starts_with('#')
        && trimmed.starts_with('[')
        && !trimmed.contains('=')
        && (trimmed.trim_end().ends_with(']') || trimmed.trim_end().ends_with("]]"))
}

fn line_is_blank(source: &str, line_start: usize) -> bool {
    let line_end = next_line_start(source, line_start);
    source[line_start..line_end].trim().is_empty()
}

fn line_is_comment_only(source: &str, line_start: usize) -> bool {
    let line_end = next_line_start(source, line_start);
    source[line_start..line_end].trim_start().starts_with('#')
}

fn next_line_start(source: &str, start: usize) -> usize {
    source[start..]
        .find('\n')
        .map(|offset| start + offset + 1)
        .unwrap_or(source.len())
}

fn starts_with(bytes: &[u8], index: usize, pattern: &[u8]) -> bool {
    bytes.get(index..index + pattern.len()) == Some(pattern)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_document_emits_root_keys_and_sections() {
        let source = "title = \"deploy\"\nkeywords = [\"blue\", \"green\"]\n\n[owner]\nname = \"platform\"\n";
        let spans = split_document(source).unwrap();
        let kinds = spans.iter().map(|span| span.kind).collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![BlockKind::Content, BlockKind::List, BlockKind::Section]
        );
        assert_eq!(
            &source[spans[0].start..spans[0].end],
            "title = \"deploy\"\n"
        );
    }


    #[test]
    fn split_inline_table_children_emits_key_value_pairs() {
        let source = "targets = { primary = \"cache\", secondary = \"backup\" }";
        let spans = split_section_children(source).unwrap();
        let items = spans
            .iter()
            .map(|span| &source[span.start..span.end])
            .collect::<Vec<_>>();
        assert_eq!(items, vec!["primary = \"cache\"", "secondary = \"backup\""]);
    }

    #[test]
    fn split_array_children_emits_scalar_items() {
        let source = "keywords = [\"blue\", \"green\"]";
        let spans = split_list_children(source).unwrap();
        let items = spans
            .iter()
            .map(|span| &source[span.start..span.end])
            .collect::<Vec<_>>();
        assert_eq!(items, vec!["\"blue\"", "\"green\""]);
    }
}
