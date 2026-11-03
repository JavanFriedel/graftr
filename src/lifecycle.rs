use crate::{
    checkout::{self, string},
    cleanup, herdr, human, locks, paths, profiles, records,
    setup::Engine,
    stateio, system,
    validation::*,
};
use serde_json::{Value, json};
use std::{env, path::Path, time::Duration};

pub struct Lifecycle<'a, 'b> {
    pub engine: &'a Engine<'b>,
}
fn cwd() -> Result<String> {
    Ok(env::current_dir()
        .map_err(|_| Error::from("Current directory is unavailable"))?
        .to_string_lossy()
        .into_owned())
}
fn inside(child: &str, parent: &str) -> bool {
    child == parent
        || child
            .strip_prefix(parent)
            .is_some_and(|s| s.starts_with('/'))
}
fn context() -> Result<Value> {
    let value: Value = serde_json::from_str(&env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or("{}".into())).map_err(|_| Error {code: "INVALID_JSON", message: "Invalid JSON; review configuration/state or the Herdr response (contents were not logged)".into()})?;
    object(&value)?;
    Ok(value)
}
pub fn native_removal_action() -> bool {
    herdr::in_plugin_action() && env::var("HERDR_PLUGIN_ACTION_ID").as_deref() == Ok("remove")
}
pub fn removal_worker() -> Result<bool> {
    Ok(native_removal_action() && context()?["invocation_source"] == "graftr-confirmed-remove")
}
impl Lifecycle<'_, '_> {
    pub fn record_closure(&self, task: &mut Value, reason: &str) -> Result<()> {
        task["status"] = json!("closed");
        self.engine.save_task(task)?;
        self.engine.cancel_setup(task, reason)
    }
    pub fn reconcile(&self) -> Result<()> {
        let (snapshot, setup_snapshot) = {
            let _state = locks::state(self.engine.paths)?;
            (
                records::tasks(&self.engine.paths.state)?,
                records::setups(&self.engine.paths.state)?,
            )
        };
        let mut sessions = std::collections::HashMap::new();
        for record in &snapshot {
            let task = record.value();
            let Some(socket) = task["socket"].as_str() else {
                continue;
            };
            if task["status"] == "preparing" && task["workspaceId"].is_null() {
                continue;
            }
            if !sessions.contains_key(socket) {
                sessions.insert(socket.to_owned(), self.engine.workspaces(socket)?);
            }
        }
        let mut pending = Vec::new();
        {
            let _state = locks::state(self.engine.paths)?;
            // External reads must not block unrelated registry operations. Refuse a
            // stale snapshot before making any closure or cleanup-plan changes.
            let current = records::tasks(&self.engine.paths.state)?;
            let setups = records::setups(&self.engine.paths.state)?;
            ensure(
                current
                    .iter()
                    .map(records::Task::value)
                    .eq(snapshot.iter().map(records::Task::value))
                    && setups
                        .iter()
                        .map(records::Setup::value)
                        .eq(setup_snapshot.iter().map(records::Setup::value)),
                "Task or setup records changed during reconciliation; retry after reviewing the current workspace state",
            )?;
            for record in current {
                let mut task = record.value().clone();
                let Some(socket) = task["socket"].as_str() else {
                    continue;
                };
                if task["status"] == "preparing" && task["workspaceId"].is_null() {
                    continue;
                }
                let spaces = &sessions[socket];
                let workspace = spaces
                    .iter()
                    .find(|w| Some(w.workspace_id.as_str()) == task["workspaceId"].as_str());
                ensure(
                    workspace.is_none_or(|w| w.checkout_path.as_deref() == task["path"].as_str()),
                    "Workspace identity does not match the owned checkout",
                )?;
                if workspace.is_none() {
                    self.record_closure(&mut task, "Workspace closed")?;
                    cleanup::queue(self.engine.paths, &mut task, "cleanup")?;
                    if cleanup::pending(&task, "cleanup") {
                        pending.push(task);
                    }
                }
            }
        }
        for task in pending {
            let _tree = locks::worktree(self.engine.paths, string(&task, "path"), false)?;
            cleanup::run(
                self.engine.paths,
                &task,
                self.engine.progress,
                false,
                "cleanup",
            )?;
        }
        Ok(())
    }
    pub fn handoff_freeze(&self, record: &Value) -> Result<Option<Value>> {
        let task = self.task(string(record, "id"))?;
        if task["socket"].is_null()
            || herdr::in_plugin_action()
            || !(inside(&cwd()?, string(&task, "path"))
                || !task["workspaceId"].is_null()
                    && herdr::caller_workspace(task["socket"].as_str())?.as_deref()
                        == task["workspaceId"].as_str())
        {
            return Ok(None);
        }
        ensure(
            self.preview(string(&task, "id"))? == record["removal"],
            "Confirmed freeze target changed; review and confirm again",
        )?;
        ensure(
            !task["workspaceId"].is_null() && self.engine.workspace(&task)?.is_some(),
            "Freeze from inside needs its owned live Herdr workspace; run from another workspace",
        )?;
        let reply = self.engine.progress.run("dispatch-freeze", string(&task, "id"), || herdr::request(task["socket"].as_str(), "plugin.action.invoke", json!({"plugin_id": "local.graftr", "action_id": "freeze", "context": {"workspace_id": task["workspaceId"], "invocation_source": "graftr-confirmed-freeze", "correlation_id": serde_json::to_string(&json!({"id": record["id"], "generation": record["generation"]})).unwrap()}}), Duration::from_secs(60)))?;
        object(&reply["log"])?;
        ensure(
            reply["type"] == "plugin_action_invoked"
                && [Some("running"), Some("succeeded")].contains(&reply["log"]["status"].as_str()),
            "Freeze handoff was not accepted; inspect graftr frozen and the Herdr plugin logs",
        )?;
        Ok(Some(
            json!({"id": record["id"], "status": "freezing", "logId": text(&reply["log"]["log_id"])?}),
        ))
    }
    fn task(&self, id: &str) -> Result<Value> {
        Ok(records::task(&self.engine.paths.state, id)?.value().clone())
    }
    pub fn close(&self, id: &str) -> Result<Value> {
        let task = self.task(id)?;
        if !herdr::in_plugin_action()
            && task["workspaceId"].as_str().is_some()
            && herdr::caller_workspace(task["socket"].as_str())?.as_deref()
                == task["workspaceId"].as_str()
        {
            ensure(
                self.engine.workspace(&task)?.is_some(),
                "Owned workspace is no longer open; inspect graftr status before retrying",
            )?;
            let reply = self.engine.progress.run("dispatch-close", id, || herdr::request(task["socket"].as_str(), "plugin.action.invoke", json!({"plugin_id": "local.graftr", "action_id": "close", "context": {"workspace_id": task["workspaceId"]}}), Duration::from_secs(60)))?;
            object(&reply["log"])?;
            ensure(
                reply["type"] == "plugin_action_invoked"
                    && [Some("running"), Some("succeeded")]
                        .contains(&reply["log"]["status"].as_str()),
                "Close handoff was not accepted; inspect the Herdr plugin logs before retrying",
            )?;
            return Ok(
                json!({"id": id, "status": "closing", "workspaceId": task["workspaceId"], "logId": text(&reply["log"]["log_id"])?}),
            );
        }
        self.cleanup(id, true)
    }
    fn require_close_owner(&self, task: &Value) -> Result<()> {
        let tasks = records::tasks(&self.engine.paths.state)?;
        ensure(
            tasks
                .iter()
                .filter(|r| r.value()["path"] == task["path"])
                .count()
                == 1
                && !tasks.iter().any(|r| {
                    r.value()["id"] != task["id"]
                        && r.value()["socket"] == task["socket"]
                        && !task["workspaceId"].is_null()
                        && r.value()["workspaceId"] == task["workspaceId"]
                }),
            "Conflicting checkout/workspace ownership; review records before closing",
        )
    }
    pub fn cleanup(&self, id: &str, close: bool) -> Result<Value> {
        let observation = if close {
            let (task, receipt) = {
                let _lock = locks::state(self.engine.paths)?;
                let task = self.task(id)?;
                self.require_close_owner(&task)?;
                let receipt = records::setup_for(
                    &self.engine.paths.state,
                    task["socket"].as_str(),
                    string(&task, "path"),
                )?;
                (task, receipt)
            };
            let workspace = self.engine.workspace(&task)?;
            Some((task, receipt, workspace))
        } else {
            None
        };
        let task = {
            let _lock = locks::state(self.engine.paths)?;
            let mut task = self.task(id)?;
            let workspace = if let Some((expected, expected_receipt, workspace)) = observation {
                let receipt = records::setup_for(
                    &self.engine.paths.state,
                    task["socket"].as_str(),
                    string(&task, "path"),
                )?;
                // Do not cancel a newer setup or act on an attachment changed while
                // waiting for inventory. The close itself remains serialized.
                ensure(
                    task == expected
                        && receipt.as_ref().map(records::Setup::value)
                            == expected_receipt.as_ref().map(records::Setup::value),
                    "Task or setup changed during close inventory; review the current state and retry close",
                )?;
                self.require_close_owner(&task)?;
                workspace
            } else {
                None
            };
            self.engine.cancel_setup(
                &task,
                if close {
                    "Workspace closed"
                } else {
                    "Setup cancelled by cleanup"
                },
            )?;
            cleanup::queue(self.engine.paths, &mut task, "cleanup")?;
            if let Some(workspace) = workspace {
                self.engine.progress.run("close-workspace", id, || {
                    herdr::request(
                        task["socket"].as_str(),
                        "workspace.close",
                        json!({"workspace_id": workspace.workspace_id}),
                        Duration::from_secs(60),
                    )
                })?;
            }
            task["status"] = json!(if close { "closed" } else { "cleaned" });
            self.engine.save_task(&task)?;
            task
        };
        if !cleanup::pending(&task, "cleanup") {
            return Ok(task);
        }
        let _lock = locks::worktree(self.engine.paths, string(&task, "path"), false)?;
        cleanup::run(
            self.engine.paths,
            &task,
            self.engine.progress,
            true,
            "cleanup",
        )?;
        self.task(id)
    }
    pub fn preview(&self, id: &str) -> Result<Value> {
        let initial = self.task(id)?;
        let _tree = locks::worktree(self.engine.paths, string(&initial, "path"), true)?;
        let _state = locks::state(self.engine.paths)?;
        let task = self.task(id)?;
        checkout::require_identity(&task, &initial)?;
        let observed = checkout::observe(&task)?;
        self.validate_removal(&task, false, Some(&observed))?;
        Ok(observed)
    }
    fn check_nuke_caller(&self, task: &Value) -> Result<()> {
        ensure(
            !herdr::in_plugin_action()
                && !inside(&cwd()?, string(task, "path"))
                && (task["workspaceId"].is_null()
                    || herdr::caller_workspace(task["socket"].as_str())?.as_deref()
                        != task["workspaceId"].as_str()),
            "Run nuke from outside the target folder and Herdr workspace, with an explicit task ID and terminal confirmation",
        )
    }
    pub fn preview_nuke(&self, id: &str) -> Result<Value> {
        let initial = self.task(id)?;
        let _tree = locks::worktree(self.engine.paths, string(&initial, "path"), true)?;
        let _state = locks::state(self.engine.paths)?;
        let task = self.task(id)?;
        checkout::require_identity(&task, &initial)?;
        self.check_nuke_caller(&task)?;
        self.validate_removal(&task, true, None)?;
        let mut target = checkout::identity(&task);
        let proof = crate::branches::merged(self.engine.paths, &task)?;
        for (key, value) in proof.as_object().unwrap() {
            target[key] = value.clone();
        }
        Ok(target)
    }
    fn require_nuke_identity(&self, task: &Value, expected: &Value) -> Result<()> {
        checkout::require_identity(task, expected)?;
        let proof = json!({"branchCommit": expected["branchCommit"], "mergeRef": expected["mergeRef"], "mergeCommit": expected["mergeCommit"], "mergedBy": expected["mergedBy"]});
        ensure(
            crate::branches::merged(self.engine.paths, task)? == proof,
            "Confirmed branch or merge target changed; review and confirm nuke again",
        )
    }
    pub fn nuke(&self, id: &str, expected: &Value) -> Result<Value> {
        let _tree = locks::worktree(self.engine.paths, string(expected, "path"), true)?;
        let initial = self.task(id)?;
        if cleanup::pending(&initial, "cleanup") {
            let queued = {
                let _state = locks::state(self.engine.paths)?;
                let mut task = self.task(id)?;
                checkout::require_identity(&task, expected)?;
                self.check_nuke_caller(&task)?;
                self.validate_removal(&task, true, None)?;
                self.require_nuke_identity(&task, expected)?;
                self.engine.cancel_setup(&task, "Setup cancelled by nuke")?;
                cleanup::queue(self.engine.paths, &mut task, "cleanup")?;
                task
            };
            cleanup::run(
                self.engine.paths,
                &queued,
                self.engine.progress,
                true,
                "cleanup",
            )?;
        }
        let nuked = {
            let _state = locks::state(self.engine.paths)?;
            let mut task = self.task(id)?;
            checkout::require_identity(&task, expected)?;
            self.check_nuke_caller(&task)?;
            let workspace = self.validate_removal(&task, true, None)?;
            self.require_nuke_identity(&task, expected)?;
            self.remove_checkout(&mut task, workspace, None, Some(expected))?;
            self.require_nuke_identity(&task, expected)?;
            if !expected["branchCommit"].is_null() {
                self.engine.progress.run("delete-branch", id, || {
                    crate::branches::delete_merged(&task, expected)
                })?;
            }
            ensure(
                crate::branches::local_commit(string(&task, "repo"), string(&task, "branch"))?
                    .is_none(),
                "Branch still exists; task record and leases retained for review",
            )?;
            cleanup::queue(self.engine.paths, &mut task, "retire")?;
            task
        };
        self.retire(
            &nuked,
            json!({"id": id, "status": "nuked", "path": nuked["path"], "branch": nuked["branch"]}),
        )
    }
    fn check_caller(&self, task: &Value) -> Result<()> {
        ensure(
            herdr::in_plugin_action()
                || task["workspaceId"].as_str().is_none()
                || herdr::caller_workspace(task["socket"].as_str())?.as_deref()
                    != task["workspaceId"].as_str(),
            "Run remove from a different Herdr workspace or use its delete action; this caller would be terminated",
        )
    }
    pub fn check_removal(
        &self,
        task: &Value,
        allow_missing: bool,
        removal: Option<&Value>,
    ) -> Result<bool> {
        if let Some(removal) = removal {
            checkout::require_identity(task, removal)?;
            ensure(
                checkout::observe(task)? == *removal,
                "Confirmed removal branch/commit or target changed; review and confirm again",
            )?;
        }
        let tree = checkout::registered(task)?;
        let present = paths::exists(Path::new(string(task, "path")))?;
        ensure(
            tree.is_some() || !present,
            "Checkout is not registered with its recorded repository; refusing filesystem deletion",
        )?;
        ensure(
            tree.as_ref().is_none_or(|t| t.locked.is_none()),
            "Worktree is locked; unlock it explicitly before removal",
        )?;
        let branch = removal.map_or(&task["branch"], |r| &r["currentBranch"]);
        ensure(
            tree.as_ref()
                .is_none_or(|t| !t.bare && !t.detached && t.branch.as_deref() == branch.as_str()),
            "Worktree is detached or its branch changed; preserve commits on the recorded branch before removal",
        )?;
        if removal.is_some() && branch.is_null() && tree.is_none() && !present {
            return Ok(false);
        }
        let branch = branch.as_str().ok_or_else(|| {
            Error::from("Recorded branch is missing; removal would not preserve its commits")
        })?;
        let retained = system::run(
            &[
                "git",
                "-C",
                string(task, "repo"),
                "rev-parse",
                "--verify",
                &format!("refs/heads/{branch}^{{commit}}"),
            ],
            Some(Path::new(string(task, "repo"))),
            false,
        )?;
        if retained.exit_code != 0 {
            if allow_missing && tree.is_none() && !present && task["status"] == "closed" {
                return Ok(false);
            }
            return Err(
                "Recorded branch is missing; removal would not preserve its commits".into(),
            );
        }
        let commit = retained.stdout.trim_matches(js_space);
        ensure(
            removal.is_none_or(|r| r["currentCommit"] == commit),
            "Confirmed removal branch commit changed; review and confirm again",
        )?;
        if present {
            checkout::verify(task)?;
            ensure(
                system::git(
                    Path::new(string(task, "path")),
                    &["symbolic-ref", "--quiet", "HEAD"],
                )? == format!("refs/heads/{branch}")
                    && system::git(
                        Path::new(string(task, "path")),
                        &["rev-parse", "HEAD^{commit}"],
                    )? == commit
                    && removal.is_none_or(|r| r["currentCommit"] == commit),
                "Worktree HEAD does not match the retained or confirmed branch commit",
            )?;
            let index = system::run(
                &["git", "-C", string(task, "path"), "ls-files", "-v", "-z"],
                Some(Path::new(string(task, "repo"))),
                true,
            )?
            .stdout;
            ensure(
                !index
                    .split('\0')
                    .any(|entry| pattern!(r"^[a-zS] ").is_match(entry)),
                "Git index hides changes with assume-unchanged or skip-worktree flags; clear them before removal",
            )?;
            ensure(
                system::run(
                    &[
                        "git",
                        "-C",
                        string(task, "path"),
                        "status",
                        "--porcelain=v1",
                        "-z",
                        "--untracked-files=all",
                        "--ignore-submodules=none",
                    ],
                    Some(Path::new(string(task, "repo"))),
                    true,
                )?
                .stdout
                .is_empty(),
                "Worktree has uncommitted or untracked work; commit or move it before removal",
            )?;
        }
        Ok(tree.is_some())
    }
    pub fn validate_removal(
        &self,
        task: &Value,
        allow_missing: bool,
        removal: Option<&Value>,
    ) -> Result<Option<herdr::Workspace>> {
        let profile = profiles::configured(self.engine.paths, string(task, "project"))?;
        ensure(
            profile.value()["repo"] == task["repo"],
            "Project profile no longer owns this repository",
        )?;
        for field in ["repo", "path"] {
            let path = string(task, field);
            ensure(
                absolute_normalized(path) && checkout::canonical(path)? == path,
                "Removal paths must be absolute and canonical; symlinks are not removed",
            )?;
        }
        ensure(
            records::tasks(&self.engine.paths.state)?
                .iter()
                .filter(|t| t.value()["path"] == task["path"])
                .count()
                == 1,
            "Multiple records claim this checkout; review the inventory first",
        )?;
        let spaces = if let Some(socket) = task["socket"].as_str() {
            self.engine.workspaces(socket)?
        } else {
            Vec::new()
        };
        let workspace = spaces
            .iter()
            .find(|w| Some(w.workspace_id.as_str()) == task["workspaceId"].as_str())
            .cloned();
        ensure(
            workspace
                .as_ref()
                .is_none_or(|w| w.checkout_path.as_deref() == task["path"].as_str()),
            "Workspace identity does not match the owned checkout",
        )?;
        ensure(
            !spaces.iter().any(|w| {
                w.checkout_path.as_deref() == task["path"].as_str()
                    && Some(w.workspace_id.as_str()) != task["workspaceId"].as_str()
            }),
            "Checkout is open in another Herdr workspace; close it explicitly first",
        )?;
        self.check_removal(task, allow_missing, removal)?;
        Ok(workspace)
    }
    pub fn require_removed(&self, task: &Value) -> Result<()> {
        ensure(
            absolute_normalized(string(task, "path"))
                && checkout::canonical(string(task, "path"))? == string(task, "path")
                && Path::new(string(task, "repo")).is_absolute()
                && checkout::canonical(string(task, "repo"))? == string(task, "repo"),
            "Native removal paths must remain canonical",
        )?;
        ensure(
            !paths::exists(Path::new(string(task, "path")))?
                && checkout::registered(task)?.is_none(),
            "Removed checkout does not match the task or is still registered",
        )
    }
    fn remove_checkout(
        &self,
        task: &mut Value,
        workspace: Option<herdr::Workspace>,
        removal: Option<&Value>,
        nuke: Option<&Value>,
    ) -> Result<()> {
        if let Some(removal) = removal {
            self.check_removal(task, false, Some(removal))?;
        }
        if let Some(workspace) = workspace {
            self.engine
                .progress
                .run("close-workspace", string(task, "id"), || {
                    herdr::request(
                        task["socket"].as_str(),
                        "workspace.close",
                        json!({"workspace_id": workspace.workspace_id}),
                        Duration::from_secs(60),
                    )
                })?;
        }
        task["status"] = json!("closed");
        self.engine.save_task(task)?;
        let registered = self.check_removal(task, nuke.is_some(), removal)?;
        if let Some(expected) = nuke {
            self.require_nuke_identity(task, expected)?;
        }
        if registered {
            self.engine
                .progress
                .run("remove-checkout", string(task, "id"), || {
                    system::run(
                        &[
                            "git",
                            "-C",
                            string(task, "repo"),
                            "worktree",
                            "remove",
                            "--",
                            string(task, "path"),
                        ],
                        Some(Path::new(string(task, "repo"))),
                        true,
                    )
                })?;
        }
        ensure(
            !paths::exists(Path::new(string(task, "path")))?
                && checkout::registered(task)?.is_none(),
            "Checkout removal is incomplete; record and leases were retained",
        )
    }
    pub fn retire(&self, removed: &Value, result: Value) -> Result<Value> {
        if cleanup::pending(removed, "retire") {
            cleanup::run(
                self.engine.paths,
                removed,
                self.engine.progress,
                true,
                "retire",
            )?;
        }
        let _state = locks::state(self.engine.paths)?;
        let task = self.task(string(removed, "id"))?;
        checkout::require_identity(&task, removed)?;
        self.require_removed(&task)?;
        ensure(
            !cleanup::pending(&task, "retire"),
            "Retire hooks are unfinished; the record and leases were retained. Review their effects, then retry remove",
        )?;
        self.engine
            .progress
            .run("delete-setup", string(&task, "id"), || {
                stateio::delete_setup(self.engine.paths, &task)
            })?;
        self.engine
            .progress
            .run("delete-task", string(&task, "id"), || {
                stateio::delete_task(self.engine.paths, &task)
            })?;
        Ok(result)
    }
    pub fn remove(&self, id: &str, expected: &Value) -> Result<Value> {
        let initial = self.task(id)?;
        let mut target = expected.clone();
        if removal_worker()? {
            let confirmed: Value = serde_json::from_str(text(&context()?["correlation_id"])?).map_err(|_| Error {code: "INVALID_JSON", message: "Invalid JSON; review configuration/state or the Herdr response (contents were not logged)".into()})?;
            object(&confirmed)?;
            checkout::require_identity(&initial, &confirmed)?;
            ensure(
                confirmed["currentBranch"].is_null() && confirmed["currentCommit"].is_null()
                    || confirmed["currentBranch"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty())
                        && confirmed["currentCommit"]
                            .as_str()
                            .is_some_and(|s| pattern!(r"^[a-f0-9]{40,64}$").is_match(s)),
                "Confirmed removal target changed or lacks a branch snapshot; review and confirm again",
            )?;
            let mut handed = checkout::identity(&initial);
            handed["currentBranch"] = confirmed["currentBranch"].clone();
            handed["currentCommit"] = confirmed["currentCommit"].clone();
            ensure(
                target == handed,
                "Confirmed removal target changed; review and confirm again",
            )?;
            target = handed;
        }
        checkout::require_identity(&initial, &target)?;
        let mut result = json!({"id": id, "status": "removed", "path": initial["path"], "branch": target["currentBranch"]});
        if target["currentBranch"] != initial["branch"] {
            result["recordedBranch"] = initial["branch"].clone();
        }
        if !initial["socket"].is_null()
            && !herdr::in_plugin_action()
            && (inside(&cwd()?, string(&initial, "path"))
                || !initial["workspaceId"].is_null()
                    && herdr::caller_workspace(initial["socket"].as_str())?.as_deref()
                        == initial["workspaceId"].as_str())
        {
            ensure(
                self.preview(id)? == target,
                "Confirmed removal target changed; review and confirm again",
            )?;
            ensure(
                !initial["workspaceId"].is_null() && self.engine.workspace(&initial)?.is_some(),
                "Removal from inside needs its owned Herdr workspace; run from outside the checkout with an explicit task ID",
            )?;
            let reply = self.engine.progress.run("dispatch-remove", id, || herdr::request(initial["socket"].as_str(), "plugin.action.invoke", json!({"plugin_id": "local.graftr", "action_id": "remove", "context": {"workspace_id": initial["workspaceId"], "invocation_source": "graftr-confirmed-remove", "correlation_id": serde_json::to_string(&target).unwrap()}}), Duration::from_secs(60)))?;
            object(&reply["log"])?;
            ensure(
                reply["type"] == "plugin_action_invoked"
                    && [Some("running"), Some("succeeded")]
                        .contains(&reply["log"]["status"].as_str()),
                "Removal handoff was not accepted; inspect the Herdr plugin logs before retrying",
            )?;
            result["status"] = json!("removing");
            result["logId"] = json!(text(&reply["log"]["log_id"])?);
            return Ok(result);
        }
        let _tree = locks::worktree(self.engine.paths, string(&initial, "path"), true)?;
        if cleanup::pending(&initial, "cleanup") {
            let queued = {
                let _state = locks::state(self.engine.paths)?;
                let mut task = self.task(id)?;
                checkout::require_identity(&task, &target)?;
                self.check_caller(&task)?;
                self.validate_removal(&task, false, Some(&target))?;
                self.engine
                    .cancel_setup(&task, "Setup cancelled by removal")?;
                cleanup::queue(self.engine.paths, &mut task, "cleanup")?;
                task
            };
            cleanup::run(
                self.engine.paths,
                &queued,
                self.engine.progress,
                true,
                "cleanup",
            )?;
        }
        let removed = {
            let _state = locks::state(self.engine.paths)?;
            let mut task = self.task(id)?;
            checkout::require_identity(&task, &target)?;
            ensure(
                task["socket"].is_null() || !inside(&cwd()?, string(&task, "path")),
                "Run remove from outside the target checkout; its Herdr shells will close",
            )?;
            self.check_caller(&task)?;
            let workspace = self.validate_removal(&task, false, Some(&target))?;
            self.remove_checkout(&mut task, workspace, Some(&target), None)?;
            cleanup::queue(self.engine.paths, &mut task, "retire")?;
            task
        };
        self.retire(&removed, result)
    }
}
pub fn task_data(task: &Value) -> Value {
    let mut data = json!({});
    for key in ["id", "status", "path", "branch", "workspaceId", "url"] {
        data[key] = task[key].clone();
    }
    if let Some(plan) = task.get("cleanup") {
        data["cleanup"] = json!({"status": plan["status"], "attempt": plan["attempt"], "results": plan["results"]});
    }
    data
}
pub fn format_task(task: &Value, columns: usize) -> String {
    let status = string(task, "status");
    human::fields(
        &format!("{}: {status}", string(task, "id")),
        &[
            ("Branch", Some(string(task, "branch").into())),
            ("Path", Some(string(task, "path").into())),
            (
                "Address",
                Some(task["url"].as_str().map_or_else(
                    || format!("localhost:{}", task["localPort"].as_f64().unwrap()),
                    str::to_owned,
                )),
            ),
        ],
        Some(if status == "closed" {
            "Workspace closed; checkout, branch and port assignments kept."
        } else if status == "cleaned" {
            "Resources cleaned; workspace, checkout and branch kept."
        } else {
            "Prepared checkout; development servers are not started."
        }),
        status,
        columns,
    )
}
pub fn format_action(result: &Value, columns: usize) -> String {
    let status = string(result, "status");
    let queued = ["closing", "removing", "freezing"].contains(&status);
    human::fields(
        &format!(
            "{}: {status}{}",
            string(result, "id"),
            if queued { " (queued)" } else { "" }
        ),
        &[
            ("Path", result["path"].as_str().map(str::to_owned)),
            ("Branch", result["branch"].as_str().map(str::to_owned)),
            ("Worker log", result["logId"].as_str().map(str::to_owned)),
        ],
        if queued {
            Some("Accepted, not completed. Check graftr status for the worker result.")
        } else if status == "removed" {
            Some("Checkout removed; existing Git branches retained.")
        } else if status == "nuked" {
            Some("Checkout and merged local branch deleted.")
        } else {
            None
        },
        status,
        columns,
    )
}
