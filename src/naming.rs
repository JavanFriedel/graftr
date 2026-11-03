use crate::{profiles, validation::*};
use serde_json::{Value, json};
use std::path::Path;

fn invalid(message: impl Into<String>) -> Error {
    Error {
        code: "INVALID_CREATION_CONFIG",
        message: message.into(),
    }
}
fn expand(template: &str, context: &Value, field: &str) -> Result<String> {
    let mut result = String::new();
    let mut previous = 0;
    for capture in pattern!(r"\{\{([^{}]*)\}\}").captures_iter(template) {
        let matched = capture.get(0).unwrap();
        let token = capture[1].trim_matches(js_space);
        let value = context[token].as_str().unwrap_or("");
        ensure(
            field != "worktreePath" || !value.is_empty(),
            invalid(format!(
                "worktreePath placeholder {token} rendered empty; choose another template"
            )),
        )?;
        result.push_str(&template[previous..matched.start()]);
        result.push_str(value);
        previous = matched.end();
    }
    result.push_str(&template[previous..]);
    ensure(
        !result.contains("{{") && !result.contains("}}"),
        invalid(format!("Malformed placeholder in {field}")),
    )?;
    Ok(result)
}
pub fn creation(
    settings: &Object,
    project: &str,
    task: &str,
    repo: &str,
    root: &str,
    existing: Option<&str>,
) -> Result<(String, String)> {
    let branch_template = settings
        .get("branchName")
        .cloned()
        .unwrap_or(json!("task/{{task}}"));
    let path_template = settings
        .get("worktreePath")
        .cloned()
        .unwrap_or(json!("{{task}}"));
    profiles::creation_template(&branch_template, "branchName")?;
    profiles::creation_template(&path_template, "worktreePath")?;
    let mut context = json!({"project": project, "task": task, "repoName": Path::new(repo).file_name().unwrap_or_default().to_string_lossy()});
    let branch = match existing {
        Some(branch) => branch.to_owned(),
        None => expand(branch_template.as_str().unwrap(), &context, "branchName")?,
    };
    ensure(
        !branch.is_empty()
            && branch.encode_utf16().count() <= 4096
            && branch == branch.trim_matches(js_space)
            && !branch.chars().any(control),
        invalid(
            "Rendered branchName must be a nonempty string of at most 4096 characters without control characters or leading/trailing whitespace",
        ),
    )?;
    let slug = pattern!(r"[^a-zA-Z0-9._-]+")
        .replace_all(&branch, "-")
        .to_string();
    let slug = pattern!(r"-+")
        .replace_all(&slug, "-")
        .trim_matches(['.', '-'])
        .to_owned();
    context["branch"] = json!(branch);
    context["branchSlug"] = json!(slug);
    let path = expand(path_template.as_str().unwrap(), &context, "worktreePath")?;
    profiles::creation_template(&json!(path), "worktreePath")?;
    let destination = Path::new(root).join(path).to_string_lossy().into_owned();
    ensure(
        absolute_normalized(root)
            && destination != root
            && Path::new(&destination).starts_with(root)
            && destination.len() <= 4095
            && !destination
                .split('/')
                .any(|part| part.eq_ignore_ascii_case(".git")),
        invalid(
            "worktreePath must resolve beneath worktreeRoot, outside Git metadata, and fit filesystem path limits",
        ),
    )?;
    Ok((branch, destination))
}
