use crate::{
    inventory::{self, Worktree},
    system,
    validation::*,
};
use serde_json::{Value, json};
use std::path::Path;

pub fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().expect("validated task field")
}
pub fn identity(value: &Value) -> Value {
    let mut result = json!({});
    for key in [
        "id",
        "path",
        "repo",
        "branch",
        "socket",
        "workspaceId",
        "baseCommit",
    ] {
        result[key] = value[key].clone();
    }
    if let Some(run) = value
        .get("cleanup")
        .and_then(|p| p.get("runId"))
        .or_else(|| value.get("cleanupRunId"))
    {
        result["cleanupRunId"] = run.clone();
    }
    result
}
pub fn require_identity(task: &Value, expected: &Value) -> Result<()> {
    ensure(
        identity(task) == identity(expected),
        "Confirmed removal target changed; review and confirm again",
    )
}
pub fn canonical(path: &str) -> Result<String> {
    let path = Path::new(path);
    match std::fs::symlink_metadata(path) {
        Ok(_) => std::fs::canonicalize(path)
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|_| "Could not establish canonical checkout path".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| Error::from("Could not establish canonical checkout path"))?;
            let name = path
                .file_name()
                .ok_or_else(|| Error::from("Could not establish canonical checkout path"))?;
            Ok(Path::new(&canonical(&parent.to_string_lossy())?)
                .join(name)
                .to_string_lossy()
                .into_owned())
        }
        Err(_) => Err("Could not establish canonical checkout path".into()),
    }
}
pub fn verify(task: &Value) -> Result<()> {
    let path = Path::new(string(task, "path"));
    ensure(
        system::git(path, &["rev-parse", "--show-toplevel"])? == string(task, "path")
            && system::git(
                path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )? == system::git(
                Path::new(string(task, "repo")),
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )?,
        "Checkout does not belong to the recorded repository",
    )
}
pub fn registered(task: &Value) -> Result<Option<Worktree>> {
    let trees = inventory::registered(string(task, "repo"))?;
    ensure(
        task["path"] != task["repo"]
            && !trees
                .first()
                .is_some_and(|tree| tree.path == string(task, "path")),
        "The main checkout cannot be removed",
    )?;
    Ok(trees
        .into_iter()
        .find(|tree| tree.path == string(task, "path")))
}
pub fn observe(task: &Value) -> Result<Value> {
    for key in ["repo", "path"] {
        let path = string(task, key);
        ensure(
            absolute_normalized(path) && canonical(path)? == path,
            "Checkout paths must be absolute and canonical; symlinks are not adopted",
        )?;
    }
    let tree = registered(task)?;
    let present = crate::paths::exists(Path::new(string(task, "path")))?;
    ensure(
        tree.is_some() || !present,
        "Checkout is not registered with its recorded repository",
    )?;
    let mut result = identity(task);
    let Some(tree) = tree else {
        result["currentBranch"] = Value::Null;
        result["currentCommit"] = Value::Null;
        return Ok(result);
    };
    ensure(
        !tree.bare && !tree.detached && tree.branch.is_some(),
        "Worktree is detached or has no attached local branch; review it first",
    )?;
    if present {
        verify(task)?;
    }
    let branch = tree.branch.unwrap();
    let reference = format!("refs/heads/{branch}");
    let commit = crate::branches::local_commit(string(task, "repo"), &branch)?.ok_or_else(|| Error::from("Current checkout branch/commit cannot be verified as retained in the recorded repository"))?;
    ensure(
        !commit.is_empty()
            && (!present
                || system::git(
                    Path::new(string(task, "path")),
                    &["symbolic-ref", "--quiet", "HEAD"],
                )? == reference
                    && system::git(
                        Path::new(string(task, "path")),
                        &["rev-parse", "HEAD^{commit}"],
                    )? == commit),
        "Current checkout branch/commit cannot be verified as retained in the recorded repository",
    )?;
    result["currentBranch"] = json!(branch);
    result["currentCommit"] = json!(commit);
    Ok(result)
}
