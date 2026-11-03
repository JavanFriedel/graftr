mod addresses;
mod adopt;
mod branches;
mod checkout;
mod cleanup;
mod completion;
mod config;
mod confirmation;
mod dependencies;
mod doctor;
mod envdoc;
mod environment;
mod envrefresh;
mod events;
mod health;
mod help;
mod herdr;
mod history;
mod hooks;
mod human;
mod inventory;
mod labels;
mod lifecycle;
mod locks;
mod logs;
mod migrate;
mod naming;
mod paths;
mod plugin;
mod postgres;
mod profiles;
mod progress;
mod prune;
mod records;
mod scheduler;
mod setup;
mod stateio;
mod status;
mod storage;
mod system;
mod tailscale;
mod targets;
mod terminal;
mod validation;

use help::Catalog;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    env,
    io::{self, IsTerminal, Write},
    process::ExitCode,
};

struct Failure {
    message: String,
    phase: &'static str,
    code: &'static str,
    details: Box<Value>,
}
impl Failure {
    fn at(error: validation::Error, phase: &'static str) -> Self {
        let code = if !["OPERATION_FAILED", "HERDR_CONNECTION_UNAVAILABLE"].contains(&error.code) {
            error.code
        } else {
            match phase {
                "arguments" => "INVALID_ARGUMENTS",
                "state" => "STATE_UNAVAILABLE",
                "target" => "TARGET_RESOLUTION_FAILED",
                "preflight" => "PREFLIGHT_FAILED",
                "confirmation" => "CONFIRMATION_FAILED",
                _ => "OPERATION_FAILED",
            }
        };
        Self {
            code,
            message: error.message,
            phase,
            details: Value::Null.into(),
        }
    }
    fn target_at(error: targets::TargetError, phase: &'static str) -> Self {
        let details = error.details;
        let mut failure = Self::at(
            validation::Error {
                code: error.code,
                message: error.message,
            },
            phase,
        );
        failure.details = details.into();
        failure
    }
}
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            message,
            phase: "arguments",
            code: "INVALID_ARGUMENTS",
            details: Value::Null.into(),
        }
    }
}
impl From<validation::Error> for Failure {
    fn from(error: validation::Error) -> Self {
        Self::at(error, "operation")
    }
}

fn parse(
    args: &[String],
    catalog: &Catalog,
    strict: bool,
) -> Result<(Vec<String>, BTreeMap<String, String>), String> {
    let (mut positionals, mut flags) = (Vec::new(), BTreeMap::new());
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            positionals.extend_from_slice(&args[index + 1..]);
            break;
        }
        if arg == "-h" {
            flags.insert("help".into(), "true".into());
        } else if let Some(option) = arg.strip_prefix("--") {
            let (name, inline) = option
                .split_once('=')
                .map_or((option, None), |(k, v)| (k, Some(v)));
            if let Some(spec) = catalog.cli_options.get(name) {
                if spec.kind == "string" {
                    let value = if let Some(value) = inline {
                        value.to_owned()
                    } else {
                        index += 1;
                        match args.get(index) {
                            Some(value) => {
                                if strict && value.starts_with('-') && value != "-" {
                                    return Err(format!(
                                        "Option '--{name}' argument is ambiguous.\nDid you forget to specify the option argument for '--{name}'?\nTo specify an option argument starting with a dash use '--{name}=-XYZ'."
                                    ));
                                }
                                value.clone()
                            }
                            None if !strict => "true".into(),
                            None => {
                                return Err(format!("Option '--{name} <value>' argument missing"));
                            }
                        }
                    };
                    flags.insert(name.to_owned(), value);
                } else {
                    if strict && inline.is_some() {
                        return Err(format!("Option '--{name}' does not take an argument"));
                    }
                    flags.insert(name.to_owned(), "true".into());
                }
            } else if strict {
                return Err(format!(
                    "Unknown option '--{name}'. To specify a positional argument starting with a '-', place it at the end of the command after '--', as in '-- \"--{name}\""
                ));
            }
        } else if arg.starts_with('-') && arg != "-" {
            let grouped = arg.chars().count() > 2;
            for short in arg[1..].chars() {
                if short == 'h' {
                    flags.insert("help".into(), "true".into());
                } else if strict {
                    let option = if grouped {
                        short.to_string()
                    } else {
                        arg.clone()
                    };
                    return Err(format!(
                        "Unknown option '{option}'. To specify a positional argument starting with a '-', place it at the end of the command after '--', as in '-- \"{option}\""
                    ));
                }
            }
        } else {
            positionals.push(arg.clone());
        }
        index += 1;
    }
    Ok((positionals, flags))
}
fn repeated_values(args: &[String], name: &str, catalog: &Catalog) -> Vec<String> {
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() && args[index] != "--" {
        if let Some(option) = args[index].strip_prefix("--") {
            let (key, inline) = option
                .split_once('=')
                .map_or((option, None), |(k, v)| (k, Some(v)));
            if catalog
                .cli_options
                .get(key)
                .is_some_and(|spec| spec.kind == "string")
            {
                let value = if let Some(value) = inline {
                    Some(value.to_owned())
                } else {
                    index += 1;
                    args.get(index).cloned()
                };
                if key == name {
                    values.extend(value);
                }
            }
        }
        index += 1;
    }
    values
}
fn validate_flags(command: &str, flags: &BTreeMap<String, String>) -> Result<(), String> {
    let has = |key: &str| flags.contains_key(key);
    for (key, commands, message) in [
        (
            "yes",
            &["remove", "freeze"][..],
            "--yes is only valid for remove/freeze",
        ),
        ("live", &["doctor"][..], "--live is only valid for doctor"),
        ("latest", &["logs"][..], "--latest is only valid for logs"),
    ] {
        if has(key) && !commands.contains(&command) {
            return Err(message.into());
        }
    }
    if has("latest") && has("json") {
        return Err("--latest prints a raw log; it cannot be combined with --json".into());
    }
    for (key, commands, message) in [
        (
            "no-herdr",
            &["create", "setup", "open", "thaw"][..],
            "--no-herdr is only valid for create/setup/open/thaw",
        ),
        (
            "until",
            &["freeze", "schedule"][..],
            "--until is only valid for freeze/schedule",
        ),
        ("note", &["freeze"][..], "--note is only valid for freeze"),
        (
            "cancel",
            &["schedule"][..],
            "--cancel is only valid for schedule",
        ),
        ("due", &["thaw"][..], "--due is only valid for thaw"),
        (
            "apply",
            &["env", "migrate", "prune", "adopt"][..],
            "--apply is only valid for env refresh, migrate and prune, or adopt --environment",
        ),
        (
            "older-than",
            &["prune"][..],
            "--older-than is only valid for prune",
        ),
        (
            "take-baseline",
            &["env", "adopt"][..],
            "--take-baseline is only valid for env refresh or adopt --environment",
        ),
    ] {
        if has(key) && !commands.contains(&command) {
            return Err(message.into());
        }
    }
    if (has("environment") || has("hook")) && command != "adopt" {
        return Err("--environment and --hook are only valid for adopt".into());
    }
    if has("json")
        && ![
            "create",
            "adopt",
            "setup",
            "open",
            "close",
            "cleanup",
            "remove",
            "nuke",
            "health",
            "doctor",
            "env",
            "label",
            "logs",
            "list",
            "status",
            "history",
            "reconcile",
            "migrate",
            "prune",
            "freeze",
            "frozen",
            "thaw",
            "schedule",
            "plugin",
        ]
        .contains(&command)
    {
        return Err("--json is only valid for lifecycle and inspection commands".into());
    }
    if has("wait") && !["create", "setup", "open"].contains(&command) {
        return Err("--wait is only valid for create/setup/open".into());
    }
    if (has("skip-install") || has("no-setup")) && !["create", "setup"].contains(&command) {
        return Err("Setup options are only valid for create/setup".into());
    }
    if has("no-setup") && command != "create" {
        return Err("--no-setup is only valid for create".into());
    }
    if has("install") && !["setup", "adopt"].contains(&command) {
        return Err("--install is only valid for setup/adopt".into());
    }
    if has("install") && has("skip-install") {
        return Err("Choose --install or --skip-install, not both".into());
    }
    for key in ["branch", "base"] {
        if has(key) && command != "create" {
            return Err(format!("--{key} is only valid for create"));
        }
    }
    if has("branch") && has("base") {
        return Err("Choose --branch or --base, not both".into());
    }
    if has("project")
        && ![
            "open", "close", "cleanup", "remove", "nuke", "health", "doctor", "env", "label",
            "logs", "freeze", "thaw", "schedule", "adopt",
        ]
        .contains(&command)
    {
        return Err("--project is only valid for worktree-targeted commands".into());
    }
    Ok(())
}

fn envelope(
    command: Option<&str>,
    outcome: &str,
    data: Value,
    error: Value,
    progress: &progress::Progress,
) -> Value {
    json!({"schemaVersion": 1, "command": command, "ok": !["failed", "partial"].contains(&outcome), "outcome": outcome,
        "data": data, "error": error, "progress": progress.steps()})
}

enum Reply {
    Line(String),
    Raw(String),
    Unsuccessful(String),
}
fn output_setup(
    command: &str,
    result: &setup::SetupResult,
    json: bool,
    progress: &progress::Progress,
) -> Result<Reply, Failure> {
    let outcome = match result.receipt["status"].as_str().unwrap() {
        "pending" => "queued",
        "ready" | "skipped" => "completed",
        _ => "partial",
    };
    let failed = outcome == "partial";
    if !json && command == "open" && (failed || result.task.is_none()) {
        return Err(validation::Error::from(result.receipt["message"].as_str().unwrap()).into());
    }
    let output = if json {
        serde_json::to_string_pretty(&envelope(Some(command), outcome, setup::data(result), if failed {json!({"code": if result.receipt["status"] == "unmanaged" {"SETUP_UNMANAGED"} else {"SETUP_INCOMPLETE"}, "message": result.receipt["message"], "phase": "operation", "details": null, "recovery": ["Keep the checkout. Inspect graftr status and the setup receipt/log before retrying.", "Fix the reported issue, then run graftr setup --wait inside its owned Herdr workspace."]})} else {Value::Null}, progress)).unwrap()
    } else {
        setup::format(result, terminal::columns())
    };
    if !json && ![Some("ready"), Some("pending")].contains(&result.receipt["status"].as_str()) {
        eprintln!("graftr: {}", result.receipt["message"].as_str().unwrap());
    }
    Ok(if failed {
        Reply::Unsuccessful(output)
    } else {
        Reply::Line(output)
    })
}

fn frozen_reply(
    command: &str,
    record: Value,
    json: bool,
    progress: &progress::Progress,
) -> Result<Reply, Failure> {
    let queued = record.get("logId").is_some();
    let output = if json {
        serde_json::to_string_pretty(&envelope(
            Some(command),
            if queued { "queued" } else { "completed" },
            if queued {
                record.clone()
            } else {
                storage::public(&record)?
            },
            Value::Null,
            progress,
        ))
        .unwrap()
    } else if queued {
        lifecycle::format_action(&record, terminal::columns())
    } else {
        storage::format_record(&record, terminal::columns())
    };
    Ok(Reply::Line(output))
}
fn thaw_reply(
    command: &str,
    result: Value,
    due: bool,
    json: bool,
    progress: &progress::Progress,
) -> Result<Reply, Failure> {
    let attention = if due {
        result["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["status"] == "attention")
    } else {
        result["status"] == "attention"
    };
    let message = if due {
        "One or more scheduled returns need review; inspect the results and graftr frozen/status."
    } else {
        result["message"].as_str().unwrap()
    };
    let output = if json {
        serde_json::to_string_pretty(&envelope(Some(command), if attention {"partial"} else {"completed"}, result.clone(), if attention {json!({"code": "THAW_ATTENTION", "message": message, "phase": "operation", "details": null, "recovery": ["Inspect graftr frozen and graftr status; preserve the recovery record, branch and any active checkout/leases.", "Review cleanup/setup effects and systemd timer state before explicitly retrying. Do not reset branches, force deletion or automatically replay hooks."]})} else {Value::Null}, progress)).unwrap()
    } else if due {
        storage::format_thaws(result["results"].as_array().unwrap(), terminal::columns())
    } else {
        storage::format_thaw(&result, terminal::columns())
    };
    Ok(if attention {
        Reply::Unsuccessful(output)
    } else {
        Reply::Line(output)
    })
}
fn execute(
    args: &[String],
    catalog: &Catalog,
    json: bool,
    progress: &progress::Progress,
) -> Result<Reply, Failure> {
    let before_separator = &args[..args.iter().position(|s| s == "--").unwrap_or(args.len())];
    let help_requested = before_separator.iter().any(|s| s == "--help" || s == "-h");
    let (preliminary, _) = parse(args, catalog, false)?;
    let topic = preliminary.first().map(String::as_str);
    if help_requested || topic == Some("help") {
        if !help_requested && preliminary.len() > 2 {
            return Err("help accepts one command or topic".to_owned().into());
        }
        return catalog
            .render(
                if topic == Some("help") {
                    preliminary.get(1).map(String::as_str)
                } else {
                    topic
                },
                terminal::columns(),
            )
            .map(Reply::Line)
            .map_err(Into::into);
    }
    let (positionals, flags) = parse(args, catalog, true)?;
    let Some(command) = positionals.first().map(String::as_str) else {
        return catalog
            .render(None, terminal::columns())
            .map(Reply::Line)
            .map_err(Into::into);
    };
    if flags.contains_key("help") {
        return catalog
            .render(Some(command), terminal::columns())
            .map(Reply::Line)
            .map_err(Into::into);
    }
    validate_flags(command, &flags)?;
    // Unknown commands never fall back to another runtime.
    let known_command = [
        "history",
        "completion",
        "status",
        "frozen",
        "list",
        "logs",
        "doctor",
        "health",
        "label",
        "env",
        "adopt",
        "plugin",
        "prune",
        "migrate",
        "hook",
        "setup",
        "create",
        "open",
        "close",
        "cleanup",
        "remove",
        "nuke",
        "freeze",
        "thaw",
        "schedule",
        "_thaw",
        "_thaw-message",
        "reconcile",
        "_startup",
        "_event",
    ]
    .contains(&command);
    if command == "completion" {
        if json {
            return Err("--json is only valid for lifecycle and inspection commands"
                .to_owned()
                .into());
        }
        if positionals.len() != 2 || positionals[1] != "bash" {
            return Err(
                "completion requires bash; other shells are not yet supported"
                    .to_owned()
                    .into(),
            );
        }
        return Ok(Reply::Line(
            include_str!("../completions/graftr.bash")
                .trim_end()
                .to_owned(),
        ));
    }
    if command == "plugin" {
        if positionals.len() != 2 || positionals[1] != "install" {
            return Err(
                "plugin requires install with no plugin target or extra arguments"
                    .to_owned()
                    .into(),
            );
        }
        let report =
            plugin::install(progress).map_err(|error| Failure::target_at(error, "operation"))?;
        return Ok(Reply::Line(if json {
            serde_json::to_string_pretty(&envelope(
                Some(command),
                "completed",
                report,
                Value::Null,
                progress,
            ))
            .unwrap()
        } else {
            plugin::format(&report, terminal::columns())
        }));
    }
    if command == "list" && positionals.len() != 2 {
        return Err("list requires PROJECT".to_owned().into());
    }
    if command == "logs" && positionals.len() > 2 {
        return Err("Unexpected logs arguments".to_owned().into());
    }
    if command == "doctor" && positionals.len() > 2 {
        return Err("Unexpected doctor arguments".to_owned().into());
    }
    if ["health", "label"].contains(&command) && positionals.len() > 2 {
        return Err(format!("Unexpected {command} arguments").into());
    }
    if command == "logs" && flags.contains_key("latest") && json {
        return Err(
            "--latest prints a raw log; it cannot be combined with --json"
                .to_owned()
                .into(),
        );
    }
    let paths = if ["_thaw", "_thaw-message"].contains(&command) {
        if positionals.len() != 5
            || !positionals
                .get(3)
                .is_some_and(|p| validation::absolute_normalized(p))
            || !positionals
                .get(4)
                .is_some_and(|p| validation::absolute_normalized(p))
        {
            return Err(Failure::at(
                validation::Error::from(
                    "Frozen workers require explicit absolute configuration/state paths",
                ),
                "state",
            ));
        }
        paths::Paths {
            config: positionals[3].clone().into(),
            state: positionals[4].clone().into(),
        }
    } else {
        paths::defaults().map_err(|error| Failure::at(error, "state"))?
    };
    let state = &paths.state;
    if !known_command {
        return Err(format!("Unknown command: {command}").into());
    }
    if ["reconcile", "_startup", "_event"].contains(&command) {
        prune::automatic(&paths);
        if positionals.len() != 1 {
            return Err("Unexpected command arguments".to_owned().into());
        }
        let engine = setup::Engine {
            paths: &paths,
            progress,
        };
        let events = events::Events { engine: &engine };
        if command == "_event" {
            let event: Value = serde_json::from_str(&env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or("null".into())).map_err(|_| validation::Error {code: "INVALID_JSON", message: "Invalid JSON; review configuration/state or the Herdr response (contents were not logged)".into()})?;
            return Ok(match events.handle(&event)? {
                None => Reply::Raw(String::new()),
                Some(events::Reply::Native(result)) => {
                    Reply::Line(serde_json::to_string_pretty(&result).unwrap())
                }
                Some(events::Reply::Setup(result)) => {
                    if ![Some("ready"), Some("pending")]
                        .contains(&result.receipt["status"].as_str())
                    {
                        eprintln!("graftr: {}", result.receipt["message"].as_str().unwrap());
                    }
                    Reply::Line(serde_json::to_string_pretty(&setup::data(&result)).unwrap())
                }
            });
        }
        if command == "_startup" {
            let report = events.startup(&herdr::caller_socket()?);
            let failed = report["reconciliation"]["status"] == "attention"
                || report["thaw"]["status"] == "attention";
            let output = serde_json::to_string_pretty(&report).unwrap();
            return Ok(if failed {
                Reply::Unsuccessful(output)
            } else {
                Reply::Line(output)
            });
        }
        lifecycle::Lifecycle { engine: &engine }.reconcile()?;
        let message = "Reconciled owned workspaces; no processes or checkouts were removed.";
        return Ok(Reply::Line(if json {
            serde_json::to_string_pretty(&envelope(
                Some(command),
                "completed",
                json!({"message": message}),
                Value::Null,
                progress,
            ))
            .unwrap()
        } else {
            terminal::wrap(message, terminal::columns())
        }));
    }
    if ["freeze", "thaw", "schedule", "_thaw", "_thaw-message"].contains(&command) {
        let engine = setup::Engine {
            paths: &paths,
            progress,
        };
        let storage = storage::Storage { engine: &engine };
        let lifecycle = lifecycle::Lifecycle { engine: &engine };
        if command == "_thaw" {
            let result = storage.thaw(&positionals[1], true, Some(&positionals[2]), false)?;
            let failed = result["status"] == "attention";
            let output = serde_json::to_string_pretty(&result).unwrap();
            return Ok(if failed {
                Reply::Unsuccessful(output)
            } else {
                Reply::Line(output)
            });
        }
        if command == "_thaw-message" {
            let record = records::frozen_task(state, &positionals[1])?.filter(|r| r.value()["generation"] == positionals[2]).ok_or_else(|| validation::Error::from("This thaw message belongs to an older recovery record; inspect graftr frozen/status"))?;
            let text = storage::message(record.value());
            if io::stdin().is_terminal() {
                println!("{text}");
                print!("\nPress Enter to dismiss this pane. ");
                io::stdout()
                    .flush()
                    .map_err(|_| validation::Error::from("Could not display thaw message"))?;
                let mut line = String::new();
                io::stdin().read_line(&mut line).map_err(|_| {
                    validation::Error::from("Could not read thaw message dismissal")
                })?;
                return Ok(Reply::Raw(String::new()));
            }
            return Ok(Reply::Line(text));
        }
        if command == "freeze" {
            if positionals.len() > 2 {
                return Err("Unexpected freeze arguments".to_owned().into());
            }
            let native_action = herdr::in_plugin_action()
                && env::var("HERDR_PLUGIN_ACTION_ID").as_deref() == Ok("freeze");
            let context: Value = if native_action {
                serde_json::from_str(&env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or("{}".into())).map_err(|_| Failure::at(validation::Error {code: "INVALID_JSON", message: "Invalid JSON; review configuration/state or the Herdr response (contents were not logged)".into()}, "arguments"))?
            } else {
                json!({})
            };
            validation::object(&context).map_err(|e| Failure::at(e, "arguments"))?;
            if context["invocation_source"] == "graftr-confirmed-freeze" {
                if positionals.len() != 1
                    || flags.contains_key("until")
                    || flags.contains_key("note")
                {
                    return Err("Confirmed freeze worker takes no new arguments"
                        .to_owned()
                        .into());
                }
                let request: Value = serde_json::from_str(validation::text(&context["correlation_id"]).map_err(|e| Failure::at(e, "arguments"))?).map_err(|_| Failure::at(validation::Error {code: "INVALID_JSON", message: "Invalid JSON; review configuration/state or the Herdr response (contents were not logged)".into()}, "arguments"))?;
                let record = storage.finish_freeze(
                    validation::text(&request["id"])?,
                    validation::text(&request["generation"])?,
                )?;
                return frozen_reply(command, record, json, progress);
            }
            if let Some(date) = flags.get("until") {
                storage::thaw_date(date).map_err(|e| Failure::at(e, "arguments"))?;
            }
            storage::note(flags.get("note").map(String::as_str))
                .map_err(|e| Failure::at(e, "arguments"))?;
            if let Some(id) = positionals.get(1)
                && !flags.contains_key("until")
                && !flags.contains_key("note")
            {
                let recovering = records::frozen(state)?.into_iter().find(|r| {
                    r.value()["id"] == *id
                        && flags
                            .get("project")
                            .is_none_or(|p| id.starts_with(&format!("{p}--")))
                });
                if let Some(record) = recovering.filter(|r| r.value()["status"] == "freezing")
                    && !records::tasks(state)?
                        .iter()
                        .any(|t| t.value()["id"] == record.value()["id"])
                {
                    let record = storage
                        .finish_freeze(id, record.value()["generation"].as_str().unwrap())?;
                    return frozen_reply(command, record, json, progress);
                }
            }
            let id = targets::resolve_id(
                &paths,
                positionals.get(1).map(String::as_str),
                flags.get("project").map(String::as_str),
            )
            .map_err(|e| Failure::target_at(e, "target"))?;
            let target = lifecycle
                .preview(&id)
                .map_err(|e| Failure::at(e, "preflight"))?;
            if !flags.contains_key("yes")
                && !native_action
                && !confirmation::confirm(&target, "freeze")
                    .map_err(|e| Failure::at(e, "confirmation"))?
            {
                return Ok(if json {
                    Reply::Line(
                        serde_json::to_string_pretty(&envelope(
                            Some(command),
                            "cancelled",
                            json!({"id": id}),
                            Value::Null,
                            progress,
                        ))
                        .unwrap(),
                    )
                } else {
                    eprintln!("Freeze cancelled; no resources changed.");
                    Reply::Raw(String::new())
                });
            }
            let record = storage.freeze(
                &id,
                &target,
                flags.get("until").map(String::as_str),
                flags.get("note").map(String::as_str),
            )?;
            return frozen_reply(command, record, json, progress);
        }
        if flags.contains_key("due") {
            if positionals.len() != 1
                || flags.contains_key("project")
                || flags.contains_key("no-herdr")
            {
                return Err(
                    "thaw --due uses only overdue tasks for the current Herdr server"
                        .to_owned()
                        .into(),
                );
            }
            let results = storage.due(&herdr::caller_socket()?)?;
            return thaw_reply(command, json!({"results": results}), true, json, progress);
        }
        if positionals.len() != 2 {
            return Err(format!("{command} requires an explicit frozen TARGET").into());
        }
        let record = storage::resolve(
            &paths,
            &positionals[1],
            flags.get("project").map(String::as_str),
        )
        .map_err(|e| Failure::at(e, "target"))?;
        if command == "schedule" {
            if flags.contains_key("until") == flags.contains_key("cancel") {
                return Err(Failure::at(
                    validation::Error::from(
                        "schedule requires exactly one of --until TIMESTAMP or --cancel",
                    ),
                    "operation",
                ));
            }
            let record = storage.schedule(
                record["id"].as_str().unwrap(),
                flags.get("until").map(String::as_str),
            )?;
            return frozen_reply(command, record, json, progress);
        }
        let result = storage.thaw(
            record["id"].as_str().unwrap(),
            false,
            None,
            flags.contains_key("no-herdr"),
        )?;
        return thaw_reply(command, result, false, json, progress);
    }
    if ["cleanup", "close", "remove", "nuke"].contains(&command) {
        prune::automatic(&paths);
        if command == "nuke" && (positionals.len() != 2 || positionals[1] == "@") {
            return Err("nuke requires an explicit worktree target (not @)"
                .to_owned()
                .into());
        }
        if positionals.len() > 2 {
            return Err("Unexpected command arguments".to_owned().into());
        }
        let id = targets::resolve_id(
            &paths,
            positionals.get(1).map(String::as_str),
            flags.get("project").map(String::as_str),
        )
        .map_err(|e| Failure::target_at(e, "target"))?;
        let engine = setup::Engine {
            paths: &paths,
            progress,
        };
        let lifecycle = lifecycle::Lifecycle { engine: &engine };
        if command == "nuke" {
            if positionals.len() != 2 || positionals[1] == "@" {
                return Err("nuke requires an explicit worktree target (not @)"
                    .to_owned()
                    .into());
            }
            let target = lifecycle
                .preview_nuke(&id)
                .map_err(|e| Failure::at(e, "preflight"))?;
            if !confirmation::confirm(&target, "nuke")
                .map_err(|e| Failure::at(e, "confirmation"))?
            {
                return Ok(if json {
                    Reply::Line(
                        serde_json::to_string_pretty(&envelope(
                            Some(command),
                            "cancelled",
                            json!({"id": id}),
                            Value::Null,
                            progress,
                        ))
                        .unwrap(),
                    )
                } else {
                    eprintln!("Nuke cancelled; no resources changed.");
                    Reply::Raw(String::new())
                });
            }
            let result = lifecycle.nuke(&id, &target)?;
            return Ok(Reply::Line(if json {
                serde_json::to_string_pretty(&envelope(
                    Some(command),
                    "completed",
                    result,
                    Value::Null,
                    progress,
                ))
                .unwrap()
            } else {
                lifecycle::format_action(&result, terminal::columns())
            }));
        }
        let result = if command == "remove" {
            let target = lifecycle
                .preview(&id)
                .map_err(|e| Failure::at(e, "preflight"))?;
            if !flags.contains_key("yes")
                && !lifecycle::removal_worker().map_err(|e| Failure::at(e, "confirmation"))?
                && !lifecycle::native_removal_action()
                && !confirmation::confirm(&target, "remove")
                    .map_err(|e| Failure::at(e, "confirmation"))?
            {
                return Ok(if json {
                    Reply::Line(
                        serde_json::to_string_pretty(&envelope(
                            Some(command),
                            "cancelled",
                            json!({"id": id}),
                            Value::Null,
                            progress,
                        ))
                        .unwrap(),
                    )
                } else {
                    eprintln!("Removal cancelled; no resources changed.");
                    Reply::Raw(String::new())
                });
            }
            lifecycle.remove(&id, &target)?
        } else if command == "close" {
            lifecycle.close(&id)?
        } else {
            lifecycle.cleanup(&id, false)?
        };
        let queued = [Some("closing"), Some("removing")].contains(&result["status"].as_str());
        let data = if command != "remove" && !queued {
            lifecycle::task_data(&result)
        } else {
            result.clone()
        };
        return Ok(Reply::Line(if json {
            serde_json::to_string_pretty(&envelope(
                Some(command),
                if queued { "queued" } else { "completed" },
                data,
                Value::Null,
                progress,
            ))
            .unwrap()
        } else if command == "remove" || queued {
            lifecycle::format_action(&result, terminal::columns())
        } else {
            lifecycle::format_task(&result, terminal::columns())
        }));
    }
    if ["setup", "create", "open"].contains(&command) {
        prune::automatic(&paths);
        if command == "setup" && positionals.len() != 1 {
            return Err(
                "setup uses the current linked Git worktree and takes no task ID"
                    .to_owned()
                    .into(),
            );
        }
        if command == "create" && positionals.len() != 3 {
            return Err("create requires PROJECT TASK".to_owned().into());
        }
        if command == "open" && positionals.len() > 2 {
            return Err("Unexpected command arguments".to_owned().into());
        }
        let options = setup::Options {
            no_herdr: flags.contains_key("no-herdr"),
            wait: flags.contains_key("wait"),
            no_setup: flags.contains_key("no-setup"),
            branch: flags.get("branch").cloned(),
            skip_install: if flags.contains_key("install") {
                Some(false)
            } else if flags.contains_key("skip-install") {
                Some(true)
            } else {
                None
            },
            ..Default::default()
        };
        let engine = setup::Engine {
            paths: &paths,
            progress,
        };
        let result = match command {
            "create" => engine.create(
                &positionals[1],
                &positionals[2],
                flags.get("base").map_or("HEAD", String::as_str),
                &options,
            )?,
            "open" => {
                let id = targets::resolve_id(
                    &paths,
                    positionals.get(1).map(String::as_str),
                    flags.get("project").map(String::as_str),
                )
                .map_err(|e| Failure::target_at(e, "target"))?;
                engine.open(&id, &options)?
            }
            _ => engine.setup(&options)?,
        };
        if command != "open" {
            engine.report_setup(&result);
        }
        return output_setup(command, &result, json, progress);
    }
    if command == "hook" {
        if positionals.len() != 2 || !["tailscale", "postgres"].contains(&positionals[1].as_str()) {
            return Err("hook requires tailscale or postgres".to_owned().into());
        }
        if positionals[1] == "postgres" {
            postgres::hook(&paths)?;
        } else {
            tailscale::hook(&paths)?;
        }
        return Ok(Reply::Raw(String::new()));
    }
    if command == "migrate" {
        if positionals.len() != 1 {
            return Err("migrate takes no arguments".to_owned().into());
        }
        let report = migrate::migrate(&paths, flags.contains_key("apply"))?;
        return Ok(Reply::Line(if json {
            serde_json::to_string_pretty(&envelope(
                Some(command),
                "completed",
                report,
                Value::Null,
                progress,
            ))
            .unwrap()
        } else {
            migrate::format(&report, terminal::columns())
        }));
    }
    if command == "prune" {
        if positionals.len() != 1 {
            return Err("prune takes no arguments".to_owned().into());
        }
        let report = prune::prune(
            &paths,
            flags.get("older-than").map(String::as_str),
            flags.contains_key("apply"),
        )?;
        return Ok(Reply::Line(if json {
            serde_json::to_string_pretty(&envelope(
                Some(command),
                "completed",
                report,
                Value::Null,
                progress,
            ))
            .unwrap()
        } else {
            prune::format(&report, terminal::columns())
        }));
    }
    if command == "adopt" {
        if positionals.len() > 2 {
            return Err("adopt accepts one optional PATH".to_owned().into());
        }
        let options = adopt::Options {
            project: flags.get("project").cloned(),
            install: flags.contains_key("install"),
            environment: flags.contains_key("environment"),
            apply: flags.contains_key("apply"),
            baseline: repeated_values(args, "take-baseline", catalog),
            hooks: repeated_values(args, "hook", catalog),
        };
        let report = adopt::adopt(
            &paths,
            positionals.get(1).map(String::as_str),
            &options,
            progress,
        )?;
        return Ok(Reply::Line(if json {
            serde_json::to_string_pretty(&envelope(
                Some(command),
                "completed",
                report,
                Value::Null,
                progress,
            ))
            .unwrap()
        } else {
            adopt::format(&report, terminal::columns())
        }));
    }
    if command == "env" {
        if positionals.get(1).map(String::as_str) != Some("refresh") || positionals.len() > 3 {
            return Err("env requires refresh and an optional TARGET"
                .to_owned()
                .into());
        }
        let selector = positionals.get(2).map(String::as_str);
        let project = flags.get("project").map(String::as_str);
        if targets::adopted(&paths, selector, project)
            .map_err(|e| Failure::target_at(e, "target"))?
            .is_some()
        {
            return Err(Failure {code: "ADOPTED_EXTERNAL", message: "This worktree is tracked with external setup. Use graftr adopt PATH for explicit setup stages; automatic setup, cleanup, removal and freeze are not enabled for adopted worktrees. Existing files and manual services were preserved.".into(), phase: "target", details: Value::Null.into()});
        }
        let task = targets::resolve_managed(&paths, selector, project).map_err(|e| Failure::target_at(e, "target"))?
            .ok_or_else(|| Failure {code: "TARGET_NOT_FOUND", message: "Supply an exact managed target or run env refresh inside its managed checkout; no Herdr workspace is inferred.".into(), phase: "target", details: Value::Null.into()})?;
        let report = envrefresh::refresh(
            &paths,
            task.value()["id"].as_str().unwrap(),
            flags.contains_key("apply"),
            &repeated_values(args, "take-baseline", catalog),
            progress,
        )?;
        return Ok(Reply::Line(if json {
            serde_json::to_string_pretty(&envelope(
                Some(command),
                "completed",
                report,
                Value::Null,
                progress,
            ))
            .unwrap()
        } else {
            envrefresh::format(&report, terminal::columns())
        }));
    }
    if ["health", "label"].contains(&command) {
        let id = targets::resolve_id(
            &paths,
            positionals.get(1).map(String::as_str),
            flags.get("project").map(String::as_str),
        )
        .map_err(|error| Failure::target_at(error, "target"))?;
        let report = if command == "health" {
            health::inspect(&paths, &id)?
        } else {
            labels::refresh(&paths, &id, progress)?
        };
        let failed = command == "health" && report["health"]["status"] == "unhealthy";
        let output = if json {
            serde_json::to_string_pretty(&envelope(Some(command), if failed {"failed"} else {"completed"}, report,
                if failed { json!({"code": "HEALTH_UNHEALTHY", "message": "One or more configured health checks failed", "phase": "operation", "details": null, "recovery": ["Review the configured checks and application state; this command does not start servers."]}) } else { Value::Null }, progress)).unwrap()
        } else if command == "health" {
            health::format(&report, terminal::columns())
        } else {
            labels::format(&report, terminal::columns())
        };
        return Ok(if failed {
            Reply::Unsuccessful(output)
        } else {
            Reply::Line(output)
        });
    }
    if command == "doctor" {
        let selector = positionals.get(1).map(String::as_str);
        let project = flags.get("project").map(String::as_str);
        let live = flags.contains_key("live");
        let adopted = targets::adopted(&paths, selector, project)
            .map_err(|error| Failure::target_at(error, "target"))?;
        let task = if adopted.is_none() {
            targets::resolve_managed(&paths, selector, project)
                .map_err(|error| Failure::target_at(error, "target"))?
        } else {
            None
        };
        let id = if let Some(record) = &adopted {
            record.value()["id"].as_str().unwrap().to_owned()
        } else if let Some(task) = task {
            task.value()["id"].as_str().unwrap().to_owned()
        } else if live {
            targets::resolve_id(&paths, selector, project).map_err(|_| Failure { code: "TARGET_RESOLUTION_FAILED", message: "Could not establish an owned caller workspace; supply an exact managed target. Herdr/state response contents were not logged.".into(), phase: "target", details: Value::Null.into() })?
        } else {
            return Err(Failure { code: "TARGET_NOT_FOUND", message: "No managed checkout matches the current directory; supply an exact target or use --live for owned Herdr workspace selection".into(), phase: "target", details: Value::Null.into() });
        };
        let report = if let Some(record) = adopted {
            doctor::diagnose_adoption(&paths, &record, live)?
        } else {
            doctor::diagnose(&paths, &id, live)?
        };
        let ok = report["status"] == "ok";
        let output = if json {
            let error = if ok {
                Value::Null
            } else {
                json!({"code": if report["status"] == "attention" { "DOCTOR_ATTENTION" } else { "DOCTOR_INCOMPLETE" }, "message": "Diagnosis found issues or incomplete evidence; review the report's findings and conditional next steps.", "phase": "operation", "details": null,
                "recovery": ["Review data.findings and their conditional next steps. Unknown or unchecked evidence is not proof that resources are absent.", "Diagnosis changes nothing. Preserve records/leases and review pending command effects before any deliberate recovery action."]})
            };
            serde_json::to_string_pretty(&envelope(
                Some(command),
                if ok { "completed" } else { "failed" },
                report,
                error,
                progress,
            ))
            .unwrap()
        } else {
            doctor::format(&report, terminal::columns())
        };
        return Ok(if ok {
            Reply::Line(output)
        } else {
            Reply::Unsuccessful(output)
        });
    }
    if command == "list" {
        let data = inventory::list(&paths, &positionals[1])?;
        return Ok(Reply::Line(if json {
            serde_json::to_string_pretty(&envelope(
                Some(command),
                "completed",
                data,
                Value::Null,
                progress,
            ))
            .unwrap()
        } else {
            inventory::format(&data, terminal::columns())
        }));
    }
    if command == "logs" {
        let selector = positionals.get(1).map(String::as_str);
        let project = flags.get("project").map(String::as_str);
        let adopted = targets::adopted(&paths, selector, project)
            .map_err(|error| Failure::target_at(error, "arguments"))?;
        let (id, phase) = if let Some(record) = adopted {
            (
                record.value()["id"].as_str().unwrap().to_owned(),
                "arguments",
            )
        } else {
            (
                targets::resolve_id(&paths, selector, project)
                    .map_err(|error| Failure::target_at(error, "target"))?,
                "operation",
            )
        };
        let entries = logs::list(state, &id).map_err(|error| Failure::at(error, phase))?;
        if flags.contains_key("latest") {
            let latest = entries.first().ok_or_else(|| Failure {
                message: "This task has no installer or hook run logs yet".into(),
                code: "TARGET_NOT_FOUND",
                phase,
                details: Value::Null.into(),
            })?;
            return Ok(Reply::Raw(
                logs::latest(std::path::Path::new(latest["path"].as_str().unwrap()))
                    .map_err(|error| Failure::at(error, phase))?,
            ));
        }
        return Ok(Reply::Line(if json {
            serde_json::to_string_pretty(&envelope(
                Some(command),
                "completed",
                json!({"id": id, "logs": entries}),
                Value::Null,
                progress,
            ))
            .unwrap()
        } else {
            terminal::wrap(
                &logs::format(&id, &entries, terminal::columns()),
                terminal::columns(),
            )
        }));
    }
    if positionals.len() > if command == "frozen" { 2 } else { 1 } {
        if command == "frozen" {
            return Err("frozen accepts an optional PROJECT".to_owned().into());
        }
        return Err("Unexpected command arguments".to_owned().into());
    }
    if command == "frozen" {
        let records: Vec<_> = records::frozen(state)
            .map_err(|error| Failure {
                message: error.message,
                phase: "arguments",
                code: if error.code == "OPERATION_FAILED" {
                    "INVALID_ARGUMENTS"
                } else {
                    error.code
                },
                details: Value::Null.into(),
            })?
            .into_iter()
            .filter(|record| {
                record.reserves()
                    && positionals.get(1).is_none_or(|project| {
                        record.value()["id"]
                            .as_str()
                            .unwrap()
                            .starts_with(&format!("{project}--"))
                    })
            })
            .collect();
        return Ok(Reply::Line(if json {
            serde_json::to_string_pretty(&envelope(
                Some(command),
                "completed",
                json!({"frozen": records.iter().map(records::Frozen::public).collect::<Vec<_>>()}),
                Value::Null,
                progress,
            ))
            .unwrap()
        } else {
            terminal::wrap(
                &status::frozen(&records, terminal::columns()),
                terminal::columns(),
            )
        }));
    }
    if command == "status" {
        let tasks = records::tasks(state)?;
        let setups = records::setups(state)?;
        let frozen = records::frozen(state)?;
        let adopted = records::adoptions(state)?;
        return Ok(Reply::Line(if json {
            let retained = history::read(state).map_err(|message| Failure {
                message,
                phase: "operation",
                code: "OPERATION_FAILED",
                details: Value::Null.into(),
            })?;
            serde_json::to_string_pretty(&envelope(
                Some(command),
                "completed",
                status::data(&tasks, &setups, &adopted, &frozen, &retained),
                Value::Null,
                progress,
            ))
            .unwrap()
        } else {
            terminal::wrap(
                &status::format(&tasks, &setups, &adopted, &frozen, terminal::columns()),
                terminal::columns(),
            )
        }));
    }
    let retained = history::read(state).map_err(|message| Failure {
        message,
        phase: "operation",
        code: "OPERATION_FAILED",
        details: Value::Null.into(),
    })?;
    if json {
        Ok(Reply::Line(
            serde_json::to_string_pretty(&envelope(
                Some(command),
                "completed",
                json!({"retained": retained}),
                Value::Null,
                progress,
            ))
            .unwrap(),
        ))
    } else {
        // Match the human print path, including its final width pass.
        Ok(Reply::Line(terminal::wrap(
            &history::format(&retained, terminal::columns()),
            terminal::columns(),
        )))
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args_os()
        .skip(1)
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    let catalog = Catalog::load();
    if args.first().map(String::as_str) == Some("_complete") {
        let offset = if args.get(1).map(String::as_str) == Some("--") {
            2
        } else {
            1
        };
        let candidates = paths::defaults()
            .and_then(|paths| completion::complete(&paths, &args[offset..], &catalog))
            .unwrap_or_default();
        if !candidates.is_empty() {
            let _ = writeln!(io::stdout().lock(), "{}", candidates.join("\n"));
        }
        return ExitCode::SUCCESS;
    }
    let before_separator = &args[..args.iter().position(|s| s == "--").unwrap_or(args.len())];
    let json = before_separator
        .iter()
        .any(|s| s == "--json" || s.starts_with("--json="));
    let command = parse(&args, &catalog, false)
        .ok()
        .and_then(|(words, _)| words.first().cloned());
    let progress = progress::Progress::default();
    let (result, failed) = match execute(&args, &catalog, json, &progress) {
        Ok(output) => {
            let failed = matches!(output, Reply::Unsuccessful(_));
            (output, failed)
        }
        Err(error) => {
            let hint = command
                .as_deref()
                .filter(|c| catalog.is_command(c))
                .map(|c| format!("graftr {c} --help"))
                .unwrap_or_else(|| "graftr --help".into());
            if !json {
                eprintln!("graftr: {}", error.message);
                if args.first().map(String::as_str) != Some("_event") {
                    eprintln!("Run '{hint}' for help.");
                }
                return ExitCode::FAILURE;
            }
            let recovery = if error.phase == "arguments" {
                vec![
                    if command.as_deref().is_some_and(|c| catalog.is_command(c)) {
                        format!(
                            "Read graftr {} --help and correct the invocation.",
                            command.as_deref().unwrap()
                        )
                    } else {
                        "Read graftr --help and choose a command.".into()
                    },
                ]
            } else if error.phase == "state" {
                vec!["Review the registry/configuration error before retrying. Keep ownership records and port leases together; do not delete them to bypass a conflict.".into()]
            } else if error.phase == "target" {
                vec![if [Some("thaw"), Some("schedule")].contains(&command.as_deref()) { "Inspect graftr frozen; choose an exact frozen task ID or use --project to disambiguate." } else { "Inspect graftr list PROJECT; choose an exact target or use --project to disambiguate." }.into()]
            } else if error.phase == "confirmation" {
                vec!["Review the target and ignored files before confirming in an interactive terminal. JSON output does not bypass confirmation.".into()]
            } else if error.code == "INVALID_CREATION_CONFIG" {
                vec!["Review branchName/worktreePath and the canonical repo/worktreeRoot paths in this project's profile.".into(), "Fix the creation settings, review any recorded progress, and retry create. Explicit parsed creation settings never silently use native naming.".into()]
            } else if command.as_deref() == Some("doctor") {
                vec!["Review data.findings and their conditional next steps. Unknown or unchecked evidence is not proof that resources are absent.".into(),
                    "Diagnosis changes nothing. Preserve records/leases and review pending command effects before any deliberate recovery action.".into()]
            } else if [Some("freeze"), Some("thaw"), Some("schedule")].contains(&command.as_deref())
            {
                vec!["Inspect graftr frozen and graftr status; preserve the recovery record, branch and any active checkout/leases.".into(), "Review cleanup/setup effects and systemd timer state before explicitly retrying. Do not reset branches, force deletion or automatically replay hooks.".into()]
            } else if command.as_deref() == Some("health") {
                vec!["Review the configured checks and application state; this command does not start servers.".into()]
            } else if command.as_deref() == Some("label") {
                vec!["Inspect the selected owned workspace and its current branch; a failed/uncertain refresh may already have changed its label.".into(), "After resolving ownership or active setup, run label again deliberately. It does not rebind ownership, change environments, or run setup.".into()]
            } else if command.as_deref() == Some("env") {
                vec!["Keep the environment, records, leases, and private env-backups. Review files privately; do not publish values or automatically retry a partial apply.".into(), "A backup-environment or refresh-environment step may have completed before failure. Inspect the private backup and current .env before another preview. Existing shells/processes retain their old environment.".into()]
            } else if command.as_deref() == Some("plugin") {
                vec!["Inspect herdr plugin list and the managed manifest under XDG_DATA_HOME/graftr/plugin; a failed link may already have registered the plugin.".into(), "Resolve Herdr CLI/registration errors and explicitly retry graftr plugin install. Installation never runs startup hooks or thaws tasks.".into()]
            } else if [Some("setup"), Some("create"), Some("open")].contains(&command.as_deref()) {
                vec!["Keep the checkout. Inspect graftr status and the setup receipt/log before retrying.".into(), "Fix the reported issue, then run graftr setup --wait inside its owned Herdr workspace.".into()]
            } else if [
                Some("prune"),
                Some("migrate"),
                Some("hook"),
                Some("cleanup"),
                Some("close"),
                Some("remove"),
                Some("nuke"),
                Some("reconcile"),
            ]
            .contains(&command.as_deref())
            {
                vec!["Inspect graftr status and the recorded progress; verify Git, Herdr, and any hook-managed external state before retrying.".into(), if command.as_deref() == Some("nuke") {"Fix the reported issue and rerun nuke with terminal confirmation; never force deletion."} else {"Do not assume rollback or blindly retry after an uncertain external request. Preserve records and leases for review."}.into()]
            } else if error.code == "ADOPTED_EXTERNAL" || command.as_deref() == Some("adopt") {
                vec!["Keep the existing checkout, environment and manual services. Inspect graftr status, list and logs before explicitly retrying a selected adoption stage.".into(), "Default adoption never replays stages. Use adopt --environment to preview environment changes; built-in resource publishers and automatic lifecycle cleanup are not enabled for externally configured worktrees.".into()]
            } else if [Some("frozen"), Some("logs")].contains(&command.as_deref()) {
                vec!["Inspect graftr status and the recorded progress; verify Git, Herdr, and any hook-managed external state before any deliberate recovery action.".into(),
                    "Do not assume rollback or blindly retry after an uncertain external request. Preserve records and leases for review.".into()]
            } else {
                vec!["Review the configuration and private registry files described by the error; do not discard ownership records or leases to bypass validation.".into()]
            };
            let output = envelope(
                command.as_deref(),
                if progress.steps().as_array().unwrap().is_empty() {
                    "failed"
                } else {
                    "partial"
                },
                Value::Null,
                json!({"code": error.code, "message": error.message, "phase": error.phase, "details": error.details, "recovery": recovery}),
                &progress,
            );
            (
                Reply::Line(serde_json::to_string_pretty(&output).unwrap()),
                true,
            )
        }
    };
    let written = match result {
        Reply::Line(text) | Reply::Unsuccessful(text) => writeln!(io::stdout().lock(), "{text}"),
        Reply::Raw(text) => io::stdout().lock().write_all(text.as_bytes()),
    };
    match written {
        Ok(()) if !failed => ExitCode::SUCCESS,
        Ok(()) => ExitCode::FAILURE,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("graftr: {error}");
            ExitCode::FAILURE
        }
    }
}
