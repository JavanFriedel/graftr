use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat};
use serde_json::{Map, Value};

pub type Object = Map<String, Value>;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
}
impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Self {
            code: "OPERATION_FAILED",
            message: message.into(),
        }
    }
}
impl From<String> for Error {
    fn from(message: String) -> Self {
        Self {
            code: "OPERATION_FAILED",
            message,
        }
    }
}

macro_rules! pattern {
    ($pattern:literal) => {{
        static REGEX: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new($pattern).expect("constant validation pattern")
        });
        &*REGEX
    }};
}
pub(crate) use pattern;

pub fn ensure(condition: bool, message: impl Into<Error>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
pub fn object(value: &Value) -> Result<&Object> {
    value
        .as_object()
        .ok_or_else(|| "Expected a JSON object".into())
}
pub fn list(value: &Value) -> Result<&Vec<Value>> {
    value
        .as_array()
        .ok_or_else(|| "Expected a JSON array".into())
}
pub fn text(value: &Value) -> Result<&str> {
    value
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Expected a nonempty string".into())
}
pub fn field<'a>(data: &'a Object, key: &str) -> Result<&'a Value> {
    data.get(key)
        .ok_or_else(|| "Missing required recorded field".into())
}
pub fn string<'a>(data: &'a Object, key: &str) -> Result<&'a str> {
    text(field(data, key)?)
}
pub fn nullable(data: &Object, key: &str) -> Result<Value> {
    let value = field(data, key)?;
    if !value.is_null() {
        text(value)?;
    }
    Ok(value.clone())
}
pub fn strings(data: &Object, keys: &[&str]) -> Result<Object> {
    keys.iter()
        .map(|key| Ok(((*key).into(), Value::String(string(data, key)?.into()))))
        .collect()
}
pub fn optional(
    data: &Object,
    target: &mut Object,
    key: &str,
    parse: impl FnOnce(&Value) -> Result<Value>,
) -> Result<()> {
    if let Some(value) = data.get(key) {
        target.insert(key.into(), parse(value)?);
    }
    Ok(())
}
pub fn choice<'a>(value: &'a Value, choices: &[&str], message: &str) -> Result<&'a str> {
    value
        .as_str()
        .filter(|s| choices.contains(s))
        .ok_or_else(|| message.into())
}
pub fn integer(value: &Value, minimum: f64, maximum: f64, message: &str) -> Result<f64> {
    value
        .as_f64()
        .filter(|v| v.is_finite() && v.fract() == 0.0 && *v >= minimum && *v <= maximum)
        .ok_or_else(|| message.into())
}
pub fn name(value: &str) -> Result<()> {
    ensure(
        pattern!(r"^[a-z0-9][a-z0-9-]{0,47}$").is_match(value) && !value.contains("--"),
        "Names require lowercase letters, numbers, and single hyphens (max 48)",
    )
}
pub fn id(value: &str) -> Result<()> {
    let parts: Vec<_> = value.split("--").collect();
    ensure(parts.len() == 2, "Task id must be PROJECT--TASK")?;
    for part in parts {
        name(part)?;
    }
    Ok(())
}
pub fn named_port(value: &str) -> bool {
    value.len() <= 32 && pattern!(r"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$").is_match(value)
}
pub fn port(value: &Value) -> Result<Value> {
    integer(value, 1024.0, 65535.0, "Invalid unprivileged port")?;
    Ok(value.clone())
}
pub fn normalized_path(value: &str) -> String {
    let absolute = value.starts_with('/');
    let mut parts = Vec::new();
    for part in value.split('/') {
        match part {
            "" | "." => (),
            ".." if parts.last().is_some_and(|p| *p != "..") => {
                parts.pop();
            }
            ".." if !absolute => parts.push(part),
            ".." => (),
            _ => parts.push(part),
        }
    }
    if absolute {
        format!("/{}", parts.join("/"))
    } else if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}
pub fn absolute_normalized(value: &str) -> bool {
    value.starts_with('/') && normalized_path(value) == value
}
pub fn project_path(value: &Value) -> Result<Value> {
    let path = text(value)?;
    ensure(
        path != "."
            && !path.ends_with('/')
            && !path.starts_with('/')
            && normalized_path(path) == path
            && !path
                .split('/')
                .any(|part| part == ".." || part.starts_with(".env"))
            && !path
                .chars()
                .any(|c| c <= '\u{1f}' || c == '\u{7f}' || "\\*?[]{}".contains(c)),
        "Install paths must be literal relative checkout paths, without traversal, globs or dotenv files",
    )?;
    Ok(value.clone())
}
pub fn url(value: &str) -> Result<url::Url> {
    let parsed = url::Url::parse(value).map_err(|_| Error::from("Invalid URL"))?;
    ensure(
        ["http", "https"].contains(&parsed.scheme())
            && parsed.username().is_empty()
            && parsed.password().is_none_or(str::is_empty)
            && parsed.query().is_none_or(str::is_empty)
            && parsed.fragment().is_none_or(str::is_empty)
            && parsed.path() == "/",
        "url must be a public HTTP(S) origin without credentials, path, query or fragment",
    )?;
    Ok(parsed)
}
pub fn date_millis(value: &str) -> Option<i64> {
    if let Ok(date) = DateTime::parse_from_rfc3339(value) {
        return Some(date.timestamp_millis());
    }
    if let Ok(date) = DateTime::parse_from_rfc2822(value) {
        return Some(date.timestamp_millis());
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(date) = NaiveDateTime::parse_from_str(value, format) {
            return Some(date.and_utc().timestamp_millis());
        }
    }
    None
}
pub fn canonical_date(value: &Value) -> Result<Value> {
    let string = text(value)?;
    let canonical = date_millis(string)
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|date| date.to_rfc3339_opts(SecondsFormat::Millis, true));
    ensure(
        canonical.as_deref() == Some(string),
        "Invalid frozen task date",
    )?;
    Ok(value.clone())
}
pub fn control(c: char) -> bool {
    c <= '\u{1f}' || ('\u{7f}'..='\u{9f}').contains(&c) || c == '\u{2028}' || c == '\u{2029}'
}
pub fn js_space(c: char) -> bool {
    matches!(c, '\u{9}'..='\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}
