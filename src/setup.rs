use crate::{
    addresses,
    checkout::{self, string},
    dependencies, envdoc, environment, health, herdr, hooks, human, inventory, locks, logs,
    paths::{self, Paths},
    profiles,
    progress::Progress,
    records, stateio, system, targets,
    validation::*,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    path::Path,
    time::{Duration, Instant},
};

#[derive(Default, Clone)]
pub struct Options {
    pub no_herdr: bool,
    pub skip_install: Option<bool>,
    pub project: Option<String>,
    pub name: Option<String>,
    pub wait: bool,
    pub herdr_socket: Option<String>,
    pub no_setup: bool,
    pub branch: Option<String>,
    pub restore: Option<Restore>,
    pub automatic: bool,
    pub expected_path: Option<String>,
    pub expected_repo: Option<String>,
    pub expected_branch: Option<String>,
}
#[derive(Clone)]
pub struct Restore {
    pub generation: String,
    pub repo: String,
    pub path: String,
    pub commit: String,
}
struct CreationSource {
    repo: String,
    branch: String,
    path: Option<String>,
    workspace: Option<String>,
    fallback: bool,
}
fn inside(child: &str, parent: &str) -> bool {
    child == parent
        || child
            .strip_prefix(parent)
            .is_some_and(|rest| rest.starts_with('/'))
}
fn invalid_creation(message: impl Into<String>) -> Error {
    Error {
        code: "INVALID_CREATION_CONFIG",
        message: message.into(),
    }
}
#[derive(Clone)]
pub struct SetupResult {
    pub receipt: Value,
    pub task: Option<Value>,
    pub local: bool,
    pub log_id: Option<String>,
    pub attention_needed: bool,
}
pub struct Engine<'a> {
    pub paths: &'a Paths,
    pub progress: &'a Progress,
}
fn timings(operation: &str) -> Result<Value> {
    Ok(
        json!({"id": system::uuid()?, "operation": operation, "startedAt": health::timestamp(), "stagesMs": {}}),
    )
}
fn add(timing: &mut Value, stage: &str, started: Instant) {
    if timing.is_object() {
        let old = timing["stagesMs"][stage].as_f64().unwrap_or(0.0);
        timing["stagesMs"][stage] = json!(old + started.elapsed().as_secs_f64() * 1000.0);
    }
}
fn finish(timing: &mut Value) {
    if timing.is_object() {
        let now = health::timestamp();
        let elapsed = date_millis(&now).unwrap()
            - date_millis(timing["startedAt"].as_str().unwrap()).unwrap();
        timing["finishedAt"] = json!(now);
        timing["elapsedMs"] = json!(elapsed.max(0));
    }
}
fn add_receipt_timing(receipt: &mut Value, stage: &str, started: Instant) {
    if let Some(timing) = receipt.get_mut("timings") {
        add(timing, stage, started);
    }
}
fn finish_receipt_timing(receipt: &mut Value) {
    if let Some(timing) = receipt.get_mut("timings") {
        finish(timing);
    }
}
fn dependency_timing(receipt: &mut Value, status: &str) {
    if let Some(timing) = receipt.get_mut("timings") {
        timing["dependencies"] = json!(status);
    }
}
fn new_receipt(path: &str) -> Value {
    json!({"schemaVersion": 1, "socket": null, "workspaceId": null, "path": path, "project": null, "taskId": null, "status": "pending", "step": "profile", "message": "Setup pending", "installKey": null, "dependencies": "not-configured", "skipInstall": false, "runId": null})
}
fn configuration(profile: &Value, task: &Value) -> Result<(Value, Value)> {
    let templates = profile
        .get("envOverrides")
        .cloned()
        .unwrap_or(json!({"PORT": "{{localPort}}"}));
    let assignments = environment::resolve(&templates, task)?;
    Ok((templates, assignments))
}
fn verify_environment(task: &Value, overrides: &Value) -> Result<()> {
    let bytes = records::read_private(&Path::new(string(task, "path")).join(".env"))?;
    let values = envdoc::loose(&bytes);
    let mut mismatched: Vec<_> = overrides
        .as_object()
        .unwrap()
        .iter()
        .filter(|(key, value)| values.get(*key).map(String::as_str) != value.as_str())
        .map(|(key, _)| key.clone())
        .collect();
    mismatched.sort();
    ensure(
        mismatched.is_empty(),
        format!(
            "Generated environment failed dotenv verification for {}; no values were logged. Existing .env kept; review/update these assignments manually and retry graftr setup.",
            mismatched.join(", ")
        ),
    )
}
impl Engine<'_> {
    pub fn create(
        &self,
        project: &str,
        name: &str,
        base: &str,
        options: &Options,
    ) -> Result<SetupResult> {
        let mut timing = timings("create")?;
        crate::validation::name(project)?;
        crate::validation::name(name)?;
        ensure(
            options.branch.is_none() || base == "HEAD",
            "Choose --branch or --base, not both",
        )?;
        let socket = self.integration_socket(options, None)?;
        let id = format!("{project}--{name}");
        if socket.is_some() && paths::exists(&self.paths.state.join(format!("{id}.json")))? {
            let task = records::task(&self.paths.state, &id)?;
            if task.value()["socket"].is_null() {
                self.wait_for_standalone_setup(string(task.value(), "path"))?;
            }
        }
        let result = if options.no_setup {
            self.create_locked(project, name, base, options, socket.as_deref(), &mut timing)?
        } else {
            let waiting = Instant::now();
            let _lock = locks::state(self.paths)?;
            add(&mut timing, "stateLockWait", waiting);
            self.create_locked(project, name, base, options, socket.as_deref(), &mut timing)?
        };
        self.finish_request(result, options)
    }
    fn create_locked(
        &self,
        project: &str,
        name: &str,
        base: &str,
        options: &Options,
        socket: Option<&str>,
        timing: &mut Value,
    ) -> Result<SetupResult> {
        let id = format!("{project}--{name}");
        ensure(!records::adoptions(&self.paths.state)?.iter().any(|r| r.value()["id"] == id), Error {code: "ADOPTED_EXTERNAL", message: "This worktree is tracked with external setup. Use graftr adopt PATH for explicit setup stages; automatic setup, cleanup, removal and freeze are not enabled for adopted worktrees. Existing files and manual services were preserved.".into()})?;
        let frozen: Vec<_> = records::frozen(&self.paths.state)?
            .into_iter()
            .filter(records::Frozen::reserves)
            .collect();
        let reservation = frozen.iter().find(|r| r.value()["id"] == id);
        ensure(
            reservation.is_none_or(|r| {
                r.value()["status"] == "thawing"
                    && options
                        .restore
                        .as_ref()
                        .is_some_and(|restore| r.value()["generation"] == restore.generation)
            }),
            "Task is frozen or has an unfinished freeze/thaw; use graftr thaw or inspect graftr frozen",
        )?;
        if let Some(restore) = &options.restore {
            ensure(
                reservation.is_some_and(|r| {
                    r.value()["removal"]["repo"] == restore.repo
                        && r.value()["removal"]["path"] == restore.path
                        && r.value()["removal"]["currentBranch"].as_str()
                            == options.branch.as_deref()
                        && r.value()["removal"]["currentCommit"] == restore.commit
                }),
                "Frozen recovery identity changed; no checkout was created",
            )?;
        }
        if paths::exists(&self.paths.state.join(format!("{id}.json")))? {
            let task = records::task(&self.paths.state, &id)?;
            ensure(
                socket.is_none()
                    || task.value()["socket"].is_null()
                    || task.value()["socket"].as_str() == socket,
                "Task belongs to another Herdr server",
            )?;
            ensure(
                options
                    .branch
                    .as_ref()
                    .is_none_or(|b| task.value()["branch"] == *b),
                "Task already owns a different branch; choose a different task name",
            )?;
            ensure(
                !options.no_setup,
                "This task is already managed; use cleanup rather than silently abandoning ownership",
            )?;
            return self.open_locked(task.value().clone(), options, timing.clone(), socket);
        }
        let mut source = self.creation_source(project, name, socket, options.branch.as_deref())?;
        if let Some(restore) = &options.restore {
            let profile = self.profile(project)?;
            ensure(
                !source.fallback
                    && source.repo == restore.repo
                    && inside(&restore.path, string(&profile, "worktreeRoot"))
                    && restore.path != string(&profile, "worktreeRoot"),
                "Frozen checkout no longer belongs to the configured repository/worktree root",
            )?;
            self.require_creation_path(&restore.path)?;
            source.path = Some(restore.path.clone());
        }
        ensure(
            !frozen.iter().any(|r| {
                r.value()["id"] != id
                    && r.value()["removal"]["repo"] == source.repo
                    && (r.value()["removal"]["currentBranch"] == source.branch
                        || source.path.as_ref().is_some_and(|p| {
                            inside(p, string(&r.value()["removal"], "path"))
                                || inside(string(&r.value()["removal"], "path"), p)
                        }))
            }),
            "Branch or path is reserved by a frozen task; use graftr thaw",
        )?;
        ensure(!records::adoptions(&self.paths.state)?.iter().any(|r| r.value()["repo"] == source.repo && (r.value()["branch"] == source.branch || source.path.as_ref().is_some_and(|p| inside(p, string(r.value(), "path")) || inside(string(r.value(), "path"), p)))), Error {code: "ADOPTED_EXTERNAL", message: "This worktree is tracked with external setup. Use graftr adopt PATH for explicit setup stages; automatic setup, cleanup, removal and freeze are not enabled for adopted worktrees. Existing files and manual services were preserved.".into()})?;
        if let Some(path) = &source.path {
            ensure(
                !paths::exists(Path::new(path))?,
                "Refusing to adopt or overwrite an existing checkout; use graftr setup inside it",
            )?;
        }
        if !source.fallback {
            let path = source.path.as_deref().unwrap();
            ensure(
                !records::tasks(&self.paths.state)?.iter().any(|t| {
                    inside(path, string(t.value(), "path"))
                        || inside(string(t.value(), "path"), path)
                }),
                "Creation path overlaps a managed checkout or retained record; inspect graftr list before creating another",
            )?;
            ensure(
                !inventory::registered(&source.repo)?.iter().any(|t| {
                    t.path == path
                        || t.path != source.repo && (inside(path, &t.path) || inside(&t.path, path))
                }),
                "Creation path overlaps an existing Git worktree registration; choose another path or review the inventory",
            )?;
        }
        let branch_commit = if options.branch.is_some() {
            ensure(
                !source.fallback,
                "Creating from an existing branch requires a configured repository and worktree root",
            )?;
            let commit = crate::branches::local_commit(&source.repo, &source.branch)?.ok_or_else(|| Error::from("Existing local branch not found; --branch never creates or fetches a branch"))?;
            ensure(
                options.restore.as_ref().is_none_or(|r| r.commit == commit),
                "Frozen branch moved since freezing; review it before thawing. No branch was reset",
            )?;
            ensure(
                !inventory::registered(&source.repo)?
                    .iter()
                    .any(|t| t.branch.as_deref() == Some(&source.branch)),
                "Branch is already checked out; use open or setup on its existing worktree",
            )?;
            ensure(
                !records::tasks(&self.paths.state)?.iter().any(|t| {
                    t.value()["repo"] == source.repo && t.value()["branch"] == source.branch
                }),
                "Branch is already claimed by a managed task; inspect graftr list before creating another",
            )?;
            Some(commit)
        } else {
            ensure(
                source.fallback
                    || system::run(
                        &[
                            "git",
                            "-C",
                            &source.repo,
                            "show-ref",
                            "--verify",
                            &format!("refs/heads/{}", source.branch),
                        ],
                        None,
                        false,
                    )?
                    .exit_code
                        != 0,
                "Branch already exists; use create --branch to explicitly reuse it",
            )?;
            None
        };
        let parent = if let Some(socket) = socket {
            Some(if source.fallback {
                source.workspace.clone().unwrap()
            } else {
                self.creation_parent(socket, &source.repo)?
            })
        } else {
            None
        };
        if let Some(path) = &source.path {
            self.require_creation_path(path)?;
        }
        stateio::creation_intent(self.paths, socket, &source.repo, &source.branch, true)?;
        let operation = || -> Result<SetupResult> {
            let mut options = options.clone();
            options.project = Some(project.into());
            options.name = Some(name.into());
            if socket.is_none() {
                ensure(
                    !source.fallback,
                    "Standalone creation requires a configured repository and worktree root",
                )?;
                let path = source.path.as_deref().unwrap();
                let revision = if options.branch.is_none() {
                    system::git(
                        Path::new(&source.repo),
                        &[
                            "rev-parse",
                            "--verify",
                            "--end-of-options",
                            &format!("{base}^{{commit}}"),
                        ],
                    )?
                } else {
                    source.branch.clone()
                };
                let mut args = vec!["git", "-C", &source.repo, "worktree", "add"];
                if options.branch.is_none() {
                    args.extend(["-b", &source.branch]);
                }
                args.extend(["--", path, &revision]);
                let started = Instant::now();
                let created = self
                    .progress
                    .run("create-checkout", &id, || system::run(&args, None, true));
                add(timing, "checkout", started);
                created?;
                ensure(
                    system::git(Path::new(path), &["symbolic-ref", "--quiet", "HEAD"])?
                        == format!("refs/heads/{}", source.branch)
                        && branch_commit.as_ref().is_none_or(|commit| {
                            system::git(Path::new(path), &["rev-parse", "HEAD^{commit}"])
                                .is_ok_and(|c| c == *commit)
                                && crate::branches::local_commit(&source.repo, &source.branch)
                                    .is_ok_and(|c| c.as_ref() == Some(commit))
                        }),
                    "Checkout branch changed during creation; keep it for review before setup",
                )?;
                let mut receipt = new_receipt(path);
                timing["checkoutReadyMs"] = json!(
                    (date_millis(&health::timestamp()).unwrap()
                        - date_millis(timing["startedAt"].as_str().unwrap()).unwrap())
                    .max(0)
                );
                receipt["timings"] = timing.clone();
                receipt["project"] = json!(project);
                self.save_setup(&receipt)?;
                if options.no_setup {
                    receipt["status"] = json!("skipped");
                    receipt["message"] = json!(
                        "Plain Git worktree; setup explicitly skipped. Run graftr setup inside it to enroll it later."
                    );
                    finish(timing);
                    receipt["timings"] = timing.clone();
                    self.save_setup(&receipt)?;
                    return Ok(SetupResult {
                        receipt,
                        task: None,
                        local: true,
                        log_id: None,
                        attention_needed: false,
                    });
                }
                return self.enroll(path, &options, timing.clone(), None, None, true);
            }
            let mut params = json!({"workspace_id": parent, "branch": source.branch, "label": name, "focus": false});
            if let Some(path) = &source.path {
                params["path"] = json!(path);
            }
            if options.branch.is_none() {
                params["base"] = json!(base);
            }
            let started = Instant::now();
            let reply = self.progress.run("create-checkout", &id, || {
                herdr::request(socket, "worktree.create", params, Duration::from_secs(60))
            });
            add(timing, "checkout", started);
            let opened = herdr::opened(&reply?)?;
            let path = opened
                .workspace
                .checkout_path
                .as_deref()
                .ok_or_else(|| Error::from("Herdr did not return a checkout path"))?;
            ensure(
                source.path.as_ref().is_none_or(|expected| expected == path),
                "Herdr returned a different checkout; refusing setup outside the requested path",
            )?;
            ensure(
                system::git(Path::new(path), &["symbolic-ref", "--quiet", "HEAD"])?
                    == format!("refs/heads/{}", source.branch),
                "Herdr checkout does not match the requested branch; checkout kept for review, no setup performed",
            )?;
            if let Some(commit) = &branch_commit {
                ensure(
                    system::git(Path::new(path), &["rev-parse", "HEAD^{commit}"])? == *commit
                        && crate::branches::local_commit(&source.repo, &source.branch)?.as_ref()
                            == Some(commit),
                    "Existing branch changed during creation; checkout kept for review, no setup performed",
                )?;
            }
            let mut receipt = new_receipt(path);
            receipt["socket"] = json!(socket);
            receipt["workspaceId"] = json!(opened.workspace.workspace_id);
            timing["checkoutReadyMs"] = json!(
                (date_millis(&health::timestamp()).unwrap()
                    - date_millis(timing["startedAt"].as_str().unwrap()).unwrap())
                .max(0)
            );
            receipt["timings"] = timing.clone();
            receipt["project"] = if source.fallback {
                Value::Null
            } else {
                json!(project)
            };
            self.save_setup(&receipt)?;
            if options.no_setup || source.fallback {
                receipt["status"] = json!(if options.no_setup {
                    "skipped"
                } else {
                    "unmanaged"
                });
                receipt["message"] = json!(if options.no_setup {
                    "Plain Herdr worktree; Graftr setup explicitly skipped. Run graftr setup to enroll it later."
                } else {
                    "Graftr creation settings unavailable; used Herdr's current repository and default path. Fix the project profile, then run graftr setup."
                });
                finish(timing);
                receipt["timings"] = timing.clone();
                self.save_setup(&receipt)?;
                return Ok(SetupResult {
                    receipt,
                    task: None,
                    local: false,
                    log_id: None,
                    attention_needed: false,
                });
            }
            let spaces = self.workspaces(socket.unwrap())?;
            let workspace = spaces
                .iter()
                .find(|w| w.workspace_id == opened.workspace.workspace_id)
                .filter(|w| w.checkout_path.is_some())
                .ok_or_else(|| Error::from("Workspace has no live worktree checkout"))?;
            self.enroll(
                workspace.checkout_path.as_deref().unwrap(),
                &options,
                timing.clone(),
                socket,
                Some(workspace),
                false,
            )
        };
        let mut operation = operation;
        let result = operation();
        let cleanup =
            stateio::creation_intent(self.paths, socket, &source.repo, &source.branch, false);
        cleanup?;
        result
    }
    fn creation_source(
        &self,
        project: &str,
        name: &str,
        socket: Option<&str>,
        existing: Option<&str>,
    ) -> Result<CreationSource> {
        let settings = match crate::config::settings(self.paths, project) {
            Ok(s) => s,
            Err(_) => return self.fallback_source(name, socket, existing),
        };
        for field in ["branchName", "worktreePath"] {
            if let Some(value) = settings.get(field) {
                profiles::creation_template(value, field)?;
            }
        }
        let paths = || -> Result<(String, String)> {
            let repo = text(settings.get("repo").unwrap_or(&Value::Null))?;
            let root = text(settings.get("worktreeRoot").unwrap_or(&Value::Null))?;
            for path in [repo, root] {
                ensure(
                    absolute_normalized(path) && checkout::canonical(path)? == path,
                    "Invalid creation path",
                )?;
            }
            Ok((repo.into(), root.into()))
        };
        let (repo, root) = match paths() {
            Ok(p) => p,
            Err(_)
                if settings.contains_key("branchName") || settings.contains_key("worktreePath") =>
            {
                return Err(invalid_creation(
                    "Explicit creation templates require absolute canonical repo and worktreeRoot paths; no native fallback was used",
                ));
            }
            Err(_) => return self.fallback_source(name, socket, existing),
        };
        let (branch, path) =
            crate::naming::creation(&settings, project, name, &repo, &root, existing)?;
        if let Err(error) = crate::branches::validate(&branch) {
            return Err(if existing.is_some() {
                error
            } else {
                invalid_creation(format!(
                    "branchName did not render a valid local Git branch. {}",
                    error.message
                ))
            });
        }
        self.require_creation_path(&path)?;
        Ok(CreationSource {
            repo,
            branch,
            path: Some(path),
            workspace: None,
            fallback: false,
        })
    }
    fn fallback_source(
        &self,
        name: &str,
        socket: Option<&str>,
        existing: Option<&str>,
    ) -> Result<CreationSource> {
        let socket = socket.ok_or_else(|| Error::from("Standalone creation requires configured absolute canonical repo/worktreeRoot paths; no Herdr fallback is used"))?;
        let branch = existing
            .map(str::to_owned)
            .unwrap_or_else(|| format!("task/{name}"));
        crate::branches::validate(&branch)?;
        let workspace = herdr::caller_workspace(Some(socket))?.ok_or_else(|| Error::from("Graftr creation settings unavailable; run from a Herdr repository pane to use native creation defaults"))?;
        let spaces = self.workspaces(socket)?;
        let source = spaces
            .iter()
            .find(|w| w.workspace_id == workspace)
            .ok_or_else(|| Error::from("Calling Herdr workspace is no longer open"))?;
        let repo = if let Some(path) = &source.checkout_path {
            inventory::registered(path)?
                .first()
                .ok_or_else(|| Error::from("Caller repository could not be identified"))?
                .path
                .clone()
        } else {
            format!("workspace:{workspace}")
        };
        Ok(CreationSource {
            repo,
            branch,
            path: None,
            workspace: Some(workspace),
            fallback: true,
        })
    }
    fn require_creation_path(&self, path: &str) -> Result<()> {
        ensure(
            checkout::canonical(path).is_ok_and(|p| p == path),
            invalid_creation(
                "Rendered creation path must be canonical; symlinked or unreadable destinations/parent directories are not used",
            ),
        )
    }
    fn parent(&self, socket: &str, repo: &str) -> Result<String> {
        let matches: Vec<_> = self
            .workspaces(socket)?
            .into_iter()
            .filter(|w| w.checkout_path.as_deref() == Some(repo))
            .collect();
        ensure(
            matches.len() == 1,
            "Open exactly one parent repository workspace in this Herdr server",
        )?;
        Ok(matches[0].workspace_id.clone())
    }
    fn creation_parent(&self, socket: &str, repo: &str) -> Result<String> {
        let matches: Vec<_> = self
            .workspaces(socket)?
            .into_iter()
            .filter(|w| w.checkout_path.as_deref() == Some(repo))
            .collect();
        if matches.len() == 1 {
            return Ok(matches[0].workspace_id.clone());
        }
        ensure(
            matches.len() <= 1,
            format!(
                "Multiple parent repository workspaces are associated with {repo}; inspect them before creating a worktree"
            ),
        )?;
        let workspace = herdr::caller_workspace(Some(socket))?.ok_or_else(|| Error::from(format!("Open the configured parent repository {repo} in a Herdr workspace before creating a worktree")))?;
        let source = herdr::request(
            Some(socket),
            "worktree.list",
            json!({"workspace_id": workspace}),
            Duration::from_secs(60),
        )?;
        object(&source["source"])?;
        let source = &source["source"];
        ensure(
            source["source_workspace_id"] == workspace
                && source["repo_root"] == repo
                && source["source_checkout_path"] == repo,
            format!(
                "Calling workspace is not the configured parent repository {repo}; run creation from that repository's existing workspace"
            ),
        )?;
        let reply = self.progress.run("register-parent", repo, || {
            herdr::request(
                Some(socket),
                "worktree.open",
                json!({"workspace_id": workspace, "path": repo, "focus": false}),
                Duration::from_secs(60),
            )
        })?;
        let opened = herdr::opened(&reply)?;
        ensure(
            opened.already_open
                && opened.workspace.workspace_id == workspace
                && opened.workspace.checkout_path.as_deref() == Some(repo),
            "Herdr did not reuse the calling parent repository workspace; inspect the workspace association before retrying",
        )?;
        ensure(
            self.parent(socket, repo)? == workspace,
            "Parent repository workspace changed during registration; inspect the association before retrying",
        )?;
        Ok(workspace)
    }
    pub fn open(&self, id: &str, options: &Options) -> Result<SetupResult> {
        let mut timing = timings("open")?;
        let initial = records::task(&self.paths.state, id)?;
        let socket = self.integration_socket(options, initial.value()["socket"].as_str())?;
        if socket.is_some() && initial.value()["socket"].is_null() {
            self.wait_for_standalone_setup(string(initial.value(), "path"))?;
        }
        let waiting = Instant::now();
        let result = {
            let _lock = locks::state(self.paths)?;
            add(&mut timing, "stateLockWait", waiting);
            self.open_locked(
                records::task(&self.paths.state, id)?.value().clone(),
                options,
                timing,
                socket.as_deref(),
            )?
        };
        self.finish_request(result, options)
    }
    fn open_locked(
        &self,
        mut task: Value,
        options: &Options,
        timing: Value,
        socket: Option<&str>,
    ) -> Result<SetupResult> {
        checkout::verify(&task)?;
        let Some(socket) = socket else {
            return self.enroll(string(&task, "path"), options, timing, None, None, true);
        };
        if task["socket"].is_null() {
            let parent = self.creation_parent(socket, string(&task, "repo"))?;
            let profile = profiles::configured(self.paths, string(&task, "project"))?;
            let label =
                crate::labels::worktree_label(&task, profile.value()["workspaceLabel"].as_str())?;
            let reply = self.progress.run("open-workspace", string(&task, "id"), || herdr::request(Some(socket), "worktree.open", json!({"workspace_id": parent, "path": task["path"], "label": label, "focus": false}), Duration::from_secs(60)))?;
            let opened = herdr::opened(&reply)?;
            ensure(
                opened.workspace.checkout_path.as_deref() == task["path"].as_str(),
                "Herdr returned a different checkout; attachment was not enrolled",
            )?;
            return self.enroll(
                string(&task, "path"),
                options,
                timing,
                Some(socket),
                Some(&opened.workspace),
                false,
            );
        }
        if self.workspace(&task)?.is_none() {
            let label = profiles::configured(self.paths, string(&task, "project"))
                .and_then(|p| {
                    crate::labels::worktree_label(&task, p.value()["workspaceLabel"].as_str())
                })
                .ok();
            let parent = self.parent(string(&task, "socket"), string(&task, "repo"))?;
            let mut params = json!({"workspace_id": parent, "path": task["path"], "focus": false});
            if let Some(label) = label {
                params["label"] = json!(label);
            }
            let reply = self
                .progress
                .run("open-workspace", string(&task, "id"), || {
                    herdr::request(
                        task["socket"].as_str(),
                        "worktree.open",
                        params,
                        Duration::from_secs(60),
                    )
                })?;
            let opened = herdr::opened(&reply)?;
            ensure(
                opened.workspace.checkout_path.as_deref() == task["path"].as_str(),
                "Herdr returned an unexpected checkout",
            )?;
            task["workspaceId"] = json!(opened.workspace.workspace_id);
            self.save_task(&task)?;
        }
        ensure(!task["workspaceId"].is_null(), "Task has no live workspace")?;
        let spaces = self.workspaces(string(&task, "socket"))?;
        let workspace = spaces
            .iter()
            .find(|w| Some(w.workspace_id.as_str()) == task["workspaceId"].as_str())
            .filter(|w| w.checkout_path.is_some())
            .ok_or_else(|| Error::from("Workspace has no live worktree checkout"))?;
        self.enroll(
            workspace.checkout_path.as_deref().unwrap(),
            options,
            timing,
            task["socket"].as_str(),
            Some(workspace),
            false,
        )
    }
    pub(crate) fn save_task(&self, task: &Value) -> Result<()> {
        self.progress.run("save-task", string(task, "id"), || {
            stateio::write_task(self.paths, task)
        })
    }
    fn save_setup(&self, receipt: &Value) -> Result<()> {
        self.progress.run(
            "save-setup",
            receipt["taskId"]
                .as_str()
                .unwrap_or(string(receipt, "path")),
            || stateio::write_setup(self.paths, receipt),
        )
    }
    fn profile(&self, project: &str) -> Result<Value> {
        let profile = profiles::configured(self.paths, project)?.value().clone();
        for key in ["repo", "worktreeRoot", "envSource"] {
            if let Some(path) = profile[key].as_str() {
                ensure(
                    absolute_normalized(path) && checkout::canonical(path)? == path,
                    "Profile paths must be absolute and canonical",
                )?;
            }
        }
        ensure(
            profile["envSource"]
                .as_str()
                .is_none_or(|p| Path::new(p).file_name().is_some_and(|n| n == ".env")),
            "Use an explicitly approved development .env baseline, never .env.prod",
        )?;
        Ok(profile)
    }
    fn integration_socket(
        &self,
        options: &Options,
        recorded: Option<&str>,
    ) -> Result<Option<String>> {
        if options.no_herdr {
            ensure(
                !herdr::in_plugin_action(),
                "Herdr plugin actions require their selected workspace; run standalone preparation from a checkout terminal instead",
            )?;
            return Ok(None);
        }
        let mode = profiles::herdr_mode(self.paths)?;
        if let Some(socket) = &options.herdr_socket {
            ensure(
                mode != "off",
                "Scheduled thaw requires Herdr integration; it will not fall back to standalone setup",
            )?;
            herdr::workspaces(&herdr::request(
                Some(socket),
                "workspace.list",
                json!({}),
                Duration::from_millis(1500),
            )?)?;
            return Ok(Some(socket.clone()));
        }
        if mode == "off" {
            ensure(
                !herdr::in_plugin_action(),
                "Herdr integration is off; run preparation from a checkout terminal instead",
            )?;
            return Ok(None);
        }
        if env::var("HERDR_ENV").as_deref() != Ok("1")
            || env::var("HERDR_SOCKET_PATH")
                .ok()
                .is_none_or(|p| p.is_empty())
        {
            ensure(mode != "required" && !herdr::in_plugin_action(), Error {code: "HERDR_REQUIRED", message: "Herdr integration requires a connected Herdr context; run from Herdr or select auto/off in configuration".into()})?;
            return Ok(None);
        }
        let socket = recorded
            .map(str::to_owned)
            .unwrap_or(herdr::caller_socket()?);
        match herdr::request(
            Some(&socket),
            "workspace.list",
            json!({}),
            Duration::from_millis(1500),
        )
        .and_then(|r| herdr::workspaces(&r))
        {
            Err(e)
                if mode != "required"
                    && !herdr::in_plugin_action()
                    && ["HERDR_UNAVAILABLE", "HERDR_CONNECTION_UNAVAILABLE"].contains(&e.code) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
            Ok(_) => Ok(Some(socket)),
        }
    }
    pub fn setup(&self, options: &Options) -> Result<SetupResult> {
        let socket = self.integration_socket(options, None)?;
        if let Some(socket) = socket {
            let workspace_id = herdr::caller_workspace(Some(&socket))?.ok_or_else(|| {
                Error::from("Run setup in a Herdr worktree; no focused-workspace fallback is used")
            })?;
            if herdr::in_plugin_action() {
                let context: Value = serde_json::from_str(&env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or("{}".into())).map_err(|_| Error {code: "INVALID_JSON", message: "Invalid JSON; review configuration/state or the Herdr response (contents were not logged)".into()})?;
                object(&context)?;
                if context["invocation_source"] == "graftr-queued-setup" {
                    return self.finish_queued(
                        &socket,
                        &workspace_id,
                        text(&context["correlation_id"])?,
                    );
                }
            }
            self.wait_for_standalone_workspace(&socket, &workspace_id)?;
            let mut timing = timings("setup")?;
            let waiting = Instant::now();
            let queued = {
                let _lock = locks::state(self.paths)?;
                add(&mut timing, "stateLockWait", waiting);
                let spaces = self.workspaces(&socket)?;
                let workspace = spaces
                    .iter()
                    .find(|w| w.workspace_id == workspace_id)
                    .filter(|w| w.checkout_path.is_some())
                    .ok_or_else(|| Error::from("Workspace has no live worktree checkout"))?;
                self.enroll(
                    workspace.checkout_path.as_deref().unwrap(),
                    options,
                    timing,
                    Some(&socket),
                    Some(workspace),
                    false,
                )?
            };
            return self.finish_request(queued, options);
        }
        let selected = targets::resolve_managed(self.paths, None, None).map_err(|e| Error {
            code: e.code,
            message: e.message,
        })?;
        let path = if let Some(task) = selected {
            string(task.value(), "path").to_owned()
        } else {
            let cwd =
                env::current_dir().map_err(|_| Error::from("Current directory is unavailable"))?;
            let cwd = fs::canonicalize(cwd)
                .map_err(|_| Error::from("Current directory is unavailable"))?;
            system::git(&cwd, &["rev-parse", "--show-toplevel"])?
        };
        let mut timing = timings("setup")?;
        let waiting = Instant::now();
        let queued = {
            let _lock = locks::state(self.paths)?;
            add(&mut timing, "stateLockWait", waiting);
            self.enroll(&path, options, timing, None, None, true)?
        };
        self.run(queued)
    }
    pub(crate) fn setup_workspace(
        &self,
        socket: &str,
        workspace_id: &str,
        options: &Options,
    ) -> Result<SetupResult> {
        let mut timing = timings("setup")?;
        self.wait_for_standalone_workspace(socket, workspace_id)?;
        let waiting = Instant::now();
        let result = {
            let _state = locks::state(self.paths)?;
            add(&mut timing, "stateLockWait", waiting);
            let spaces = self.workspaces(socket)?;
            let workspace = spaces
                .iter()
                .find(|w| w.workspace_id == workspace_id)
                .filter(|w| w.checkout_path.is_some())
                .ok_or_else(|| Error::from("Workspace has no live worktree checkout"))?;
            self.enroll(
                workspace.checkout_path.as_deref().unwrap(),
                options,
                timing,
                Some(socket),
                Some(workspace),
                false,
            )?
        };
        self.run(result)
    }
    pub(crate) fn workspaces(&self, socket: &str) -> Result<Vec<herdr::Workspace>> {
        herdr::workspaces(&herdr::request(
            Some(socket),
            "workspace.list",
            json!({}),
            Duration::from_secs(60),
        )?)
    }
    pub(crate) fn workspace(&self, task: &Value) -> Result<Option<herdr::Workspace>> {
        let Some(socket) = task["socket"]
            .as_str()
            .filter(|_| !task["workspaceId"].is_null())
        else {
            return Ok(None);
        };
        let found = self
            .workspaces(socket)?
            .into_iter()
            .find(|w| Some(w.workspace_id.as_str()) == task["workspaceId"].as_str());
        ensure(
            found
                .as_ref()
                .is_none_or(|w| w.checkout_path.as_deref() == task["path"].as_str()),
            "Workspace identity does not match the owned checkout",
        )?;
        Ok(found)
    }
    fn label_workspace(&self, task: &Value, workspace: &herdr::Workspace, profile: &Value) {
        if let Ok(label) = crate::labels::worktree_label(task, profile["workspaceLabel"].as_str())
            && label != workspace.label
        {
            let _ = herdr::request(
                task["socket"].as_str(),
                "workspace.rename",
                json!({"workspace_id": workspace.workspace_id, "label": label}),
                Duration::from_secs(60),
            );
        }
    }
    fn wait_for_standalone_setup(&self, path: &str) -> Result<()> {
        if records::setup_for(&self.paths.state, None, path)?
            .is_none_or(|r| r.value()["status"] != "pending")
        {
            return Ok(());
        }
        let _lock = locks::worktree(self.paths, path, false)?;
        ensure(
            records::setup_for(&self.paths.state, None, path)?
                .is_none_or(|r| r.value()["status"] != "pending"),
            "Standalone setup remains pending without a completed worker; inspect it and retry explicit local setup before attaching",
        )
    }
    fn wait_for_standalone_workspace(&self, socket: &str, workspace: &str) -> Result<()> {
        let unattached: Vec<_> = records::tasks(&self.paths.state)?
            .into_iter()
            .filter(|t| t.value()["socket"].is_null())
            .collect();
        if unattached.is_empty() {
            return Ok(());
        }
        let spaces = self.workspaces(socket)?;
        if let Some(space) = spaces.iter().find(|w| w.workspace_id == workspace)
            && let Some(task) = unattached
                .iter()
                .find(|t| t.value()["path"].as_str() == space.checkout_path.as_deref())
        {
            self.wait_for_standalone_setup(string(task.value(), "path"))?;
        }
        Ok(())
    }
    fn latest_result(&self, result: &SetupResult) -> Result<SetupResult> {
        let mut latest = result.clone();
        latest.task = if let Some(task) = &result.task
            && paths::exists(
                &self
                    .paths
                    .state
                    .join(format!("{}.json", string(task, "id"))),
            )? {
            Some(
                records::task(&self.paths.state, string(task, "id"))?
                    .value()
                    .clone(),
            )
        } else {
            None
        };
        if let Some(receipt) = records::setup_for(
            &self.paths.state,
            result.receipt["socket"].as_str(),
            string(&result.receipt, "path"),
        )? {
            latest.receipt = receipt.value().clone();
        } else {
            latest.receipt["runId"] = Value::Null;
            latest.receipt["status"] = json!("incomplete");
            latest.receipt["message"] =
                json!("Setup cancelled; checkout/record removed. No resources restored.");
        }
        Ok(latest)
    }
    fn finish_queued(&self, socket: &str, workspace: &str, run_id: &str) -> Result<SetupResult> {
        let waiting = Instant::now();
        let result = {
            let _lock = locks::state(self.paths)?;
            let tasks: Vec<_> = records::tasks(&self.paths.state)?
                .into_iter()
                .filter(|t| t.value()["socket"] == socket && t.value()["workspaceId"] == workspace)
                .collect();
            ensure(
                tasks.len() == 1,
                "Queued setup no longer owns this workspace; no resources restored",
            )?;
            let task = tasks[0].value().clone();
            let mut receipt = records::setup_for(
                &self.paths.state,
                task["socket"].as_str(),
                string(&task, "path"),
            )?
            .filter(|r| r.value()["workspaceId"] == workspace)
            .ok_or_else(|| Error::from("Queued setup receipt no longer owns this workspace"))?
            .value()
            .clone();
            if receipt["runId"] != run_id {
                receipt["status"] = json!("incomplete");
                receipt["message"] =
                    json!("Setup request cancelled or superseded; no resources restored.");
            } else {
                add_receipt_timing(&mut receipt, "stateLockWait", waiting);
            }
            SetupResult {
                task: Some(task),
                receipt,
                local: false,
                log_id: None,
                attention_needed: false,
            }
        };
        self.run(result)
    }
    fn finish_request(&self, mut result: SetupResult, options: &Options) -> Result<SetupResult> {
        if result.task.is_none() || result.receipt["status"] != "pending" {
            return Ok(result);
        }
        if result.local
            || result.task.as_ref().unwrap()["socket"].is_null()
            || options.wait
            || herdr::in_plugin_action()
        {
            return self.run(result);
        }
        let task = result.task.as_ref().unwrap();
        let dispatched = self.progress.run("dispatch-setup", string(task, "id"), || {
            herdr::request(task["socket"].as_str(), "plugin.action.invoke", json!({"plugin_id": "local.graftr", "action_id": "setup", "context": {"workspace_id": result.receipt["workspaceId"], "invocation_source": "graftr-queued-setup", "correlation_id": result.receipt["runId"]}}), Duration::from_secs(60))
        }).and_then(|reply| {
            object(&reply["log"])?; ensure(reply["type"] == "plugin_action_invoked" && [Some("running"), Some("succeeded")].contains(&reply["log"]["status"].as_str()), "Herdr did not accept the setup worker")?; Ok(text(&reply["log"]["log_id"])?.into())
        });
        match dispatched {
            Ok(log) => {
                result.log_id = Some(log);
                Ok(result)
            }
            Err(error) => {
                let _lock = locks::state(self.paths)?;
                let mut latest = self.latest_result(&result)?;
                if latest.receipt["runId"] != result.receipt["runId"]
                    || latest.receipt["status"] != "pending"
                {
                    return Ok(latest);
                }
                latest.receipt["status"] = json!("incomplete");
                latest.receipt["runId"] = Value::Null;
                latest.receipt["message"] = json!(format!(
                    "Setup dispatch failed: {}. Checkout kept; enable the Graftr plugin and retry graftr setup, or use graftr setup --wait.",
                    error.message
                ));
                finish_receipt_timing(&mut latest.receipt);
                self.save_setup(&latest.receipt)?;
                latest.attention_needed = true;
                Ok(latest)
            }
        }
    }
    pub(crate) fn cancel_setup(&self, task: &Value, reason: &str) -> Result<()> {
        let Some(mut receipt) = records::setup_for(
            &self.paths.state,
            task["socket"].as_str(),
            string(task, "path"),
        )?
        .map(|r| r.value().clone())
        .filter(|r| r["workspaceId"] == task["workspaceId"] && r["status"] == "pending") else {
            return Ok(());
        };
        receipt["runId"] = Value::Null;
        receipt["status"] = json!("incomplete");
        receipt["message"] = json!(format!(
            "{reason}; checkout kept. An active installer may finish, but cannot restore tabs or publish readiness. Reopen/retry setup when needed."
        ));
        finish_receipt_timing(&mut receipt);
        self.save_setup(&receipt)
    }
    pub fn report_setup(&self, result: &SetupResult) {
        if result.local
            || !result.attention_needed
            || result.receipt["status"] != "incomplete"
            || result.task.as_ref().is_none_or(|t| t["socket"].is_null())
        {
            return;
        }
        let operation = || -> Result<()> {
            let original = result.task.as_ref().unwrap();
            let task = records::task(&self.paths.state, string(original, "id"))?;
            let task = task.value();
            let current = records::setup_for(
                &self.paths.state,
                result.receipt["socket"].as_str(),
                string(&result.receipt, "path"),
            )?;
            if current.as_ref().is_none_or(|r| {
                ["status", "runId", "workspaceId", "step", "message"]
                    .iter()
                    .any(|key| r.value()[key] != result.receipt[key])
            }) || task["path"] != result.receipt["path"]
                || task["workspaceId"] != result.receipt["workspaceId"]
                || [Some("closed"), Some("cleaned")].contains(&task["status"].as_str())
                || self.workspace(task)?.is_none()
            {
                return Ok(());
            }
            herdr::request(
                task["socket"].as_str(),
                "notification.show",
                json!({"title": "Worktree setup needs attention", "body": format!("{}/{}: {} failed. Check graftr status; retry with graftr setup.", string(task, "project"), Path::new(string(&result.receipt, "path")).file_name().unwrap_or_default().to_string_lossy(), string(&result.receipt, "step"))}),
                Duration::from_secs(60),
            )?;
            Ok(())
        };
        let _ = operation();
    }
    fn enroll(
        &self,
        path: &str,
        options: &Options,
        timing: Value,
        socket: Option<&str>,
        workspace: Option<&herdr::Workspace>,
        local: bool,
    ) -> Result<SetupResult> {
        let started = Instant::now();
        ensure(!records::adoptions(&self.paths.state)?.iter().any(|a| a.value()["path"] == path), Error {code: "ADOPTED_EXTERNAL", message: "This worktree is tracked with external setup. Use graftr adopt PATH for explicit setup stages; automatic setup, cleanup, removal and freeze are not enabled for adopted worktrees. Existing files and manual services were preserved.".into()})?;
        ensure(
            options
                .expected_path
                .as_ref()
                .is_none_or(|expected| expected == path),
            "Worktree event no longer owns this workspace's checkout; refusing stale setup",
        )?;
        let matching: Vec<_> = records::tasks(&self.paths.state)?
            .into_iter()
            .filter(|t| t.value()["path"] == path)
            .collect();
        if matching.len() > 1
            || socket.is_some()
                && matching.iter().any(|t| {
                    t.value()["socket"]
                        .as_str()
                        .is_some_and(|s| Some(s) != socket)
                })
        {
            let mut receipt = new_receipt(path);
            receipt["socket"] = json!(socket);
            receipt["workspaceId"] = json!(workspace.map(|w| w.workspace_id.as_str()));
            receipt["status"] = json!("unmanaged");
            receipt["message"] = json!(
                "Checkout is claimed by another task/server; review ownership first. Existing records, receipt and files were not changed."
            );
            return Ok(SetupResult {
                receipt,
                task: None,
                local,
                log_id: None,
                attention_needed: false,
            });
        }
        let receipts: Vec<_> = records::setups(&self.paths.state)?
            .into_iter()
            .filter(|r| r.value()["path"] == path)
            .collect();
        ensure(
            receipts.len() <= 1,
            "Multiple setup receipt lineages claim this checkout; review them before setup",
        )?;
        let mut result = SetupResult {
            task: matching.first().map(|t| t.value().clone()),
            receipt: receipts
                .first()
                .map_or_else(|| new_receipt(path), |r| r.value().clone()),
            local,
            log_id: None,
            attention_needed: false,
        };
        let workspace_id = workspace.map(|w| json!(w.workspace_id)).unwrap_or_else(|| {
            result
                .task
                .as_ref()
                .map_or(Value::Null, |t| t["workspaceId"].clone())
        });
        if receipts.is_empty() {
            result.receipt["workspaceId"] = workspace_id.clone();
        }
        if result.task.as_ref().is_some_and(|t| {
            [Some("queued"), Some("running"), Some("failed")]
                .contains(&t["cleanup"]["status"].as_str())
        }) {
            result.receipt["status"] = json!("incomplete");
            result.receipt["message"] = json!(
                "Cleanup is pending, failed or interrupted; inspect effects and finish an explicit cleanup retry before setup/open. Existing ownership was not changed."
            );
            return Ok(result);
        }
        if !local
            && result.task.as_ref().is_some_and(|t| t["socket"].is_null())
            && result.receipt["status"] == "pending"
        {
            result.receipt["status"] = json!("incomplete");
            result.receipt["message"] = json!(
                "Standalone setup is running/pending; wait for it before attaching this workspace, then retry setup. The original setup attempt is unchanged."
            );
            return Ok(result);
        }
        if options.automatic && result.receipt["status"] == "skipped" {
            result.task = None;
            return Ok(result);
        }
        let mut prepare = || -> Result<Option<(Value, Value)>> {
            ensure(
                absolute_normalized(path) && checkout::canonical(path)? == path,
                "Setup requires a canonical checkout path, not a symlink",
            )?;
            let trees = inventory::registered(path)?;
            let repo = trees.first().ok_or_else(|| {
                Error::from(
                    "Only registered linked worktrees with an attached branch can be enrolled",
                )
            })?;
            let tree = trees.iter().find(|t| t.path == path);
            ensure(
                path != repo.path
                    && tree.is_some_and(|t| !t.bare && !t.detached && t.branch.is_some()),
                "Only registered linked worktrees with an attached branch can be enrolled",
            )?;
            let tree = tree.unwrap();
            ensure(
                options
                    .expected_repo
                    .as_ref()
                    .is_none_or(|expected| *expected == repo.path)
                    && options
                        .expected_branch
                        .as_ref()
                        .is_none_or(|expected| Some(expected.as_str()) == tree.branch.as_deref()),
                "Worktree event repository/branch no longer matches; retry setup explicitly",
            )?;
            let project = profiles::for_repo(self.paths, &repo.path)?;
            ensure(
                project
                    .as_ref()
                    .is_some_and(|(name, _)| options.project.as_ref().is_none_or(|p| p == name)),
                "No matching project profile; configure this repository before retrying graftr setup",
            )?;
            let (project, settings) = project.unwrap();
            result.receipt["project"] = json!(project);
            if options.automatic
                && result.task.is_none()
                && settings.get("autoSetup") != Some(&json!(true))
            {
                result.receipt["status"] = json!("unmanaged");
                result.receipt["message"] = json!(
                    "Plain Herdr worktree; automatic setup is not enabled for this project. Run graftr setup to enroll it."
                );
                return Ok(None);
            }
            if let Some(task) = &result.task {
                ensure(
                    task["repo"] == repo.path
                        && task["project"] == project
                        && task["branch"].as_str() == tree.branch.as_deref(),
                    "Recorded repository/project/branch no longer matches this checkout",
                )?;
            }
            let profile = self.profile(&project)?;
            if !local
                && let Some(task) = &result.task
                && task["workspaceId"] != workspace_id
                && self.workspace(task)?.is_some()
            {
                return Err("Task is still open in another owned workspace; close it first".into());
            }
            if let Some(task) = &result.task {
                addresses::verify(&profile, task)?;
                if task.get("cleanup").is_some() && task["cleanup"]["status"] != "complete" {
                    let configured = configuration(&profile, task)?;
                    ensure(
                        task["cleanup"]["hooks"]
                            == profile["hooks"]
                                .get("cleanup")
                                .cloned()
                                .unwrap_or(json!([]))
                            && task["cleanup"]["context"] == configured.1,
                        "Cleanup configuration changed while its resources remain owned; finish the recorded cleanup before changing hooks or environment context",
                    )?;
                }
            }
            let mut task = if let Some(task) = &result.task {
                task.clone()
            } else {
                result.receipt["step"] = json!("enrollment");
                let prefix = pattern!(r"[^a-z0-9-]+")
                    .replace_all(
                        &Path::new(path)
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
                            .to_lowercase(),
                        "-",
                    )
                    .to_string();
                let prefix = pattern!(r"-+")
                    .replace_all(&prefix, "-")
                    .trim_matches('-')
                    .chars()
                    .take(30)
                    .collect::<String>()
                    .trim_end_matches('-')
                    .to_owned();
                let name = options.name.clone().unwrap_or_else(|| {
                    format!(
                        "{}-{}",
                        if prefix.is_empty() {
                            "worktree"
                        } else {
                            &prefix
                        },
                        &format!(
                            "{:x}",
                            Sha256::digest(serde_json::to_vec(&json!([path])).unwrap())
                        )[..8]
                    )
                });
                crate::validation::name(&name)?;
                let id = format!("{project}--{name}");
                ensure(
                    !records::frozen(&self.paths.state)?.iter().any(|f| {
                        f.reserves()
                            && (f.value()["id"] == id
                                || f.value()["removal"]["repo"] == repo.path
                                    && (f.value()["removal"]["path"] == path
                                        || f.value()["removal"]["currentBranch"].as_str()
                                            == tree.branch.as_deref()))
                            && (f.value()["id"] != id
                                || f.value()["status"] != "thawing"
                                || options.restore.as_ref().is_none_or(|restore| {
                                    f.value()["generation"] != restore.generation
                                }))
                    }),
                    "Checkout is reserved by a frozen task; use graftr thaw instead of enrolling it separately",
                )?;
                ensure(
                    !paths::exists(&self.paths.state.join(format!("{id}.json")))?,
                    "Task name is already owned by another checkout",
                )?;
                let mut task = json!({"id": id, "project": project, "path": path, "branch": tree.branch, "repo": repo.path, "baseCommit": tree.head.clone().unwrap_or(system::git(Path::new(path), &["rev-parse", "HEAD"])?), "socket": socket, "workspaceId": workspace_id, "tabs": {}, "status": "preparing"});
                let address = addresses::allocate(self.paths, &profile, &task)?;
                for (key, value) in address.as_object().unwrap() {
                    task[key] = value.clone();
                }
                let (templates, overrides) = configuration(&profile, &task)?;
                task["envOverrides"] = templates;
                task["overrides"] = overrides;
                task
            };
            if workspace.is_some() {
                task["socket"] = json!(socket);
                task["workspaceId"] = workspace_id.clone();
            }
            Ok(Some((task, profile)))
        };
        let prepared = prepare();
        let mut timing = timing;
        let enrolled = prepared.and_then(|prepared| -> Result<()> {
        match prepared {
            Some((mut task, profile)) => {
                result.task = Some(task.clone());
                if result.receipt["status"] == "pending"
                    && result.receipt["runId"].as_str().is_some()
                    && result.receipt["workspaceId"] == workspace_id
                    && options.skip_install.is_none()
                {
                    if let Some(workspace) = workspace {
                        self.label_workspace(&task, workspace, &profile);
                    }
                    result.task = Some(task);
                    return Ok(());
                }
                task["status"] = json!("preparing");
                result.receipt["workspaceId"] = workspace_id.clone();
                result.receipt["taskId"] = task["id"].clone();
                result.receipt["status"] = json!("pending");
                result.receipt["step"] = json!("profile");
                result.receipt["runId"] = json!(system::uuid()?);
                let (_, overrides) = configuration(&profile, &task)?;
                if let Some(cleanup) = profile["hooks"]["cleanup"]
                    .as_array()
                    .filter(|h| !h.is_empty())
                {
                    if task.get("cleanup").is_none() || task["cleanup"]["status"] == "complete" {
                        task["cleanup"] = json!({"runId": result.receipt["runId"], "attempt": 0, "hooks": cleanup, "context": overrides, "status": "armed", "results": []});
                    }
                } else {
                    task.as_object_mut().unwrap().remove("cleanup");
                }
                if let Some(retire) = profile["hooks"]["retire"]
                    .as_array()
                    .filter(|h| !h.is_empty())
                    && task.get("retire").is_none()
                {
                    task["retire"] = json!({"runId": system::uuid()?, "attempt": 0, "hooks": retire, "context": overrides, "status": "armed", "results": []});
                }
                result.task = Some(task.clone());
                self.save_task(&task)?;
                result.receipt.as_object_mut().unwrap().remove("hooks");
                add(&mut timing, "enrollment", started);
                result.receipt["timings"] = timing.clone();
                result.receipt["message"] = json!(if local {
                    "Checkout available; standalone setup is preparing synchronously."
                } else {
                    "Checkout available; setup queued in Herdr. Check graftr status or the plugin logs for completion."
                });
                if let Some(skip) = options.skip_install {
                    result.receipt["skipInstall"] = json!(skip);
                    if !skip {
                        result.receipt["installKey"] = Value::Null;
                    }
                }
                self.save_setup(&result.receipt)?;
                if let Some(workspace) = workspace {
                    self.label_workspace(result.task.as_ref().unwrap(), workspace, &profile);
                }
            }
            None => {
                add(&mut timing, "enrollment", started);
                finish(&mut timing);
                result.receipt["timings"] = timing.clone();
                self.save_setup(&result.receipt)?;
                result.task = None;
            }
        }
        Ok(())
        });
        if let Err(error) = enrolled {
            add(&mut timing, "enrollment", started);
            finish(&mut timing);
            result.receipt["timings"] = timing;
            result.receipt["status"] = json!(if result.task.is_some() {
                "incomplete"
            } else {
                "unmanaged"
            });
            result.receipt["workspaceId"] = workspace_id;
            result.receipt["runId"] = Value::Null;
            result.receipt["message"] = json!(format!(
                "{}: {}. Plain checkout kept; fix the configuration and run graftr setup.",
                string(&result.receipt, "step"),
                error.message
            ));
            if !options.automatic || !result.receipt["project"].is_null() {
                self.save_setup(&result.receipt)?;
            }
            result.attention_needed = result.task.is_some();
        }
        Ok(result)
    }
    fn step(
        &self,
        result: &mut SetupResult,
        step: &str,
        operation: impl FnOnce(&mut Value, &mut Value) -> Result<()>,
    ) -> Result<()> {
        let waiting = Instant::now();
        let _lock = locks::state(self.paths)?;
        add_receipt_timing(&mut result.receipt, "stateLockWait", waiting);
        let expected = result.task.as_ref().unwrap();
        let mut latest = records::task(&self.paths.state, string(expected, "id"))?
            .value()
            .clone();
        let receipt = records::setup_for(
            &self.paths.state,
            result.receipt["socket"].as_str(),
            string(&result.receipt, "path"),
        )?;
        ensure(
            receipt.as_ref().is_some_and(|r| {
                r.value()["runId"] == result.receipt["runId"] && r.value()["status"] == "pending"
            }) && latest["workspaceId"] == result.receipt["workspaceId"]
                && latest["socket"] == expected["socket"]
                && latest["status"] == "preparing",
            "Setup superseded or cancelled",
        )?;
        if !result.local && !latest["socket"].is_null() && self.workspace(&latest)?.is_none() {
            latest["status"] = json!("closed");
            self.save_task(&latest)?;
            self.cancel_setup(&latest, "Workspace closed during setup")?;
            if latest["cleanup"]["status"] == "armed" {
                latest["cleanup"]["status"] = json!("queued");
                stateio::write_task(self.paths, &latest)?;
            }
            return Err("Workspace closed during setup".into());
        }
        result.receipt["step"] = json!(step);
        result.receipt["message"] = json!(format!("Setup running: {step}"));
        operation(&mut latest, &mut result.receipt)?;
        result.task = Some(latest);
        self.save_setup(&result.receipt)
    }
    fn prepare_environment(&self, profile: &Value, result: &mut SetupResult) -> Result<()> {
        self.step(result, "environment", |_, _| Ok(()))?;
        let task = result.task.as_ref().unwrap();
        let (templates, overrides) = configuration(profile, task)?;
        let changed = task["envOverrides"] != templates || task["overrides"] != overrides;
        if let Some(source) = profile["envSource"].as_str() {
            let target = Path::new(string(task, "path")).join(".env");
            ensure(
                system::run(
                    &[
                        "git",
                        "-C",
                        string(task, "path"),
                        "ls-files",
                        "--error-unmatch",
                        ".env",
                    ],
                    None,
                    false,
                )?
                .exit_code
                    != 0,
                "Repository tracks .env; refusing to write credentials into it",
            )?;
            if !paths::exists(&target)? {
                self.progress
                    .run("write-environment", string(task, "id"), || {
                        let mut bytes = fs::read(source).map_err(|_| {
                            Error::from(
                                "Could not read environment baseline; contents were not logged",
                            )
                        })?;
                        bytes.extend_from_slice(
                            b"\n\n# Worktree-specific values managed by graftr.\n",
                        );
                        for (key, value) in overrides.as_object().unwrap() {
                            bytes.extend_from_slice(
                                format!("{key}={}\n", value.as_str().unwrap()).as_bytes(),
                            );
                        }
                        stateio::atomic_write(&target, &bytes, true)
                    })?;
            }
            if let Err(error) = verify_environment(task, &overrides) {
                return Err(if changed {
                    format!("Project environment overrides changed; recorded overrides and .env kept. {}", error.message).into()
                } else {
                    error
                });
            }
        }
        if changed {
            self.step(result, "environment", |current, receipt| {
            let configured = configuration(&self.profile(string(current, "project"))?, current)?;
            ensure(configured == (templates.clone(), overrides.clone()), "Project environment overrides changed during verification; existing files and shells kept. Review the profile and retry graftr setup.")?;
            if current["overrides"] != overrides {receipt["installKey"] = Value::Null;}
            current["envOverrides"] = templates; current["overrides"] = overrides; self.save_task(current)
        })?;
        }
        Ok(())
    }
    fn ensure_tabs(&self, task: &mut Value, reuse_initial: bool) -> Result<()> {
        let reply = herdr::request(
            task["socket"].as_str(),
            "tab.list",
            json!({"workspace_id": task["workspaceId"]}),
            Duration::from_secs(60),
        )?;
        let live = herdr::tabs(&reply)?;
        let present = |id: Option<&str>| live.iter().any(|tab| Some(tab.tab_id.as_str()) == id);
        let initial = live.first().filter(|tab| {
            reuse_initial && live.len() == 1 && tab.label == "1" && tab.pane_count == 1.0
        });
        if !present(task["tabs"]["agent"].as_str()) {
            if let Some(tab) = initial {
                herdr::request(
                    task["socket"].as_str(),
                    "tab.rename",
                    json!({"tab_id": tab.tab_id, "label": "agent"}),
                    Duration::from_secs(60),
                )?;
                task["tabs"]["agent"] = json!(tab.tab_id);
            } else {
                task["tabs"]["agent"] = json!(self.create_tab(task, "agent")?);
            }
            self.save_task(task)?;
        }
        for label in ["editor", "terminal", "server"] {
            if !present(task["tabs"][label].as_str()) {
                task["tabs"][label] = json!(self.create_tab(task, label)?);
                self.save_task(task)?;
            }
        }
        Ok(())
    }
    fn create_tab(&self, task: &Value, label: &str) -> Result<String> {
        let reply = herdr::request(
            task["socket"].as_str(),
            "tab.create",
            json!({"workspace_id": task["workspaceId"], "cwd": task["path"], "label": label, "env": task["overrides"], "focus": false}),
            Duration::from_secs(60),
        )?;
        object(&reply["tab"])?;
        Ok(text(&reply["tab"]["tab_id"])?.into())
    }
    fn prepare_dependencies(&self, profile: &Value, result: &mut SetupResult) -> Result<()> {
        self.step(result, "dependencies", |_, _| Ok(()))?;
        if result.receipt["skipInstall"] == true {
            result.receipt["dependencies"] = json!("skipped");
            dependency_timing(&mut result.receipt, "skipped");
            return Ok(());
        }
        let Some(command) = profile.get("installCommand") else {
            result.receipt["dependencies"] = json!("not-configured");
            dependency_timing(&mut result.receipt, "not-configured");
            return Ok(());
        };
        let task = result.task.as_ref().unwrap().clone();
        let mut prepare = || -> Result<()> {
            let inputs = dependencies::inputs(profile, &task)?;
            if result.receipt["installKey"] == inputs.key
                && dependencies::outputs_present(&task, result.receipt.get("installOutputs"))?
            {
                result.receipt["dependencies"] = json!("ready");
                dependency_timing(&mut result.receipt, "reused");
                return Ok(());
            }
            result.receipt["installKey"] = Value::Null;
            result
                .receipt
                .as_object_mut()
                .unwrap()
                .remove("installOutputs");
            result.receipt["dependencies"] = json!("pending");
            self.step(result, "dependencies", |_, _| Ok(()))?;
            let log = logs::open(
                self.paths,
                string(&task, "id"),
                "install",
                &format!("install for {}", string(&task, "id")),
            )?;
            result.receipt["installLog"] = json!(log.path.to_string_lossy());
            self.step(result, "dependencies", |_, _| Ok(()))?;
            self.progress
                .run("install-dependencies", string(&task, "id"), || {
                    hooks::install(
                        command,
                        Path::new(string(&task, "path")),
                        &task["overrides"],
                        log,
                        "graftr setup",
                    )
                })?;
            let current = self.profile(string(&task, "project"))?;
            ensure(
                current["repo"] == task["repo"],
                "Project profile no longer owns this repository",
            )?;
            let latest = dependencies::inputs(&current, &task)?;
            ensure(
                latest.key == inputs.key,
                "Dependency inputs changed during installation; checkout kept. Review the changes and retry setup",
            )?;
            result.receipt["installOutputs"] =
                dependencies::installed_outputs(&current, &task, &latest.manifests)?;
            result.receipt["installKey"] = json!(inputs.key);
            result.receipt["dependencies"] = json!("ready");
            dependency_timing(&mut result.receipt, "installed");
            Ok(())
        };
        if let Err(error) = prepare() {
            result.receipt["installKey"] = Value::Null;
            result
                .receipt
                .as_object_mut()
                .unwrap()
                .remove("installOutputs");
            result.receipt["dependencies"] = json!("failed");
            dependency_timing(&mut result.receipt, "failed");
            return Err(error);
        }
        Ok(())
    }
    fn hook_context(
        &self,
        profile: &Value,
        task: &Value,
        receipt: &mut Value,
        phase: &str,
    ) -> Result<()> {
        let current = self.profile(string(task, "project"))?;
        let signature = |p: &Value| {
            json!([
                p["hooks"],
                p["installCommand"],
                p["installInputs"],
                p["installOutputs"]
            ])
        };
        let configured = configuration(&current, task)?;
        ensure(
            current["repo"] == task["repo"]
                && current["envSource"] == profile["envSource"]
                && signature(&current) == signature(profile)
                && configured == (task["envOverrides"].clone(), task["overrides"].clone()),
            "Project setup configuration changed during hooks; review it and retry setup",
        )?;
        addresses::verify(&current, task)?;
        ensure(
            checkout::canonical(string(task, "path"))? == string(task, "path"),
            "Hook checkout is no longer canonical",
        )?;
        checkout::verify(task)?;
        ensure(
            checkout::registered(task)?
                .is_some_and(|t| !t.detached && t.branch.as_deref() == task["branch"].as_str()),
            "Hook checkout registration/branch changed; review ownership before retrying setup",
        )?;
        if current.get("envSource").is_some() {
            verify_environment(task, &task["overrides"])?;
        }
        if phase == "afterDependencies"
            && receipt["dependencies"] == "ready"
            && current.get("installCommand").is_some()
            && (dependencies::inputs(&current, task)?.key != receipt["installKey"]
                || !dependencies::outputs_present(task, receipt.get("installOutputs"))?)
        {
            receipt["installKey"] = Value::Null;
            receipt.as_object_mut().unwrap().remove("installOutputs");
            receipt["dependencies"] = json!("failed");
            return Err("Dependency inputs or required outputs changed during hooks; retry setup before using the installed dependencies".into());
        }
        Ok(())
    }
    fn prepare_hooks(&self, profile: &Value, result: &mut SetupResult, phase: &str) -> Result<()> {
        let Some(configured) = profile["hooks"][phase].as_array().filter(|h| !h.is_empty()) else {
            return Ok(());
        };
        let timing = Instant::now();
        let mut execute = || -> Result<()> {
            for hook in configured {
                self.step(result, "hooks", |_, _| Ok(()))?;
                let task = result.task.as_ref().unwrap().clone();
                self.hook_context(profile, &task, &mut result.receipt, phase)?;
                let index = result.receipt["hooks"].as_array().map_or(0, Vec::len);
                self.step(result, "hooks", |_, receipt| {
                    if receipt.get("hooks").is_none() {
                        receipt["hooks"] = json!([]);
                    }
                    receipt["hooks"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!({"phase": phase, "name": hook["name"], "status": "running"}));
                    receipt["message"] =
                        json!(format!("Setup running: {phase}/{}", string(hook, "name")));
                    Ok(())
                })?;
                let started = Instant::now();
                let mut hook_failure = None;
                let outcome =
                    self.progress
                        .run("run-hook", string(&task, "id"), || -> Result<Value> {
                            let log = logs::open(
                                self.paths,
                                string(&task, "id"),
                                &format!("{phase}-{}", string(hook, "name")),
                                &format!(
                                    "{phase} hook {} for {}",
                                    string(hook, "name"),
                                    string(&task, "id")
                                ),
                            )?;
                            let dependency = if phase == "afterEnvironment" {
                                "pending"
                            } else {
                                string(&result.receipt, "dependencies")
                            };
                            let outcome = hooks::run(
                                hook,
                                phase,
                                &task,
                                dependency,
                                string(&result.receipt, "runId"),
                                Path::new(string(&task, "path")),
                                log,
                            )
                            .map_err(|failure| {
                                hook_failure = Some(failure.result);
                                failure.error
                            })?;
                            self.step(result, "hooks", |_, _| Ok(()))?;
                            self.hook_context(profile, &task, &mut result.receipt, phase)?;
                            Ok(outcome)
                        });
                let (outcome, error) = match outcome {Ok(value) => (value, None), Err(error) => (hook_failure.unwrap_or(json!({"phase": phase, "name": hook["name"], "status": "failed", "reason": "context-changed", "elapsedMs": started.elapsed().as_secs_f64() * 1000.0})), Some(error))};
                self.step(result, "hooks", |_, receipt| {
                    receipt["hooks"][index] = outcome;
                    Ok(())
                })?;
                if let Some(error) = error {
                    return Err(error);
                }
            }
            Ok(())
        };
        let outcome = execute();
        add_receipt_timing(&mut result.receipt, "hooks", timing);
        outcome
    }
    fn run(&self, mut result: SetupResult) -> Result<SetupResult> {
        if result.task.is_none() || result.receipt["status"] != "pending" {
            return Ok(result);
        }
        let task = result.task.as_ref().unwrap().clone();
        let waiting = Instant::now();
        let lock = locks::worktree(self.paths, string(&task, "path"), false);
        add_receipt_timing(&mut result.receipt, "worktreeLockWait", waiting);
        let mut operation = || -> Result<()> {
            if let Err(error) = &lock {
                return Err(Error {
                    code: error.code,
                    message: error.message.clone(),
                });
            }
            let profile_started = Instant::now();
            self.step(&mut result, "profile", |_, _| Ok(()))?;
            ensure(
                checkout::canonical(string(&task, "path"))? == string(&task, "path"),
                "Setup checkout is no longer canonical",
            )?;
            checkout::verify(&task)?;
            ensure(
                checkout::registered(&task)?
                    .is_some_and(|t| !t.detached && t.branch.as_deref() == task["branch"].as_str()),
                "Setup checkout registration/branch changed; review ownership before retrying",
            )?;
            let profile = self.profile(string(&task, "project"))?;
            ensure(
                profile["repo"] == task["repo"],
                "Project profile no longer owns this repository",
            )?;
            add_receipt_timing(&mut result.receipt, "profile", profile_started);
            if result.receipt.get("hooks").is_some() {
                self.step(&mut result, "profile", |_, receipt| {
                    receipt.as_object_mut().unwrap().remove("hooks");
                    Ok(())
                })?;
            }
            let started = Instant::now();
            let prepared = self.prepare_environment(&profile, &mut result);
            add_receipt_timing(&mut result.receipt, "environment", started);
            prepared?;
            self.prepare_hooks(&profile, &mut result, "afterEnvironment")?;
            if !result.local && !task["socket"].is_null() {
                let started = Instant::now();
                let tabs = self.step(&mut result, "tabs", |current, _| {
                    let id = string(current, "id").to_owned();
                    self.progress.run("prepare-tabs", &id, || {
                        self.ensure_tabs(current, profile.get("envSource").is_some())
                    })
                });
                add_receipt_timing(&mut result.receipt, "tabs", started);
                tabs?;
            }
            let started = Instant::now();
            let prepared = self.prepare_dependencies(&profile, &mut result);
            add_receipt_timing(&mut result.receipt, "dependencies", started);
            prepared?;
            self.prepare_hooks(&profile, &mut result, "afterDependencies")?;
            let started = Instant::now();
            let finalized = self.step(&mut result, "finalize", |current, receipt| {
                let latest = self.profile(string(current, "project"))?;
                ensure(latest["envSource"] == profile["envSource"], "Project environment source changed during setup; existing files and shells kept. Review the profile and retry graftr setup.")?;
                ensure(configuration(&latest, current)? == (current["envOverrides"].clone(), current["overrides"].clone()), "Project environment overrides changed during setup; existing files and shells kept. Review the profile and environment, then retry graftr setup.")?;
                ensure(latest["hooks"] == profile["hooks"], "Setup hooks changed during preparation; review the profile and retry setup before publishing readiness")?; addresses::verify(&latest, current)?;
                current["status"] = json!("ready"); self.save_task(current)?; receipt["status"] = json!("ready"); receipt["step"] = json!("complete");
                receipt["message"] = json!(if receipt["dependencies"] == "ready" {"Worktree setup complete"} else if receipt["dependencies"] == "skipped" {"Environment and configured hooks ready; dependency install explicitly skipped"} else {"Environment and configured hooks ready; no dependency installer configured (install manually)"}); Ok(())
            });
            add_receipt_timing(&mut result.receipt, "finalize", started);
            finalized?;
            Ok(())
        };
        if let Err(error) = operation() {
            let _lock = locks::state(self.paths)?;
            result.task = if paths::exists(
                &self
                    .paths
                    .state
                    .join(format!("{}.json", string(&task, "id"))),
            )? {
                Some(
                    records::task(&self.paths.state, string(&task, "id"))?
                        .value()
                        .clone(),
                )
            } else {
                None
            };
            let latest = records::setup_for(
                &self.paths.state,
                result.receipt["socket"].as_str(),
                string(&result.receipt, "path"),
            )?;
            if latest.is_none() {
                result.receipt["runId"] = Value::Null;
                result.receipt["status"] = json!("incomplete");
                result.receipt["message"] =
                    json!("Setup cancelled; checkout/record removed. No resources restored.");
                return Ok(result);
            }
            if let Some(latest) = latest
                && (latest.value()["runId"] != result.receipt["runId"]
                    || latest.value()["status"] != "pending")
            {
                result.receipt = latest.value().clone();
                return Ok(result);
            }
            result.receipt["status"] = json!("incomplete");
            result.receipt["message"] = json!(format!(
                "{}: {}. Checkout kept; fix the problem and run graftr setup (or --skip-install for an installer failure).",
                string(&result.receipt, "step"),
                error.message
            ));
            self.save_setup(&result.receipt)?;
            result.attention_needed = true;
        }
        finish_receipt_timing(&mut result.receipt);
        // Timing telemetry must not turn a completed lifecycle operation into a failure.
        let save_timings = || -> Result<()> {
            let Some(timing) = result.receipt.get("timings") else {
                return Ok(());
            };
            let _lock = locks::state(self.paths)?;
            let latest = records::setup_for(
                &self.paths.state,
                result.receipt["socket"].as_str(),
                string(&result.receipt, "path"),
            )?;
            if let Some(latest) = latest.filter(|r| {
                r.value()["runId"] == result.receipt["runId"]
                    && r.value()["timings"]["id"] == timing["id"]
                    && r.value()["timings"].get("finishedAt").is_none()
            }) {
                let mut receipt = latest.value().clone();
                receipt["timings"] = timing.clone();
                stateio::write_setup(self.paths, &receipt)?;
            }
            Ok(())
        };
        if save_timings().is_err() {
            eprintln!("graftr: Could not save setup timings; lifecycle result is unchanged.");
        }
        Ok(result)
    }
}
pub fn data(result: &SetupResult) -> Value {
    let mut output = json!({});
    if let Some(task) = &result.task {
        for key in ["id", "branch", "url"] {
            output[key] = task[key].clone();
        }
    }
    for key in [
        "status",
        "path",
        "workspaceId",
        "step",
        "dependencies",
        "message",
        "timings",
        "hooks",
    ] {
        if let Some(value) = result.receipt.get(key) {
            output[key] = value.clone();
        }
    }
    if let Some(log_id) = &result.log_id {
        output["logId"] = json!(log_id);
    }
    output
}
pub fn format(result: &SetupResult, columns: usize) -> String {
    let receipt = &result.receipt;
    let task = result.task.as_ref();
    human::fields(
        &format!(
            "{}: {}",
            task.map_or(string(receipt, "path"), |t| string(t, "id")),
            string(receipt, "status")
        ),
        &[
            ("Branch", task.map(|t| string(t, "branch").into())),
            ("Path", Some(string(receipt, "path").into())),
            ("Setup", Some(string(receipt, "step").into())),
            ("Dependencies", Some(string(receipt, "dependencies").into())),
            (
                "Address",
                task.map(|t| {
                    t["url"].as_str().map_or_else(
                        || format!("localhost:{}", t["localPort"].as_f64().unwrap()),
                        str::to_owned,
                    )
                }),
            ),
            ("Worker log", result.log_id.clone()),
        ],
        receipt["message"].as_str(),
        string(receipt, "status"),
        columns,
    )
}
