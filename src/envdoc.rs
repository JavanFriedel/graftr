use crate::validation::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_BYTES: usize = 4 * 1024 * 1024;
pub struct Assignment {
    pub raw: String,
    pub value: String,
}
fn invalid() -> Error {
    Error {code: "ENV_INVALID_DOCUMENT", message: "Environment document has unsupported, malformed, or ambiguous dotenv syntax; no contents were logged. Review it privately before retrying.".into()}
}
fn line_end(bytes: &[u8], offset: &mut usize) {
    while *offset < bytes.len() && !b"\r\n".contains(&bytes[*offset]) {
        *offset += 1;
    }
}
fn spaces(bytes: &[u8], offset: &mut usize) {
    while bytes.get(*offset).is_some_and(|b| b" \t".contains(b)) {
        *offset += 1;
    }
}
// Dotenv's tolerant reader is used only to observe public assignments or verify
// generated managed values. Refresh planning retains the stricter lexer below.
pub fn loose(bytes: &[u8]) -> BTreeMap<String, String> {
    let text = String::from_utf8_lossy(bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let text = text.trim_start_matches('\u{feff}');
    let bytes = text.as_bytes();
    let mut values = BTreeMap::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let captures =
            pattern!(r"^[ \t]*(?:export[ \t]+)?([A-Za-z_][A-Za-z0-9_]*)(?:[ \t]*=[ \t]*|:[ \t]+)")
                .captures(&text[offset..]);
        if let Some(captures) = captures {
            let key = captures[1].to_owned();
            offset += captures.get(0).unwrap().len();
            let start = offset;
            let quote = bytes.get(offset).copied().filter(|b| b"'\"`".contains(b));
            let end = quote.and_then(|quote| {
                let mut i = offset + 1;
                while i < bytes.len() && bytes[i] != quote {
                    if bytes[i] == b'\\' && bytes.get(i + 1) == Some(&quote) {
                        i += 1;
                    }
                    i += 1;
                }
                (i < bytes.len()).then_some(i)
            });
            let value = if let Some(end) = end {
                let value = &text[start + 1..end];
                offset = end + 1;
                if quote == Some(b'"') {
                    value.replace("\\n", "\n").replace("\\r", "\r")
                } else {
                    value.into()
                }
            } else {
                line_end(bytes, &mut offset);
                text[start..offset]
                    .split('#')
                    .next()
                    .unwrap()
                    .trim_matches([' ', '\t'])
                    .into()
            };
            values.insert(key, value);
        }
        line_end(bytes, &mut offset);
        if offset < bytes.len() {
            offset += 1;
        }
    }
    values
}
pub fn assignments(bytes: &[u8]) -> Result<BTreeMap<String, Assignment>> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    if bytes.len() > MAX_BYTES
        || bytes
            .iter()
            .any(|b| matches!(b, 0..=8 | 11 | 12 | 14..=31 | 127))
    {
        return Err(invalid());
    }
    let mut entries = BTreeMap::new();
    let mut offset = if text.starts_with('\u{feff}') { 3 } else { 0 };
    while offset < bytes.len() {
        let start = offset;
        spaces(bytes, &mut offset);
        if offset == bytes.len() {
            break;
        }
        if bytes[offset] == b'#' {
            line_end(bytes, &mut offset);
        } else if !b"\r\n".contains(&bytes[offset]) {
            let captures =
                pattern!(r"^(?:export[ \t]+)?([A-Za-z_][A-Za-z0-9_]*)(?:[ \t]*=[ \t]*|:[ \t]+)")
                    .captures(&text[offset..])
                    .ok_or_else(invalid)?;
            let key = captures[1].to_owned();
            offset += captures.get(0).unwrap().len();
            let value;
            if bytes.get(offset).is_some_and(|b| b"'\"`".contains(b)) {
                let quote = bytes[offset];
                offset += 1;
                let value_start = offset;
                while offset < bytes.len() && bytes[offset] != quote {
                    if bytes[offset] == b'\\' && bytes.get(offset + 1) == Some(&quote) {
                        offset += 1;
                    }
                    offset += 1;
                }
                if offset == bytes.len() {
                    return Err(invalid());
                }
                let mut parsed = text[value_start..offset]
                    .replace("\r\n", "\n")
                    .replace('\r', "\n");
                if quote == b'"' {
                    parsed = parsed.replace("\\n", "\n").replace("\\r", "\r");
                }
                value = parsed;
                offset += 1;
                spaces(bytes, &mut offset);
                if bytes.get(offset).is_some_and(|b| !b"#\r\n".contains(b)) {
                    return Err(invalid());
                }
                line_end(bytes, &mut offset);
            } else {
                let value_start = offset;
                line_end(bytes, &mut offset);
                value = text[value_start..offset]
                    .split('#')
                    .next()
                    .unwrap()
                    .trim_matches([' ', '\t'])
                    .to_owned();
            }
            entries.insert(
                key,
                Assignment {
                    raw: text[start..offset].into(),
                    value,
                },
            );
        }
        if bytes.get(offset) == Some(&b'\r') {
            offset += 1;
        }
        if bytes.get(offset) == Some(&b'\n') {
            offset += 1;
        }
    }
    Ok(entries)
}
pub struct Plan {
    pub entries: Vec<Value>,
    pub content: Vec<u8>,
}
pub fn plan(
    baseline: &[u8],
    current: &[u8],
    overrides: &Value,
    selection: &[String],
) -> Result<Plan> {
    let selected: BTreeSet<_> = selection.iter().collect();
    ensure(selected.len() == selection.len() && selection.iter().all(|key| pattern!(r"^[A-Za-z_][A-Za-z0-9_]*$").is_match(key)),
        Error {code: "ENV_INVALID_SELECTION", message: "Select unique literal environment key names; no values or shell expressions are accepted.".into()})?;
    let source = assignments(baseline)?;
    let target = assignments(current)?;
    let overrides = overrides.as_object().expect("validated overrides");
    ensure(!overrides.iter().any(|(key, value)| target.get(key).map(|a| a.value.as_str()) != value.as_str()), Error {code: "ENV_MANAGED_MISMATCH", message: "Existing managed assignments do not match this task's fixed public values; review them privately and use normal setup verification. Refresh will not change port/URL assignments.".into()})?;
    for key in selection {
        ensure(!overrides.contains_key(key) && source.contains_key(key), Error {code: "ENV_INVALID_SELECTION", message: "Selected keys must exist in the baseline and must not be managed public assignments; no values were logged.".into()})?;
    }
    let keys: BTreeSet<_> = source.keys().chain(target.keys()).collect();
    let mut entries = Vec::new();
    let mut additions = Vec::new();
    for key in keys {
        let from = source.get(key);
        let to = target.get(key);
        let status = if overrides.contains_key(key) {
            "managed"
        } else if from.is_none() {
            "local-only"
        } else {
            match to {
                None => "missing",
                Some(to) if from.unwrap().value == to.value => "same",
                Some(_) => "different",
            }
        };
        let action = if status == "missing" {
            "add"
        } else if status == "different" && selected.contains(key) {
            "replace"
        } else {
            "preserve"
        };
        entries.push(json!({"key": key, "status": status, "action": action}));
        if action != "preserve" {
            additions.push(from.unwrap().raw.as_str());
        }
    }
    let mut content = current.to_vec();
    if !additions.is_empty() {
        content.extend_from_slice(
            format!(
                "\n\n# Baseline assignments explicitly refreshed by graftr.\n{}\n",
                additions.join("\n")
            )
            .as_bytes(),
        );
    }
    ensure(content.len() <= MAX_BYTES, Error {code: "ENV_INVALID_DOCUMENT", message: "Refreshed environment would exceed the supported 4 MiB size limit; no file was changed.".into()})?;
    let result = assignments(&content)?;
    for entry in &entries {
        let key = entry["key"].as_str().unwrap();
        let expected = if entry["action"] == "preserve" {
            target.get(key)
        } else {
            source.get(key)
        };
        if result.get(key).map(|a| &a.value) != expected.map(|a| &a.value) {
            return Err(invalid());
        }
    }
    Ok(Plan { entries, content })
}
