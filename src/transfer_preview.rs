//! Read-only inspection deliberately avoids capture/write-tree, persistent locks,
//! installers and hooks. Actual receipt still independently checks everything.
use super::*;
use std::os::unix::ffi::OsStrExt;

const MAX_REQUEST: u64 = 16 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    project: String,
    task: String,
    branch: String,
    anchor: String,
    object_format: String,
    includes: Vec<String>,
    files: Vec<String>,
    parent: Option<String>,
    lineage: Option<String>,
    retry: Option<Manifest>,
    no_herdr: bool,
}

fn staged(source: &Source) -> Result<BTreeMap<String, TreeEntry>> {
    let mut result = BTreeMap::new();
    for entry in git(Path::new(&source.path), &["ls-files", "--stage", "-z"], &[])?
        .split('\0')
        .filter(|s| !s.is_empty())
    {
        let (header, path) = entry.split_once('\t').ok_or("Invalid Git index entry")?;
        relative(path)?;
        let parts: Vec<_> = header.split(' ').collect();
        ensure(
            parts.len() == 3 && parts[2] == "0",
            "Resolve Git conflicts before transferring",
        )?;
        result.insert(
            path.to_owned(),
            TreeEntry {
                mode: parts[0].into(),
                object: parts[1].into(),
            },
        );
    }
    Ok(result)
}

fn working(
    source: &Source,
    includes: &[String],
    hashes: bool,
) -> Result<BTreeMap<String, TreeEntry>> {
    let repo = Path::new(&source.path);
    let mut paths: BTreeSet<_> = staged(source)?.into_keys().collect();
    let untracked = git(
        repo,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        &[],
    )?;
    for path in untracked.split('\0').filter(|s| !s.is_empty()) {
        let basename = path.rsplit('/').next().unwrap();
        if basename != ".env" && !basename.starts_with(".env.") {
            paths.insert(path.to_owned());
        }
    }
    paths.extend(includes.iter().cloned());
    let mut result = BTreeMap::new();
    let mut regular = Vec::new();
    for path in paths {
        safe_parents(repo, &path)?;
        let metadata = match fs::symlink_metadata(repo.join(&path)) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err("Could not inspect portable working files".into()),
        };
        let mode = if metadata.is_symlink() {
            "120000"
        } else if metadata.is_file() {
            if metadata.mode() & 0o100 == 0 {
                "100644"
            } else {
                "100755"
            }
        } else {
            return Err(
                "Portable files must be regular files or symlinks, not directories/devices/sockets"
                    .into(),
            );
        };
        let mut object = String::new();
        if hashes {
            if metadata.is_symlink() {
                let target = fs::read_link(repo.join(&path))
                    .map_err(|_| Error::from("Could not read portable symlink"))?;
                object = git_input(
                    repo,
                    &["hash-object", "--no-filters", "--stdin"],
                    &[],
                    Some(system::memory_input(target.as_os_str().as_bytes())?),
                )?;
            } else {
                regular.push(path.clone());
            }
        }
        result.insert(
            path,
            TreeEntry {
                mode: mode.into(),
                object,
            },
        );
    }
    ensure(
        result.len() <= 100000,
        "Transfer exceeds the 100,000-file limit",
    )?;
    for chunk in regular.chunks(128) {
        let mut args = vec!["hash-object", "--no-filters", "--"];
        args.extend(chunk.iter().map(String::as_str));
        let output = git(repo, &args, &[])?;
        let hashes: Vec<_> = output.lines().collect();
        ensure(
            hashes.len() == chunk.len(),
            "Could not inspect portable file identities",
        )?;
        for (path, hash) in chunk.iter().zip(hashes) {
            result.get_mut(path).unwrap().object = hash.into();
        }
    }
    Ok(result)
}

pub(super) fn send(
    paths: &Paths,
    selector: Option<&str>,
    project_name: Option<&str>,
    options: &Options<'_>,
    progress: &Progress,
) -> Result<Value> {
    host(options.destination)?;
    let source = source(paths, selector, project_name)?;
    let previous = receipt(&paths.state.join("transfers").join(source.id()))?;
    if let Some(previous) = &previous {
        ensure(
            previous.repo == source.repo
                && previous.path == source.path
                && previous.manifest.branch == source.branch,
            "Transfer receipt belongs to a different checkout; preserve it for review",
        )?;
        ensure(
            if previous.direction == "outgoing" {
                previous.peer.as_deref() == Some(options.destination)
                    && options
                        .extras
                        .iter()
                        .all(|p| previous.manifest.includes.contains(p))
                    && ["sending", "sent"].contains(&previous.status.as_str())
            } else {
                ["ready", "attention", "restored"].contains(&previous.status.as_str())
            },
            "Existing handoff needs its original destination/extras or an explicit review before another transfer",
        )?;
    }
    let retry = previous
        .as_ref()
        .filter(|r| r.direction == "outgoing")
        .map(|r| r.manifest.clone());
    let mut includes: BTreeSet<_> = previous.as_ref().map_or_else(BTreeSet::new, |r| {
        r.manifest.includes.iter().cloned().collect()
    });
    for path in options.extras {
        relative(path)?;
        safe_parents(Path::new(&source.path), path)?;
        if retry.is_none() {
            let metadata = fs::symlink_metadata(Path::new(&source.path).join(path))
                .map_err(|_| Error::from("An explicitly included file is missing or unreadable"))?;
            ensure(
                metadata.is_file() || metadata.is_symlink(),
                "--include accepts individual files, not directories, devices or sockets",
            )?;
        }
        includes.insert(path.clone());
    }
    ensure(includes.len() <= 128, "Choose at most 128 extra files")?;
    let includes: Vec<_> = includes.into_iter().collect();
    let files = if let Some(manifest) = &retry {
        let private = paths
            .state
            .join("transfers")
            .join(source.id())
            .join(&manifest.generation)
            .join("snapshot.git");
        records::private_dir(&private)?;
        tree_files(&private, &manifest.snapshot.worktree_tree)?
            .into_iter()
            .collect()
    } else {
        working(&source, &includes, false)?.into_keys().collect()
    };
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
    let request = Request {
        project: source.project.clone(),
        task: source.task.clone(),
        branch: source.branch.clone(),
        anchor: roots
            .lines()
            .next()
            .ok_or("Repository has no root commit")?
            .into(),
        object_format: format,
        includes,
        files,
        parent: previous.as_ref().map(|p| p.manifest.generation.clone()),
        lineage: previous.as_ref().map(|p| p.manifest.lineage.clone()),
        retry,
        no_herdr: options.no_herdr,
    };
    let bytes = serde_json::to_vec(&request).unwrap();
    ensure(
        bytes.len() as u64 <= MAX_REQUEST,
        "Dry-run file inventory exceeds its 16 MiB protocol limit",
    )?;
    let mut result =
        progress.run_timed("check-transfer-destination", options.destination, || {
            ssh(
                options.destination,
                &["_transfer-preview", "--json"],
                Some(system::memory_input(&bytes)?),
            )
        })?;
    ensure(
        result["protocol"] == PROTOCOL && result["id"] == source.id(),
        "Destination uses an incompatible dry-run protocol or task identity",
    )?;
    if result["projectMissing"] == true {
        ensure(
            request.retry.is_none(),
            "A recorded handoff lost its destination project; restore its configuration manually before retrying",
        )?;
        let recipe = project::request(paths, &source, options.destination_config)?;
        return Ok(project::proposal(
            &source,
            options,
            project::remote_plan(options.destination, &recipe)?,
        ));
    }
    ensure(
        options.destination_config.is_none(),
        "Destination project already exists; --destination-config never overrides it",
    )?;
    result["dryRun"] = json!(true);
    result["destination"] = json!(options.destination);
    result["sourceKept"] = json!(true);
    result["retry"] = json!(request.retry.is_some());
    if previous.as_ref().is_some_and(|p| {
        p.direction == "outgoing"
            && p.status == "sent"
            && p.result
                .as_ref()
                .is_some_and(|r| r["status"] == "attention")
    }) {
        result["status"] = json!("attention");
        result["canTransfer"] = json!(false);
        finding(
            &mut result,
            "attention",
            "The sender recorded an incomplete handoff/follow-up result. An ordinary retry returns that result, not a repair; inspect its receipt/logs before explicit recovery.",
        );
    }
    if request.retry.is_some() {
        result["message"] = json!(
            "Dry run only. A retry uses the original immutable snapshot/acknowledgement, not newer local edits; no files or hooks were replayed."
        );
    }
    Ok(result)
}

pub(super) fn receive(paths: &Paths) -> Result<Value> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_REQUEST + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::from("Could not read dry-run request"))?;
    ensure(
        bytes.len() as u64 <= MAX_REQUEST,
        "Dry-run request exceeds its limit",
    )?;
    let request: Request = serde_json::from_slice(&bytes)
        .map_err(|_| Error::from("Invalid dry-run request; contents were not logged"))?;
    name(&request.project)?;
    name(&request.task)?;
    branches::validate(&request.branch)?;
    ensure(
        ["sha1", "sha256"].contains(&request.object_format.as_str())
            && oid(&request.anchor, &request.object_format)
            && request.parent.as_deref().is_none_or(uuid)
            && request.lineage.as_deref().is_none_or(uuid)
            && request.includes.len() <= 128
            && request.files.len() <= 100000,
        "Invalid dry-run repository/file identity",
    )?;
    for path in request.files.iter().chain(&request.includes) {
        relative(path)?;
    }
    if let Some(retry) = &request.retry {
        validate_manifest(retry)?;
        ensure(
            retry.project == request.project
                && retry.task == request.task
                && retry.branch == request.branch,
            "Dry-run retry has a different task identity",
        )?;
    }
    let mut result = json!({"protocol":PROTOCOL,"id":format!("{}--{}",request.project,request.task),"status":"checked","canTransfer":true,"dryRun":true,"findings":[],
        "message":"Dry run only. Read-only preflight checks passed; actual capture, restoration and setup must still succeed."});
    if project::missing(paths, &request.project)? {
        result["projectMissing"] = json!(true);
        return Ok(result);
    }
    if let Err(error) = inspect(paths, &request, &mut result) {
        result["status"] = json!("attention");
        result["canTransfer"] = json!(false);
        finding(&mut result, "attention", &error.message);
        result["message"] = json!(
            "Dry run found a blocker. Neither machine's project, files or Graftr state were changed."
        );
    }
    finding(
        &mut result,
        "not-checked",
        "Installers/hooks, port allocation, exact bundle size, concurrent writers and final environment/resource verification require the real transfer. No project commands or services were run.",
    );
    Ok(result)
}

fn finding(result: &mut Value, status: &str, message: &str) {
    result["findings"]
        .as_array_mut()
        .unwrap()
        .push(json!({"status":status,"message":message}));
}

fn inspect(paths: &Paths, request: &Request, result: &mut Value) -> Result<()> {
    let directory = paths
        .state
        .join("transfers")
        .join(format!("{}--{}", request.project, request.task));
    let previous = receipt(&directory)?;
    if let Some(retry) = &request.retry
        && let Some(previous) = &previous
        && previous.direction == "incoming"
        && previous.manifest.generation == retry.generation
    {
        ensure(
            serde_json::to_vec(&previous.manifest).unwrap() == serde_json::to_vec(retry).unwrap(),
            "Transfer generation was reused with different contents",
        )?;
        ensure(
            previous.result.is_some(),
            "Interrupted restoration/setup requires explicit review; it cannot be repaired by a retry",
        )?;
        result["path"] = json!(previous.path);
        finding(
            result,
            "ok",
            "Destination already recorded this snapshot; a retry returns its acknowledgement without replaying setup or hooks.",
        );
        if previous.status == "attention" {
            return Err(
                "Recorded destination setup still needs review; a retry will not repair it".into(),
            );
        }
        return Ok(());
    }
    let source = destination(
        paths,
        &request.project,
        &request.task,
        &request.branch,
        &request.anchor,
        &request.object_format,
    )?;
    result["path"] = json!(source.path);
    if paths::exists(Path::new(&source.path))? {
        let previous = previous.as_ref().ok_or(
            "Destination already exists without an outgoing handoff; it will not be overwritten",
        )?;
        ensure(
            previous.direction == "outgoing"
                && ["sending", "sent"].contains(&previous.status.as_str())
                && request.parent.as_deref() == Some(&previous.manifest.generation)
                && request.lineage.as_deref() == Some(&previous.manifest.lineage)
                && previous.repo == source.repo
                && previous.path == source.path
                && previous
                    .manifest
                    .includes
                    .iter()
                    .all(|p| request.includes.contains(p)),
            "Destination is not the unchanged source of this handoff; independent work will not be overwritten",
        )?;
        verify_source(&source)?;
        let private = directory
            .join(&previous.manifest.generation)
            .join("snapshot.git");
        records::private_dir(&private)?;
        let snapshot = &previous.manifest.snapshot;
        ensure(
            git(
                Path::new(&source.path),
                &["rev-parse", "HEAD^{commit}"],
                &[],
            )? == snapshot.head
                && staged(&source)? == tree_entries(&private, &snapshot.index_tree)?
                && working(&source, &previous.manifest.includes, true)?
                    == tree_entries(&private, &snapshot.worktree_tree)?,
            "Destination changed since handoff; commits, staged/unstaged edits or files differ. Nothing was overwritten",
        )?;
        let old = tree_files(&private, &snapshot.worktree_tree)?;
        validate_restore_files(&source, &old, &request.files.iter().cloned().collect())?;
        finding(
            result,
            "ok",
            "Return lineage and the retained source's literal staged/working files match the saved snapshot.",
        );
        if !source.provisioned {
            finding(
                result,
                "ok",
                "Returning to external setup preserves its environment/resources; no destination installer or setup hooks will run.",
            );
            return Ok(());
        }
    } else {
        ensure(
            previous.is_none()
                && branches::local_commit(&source.repo, &source.branch)?.is_none()
                && !paths::exists(&paths.state.join(format!("{}.json", source.id())))?,
            "Destination has a retained branch/task or transfer receipt; inspect it before receiving",
        )?;
        for path in &request.files {
            safe_parents(Path::new(&source.path), path)?;
        }
        finding(
            result,
            "ok",
            "Repository history/object format match and the destination branch/path are available.",
        );
    }
    let profile = profiles::configured(paths, &request.project)?;
    if let Some(path) = profile.value()["envSource"].as_str() {
        ensure(
            absolute_normalized(path)
                && checkout::canonical(path)? == path
                && Path::new(path)
                    .file_name()
                    .is_some_and(|name| name == ".env"),
            "Destination environment baseline must have an absolute canonical path named .env",
        )?;
        ensure(
            fs::metadata(path).is_ok_and(|m| m.is_file()),
            "Destination development environment baseline is missing or unreadable; its contents were not read",
        )?;
        finding(
            result,
            "ok",
            "Destination environment baseline exists; values and application-specific requirements were not inspected.",
        );
    }
    if let Some(command) = profile.value()["installCommand"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_str)
    {
        if command.starts_with('/') || !command.contains('/') {
            ensure(
                system::which(command, Path::new(&source.repo)).is_some(),
                "Destination installer executable is unavailable on its noninteractive PATH",
            )?;
            finding(
                result,
                "ok",
                "Destination installer executable is available; it was not run.",
            );
        } else {
            finding(
                result,
                "not-checked",
                "Checkout-relative installer availability is checked after restoration.",
            );
        }
    }
    if !request.no_herdr && profiles::herdr_mode(paths)? == "required" {
        ensure(
            crate::herdr::caller_socket().is_ok(),
            "Destination requires Herdr but has no destination-local context; explicitly use --no-herdr or provide that context",
        )?;
        finding(
            result,
            "not-checked",
            "Destination requires Herdr; workspace attachment needs destination-local context, or explicitly use --no-herdr.",
        );
    }
    Ok(())
}
