use std::ops::Range;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextRange {
    pub start: Position,
    pub end: Position,
}

pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        Self {
            starts: std::iter::once(0)
                .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
                .collect(),
        }
    }

    pub fn position(&self, text: &str, offset: usize) -> Position {
        let line = self.starts.partition_point(|&start| start <= offset) - 1;
        Position {
            line: line as u32,
            character: text[self.starts[line]..offset].encode_utf16().count() as u32,
        }
    }

    pub fn offset(&self, text: &str, position: Position) -> Option<usize> {
        let line = position.line as usize;
        let start = *self.starts.get(line)?;
        let end = self.starts.get(line + 1).map_or(text.len(), |next| next - 1);
        let content = &text[start..end];
        let content = content.strip_suffix('\r').unwrap_or(content);
        Some(start + utf16_offset(content, position.character)?)
    }
}

pub fn utf16_offset(text: &str, mut column: u32) -> Option<usize> {
    for (offset, ch) in text.char_indices() {
        if column == 0 {
            return Some(offset);
        }
        column = column.checked_sub(ch.len_utf16() as u32)?;
    }
    (column == 0).then_some(text.len())
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Inline,
    Concat,
    Bang,
    Open,
    Close,
    Raw(Range<usize>),
    Other,
}

struct Token {
    kind: Kind,
    start: usize,
    end: usize,
}

pub struct InlineBody {
    pub span: Range<usize>,
    pub raw_hashes: Option<usize>,
    pub imports: Option<Range<usize>>,
}

/// Scan the editor buffer even when other parts of the Rust file have syntax errors.
pub fn inline_bodies(text: &str) -> Vec<InlineBody> {
    let tokens = tokens(text);
    let mut bodies = Vec::new();
    let mut index = 0;
    while let Some([name, bang, open]) = tokens.get(index..index + 3) {
        let open_index = index + 2;
        index += 1;
        if name.kind != Kind::Inline
            || bang.kind != Kind::Bang
            || !matches!(&text[open.start..open.end], "{" | "(" | "[")
        {
            continue;
        }

        let Some(close) = matching_delimiter(text, &tokens, open_index) else {
            break;
        };

        let contents = &tokens[open_index + 1..close];
        let mut shader = contents;
        let mut shader_start = open.end;
        let mut imports = None;
        if let [keyword, header, remainder @ ..] = contents
            && &text[keyword.start..keyword.end] == "include"
            && header.kind == Kind::Open
        {
            // The injection list contains Rust paths separated by commas.
            let Some(end) = remainder.iter().position(|token| token.kind == Kind::Close) else {
                index = close + 1;
                continue;
            };
            imports = Some(header.end..remainder[end].start);
            shader_start = remainder[end].end;
            shader = &remainder[end + 1..];
        }
        if let Some((last, prefix)) = shader.split_last()
            && &text[last.start..last.end] == ","
        {
            shader = prefix;
        }
        if let [
            Token {
                kind: Kind::Raw(contents),
                end,
                ..
            },
        ] = shader
            && is_raw_expression(&text[shader_start..tokens[close].start])
        {
            bodies.push(InlineBody {
                span: contents.clone(),
                raw_hashes: Some(end - contents.end - 1),
                imports,
            });
        } else {
            bodies.push(InlineBody {
                span: shader_start..tokens[close].start,
                raw_hashes: None,
                imports,
            });
        }
        index = close + 1;
    }
    bodies
}

pub fn concat_arguments(text: &str) -> Vec<Range<usize>> {
    let tokens = tokens(text);
    let mut calls = Vec::new();
    for (index, window) in tokens.windows(3).enumerate() {
        let [name, bang, open] = window else { continue };
        if name.kind != Kind::Concat || bang.kind != Kind::Bang {
            continue;
        }
        if let Some(close) = matching_delimiter(text, &tokens, index + 2) {
            calls.push(open.end..tokens[close].start);
        }
    }
    calls
}

fn matching_delimiter(text: &str, tokens: &[Token], open: usize) -> Option<usize> {
    let opener = &text[tokens[open].start..tokens[open].end];
    let closer = match opener {
        "(" => ")",
        "{" => "}",
        "[" => "]",
        _ => return None,
    };
    let mut depth = 1;
    (open + 1..tokens.len()).find(|&index| {
        let token = &tokens[index];
        let spelling = &text[token.start..token.end];
        if spelling == opener {
            depth += 1;
        } else if spelling == closer {
            depth -= 1;
        }
        depth == 0
    })
}

fn is_raw_expression(text: &str) -> bool {
    use syn::parse::Parser;
    syn::punctuated::Punctuated::<syn::LitStr, syn::Token![,]>::parse_terminated
        .parse_str(text)
        .is_ok()
}

fn tokens(text: &str) -> Vec<Token> {
    let mut result = Vec::new();
    let mut cursor = 0;
    while let Some(ch) = text[cursor..].chars().next() {
        let start = cursor;
        let rest = &text[start..];
        cursor += ch.len_utf8();
        if ch.is_whitespace() {
            continue;
        }
        if rest.starts_with("//") {
            cursor = start + rest.find('\n').unwrap_or(rest.len());
            continue;
        }
        if rest.starts_with("/*") {
            cursor = skip_block_comment(text, start);
            continue;
        }

        let kind = if let Some((end, body)) = raw_string(text, start) {
            cursor = end;
            body.map_or(Kind::Other, Kind::Raw)
        } else {
            match ch {
                '!' => Kind::Bang,
                '{' => Kind::Open,
                '}' => Kind::Close,
                '"' => {
                    cursor = quoted_end(text, start, b'"');
                    Kind::Other
                }
                '\'' => {
                    cursor = char_literal_end(text, start).unwrap_or(cursor);
                    Kind::Other
                }
                ch if ch == '_' || unicode_ident::is_xid_start(ch) => {
                    let ident = rest.strip_prefix("r#").unwrap_or(rest);
                    let end = ident
                        .find(|ch| !unicode_ident::is_xid_continue(ch))
                        .unwrap_or(ident.len());
                    cursor = text.len() - ident.len() + end;
                    match &ident[..end] {
                        "slang_inline" => Kind::Inline,
                        "slang_concat" => Kind::Concat,
                        _ => Kind::Other,
                    }
                }
                _ => Kind::Other,
            }
        };
        result.push(Token {
            kind,
            start,
            end: cursor,
        });
    }
    result
}

fn skip_block_comment(text: &str, start: usize) -> usize {
    let mut cursor = start + 2;
    let mut depth = 1;
    let bytes = text.as_bytes();
    while cursor + 1 < bytes.len() {
        match &bytes[cursor..cursor + 2] {
            b"/*" => {
                depth += 1;
                cursor += 2;
            }
            b"*/" => {
                depth -= 1;
                cursor += 2;
                if depth == 0 {
                    return cursor;
                }
            }
            _ => cursor += 1,
        }
    }
    text.len()
}

pub fn raw_string(text: &str, start: usize) -> Option<(usize, Option<Range<usize>>)> {
    let rest = &text[start..];
    let prefix = ["r", "br", "cr"].into_iter().find(|p| rest.starts_with(p))?;
    let after_prefix = &rest[prefix.len()..];
    let hashes = after_prefix.bytes().take_while(|b| *b == b'#').count();
    let tail = after_prefix[hashes..].strip_prefix('"')?;
    let body_start = text.len() - tail.len();
    let suffix = &after_prefix[..hashes];
    for (quote, _) in tail.match_indices('"') {
        if tail[quote + 1..].starts_with(suffix) {
            let body_end = body_start + quote;
            let body = (prefix == "r").then_some(body_start..body_end);
            return Some((body_end + 1 + hashes, body));
        }
    }
    Some((text.len(), None))
}

fn quoted_end(text: &str, start: usize, quote: u8) -> usize {
    let mut cursor = start + 1;
    let bytes = text.as_bytes();
    while let Some(&byte) = bytes.get(cursor) {
        match byte {
            b'\\' => cursor += 2,
            byte if byte == quote => return cursor + 1,
            _ => cursor += 1,
        }
    }
    text.len()
}

fn char_literal_end(text: &str, start: usize) -> Option<usize> {
    let after = &text[start + 1..];
    let first = after.chars().next()?;
    if first == '\\' {
        let line_end = after.find('\n').map_or(text.len(), |end| start + 1 + end);
        let end = quoted_end(&text[..line_end], start, b'\'');
        return (end > start + 2 && text.as_bytes()[end - 1] == b'\'').then_some(end);
    }
    let closing = start + 1 + first.len_utf8();
    (text.as_bytes().get(closing) == Some(&b'\'')).then_some(closing + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_bare_and_raw_sources_verbatim() {
        let text = "a::slang_inline! {\r\n float f() { return 1; }\r\n} \
                    slang_inline! { r##\"#if 1\n{\" }\n#endif\"## }";
        let bodies: Vec<_> = inline_bodies(text).into_iter().map(|body| &text[body.span]).collect();
        assert_eq!(bodies, ["\r\n float f() { return 1; }\r\n", "#if 1\n{\" }\n#endif"]);
    }

    #[test]
    fn ignores_comments_strings_and_characters() {
        let text = r###"
// slang_inline! { fake }
/* /* fake nested */ slang_inline! { fake } */
const STR: &str = r##"slang_inline! { fake }"##;
const OTHER: &str = "escaped \" slang_inline! { fake }";
fn f<'a>() { slang_inline! { int a = '}'; } }
slang_inline! { unfinished
"###;
        let bodies: Vec<_> = inline_bodies(text).into_iter().map(|body| &text[body.span]).collect();
        assert_eq!(bodies, [" int a = '}'; "]);
    }

    #[test]
    fn counts_utf16_columns_with_crlf() {
        let text = "line\r\n/* 🦀 */ slang_inline! {body}";
        let start = inline_bodies(text)[0].span.start;
        let point = LineIndex::new(text).position(text, start);
        assert_eq!(point, Position { line: 1, character: 24 });
    }
}
