use crate::validation::{pattern, *};
use serde_json::{Value, json};

fn key(key: &str) -> Result<()> {
    ensure(
        pattern!(r"^[A-Za-z_][A-Za-z0-9_]{0,127}$").is_match(key),
        "Invalid environment variable name in envOverrides",
    )?;
    let upper = key.to_ascii_uppercase();
    ensure(
        ![
            "PATH",
            "HOME",
            "SHELL",
            "ENV",
            "BASH_ENV",
            "IFS",
            "CDPATH",
            "TMPDIR",
            "TMP",
            "TEMP",
            "NODE_OPTIONS",
            "NODE_PATH",
            "PYTHONPATH",
            "PYTHONHOME",
            "RUBYOPT",
            "PERL5OPT",
            "PERL5LIB",
            "ZDOTDIR",
            "__PROTO__",
            "CONSTRUCTOR",
            "PROTOTYPE",
        ]
        .contains(&upper.as_str())
            && !pattern!(r"^(?:HERDR|XDG|GIT|LD|DYLD|SSH|BUN|DOTENV)_").is_match(&upper),
        format!("Process-control variable {key} cannot be managed by envOverrides"),
    )?;
    ensure(
        !pattern!(r"(?:^|_)(?:SECRET|PASSWORD|PASSWD|TOKEN|KEY|CREDENTIALS?)(?:$|_)")
            .is_match(&upper),
        format!(
            "Keep secret-bearing variable {key} in the local environment baseline, not envOverrides"
        ),
    )
}
fn value(key: &str, value: &str) -> Result<()> {
    ensure(
        value == value.trim_matches(js_space)
            && !value.chars().any(|c| control(c) || "\"'$`\\#".contains(c)),
        format!("Unsafe public environment assignment for {key}; values are not displayed"),
    )
}
fn expand(template: &str, key: &str, lookup: impl Fn(&str) -> Result<String>) -> Result<String> {
    let mut output = String::new();
    let mut previous = 0;
    for captures in pattern!(r"\{\{([^{}]*)\}\}").captures_iter(template) {
        let matched = captures.get(0).unwrap();
        ensure(
            matched
                .start()
                .checked_sub(1)
                .and_then(|i| template.as_bytes().get(i))
                != Some(&b'{')
                && template.as_bytes().get(matched.end()) != Some(&b'}'),
            format!("Malformed placeholder in envOverrides.{key}"),
        )?;
        let token = captures[1].trim_matches(js_space);
        ensure(
            [
                "localPort",
                "httpsPort",
                "url",
                "host",
                "worktreeName",
                "worktreePath",
                "branch",
                "project",
                "taskSlug",
            ]
            .contains(&token)
                || token.strip_prefix("port.").is_some_and(named_port),
            format!(
                "Unknown placeholder in envOverrides.{key}; use a documented worktree placeholder"
            ),
        )?;
        output.push_str(&template[previous..matched.start()]);
        output.push_str(&lookup(token)?);
        previous = matched.end();
    }
    output.push_str(&template[previous..]);
    ensure(
        !output.contains("{{") && !output.contains("}}"),
        format!("Malformed placeholder in envOverrides.{key}"),
    )?;
    Ok(output)
}
pub fn parse(value_: &Value, templates: bool) -> Result<Value> {
    let data = value_.as_object().ok_or_else(|| {
        Error::from("envOverrides must be an object of non-secret string assignments")
    })?;
    ensure(
        data.len() <= 64,
        "envOverrides supports at most 64 assignments",
    )?;
    for (name, item) in data {
        key(name)?;
        let item = item
            .as_str()
            .filter(|s| s.encode_utf16().count() <= 4096)
            .ok_or_else(|| {
                Error::from(format!(
                    "Environment assignment for {name} must be a string of at most 4096 characters"
                ))
            })?;
        value(name, item)?;
        if templates {
            value(name, &expand(item, name, |_| Ok("placeholder".into()))?)?;
        }
    }
    Ok(value_.clone())
}
pub fn validate_task(task: &Value) -> Result<()> {
    let templates = parse(&task["envOverrides"], true)?;
    let assignments = parse(&task["overrides"], false)?;
    let templates = object(&templates)?;
    let assignments = object(&assignments)?;
    ensure(
        templates.len() == assignments.len()
            && templates.keys().all(|key| assignments.contains_key(key)),
        "Resolved environment keys differ from the declared envOverrides",
    )?;
    let resolved = resolve(&templates.clone().into(), task)?;
    ensure(
        object(&resolved)? == assignments,
        "Mismatched task environment assignments for declared envOverrides",
    )
}
pub fn resolve(templates: &Value, task: &Value) -> Result<Value> {
    let templates = parse(templates, true)?;
    let templates = object(&templates)?;
    let path = task["path"].as_str().unwrap();
    let basename = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let slug = task_slug(task);
    let mut resolved = Object::new();
    for (key, template) in templates {
        let expanded = expand(template.as_str().unwrap(), key, |token| {
            let value = match token {
                "localPort" | "httpsPort" | "url" | "host" | "branch" | "project" => {
                    task.get(token).cloned()
                }
                "worktreeName" => Some(json!(basename)),
                "worktreePath" => Some(json!(path)),
                "taskSlug" => Some(json!(slug)),
                _ => token
                    .strip_prefix("port.")
                    .and_then(|name| task.get("ports").and_then(|ports| ports.get(name)))
                    .cloned(),
            };
            match value {
                Some(Value::String(s)) => Ok(s), Some(Value::Number(number)) => Ok(number.as_f64().unwrap().to_string()),
                _ => Err(format!("Placeholder {token} is unavailable for this task; configure its address or remove that assignment").into()),
            }
        })?;
        value(key, &expanded)?;
        resolved.insert(key.clone(), json!(expanded));
    }
    parse(&Value::Object(resolved), false)
}
pub fn task_slug(task: &Value) -> String {
    let path = task["path"].as_str().unwrap();
    let basename = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    pattern!(r"[^a-z0-9]+")
        .replace_all(
            &format!("{}_{basename}", task["project"].as_str().unwrap()).to_lowercase(),
            "_",
        )
        .trim_matches('_')
        .chars()
        .take(48)
        .collect::<String>()
        .trim_end_matches('_')
        .to_owned()
}
