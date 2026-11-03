use crate::validation::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_BYTES: usize = 4 * 1024 * 1024;
pub struct Assignment {
    pub raw: String,
    pub value: String,
}
struct Statement {
    key: String,
    value: String,
    start: usize,
    raw_end: usize,
    end: usize,
}
struct Document<'a> {
    text: &'a str,
    statements: Vec<Statement>,
}
impl Document<'_> {
    fn assignments(&self) -> BTreeMap<String, Assignment> {
        self.statements
            .iter()
            .map(|entry| {
                (
                    entry.key.clone(),
                    Assignment {
                        raw: self.text[entry.start..entry.raw_end].into(),
                        value: entry.value.clone(),
                    },
                )
            })
            .collect()
    }
    fn count(&self, key: &str) -> usize {
        self.statements
            .iter()
            .filter(|entry| entry.key == key)
            .count()
    }
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
// generated managed values. All composition uses the stricter lexer below.
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
fn document(bytes: &[u8]) -> Result<Document<'_>> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    if bytes.len() > MAX_BYTES
        || bytes
            .iter()
            .any(|b| matches!(b, 0..=8 | 11 | 12 | 14..=31 | 127))
    {
        return Err(invalid());
    }
    let mut entries = Vec::new();
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
            entries.push(Statement {
                key,
                value,
                start,
                raw_end: offset,
                end: offset,
            });
        }
        if bytes.get(offset) == Some(&b'\r') {
            offset += 1;
        }
        if bytes.get(offset) == Some(&b'\n') {
            offset += 1;
        }
        if let Some(entry) = entries.last_mut().filter(|entry| entry.start == start) {
            entry.end = offset;
        }
    }
    Ok(Document {
        text,
        statements: entries,
    })
}
pub fn assignments(bytes: &[u8]) -> Result<BTreeMap<String, Assignment>> {
    Ok(document(bytes)?.assignments())
}

// Keep raw syntax for surviving assignments; never expand interpolation or execute
// the document. Replacements occupy the first old declaration, before references
// that may follow it. Unmanaged baseline duplicates resolve to their last entry.
fn rewrite(
    input: &Document<'_>,
    replacements: &BTreeMap<String, String>,
    canonical: bool,
    heading: &str,
) -> Result<Vec<u8>> {
    let mut expected = BTreeMap::new();
    for (key, raw) in replacements {
        let parsed = document(raw.as_bytes())?;
        let entry = parsed.statements.first().ok_or_else(invalid)?;
        ensure(
            parsed.statements.len() == 1
                && entry.key == *key
                && entry.start == 0
                && entry.raw_end == raw.len(),
            invalid(),
        )?;
        expected.insert(key, entry.value.clone());
    }
    let last: BTreeMap<_, _> = input
        .statements
        .iter()
        .enumerate()
        .map(|(i, entry)| (entry.key.as_str(), i))
        .collect();
    let mut emitted = BTreeSet::new();
    let mut output = String::new();
    let mut cursor = 0;
    for (i, entry) in input.statements.iter().enumerate() {
        output.push_str(&input.text[cursor..entry.start]);
        if let Some(raw) = replacements.get(&entry.key) {
            if emitted.insert(entry.key.as_str()) {
                output.push_str(raw);
                output.push_str(&input.text[entry.raw_end..entry.end]);
            }
        } else if !canonical || last[entry.key.as_str()] == i {
            output.push_str(&input.text[entry.start..entry.end]);
        }
        cursor = entry.end;
    }
    output.push_str(&input.text[cursor..]);
    let missing: Vec<_> = replacements
        .iter()
        .filter(|(key, _)| !emitted.contains(key.as_str()))
        .collect();
    if !missing.is_empty() {
        let newline = if input.text.contains("\r\n") {
            "\r\n"
        } else if input.text.contains('\r') {
            "\r"
        } else {
            "\n"
        };
        if !output.is_empty() && !output.ends_with(['\r', '\n']) {
            output.push_str(newline);
        }
        output.push_str(heading);
        output.push_str(newline);
        for (_, raw) in missing {
            output.push_str(raw);
            output.push_str(newline);
        }
    }
    let result = document(output.as_bytes())?;
    let values = result.assignments();
    for (key, value) in expected {
        ensure(
            result.count(key) == 1 && values.get(key).is_some_and(|entry| entry.value == value),
            invalid(),
        )?;
    }
    if canonical {
        ensure(values.len() == result.statements.len(), invalid())?;
    }
    Ok(output.into_bytes())
}
pub fn compose(baseline: &[u8], overrides: &Value) -> Result<Vec<u8>> {
    let overrides = crate::environment::parse(overrides, false)?;
    let replacements = overrides
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| (key.clone(), format!("{key}={}", value.as_str().unwrap())))
        .collect();
    rewrite(
        &document(baseline)?,
        &replacements,
        true,
        "# Worktree-specific values managed by graftr.",
    )
}
pub fn replace(
    current: &[u8],
    replacements: &BTreeMap<String, String>,
    heading: &str,
) -> Result<Vec<u8>> {
    rewrite(&document(current)?, replacements, false, heading)
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
    let target_document = document(current)?;
    let target = target_document.assignments();
    let overrides = overrides.as_object().expect("validated overrides");
    ensure(!overrides.iter().any(|(key, value)| target.get(key).map(|a| a.value.as_str()) != value.as_str()), Error {code: "ENV_MANAGED_MISMATCH", message: "Existing managed assignments do not match this task's fixed public values; review them privately and use normal setup verification. Refresh will not change port/URL assignments.".into()})?;
    for key in selection {
        ensure(!overrides.contains_key(key) && source.contains_key(key), Error {code: "ENV_INVALID_SELECTION", message: "Selected keys must exist in the baseline and must not be managed public assignments; no values were logged.".into()})?;
    }
    let keys: BTreeSet<_> = source.keys().chain(target.keys()).collect();
    let mut entries = Vec::new();
    let mut replacements = BTreeMap::new();
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
        } else if selected.contains(key)
            && (status == "different" || target_document.count(key) > 1)
        {
            "replace"
        } else {
            "preserve"
        };
        entries.push(json!({"key": key, "status": status, "action": action}));
        if action != "preserve" {
            replacements.insert(key.clone(), from.unwrap().raw.clone());
        }
    }
    let content = rewrite(
        &target_document,
        &replacements,
        false,
        "# Baseline assignments explicitly refreshed by graftr.",
    )?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_composition_preserves_raw_winners_and_uses_unique_keys() {
        let input = b"\xef\xbb\xbfexport PORT = 3000\r\nPORT=3001\r\nSECRET=old\r\n# retained\r\nSECRET='literal ${PORT} # secret' # last\r\nREF=${PORT}\r\nMULTI=\"one\r\nPORT=literal\r\ntwo\"\r\n";
        let output = compose(input, &json!({"PORT": "4000"})).unwrap();
        assert_eq!(output, b"\xef\xbb\xbfPORT=4000\r\n# retained\r\nSECRET='literal ${PORT} # secret' # last\r\nREF=${PORT}\r\nMULTI=\"one\r\nPORT=literal\r\ntwo\"\r\n");
        let parsed = document(&output).unwrap();
        assert_eq!(parsed.count("PORT"), 1);
        assert_eq!(parsed.count("SECRET"), 1);
        assert_eq!(compose(&output, &json!({"PORT": "4000"})).unwrap(), output);
    }

    #[test]
    fn first_and_last_assignment_readers_agree() {
        let output = compose(
            b"PORT=3000\nexport PORT=3001\nNAME=old\nNAME=new\n",
            &json!({"PORT":"4000"}),
        )
        .unwrap();
        let parsed = document(&output).unwrap();
        let mut first = BTreeMap::new();
        let mut last = BTreeMap::new();
        for entry in &parsed.statements {
            first.entry(&entry.key).or_insert(&entry.value);
            last.insert(&entry.key, &entry.value);
        }
        assert_eq!(first, last);
        assert_eq!(last[&"PORT".to_owned()], "4000");
        assert_eq!(last[&"NAME".to_owned()], "new");
    }

    #[test]
    fn targeted_replacement_preserves_unselected_bytes_and_is_idempotent() {
        let input = b"# local\r\nexport SECRET = 'old\r\nPORT=literal' # replaced\r\nLOCAL=one\r\nLOCAL=two\r\nSECRET=shadow";
        let replacements = [("SECRET".into(), "SECRET='new' # baseline".into())].into();
        let output = replace(input, &replacements, "# missing").unwrap();
        assert_eq!(
            output,
            b"# local\r\nSECRET='new' # baseline\r\nLOCAL=one\r\nLOCAL=two\r\n"
        );
        assert_eq!(
            replace(&output, &replacements, "# missing").unwrap(),
            output
        );
    }

    #[test]
    fn missing_keys_use_stable_order_and_existing_line_endings() {
        for ending in ["\n", "\r\n", "\r"] {
            let input = format!("# comment{ending}LOCAL=literal");
            let replacements =
                [("B".into(), "B='two'".into()), ("A".into(), "A=one".into())].into();
            let output = replace(input.as_bytes(), &replacements, "# added").unwrap();
            assert_eq!(
                output,
                format!("{input}{ending}# added{ending}A=one{ending}B='two'{ending}").as_bytes()
            );
        }
        assert_eq!(compose(b"", &json!({})).unwrap(), b"");
        assert_eq!(compose(b"# comment", &json!({})).unwrap(), b"# comment");
        assert_eq!(
            compose(b"DUP=old\nDUP=new", &json!({})).unwrap(),
            b"DUP=new"
        );
    }

    #[test]
    fn invalid_documents_and_replacement_injection_are_refused() {
        for input in [
            &b"SECRET='unclosed"[..],
            b"BAD.KEY=value",
            b"KEY='value' extra",
            b"\xff",
            b"KEY=one\0two",
        ] {
            assert_eq!(
                compose(input, &json!({})).unwrap_err().code,
                "ENV_INVALID_DOCUMENT"
            );
        }
        for raw in [
            "KEY=value\nOTHER=injected",
            "OTHER=value",
            "KEY=value\n",
            "# comment\nKEY=value",
        ] {
            assert!(replace(b"KEY=old", &[("KEY".into(), raw.into())].into(), "# added").is_err());
        }
        assert!(compose(b"", &json!({"PORT":"$(touch NEVER_RUN)"})).is_err());
        assert!(compose(&vec![b' '; MAX_BYTES + 1], &json!({})).is_err());
        let mut input = b"#".to_vec();
        input.resize(MAX_BYTES, b' ');
        assert!(compose(&input, &json!({"PORT":"4000"})).is_err());
    }

    #[test]
    fn refresh_only_normalizes_explicitly_selected_duplicates() {
        let baseline = b"SECRET=old\nSECRET='current' # last\n";
        let current = b"SECRET=shadow\nSECRET=current\nLOCAL=one\nLOCAL=two\n";
        assert_eq!(
            plan(baseline, current, &json!({}), &[]).unwrap().content,
            current
        );
        let result = plan(baseline, current, &json!({}), &["SECRET".into()]).unwrap();
        assert_eq!(
            result.content,
            b"SECRET='current' # last\nLOCAL=one\nLOCAL=two\n"
        );
        assert_eq!(result.entries[1]["status"], "same");
        assert_eq!(result.entries[1]["action"], "replace");
    }
}
