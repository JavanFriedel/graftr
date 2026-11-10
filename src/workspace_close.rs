use crate::{herdr, locks, paths::Paths, progress::Progress, records, targets, validation::*};
use serde_json::{Value, json};
use std::{env, time::Duration};

const SOURCE: &str = "graftr-confirmed-workspace-close";

pub fn worker_target() -> Result<Option<Value>> {
    if !herdr::in_plugin_action() || env::var("HERDR_PLUGIN_ACTION_ID").as_deref() != Ok("close") {
        return Ok(None);
    }
    let context: Value = serde_json::from_str(
        &env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_else(|_| "{}".into()),
    )
    .map_err(|_| Error::from("Invalid workspace-close context; no workspace was closed"))?;
    object(&context)?;
    if context["invocation_source"] != SOURCE {
        return Ok(None);
    }
    let target: Value = serde_json::from_str(text(&context["correlation_id"])?)
        .map_err(|_| Error::from("Invalid confirmed workspace; no workspace was closed"))?;
    object(&target)?;
    ensure(
        context["workspace_id"] == target["workspaceId"],
        "Confirmed workspace does not match the selected Herdr action",
    )?;
    Ok(Some(target))
}

pub fn preview(paths: &Paths, selection: &Value) -> Result<Value> {
    let socket = text(&selection["socket"])?;
    let workspace_id = text(&selection["workspaceId"])?;
    ensure(
        herdr::caller_socket()? == socket
            && herdr::caller_workspace(Some(socket))?.as_deref() == Some(workspace_id),
        "Herdr caller workspace changed; review the selected workspace and retry",
    )?;
    let spaces = herdr::workspaces(&herdr::request(
        Some(socket),
        "workspace.list",
        json!({}),
        Duration::from_secs(60),
    )?)?;
    let matches: Vec<_> = spaces
        .iter()
        .filter(|w| w.workspace_id == workspace_id)
        .collect();
    ensure(
        matches.len() == 1,
        "Selected Herdr workspace is missing or ambiguous",
    )?;
    let workspace = matches[0];
    let target = json!({"socket": socket, "workspaceId": workspace_id, "label": workspace.label,
        "path": workspace.checkout_path, "managed": false, "action": "close-workspace"});
    require_unmanaged(paths, &target)?;
    Ok(target)
}

fn require_unmanaged(paths: &Paths, target: &Value) -> Result<()> {
    ensure(
        !records::tasks(&paths.state)?.iter().any(|record| {
            let task = record.value();
            task["socket"] == target["socket"] && task["workspaceId"] == target["workspaceId"]
                || target["path"]
                    .as_str()
                    .is_some_and(|path| targets::at_directory(task, path))
        }),
        "A Graftr task owns this workspace or checkout; use its managed lifecycle instead",
    )?;
    ensure(
        !records::adoptions(&paths.state)?.iter().any(|record| {
            let adoption = record.value();
            adoption["socket"] == target["socket"]
                && adoption["workspaceId"] == target["workspaceId"]
                || target["path"]
                    .as_str()
                    .is_some_and(|path| targets::at_directory(adoption, path))
        }),
        "This workspace has adopted external setup; use Herdr directly for its lifecycle",
    )
}

pub fn dispatch(paths: &Paths, progress: &Progress, expected: &Value) -> Result<Value> {
    ensure(
        preview(paths, expected)? == *expected,
        "Workspace identity changed; review and confirm close again",
    )?;
    let reply = progress.run(
        "dispatch-workspace-close",
        text(&expected["workspaceId"])?,
        || {
            herdr::request(
                expected["socket"].as_str(),
                "plugin.action.invoke",
                json!({"plugin_id": "local.graftr", "action_id": "close", "context": {
                    "workspace_id": expected["workspaceId"], "invocation_source": SOURCE,
                    "correlation_id": serde_json::to_string(expected).unwrap(),
                }}),
                Duration::from_secs(60),
            )
        },
    )?;
    object(&reply["log"])?;
    ensure(
        reply["type"] == "plugin_action_invoked"
            && [Some("running"), Some("succeeded")].contains(&reply["log"]["status"].as_str()),
        "Workspace-close handoff was not accepted; inspect Herdr and its plugin logs before retrying",
    )?;
    let mut result = expected.clone();
    result["status"] = json!("closing");
    result["logId"] = json!(text(&reply["log"]["log_id"])?);
    Ok(result)
}

pub fn complete(paths: &Paths, progress: &Progress, expected: &Value) -> Result<Value> {
    ensure(
        preview(paths, expected)? == *expected,
        "Confirmed workspace identity changed; no workspace was closed",
    )?;
    // Fence enrollment while closing, without claiming ownership or running task cleanup.
    let _state = locks::state(paths)?;
    require_unmanaged(paths, expected)?;
    progress.run("close-workspace", text(&expected["workspaceId"])?, || {
        herdr::request(
            expected["socket"].as_str(),
            "workspace.close",
            json!({"workspace_id": expected["workspaceId"]}),
            Duration::from_secs(60),
        )
    })?;
    let mut result = expected.clone();
    result["status"] = json!("closed");
    Ok(result)
}
