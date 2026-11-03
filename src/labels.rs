use crate::{
    checkout::{self, string},
    config, herdr, human, locks,
    paths::Paths,
    profiles, records,
    validation::*,
};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

pub fn worktree_label(task: &Value, template: Option<&str>) -> Result<String> {
    let template = template.unwrap_or(if task["httpsPort"].is_null() {
        "{{localPort}} {{taskName}}"
    } else {
        "{{httpsPort}} {{taskName}}"
    });
    profiles::label_template(&json!(template))?;
    let branch = string(task, "branch");
    let task_name = pattern!(r"[-/]+")
        .replace_all(branch.split_once('/').map_or(branch, |(_, rest)| rest), " ")
        .trim_matches(js_space)
        .to_owned();
    let mut context = json!({"localPort": task["localPort"].as_f64().unwrap().to_string(), "httpsPort": task["httpsPort"].as_f64().map(|n| n.to_string()), "url": task["url"], "host": task["host"],
        "worktreeName": Path::new(string(task, "path")).file_name().unwrap_or_default().to_string_lossy(), "worktreePath": task["path"], "branch": branch, "project": task["project"], "taskName": task_name});
    if let Some(ports) = task["ports"].as_object() {
        for (key, value) in ports {
            context[format!("port.{key}")] = json!(value.as_f64().unwrap().to_string());
        }
    }
    let mut result = String::new();
    let mut previous = 0;
    for captures in pattern!(r"\{\{([^{}]*)\}\}").captures_iter(template) {
        let matched = captures.get(0).unwrap();
        let name = captures[1].trim_matches(js_space);
        let value = context[name].as_str().ok_or_else(|| {
            Error::from(format!(
                "Workspace label placeholder {name} is unavailable for this task"
            ))
        })?;
        result.push_str(&template[previous..matched.start()]);
        result.push_str(value);
        previous = matched.end();
    }
    result.push_str(&template[previous..]);
    // Expanded public values may contain literal braces; only label shape is revalidated.
    profiles::label_shape(&result)?;
    Ok(result)
}
fn owned<'a>(task: &Value, spaces: &'a [herdr::Workspace]) -> Result<Option<&'a herdr::Workspace>> {
    let workspace = spaces
        .iter()
        .find(|w| Some(w.workspace_id.as_str()) == task["workspaceId"].as_str());
    if let Some(workspace) = workspace {
        ensure(
            workspace.checkout_path.as_deref() == task["path"].as_str(),
            "Workspace identity does not match the owned checkout",
        )?;
    }
    Ok(workspace)
}
fn list(task: &Value) -> Result<Vec<herdr::Workspace>> {
    herdr::workspaces(&herdr::request(
        task["socket"].as_str(),
        "workspace.list",
        json!({}),
        Duration::from_secs(60),
    )?)
}
fn unique(task: &Value, spaces: &[herdr::Workspace]) -> bool {
    spaces
        .iter()
        .filter(|w| Some(w.workspace_id.as_str()) == task["workspaceId"].as_str())
        .count()
        == 1
        && !spaces.iter().any(|w| {
            Some(w.checkout_path.as_deref()) == Some(task["path"].as_str())
                && Some(w.workspace_id.as_str()) != task["workspaceId"].as_str()
        })
}
pub fn refresh(paths: &Paths, id: &str, progress: &crate::progress::Progress) -> Result<Value> {
    let initial = records::task(&paths.state, id)?;
    let initial = initial.value();
    ensure(
        !initial["socket"].is_null(),
        "Standalone task has no Herdr attachment; open it from Herdr before refreshing its workspace label",
    )?;
    let _tree_lock = locks::worktree(paths, string(initial, "path"), true)?;
    let _state_lock = locks::state(paths)?;
    let task = records::task(&paths.state, id)?;
    let task = task.value();
    checkout::require_identity(task, initial)?;
    let receipt = records::setup_for(&paths.state, task["socket"].as_str(), string(task, "path"))?;
    let receipt_status = receipt.as_ref().and_then(|r| r.value()["status"].as_str());
    ensure(
        receipt_status != Some("pending")
            && !(task["status"] == "preparing" && receipt_status != Some("incomplete")),
        "Setup is pending/preparing without a recorded failure; wait for its worker before refreshing the label",
    )?;
    let records = records::tasks(&paths.state)?;
    ensure(
        records
            .iter()
            .filter(|r| r.value()["path"] == task["path"])
            .count()
            == 1
            && !records.iter().any(|r| {
                r.value()["id"] != id
                    && r.value()["socket"] == task["socket"]
                    && !task["workspaceId"].is_null()
                    && r.value()["workspaceId"] == task["workspaceId"]
            }),
        "Conflicting checkout/workspace ownership; review records before refreshing the label",
    )?;
    let settings = config::settings(paths, string(task, "project"))?;
    ensure(
        settings.get("repo") == Some(&task["repo"]),
        "Project profile no longer owns this repository",
    )?;
    let observed = checkout::observe(task)?;
    ensure(
        !observed["currentBranch"].is_null()
            && crate::paths::exists(Path::new(string(task, "path")))?,
        "Label refresh requires an existing registered checkout with an attached local branch",
    )?;
    let mut current = task.clone();
    current["branch"] = observed["currentBranch"].clone();
    let template = settings.get("workspaceLabel").map(text).transpose()?;
    let label = worktree_label(&current, template)?;
    let spaces = list(task)?;
    let workspace = owned(task, &spaces)?;
    ensure(
        workspace.is_some() && unique(task, &spaces),
        "Label refresh requires its unique live owned workspace; it will not open or adopt one",
    )?;
    let workspace = workspace.unwrap();
    let check = || -> Result<()> {
        ensure(
            records::task(&paths.state, id)?.value() == task
                && config::settings(paths, string(task, "project"))? == settings
                && checkout::observe(task)?["currentBranch"] == observed["currentBranch"],
            "Task, profile or current branch changed during label refresh; review and refresh again",
        )
    };
    check()?;
    if label != workspace.label {
        progress.run("refresh-label", id, || {
            herdr::request(
                task["socket"].as_str(),
                "workspace.rename",
                json!({"workspace_id": workspace.workspace_id, "label": label}),
                Duration::from_secs(60),
            )
        })?;
        check()?;
        let latest_spaces = list(task)?;
        ensure(
            owned(task, &latest_spaces)?.is_some_and(|w| w.label == label)
                && unique(task, &latest_spaces),
            "Workspace label refresh was not verified; inspect Herdr and refresh again",
        )?;
    }
    Ok(
        json!({"id": id, "status": if label == workspace.label {"unchanged"} else {"refreshed"}, "workspaceId": task["workspaceId"], "branch": observed["currentBranch"], "recordedBranch": task["branch"], "label": label}),
    )
}
pub fn format(report: &Value, columns: usize) -> String {
    human::fields(
        &format!(
            "{}: label {}",
            string(report, "id"),
            string(report, "status")
        ),
        &[
            ("Label", Some(string(report, "label").into())),
            ("Branch", Some(string(report, "branch").into())),
        ],
        None,
        string(report, "status"),
        columns,
    )
}
