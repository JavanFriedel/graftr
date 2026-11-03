use crate::{
    config,
    paths::Paths,
    records,
    status::compare,
    system,
    terminal::{status_color, wrap},
    validation::*,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;

#[derive(Clone, Serialize, Deserialize)]
pub struct Worktree {
    pub path: String,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    pub locked: Option<String>,
    pub prunable: Option<String>,
}

pub fn parse(output: &str) -> Result<Vec<Worktree>> {
    ensure(output.ends_with("\0\0"), "Invalid Git worktree inventory")?;
    output[..output.len() - 2]
        .split("\0\0")
        .map(|record| {
            let mut fields = record.split('\0');
            let path = fields
                .next()
                .and_then(|s| s.strip_prefix("worktree "))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| Error::from("Missing Git worktree path"))?;
            let mut tree = Worktree {
                path: path.into(),
                head: None,
                branch: None,
                bare: false,
                detached: false,
                locked: None,
                prunable: None,
            };
            for field in fields {
                let (key, value) = field.split_once(' ').unwrap_or((field, ""));
                match key {
                    "HEAD" => tree.head = Some(value.into()),
                    "branch" => {
                        tree.branch =
                            Some(value.strip_prefix("refs/heads/").unwrap_or(value).into())
                    }
                    "bare" => tree.bare = true,
                    "detached" => tree.detached = true,
                    "locked" => tree.locked = Some(value.into()),
                    "prunable" => tree.prunable = Some(value.into()),
                    _ => (),
                }
            }
            Ok(tree)
        })
        .collect()
}
pub fn registered(repo: &str) -> Result<Vec<Worktree>> {
    parse(
        &system::run(
            &["git", "-C", repo, "worktree", "list", "--porcelain", "-z"],
            None,
            true,
        )?
        .stdout,
    )
}
pub fn list(paths: &Paths, project: &str) -> Result<Value> {
    let profile = config::settings(paths, project)?;
    let repo = string(&profile, "repo")?;
    let root = string(&profile, "worktreeRoot")?;
    ensure(
        absolute_normalized(repo) && absolute_normalized(root),
        "Inventory paths must be absolute and normalized",
    )?;
    let registered = registered(repo)?;
    let tasks: Vec<_> = records::tasks(&paths.state)?
        .into_iter()
        .filter(|t| t.value()["project"] == project || t.value()["repo"] == repo)
        .collect();
    let adopted: Vec<_> = records::adoptions(&paths.state)?
        .into_iter()
        .filter(|t| t.value()["project"] == project || t.value()["repo"] == repo)
        .collect();
    let setups = records::setups(&paths.state)?;
    let mut candidates: Vec<String> = registered
        .iter()
        .map(|t| t.path.clone())
        .chain(
            tasks
                .iter()
                .map(|t| t.value()["path"].as_str().unwrap().into()),
        )
        .chain(
            adopted
                .iter()
                .map(|t| t.value()["path"].as_str().unwrap().into()),
        )
        .collect();
    match fs::read_dir(root) {
        Ok(children) => {
            for child in children {
                let child = child.map_err(|_| Error::from("Could not read worktree root"))?;
                if child
                    .file_type()
                    .map_err(|_| Error::from("Could not read worktree root"))?
                    .is_dir()
                    && crate::paths::exists(&child.path().join(".git"))?
                {
                    candidates.push(child.path().to_string_lossy().into_owned());
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(_) => return Err("Could not read worktree root".into()),
    }
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|path| seen.insert(path.clone()));
    let main = registered.first().map(|t| t.path.as_str());
    let mut worktrees = Vec::new();
    for path in candidates {
        let git = registered.iter().find(|tree| tree.path == path);
        let tasks: Vec<_> = tasks
            .iter()
            .map(records::Task::value)
            .filter(|t| t["path"] == path)
            .collect();
        let adoptions: Vec<_> = adopted
            .iter()
            .map(records::Adoption::value)
            .filter(|t| t["path"] == path)
            .collect();
        let present = match fs::metadata(&path) {
            Ok(data) => data.is_dir(),
            Err(e) if [Some(libc::ENOENT), Some(libc::ENOTDIR)].contains(&e.raw_os_error()) => {
                false
            }
            Err(_) => return Err("Could not inspect checkout directory".into()),
        };
        let mut issues = Vec::new();
        if !present {
            issues.push("missing-checkout");
        }
        if git.is_none() {
            issues.push("not-registered");
        }
        if git.is_some_and(|g| g.prunable.is_some()) {
            issues.push("prunable");
        }
        if git.is_some_and(|g| {
            tasks
                .iter()
                .chain(&adoptions)
                .any(|t| t["branch"].as_str() != g.branch.as_deref())
        }) {
            issues.push("branch-mismatch");
        }
        if tasks.len() + adoptions.len() > 1 {
            issues.push("duplicate-records");
        }
        if tasks.iter().chain(&adoptions).any(|t| t["repo"] != repo) {
            issues.push("repository-mismatch");
        }
        let fallback = Worktree {
            path: path.clone(),
            head: None,
            branch: None,
            bare: false,
            detached: false,
            locked: None,
            prunable: None,
        };
        let mut entry = serde_json::to_value(git.unwrap_or(&fallback)).unwrap();
        let object = entry.as_object_mut().unwrap();
        object.insert(
            "tracking".into(),
            json!(if main == Some(&path) {
                "main"
            } else if !tasks.is_empty() {
                "managed"
            } else if !adoptions.is_empty() {
                "adopted"
            } else if git.is_some() {
                "unmanaged"
            } else {
                "unregistered"
            }),
        );
        object.insert("registered".into(), json!(git.is_some()));
        object.insert("present".into(), json!(present));
        object.insert("issues".into(), json!(issues));
        object.insert(
            "tasks".into(),
            json!(
                tasks
                    .iter()
                    .map(|task| {
                        let mut data = Object::new();
                        for key in [
                            "id",
                            "branch",
                            "status",
                            "localPort",
                            "httpsPort",
                            "url",
                            "ports",
                        ] {
                            if let Some(value) = task.get(key) {
                                data.insert(key.into(), value.clone());
                            }
                        }
                        Value::Object(data)
                    })
                    .collect::<Vec<_>>()
            ),
        );
        if let Some(setup) = setups
            .iter()
            .map(records::Setup::value)
            .find(|r| r["path"] == path)
        {
            object.insert("setup".into(), json!({"status": setup["status"], "step": setup["step"], "dependencies": setup["dependencies"], "message": setup["message"]}));
        }
        if let Some(adoption) = adoptions.first() {
            object.insert("adoption".into(), (*adoption).clone());
        }
        worktrees.push(entry);
    }
    worktrees.sort_by(|a, b| {
        (b["path"].as_str() == main)
            .cmp(&(a["path"].as_str() == main))
            .then_with(|| compare(a["path"].as_str().unwrap(), b["path"].as_str().unwrap()))
    });
    Ok(json!({"project": project, "repo": repo, "worktreeRoot": root, "worktrees": worktrees}))
}

fn display(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c <= '\u{1f}' || ('\u{7f}'..='\u{9f}').contains(&c) {
                format!("\\u{:04x}", u32::from(c))
            } else {
                c.to_string()
            }
        })
        .collect()
}
pub fn format(data: &Value, columns: usize) -> String {
    let worktrees = data["worktrees"].as_array().unwrap();
    let mut lines = vec![format!(
        "{}: {} checkouts / entries",
        display(data["project"].as_str().unwrap()),
        worktrees.len()
    )];
    for tree in worktrees {
        let branch = tree["branch"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if tree["bare"] == true {
                    "(bare)".into()
                } else if tree["detached"] == true {
                    format!(
                        "(detached {})",
                        tree["head"]
                            .as_str()
                            .map(|s| &s[..s.len().min(8)])
                            .unwrap_or("HEAD")
                    )
                } else {
                    "(unknown branch)".into()
                }
            });
        lines.extend([
            "".into(),
            format!(
                "{:<12} {}",
                tree["tracking"].as_str().unwrap(),
                display(&branch)
            ),
            format!("  {}", display(tree["path"].as_str().unwrap())),
        ]);
        for task in tree["tasks"].as_array().unwrap() {
            let ports = if task["httpsPort"].is_null() {
                format!("port {}", task["localPort"].as_f64().unwrap())
            } else {
                format!(
                    "ports {}/{}",
                    task["localPort"].as_f64().unwrap(),
                    task["httpsPort"].as_f64().unwrap()
                )
            };
            let named = task["ports"]
                .as_object()
                .map(|ports| {
                    ports
                        .iter()
                        .map(|(name, port)| format!("  {name} {}", port.as_f64().unwrap()))
                        .collect::<String>()
                })
                .unwrap_or_default();
            lines.push(format!(
                "  {}  {}  {ports}{named}",
                display(task["id"].as_str().unwrap()),
                status_color(task["status"].as_str().unwrap())
            ));
        }
        if let Some(setup) = tree.get("setup") {
            lines.extend([
                format!(
                    "  setup {} ({}); dependencies {}",
                    status_color(setup["status"].as_str().unwrap()),
                    setup["step"].as_str().unwrap(),
                    status_color(setup["dependencies"].as_str().unwrap())
                ),
                format!("  {}", display(setup["message"].as_str().unwrap())),
            ]);
        }
        if let Some(adopted) = tree.get("adoption") {
            let address = adopted["url"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    adopted["localPort"]
                        .as_f64()
                        .map(|p| format!("localhost:{p}"))
                        .unwrap_or_else(|| "Address not recorded".into())
                });
            lines.extend([
                format!(
                    "  {}  {}; setup {}",
                    display(adopted["id"].as_str().unwrap()),
                    status_color("adopted"),
                    status_color("external")
                ),
                format!("  {}", display(&address)),
            ]);
        }
        if let Some(locked) = tree["locked"].as_str() {
            lines.push(format!(
                "  locked{}",
                if locked.is_empty() {
                    String::new()
                } else {
                    format!(": {}", display(locked))
                }
            ));
        }
        let issues = tree["issues"].as_array().unwrap();
        if !issues.is_empty() {
            lines.push(format!(
                "  REVIEW: {}{}",
                issues
                    .iter()
                    .map(|v| v.as_str().unwrap())
                    .collect::<Vec<_>>()
                    .join(", "),
                tree["prunable"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|s| format!(" — {}", display(s)))
                    .unwrap_or_default()
            ));
        }
    }
    lines.extend([
        "".into(),
        format!(
            "{} entries need review. Unmanaged does not mean orphaned.",
            worktrees
                .iter()
                .filter(|t| !t["issues"].as_array().unwrap().is_empty())
                .count()
        ),
    ]);
    wrap(&lines.join("\n"), columns)
}

#[cfg(test)]
mod tests {
    #[test]
    fn inventory_keeps_nul_delimited_paths_and_empty_lock_reasons() {
        let trees = super::parse("worktree /fixture/a\nspace\0HEAD abc\0branch refs/heads/topic\0locked\0unknown value\0\0").unwrap();
        assert_eq!(trees[0].path, "/fixture/a\nspace");
        assert_eq!(trees[0].locked.as_deref(), Some(""));
        assert_eq!(trees[0].branch.as_deref(), Some("topic"));
        assert!(super::parse("worktree broken\n").is_err());
    }
}
