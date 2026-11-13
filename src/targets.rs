use crate::{
    herdr, inventory,
    paths::{self, Paths},
    records::{self, Adoption, Task},
    system,
    validation::{self, *},
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct TargetError {
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}
impl From<validation::Error> for TargetError {
    fn from(error: validation::Error) -> Self {
        Self {
            code: error.code,
            message: error.message,
            details: Value::Null,
        }
    }
}
impl From<&str> for TargetError {
    fn from(message: &str) -> Self {
        Self {
            code: "OPERATION_FAILED",
            message: message.into(),
            details: Value::Null,
        }
    }
}
impl From<String> for TargetError {
    fn from(message: String) -> Self {
        Self {
            code: "OPERATION_FAILED",
            message,
            details: Value::Null,
        }
    }
}
type TargetResult<T> = std::result::Result<T, TargetError>;

pub fn basename(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}
pub fn at_directory(task: &Value, path: &str) -> bool {
    task["path"] == path || path.starts_with(&format!("{}/", task["path"].as_str().unwrap()))
}
pub fn resolve_path(cwd: &Path, value: &str) -> Result<PathBuf> {
    let expanded = if let Some(rest) = value.strip_prefix("~/") {
        paths::home()?.join(rest)
    } else {
        PathBuf::from(value)
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    Ok(PathBuf::from(normalized_path(&absolute.to_string_lossy())))
}
pub fn explicit_path(value: &str) -> bool {
    value.starts_with('/')
        || value == "."
        || value == ".."
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with("~/")
        || value.ends_with('/')
}
pub fn at_path(
    tasks: &[Task],
    selector: &str,
    cwd: &Path,
    project: Option<&str>,
    reject_file: bool,
) -> Result<Vec<usize>> {
    record_paths(
        &tasks.iter().map(Task::value).collect::<Vec<_>>(),
        selector,
        cwd,
        project,
        reject_file,
    )
}
fn record_paths(
    records: &[&Value],
    selector: &str,
    cwd: &Path,
    project: Option<&str>,
    reject_file: bool,
) -> Result<Vec<usize>> {
    let mut path = resolve_path(cwd, selector)?;
    let mut present = false;
    match fs::canonicalize(&path) {
        Ok(canonical) => {
            path = canonical;
            present = fs::metadata(&path)
                .map_err(|e| Error::from(e.to_string()))?
                .is_dir();
            if !present {
                ensure(
                    !reject_file,
                    "Target path must be a managed worktree directory, not a file",
                )?;
                return Ok(vec![]);
            }
        }
        Err(e) if [Some(libc::ENOENT), Some(libc::ENOTDIR)].contains(&e.raw_os_error()) => (),
        Err(e) => return Err(e.to_string().into()),
    }
    let path = path.to_string_lossy();
    Ok(records
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            (if present {
                at_directory(t, &path)
            } else {
                t["path"].as_str() == Some(&path)
            }) && project.is_none_or(|project| t["project"] == project)
        })
        .map(|(index, _)| index)
        .collect())
}
pub fn short_matches(tasks: &[Task], selector: &str, project: Option<&str>) -> Vec<usize> {
    record_aliases(
        &tasks.iter().map(Task::value).collect::<Vec<_>>(),
        selector,
        project,
    )
}
fn record_aliases(records: &[&Value], selector: &str, project: Option<&str>) -> Vec<usize> {
    records
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            project.is_none_or(|project| t["project"] == project)
                && (basename(t["path"].as_str().unwrap()) == selector || t["branch"] == selector)
        })
        .map(|(i, _)| i)
        .collect()
}
pub fn infer_project(tasks: &[Task], cwd: &Path, project: Option<&str>) -> Result<Option<String>> {
    record_project(
        &tasks.iter().map(Task::value).collect::<Vec<_>>(),
        cwd,
        project,
    )
}
fn record_project(records: &[&Value], cwd: &Path, project: Option<&str>) -> Result<Option<String>> {
    if let Some(project) = project {
        name(project)?;
        return Ok(Some(project.into()));
    }
    let cwd_text = cwd.to_string_lossy();
    let mut candidates: Vec<_> = records
        .iter()
        .filter(|t| at_directory(t, &cwd_text))
        .collect();
    if candidates.is_empty() {
        let result = system::run(
            &[
                "git",
                "-C",
                &cwd_text,
                "worktree",
                "list",
                "--porcelain",
                "-z",
            ],
            None,
            false,
        )?;
        if result.exit_code == 0
            && let Some(main) = inventory::parse(&result.stdout)?.first()
        {
            candidates = records.iter().filter(|t| t["repo"] == main.path).collect();
        }
    }
    let projects: HashSet<_> = candidates
        .iter()
        .map(|t| t["project"].as_str().unwrap())
        .collect();
    ensure(
        projects.len() <= 1,
        "Current directory belongs to multiple managed projects; use --project",
    )?;
    Ok(projects.into_iter().next().map(str::to_owned))
}
fn require_project(task: Task, project: Option<&str>) -> TargetResult<Task> {
    require_record_project(task.value(), project)?;
    Ok(task)
}
fn require_record_project(task: &Value, project: Option<&str>) -> TargetResult<()> {
    if let Some(project) = project
        && task["project"] != project
    {
        return Err(format!(
            "Worktree {} belongs to project {}, not {}",
            task["id"],
            task["project"],
            json!(project)
        )
        .into());
    }
    Ok(())
}
fn ambiguous(selector: &str, tasks: &[&Value]) -> TargetError {
    let candidates: Vec<_> = tasks.iter().map(|task| {
        json!({"id": task["id"], "project": task["project"], "branch": task["branch"], "path": task["path"]})
    }).collect();
    let choices = candidates
        .iter()
        .map(|c| {
            format!(
                "  {{\"id\":{},\"project\":{},\"branch\":{},\"path\":{}}}",
                c["id"], c["project"], c["branch"], c["path"]
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    TargetError {
        code: "TARGET_AMBIGUOUS",
        message: format!(
            "Ambiguous worktree target {}; use --project, a full ID, or an exact checkout path:\n{choices}",
            json!(selector)
        ),
        details: json!({"candidates": candidates}),
    }
}
pub fn resolve_managed(
    paths: &Paths,
    selector: Option<&str>,
    project: Option<&str>,
) -> TargetResult<Option<Task>> {
    if let Some(project) = project {
        name(project)?;
    }
    if let Some(selector) = selector
        && id(selector).is_ok()
        && paths::exists(&paths.state.join(format!("{selector}.json")))?
    {
        return require_project(records::task(&paths.state, selector)?, project).map(Some);
    }
    let tasks = records::tasks(&paths.state)?;
    let values = tasks.iter().map(Task::value).collect::<Vec<_>>();
    select_record(&values, selector, project).map(|index| index.map(|index| tasks[index].clone()))
}
// Selection over validated records also serves tolerant, read-only reports. Strict
// lifecycle callers still load their records through resolve_managed above.
pub fn select_record(
    records: &[&Value],
    selector: Option<&str>,
    project: Option<&str>,
) -> TargetResult<Option<usize>> {
    if let Some(project) = project {
        name(project)?;
    }
    let exact: Vec<_> = records
        .iter()
        .enumerate()
        .filter(|(_, record)| record["id"].as_str() == selector)
        .map(|(index, _)| index)
        .collect();
    if exact.len() > 1 {
        return Err(ambiguous(
            selector.unwrap(),
            &exact
                .iter()
                .map(|index| records[*index])
                .collect::<Vec<_>>(),
        ));
    }
    if let Some(index) = exact.first() {
        require_record_project(records[*index], project)?;
        return Ok(Some(*index));
    }
    let cwd = env::current_dir()
        .and_then(fs::canonicalize)
        .map_err(|_| TargetError::from("Current directory is unavailable"))?;
    if selector.is_none() || selector == Some("@") {
        let matches: Vec<_> = records
            .iter()
            .enumerate()
            .filter(|(_, t)| at_directory(t, &cwd.to_string_lossy()))
            .map(|(index, _)| index)
            .collect();
        if matches.len() > 1 {
            return Err(ambiguous(
                "current directory",
                &matches.iter().map(|i| records[*i]).collect::<Vec<_>>(),
            ));
        }
        if let Some(index) = matches.first() {
            require_record_project(records[*index], project)?;
            return Ok(Some(*index));
        }
        return Ok(None);
    }
    let selector = selector.unwrap();
    let explicit = explicit_path(selector);
    let paths_ = record_paths(records, selector, &cwd, project, explicit)?;
    let matches = if explicit {
        paths_
    } else {
        let scope = record_project(records, &cwd, project)?;
        let mut matches: Vec<_> = paths_
            .into_iter()
            .filter(|i| {
                scope
                    .as_deref()
                    .is_none_or(|project| records[*i]["project"] == project)
            })
            .chain(record_aliases(records, selector, scope.as_deref()))
            .collect();
        let mut seen = HashSet::new();
        matches.retain(|index| seen.insert(*index));
        matches
    };
    if matches.len() > 1 {
        return Err(ambiguous(
            selector,
            &matches.iter().map(|i| records[*i]).collect::<Vec<_>>(),
        ));
    }
    if let Some(index) = matches.first() {
        return Ok(Some(*index));
    }
    Err(TargetError {
        code: "TARGET_NOT_FOUND",
        message: format!(
            "No managed worktree matches {}{}; use graftr list PROJECT to inspect targets",
            json!(selector),
            project
                .map(|p| format!(" in project {}", json!(p)))
                .unwrap_or_default()
        ),
        details: Value::Null,
    })
}
pub fn adopted(
    paths: &Paths,
    selector: Option<&str>,
    project: Option<&str>,
) -> TargetResult<Option<Adoption>> {
    if let Some(project) = project {
        name(project)?;
    }
    if let Some(selector) = selector
        && id(selector).is_ok()
        && paths::exists(&paths.state.join(format!("{selector}.json")))?
    {
        return Ok(None);
    }
    let records: Vec<_> = records::adoptions(&paths.state)?
        .into_iter()
        .filter(|r| project.is_none_or(|p| r.value()["project"] == p))
        .collect();
    if let Some(record) = records
        .iter()
        .find(|r| r.value()["id"].as_str() == selector)
    {
        return Ok(Some(record.clone()));
    }
    if records.is_empty() {
        return Ok(None);
    }
    let inferred = selector.is_none() || selector == Some("@");
    let directory = if inferred || selector.is_some_and(explicit_path) {
        let cwd = env::current_dir()
            .map_err(|_| TargetError::from("Current directory is unavailable"))?;
        Some(
            if inferred {
                cwd
            } else {
                resolve_path(&cwd, selector.unwrap())?
            }
            .to_string_lossy()
            .into_owned(),
        )
    } else {
        None
    };
    let matches: Vec<_> = records
        .iter()
        .filter(|r| {
            if let Some(directory) = &directory {
                at_directory(r.value(), directory)
            } else {
                selector.is_some_and(|selector| {
                    r.value()["branch"] == selector
                        || basename(r.value()["path"].as_str().unwrap()) == selector
                })
            }
        })
        .collect();
    if matches.len() > 1 {
        return Err(TargetError {
            code: "TARGET_AMBIGUOUS",
            message: format!(
                "Ambiguous adopted target; choose a full ID or path: {}",
                matches
                    .iter()
                    .map(|r| r.value()["id"].as_str().unwrap())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            details: Value::Null,
        });
    }
    Ok(matches.first().map(|r| (*r).clone()))
}
pub fn resolve_id(
    paths: &Paths,
    selector: Option<&str>,
    project: Option<&str>,
) -> TargetResult<String> {
    if adopted(paths, selector, project)?.is_some() {
        return Err(TargetError { code: "ADOPTED_EXTERNAL", message: "This worktree is tracked with external setup. Use graftr adopt PATH for explicit setup stages; automatic setup, cleanup, removal and freeze are not enabled for adopted worktrees. Existing files and manual services were preserved.".into(), details: Value::Null });
    }
    if (selector.is_none() || selector == Some("@")) && herdr::in_plugin_action() {
        return current_id(paths, project);
    }
    if let Some(task) = resolve_managed(paths, selector, project)? {
        return Ok(task.value()["id"].as_str().unwrap().into());
    }
    current_id(paths, project)
}
fn current_id(paths: &Paths, project: Option<&str>) -> TargetResult<String> {
    let socket = herdr::caller_socket()?;
    let workspace = herdr::caller_workspace(Some(&socket))?;
    let records = records::tasks(&paths.state)?;
    let matches: Vec<_> = records
        .iter()
        .filter(|t| {
            t.value()["socket"] == socket
                && t.value()["workspaceId"].as_str() == workspace.as_deref()
        })
        .collect();
    if matches.is_empty() && workspace.is_some() {
        return Err(TargetError {
            code: "WORKSPACE_UNMANAGED",
            message: "This workspace does not own a Graftr task; use graftr close in an interactive terminal to close only its Herdr workspace".into(),
            details: json!({"socket": socket, "workspaceId": workspace}),
        });
    }
    if matches.len() != 1 {
        return Err("This workspace does not own a task; provide PROJECT--TASK".into());
    }
    let task = require_project(matches[0].clone(), project)?;
    Ok(task.value()["id"].as_str().unwrap().into())
}
