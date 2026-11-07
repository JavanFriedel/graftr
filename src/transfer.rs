//! SSH handoffs retain private snapshots, never copy a worktree's .git pointer,
//! and never put ignored extras into the project's object database or refs.
use crate::{
    branches, checkout, hooks, inventory, locks, logs, naming,
    paths::{self, Paths},
    profiles,
    progress::Progress,
    records, setup, stateio, system, targets,
    validation::*,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const PROTOCOL: u32 = 1;
const MAGIC: &[u8; 8] = b"GRAFTR01";
const MAX_BUNDLE: u64 = 2 * 1024 * 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(900);

#[path = "transfer_preview.rs"]
mod preview;
#[path = "transfer_project.rs"]
mod project;

pub struct Options<'a> {
    pub destination: &'a str,
    pub extras: &'a [String],
    pub no_herdr: bool,
    pub dry_run: bool,
    pub approve_project: Option<&'a str>,
    pub destination_config: Option<&'a str>,
}

struct Timings {
    started: Instant,
    first_step: usize,
    side: &'static str,
}
impl Timings {
    fn start(progress: &Progress, side: &'static str) -> Self {
        Self {
            started: Instant::now(),
            first_step: progress.steps().as_array().unwrap().len(),
            side,
        }
    }
    fn report(&self, progress: &Progress) -> Value {
        let mut stages = Object::new();
        let steps = progress.steps();
        for step in steps.as_array().unwrap().iter().skip(self.first_step) {
            if let (Some(action), Some(elapsed)) =
                (step["action"].as_str(), step["elapsedMs"].as_f64())
            {
                let previous = stages.get(action).and_then(Value::as_f64).unwrap_or(0.0);
                stages.insert(action.into(), json!(previous + elapsed));
            }
        }
        json!({"side":self.side,"elapsedMs":self.started.elapsed().as_secs_f64()*1000.0,"stagesMs":stages})
    }
}

pub fn format(result: &Value, columns: usize) -> String {
    let mut text = format!(
        "{}\n{} → {}\n{}",
        result["id"].as_str().unwrap_or("Transfer"),
        result["status"].as_str().unwrap_or("checked"),
        result["path"].as_str().unwrap_or(""),
        result["message"].as_str().unwrap_or("Destination checked.")
    );
    if result["dryRun"] == true || result.get("projectMigration").is_some() {
        for finding in result["findings"].as_array().into_iter().flatten() {
            text.push_str(&format!(
                "\n{}: {}",
                finding["status"].as_str().unwrap_or("not-checked"),
                finding["message"].as_str().unwrap_or("")
            ));
        }
        if let Some(migration) = result.get("projectMigration") {
            text.push_str(&project::format_plan(migration));
        }
        if let Some(command) = result["approvalCommand"].as_str() {
            text.push_str(&format!(
                "\nAfter reviewing the proposal, approve with:\n  {command}"
            ));
        }
    }
    let valid = |value: &Value| value.as_f64().filter(|n| n.is_finite() && *n >= 0.0);
    let duration = |ms: f64| {
        if ms >= 1000.0 {
            format!("{:.2}s", ms / 1000.0)
        } else {
            format!("{ms:.0}ms")
        }
    };
    if let Some(elapsed) = valid(&result["timings"]["elapsedMs"]) {
        let label = if result["timings"]["side"] == "receiver" {
            "Recorded destination timing"
        } else {
            "Recorded timing"
        };
        let mut parts = vec![format!("{label}: {} total", duration(elapsed))];
        for (key, label) in [
            ("capture-transfer", "capture"),
            ("verify-transfer", "verify"),
            ("bundle-transfer", "bundle"),
            ("send-transfer", "transfer/setup"),
        ] {
            if let Some(elapsed) = valid(&result["timings"]["stagesMs"][key]) {
                parts.push(format!("{label} {}", duration(elapsed)));
            }
        }
        text.push('\n');
        text.push_str(&parts.join("; "));
    }
    crate::terminal::wrap(&text, columns)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Snapshot {
    head: String,
    index_tree: String,
    worktree_tree: String,
    commit: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Manifest {
    protocol: u32,
    generation: String,
    lineage: String,
    parent: Option<String>,
    project: String,
    task: String,
    branch: String,
    anchor: String,
    object_format: String,
    includes: Vec<String>,
    snapshot: Snapshot,
    bundle_size: u64,
    bundle_sha256: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Receipt {
    schema_version: u32,
    manifest: Manifest,
    repo: String,
    path: String,
    provisioned: bool,
    direction: String,
    status: String,
    peer: Option<String>,
    result: Option<Value>,
}
struct Source {
    project: String,
    task: String,
    repo: String,
    path: String,
    branch: String,
    provisioned: bool,
}
impl Source {
    fn id(&self) -> String {
        format!("{}--{}", self.project, self.task)
    }
}

fn checked(result: system::CommandResult) -> Result<String> {
    ensure(
        result.exit_code == 0,
        "Git transfer operation failed; no subprocess output was logged. Keep the checkout and private transfer files for review",
    )?;
    Ok(result.stdout.trim_end_matches('\n').into())
}
fn git(repo: &Path, args: &[&str], env: &[(String, String)]) -> Result<String> {
    git_input(repo, args, env, None)
}
fn git_input(
    repo: &Path,
    args: &[&str],
    env: &[(String, String)],
    input: Option<File>,
) -> Result<String> {
    let mut env = env.to_vec();
    // Read-only preflights must not refresh a checkout's index as a side effect.
    env.push(("GIT_OPTIONAL_LOCKS".into(), "0".into()));
    checked(system::run_file_input(
        &[
            &[
                "git",
                "-c",
                "core.hooksPath=/dev/null",
                "-C",
                repo.to_str().ok_or("Transfer path must be UTF-8")?,
            ],
            args,
        ]
        .concat(),
        None,
        &env,
        input,
        COMMAND_TIMEOUT,
    )?)
}
fn private_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| Error::from("Could not open private transfer file"))?;
    let metadata = file
        .metadata()
        .map_err(|_| Error::from("Could not inspect private transfer file"))?;
    ensure(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::getuid() }
            && metadata.mode() & 0o077 == 0,
        "Expected a private regular transfer file owned by you",
    )?;
    Ok(file)
}
fn create_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| Error::from("Could not create private transfer file"))
}
fn category(paths: &Paths, id: &str) -> Result<PathBuf> {
    crate::validation::id(id)?;
    locks::private_directory(&paths.state)?;
    let root = paths.state.join("transfers");
    locks::private_directory(&root)?;
    let directory = root.join(id);
    locks::private_directory(&directory)?;
    Ok(directory)
}
fn receipt(directory: &Path) -> Result<Option<Receipt>> {
    for parent in [directory.parent().unwrap(), directory] {
        if paths::exists(parent)? {
            records::private_dir(parent)?;
        }
    }
    let path = directory.join("receipt.json");
    if !paths::exists(&path)? {
        return Ok(None);
    }
    let value: Receipt = serde_json::from_slice(&records::read_private(&path)?)
        .map_err(|_| Error::from("Invalid transfer receipt; preserve it for review"))?;
    ensure(
        value.schema_version == 1
            && ["outgoing", "incoming"].contains(&value.direction.as_str())
            && [
                "sending",
                "sent",
                "restoring",
                "restored",
                "ready",
                "attention",
            ]
            .contains(&value.status.as_str())
            && absolute_normalized(&value.repo)
            && absolute_normalized(&value.path),
        "Invalid transfer receipt identity",
    )?;
    validate_manifest(&value.manifest)?;
    ensure(
        directory.file_name().and_then(|n| n.to_str())
            == Some(&format!(
                "{}--{}",
                value.manifest.project, value.manifest.task
            )),
        "Transfer receipt filename and task identity differ",
    )?;
    Ok(Some(value))
}
fn save(directory: &Path, value: &Receipt) -> Result<()> {
    stateio::atomic_write(
        &directory.join("receipt.json"),
        &serde_json::to_vec_pretty(value).unwrap(),
        false,
    )
}
fn uuid(value: &str) -> bool {
    pattern!(r"^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$")
        .is_match(value)
}
fn oid(value: &str, format: &str) -> bool {
    value.len() == if format == "sha256" { 64 } else { 40 }
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn relative(value: &str) -> Result<()> {
    ensure(
        !value.is_empty()
            && value.len() <= 4095
            && !value.starts_with('/')
            && !value.chars().any(|c| control(c) || c == '\\')
            && !value.split('/').any(|part| {
                part.is_empty()
                    || [".", ".."].contains(&part)
                    || part.eq_ignore_ascii_case(".git")
                    || part.len() > 255
            }),
        "Transfer paths must be literal relative file paths without traversal, Git metadata, symlinked parents or control characters",
    )
}
fn validate_manifest(value: &Manifest) -> Result<()> {
    ensure(
        value.protocol == PROTOCOL,
        "Incompatible graftr transfer protocol; install compatible graftr versions on both machines",
    )?;
    name(&value.project)?;
    name(&value.task)?;
    branches::validate(&value.branch)?;
    ensure(
        uuid(&value.generation) && uuid(&value.lineage) && value.parent.as_deref().is_none_or(uuid),
        "Invalid transfer generation",
    )?;
    ensure(
        ["sha1", "sha256"].contains(&value.object_format.as_str())
            && [
                &value.anchor,
                &value.snapshot.head,
                &value.snapshot.index_tree,
                &value.snapshot.worktree_tree,
                &value.snapshot.commit,
            ]
            .iter()
            .all(|s| oid(s, &value.object_format)),
        "Invalid transfer Git identity",
    )?;
    ensure(
        value.includes.len() <= 128
            && value.includes.iter().collect::<HashSet<_>>().len() == value.includes.len(),
        "Choose at most 128 distinct extra files",
    )?;
    for path in &value.includes {
        relative(path)?;
    }
    ensure(
        value.bundle_size > 0
            && value.bundle_size <= MAX_BUNDLE
            && oid(&value.bundle_sha256, "sha256"),
        "Invalid transfer bundle size or checksum",
    )
}
fn host(value: &str) -> Result<()> {
    ensure(
        !value.is_empty()
            && value.len() <= 255
            && !value.starts_with('-')
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-@:[]".contains(&b))
            && value.matches('@').count() <= 1,
        "Use an SSH alias, hostname, Tailscale hostname or user@host, not an SSH command or URL",
    )
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn ssh(destination: &str, arguments: &[&str], input: Option<File>) -> Result<Value> {
    host(destination)?;
    let command = std::iter::once("graftr")
        .chain(arguments.iter().copied())
        .map(quote)
        .collect::<Vec<_>>()
        .join(" ");
    let result = system::run_file_input(
        &[
            "ssh",
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "--",
            destination,
            &command,
        ],
        None,
        &[],
        input,
        COMMAND_TIMEOUT,
    )?;
    let response: Value = serde_json::from_str(&result.stdout).map_err(|_| Error::from("SSH did not return a graftr response. Verify SSH authentication/known_hosts, graftr on the remote PATH, and the remote project configuration; the source was kept"))?;
    ensure(
        response["schemaVersion"] == 1
            && response["command"].as_str() == arguments.first().copied(),
        "Unexpected remote transfer response",
    )?;
    if response["ok"] != true {
        return Err(Error {
            code: "TRANSFER_REMOTE_FAILED",
            message: format!(
                "Remote transfer needs review: {}. Source and private transfer files kept",
                response["error"]["message"]
                    .as_str()
                    .unwrap_or("inspect the remote transfer receipt")
            ),
        });
    }
    ensure(
        result.exit_code == 0,
        "SSH failed after a response; retry the same transfer to confirm receipt",
    )?;
    Ok(response["data"].clone())
}

fn source(paths: &Paths, selector: Option<&str>, project: Option<&str>) -> Result<Source> {
    let adopted = targets::adopted(paths, selector, project).map_err(|e| Error {
        code: e.code,
        message: e.message,
    })?;
    let managed = if adopted.is_none() {
        match targets::resolve_managed(paths, selector, project) {
            Ok(task) => task,
            Err(error) if error.code == "TARGET_NOT_FOUND" => None,
            Err(e) => {
                return Err(Error {
                    code: e.code,
                    message: e.message,
                });
            }
        }
    } else {
        None
    };
    if let Some(value) = managed
        .as_ref()
        .map(|t| t.value())
        .or_else(|| adopted.as_ref().map(|t| t.value()))
    {
        let id = checkout::string(value, "id");
        let (project, task) = id.split_once("--").ok_or("Invalid transfer task ID")?;
        let result = Source {
            project: project.into(),
            task: task.into(),
            repo: checkout::string(value, "repo").into(),
            path: checkout::string(value, "path").into(),
            branch: checkout::string(value, "branch").into(),
            provisioned: managed.is_some(),
        };
        let profile = profiles::configured(paths, &result.project)?;
        ensure(
            profile.value()["repo"] == result.repo,
            "Source repository no longer matches its configured project",
        )?;
        verify_source(&result)?;
        return Ok(result);
    }
    if let Some(id) = selector.filter(|id| crate::validation::id(id).is_ok()) {
        let directory = paths.state.join("transfers").join(id);
        if let Some(previous) = receipt(&directory)? {
            let manifest = &previous.manifest;
            ensure(
                format!("{}--{}", manifest.project, manifest.task) == id
                    && project.is_none_or(|p| p == manifest.project),
                "Transfer receipt project/task does not match its selection",
            )?;
            let result = Source {
                project: manifest.project.clone(),
                task: manifest.task.clone(),
                repo: previous.repo,
                path: previous.path,
                branch: manifest.branch.clone(),
                provisioned: previous.provisioned,
            };
            let profile = profiles::configured(paths, &result.project)?;
            ensure(
                profile.value()["repo"] == result.repo,
                "Source repository no longer matches its configured project",
            )?;
            verify_source(&result)?;
            return Ok(result);
        }
    }
    ensure(
        selector.is_none_or(|s| s == "@" || targets::explicit_path(s)),
        "Supply a managed task or an explicit linked worktree path",
    )?;
    let cwd = std::env::current_dir().map_err(|_| Error::from("Current directory unavailable"))?;
    let path = targets::resolve_path(&cwd, selector.filter(|s| *s != "@").unwrap_or("."))?;
    ensure(
        checkout::canonical(&path.to_string_lossy())? == path.to_string_lossy(),
        "Transfer requires a canonical path, not a symlink",
    )?;
    let path = git(&path, &["rev-parse", "--show-toplevel"], &[])?;
    let trees = inventory::registered(&path)?;
    let repo = &trees.first().ok_or("Missing Git repository")?.path;
    let (project_name, _) = profiles::for_repo(paths, repo)?
        .ok_or("Configure this repository before transferring it")?;
    ensure(
        project.is_none_or(|p| p == project_name),
        "Worktree belongs to a different project",
    )?;
    let branch = trees
        .iter()
        .find(|t| t.path == path)
        .and_then(|t| t.branch.as_ref())
        .ok_or("Transfer requires an attached feature branch")?;
    let task = Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("Worktree name must be UTF-8")?
        .to_owned();
    name(&task)?;
    let result = Source {
        project: project_name,
        task,
        repo: repo.clone(),
        path,
        branch: branch.clone(),
        provisioned: false,
    };
    verify_source(&result)?;
    Ok(result)
}
fn verify_source(source: &Source) -> Result<()> {
    for path in [&source.repo, &source.path] {
        ensure(
            absolute_normalized(path) && checkout::canonical(path)? == *path,
            "Transfer paths must be canonical",
        )?;
    }
    let trees = inventory::registered(&source.repo)?;
    ensure(
        trees.first().is_some_and(|t| t.path == source.repo)
            && source.path != source.repo
            && trees.iter().any(|t| {
                t.path == source.path
                    && t.branch.as_deref() == Some(&source.branch)
                    && !t.detached
                    && t.locked.is_none()
                    && t.prunable.is_none()
            }),
        "Transfer requires an unlocked, registered linked worktree on its recorded branch",
    )?;
    checkout::verify(&json!({"repo": source.repo, "path": source.path}))?;
    let repo = Path::new(&source.path);
    ensure(
        git(repo, &["rev-parse", "--is-shallow-repository"], &[])? == "false",
        "Shallow repositories cannot be transferred yet",
    )?;
    ensure(
        git(repo, &["ls-files", "--unmerged", "-z"], &[])?.is_empty(),
        "Resolve Git conflicts before transferring",
    )?;
    let index = git(repo, &["ls-files", "--stage", "-z"], &[])?;
    ensure(
        !index.split('\0').any(|s| s.starts_with("160000 ")),
        "Submodules cannot be transferred yet; their independent work would be omitted",
    )?;
    let flags = git(repo, &["ls-files", "-v", "-z"], &[])?;
    ensure(
        !flags
            .split('\0')
            .filter(|s| !s.is_empty())
            .any(|s| s.starts_with('S') || s.as_bytes()[0].is_ascii_lowercase()),
        "Sparse/skip-worktree and assume-unchanged entries must be cleared before transfer",
    )?;
    ensure(
        git(
            repo,
            &[
                "diff",
                "--cached",
                "--name-only",
                "-z",
                "--ita-visible-in-index",
            ],
            &[],
        )? == git(
            repo,
            &[
                "diff",
                "--cached",
                "--name-only",
                "-z",
                "--ita-invisible-in-index",
            ],
            &[],
        )?,
        "Stage or unstage intent-to-add entries before transfer",
    )?;
    for marker in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
    ] {
        let marker = git(
            repo,
            &["rev-parse", "--path-format=absolute", "--git-path", marker],
            &[],
        )?;
        ensure(
            !paths::exists(Path::new(&marker))?,
            "Finish the in-progress Git merge/rebase/cherry-pick before transferring",
        )?;
    }
    branches::validate(&source.branch)
}
fn safe_parents(root: &Path, relative_path: &str) -> Result<()> {
    relative(relative_path)?;
    let mut parent = root.to_path_buf();
    let components: Vec<_> = relative_path.split('/').collect();
    for part in &components[..components.len() - 1] {
        parent.push(part);
        match fs::symlink_metadata(&parent) {
            Ok(metadata) => ensure(
                metadata.is_dir() && !metadata.is_symlink(),
                "Transfer refuses a symlinked or non-directory parent",
            )?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err("Could not inspect transfer file parents".into()),
        }
    }
    Ok(())
}
#[derive(PartialEq, Eq)]
struct TreeEntry {
    mode: String,
    object: String,
}
fn tree_entries(repo: &Path, tree: &str) -> Result<BTreeMap<String, TreeEntry>> {
    let output = git(repo, &["ls-tree", "-r", "-z", tree], &[])?;
    let mut files = BTreeMap::new();
    for entry in output.split('\0').filter(|s| !s.is_empty()) {
        let (metadata, path) = entry.split_once('\t').ok_or("Invalid snapshot tree")?;
        let fields: Vec<_> = metadata.split(' ').collect();
        ensure(
            fields.len() == 3
                && ["100644", "100755", "120000"].contains(&fields[0])
                && fields[1] == "blob"
                && (oid(fields[2], "sha1") || oid(fields[2], "sha256")),
            "Unsupported snapshot file type",
        )?;
        relative(path)?;
        ensure(
            files
                .insert(
                    path.into(),
                    TreeEntry {
                        mode: fields[0].into(),
                        object: fields[2].into(),
                    },
                )
                .is_none(),
            "Snapshot contains duplicate file paths",
        )?;
    }
    ensure(files.len() <= 100_000, "Transfer contains too many files")?;
    Ok(files)
}
fn tree_files(repo: &Path, tree: &str) -> Result<BTreeSet<String>> {
    Ok(tree_entries(repo, tree)?.into_keys().collect())
}
fn init_repository(path: &Path, format: &str) -> Result<()> {
    locks::private_directory(path)?;
    git(
        path.parent().unwrap(),
        &[
            "init",
            "--quiet",
            "--bare",
            &format!("--object-format={format}"),
            path.to_str().unwrap(),
        ],
        &[],
    )?;
    // Private snapshots contain literal working-file bytes, not filter/LFS/EOL
    // conversions. The staged tree is retained separately from the source index.
    stateio::atomic_write(
        &path.join("info/attributes"),
        b"* -text -filter -working-tree-encoding\n",
        false,
    )
}
fn capture(
    source: &Source,
    directory: &Path,
    includes: &[String],
    borrow_from: Option<&Path>,
) -> Result<Snapshot> {
    locks::private_directory(directory)?;
    let repo = Path::new(&source.path);
    let format = git(repo, &["rev-parse", "--show-object-format"], &[])?;
    let private = directory.join("snapshot.git");
    init_repository(&private, &format)?;
    let objects = git(
        repo,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ],
        &[],
    )?;
    ensure(
        !objects.contains([':', '\n', '\r']),
        "Git object paths containing colons or newlines cannot be transferred",
    )?;
    let mut alternates = format!("{objects}\n");
    if let Some(captured) = borrow_from {
        let captured_repo = captured.join("snapshot.git");
        records::private_dir(captured)?;
        records::private_dir(&captured_repo)?;
        let captured_objects = captured_repo.join("objects");
        let metadata = fs::symlink_metadata(&captured_objects)
            .map_err(|_| Error::from("Could not inspect captured private objects"))?;
        ensure(
            metadata.is_dir() && metadata.uid() == unsafe { libc::getuid() },
            "Captured objects must be an owned directory, not a symlink",
        )?;
        let canonical = fs::canonicalize(&captured_objects)
            .map_err(|_| Error::from("Could not resolve captured private objects"))?;
        // Borrow immutable objects read-only, never the captured index. A fresh
        // index below still forces independent working-file reads in verification.
        // Alternate files are line-delimited; unusual state paths can forgo reuse.
        if let Some(path) = canonical.to_str().filter(|s| !s.contains(['\n', '\r'])) {
            alternates = format!("{path}\n{objects}\n");
        }
    }
    stateio::atomic_write(
        &private.join("objects/info/alternates"),
        alternates.as_bytes(),
        true,
    )?;
    let index = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        &[],
    )?;
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(index)
        .map_err(|_| Error::from("Could not read Git index"))?;
    ensure(
        input
            .metadata()
            .is_ok_and(|m| m.is_file() && m.len() <= 128 * 1024 * 1024),
        "Unsupported Git index",
    )?;
    let mut output = create_file(&directory.join("index"))?;
    std::io::copy(&mut input, &mut output)
        .map_err(|_| Error::from("Could not capture Git index"))?;
    drop(output);
    let mut env = vec![
        (
            "GIT_INDEX_FILE".into(),
            directory.join("index").to_string_lossy().into_owned(),
        ),
        (
            "GIT_OBJECT_DIRECTORY".into(),
            private.join("objects").to_string_lossy().into_owned(),
        ),
        ("GIT_ALTERNATE_OBJECT_DIRECTORIES".into(), objects),
        ("GIT_AUTHOR_NAME".into(), "Graftr snapshot".into()),
        ("GIT_AUTHOR_EMAIL".into(), "snapshot@graftr.invalid".into()),
        ("GIT_COMMITTER_NAME".into(), "Graftr snapshot".into()),
        (
            "GIT_COMMITTER_EMAIL".into(),
            "snapshot@graftr.invalid".into(),
        ),
        ("GIT_AUTHOR_DATE".into(), "2000-01-01T00:00:00Z".into()),
        ("GIT_COMMITTER_DATE".into(), "2000-01-01T00:00:00Z".into()),
    ];
    let head = git(repo, &["rev-parse", "HEAD^{commit}"], &[])?;
    let index_tree = git(repo, &["write-tree"], &env)?;
    // Read a split index in its source repository, then flatten it privately.
    fs::remove_file(directory.join("index"))
        .map_err(|_| Error::from("Could not flatten private index"))?;
    env.push(("GIT_DIR".into(), private.to_string_lossy().into_owned()));
    env.push(("GIT_WORK_TREE".into(), source.path.clone()));
    git(
        repo,
        &["-c", "core.splitIndex=false", "read-tree", &index_tree],
        &env,
    )?;
    let index_commit = git(
        repo,
        &[
            "-c",
            "commit.gpgSign=false",
            "commit-tree",
            &index_tree,
            "-p",
            &head,
            "-m",
            "graftr index snapshot",
        ],
        &env,
    )?;
    git(repo, &["add", "--update", "--", "."], &env)?;
    let untracked = git(
        repo,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        &[],
    )?;
    let mut add = BTreeSet::new();
    for path in untracked.split('\0').filter(|s| !s.is_empty()) {
        let basename = path.rsplit('/').next().unwrap();
        if basename != ".env" && !basename.starts_with(".env.") {
            add.insert(path.to_owned());
        }
    }
    for path in includes {
        safe_parents(repo, path)?;
        if paths::exists(&repo.join(path))? {
            add.insert(path.clone());
        }
    }
    // Literal pathspecs prevent filenames or explicit extras from becoming globs.
    for chunk in add.iter().collect::<Vec<_>>().chunks(128) {
        let mut args = vec!["--literal-pathspecs", "add", "--force", "--"];
        for path in chunk {
            safe_parents(repo, path)?;
            args.push(path);
        }
        git(repo, &args, &env)?;
    }
    let worktree_tree = git(repo, &["write-tree"], &env)?;
    let commit = git(
        repo,
        &[
            "-c",
            "commit.gpgSign=false",
            "commit-tree",
            &worktree_tree,
            "-p",
            &index_commit,
            "-m",
            "graftr worktree snapshot",
        ],
        &env,
    )?;
    for path in tree_files(&private, &worktree_tree)? {
        safe_parents(repo, &path)?;
    }
    git(
        &private,
        &["update-ref", "refs/heads/snapshot", &commit],
        &[],
    )?;
    Ok(Snapshot {
        head,
        index_tree,
        worktree_tree,
        commit,
    })
}
fn bundle(directory: &Path) -> Result<(u64, String)> {
    let file = directory.join("work.bundle");
    git(
        &directory.join("snapshot.git"),
        &[
            "bundle",
            "create",
            file.to_str().unwrap(),
            "refs/heads/snapshot",
        ],
        &[],
    )?;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .map_err(|_| Error::from("Could not protect transfer bundle"))?;
    let identity = checksum(&file)?;
    private_file(&file)?
        .sync_all()
        .map_err(|_| Error::from("Could not sync transfer bundle"))?;
    stateio::sync_directory(directory)?;
    Ok(identity)
}
fn checksum(path: &Path) -> Result<(u64, String)> {
    let mut file = private_file(path)?;
    let size = file
        .metadata()
        .map_err(|_| Error::from("Could not inspect transfer bundle"))?
        .len();
    ensure(
        size > 0 && size <= MAX_BUNDLE,
        "Transfer bundle exceeds the 2 GiB limit",
    )?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| Error::from("Could not read transfer bundle"))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok((size, format!("{:x}", hash.finalize())))
}
struct RecoveryCopy(PathBuf);
impl Drop for RecoveryCopy {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.0);
        }
    }
}
fn copy_recovery_bundle(original: &Path, before: &Path, manifest: &Manifest) -> Result<bool> {
    let file = private_file(original)?;
    if file
        .metadata()
        .map_err(|_| Error::from("Could not inspect retained recovery bundle"))?
        .len()
        != manifest.bundle_size
    {
        return Ok(false);
    }
    let temporary = before.join(format!(".recovery-{}", system::uuid()?));
    let mut output = create_file(&temporary)?;
    let mut temporary = RecoveryCopy(temporary);
    let mut input = file.take(manifest.bundle_size + 1);
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut buffer = [0; 65536];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|_| Error::from("Could not read retained recovery bundle"))?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(|_| Error::from("Could not preserve recovery bundle"))?;
        hash.update(&buffer[..count]);
        bytes += count as u64;
    }
    if bytes != manifest.bundle_size || format!("{:x}", hash.finalize()) != manifest.bundle_sha256 {
        return Ok(false);
    }
    output
        .sync_all()
        .map_err(|_| Error::from("Could not sync recovery bundle"))?;
    drop(output);
    // Link our newly written, independent copy exclusively, never the original
    // bundle's inode. A later edit to either recovery copy cannot corrupt both.
    fs::hard_link(&temporary.0, before.join("work.bundle"))
        .map_err(|_| Error::from("Could not preserve an independent recovery bundle"))?;
    fs::remove_file(&temporary.0).map_err(|_| Error::from("Could not finalize recovery bundle"))?;
    temporary.0.clear();
    stateio::sync_directory(before)?;
    Ok(true)
}
fn preserve_recovery_bundle(directory: &Path, before: &Path, previous: &Receipt) -> Result<()> {
    let retained = directory.join(&previous.manifest.generation);
    if paths::exists(&retained)? {
        records::private_dir(&retained)?;
        let original = retained.join("work.bundle");
        if paths::exists(&original)? && copy_recovery_bundle(&original, before, &previous.manifest)?
        {
            return Ok(());
        }
    }
    // Missing/corrupt cached bytes are not trusted. The live checkout has already
    // matched the saved snapshot; rebuild its full recovery bundle before writes.
    bundle(before).map(|_| ())
}
fn packet(directory: &Path, manifest: &Manifest) -> Result<()> {
    let bytes = serde_json::to_vec(manifest).unwrap();
    let mut output = create_file(&directory.join("packet"))?;
    output
        .write_all(MAGIC)
        .and_then(|_| output.write_all(&(bytes.len() as u64).to_be_bytes()))
        .and_then(|_| output.write_all(&bytes))
        .map_err(|_| Error::from("Could not write transfer packet"))?;
    std::io::copy(
        &mut private_file(&directory.join("work.bundle"))?,
        &mut output,
    )
    .map_err(|_| Error::from("Could not write transfer packet"))?;
    output
        .sync_all()
        .map_err(|_| Error::from("Could not sync transfer packet"))
}

pub fn send(
    paths: &Paths,
    selector: Option<&str>,
    project_name: Option<&str>,
    options: &Options<'_>,
    progress: &Progress,
) -> Result<Value> {
    if options.dry_run {
        return preview::send(paths, selector, project_name, options, progress);
    }
    send_worktree(paths, selector, project_name, options, progress)
}

fn send_worktree(
    paths: &Paths,
    selector: Option<&str>,
    project_name: Option<&str>,
    options: &Options<'_>,
    progress: &Progress,
) -> Result<Value> {
    let destination = options.destination;
    let extras = options.extras;
    let no_herdr = options.no_herdr;
    let timings = Timings::start(progress, "sender");
    host(destination)?;
    let source = source(paths, selector, project_name)?;
    let _transfer = locks::transfer(paths, &source.id())?;
    let directory = category(paths, &source.id())?;
    let previous = receipt(&directory)?;
    if let Some(previous) = &previous {
        ensure(
            previous.repo == source.repo
                && previous.path == source.path
                && previous.manifest.branch == source.branch,
            "Transfer receipt belongs to a different checkout; preserve it for review",
        )?;
    }
    let profile = profiles::configured(paths, &source.project)?;
    let mut outgoing = if let Some(previous) =
        previous.as_ref().filter(|r| r.direction == "outgoing")
    {
        ensure(
            options.approve_project.is_none() && options.destination_config.is_none(),
            "Project migration options cannot change a recorded handoff; retry its original transfer",
        )?;
        ensure(
            previous.peer.as_deref() == Some(destination)
                && extras
                    .iter()
                    .all(|s| previous.manifest.includes.contains(s)),
            "An existing handoff owns this task; retry the same destination/extras or transfer it back first",
        )?;
        if previous.status == "sent" {
            return Ok(previous
                .result
                .clone()
                .ok_or("Missing transfer acknowledgement")?);
        }
        ensure(
            previous.status == "sending",
            "Interrupted handoff needs review",
        )?;
        previous.clone()
    } else {
        if let Some(previous) = &previous {
            ensure(
                ["ready", "attention", "restored"].contains(&previous.status.as_str()),
                "An interrupted incoming restoration must be reviewed before another transfer",
            )?;
        }
        let mut includes: BTreeSet<_> = previous.as_ref().map_or_else(BTreeSet::new, |r| {
            r.manifest.includes.iter().cloned().collect()
        });
        for path in extras {
            relative(path)?;
            safe_parents(Path::new(&source.path), path)?;
            let metadata = fs::symlink_metadata(Path::new(&source.path).join(path))
                .map_err(|_| Error::from("An explicitly included file is missing or unreadable"))?;
            ensure(
                metadata.is_file() || metadata.is_symlink(),
                "--include accepts individual files, not directories, devices or sockets",
            )?;
            includes.insert(path.clone());
        }
        ensure(includes.len() <= 128, "Choose at most 128 extra files")?;
        let format = git(
            Path::new(&source.path),
            &["rev-parse", "--show-object-format"],
            &[],
        )?;
        let roots = git(
            Path::new(&source.path),
            &["rev-list", "--max-parents=0", "HEAD"],
            &[],
        )?;
        let anchor = roots
            .lines()
            .next()
            .ok_or("Repository has no root commit")?
            .to_owned();
        let mut remote = progress.run_timed("check-transfer-destination", destination, || {
            ssh(
                destination,
                &[
                    "_transfer-check",
                    &source.project,
                    &source.task,
                    &source.branch,
                    &anchor,
                    &format,
                    "--json",
                ],
                None,
            )
        })?;
        ensure(
            remote["protocol"] == PROTOCOL && remote["id"] == source.id(),
            "Destination uses an incompatible transfer protocol or task identity",
        )?;
        if remote["projectMissing"] == true {
            let request = project::request(paths, &source, options.destination_config)?;
            let migration = project::remote_plan(destination, &request)?;
            if let Some(approval) = options.approve_project {
                project::send(
                    paths,
                    &source,
                    destination,
                    &request,
                    &migration,
                    approval,
                    progress,
                )?;
                remote = ssh(
                    destination,
                    &[
                        "_transfer-check",
                        &source.project,
                        &source.task,
                        &source.branch,
                        &anchor,
                        &format,
                        "--json",
                    ],
                    None,
                )?;
                ensure(
                    remote["projectMissing"] != true,
                    "Destination project creation was not confirmed; preserve its private project migration receipt",
                )?;
            } else {
                return Ok(project::proposal(&source, options, migration));
            }
        } else {
            ensure(
                options.approve_project.is_none() && options.destination_config.is_none(),
                "Destination project already exists; its configuration is never overwritten by project migration",
            )?;
        }
        let generation = system::uuid()?;
        let capture_dir = directory.join(&generation);
        let includes: Vec<_> = includes.into_iter().collect();
        let snapshot = {
            let _worktree = locks::worktree(paths, &source.path, true)?;
            verify_source(&source)?;
            let captured = progress.run_timed("capture-transfer", &source.id(), || {
                capture(&source, &capture_dir, &includes, None)
            })?;
            progress.run_timed("verify-transfer", &source.id(), || {
                // Equal working/staged trees have no new working-file blobs to
                // reuse. Keep their ordinary object lookup path, but still make
                // the same independent second capture.
                let borrow_from = (captured.worktree_tree != captured.index_tree)
                    .then_some(capture_dir.as_path());
                let verify = capture(&source, &capture_dir.join("verify"), &includes, borrow_from)?;
                ensure(captured == verify, "Worktree changed while being captured; stop editors/servers and retry. No remote work was changed")
            })?;
            captured
        };
        let (bundle_size, bundle_sha256) =
            progress.run_timed("bundle-transfer", &source.id(), || bundle(&capture_dir))?;
        let manifest = Manifest {
            protocol: PROTOCOL,
            generation: generation.clone(),
            lineage: previous
                .as_ref()
                .map_or_else(|| generation.clone(), |r| r.manifest.lineage.clone()),
            parent: previous.as_ref().map(|r| r.manifest.generation.clone()),
            project: source.project.clone(),
            task: source.task.clone(),
            branch: source.branch.clone(),
            anchor,
            object_format: format,
            includes,
            snapshot,
            bundle_size,
            bundle_sha256,
        };
        validate_manifest(&manifest)?;
        progress.run_timed("write-transfer-packet", &source.id(), || {
            packet(&capture_dir, &manifest)
        })?;
        let outgoing = Receipt {
            schema_version: 1,
            manifest,
            repo: source.repo.clone(),
            path: source.path.clone(),
            provisioned: source.provisioned,
            direction: "outgoing".into(),
            status: "sending".into(),
            peer: Some(destination.into()),
            result: None,
        };
        save(&directory, &outgoing)?;
        outgoing
    };
    let capture_dir = directory.join(&outgoing.manifest.generation);
    let mut arguments = vec!["_transfer-receive", "--json"];
    if no_herdr {
        arguments.push("--no-herdr");
    }
    let mut result = progress.run_timed("send-transfer", destination, || {
        ssh(
            destination,
            &arguments,
            Some(private_file(&capture_dir.join("packet"))?),
        )
    })?;
    ensure(
        result["generation"] == outgoing.manifest.generation
            && result["id"] == source.id()
            && result["branch"] == source.branch
            && result["path"].as_str().is_some_and(|path| {
                absolute_normalized(path) && path.len() <= 4095 && !path.chars().any(control)
            })
            && [Some("ready"), Some("attention")].contains(&result["status"].as_str()),
        "Remote acknowledgement does not match the captured handoff; retry to confirm",
    )?;
    result["destination"] = json!(destination);
    result["sourceKept"] = json!(true);
    let destination_timings = result.get("timings").cloned();
    result["timings"] = timings.report(progress);
    if let Some(destination_timings) = destination_timings {
        result["timings"]["destination"] = destination_timings;
    }
    outgoing.status = "sent".into();
    after_transfer(
        paths,
        &source,
        profile.value(),
        &mut outgoing,
        &mut result,
        progress,
        false,
    )?;
    if result.get("afterTransfer").is_some() {
        let report = timings.report(progress);
        result["timings"]["elapsedMs"] = report["elapsedMs"].clone();
        result["timings"]["stagesMs"] = report["stagesMs"].clone();
    }
    outgoing.result = Some(result.clone());
    save(&directory, &outgoing)?;
    Ok(result)
}

pub fn retry_hooks(
    paths: &Paths,
    selector: Option<&str>,
    project: Option<&str>,
    progress: &Progress,
) -> Result<Value> {
    let timings = Timings::start(progress, "sender");
    let source = source(paths, selector, project)?;
    let _transfer = locks::transfer(paths, &source.id())?;
    let directory = category(paths, &source.id())?;
    let mut outgoing = receipt(&directory)?.ok_or("No recorded transfer follow-up to retry")?;
    ensure(
        outgoing.direction == "outgoing"
            && outgoing.status == "sent"
            && outgoing.repo == source.repo
            && outgoing.path == source.path
            && outgoing.manifest.branch == source.branch,
        "Only the latest confirmed outgoing handoff can retry follow-up hooks; no files were sent",
    )?;
    let mut result = outgoing
        .result
        .clone()
        .ok_or("Missing transfer acknowledgement")?;
    ensure(
        result["destinationStatus"] == "ready"
            && result["generation"] == outgoing.manifest.generation
            && result["id"] == source.id()
            && result["branch"] == source.branch
            && result["destination"].as_str() == outgoing.peer.as_deref()
            && result["path"].as_str().is_some_and(|path| {
                absolute_normalized(path) && path.len() <= 4095 && !path.chars().any(control)
            }),
        "This handoff has no confirmed-ready destination follow-up; inspect its original receipt",
    )?;
    host(
        outgoing
            .peer
            .as_deref()
            .ok_or("Missing transfer destination")?,
    )?;
    if result["afterTransfer"]["status"] == "complete" {
        return Ok(result);
    }
    ensure(
        result["status"] == "attention"
            && [Some("failed"), Some("incomplete")]
                .contains(&result["afterTransfer"]["status"].as_str()),
        "No unfinished sender follow-up to retry",
    )?;
    let profile = profiles::configured(paths, &source.project)?;
    let configured = profile.value()["hooks"]["afterTransfer"]
        .as_array()
        .ok_or("Keep the original afterTransfer hook names/order before retrying")?;
    let planned: Vec<_> = configured.iter().map(|hook| hook["name"].clone()).collect();
    ensure(
        !planned.is_empty() && result["afterTransfer"]["plannedHooks"] == json!(planned),
        "The original follow-up hook-name plan is missing or changed; keep its names/order, or finish the integration explicitly with its own CLI",
    )?;
    let observations = result["afterTransfer"]["hooks"]
        .as_array()
        .ok_or("Invalid recorded follow-up observations; preserve the receipt for review")?;
    ensure(
        observations.len() <= planned.len()
            && observations.iter().enumerate().all(|(index, hook)| {
                hook["phase"] == "afterTransfer"
                    && hook["name"] == planned[index]
                    && if index + 1 < observations.len() {
                        hook["status"] == "succeeded"
                    } else {
                        [Some("succeeded"), Some("failed"), Some("running")]
                            .contains(&hook["status"].as_str())
                    }
            }),
        "Invalid recorded follow-up observations; preserve the receipt for review",
    )?;
    let attempt = result["afterTransfer"]["attempt"]
        .as_u64()
        .filter(|attempt| (1..1000).contains(attempt))
        .ok_or("Invalid or exhausted follow-up attempt counter; inspect the receipt")?;
    let archived = directory.join(format!(
        "follow-up-{}-{attempt}.json",
        outgoing.manifest.generation
    ));
    let bytes = serde_json::to_vec_pretty(&result).unwrap();
    if paths::exists(&archived)? {
        ensure(
            records::read_private(&archived)? == bytes,
            "Prior follow-up attempt evidence differs; preserve both records for review",
        )?;
    } else {
        stateio::atomic_write(&archived, &bytes, true)?;
    }
    result["followUpOnly"] = json!(true);
    result["timings"] = timings.report(progress);
    after_transfer(
        paths,
        &source,
        profile.value(),
        &mut outgoing,
        &mut result,
        progress,
        true,
    )?;
    result["timings"] = timings.report(progress);
    outgoing.result = Some(result.clone());
    save(&directory, &outgoing)?;
    Ok(result)
}

fn after_transfer(
    paths: &Paths,
    source: &Source,
    profile: &Value,
    outgoing: &mut Receipt,
    result: &mut Value,
    progress: &Progress,
    retry: bool,
) -> Result<()> {
    let directory = category(paths, &source.id())?;
    let configured = profile["hooks"]["afterTransfer"].as_array();
    let Some(configured) = configured.filter(|hooks| !hooks.is_empty()) else {
        return Ok(());
    };
    if !retry && result["status"] != "ready" {
        return Ok(());
    }
    let ready_message = if retry {
        "The worktree transfer was already complete; no worktree files were resent or destination setup rerun.".to_owned()
    } else {
        result["message"]
            .as_str()
            .unwrap_or("Work transferred.")
            .to_owned()
    };
    let retry_command = format!("graftr transfer {} --retry-hooks", source.id());
    result["destinationStatus"] = json!("ready");
    result["status"] = json!("attention");
    result["retryCommand"] = json!(retry_command);
    result["message"] = json!(format!(
        "Warning: work transferred and destination ready; sender afterTransfer hooks are unfinished. Inspect sender logs and external effects, then retry only follow-ups with: {retry_command}. Ordinary transfer retries do not replay them."
    ));
    if retry {
        let batch = result["afterTransfer"].as_object_mut().unwrap();
        batch.insert("status".into(), json!("incomplete"));
        batch.insert(
            "attempt".into(),
            json!(batch["attempt"].as_u64().unwrap() + 1),
        );
        batch.remove("message");
    } else {
        result["afterTransfer"] = json!({"status":"incomplete","attempt":1,"plannedHooks":configured.iter().map(|hook|hook["name"].clone()).collect::<Vec<_>>(),"hooks":[]});
    }
    // Confirm the handoff and record uncertainty before executing any arbitrary
    // command. An interrupted sender returns this receipt, never replays effects.
    outgoing.result = Some(result.clone());
    save(&directory, outgoing)?;
    let execution = (|| -> Result<()> {
        let _worktree = locks::worktree(paths, &source.path, true)?;
        let verify_context = || -> Result<()> {
            verify_source(source)?;
            ensure(
                profiles::configured(paths, &source.project)?.value() == profile,
                "Source project configuration changed; sender follow-up hooks stopped for review",
            )
        };
        verify_context()?;
        let env = json!({
            "GRAFTR_PROJECT":source.project,
            "GRAFTR_TASK_ID":source.id(),
            "GRAFTR_WORKTREE_PATH":source.path,
            "GRAFTR_BRANCH":source.branch,
            "GRAFTR_TRANSFER_ID":outgoing.manifest.generation,
            "GRAFTR_TRANSFER_LINEAGE":outgoing.manifest.lineage,
            "GRAFTR_TRANSFER_DESTINATION":outgoing.peer,
            "GRAFTR_TRANSFER_DESTINATION_PATH":result["path"],
        });
        for (index, hook) in configured.iter().enumerate() {
            if result["afterTransfer"]["hooks"][index]["status"] == "succeeded" {
                continue;
            }
            let log = logs::open(
                paths,
                &source.id(),
                &format!("after-transfer-{}", hook["name"].as_str().unwrap()),
                &format!(
                    "transfer hook afterTransfer/{}",
                    hook["name"].as_str().unwrap()
                ),
            )?;
            let observations = result["afterTransfer"]["hooks"].as_array_mut().unwrap();
            let running = json!({"phase":"afterTransfer","name":hook["name"],"status":"running","log":log.path.to_string_lossy()});
            if index == observations.len() {
                observations.push(running);
            } else {
                observations[index] = running;
            }
            outgoing.result = Some(result.clone());
            save(&directory, outgoing)?;
            let observation = progress.run_timed("run-after-transfer-hook", &source.id(), || {
                hooks::run_with_env(
                    hook,
                    "afterTransfer",
                    Path::new(&source.path),
                    env.as_object().unwrap().clone(),
                    log,
                )
            });
            let failure = match observation {
                Ok(observation) => {
                    result["afterTransfer"]["hooks"][index] = observation;
                    None
                }
                Err(failure) => {
                    result["afterTransfer"]["hooks"][index] = failure.result;
                    result["afterTransfer"]["status"] = json!("failed");
                    Some(failure.error)
                }
            };
            outgoing.result = Some(result.clone());
            save(&directory, outgoing)?;
            if let Some(error) = failure {
                return Err(error);
            }
            verify_context()?;
        }
        Ok(())
    })();
    match execution {
        Ok(()) => {
            result["afterTransfer"]["status"] = json!("complete");
            result["status"] = json!("ready");
            result.as_object_mut().unwrap().remove("retryCommand");
            result["message"] = json!(format!(
                "{ready_message} Sender afterTransfer hooks completed."
            ));
        }
        Err(error) => {
            result["message"] = json!(format!(
                "Warning: work transferred and destination ready; sender afterTransfer hooks need attention. {}. Review sender logs and external effects, then retry only follow-ups with: {retry_command}. Ordinary transfer retries do not replay them.",
                error.message
            ));
            result["afterTransfer"]["message"] = json!(error.message);
        }
    }
    Ok(())
}

fn destination(
    paths: &Paths,
    project: &str,
    task: &str,
    branch: &str,
    anchor: &str,
    format: &str,
) -> Result<Source> {
    let profile = profiles::configured(paths, project)?;
    destination_profile(
        paths,
        project,
        task,
        branch,
        anchor,
        format,
        profile.value(),
    )
}

fn destination_profile(
    paths: &Paths,
    project: &str,
    task: &str,
    branch: &str,
    anchor: &str,
    format: &str,
    profile: &Value,
) -> Result<Source> {
    name(project)?;
    name(task)?;
    branches::validate(branch)?;
    ensure(
        ["sha1", "sha256"].contains(&format) && oid(anchor, format),
        "Invalid source repository identity",
    )?;
    let settings = object(profile)?;
    let repo = string(settings, "repo")?;
    let root = string(settings, "worktreeRoot")?;
    ensure(
        checkout::canonical(repo)? == repo && checkout::canonical(root)? == root,
        "Destination project paths must be canonical",
    )?;
    ensure(
        git(Path::new(repo), &["rev-parse", "--show-object-format"], &[])? == format,
        "Git object formats differ between machines",
    )?;
    git(Path::new(repo), &["cat-file", "-e", &format!("{anchor}^{{commit}}")], &[]).map_err(|_| Error::from("Destination repository does not share the source history; configure the matching clone first"))?;
    let id = format!("{project}--{task}");
    let existing = records::tasks(&paths.state)?;
    let adoption = records::adoptions(&paths.state)?;
    let frozen = records::frozen(&paths.state)?;
    let saved = existing.iter().find(|t| t.value()["id"] == id);
    let outgoing = receipt(&paths.state.join("transfers").join(&id))?;
    let returning = outgoing
        .as_ref()
        .filter(|r| r.direction == "outgoing" && r.repo == repo && r.manifest.branch == branch);
    let path = if let Some(saved) = saved {
        checkout::string(saved.value(), "path").to_owned()
    } else if let Some(returning) = returning {
        returning.path.clone()
    } else {
        naming::creation(settings, project, task, repo, root, Some(branch))?.1
    };
    ensure(
        path != root
            && (Path::new(&path).starts_with(root) || returning.is_some_and(|r| r.path == path))
            && checkout::canonical(&path)? == path,
        "Destination worktree path must be beneath its configured root",
    )?;
    ensure(
        adoption
            .iter()
            .filter(|r| {
                r.value()["id"] == id
                    || r.value()["repo"] == repo
                        && (r.value()["branch"] == branch || r.value()["path"] == path)
            })
            .all(|r| {
                returning.is_some_and(|returning| !returning.provisioned && returning.path == path)
                    && r.value()["id"] == id
                    && r.value()["path"] == path
                    && r.value()["branch"] == branch
                    && r.value()["repo"] == repo
            }),
        "Destination has adopted/external setup; it will not be overwritten or provisioned",
    )?;
    ensure(
        !frozen.iter().any(|r| {
            r.reserves()
                && (r.value()["id"] == id
                    || r.value()["removal"]["repo"] == repo
                        && (r.value()["removal"]["path"] == path
                            || r.value()["removal"]["currentBranch"] == branch))
        }),
        "Destination is reserved by frozen work",
    )?;
    ensure(
        !existing.iter().any(|r| {
            r.value()["id"] != id
                && (Path::new(&path).starts_with(checkout::string(r.value(), "path"))
                    || Path::new(checkout::string(r.value(), "path")).starts_with(&path)
                    || r.value()["repo"] == repo && r.value()["branch"] == branch)
        }),
        "Destination branch/path is owned by another task",
    )?;
    let trees = inventory::registered(repo)?;
    ensure(
        !trees.iter().any(|t| {
            t.path != path
                && (t.branch.as_deref() == Some(branch)
                    || t.path != repo
                        && (Path::new(&path).starts_with(&t.path)
                            || Path::new(&t.path).starts_with(&path)))
        }),
        "Destination overlaps an existing checkout or checked-out branch",
    )?;
    if let Some(saved) = saved {
        ensure(
            saved.value()["repo"] == repo && saved.value()["branch"] == branch,
            "Destination task owns a different repository/branch",
        )?;
        ensure(
            ![Some("queued"), Some("running"), Some("failed")]
                .contains(&saved.value()["cleanup"]["status"].as_str()),
            "Destination cleanup is unfinished; review it before receiving work",
        )?;
    }
    Ok(Source {
        project: project.into(),
        task: task.into(),
        repo: repo.into(),
        path,
        branch: branch.into(),
        provisioned: saved.is_some() || returning.is_none_or(|r| r.provisioned),
    })
}
pub fn check(paths: &Paths, args: &[String]) -> Result<Value> {
    ensure(
        args.len() == 5,
        "Transfer check requires project, task, branch and repository identity",
    )?;
    name(&args[0])?;
    name(&args[1])?;
    branches::validate(&args[2])?;
    ensure(
        ["sha1", "sha256"].contains(&args[4].as_str()) && oid(&args[3], &args[4]),
        "Invalid source repository identity",
    )?;
    if project::missing(paths, &args[0])? {
        return Ok(
            json!({"protocol":PROTOCOL,"id":format!("{}--{}",args[0],args[1]),"projectMissing":true}),
        );
    }
    let source = destination(paths, &args[0], &args[1], &args[2], &args[3], &args[4])?;
    check_destination(paths, &source)
}

fn check_destination(paths: &Paths, source: &Source) -> Result<Value> {
    let directory = paths.state.join("transfers").join(source.id());
    if paths::exists(&directory)? {
        records::private_dir(&directory)?;
    }
    let previous = receipt(&directory)?;
    if paths::exists(Path::new(&source.path))? {
        ensure(
            previous.as_ref().is_some_and(|r| {
                r.direction == "outgoing"
                    && ["sending", "sent"].contains(&r.status.as_str())
                    && r.repo == source.repo
                    && r.path == source.path
            }),
            "Destination already exists without an outgoing handoff; it will not be overwritten",
        )?;
        verify_source(source)?;
    } else {
        ensure(
            previous.is_none() && branches::local_commit(&source.repo, &source.branch)?.is_none(),
            "Destination has a retained branch/task or transfer receipt; inspect it before receiving",
        )?;
    }
    Ok(json!({"protocol":PROTOCOL, "id":source.id(), "path":source.path}))
}

pub fn preview_receive(paths: &Paths) -> Result<Value> {
    preview::receive(paths)
}

pub fn project_plan(paths: &Paths) -> Result<Value> {
    project::receive_plan(paths)
}

pub fn project_receive(paths: &Paths, progress: &Progress) -> Result<Value> {
    project::receive(paths, progress)
}

struct GitIndexLock {
    path: PathBuf,
    file: File,
    active: bool,
}
impl GitIndexLock {
    fn acquire(source: &Source) -> Result<Self> {
        let index = git(
            Path::new(&source.path),
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
            &[],
        )?;
        let path = PathBuf::from(format!("{index}.lock"));
        let file = create_file(&path).map_err(|_| {
            Error::from("Git index is locked; finish the other Git operation before receiving")
        })?;
        Ok(Self {
            path,
            file,
            active: true,
        })
    }
    fn install(&mut self, index: &Path) -> Result<()> {
        std::io::copy(&mut private_file(index)?, &mut self.file)
            .and_then(|_| self.file.sync_all())
            .map_err(|_| {
                Error::from("Could not restore staged index; preserve transfer recovery files")
            })?;
        let destination = self.path.with_extension("");
        fs::rename(&self.path, destination)
            .map_err(|_| Error::from("Could not install staged index"))?;
        self.active = false;
        stateio::sync_directory(self.path.parent().unwrap())
    }
}
impl Drop for GitIndexLock {
    fn drop(&mut self) {
        if self.active {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn validate_restore_files(
    source: &Source,
    old: &BTreeSet<String>,
    new: &BTreeSet<String>,
) -> Result<()> {
    let repo = Path::new(&source.path);
    for path in old.union(new) {
        safe_parents(repo, path)?;
        match fs::symlink_metadata(repo.join(path)) {
            Ok(metadata) => ensure(
                !metadata.is_dir() && (old.contains(path) || !new.contains(path)),
                "Incoming file collides with preserved local/ignored data; nothing will be overwritten",
            )?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err("Could not inspect a destination transfer file".into()),
        }
    }
    Ok(())
}

fn restore(
    source: &Source,
    private: &Path,
    current: &Snapshot,
    incoming: &Snapshot,
    includes: &[String],
    directory: &Path,
) -> Result<()> {
    let repo = Path::new(&source.path);
    let old_entries = tree_entries(
        &directory.join("before/snapshot.git"),
        &current.worktree_tree,
    )?;
    let new_entries = tree_entries(private, &incoming.worktree_tree)?;
    let old: BTreeSet<_> = old_entries.keys().cloned().collect();
    let new: BTreeSet<_> = new_entries.keys().cloned().collect();
    validate_restore_files(source, &old, &new)?;
    let mut changed: BTreeSet<_> = new_entries
        .iter()
        .filter(|(path, entry)| old_entries.get(*path) != Some(*entry))
        .map(|(path, _)| path.clone())
        .collect();
    for path in includes {
        if let Ok(metadata) = fs::symlink_metadata(repo.join(path))
            && new.contains(path)
            && metadata.is_file()
            && (metadata.nlink() > 1 || metadata.mode() & 0o077 != 0)
        {
            // Replacing a shared/permissive extra keeps privacy enforcement from
            // chmod'ing an alias outside this checkout when its bytes are equal.
            changed.insert(path.clone());
        }
    }
    let staged_index = directory.join("staged-index");
    let staged_env = vec![(
        "GIT_INDEX_FILE".into(),
        staged_index.to_string_lossy().into_owned(),
    )];
    git(
        Path::new(&source.repo),
        &["read-tree", &incoming.index_tree],
        &staged_env,
    )?;
    fs::set_permissions(&staged_index, fs::Permissions::from_mode(0o600))
        .map_err(|_| Error::from("Could not protect restored index"))?;
    for path in old.difference(&new) {
        match fs::remove_file(repo.join(path)) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => {
                return Err(
                    "Could not retire an old transferred file; preserve recovery files".into(),
                );
            }
        }
        let mut parent = repo.join(path).parent().unwrap().to_path_buf();
        while parent != repo {
            if fs::remove_dir(&parent).is_err() {
                break;
            }
            parent.pop();
        }
    }
    if !changed.is_empty() {
        // The real index is locked separately. Literal NUL-delimited paths avoid
        // argv limits/glob interpretation and leave equal file bytes/mtimes alone.
        let checkout_env = vec![(
            "GIT_INDEX_FILE".into(),
            directory
                .join("checkout-index")
                .to_string_lossy()
                .into_owned(),
        )];
        git(
            private,
            &["read-tree", &incoming.worktree_tree],
            &checkout_env,
        )?;
        let paths_file = directory.join("restore-paths");
        let mut output = create_file(&paths_file)?;
        for path in changed {
            output
                .write_all(path.as_bytes())
                .and_then(|_| output.write_all(b"\0"))
                .map_err(|_| Error::from("Could not prepare literal restoration paths"))?;
        }
        drop(output);
        git_input(
            private,
            &[
                &format!("--work-tree={}", source.path),
                "checkout-index",
                "--force",
                "-z",
                "--stdin",
            ],
            &checkout_env,
            Some(private_file(&paths_file)?),
        )?;
    }
    for path in includes {
        let path = repo.join(path);
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && metadata.is_file()
        {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
                .map_err(|_| Error::from("Could not protect included private file"))?;
            file.set_permissions(fs::Permissions::from_mode(
                0o600 | (metadata.mode() & 0o100),
            ))
            .map_err(|_| Error::from("Could not protect included private file"))?;
        }
    }
    git(
        repo,
        &[
            "update-ref",
            &format!("refs/heads/{}", source.branch),
            &incoming.head,
            &current.head,
        ],
        &[],
    )?;
    Ok(())
}

struct PrivateUmask(libc::mode_t);
impl Drop for PrivateUmask {
    fn drop(&mut self) {
        unsafe {
            libc::umask(self.0);
        }
    }
}

pub fn receive(paths: &Paths, no_herdr: bool, progress: &Progress) -> Result<Value> {
    let timings = Timings::start(progress, "receiver");
    // Checkout-index otherwise creates explicitly included secrets as 0644 before
    // they can be protected. This one-shot worker creates all new files privately.
    let _umask = PrivateUmask(unsafe { libc::umask(0o077) });
    let mut input = std::io::stdin().lock();
    let mut magic = [0; 8];
    let mut length = [0; 8];
    input
        .read_exact(&mut magic)
        .and_then(|_| input.read_exact(&mut length))
        .map_err(|_| Error::from("Incomplete transfer header"))?;
    ensure(
        &magic == MAGIC && u64::from_be_bytes(length) <= 65536,
        "Invalid or oversized transfer header",
    )?;
    let mut bytes = vec![0; u64::from_be_bytes(length) as usize];
    input
        .read_exact(&mut bytes)
        .map_err(|_| Error::from("Incomplete transfer manifest"))?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|_| Error::from("Invalid transfer manifest"))?;
    validate_manifest(&manifest)?;
    let id = format!("{}--{}", manifest.project, manifest.task);
    let _transfer = locks::transfer(paths, &id)?;
    let directory = category(paths, &id)?;
    let previous = receipt(&directory)?;
    let incoming_dir = directory.join(format!("received-{}", manifest.generation));
    locks::private_directory(&incoming_dir)?;
    // Always drain and verify the packet, even when acknowledging an earlier receipt.
    let incoming_file = incoming_dir.join(format!("{}.bundle", system::uuid()?));
    progress.run_timed("receive-transfer", &id, || -> Result<()> {
        let mut output = create_file(&incoming_file)?;
        let copied = std::io::copy(&mut input.by_ref().take(manifest.bundle_size), &mut output)
            .map_err(|_| Error::from("Transfer was interrupted; source kept"))?;
        output
            .sync_all()
            .map_err(|_| Error::from("Could not persist received bundle"))?;
        drop(output);
        let mut tail = [0; 1];
        ensure(
            copied == manifest.bundle_size && input.read(&mut tail).is_ok_and(|n| n == 0),
            "Transfer bundle length mismatch; source kept",
        )?;
        ensure(
            checksum(&incoming_file)? == (manifest.bundle_size, manifest.bundle_sha256.clone()),
            "Transfer bundle checksum mismatch; no checkout restored",
        )?;
        stateio::sync_directory(&incoming_dir)?;
        Ok(())
    })?;
    if let Some(previous) = &previous
        && previous.manifest.generation == manifest.generation
        && previous.direction == "incoming"
    {
        ensure(
            serde_json::to_vec(&previous.manifest).unwrap()
                == serde_json::to_vec(&manifest).unwrap(),
            "Transfer generation was reused with different contents",
        )?;
        fs::remove_file(&incoming_file)
            .map_err(|_| Error::from("Could not remove duplicate received packet"))?;
        return previous.result.clone().ok_or_else(|| "Interrupted restoration/setup requires explicit review; inspect the remote transfer receipt and checkout, then run graftr setup --wait. Hooks were not replayed".into());
    }
    let source = destination(
        paths,
        &manifest.project,
        &manifest.task,
        &manifest.branch,
        &manifest.anchor,
        &manifest.object_format,
    )?;
    let _frozen = locks::frozen(paths, &id)?;
    let _worktree = locks::worktree(paths, &source.path, true)?;
    let private = incoming_dir.join("snapshot.git");
    progress.run_timed("unpack-transfer", &id, || -> Result<()> {
        init_repository(&private, &manifest.object_format)?;
        git(
            &private,
            &["bundle", "verify", incoming_file.to_str().unwrap()],
            &[],
        )?;
        git(
            &private,
            &["bundle", "unbundle", incoming_file.to_str().unwrap()],
            &[],
        )?;
        let snapshot = &manifest.snapshot;
        ensure(
            git(
                &private,
                &["rev-parse", &format!("{}^{{tree}}", snapshot.commit)],
                &[],
            )? == snapshot.worktree_tree
                && git(
                    &private,
                    &["rev-parse", &format!("{}^^{{tree}}", snapshot.commit)],
                    &[],
                )? == snapshot.index_tree
                && git(
                    &private,
                    &["rev-parse", &format!("{}^^", snapshot.commit)],
                    &[],
                )? == snapshot.head,
            "Bundle does not match its snapshot identity",
        )?;
        git(
            &private,
            &[
                "merge-base",
                "--is-ancestor",
                &manifest.anchor,
                &snapshot.head,
            ],
            &[],
        )?;
        tree_files(&private, &snapshot.index_tree)?;
        for path in tree_files(&private, &snapshot.worktree_tree)? {
            safe_parents(Path::new(&source.path), &path)?;
        }
        Ok(())
    })?;
    let snapshot = &manifest.snapshot;
    let exists = paths::exists(Path::new(&source.path))?;
    if exists {
        let previous = previous
            .as_ref()
            .ok_or("Existing destination has no handoff receipt; it will not be overwritten")?;
        ensure(
            previous.direction == "outgoing"
                && ["sending", "sent"].contains(&previous.status.as_str())
                && manifest.parent.as_deref() == Some(&previous.manifest.generation)
                && manifest.lineage == previous.manifest.lineage
                && previous.repo == source.repo
                && previous.path == source.path
                && previous.manifest.branch == source.branch
                && previous
                    .manifest
                    .includes
                    .iter()
                    .all(|path| manifest.includes.contains(path)),
            "Destination is not the unchanged source of this handoff; independent work will not be overwritten",
        )?;
        verify_source(&source)?;
    } else {
        ensure(
            previous.is_none()
                && branches::local_commit(&source.repo, &source.branch)?.is_none()
                && !paths::exists(&paths.state.join(format!("{id}.json")))?,
            "Destination already has a retained branch/task or recovery receipt; inspect it first",
        )?;
    }
    let mut record = Receipt {
        schema_version: 1,
        manifest: manifest.clone(),
        repo: source.repo.clone(),
        path: source.path.clone(),
        provisioned: source.provisioned,
        direction: "incoming".into(),
        status: "restoring".into(),
        peer: None,
        result: None,
    };
    let _state = locks::state(paths)?;
    let rechecked = destination(
        paths,
        &manifest.project,
        &manifest.task,
        &manifest.branch,
        &manifest.anchor,
        &manifest.object_format,
    )?;
    ensure(
        rechecked.path == source.path
            && rechecked.repo == source.repo
            && rechecked.provisioned == source.provisioned,
        "Destination ownership/configuration changed during receipt; inspect it before retrying",
    )?;
    // Import only HEAD and the staged tree, never the private worktree/ignored extras.
    let index_commit = git(
        &private,
        &["rev-parse", &format!("{}^", snapshot.commit)],
        &[],
    )?;
    progress.run_timed("import-transfer-history", &id, || {
        git(
            Path::new(&source.repo),
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "fetch",
                "--no-write-fetch-head",
                "--no-tags",
                "--no-recurse-submodules",
                private.to_str().unwrap(),
                &index_commit,
            ],
            &[],
        )
    })?;
    if !exists {
        save(&directory, &record)?;
        let parent = Path::new(&source.path).parent().unwrap();
        fs::create_dir_all(parent)
            .map_err(|_| Error::from("Could not create destination checkout parent"))?;
        ensure(
            checkout::canonical(&source.path)? == source.path,
            "Destination parent changed; checkout not created",
        )?;
        progress.run_timed("create-transfer-checkout", &id, || {
            git(
                Path::new(&source.repo),
                &[
                    "-c",
                    "core.hooksPath=/dev/null",
                    "worktree",
                    "add",
                    "-b",
                    &source.branch,
                    "--",
                    &source.path,
                    &snapshot.head,
                ],
                &[],
            )
        })?;
    }
    let mut index_lock = GitIndexLock::acquire(&source)?;
    let before_includes = if exists {
        &previous.as_ref().unwrap().manifest.includes
    } else {
        &manifest.includes
    };
    let before = progress.run_timed("capture-transfer-destination", &id, || {
        capture(&source, &incoming_dir.join("before"), before_includes, None)
    })?;
    if exists {
        ensure(
            before == previous.as_ref().unwrap().manifest.snapshot,
            "Destination changed since handoff; commits, staged/unstaged edits or files differ. Nothing was overwritten",
        )?;
        progress.run_timed("save-transfer-recovery", &id, || {
            preserve_recovery_bundle(
                &directory,
                &incoming_dir.join("before"),
                previous.as_ref().unwrap(),
            )
        })?;
        validate_restore_files(
            &source,
            &tree_files(
                &incoming_dir.join("before/snapshot.git"),
                &before.worktree_tree,
            )?,
            &tree_files(&private, &snapshot.worktree_tree)?,
        )?;
        save(&directory, &record)?;
    }
    progress.run_timed("restore-transfer-files", &id, || {
        restore(
            &source,
            &private,
            &before,
            snapshot,
            &manifest.includes,
            &incoming_dir,
        )?;
        index_lock.install(&incoming_dir.join("staged-index"))
    })?;
    drop(index_lock);
    record.status = "restored".into();
    save(&directory, &record)?;
    drop(_state);
    drop(_worktree);
    if exists && !source.provisioned {
        let result = json!({"id":id,"generation":manifest.generation,"status":"ready","path":source.path,"branch":source.branch,"includedFiles":manifest.includes,"sourceKept":true,"message":"Work returned. Existing external setup, environment, services and workspace were preserved; no installer or setup hooks ran.","timings":timings.report(progress)});
        record.status = "ready".into();
        record.result = Some(result.clone());
        save(&directory, &record)?;
        return Ok(result);
    }
    let engine = setup::Engine { paths, progress };
    let setup_options = setup::Options {
        no_herdr,
        project: Some(source.project.clone()),
        name: Some(source.task.clone()),
        expected_path: Some(source.path.clone()),
        expected_repo: Some(source.repo.clone()),
        expected_branch: Some(source.branch.clone()),
        wait: true,
        ..Default::default()
    };
    let prepared = progress.run_timed("prepare-transfer-destination", &id, || {
        engine.prepare_transferred(&source.path, &setup_options)
    });
    let mut status = "attention";
    let mut message = "Work received; destination setup needs review. Source kept. Inspect graftr status/logs and explicitly retry setup; a transfer retry does not replay hooks.".to_owned();
    if let Ok(result) = prepared
        && result.receipt["status"] == "ready"
    {
        status = "ready";
        message = "Work received and prepared. Source kept; return it with graftr transfer --to SOURCE_HOST.".into();
    }
    let result = json!({"id":id,"generation":manifest.generation,"status":status,"path":source.path,"branch":source.branch,"includedFiles":manifest.includes,"sourceKept":true,"message":message,"timings":timings.report(progress)});
    record.status = status.into();
    record.result = Some(result.clone());
    save(&directory, &record)?;
    Ok(result)
}
