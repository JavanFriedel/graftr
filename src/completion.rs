use crate::{
    config, help::Catalog, inventory, paths::Paths, records, system, targets, validation::*,
};
use serde_json::Value;
use std::{collections::HashSet, env, fs, path::Path};

fn relative(from: &Path, to: &str) -> Result<String> {
    let to = targets::resolve_path(from, to)?;
    let from = from.to_string_lossy();
    let to = to.to_string_lossy();
    let from: Vec<_> = from.split('/').filter(|s| !s.is_empty()).collect();
    let to: Vec<_> = to.split('/').filter(|s| !s.is_empty()).collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    Ok(std::iter::repeat_n("..", from.len() - common)
        .chain(to[common..].iter().copied())
        .collect::<Vec<_>>()
        .join("/"))
}
fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(_)) => "[object Object]".into(),
        Some(Value::Array(values)) => values
            .iter()
            .map(|v| {
                if v.is_null() {
                    String::new()
                } else {
                    js_string(Some(v))
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(value) => value.to_string(),
    }
}
pub fn complete(paths: &Paths, words: &[String], catalog: &Catalog) -> Result<Vec<String>> {
    let current = words.last().map(String::as_str).unwrap_or("");
    let mut command: Option<String> = None;
    let mut pending: Option<String> = None;
    let mut project: Option<String> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut used = HashSet::new();
    for word in words.iter().take(words.len().saturating_sub(1)) {
        if let Some(option) = pending.as_deref() {
            if word == "=" {
                continue;
            }
            if option == "project" {
                project = Some(word.clone());
            }
            pending = None;
            continue;
        }
        if let Some(option) = word.strip_prefix("--") {
            let (key, value) = option
                .split_once('=')
                .map_or((option, None), |(k, v)| (k, Some(v)));
            if let Some(spec) = catalog.cli_options.get(key) {
                used.insert(format!("--{key}"));
                if key == "project"
                    && let Some(value) = value
                {
                    project = Some(value.into());
                }
                if value.is_none() && spec.kind == "string" {
                    pending = Some(key.into());
                }
            }
        } else if !word.starts_with('-') {
            if command.is_none() {
                command = Some(word.clone());
            } else {
                positionals.push(word.clone());
            }
        }
    }
    let inline = current
        .strip_prefix("--")
        .and_then(|s| s.split_once('='))
        .filter(|(key, _)| {
            [
                "project",
                "base",
                "branch",
                "take-baseline",
                "until",
                "note",
                "hook",
                "to",
                "include",
                "approve-project",
                "destination-config",
            ]
            .contains(key)
        });
    if let Some((key, _)) = inline {
        pending = Some(key.into());
    }
    let prefix = inline.map(|(_, value)| value).unwrap_or(current);
    let finish = |values: Vec<String>| -> Vec<String> {
        let mut seen = HashSet::new();
        let mut values: Vec<_> = values
            .into_iter()
            .filter(|value| {
                !value.chars().any(control)
                    && value.starts_with(prefix)
                    && seen.insert(value.clone())
            })
            .collect();
        // JavaScript completion uses UTF-16 lexical order, not locale collation/scalar order.
        values.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        values
            .into_iter()
            .map(|value| {
                if inline.is_some() {
                    format!("--{}={value}", pending.as_deref().unwrap())
                } else {
                    value
                }
            })
            .collect()
    };
    let command = command.as_deref();
    let pending_ = pending.as_deref();
    if pending_.is_none() && command.is_none() {
        return Ok(finish(
            catalog
                .public_commands()
                .into_iter()
                .chain([
                    "--help".into(),
                    "-h".into(),
                    "--version".into(),
                    "-V".into(),
                ])
                .collect(),
        ));
    }
    if pending_.is_none() && command == Some("help") {
        return Ok(if positionals.is_empty() {
            finish(
                catalog
                    .public_commands()
                    .into_iter()
                    .chain(
                        [
                            "getting-started",
                            "recovery",
                            "targeting",
                            "output",
                            "naming",
                            "hooks",
                        ]
                        .map(str::to_owned),
                    )
                    .collect(),
            )
        } else {
            vec![]
        });
    }
    if pending_.is_none() && command == Some("completion") {
        return Ok(if positionals.is_empty() {
            finish(["bash", "--help", "-h"].map(str::to_owned).to_vec())
        } else {
            vec![]
        });
    }
    if pending_.is_none() && current.starts_with('-') {
        return Ok(finish(
            catalog
                .command_flags(command.unwrap_or(""))
                .into_iter()
                .filter(|flag| {
                    !used.contains(flag)
                        || flag == "--take-baseline"
                        || flag == "--hook"
                        || flag == "--include"
                })
                .collect(),
        ));
    }
    if pending_.is_none() && command == Some("plugin") {
        return Ok(finish(if positionals.is_empty() {
            vec!["install".into()]
        } else {
            vec![]
        }));
    }
    let projects = config::projects(paths).unwrap_or_default();
    if pending_ == Some("project")
        || pending_.is_none()
            && [Some("create"), Some("list"), Some("frozen")].contains(&command)
            && positionals.is_empty()
    {
        return Ok(finish(projects.keys().cloned().collect()));
    }
    let cwd = env::current_dir()
        .and_then(fs::canonicalize)
        .map_err(|_| Error::from("Current directory is unavailable"))?;
    if command == Some("adopt") && pending_ == Some("hook") {
        let mut candidates = Vec::new();
        for (name, settings) in &projects {
            if project.as_deref().is_some_and(|project| project != name) {
                continue;
            }
            let settings = object(settings)?;
            let empty = Value::Object(Object::new());
            let hooks = object(settings.get("hooks").unwrap_or(&empty))?;
            for phase in ["afterEnvironment", "afterDependencies"] {
                if let Some(hooks) = hooks.get(phase).and_then(Value::as_array) {
                    for hook in hooks {
                        candidates
                            .push(format!("{phase}/{}", js_string(object(hook)?.get("name"))));
                    }
                }
            }
        }
        return Ok(finish(candidates));
    }
    if command == Some("adopt") && pending_.is_none() && positionals.is_empty() {
        let mut candidates = Vec::new();
        for (name, settings) in &projects {
            if project.as_deref().is_some_and(|project| project != name) {
                continue;
            }
            let Some(repo) = object(settings)?.get("repo").and_then(Value::as_str) else {
                continue;
            };
            let result = system::run(
                &["git", "-C", repo, "worktree", "list", "--porcelain", "-z"],
                None,
                false,
            )?;
            if result.exit_code == 0 {
                candidates.extend(
                    inventory::parse(&result.stdout)?
                        .into_iter()
                        .skip(1)
                        .filter(|tree| !tree.bare && !tree.detached)
                        .map(|tree| format!("{}/", tree.path)),
                );
            }
        }
        return Ok(finish(candidates));
    }
    if [Some("base"), Some("branch")].contains(&pending_) {
        let settings = positionals.first().and_then(|name| projects.get(name));
        let repo = match settings {
            Some(value) if value.is_object() || value.is_array() => object(value)?
                .get("repo")
                .and_then(Value::as_str)
                .map(str::to_owned),
            _ => None,
        }
        .unwrap_or_else(|| cwd.to_string_lossy().into_owned());
        let output = match system::git(
            Path::new(&repo),
            &["for-each-ref", "--format=%(refname:strip=2)", "refs/heads"],
        ) {
            Ok(output) => output,
            Err(_) => return Ok(vec![]),
        };
        let mut branches: Vec<_> = output
            .trim_matches(js_space)
            .split('\n')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if pending_ == Some("branch") {
            let result = system::run(
                &["git", "-C", &repo, "worktree", "list", "--porcelain", "-z"],
                None,
                false,
            )?;
            if result.exit_code != 0 {
                return Ok(vec![]);
            }
            let mut checked: HashSet<_> = inventory::parse(&result.stdout)?
                .into_iter()
                .filter_map(|t| t.branch)
                .collect();
            for record in records::frozen(&paths.state)?
                .into_iter()
                .filter(|r| r.reserves())
            {
                if record.value()["removal"]["repo"] == repo {
                    checked.insert(
                        record.value()["removal"]["currentBranch"]
                            .as_str()
                            .unwrap()
                            .into(),
                    );
                }
            }
            branches.retain(|branch| !checked.contains(branch));
        } else {
            branches.push("HEAD".into());
        }
        return Ok(finish(branches));
    }
    if pending_.is_some() {
        return Ok(vec![]);
    }
    if command == Some("env") && positionals.is_empty() {
        return Ok(finish(vec!["refresh".into()]));
    }
    if command == Some("hook") && positionals.is_empty() {
        return Ok(finish(vec!["postgres".into(), "tailscale".into()]));
    }
    if [Some("thaw"), Some("schedule")].contains(&command) && positionals.is_empty() {
        let records: Vec<_> = records::frozen(&paths.state)?
            .into_iter()
            .filter(|r| {
                r.reserves()
                    && project.as_deref().is_none_or(|p| {
                        r.value()["id"]
                            .as_str()
                            .unwrap()
                            .starts_with(&format!("{p}--"))
                    })
            })
            .collect();
        let mut candidates: Vec<_> = records
            .iter()
            .flat_map(|r| {
                [
                    r.value()["id"].as_str().unwrap().into(),
                    r.value()["removal"]["path"].as_str().unwrap().into(),
                ]
            })
            .collect();
        let aliases: HashSet<_> = records
            .iter()
            .flat_map(|r| {
                [
                    targets::basename(r.value()["removal"]["path"].as_str().unwrap()),
                    r.value()["removal"]["currentBranch"].as_str().unwrap(),
                ]
            })
            .collect();
        for alias in aliases {
            if records
                .iter()
                .filter(|r| {
                    targets::basename(r.value()["removal"]["path"].as_str().unwrap()) == alias
                        || r.value()["removal"]["currentBranch"] == alias
                })
                .count()
                == 1
            {
                candidates.push(alias.into());
            }
        }
        return Ok(finish(candidates));
    }
    let Some(command) = command else {
        return Ok(vec![]);
    };
    if !catalog.target_command(command)
        || command == "env" && positionals.first().map(String::as_str) != Some("refresh")
        || positionals.len() >= if command == "env" { 2 } else { 1 }
    {
        return Ok(vec![]);
    }
    let tasks = match records::tasks(&paths.state) {
        Ok(tasks) => tasks,
        Err(_) => return Ok(vec![]),
    };
    let scope = targets::infer_project(&tasks, &cwd, project.as_deref())?;
    let mut candidates = Vec::new();
    for task in tasks.iter().filter(|t| {
        project
            .as_deref()
            .is_none_or(|project| t.value()["project"] == project)
    }) {
        let value = task.value();
        let path = value["path"].as_str().unwrap();
        let rel = relative(&cwd, path)?;
        candidates.extend([
            value["id"].as_str().unwrap().into(),
            format!("{path}/"),
            if rel.is_empty() {
                ".".into()
            } else if rel.starts_with("../") {
                format!("{rel}/")
            } else {
                format!("./{rel}/")
            },
        ]);
    }
    if ["logs", "doctor", "transfer"].contains(&command) {
        candidates.extend(
            records::adoptions(&paths.state)?
                .iter()
                .filter(|r| project.as_deref().is_none_or(|p| r.value()["project"] == p))
                .flat_map(|r| {
                    [
                        r.value()["id"].as_str().unwrap().into(),
                        format!("{}/", r.value()["path"].as_str().unwrap()),
                    ]
                }),
        );
    }
    let aliases: HashSet<_> = tasks
        .iter()
        .filter(|t| scope.as_deref().is_none_or(|p| t.value()["project"] == p))
        .flat_map(|t| {
            [
                targets::basename(t.value()["path"].as_str().unwrap()),
                t.value()["branch"].as_str().unwrap(),
            ]
        })
        .collect();
    for alias in aliases {
        let matches: HashSet<_> = targets::short_matches(&tasks, alias, scope.as_deref())
            .into_iter()
            .chain(targets::at_path(
                &tasks,
                alias,
                &cwd,
                scope.as_deref(),
                false,
            )?)
            .collect();
        if matches.len() == 1 && !tasks.iter().any(|t| t.value()["id"] == alias) {
            candidates.push(alias.into());
        }
    }
    if command != "nuke"
        && tasks
            .iter()
            .filter(|t| {
                project.as_deref().is_none_or(|p| t.value()["project"] == p)
                    && targets::at_directory(t.value(), &cwd.to_string_lossy())
            })
            .count()
            == 1
    {
        candidates.push("@".into());
    }
    Ok(finish(candidates))
}
