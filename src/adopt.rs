use crate::{
    addresses,
    checkout::string,
    doctor, envdoc, envrefresh, health, herdr, hooks, human, locks, logs,
    paths::{self, Paths},
    profiles,
    progress::Progress,
    records, setup, stateio, system, targets, terminal,
    validation::*,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, env, path::Path, time::Duration};

#[derive(Default)]
pub struct Options {
    pub project: Option<String>,
    pub install: bool,
    pub environment: bool,
    pub apply: bool,
    pub baseline: Vec<String>,
    pub hooks: Vec<String>,
}
pub enum ProvisionResult {
    Preview(Value),
    Setup(setup::SetupResult),
}

/// Explicit ownership transfer for migrated, addressless checkouts. Existing
/// environments, attachments and manual addresses must never be silently claimed.
pub fn provision(
    paths: &Paths,
    directory: Option<&str>,
    project: Option<&str>,
    dry_run: bool,
    progress: &Progress,
) -> Result<ProvisionResult> {
    ensure(
        !herdr::in_plugin_action(),
        "Provision adoption from a checkout terminal, not a Herdr plugin action",
    )?;
    let cwd = env::current_dir().map_err(|_| Error::from("Current directory is unavailable"))?;
    let requested = targets::resolve_path(&cwd, directory.unwrap_or("."))?;
    let initial = doctor::adopted_checkout(&requested.to_string_lossy())?;
    let (project_name, settings) = profiles::for_repo(paths, string(&initial, "repo"))?
        .ok_or("Configure exactly one matching project profile before provisioning")?;
    if let Some(project) = project {
        name(project)?;
        ensure(
            project == project_name,
            "The adopted worktree belongs to another project",
        )?;
    }
    let record = records::adoptions(&paths.state)?.into_iter()
        .find(|record| record.value()["path"] == initial["path"])
        .ok_or("Provisioning requires an existing adopted worktree; use setup/open for an already managed task")?
        .value().clone();
    let adoption = Adoption {
        paths,
        initial,
        project: project_name,
        settings,
        expected: Some(record.clone()),
        record,
        progress,
    };
    let engine = setup::Engine { paths, progress };
    // Preview is entirely read-only. Apply takes the checkout lock before the
    // registry lock, as explicit adoption stages do, and revalidates everything.
    let tree_lock = if dry_run {
        None
    } else {
        Some(locks::worktree(
            paths,
            string(&adoption.record, "path"),
            true,
        )?)
    };
    let state_lock = if dry_run {
        None
    } else {
        Some(locks::state(paths)?)
    };
    adoption.verify()?;
    let record = &adoption.record;
    ensure(
        record["localPort"].is_null() && record["url"].is_null(),
        "This adoption has a manual address; provisioning will not take over or reassign it",
    )?;
    ensure(
        record["socket"].is_null() && record["workspaceId"].is_null(),
        "This adoption has an external Herdr attachment; review it before transferring lifecycle ownership",
    )?;
    ensure(
        record["environment"] == "external"
            && record["dependencies"] == "external"
            && record["hooks"].as_array().unwrap().is_empty(),
        "Selected adoption stages require review before provisioning; their effects were preserved",
    )?;
    let path = string(record, "path");
    ensure(
        !records::setups(&paths.state)?
            .iter()
            .any(|receipt| receipt.value()["path"] == path),
        "Existing setup receipts require review before transferring adoption ownership",
    )?;
    ensure(
        !paths::exists(&Path::new(path).join(".env"))?,
        "Existing .env preserved; provisioning only supports checkouts without a manual environment",
    )?;
    ensure(
        system::git(Path::new(path), &["ls-files", "-z", "--", ".env"])?.is_empty(),
        "Repository tracks .env; refusing to provision credentials",
    )?;
    let profile = engine.profile(&adoption.project)?;
    if let Some(source) = profile["envSource"].as_str() {
        envrefresh::read_environment(source)?;
        ensure(
            system::run(
                &["git", "-C", path, "check-ignore", "-q", "--", ".env"],
                None,
                false,
            )?
            .exit_code
                == 0,
            "The checkout must ignore .env before provisioning a private environment",
        )?;
    }
    let mut task = json!({"schemaVersion":4,"id":record["id"],"project":record["project"],"repo":record["repo"],"path":path,"branch":record["branch"],
        "baseCommit":system::git(Path::new(path), &["rev-parse","HEAD"] )?,"socket":null,"workspaceId":null,"tabs":{},"status":"preparing"});
    for (key, value) in addresses::allocate(paths, &profile, &task)?
        .as_object()
        .unwrap()
    {
        task[key] = value.clone();
    }
    let (templates, assignments) = setup::configuration(&profile, &task)?;
    task["envOverrides"] = templates;
    task["overrides"] = assignments;
    let task = records::Task::parse(&task)?;
    let task = task.value();
    if dry_run {
        return Ok(ProvisionResult::Preview(
            json!({"status":"preview","id":task["id"],"path":path,"branch":task["branch"],"localPort":task["localPort"],"httpsPort":task["httpsPort"],"url":task["url"],"ports":task.get("ports"),"standalone":true}),
        ));
    }
    adoption.verify()?;
    ensure(
        !paths::exists(&Path::new(path).join(".env"))?,
        "Environment appeared during provisioning; existing files preserved",
    )?;
    let backup_root = paths.state.join("migration-backups");
    locks::private_directory(&backup_root)?;
    let backup = backup_root.join(format!(
        "adoption-{}-{}.json",
        string(record, "id"),
        system::uuid()?
    ));
    let adoption_file = paths
        .state
        .join("adopted")
        .join(format!("{}.json", string(record, "id")));
    let original = records::read_private(&adoption_file)?;
    progress.run("backup-adoption", string(record, "id"), || {
        stateio::atomic_write(&backup, &original, true)
    })?;
    let receipt = json!({"schemaVersion":1,"socket":null,"workspaceId":null,"path":path,"project":record["project"],"taskId":record["id"],"status":"incomplete","step":"enrollment", "message":"Adoption ownership transferred; run graftr setup --no-herdr if provisioning was interrupted.","installKey":null,"dependencies":"not-configured","skipInstall":false,"runId":null});
    progress.run("save-setup", string(record, "id"), || {
        stateio::write_setup(paths, &receipt)
    })?;
    engine.save_task(task)?;
    // Never create an unowned gap: the durable managed record and receipt precede
    // removal of external tracking. On interruption both records remain fail-closed.
    progress.run("provision-adoption", string(record,"id"), || {
        let directory = paths.state.join("adopted");
        std::fs::remove_file(&adoption_file)
            .map_err(|_| Error::from("Ownership transfer interrupted; preserve both records and the migration backup for review"))?;
        stateio::sync_directory(&directory)
    })?;
    drop(state_lock);
    drop(tree_lock);
    engine
        .open(
            string(record, "id"),
            &setup::Options {
                no_herdr: true,
                wait: true,
                ..Default::default()
            },
        )
        .map(ProvisionResult::Setup)
}

pub fn format_provision_preview(report: &Value, columns: usize) -> String {
    human::fields(
        &format!("{}: provision preview", string(report, "id")),
        &[
            ("Path", Some(string(report, "path").into())),
            ("Branch", Some(string(report, "branch").into())),
            ("Local port", Some(report["localPort"].to_string())),
            ("URL", report["url"].as_str().map(str::to_owned)),
        ],
        Some(
            "Read-only preview; ports are not reserved. Omit --dry-run to transfer ownership and run the approved setup synchronously. No Herdr workspace is changed; attach later with graftr open.",
        ),
        "preview",
        columns,
    )
}
pub fn ports(record: &Value) -> Vec<f64> {
    let mut ports = Vec::new();
    if let Some(port) = record["localPort"].as_f64() {
        ports.push(port);
    }
    if let Some(port) = record["url"]
        .as_str()
        .and_then(|s| url(s).ok())
        .and_then(|u| u.port())
    {
        ports.push(port.into());
    }
    ports
}
fn leases(task: &Value) -> Vec<f64> {
    let mut values = vec![task["localPort"].as_f64().unwrap()];
    if let Some(port) = task["httpsPort"].as_f64() {
        values.push(port);
    }
    if let Some(ports) = task["ports"].as_object() {
        values.extend(ports.values().map(|p| p.as_f64().unwrap()));
    }
    values
}
fn observed_address(path: &str) -> (Option<u16>, Option<String>) {
    let Ok(file) = envrefresh::read_environment_mode(&format!("{path}/.env"), 0o022) else {
        return (None, None);
    };
    let values = envdoc::loose(&file.bytes);
    let port = values
        .get("PORT")
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|p| *p > 0);
    let url = values
        .get("BASE_URL")
        .and_then(|s| url(s).ok())
        .map(|u| u.origin().ascii_serialization());
    (port, url)
}
fn selected_hooks(profile: &Value, selectors: &[String]) -> Result<Vec<(String, Value)>> {
    let mut all = Vec::new();
    for phase in ["afterEnvironment", "afterDependencies"] {
        if let Some(hooks) = profile["hooks"][phase].as_array() {
            for hook in hooks {
                all.push((phase.to_owned(), hook.clone()));
            }
        }
    }
    let mut selected = HashSet::new();
    for selector in selectors {
        let matches: Vec<_> = all
            .iter()
            .enumerate()
            .filter(|(_, (phase, hook))| {
                selector == string(hook, "name")
                    || selector == &format!("{phase}/{}", string(hook, "name"))
            })
            .collect();
        ensure(
            matches.len() == 1,
            format!(
                "Choose one configured setup hook by PHASE/NAME; {} is missing or ambiguous",
                json!(selector)
            ),
        )?;
        let (index, (_, hook)) = matches[0];
        let command = hook["command"].as_array().unwrap();
        ensure(
            !command.windows(2).any(|args| {
                args[0] == "hook"
                    && [Some("tailscale"), Some("postgres")].contains(&args[1].as_str())
            }),
            "Built-in Tailscale/Postgres hooks require provisioned resource ownership; adoption leaves manual services external and will not take over or replace them",
        )?;
        selected.insert(index);
    }
    Ok(all
        .into_iter()
        .enumerate()
        .filter_map(|(index, value)| selected.contains(&index).then_some(value))
        .collect())
}
struct EnvironmentPlan {
    baseline: envrefresh::EnvironmentFile,
    current: envrefresh::EnvironmentFile,
    plan: envdoc::Plan,
}
fn environment_plan(
    profile: &Value,
    record: &Value,
    selected: &[String],
) -> Result<EnvironmentPlan> {
    let source = profile["envSource"].as_str();
    let target = format!("{}/.env", string(record, "path"));
    ensure(
        source
            .is_some_and(|s| Path::new(s).file_name().is_some_and(|p| p == ".env") && s != target),
        "Environment adoption requires a separate approved .env baseline",
    )?;
    ensure(
        system::git(
            Path::new(string(record, "path")),
            &["ls-files", "-z", "--", ".env"],
        )?
        .is_empty(),
        "Refusing to refresh a tracked .env",
    )?;
    let baseline = envrefresh::read_environment(source.unwrap())?;
    let current = envrefresh::read_environment(&target)?;
    let from = envdoc::loose(&baseline.bytes);
    let to = envdoc::loose(&current.bytes);
    let mut protected: HashSet<String> = ["PORT", "BASE_URL", "AUTH_URL", "NEXTAUTH_URL"]
        .map(str::to_owned)
        .into_iter()
        .collect();
    if let Some(overrides) = profile["envOverrides"].as_object() {
        protected.extend(overrides.keys().cloned());
    }
    ensure(
        !protected
            .iter()
            .any(|key| from.contains_key(key) && !to.contains_key(key)),
        "Configure missing public environment assignments manually before refresh; adoption will not invent or reassign ports and URLs",
    )?;
    let overrides: serde_json::Map<_, _> = protected
        .iter()
        .filter_map(|key| to.get(key).map(|value| (key.clone(), json!(value))))
        .collect();
    let plan = envdoc::plan(&baseline.bytes, &current.bytes, &json!(overrides), selected)?;
    Ok(EnvironmentPlan {
        baseline,
        current,
        plan,
    })
}
struct Adoption<'a> {
    paths: &'a Paths,
    initial: Value,
    project: String,
    settings: Object,
    record: Value,
    expected: Option<Value>,
    progress: &'a Progress,
}
enum Stage {
    Environment,
    Dependencies,
    Hook(usize),
}
impl Adoption<'_> {
    fn verify(&self) -> Result<()> {
        let record = &self.record;
        ensure(
            doctor::adopted_checkout(string(&self.initial, "path"))? == self.initial,
            "Checkout identity changed during adoption; no branch or path was rebound",
        )?;
        ensure(
            record["repo"] == self.initial["repo"]
                && record["branch"] == self.initial["branch"]
                && record["project"] == self.project,
            "Adopted checkout identity changed; review the original record",
        )?;
        ensure(
            profiles::for_repo(self.paths, string(&self.initial, "repo"))?
                .is_some_and(|(_, settings)| settings == self.settings),
            "Project configuration changed during adoption",
        )?;
        let tasks = records::tasks(&self.paths.state)?;
        let others = records::adoptions(&self.paths.state)?;
        ensure(
            !tasks.iter().any(|t| {
                let t = t.value();
                t["id"] == record["id"]
                    || t["path"] == record["path"]
                    || t["repo"] == record["repo"] && t["branch"] == record["branch"]
            }) && !others.iter().any(|o| {
                let o = o.value();
                if o["id"] == record["id"] {
                    o["path"] != record["path"]
                } else {
                    o["path"] == record["path"]
                        || o["repo"] == record["repo"] && o["branch"] == record["branch"]
                }
            }),
            "Task, path or branch is already owned; no existing record was replaced",
        )?;
        ensure(
            record["socket"].is_null()
                || !(tasks.iter().any(|t| {
                    t.value()["socket"] == record["socket"]
                        && t.value()["workspaceId"] == record["workspaceId"]
                }) || others.iter().any(|o| {
                    o.value()["id"] != record["id"]
                        && o.value()["socket"] == record["socket"]
                        && o.value()["workspaceId"] == record["workspaceId"]
                })),
            "The observed Herdr attachment is already claimed by another tracked checkout",
        )?;
        let claimed = ports(record);
        ensure(
            !tasks
                .iter()
                .any(|t| leases(t.value()).iter().any(|p| claimed.contains(p)))
                && !others.iter().any(|o| {
                    o.value()["id"] != record["id"]
                        && ports(o.value()).iter().any(|p| claimed.contains(p))
                }),
            "Observed ports conflict with another tracked worktree; resolve the conflict before adoption",
        )?;
        ensure(
            !records::frozen(&self.paths.state)?.iter().any(|f| {
                let v = f.value();
                f.reserves()
                    && (v["id"] == record["id"]
                        || v["removal"]["repo"] == record["repo"]
                            && (v["removal"]["path"] == record["path"]
                                || v["removal"]["currentBranch"] == record["branch"]))
            }),
            "This checkout is reserved by frozen recovery; use graftr thaw instead",
        )?;
        ensure(
            !records::setups(&self.paths.state)?
                .iter()
                .any(|r| r.value()["path"] == record["path"] && r.value()["status"] == "pending"),
            "A setup worker is pending for this checkout; wait before adoption",
        )?;
        ensure(
            others
                .iter()
                .find(|o| o.value()["id"] == record["id"])
                .map(|o| o.value())
                == self.expected.as_ref(),
            "Adoption record changed concurrently; review and retry",
        )
    }
    fn persist(&mut self) -> Result<()> {
        let _lock = locks::state(self.paths)?;
        self.verify()?;
        self.progress
            .run("save-adoption", string(&self.record, "id"), || {
                stateio::write_adoption(self.paths, &self.record)
            })?;
        self.expected = Some(self.record.clone());
        Ok(())
    }
    fn mark(&mut self, stage: &Stage, status: &str) {
        match stage {
            Stage::Environment => self.record["environment"] = json!(status),
            Stage::Dependencies => self.record["dependencies"] = json!(status),
            Stage::Hook(index) => self.record["hooks"][index]["status"] = json!(status),
        }
    }
    fn stage(
        &mut self,
        stage: Stage,
        operation: impl FnOnce(&mut Self) -> Result<()>,
    ) -> Result<()> {
        self.mark(&stage, "pending");
        self.persist()?;
        let result = operation(self);
        self.mark(
            &stage,
            if result.is_ok() {
                "completed"
            } else {
                "failed"
            },
        );
        self.persist()?;
        result
    }
}
pub fn adopt(
    paths: &Paths,
    directory: Option<&str>,
    options: &Options,
    progress: &Progress,
) -> Result<Value> {
    let cwd = env::current_dir().map_err(|_| Error::from("Current directory is unavailable"))?;
    let requested = targets::resolve_path(&cwd, directory.unwrap_or("."))?;
    let initial = doctor::adopted_checkout(&requested.to_string_lossy())?;
    let (project, settings) =
        profiles::for_repo(paths, string(&initial, "repo"))?.ok_or_else(|| {
            Error::from("Configure exactly one matching project profile before adoption")
        })?;
    ensure(
        options.project.as_ref().is_none_or(|p| *p == project),
        "Configure exactly one matching project profile before adoption",
    )?;
    if let Some(project) = &options.project {
        name(project)?;
    }
    ensure(
        !(options.apply || !options.baseline.is_empty()) || options.environment,
        "--apply and --take-baseline require --environment during adoption",
    )?;
    ensure(
        !(options.environment && !options.apply && (options.install || !options.hooks.is_empty())),
        "Preview the environment separately, or use --environment --apply to combine explicit setup stages",
    )?;
    let profile = if options.environment || options.install || !options.hooks.is_empty() {
        Some(profiles::configured(paths, &project)?.value().clone())
    } else {
        None
    };
    let selected = profile
        .as_ref()
        .map(|p| selected_hooks(p, &options.hooks))
        .transpose()?
        .unwrap_or_default();
    ensure(
        !options.install
            || profile
                .as_ref()
                .is_some_and(|p| p.get("installCommand").is_some()),
        "This project has no configured dependency installer",
    )?;
    let records = records::adoptions(&paths.state)?;
    let existing = records
        .iter()
        .find(|r| r.value()["path"] == initial["path"])
        .map(|r| r.value().clone());
    let basename = Path::new(string(&initial, "path"))
        .file_name()
        .unwrap()
        .to_string_lossy();
    let prefix = pattern!(r"[^a-z0-9]+")
        .replace_all(&basename.to_lowercase(), "-")
        .trim_matches('-')
        .chars()
        .take(39)
        .collect::<String>()
        .trim_end_matches('-')
        .to_owned();
    let task_name = if name(&basename).is_ok() {
        basename.into_owned()
    } else {
        format!(
            "{}-{}",
            if prefix.is_empty() {
                "worktree"
            } else {
                &prefix
            },
            &format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&json!([initial["path"]])).unwrap())
            )[..8]
        )
    };
    let record = existing.clone().unwrap_or_else(|| {
        let (port, url) = observed_address(string(&initial, "path"));
        json!({"schemaVersion": 1, "id": format!("{project}--{task_name}"), "project": project, "repo": initial["repo"], "path": initial["path"], "branch": initial["branch"], "adoptedAt": health::timestamp(), "socket": null, "workspaceId": null, "localPort": port, "url": url, "environment": "external", "dependencies": "external", "hooks": []})
    });
    let mut adoption = Adoption {
        paths,
        initial,
        project,
        settings,
        record,
        expected: existing.clone(),
        progress,
    };
    adoption.verify()?;
    let environment = if options.environment {
        Some(environment_plan(
            profile.as_ref().unwrap(),
            &adoption.record,
            &options.baseline,
        )?)
    } else {
        None
    };
    let mut environment_report = environment.as_ref().map(|plan| json!({"entries": plan.plan.entries, "reviewRequired": plan.plan.entries.iter().any(|e| e["status"] == "different" && e["action"] == "preserve"), "backupPath": null}));
    if options.environment && !options.apply {
        return Ok(
            json!({"status": "preview", "worktree": adoption.record, "environment": environment_report}),
        );
    }
    let _tree_lock = locks::worktree(paths, string(&adoption.initial, "path"), false)?;
    {
        let _lock = locks::state(paths)?;
        adoption.verify()?;
    }
    if existing.is_none()
        && profiles::herdr_mode(paths)? != "off"
        && env::var("HERDR_ENV").as_deref() == Ok("1")
        && let Ok(socket) = env::var("HERDR_SOCKET_PATH")
        && !socket.is_empty()
    {
        match herdr::request(
            Some(&socket),
            "workspace.list",
            json!({}),
            Duration::from_millis(1500),
        ) {
            Ok(reply) => {
                let spaces = herdr::workspaces(&reply)?;
                let matches: Vec<_> = spaces
                    .iter()
                    .filter(|s| s.checkout_path.as_deref() == adoption.record["path"].as_str())
                    .collect();
                ensure(
                    matches.len() <= 1,
                    "Multiple Herdr workspaces claim this checkout",
                )?;
                if let Some(space) = matches.first() {
                    adoption.record["socket"] = json!(socket);
                    adoption.record["workspaceId"] = json!(space.workspace_id);
                }
            }
            Err(error)
                if ["HERDR_UNAVAILABLE", "HERDR_CONNECTION_UNAVAILABLE"].contains(&error.code) => {}
            Err(error) => return Err(error),
        }
    }
    if existing.is_none() {
        adoption.persist()?;
    }
    let mut status = if existing.is_some() {
        "unchanged"
    } else {
        "adopted"
    };
    if let Some(environment) = environment {
        let profile = profile.as_ref().unwrap();
        adoption.stage(Stage::Environment, |adoption| {
            let latest = environment_plan(profile, &adoption.record, &options.baseline)?;
            ensure(
                latest.current == environment.current && latest.baseline == environment.baseline,
                "Environment changed since preview; review a new preview before applying",
            )?;
            if latest.plan.content == latest.current.bytes {
                return Ok(());
            }
            ensure(
                system::run(
                    &[
                        "git",
                        "-C",
                        &paths.state.to_string_lossy(),
                        "rev-parse",
                        "--is-inside-work-tree",
                    ],
                    None,
                    false,
                )?
                .exit_code
                    != 0,
                "Secret-bearing environment backups cannot be saved inside a Git worktree",
            )?;
            let root = paths.state.join("env-backups");
            locks::private_directory(&root)?;
            ensure(
                system::run(
                    &[
                        "git",
                        "-C",
                        &root.to_string_lossy(),
                        "rev-parse",
                        "--is-inside-work-tree",
                    ],
                    None,
                    false,
                )?
                .exit_code
                    != 0,
                "Environment backup directory belongs to a Git worktree",
            )?;
            let backup = root.join(format!(
                "{}-{}.env",
                string(&adoption.record, "id"),
                system::uuid()?
            ));
            progress.run("backup-environment", string(&adoption.record, "id"), || {
                stateio::atomic_write(&backup, &latest.current.bytes, true)
            })?;
            environment_report.as_mut().unwrap()["backupPath"] = json!(backup.to_string_lossy());
            let check = environment_plan(profile, &adoption.record, &options.baseline)?;
            ensure(
                check.current == latest.current && check.baseline == latest.baseline,
                "Environment changed before apply; backup retained and no stale plan was written",
            )?;
            progress.run(
                "refresh-environment",
                string(&adoption.record, "id"),
                || {
                    stateio::atomic_write(
                        &Path::new(string(&adoption.record, "path")).join(".env"),
                        &latest.plan.content,
                        false,
                    )
                },
            )
        })?;
        status = "adopted";
    }
    for phase in ["afterEnvironment", "afterDependencies"] {
        if phase == "afterDependencies" && options.install {
            adoption.stage(Stage::Dependencies, |adoption| {
                let log = logs::open(
                    paths,
                    string(&adoption.record, "id"),
                    "install",
                    "adopt dependency installation",
                )?;
                progress.run(
                    "install-dependencies",
                    string(&adoption.record, "id"),
                    || {
                        hooks::install(
                            &profile.as_ref().unwrap()["installCommand"],
                            Path::new(string(&adoption.record, "path")),
                            &json!({}),
                            log,
                            "graftr adopt PATH --install",
                        )
                    },
                )
            })?;
            status = "adopted";
        }
        for (_, hook) in selected.iter().filter(|(p, _)| p == phase) {
            let index = adoption.record["hooks"]
                .as_array()
                .unwrap()
                .iter()
                .position(|h| h["phase"] == phase && h["name"] == hook["name"])
                .unwrap_or_else(|| {
                    let hooks = adoption.record["hooks"].as_array_mut().unwrap();
                    hooks.push(json!({"phase": phase, "name": hook["name"], "status": "pending"}));
                    hooks.len() - 1
                });
            adoption.stage(Stage::Hook(index), |adoption| {
                let log = logs::open(
                    paths,
                    string(&adoption.record, "id"),
                    &format!("{phase}-{}", string(hook, "name")),
                    &format!("adopt hook {phase}/{}", string(hook, "name")),
                )?;
                let mut task = adoption.record.clone();
                task["overrides"] = json!({});
                task["httpsPort"] = json!(
                    task["url"]
                        .as_str()
                        .and_then(|s| url(s).ok())
                        .filter(|u| u.scheme() == "https")
                        .map(|u| u.port_or_known_default().unwrap())
                );
                progress.run("run-hook", string(&adoption.record, "id"), || {
                    hooks::run(
                        hook,
                        phase,
                        &task,
                        string(&adoption.record, "dependencies"),
                        &system::uuid()?,
                        Path::new(string(&adoption.record, "path")),
                        log,
                    )
                    .map(|_| ())
                    .map_err(|failure| failure.into_error())
                })
            })?;
            status = "adopted";
        }
    }
    let mut report = json!({"status": status, "worktree": adoption.record});
    if let Some(environment) = environment_report {
        report["environment"] = environment;
    }
    Ok(report)
}
pub fn format(report: &Value, columns: usize) -> String {
    let record = &report["worktree"];
    let status = string(report, "status");
    let text = human::fields(
        &format!("{}: {status}", string(record, "id")),
        &[
            ("Branch", Some(string(record, "branch").into())),
            ("Path", Some(string(record, "path").into())),
            ("Setup", Some("external".into())),
            (
                "Address",
                Some(
                    record["url"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            record["localPort"]
                                .as_f64()
                                .map_or("not recorded".into(), |p| format!("localhost:{p}"))
                        }),
                ),
            ),
            ("Environment", Some(string(record, "environment").into())),
            ("Dependencies", Some(string(record, "dependencies").into())),
        ],
        Some(if status == "preview" {
            "Environment preview only; no registration or files changed. Use --apply after review."
        } else {
            "Tracked with external setup; manual services remain externally owned. Only explicitly selected stages run."
        }),
        status,
        columns,
    );
    let mut lines = vec![text];
    if let Some(entries) = report["environment"]["entries"].as_array() {
        for entry in entries {
            lines.push(format!(
                "  {}: {}; {}",
                string(entry, "key"),
                terminal::status_color(string(entry, "status")),
                terminal::status_color(string(entry, "action"))
            ));
        }
    }
    if let Some(backup) = report["environment"]["backupPath"].as_str() {
        lines.push(format!(
            "Private recovery backup: {}",
            terminal::display(backup)
        ));
    }
    for hook in record["hooks"].as_array().unwrap() {
        lines.push(format!(
            "  {}: {}",
            terminal::display(&format!(
                "{}/{}",
                string(hook, "phase"),
                string(hook, "name")
            )),
            terminal::status_color(string(hook, "status"))
        ));
    }
    terminal::wrap(&lines.join("\n"), columns)
}
