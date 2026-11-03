use crate::{system, validation::*};
use serde_json::{Value, json};
use std::path::Path;
pub fn merged(paths: &crate::paths::Paths, task: &Value) -> Result<Value> {
    let repo = task["repo"].as_str().unwrap();
    let branch = task["branch"].as_str().unwrap();
    let branch_commit = local_commit(repo, branch)?;
    let main = system::run(
        &["git", "-C", repo, "symbolic-ref", "--quiet", "HEAD"],
        None,
        false,
    )?;
    let merge_ref = main.stdout.trim_matches(crate::validation::js_space);
    ensure(
        main.exit_code == 0 && merge_ref.starts_with("refs/heads/"),
        "The main checkout must be on an attached local branch",
    )?;
    let defaults = system::git(
        Path::new(repo),
        &["for-each-ref", "--format=%(symref)", "refs/remotes"],
    )?;
    let protected = ["main", "master", &merge_ref[11..]].contains(&branch)
        || defaults
            .split('\n')
            .any(|r| crate::validation::pattern!(r"^refs/remotes/[^/]+/").replace(r, "") == branch);
    ensure(!protected, "Refusing to delete a main/default branch")?;
    ensure(
        !crate::inventory::registered(repo)?.iter().any(|t| {
            t.branch.as_deref() == Some(branch) && t.path != task["path"].as_str().unwrap()
        }),
        "Branch is checked out in another worktree",
    )?;
    ensure(
        !crate::records::tasks(&paths.state)?.iter().any(|t| {
            t.value()["id"] != task["id"]
                && t.value()["repo"] == repo
                && t.value()["branch"] == branch
        }),
        "Branch is claimed by another managed task",
    )?;
    let merge_commit = system::git(
        Path::new(repo),
        &["rev-parse", "--verify", &format!("{merge_ref}^{{commit}}")],
    )?;
    let mut merged_by = "ancestry";
    if let Some(commit) = &branch_commit
        && system::run(
            &[
                "git",
                "-C",
                repo,
                "merge-base",
                "--is-ancestor",
                commit,
                &merge_commit,
            ],
            None,
            false,
        )?
        .exit_code
            != 0
    {
        let merge = system::run(
            &[
                "git",
                "-C",
                repo,
                "merge-tree",
                "--write-tree",
                "--no-messages",
                &merge_commit,
                commit,
            ],
            None,
            false,
        )?;
        ensure(
            merge.exit_code == 0
                && merge
                    .stdout
                    .split('\n')
                    .next()
                    .unwrap_or("")
                    .trim_matches(js_space)
                    == system::git(
                        Path::new(repo),
                        &["rev-parse", "--verify", &format!("{merge_commit}^{{tree}}")],
                    )?,
            format!(
                "Branch has unmerged commits relative to {}, and merging it would still change files; merge it first or use remove to keep the branch",
                &merge_ref[11..]
            ),
        )?;
        merged_by = "content";
    }
    let upstream = system::git(
        Path::new(repo),
        &[
            "for-each-ref",
            "--format=%(upstream)",
            &format!("refs/heads/{branch}"),
        ],
    )?;
    if let Some(commit) = &branch_commit
        && !upstream.is_empty()
    {
        ensure(
            system::run(
                &[
                    "git",
                    "-C",
                    repo,
                    "merge-base",
                    "--is-ancestor",
                    commit,
                    &upstream,
                ],
                None,
                false,
            )?
            .exit_code
                == 0,
            "Branch is not fully merged into its configured upstream; review the upstream or use remove to keep the branch",
        )?;
    }
    Ok(
        json!({"branchCommit": branch_commit, "mergeRef": merge_ref, "mergeCommit": merge_commit, "mergedBy": merged_by}),
    )
}
pub fn delete_merged(task: &Value, target: &Value) -> Result<()> {
    let Some(commit) = target["branchCommit"].as_str() else {
        return Ok(());
    };
    let repo = task["repo"].as_str().unwrap();
    let branch = task["branch"].as_str().unwrap();
    let result = if target["mergedBy"] == "content" {
        system::run(
            &[
                "git",
                "-C",
                repo,
                "update-ref",
                "-d",
                &format!("refs/heads/{branch}"),
                commit,
            ],
            None,
            false,
        )?
    } else {
        system::run(
            &[
                "git",
                "-C",
                repo,
                "-c",
                &format!("branch.{branch}.remote="),
                "branch",
                "--delete",
                "--",
                branch,
            ],
            None,
            false,
        )?
    };
    ensure(
        result.exit_code == 0,
        "Git refused branch deletion; the branch, task record, and leases were retained. Review the repository, then retry nuke",
    )
}

pub fn validate(branch: &str) -> Result<()> {
    ensure(
        !branch.is_empty()
            && !branch.starts_with('-')
            && !branch.starts_with("refs/")
            && branch != "HEAD"
            && system::run(
                &["git", "check-ref-format", &format!("refs/heads/{branch}")],
                None,
                false,
            )?
            .exit_code
                == 0,
        "Use an exact local branch name, not a ref, revision, or option",
    )
}
pub fn local_commit(repo: &str, branch: &str) -> Result<Option<String>> {
    validate(branch)?;
    let reference = format!("refs/heads/{branch}");
    ensure(
        system::run(
            &["git", "-C", repo, "symbolic-ref", "--quiet", &reference],
            None,
            false,
        )?
        .exit_code
            != 0,
        "Symbolic local branches are not supported",
    )?;
    let status = system::run(
        &[
            "git", "-C", repo, "show-ref", "--verify", "--quiet", &reference,
        ],
        None,
        false,
    )?
    .exit_code;
    if status == 1 {
        return Ok(None);
    }
    ensure(status == 0, "Could not inspect the local branch")?;
    Ok(Some(system::git(
        Path::new(repo),
        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )?))
}
