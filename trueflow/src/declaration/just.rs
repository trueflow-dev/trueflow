use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};

use crate::analysis::Language;

use super::projection::{declaration_id, declaration_key};
use super::{
    DeclarationId, DeclarationKind, DeclarationNode, ProjectionDiagnostic, SourceComponent,
    SourceComponentRole, Visibility, projection_hash,
};

#[derive(Debug, Clone, Copy)]
struct SourceLine<'a> {
    start: usize,
    content_end: usize,
    text: &'a str,
}

#[derive(Debug, Clone)]
struct Attachment {
    range: Range<usize>,
    role: SourceComponentRole,
}

struct Projector<'a> {
    path: &'a Path,
    source: &'a str,
    lines: Vec<SourceLine<'a>>,
    next_ordinal: usize,
    declarations: Vec<DeclarationNode>,
    diagnostics: Vec<ProjectionDiagnostic>,
}

pub(super) fn project(
    path: &Path,
    source: &str,
) -> Result<(Vec<DeclarationNode>, Vec<ProjectionDiagnostic>)> {
    let mut projector = Projector {
        path,
        source,
        lines: source_lines(source),
        next_ordinal: 0,
        declarations: Vec::new(),
        diagnostics: Vec::new(),
    };
    projector.collect()?;
    Ok((projector.declarations, projector.diagnostics))
}

impl Projector<'_> {
    fn collect(&mut self) -> Result<()> {
        let mut pending = Vec::<Attachment>::new();
        let mut index = 0usize;
        while index < self.lines.len() {
            let line = self.lines[index];
            let trimmed = line.text.trim();
            if trimmed.is_empty() {
                pending.clear();
                index += 1;
                continue;
            }
            if line
                .text
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_whitespace)
            {
                pending.clear();
                index += 1;
                continue;
            }
            if trimmed.starts_with('#') {
                pending.push(Attachment {
                    range: line.start..line.content_end,
                    role: SourceComponentRole::Documentation,
                });
                index += 1;
                continue;
            }
            if is_attribute_line(trimmed) {
                pending.push(Attachment {
                    range: line.start..line.content_end,
                    role: SourceComponentRole::Attribute,
                });
                index += 1;
                continue;
            }

            if let Some(name) = alias_name(trimmed) {
                let attachments = take_attached(self.source, &mut pending, line.start);
                let visibility = declaration_visibility(&name, &attachments, self.source);
                self.push_declaration(
                    name,
                    DeclarationKind::TypeAlias,
                    visibility,
                    &attachments,
                    line.start..line.content_end,
                    line.content_end,
                    SourceComponentRole::TypeAlias,
                )?;
                index += 1;
                continue;
            }

            if let Some(name) = recipe_name(trimmed) {
                let attachments = take_attached(self.source, &mut pending, line.start);
                let visibility = declaration_visibility(&name, &attachments, self.source);
                let (source_end, next_index) = recipe_extent(&self.lines, index);
                self.push_declaration(
                    name,
                    DeclarationKind::Function,
                    visibility,
                    &attachments,
                    line.start..line.content_end,
                    source_end,
                    SourceComponentRole::Signature,
                )?;
                index = next_index;
                continue;
            }

            if let Some(name) = variable_name(trimmed) {
                self.diagnostics.push(ProjectionDiagnostic::new(format!(
                    "omitted Just variable {name} at byte {} because declaration review currently inventories recipes and aliases",
                    line.start
                )));
            } else if trimmed.contains(':') && !is_known_directive(trimmed) {
                self.diagnostics.push(ProjectionDiagnostic::new(format!(
                    "omitted unrecognized Just declaration header at byte {}: {trimmed}",
                    line.start
                )));
            }
            pending.clear();
            index += 1;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn push_declaration(
        &mut self,
        name: String,
        kind: DeclarationKind,
        visibility: Visibility,
        attachments: &[Attachment],
        projected_item: Range<usize>,
        source_end: usize,
        item_role: SourceComponentRole,
    ) -> Result<()> {
        let components =
            declaration_components(self.source, attachments, projected_item.clone(), item_role)?;
        let projection_text = components
            .iter()
            .map(|component| component.text.as_str())
            .collect::<String>();
        let projection_hash = projection_hash(Language::Just, kind, &components);
        let source_start = attachments
            .first()
            .map_or(projected_item.start, |attachment| attachment.range.start);
        let source_ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        let id = declaration_id(
            self.path,
            kind,
            &name,
            source_ordinal,
            source_start,
            &projection_hash,
        );
        let key = declaration_key(Language::Just, kind, &name, None, "");
        self.declarations.push(DeclarationNode {
            id: id.clone(),
            key,
            name,
            kind,
            visibility,
            parent_part: None,
            source_ordinal,
            source_span: source_start..source_end,
            components,
            projection_text,
            projection_hash,
            review_owner: id,
            children: Vec::<DeclarationId>::new(),
            type_use_sites: Vec::new(),
        });
        Ok(())
    }
}

fn source_lines(source: &str) -> Vec<SourceLine<'_>> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for segment in source.split_inclusive('\n') {
        let mut content_end = start + segment.len();
        if segment.ends_with('\n') {
            content_end -= 1;
            if source.as_bytes().get(content_end.wrapping_sub(1)) == Some(&b'\r') {
                content_end -= 1;
            }
        }
        lines.push(SourceLine {
            start,
            content_end,
            text: &source[start..content_end],
        });
        start += segment.len();
    }
    if start < source.len() {
        lines.push(SourceLine {
            start,
            content_end: source.len(),
            text: &source[start..],
        });
    }
    lines
}

fn is_attribute_line(line: &str) -> bool {
    line.starts_with('[') && line.ends_with(']')
}

fn alias_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix("alias")?;
    if !rest.as_bytes().first().is_some_and(u8::is_ascii_whitespace) {
        return None;
    }
    let (name, target) = rest.trim_start().split_once(":=")?;
    let name = name.trim();
    if !valid_name(name) || target.trim().is_empty() {
        return None;
    }
    Some(name.to_owned())
}

fn recipe_name(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let name_start = usize::from(bytes.first() == Some(&b'@'));
    let name_end = bytes[name_start..]
        .iter()
        .position(|byte| !is_name_byte(*byte))
        .map_or(bytes.len(), |offset| name_start + offset);
    if name_end == name_start {
        return None;
    }
    let name = line.get(name_start..name_end)?;
    let separator = recipe_separator(line, name_end)?;
    (separator > name_end || line.as_bytes().get(separator) == Some(&b':')).then(|| name.to_owned())
}

fn recipe_separator(line: &str, start: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut quote = None;
    let mut escaped = false;
    for (offset, byte) in bytes.get(start..)?.iter().copied().enumerate() {
        let index = start + offset;
        if escaped {
            escaped = false;
            continue;
        }
        if byte == b'\\' && quote.is_some() {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if byte == active {
                quote = None;
            }
            continue;
        }
        if matches!(byte, b'\'' | b'"' | b'`') {
            quote = Some(byte);
            continue;
        }
        if byte == b':' && bytes.get(index + 1) != Some(&b'=') {
            return Some(index);
        }
    }
    None
}

fn variable_name(line: &str) -> Option<String> {
    let line = line.strip_prefix("export ").unwrap_or(line);
    for operator in [":=", "+=", "?=", "="] {
        let Some((left, right)) = line.split_once(operator) else {
            continue;
        };
        let name = left.trim();
        if valid_name(name) && !right.trim().is_empty() {
            return Some(name.to_owned());
        }
    }
    None
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(is_name_byte)
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn is_known_directive(line: &str) -> bool {
    ["set ", "import ", "mod ", "unexport "]
        .iter()
        .any(|prefix| line.starts_with(prefix))
}

fn declaration_visibility(name: &str, attachments: &[Attachment], source: &str) -> Visibility {
    let explicitly_private = attachments.iter().any(|attachment| {
        attachment.role == SourceComponentRole::Attribute
            && source
                .get(attachment.range.clone())
                .is_some_and(|text| text.contains("private"))
    });
    if name.starts_with('_') || explicitly_private {
        Visibility::Private
    } else {
        Visibility::Public
    }
}

fn recipe_extent(lines: &[SourceLine<'_>], header_index: usize) -> (usize, usize) {
    let header = lines[header_index];
    let mut source_end = header.content_end;
    let mut index = header_index + 1;
    while let Some(line) = lines.get(index) {
        if line.text.trim().is_empty() {
            index += 1;
            continue;
        }
        if line
            .text
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_whitespace)
        {
            source_end = line.content_end;
            index += 1;
            continue;
        }
        break;
    }
    (source_end, index)
}

fn take_attached(
    source: &str,
    pending: &mut Vec<Attachment>,
    item_start: usize,
) -> Vec<Attachment> {
    let attached = pending.last().is_some_and(|attachment| {
        source
            .get(attachment.range.end..item_start)
            .is_some_and(|gap| {
                gap.bytes().all(|byte| byte.is_ascii_whitespace())
                    && gap.bytes().filter(|byte| *byte == b'\n').count() <= 1
            })
    });
    if attached {
        std::mem::take(pending)
    } else {
        pending.clear();
        Vec::new()
    }
}

fn declaration_components(
    source: &str,
    attachments: &[Attachment],
    item: Range<usize>,
    item_role: SourceComponentRole,
) -> Result<Vec<SourceComponent>> {
    let mut components = Vec::with_capacity(attachments.len().saturating_mul(2) + 2);
    let mut cursor = attachments
        .first()
        .map_or(item.start, |attachment| attachment.range.start);
    for attachment in attachments {
        push_component(
            &mut components,
            source,
            cursor..attachment.range.start,
            SourceComponentRole::Layout,
        )?;
        push_component(
            &mut components,
            source,
            attachment.range.clone(),
            attachment.role,
        )?;
        cursor = attachment.range.end;
    }
    push_component(
        &mut components,
        source,
        cursor..item.start,
        SourceComponentRole::Layout,
    )?;
    push_component(&mut components, source, item, item_role)?;
    Ok(components)
}

fn push_component(
    components: &mut Vec<SourceComponent>,
    source: &str,
    range: Range<usize>,
    role: SourceComponentRole,
) -> Result<()> {
    if range.is_empty() {
        return Ok(());
    }
    let text = source
        .get(range.clone())
        .context("Just declaration component was not on UTF-8 boundaries")?;
    components.push(SourceComponent {
        role,
        source_range: range,
        text: text.to_owned(),
    });
    Ok(())
}
