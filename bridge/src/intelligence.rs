use serde::Deserialize;
use serde_json::{Value, json};

use crate::Region;
use crate::injections::FragmentKind;
use crate::source::{LineIndex, Position, TextRange, utf16_offset};

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Feature {
    Completion,
    Hover,
}

impl Feature {
    pub fn method(self) -> &'static str {
        match self {
            Self::Completion => "textDocument/completion",
            Self::Hover => "textDocument/hover",
        }
    }

    pub fn map_result(self, source: &str, region: &Region, cursor: Position, mut result: Value) -> Value {
        match self {
            Self::Completion => {
                let fallback = identifier_range(source, cursor);
                let items = match &mut result {
                    Value::Array(items) => Some(items),
                    Value::Object(list) => list.get_mut("items").and_then(Value::as_array_mut),
                    _ => None,
                };
                if let Some(items) = items {
                    items.retain_mut(|item| map_completion(item, source, region, cursor, fallback).is_some());
                }
            }
            Self::Hover => {
                if let Some(range) = result.get_mut("range") {
                    let Some(mapped) = read_range(range).and_then(|range| region.map_range(range)) else {
                        return Value::Null;
                    };
                    *range = json!(mapped);
                }
            }
        }
        result
    }
}

fn identifier_range(source: &str, cursor: Position) -> TextRange {
    let mut range = TextRange {
        start: cursor,
        end: cursor,
    };
    if let Some(line) = source.split('\n').nth(cursor.line as usize)
        && let Some(offset) = utf16_offset(line, cursor.character)
    {
        let (before, after) = line.split_at(offset);
        let prefix = before.trim_end_matches(unicode_ident::is_xid_continue);
        let end = after
            .find(|ch| !unicode_ident::is_xid_continue(ch))
            .unwrap_or(after.len());
        range.start.character = prefix.encode_utf16().count() as u32;
        range.end.character += after[..end].encode_utf16().count() as u32;
    }
    range
}

fn map_completion(
    item: &mut Value,
    source: &str,
    region: &Region,
    cursor: Position,
    fallback: TextRange,
) -> Option<()> {
    let label = item["label"].as_str()?;
    // Snippets have expansion rules beyond their literal edit text.
    if item["insertTextFormat"].as_u64() == Some(2) {
        return None;
    }

    let (replace, insert, text) = if let Some(edit) = item.get("textEdit") {
        let replace = read_range(edit.get("range").or_else(|| edit.get("replace"))?)?;
        let insert = match edit.get("insert") {
            Some(range) => Some(read_range(range)?),
            None => None,
        };
        (replace, insert, edit["newText"].as_str()?)
    } else {
        (fallback, None, item["insertText"].as_str().unwrap_or(label))
    };
    if !contains_cursor(replace, cursor)
        || insert.is_some_and(|range| {
            !contains_cursor(range, cursor) || range.start != replace.start || range.end > replace.end
        })
    {
        return None;
    }
    let mapped_replace = region.map_range(replace)?;
    let mapped_edit = if let Some(insert) = insert {
        json!({ "newText": text, "insert": region.map_range(insert)?, "replace": mapped_replace })
    } else {
        json!({ "newText": text, "range": mapped_replace })
    };

    let mut edits = vec![(replace, text)];
    let mut mapped_additional = Vec::new();
    if let Some(additional) = item.get("additionalTextEdits") {
        for edit in additional.as_array()? {
            let range = read_range(&edit["range"])?;
            let new_text = edit["newText"].as_str()?;
            if edits.iter().any(|(previous, _)| intersects(*previous, range)) {
                return None;
            }
            mapped_additional.push(json!({ "range": region.map_range(range)?, "newText": new_text }));
            edits.push((range, new_text));
        }
    }
    if let FragmentKind::Body {
        raw_hashes: Some(hashes),
    } = region.kind
    {
        let body = &source[region.content.clone()];
        if !fits_raw_string(body, region.start_line, hashes, &mut edits) {
            return None;
        }
        if let Some(insert) = insert {
            edits.iter_mut().find(|(range, _)| *range == replace)?.0 = insert;
            if !fits_raw_string(body, region.start_line, hashes, &mut edits) {
                return None;
            }
        }
    }
    item["textEdit"] = mapped_edit;
    if !mapped_additional.is_empty() {
        item["additionalTextEdits"] = json!(mapped_additional);
    }
    Some(())
}

fn read_range(value: &Value) -> Option<TextRange> {
    serde_json::from_value(value.clone()).ok()
}

fn contains_cursor(range: TextRange, cursor: Position) -> bool {
    range.start.line == range.end.line && range.start <= cursor && cursor <= range.end
}

fn intersects(first: TextRange, second: TextRange) -> bool {
    first.start == second.start || (first.start < second.end && second.start < first.end)
}

fn fits_raw_string(source: &str, start_line: u32, hashes: usize, edits: &mut [(TextRange, &str)]) -> bool {
    // Nonempty edits without quotes or hashes cannot introduce a closing delimiter.
    if edits
        .iter()
        .all(|(_, new_text)| !new_text.is_empty() && !new_text.contains(['"', '#']))
    {
        return true;
    }
    let index = LineIndex::new(source);
    let mut edited = source.to_owned();
    edits.sort_unstable_by_key(|(range, _)| std::cmp::Reverse(range.start));
    for (range, new_text) in edits {
        let relative = |position: Position| Position {
            line: position.line - start_line,
            ..position
        };
        let Some(start) = index.offset(source, relative(range.start)) else {
            return false;
        };
        let Some(end) = index.offset(source, relative(range.end)) else {
            return false;
        };
        edited.replace_range(start..end, new_text);
    }
    let closing = format!("\"{}", "#".repeat(hashes));
    !edited.contains(&closing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_insert_and_replace_and_rejects_a_raw_string_closer() {
        let source = "v.Load#";
        let mut region = Region::new(String::new(), Position { line: 5, character: 20 }, 0, source);
        region.kind = FragmentKind::Body { raw_hashes: Some(1) };
        let cursor = Position { line: 0, character: 4 };
        let start = Position { line: 0, character: 2 };
        let insert = TextRange { start, end: cursor };
        let replace = TextRange {
            start,
            end: Position { line: 0, character: 6 },
        };
        let extra = TextRange {
            start: Position { line: 0, character: 0 },
            end: Position { line: 0, character: 0 },
        };
        let invalid = TextRange {
            start,
            end: Position { line: 0, character: 8 },
        };
        let result = Feature::Completion.map_result(
            source,
            &region,
            cursor,
            json!({
                "isIncomplete": true,
                "items": [
                    { "label": "Load", "textEdit": { "newText": "Load", "insert": insert, "replace": replace },
                        "additionalTextEdits": [{ "range": extra, "newText": "prefix " }] },
                    { "label": "Closes the Rust literal", "textEdit": { "newText": "\"", "range": replace } },
                    { "label": "Out of bounds", "textEdit": { "newText": "Load", "range": invalid } }
                ],
            }),
        );
        assert_eq!(result["isIncomplete"], true);
        assert_eq!(result["items"].as_array().unwrap().len(), 1);
        let item = &result["items"][0];
        assert_eq!(
            item["textEdit"]["insert"]["start"],
            json!({ "line": 5, "character": 22 })
        );
        assert_eq!(item["textEdit"]["insert"]["end"], json!({ "line": 5, "character": 24 }));
        assert_eq!(
            item["textEdit"]["replace"]["end"],
            json!({ "line": 5, "character": 26 })
        );
        assert_eq!(
            item["additionalTextEdits"][0]["range"]["start"],
            json!({ "line": 5, "character": 20 })
        );
    }
}
