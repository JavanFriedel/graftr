use crate::{
    config, environment,
    paths::Paths,
    validation::{pattern, *},
};
use serde_json::{Value, json};
use std::collections::HashSet;

#[derive(Clone)]
pub struct Profile(Value);
impl Profile {
    pub fn value(&self) -> &Value {
        &self.0
    }
}

pub fn command(value: &Value, label: &str) -> Result<Value> {
    let args = list(value)?;
    for arg in args {
        text(arg)?;
    }
    ensure(
        !args.is_empty()
            && args.len() <= 64
            && !args.iter().any(|arg| {
                arg.as_str()
                    .unwrap()
                    .chars()
                    .any(|c| c <= '\u{1f}' || c == '\u{7f}')
            }),
        format!("{label} must be a nonempty argv array without control characters"),
    )?;
    Ok(value.clone())
}
pub fn hook_commands(value: &Value, cleanup: bool) -> Result<Value> {
    let entries = list(value)?;
    ensure(
        entries.len() <= 16,
        "hooks supports at most 16 hooks per phase",
    )?;
    let mut names = HashSet::new();
    let hooks: Result<Vec<_>> = entries.iter().map(|value| {
        let hook = object(value)?;
        ensure(hook.keys().all(|key| ["name", "command", "timeoutMs"].contains(&key.as_str())), "Setup hook has unsupported fields")?;
        let hook_name = string(hook, "name")?; name(hook_name)?;
        ensure(names.insert(hook_name), "Setup hook names must be unique within their phase")?;
        let timeout = hook.get("timeoutMs").cloned().unwrap_or(json!(60000));
        integer(&timeout, 100.0, 600000.0, "Setup hook timeoutMs must be between 100 and 600000 milliseconds")?;
        let args = command(field(hook, "command")?, "Hook command")?;
        let executable = args[0].as_str().unwrap();
        ensure(!cleanup || !executable.contains('/') || executable.starts_with('/'), "Cleanup executables must be absolute or resolved through PATH, not relative to the checkout")?;
        Ok(json!({"name": hook_name, "command": args, "timeoutMs": timeout}))
    }).collect();
    Ok(json!(hooks?))
}
fn port_range(value: &Value) -> Result<Value> {
    let values = list(value)?;
    port(values.first().unwrap_or(&Value::Null))?;
    ensure(values.len() == 2, "Invalid half-open port range")?;
    let end = integer(&values[1], 0.0, 65536.0, "Invalid half-open port range")?;
    ensure(
        end > values[0].as_f64().unwrap(),
        "Invalid half-open port range",
    )?;
    Ok(value.clone())
}
fn paths(value: &Value) -> Result<Value> {
    let paths: Result<Vec<_>> = list(value)?.iter().map(project_path).collect();
    let paths = paths?;
    ensure(
        paths.len() <= 256 && paths.iter().collect::<HashSet<_>>().len() == paths.len(),
        "Install paths must be unique and contain at most 256 entries",
    )?;
    Ok(json!(paths))
}
fn template(value: &Value, field_name: &str) -> Result<Value> {
    let creation = field_name != "workspaceLabel";
    let invalid = |message: String| Error {
        code: if creation {
            "INVALID_CREATION_CONFIG"
        } else {
            "OPERATION_FAILED"
        },
        message,
    };
    if !creation && !value.is_string() {
        return Err("workspaceLabel must be a string".into());
    }
    let template = value.as_str().ok_or_else(|| invalid(format!("{field_name} must be a nonempty string of at most 4096 characters without control characters or leading/trailing whitespace")))?;
    ensure(
        !template.is_empty()
            && template.encode_utf16().count() <= 4096
            && template == template.trim_matches(js_space)
            && !template.chars().any(control),
        invalid(format!(
            "{field_name} must be a nonempty string of at most 4096 characters without control characters or leading/trailing whitespace"
        )),
    )?;
    let fields: &[&str] = match field_name {
        "branchName" => &["project", "task", "repoName"],
        "worktreePath" => &["project", "task", "repoName", "branch", "branchSlug"],
        _ => &[
            "localPort",
            "httpsPort",
            "url",
            "host",
            "worktreeName",
            "worktreePath",
            "branch",
            "project",
            "taskName",
        ],
    };
    let mut expanded = String::new();
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
            invalid(format!("Malformed placeholder in {field_name}")),
        )?;
        let name = captures[1].trim_matches(js_space);
        ensure(
            fields.contains(&name)
                || !creation && name.strip_prefix("port.").is_some_and(named_port),
            invalid(format!(
                "Unknown placeholder in {field_name}; use a documented {} placeholder",
                if creation { "creation" } else { "worktree" }
            )),
        )?;
        expanded.push_str(&template[previous..matched.start()]);
        expanded.push_str("placeholder");
        previous = matched.end();
    }
    expanded.push_str(&template[previous..]);
    ensure(
        !expanded.contains("{{") && !expanded.contains("}}"),
        invalid(format!("Malformed placeholder in {field_name}")),
    )?;
    if field_name == "worktreePath" {
        ensure(!expanded.is_empty() && expanded == expanded.trim_matches(js_space) && !expanded.starts_with('/') && normalized_path(&expanded) == expanded
            && !expanded.chars().any(|c| control(c) || c == '\\')
            && !expanded.split('/').any(|part| part.is_empty() || [".", ".."].contains(&part) || part.eq_ignore_ascii_case(".git") || part.len() > 255),
            invalid("worktreePath must be a normalized relative checkout path without traversal, Git metadata, backslashes, control characters, or oversized path components".into()))?;
    }
    Ok(value.clone())
}
pub fn label_template(value: &Value) -> Result<Value> {
    template(value, "workspaceLabel")
}
pub fn creation_template(value: &Value, field: &str) -> Result<Value> {
    template(value, field)
}
pub fn label_shape(value: &str) -> Result<()> {
    ensure(
        !value.is_empty()
            && value.encode_utf16().count() <= 4096
            && value == value.trim_matches(js_space)
            && !value.chars().any(control),
        "workspaceLabel must be a nonempty string of at most 4096 characters without control characters or leading/trailing whitespace",
    )
}
fn health_checks(value: &Value) -> Result<Value> {
    let mut names = HashSet::new();
    let checks: Result<Vec<_>> = list(value)?.iter().map(|value| {
        let check = object(value)?; let check_name = string(check, "name")?; name(check_name)?;
        let timeout = check.get("timeoutMs").cloned().unwrap_or(json!(2000));
        integer(&timeout, 100.0, 30000.0, "Health timeouts must be between 100 and 30000 milliseconds")?;
        let kind = field(check, "type")?.as_str().unwrap_or("");
        ensure(check.keys().all(|key| ["name", "type", "timeoutMs"].contains(&key.as_str()) || kind == "http" && ["path", "expectedStatus"].contains(&key.as_str()) || kind == "command" && key == "command"), "Health check has unsupported fields")?;
        let mut result = json!({"name": check_name, "type": kind, "timeoutMs": timeout});
        match kind {
            "tcp" => (), "command" => { result["command"] = command(field(check, "command")?, "Health command")?; },
            "http" => {
                let path = string(check, "path")?;
                ensure(path.starts_with('/') && !path.starts_with("//") && path.bytes().all(|b| (0x21..=0x7e).contains(&b)) && !path.contains('\\') && !path.contains('#'), "HTTP health path must be an ASCII local absolute request path without a fragment; percent-encode other characters")?;
                let expected = check.get("expectedStatus").cloned().unwrap_or(json!(200));
                integer(&expected, 100.0, 599.0, "Invalid expected HTTP health status")?;
                result["path"] = json!(path); result["expectedStatus"] = expected;
            },
            _ => return Err("Health check type must be http, tcp or command".into()),
        }
        names.insert(check_name); Ok(result)
    }).collect();
    let checks = checks?;
    ensure(
        checks.len() <= 16 && names.len() == checks.len(),
        "Health checks must have unique names and contain at most 16 entries",
    )?;
    Ok(json!(checks))
}
pub fn parse(value: &Value) -> Result<Profile> {
    let data = object(value)?;
    let retired: Vec<_> = ["originsKey", "setupHooks"]
        .into_iter()
        .filter(|key| data.contains_key(*key))
        .collect();
    if !retired.is_empty() {
        return Err(Error {
            code: "LEGACY_STATE",
            message: format!(
                "Profile uses retired {}; run graftr migrate to convert it to hooks",
                retired.join("/")
            ),
        });
    }
    ensure(
        ["repo", "worktreeRoot", "localPorts"]
            .iter()
            .all(|key| data.contains_key(*key))
            && data.keys().all(|key| {
                [
                    "repo",
                    "worktreeRoot",
                    "localPorts",
                    "httpsPorts",
                    "ports",
                    "url",
                    "hooks",
                    "branchName",
                    "worktreePath",
                    "envSource",
                    "workspaceLabel",
                    "envOverrides",
                    "autoSetup",
                    "installCommand",
                    "installInputs",
                    "installOutputs",
                    "healthChecks",
                ]
                .contains(&key.as_str())
            }),
        "Profile has missing or unsupported fields",
    )?;
    if let Some(value) = data.get("autoSetup") {
        ensure(value.is_boolean(), "autoSetup must be a boolean")?;
    }
    let mut result = strings(data, &["repo", "worktreeRoot"])?;
    for key in ["branchName", "worktreePath", "workspaceLabel"] {
        optional(data, &mut result, key, |value| template(value, key))?;
    }
    optional(data, &mut result, "envSource", |value| {
        text(value)?;
        Ok(value.clone())
    })?;
    optional(data, &mut result, "envOverrides", |value| {
        environment::parse(value, true)
    })?;
    result.insert("localPorts".into(), port_range(field(data, "localPorts")?)?);
    optional(data, &mut result, "httpsPorts", port_range)?;
    optional(data, &mut result, "ports", |value| {
        let ports = object(value)?;
        ensure(ports.len() <= 16, "ports supports at most 16 named ports")?;
        for (name, value) in ports {
            ensure(
                named_port(name),
                "Port names use lowercase letters, numbers and single hyphens (at most 32 characters)",
            )?;
            port_range(value)?;
        }
        Ok(value.clone())
    })?;
    optional(data, &mut result, "url", |value| {
        text(value)?;
        environment::parse(&json!({"BASE_URL": value}), true)?;
        Ok(value.clone())
    })?;
    optional(data, &mut result, "autoSetup", |value| Ok(value.clone()))?;
    optional(data, &mut result, "installCommand", |value| {
        command(value, "installCommand")
    })?;
    for key in ["installInputs", "installOutputs"] {
        optional(data, &mut result, key, paths)?;
    }
    optional(data, &mut result, "healthChecks", health_checks)?;
    optional(data, &mut result, "hooks", |value| {
        let hooks = object(value)?;
        ensure(
            hooks.keys().all(|key| {
                ["afterEnvironment", "afterDependencies", "cleanup", "retire"]
                    .contains(&key.as_str())
            }),
            "hooks supports only afterEnvironment, afterDependencies, cleanup and retire",
        )?;
        let mut result = Object::new();
        for phase in ["afterEnvironment", "afterDependencies", "cleanup", "retire"] {
            if let Some(value) = hooks.get(phase) {
                result.insert(
                    phase.into(),
                    hook_commands(value, ["cleanup", "retire"].contains(&phase))?,
                );
            }
        }
        Ok(Value::Object(result))
    })?;
    Ok(Profile(Value::Object(result)))
}
pub fn configured(paths: &Paths, project: &str) -> Result<Profile> {
    parse(&Value::Object(config::settings(paths, project)?))
}
pub fn for_repo(paths: &Paths, repo: &str) -> Result<Option<(String, Object)>> {
    let projects = config::projects(paths)?;
    let matches: Vec<_> = projects
        .iter()
        .filter(|(_, value)| value.is_object() && value["repo"] == repo)
        .collect();
    ensure(
        matches.len() <= 1,
        "Multiple project profiles claim this repository; select one before setup",
    )?;
    matches
        .first()
        .map(|(key, value)| {
            name(key)?;
            Ok(((*key).clone(), object(value)?.clone()))
        })
        .transpose()
}
pub fn herdr_mode(paths: &Paths) -> Result<String> {
    let value = if crate::paths::exists(&paths.config)? {
        config::read(paths)?
    } else {
        json!({})
    };
    let mode = value.get("herdr").cloned().unwrap_or(json!("auto"));
    Ok(choice(
        &mode,
        &["auto", "off", "required"],
        "herdr must be auto, off, or required",
    )?
    .into())
}
pub fn startup_thaw(paths: &Paths) -> Result<bool> {
    let value = if crate::paths::exists(&paths.config)? {
        config::read(paths)?
    } else {
        json!({})
    };
    value.get("startupThaw").map_or(Ok(true), |value| {
        value
            .as_bool()
            .ok_or_else(|| "startupThaw must be a boolean".into())
    })
}
