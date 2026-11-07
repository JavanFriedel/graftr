//! Project bootstrap is a separate, explicitly approved operation. Only a normal
//! main-branch bundle travels here; private worktree snapshots use the usual protocol.
use super::*;
use crate::config;
use std::os::unix::fs::DirBuilderExt;

const MAGIC: &[u8; 8] = b"GRAFPR01";
const MAX_REQUEST: u64 = 256 * 1024;
const LOCAL: &[&str] = &[
    "repo",
    "worktreeRoot",
    "envSource",
    "localPorts",
    "httpsPorts",
    "ports",
    "url",
];

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Request {
    project: String,
    task: String,
    branch: String,
    anchor: String,
    object_format: String,
    portable: Value,
    source_local_fields: Vec<String>,
    required_overrides: Vec<String>,
    environment_required: bool,
    url_required: bool,
    https: bool,
    named_ports: Vec<String>,
    seed: Option<Seed>,
    destination_config: Option<String>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Seed {
    branch: String,
    commit: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Header {
    request: Request,
    approval: String,
    bundle_size: u64,
    bundle_sha256: String,
}

pub(super) fn missing(paths: &Paths, project: &str) -> Result<bool> {
    name(project)?;
    Ok(!paths::exists(&paths.config)? || !config::projects(paths)?.contains_key(project))
}

pub(super) fn request(
    paths: &Paths,
    source: &Source,
    destination_config: Option<&str>,
) -> Result<Request> {
    let profile = profiles::configured(paths, &source.project)?;
    let settings = profile.value().as_object().unwrap();
    let mut portable = settings.clone();
    let mut source_local_fields = Vec::new();
    for key in LOCAL {
        if portable.remove(*key).is_some() {
            source_local_fields.push((*key).to_owned());
        }
    }
    // Commands are trusted code, not something we can rewrite for another host.
    // Detect obvious embedded local paths/addresses and require a local replacement.
    let mut local_literals: Vec<_> = LOCAL
        .iter()
        .filter_map(|key| settings.get(*key).and_then(Value::as_str))
        .filter(|s| s.len() > 1)
        .map(str::to_owned)
        .collect();
    local_literals.push(paths::home()?.to_string_lossy().into_owned());
    let mut required_overrides = Vec::new();
    for (key, value) in portable.clone() {
        let encoded = serde_json::to_string(&value).unwrap();
        if local_literals
            .iter()
            .any(|literal| encoded.contains(literal))
            || command_has_local_path(&value, key == "installCommand")
        {
            portable.remove(&key);
            required_overrides.push(key);
        }
    }
    let repo = Path::new(&source.repo);
    let base_branch = git(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"], &[]).ok();
    let mut seed = base_branch
        .filter(|branch| branch != &source.branch)
        .map(|branch| -> Result<_> {
            branches::validate(&branch)?;
            Ok(Seed {
                branch,
                commit: git(repo, &["rev-parse", "HEAD^{commit}"], &[])?,
            })
        })
        .transpose()?;
    let object_format = git(
        Path::new(&source.path),
        &["rev-parse", "--show-object-format"],
        &[],
    )?;
    let anchor = git(
        Path::new(&source.path),
        &["rev-list", "--max-parents=0", "HEAD"],
        &[],
    )?
    .lines()
    .next()
    .ok_or("Repository has no root commit")?
    .to_owned();
    if let Some(base) = &seed
        && git(
            repo,
            &["merge-base", "--is-ancestor", &anchor, &base.commit],
            &[],
        )
        .is_err()
    {
        seed = None;
    }
    Ok(Request {
        project: source.project.clone(),
        task: source.task.clone(),
        branch: source.branch.clone(),
        anchor,
        object_format,
        portable: Value::Object(portable),
        source_local_fields,
        required_overrides,
        environment_required: settings.contains_key("envSource"),
        url_required: settings.contains_key("url"),
        https: settings.contains_key("httpsPorts"),
        named_ports: settings
            .get("ports")
            .and_then(Value::as_object)
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default(),
        seed,
        destination_config: destination_config.map(str::to_owned),
    })
}

fn command_has_local_path(value: &Value, command: bool) -> bool {
    match value {
        Value::String(value) if command => {
            value.starts_with("~/")
                || value.starts_with('/')
                    && !value.starts_with("/bin/")
                    && !value.starts_with("/usr/bin/")
        }
        Value::Array(values) => values
            .iter()
            .any(|value| command_has_local_path(value, command)),
        Value::Object(fields) => fields
            .iter()
            .any(|(key, value)| command_has_local_path(value, key == "command")),
        _ => false,
    }
}

fn validate(request: &Request) -> Result<()> {
    name(&request.project)?;
    name(&request.task)?;
    branches::validate(&request.branch)?;
    ensure(
        ["sha1", "sha256"].contains(&request.object_format.as_str())
            && oid(&request.anchor, &request.object_format),
        "Invalid project repository identity",
    )?;
    ensure(
        object(&request.portable)?
            .keys()
            .all(|key| !LOCAL.contains(&key.as_str())),
        "Portable configuration contains machine-local fields",
    )?;
    ensure(
        request
            .source_local_fields
            .iter()
            .all(|key| LOCAL.contains(&key.as_str()))
            && request.required_overrides.len() <= 32
            && request.named_ports.len() <= 16
            && request.named_ports.iter().all(|key| named_port(key)),
        "Invalid project migration requirements",
    )?;
    if let Some(seed) = &request.seed {
        branches::validate(&seed.branch)?;
        ensure(
            seed.branch != request.branch && oid(&seed.commit, &request.object_format),
            "Invalid project seed branch",
        )?;
    }
    Ok(())
}

pub(super) fn read_request<T: for<'de> Deserialize<'de>>(input: &mut impl Read) -> Result<T> {
    let mut bytes = Vec::new();
    input
        .take(MAX_REQUEST + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::from("Could not read project preview request"))?;
    ensure(
        bytes.len() as u64 <= MAX_REQUEST,
        "Project preview request exceeds its limit",
    )?;
    serde_json::from_slice(&bytes)
        .map_err(|_| Error::from("Invalid project preview request; contents were not logged"))
}

pub(super) fn remote_plan(destination: &str, request: &Request) -> Result<Value> {
    let report = ssh(
        destination,
        &["_transfer-project-plan", "--json"],
        Some(system::memory_input(&serde_json::to_vec(request).unwrap())?),
    )?;
    ensure(
        report["project"] == request.project
            && report["approval"]
                .as_str()
                .is_some_and(|s| oid(s, "sha256"))
            && report["repoExists"].is_boolean()
            && report["path"]
                .as_str()
                .is_some_and(|s| absolute_normalized(s) && !s.chars().any(control))
            && report["portableFields"]
                .as_array()
                .is_some_and(|v| v.iter().all(Value::is_string))
            && report["blocked"]
                .as_array()
                .is_some_and(|v| v.iter().all(Value::is_string)),
        "Invalid destination project migration proposal",
    )?;
    profiles::parse(&report["local"])?;
    Ok(report)
}

pub(super) fn receive_plan(paths: &Paths) -> Result<Value> {
    let request: Request = read_request(&mut std::io::stdin().lock())?;
    Ok(plan(paths, &request)?.report)
}

struct Plan {
    report: Value,
    profile: Value,
    config: Value,
}

fn canonical(path: &str) -> Result<()> {
    ensure(
        absolute_normalized(path)
            && path.len() <= 4095
            && !path.chars().any(control)
            && checkout::canonical(path)? == path,
        "Destination project paths must be absolute and canonical, without symlinked parents",
    )
}

fn next_ports(used: &mut Vec<(u64, u64)>) -> Result<Value> {
    for start in (32000..64000).step_by(100) {
        if used.iter().all(|(a, b)| start + 100 <= *a || start >= *b) {
            used.push((start, start + 100));
            return Ok(json!([start, start + 100]));
        }
    }
    Err("No unused suggested port range; provide local port ranges in --destination-config".into())
}

fn plan(paths: &Paths, request: &Request) -> Result<Plan> {
    validate(request)?;
    canonical(
        paths
            .config
            .to_str()
            .ok_or("Project configuration path must be UTF-8")?,
    )?;
    let config = if paths::exists(&paths.config)? {
        let metadata = fs::symlink_metadata(&paths.config)
            .map_err(|_| Error::from("Could not inspect project configuration"))?;
        ensure(
            metadata.is_file() && metadata.uid() == unsafe { libc::getuid() },
            "Project migration requires an owned regular configuration file, not a symlink",
        )?;
        config::read(paths)?
    } else {
        json!({"schemaVersion":2,"projects":{}})
    };
    let projects = object(&config["projects"])?;
    ensure(
        !projects.contains_key(&request.project),
        "Destination project already exists; its configuration is never overwritten by project migration",
    )?;
    let mut used = Vec::new();
    for profile in projects.values() {
        let profile = profiles::parse(profile)?;
        let settings = profile.value();
        for range in [settings.get("localPorts"), settings.get("httpsPorts")]
            .into_iter()
            .flatten()
            .chain(
                settings["ports"]
                    .as_object()
                    .into_iter()
                    .flat_map(|p| p.values()),
            )
        {
            used.push((
                range[0].as_f64().unwrap() as u64,
                range[1].as_f64().unwrap() as u64,
            ));
        }
    }
    let home = paths::home()?;
    let mut local = json!({
        "repo":home.join("projects").join(&request.project).to_string_lossy(),
        "worktreeRoot":home.join("worktrees").join(&request.project).to_string_lossy(),
        "localPorts":next_ports(&mut used)?
    });
    if request.environment_required {
        local["envSource"] = json!(
            paths
                .config
                .parent()
                .unwrap()
                .join("environments")
                .join(&request.project)
                .join(".env")
                .to_string_lossy()
        );
    }
    if request.https {
        local["httpsPorts"] = next_ports(&mut used)?;
    }
    for name in &request.named_ports {
        if local.get("ports").is_none() {
            local["ports"] = json!({});
        }
        local["ports"][name] = next_ports(&mut used)?;
    }
    let overrides = if let Some(path) = &request.destination_config {
        ensure(
            path.starts_with('/') || path.starts_with("~/"),
            "--destination-config must be an absolute or ~/ path on the destination",
        )?;
        let path = targets::resolve_path(&home, path)?;
        let value: Value =
            serde_json::from_slice(&records::read_private(&path)?).map_err(|_| {
                Error::from("Invalid destination-local configuration; contents were not logged")
            })?;
        object(&value)?;
        value
    } else {
        json!({})
    };
    let mut profile = request.portable.clone();
    for (key, value) in object(&local)?.iter().chain(object(&overrides)?.iter()) {
        profile[key] = value.clone();
    }
    let mut blocked = Vec::new();
    for field in &request.required_overrides {
        if overrides.get(field).is_none() {
            blocked.push(format!("Source {field} contains a machine-local path/address; provide a destination replacement in --destination-config"));
        }
    }
    if request.url_required && overrides.get("url").is_none() {
        blocked.push("Provide the destination's URL in --destination-config; the source hostname is not copied".into());
    }
    let parsed = profiles::parse(&profile)?;
    profile = parsed.value().clone();
    for key in ["repo", "worktreeRoot", "envSource"] {
        if let Some(value) = profile[key].as_str() {
            canonical(value)?;
        }
    }
    let repo = profile["repo"].as_str().unwrap();
    let root = profile["worktreeRoot"].as_str().unwrap();
    ensure(
        repo != root && !Path::new(repo).starts_with(root),
        "The main repository must not overlap its worktree root",
    )?;
    ensure(
        projects.values().all(|p| p["repo"] != repo),
        "Another destination project already owns this repository",
    )?;
    ensure(
        !records::tasks(&paths.state)?
            .iter()
            .any(|t| t.value()["repo"] == repo)
            && !records::adoptions(&paths.state)?
                .iter()
                .any(|t| t.value()["repo"] == repo),
        "Destination repository already has task records; restore its project configuration manually",
    )?;
    let repo_exists = paths::exists(Path::new(repo))?;
    let head = if repo_exists {
        ensure(
            git(Path::new(repo), &["rev-parse", "--show-toplevel"], &[])? == repo
                && inventory::registered(repo)?
                    .first()
                    .is_some_and(|t| t.path == repo && !t.bare),
            "Destination repository must be an existing main checkout, not a linked/bare repository",
        )?;
        ensure(
            git(Path::new(repo), &["rev-parse", "--show-object-format"], &[])?
                == request.object_format,
            "Git object formats differ between machines",
        )?;
        git(
            Path::new(repo),
            &["cat-file", "-e", &format!("{}^{{commit}}", request.anchor)],
            &[],
        )
        .map_err(|_| Error::from("Destination repository does not share the source history"))?;
        Some(git(Path::new(repo), &["rev-parse", "HEAD^{commit}"], &[])?)
    } else {
        if request.seed.is_none() {
            blocked.push("A new clone requires an attached source main-checkout branch different from the task branch".into());
        }
        None
    };
    let task_path = naming::creation(
        object(&profile)?,
        &request.project,
        &request.task,
        repo,
        root,
        Some(&request.branch),
    )?
    .1;
    if repo_exists {
        if let Err(error) = destination_profile(
            paths,
            &request.project,
            &request.task,
            &request.branch,
            &request.anchor,
            &request.object_format,
            &profile,
        )
        .and_then(|source| check_destination(paths, &source))
        {
            blocked.push(error.message);
        }
    } else if paths::exists(Path::new(&task_path))? {
        blocked.push(
            "The proposed destination worktree path already exists; it will not be overwritten"
                .into(),
        );
    }
    if let Some(env) = profile["envSource"].as_str() {
        ensure(
            Path::new(env).file_name().is_some_and(|n| n == ".env"),
            "Destination environment baseline must be named .env",
        )?;
        if !fs::metadata(env).is_ok_and(|m| m.is_file()) {
            blocked.push(format!("Prepare a private development baseline at {env}; environment values are never migrated"));
        }
    }
    if let Some(command) = profile["installCommand"]
        .as_array()
        .and_then(|v| v.first())
        .and_then(Value::as_str)
        && (command.starts_with('/') || !command.contains('/'))
        && system::which(command, &home).is_none()
    {
        blocked.push(
            "Destination installer executable is unavailable on its noninteractive PATH".into(),
        );
    }
    if system::which("git", &home).is_none() {
        blocked.push("Git is unavailable on the destination's noninteractive PATH".into());
    }
    let hash = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&json!([request, config, profile, repo_exists, head])).unwrap()
        )
    );
    let mut local_report = json!({});
    for key in LOCAL {
        if let Some(value) = profile.get(*key) {
            local_report[*key] = value.clone();
        }
    }
    let report = json!({
        "project":request.project,"approval":hash,"repoExists":repo_exists,"path":task_path,
        "portableFields":object(&request.portable)?.keys().collect::<Vec<_>>(),
        "sourceLocalFields":request.source_local_fields,"local":local_report,"blocked":blocked,
        "note":"Imports the approved source installer/hooks as trusted code. Review them for machine-specific assumptions or inline secrets; --destination-config can replace any project field. Environment files, Git remotes/config, task registries and service identities are not copied."
    });
    Ok(Plan {
        report,
        profile,
        config,
    })
}

pub(super) fn proposal(source: &Source, options: &Options<'_>, migration: Value) -> Value {
    let mut command = format!(
        "graftr transfer {} --to {} --approve-project {}",
        quote(&source.id()),
        quote(options.destination),
        migration["approval"].as_str().unwrap()
    );
    if let Some(path) = options.destination_config {
        command.push_str(&format!(" --destination-config {}", quote(path)));
    }
    if options.no_herdr {
        command.push_str(" --no-herdr");
    }
    for path in options.extras {
        command.push_str(&format!(" --include {}", quote(path)));
    }
    json!({"id":source.id(),"status":"attention","dryRun":options.dry_run,"canTransfer":false,"sourceKept":true,
        "destination":options.destination,"path":migration["path"],"projectMigration":migration,
        "findings":[{"status":"attention","message":"The missing destination project needs explicit approval of its current migration proposal."},
            {"status":"not-checked","message":"Project creation, installer/hook execution, port allocation, exact bundle size and final environment verification require an approved real transfer."}],
        "approvalCommand":command,"message":"Destination project is missing. No project or worktree was created. Review the migration proposal and prepare any blocked local settings before approving it."})
}

pub(super) fn format_plan(plan: &Value) -> String {
    let mut text = format!(
        "\nProject migration proposal for {}:\n  Repository: {} ({})\n  Worktree root: {}\n  Local ports: {}\n  Portable fields: {}",
        plan["project"].as_str().unwrap_or(""),
        plan["local"]["repo"].as_str().unwrap_or(""),
        if plan["repoExists"] == true {
            "use existing clone; keep its files/config"
        } else {
            "create from a main-branch bundle"
        },
        plan["local"]["worktreeRoot"].as_str().unwrap_or(""),
        plan["local"]["localPorts"],
        plan["portableFields"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    );
    for key in ["envSource", "httpsPorts", "ports", "url"] {
        if let Some(value) = plan["local"].get(key) {
            text.push_str(&format!("\n  {key}: {value}"));
        }
    }
    for block in plan["blocked"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        text.push_str(&format!("\n  Blocked: {block}"));
    }
    text.push_str("\n  Existing destination projects are never overwritten; known local fields stay destination-owned.\n  Approval imports trusted installer/hook commands; review the source profile for local assumptions and inline secrets first.");
    text
}

pub(super) fn send(
    paths: &Paths,
    source: &Source,
    destination: &str,
    request: &Request,
    migration: &Value,
    approval: &str,
    progress: &Progress,
) -> Result<()> {
    ensure(
        oid(approval, "sha256") && migration["approval"] == approval,
        "Project migration proposal changed or approval does not match; run --dry-run again",
    )?;
    ensure(
        migration["blocked"].as_array().is_some_and(Vec::is_empty),
        "Project migration has blocked local settings; review --dry-run and --destination-config before approving",
    )?;
    let directory = category(paths, &source.id())?.join(format!("project-{approval}"));
    locks::private_directory(&directory)?;
    let bundle = directory.join(format!("{}.bundle", system::uuid()?));
    let (bundle_size, bundle_sha256) = if migration["repoExists"] == true {
        (0, String::new())
    } else {
        let seed = request.seed.as_ref().ok_or("Missing project seed branch")?;
        let reference = format!("refs/heads/{}", seed.branch);
        ensure(
            git(Path::new(&source.repo), &["rev-parse", &reference], &[])? == seed.commit,
            "Source main branch changed; run --dry-run and approve again",
        )?;
        git(Path::new(&source.repo), &["merge-base","--is-ancestor",&request.anchor,&seed.commit],&[]).map_err(|_| Error::from("The source main branch does not share the task's root history; configure a matching destination clone manually"))?;
        progress.run_timed("bundle-project", &source.project, || -> Result<()> {
            git(
                Path::new(&source.repo),
                &["bundle", "create", bundle.to_str().unwrap(), &reference],
                &[],
            )?;
            fs::set_permissions(&bundle, fs::Permissions::from_mode(0o600))
                .map_err(|_| Error::from("Could not protect project bundle"))?;
            Ok(())
        })?;
        let result = checksum(&bundle)?;
        ensure(
            result.0 <= MAX_BUNDLE,
            "Project main-branch bundle exceeds 2 GiB",
        )?;
        result
    };
    let header = Header {
        request: request.clone(),
        approval: approval.into(),
        bundle_size,
        bundle_sha256,
    };
    let packet = directory.join(format!("{}.packet", system::uuid()?));
    let mut output = create_file(&packet)?;
    let bytes = serde_json::to_vec(&header).unwrap();
    ensure(
        bytes.len() as u64 <= MAX_REQUEST,
        "Project migration request exceeds its limit",
    )?;
    output
        .write_all(MAGIC)
        .and_then(|_| output.write_all(&(bytes.len() as u64).to_be_bytes()))
        .and_then(|_| output.write_all(&bytes))
        .map_err(|_| Error::from("Could not write project packet"))?;
    if bundle_size > 0 {
        std::io::copy(&mut private_file(&bundle)?, &mut output)
            .map_err(|_| Error::from("Could not write project bundle packet"))?;
    }
    output
        .sync_all()
        .map_err(|_| Error::from("Could not persist project packet"))?;
    let result = progress.run_timed("create-destination-project", destination, || {
        ssh(
            destination,
            &["_transfer-project-receive", "--json"],
            Some(private_file(&packet)?),
        )
    })?;
    ensure(
        result["project"] == source.project
            && result["approval"] == approval
            && result["status"] == "created",
        "Project creation acknowledgement did not match; inspect destination project configuration before retrying",
    )
}

pub(super) fn receive(paths: &Paths, progress: &Progress) -> Result<Value> {
    let _umask = PrivateUmask(unsafe { libc::umask(0o077) });
    let mut input = std::io::stdin().lock();
    let mut magic = [0; 8];
    let mut length = [0; 8];
    input
        .read_exact(&mut magic)
        .and_then(|_| input.read_exact(&mut length))
        .map_err(|_| Error::from("Incomplete project migration header"))?;
    ensure(
        &magic == MAGIC && u64::from_be_bytes(length) <= MAX_REQUEST,
        "Invalid project migration header",
    )?;
    let mut bytes = vec![0; u64::from_be_bytes(length) as usize];
    input
        .read_exact(&mut bytes)
        .map_err(|_| Error::from("Incomplete project migration request"))?;
    let header: Header = serde_json::from_slice(&bytes)
        .map_err(|_| Error::from("Invalid project migration request"))?;
    validate(&header.request)?;
    ensure(
        oid(&header.approval, "sha256")
            && header.bundle_size <= MAX_BUNDLE
            && (header.bundle_size == 0 && header.bundle_sha256.is_empty()
                || header.bundle_size > 0 && oid(&header.bundle_sha256, "sha256")),
        "Invalid project migration bundle identity",
    )?;
    let _project = locks::project(paths, &header.request.project)?;
    let planned = plan(paths, &header.request)?;
    ensure(
        planned.report["approval"] == header.approval,
        "Destination project proposal changed; run --dry-run and approve again before creating anything",
    )?;
    ensure(
        planned.report["blocked"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "Destination project has blocked local settings; no project was created",
    )?;
    let mut directory = paths.state.clone();
    for component in [
        "project-transfers",
        &header.request.project,
        &header.approval,
    ] {
        directory.push(component);
        locks::private_directory(&directory)?;
    }
    let receipt_path = directory.join("receipt.json");
    ensure(
        !paths::exists(&receipt_path)?,
        "An earlier project migration may have started; preserve its receipt and inspect the destination before retrying",
    )?;
    let bundle = directory.join(format!("{}.bundle", system::uuid()?));
    if header.bundle_size > 0 {
        let mut output = create_file(&bundle)?;
        let copied = std::io::copy(&mut input.by_ref().take(header.bundle_size), &mut output)
            .map_err(|_| Error::from("Project bundle transfer was interrupted"))?;
        output
            .sync_all()
            .map_err(|_| Error::from("Could not persist project bundle"))?;
        drop(output);
        ensure(
            copied == header.bundle_size
                && checksum(&bundle)? == (header.bundle_size, header.bundle_sha256.clone()),
            "Project bundle checksum/length mismatch; no project was created",
        )?;
    }
    let mut tail = [0; 1];
    ensure(
        input.read(&mut tail).is_ok_and(|n| n == 0),
        "Unexpected trailing project migration data",
    )?;
    let rechecked = plan(paths, &header.request)?;
    ensure(
        rechecked.report["approval"] == header.approval,
        "Destination project proposal changed during receipt; no project was created",
    )?;
    let repo = planned.profile["repo"].as_str().unwrap();
    let root = planned.profile["worktreeRoot"].as_str().unwrap();
    let mut record = json!({"schemaVersion":1,"project":header.request.project,"approval":header.approval,"repo":repo,"worktreeRoot":root,"status":"creating"});
    if planned.report["repoExists"] != true {
        ensure(
            header.bundle_size > 0,
            "New destination repository requires a main-branch bundle",
        )?;
        let seed = header
            .request
            .seed
            .as_ref()
            .ok_or("Missing source main-branch identity")?;
        let private = directory.join("verify.git");
        init_repository(&private, &header.request.object_format)?;
        git(
            &private,
            &["bundle", "verify", bundle.to_str().unwrap()],
            &[],
        )?;
        let heads = git(
            &private,
            &["bundle", "list-heads", bundle.to_str().unwrap()],
            &[],
        )?;
        ensure(
            heads == format!("{} refs/heads/{}", seed.commit, seed.branch),
            "Project bundle does not match the approved main branch",
        )?;
        stateio::atomic_write(
            &receipt_path,
            &serde_json::to_vec_pretty(&record).unwrap(),
            true,
        )?;
        let parent = Path::new(repo)
            .parent()
            .ok_or("Invalid project repository path")?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|_| Error::from("Could not create destination repository parent"))?;
        canonical(repo)?;
        ensure(
            !paths::exists(Path::new(repo))?,
            "Destination repository appeared before creation; nothing was overwritten",
        )?;
        progress.run_timed(
            "clone-project",
            &header.request.project,
            || -> Result<()> {
                git(
                    parent,
                    &[
                        "clone",
                        "--no-local",
                        "--template=",
                        "--branch",
                        &seed.branch,
                        "--",
                        bundle.to_str().unwrap(),
                        repo,
                    ],
                    &[],
                )?;
                git(Path::new(repo), &["remote", "remove", "origin"], &[])?;
                ensure(
                    git(Path::new(repo), &["rev-parse", "HEAD^{commit}"], &[])? == seed.commit,
                    "Created repository has a different main-branch identity",
                )?;
                Ok(())
            },
        )?;
    } else {
        ensure(
            header.bundle_size == 0,
            "Existing destination repository must not receive a project bundle",
        )?;
        stateio::atomic_write(
            &receipt_path,
            &serde_json::to_vec_pretty(&record).unwrap(),
            true,
        )?;
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)
        .map_err(|_| Error::from("Could not create destination worktree root"))?;
    canonical(root)?;
    let _state = locks::state(paths)?;
    let current = if paths::exists(&paths.config)? {
        config::read(paths)?
    } else {
        json!({"schemaVersion":2,"projects":{}})
    };
    ensure(
        current == planned.config,
        "Destination configuration changed after project creation; keep the repository and migration receipt, then review configuration manually",
    )?;
    let mut config = current;
    config["projects"][&header.request.project] = planned.profile.clone();
    locks::private_directory(
        paths
            .config
            .parent()
            .ok_or("Missing project configuration directory")?,
    )?;
    stateio::atomic_write(
        &paths.config,
        &serde_json::to_vec_pretty(&config).unwrap(),
        false,
    )?;
    record["status"] = json!("complete");
    stateio::atomic_write(
        &receipt_path,
        &serde_json::to_vec_pretty(&record).unwrap(),
        false,
    )?;
    Ok(
        json!({"project":header.request.project,"approval":header.approval,"status":"created","repo":repo,"worktreeRoot":root}),
    )
}
