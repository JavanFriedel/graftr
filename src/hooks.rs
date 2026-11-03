use crate::{checkout::string, environment, logs::RunLog, system, validation::*};
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub struct HookFailure {
    pub result: Value,
    pub error: Error,
}
impl HookFailure {
    pub fn into_error(self) -> Error {
        debug_assert_eq!(self.result["status"], "failed");
        self.error
    }
}
pub fn run(
    hook: &Value,
    phase: &str,
    task: &Value,
    dependencies: &str,
    run_id: &str,
    cwd: &Path,
    mut log: RunLog,
) -> std::result::Result<Value, HookFailure> {
    let started = Instant::now();
    let mut env = task["overrides"]
        .as_object()
        .expect("validated hook environment")
        .clone();
    env.retain(|key, _| {
        ![
            "GRAFTR_LOCAL_PORT",
            "GRAFTR_HTTPS_PORT",
            "GRAFTR_URL",
            "GRAFTR_CLEANUP_RUN_ID",
            "GRAFTR_RETIRE_RUN_ID",
        ]
        .contains(&key.as_str())
            && !key.starts_with("GRAFTR_PORT_")
    });
    for (key, source) in [
        ("GRAFTR_PROJECT", "project"),
        ("GRAFTR_TASK_ID", "id"),
        ("GRAFTR_WORKTREE_PATH", "path"),
        ("GRAFTR_BRANCH", "branch"),
    ] {
        env.insert(key.into(), task[source].clone());
    }
    for (key, source) in [
        ("GRAFTR_LOCAL_PORT", "localPort"),
        ("GRAFTR_HTTPS_PORT", "httpsPort"),
    ] {
        if let Some(port) = task[source].as_f64() {
            env.insert(key.into(), json!(port.to_string()));
        }
    }
    if let Some(url) = task["url"].as_str() {
        env.insert("GRAFTR_URL".into(), json!(url));
    }
    if let Some(ports) = task["ports"].as_object() {
        for (name, port) in ports {
            env.insert(
                format!("GRAFTR_PORT_{}", name.to_uppercase().replace('-', "_")),
                json!(port.as_f64().unwrap().to_string()),
            );
        }
    }
    for (key, source) in [
        ("GRAFTR_CLEANUP_RUN_ID", "cleanup"),
        ("GRAFTR_RETIRE_RUN_ID", "retire"),
    ] {
        if let Some(run) = task[source]["runId"].as_str() {
            env.insert(key.into(), json!(run));
        }
    }
    for (key, value) in [
        ("GRAFTR_TASK_SLUG", environment::task_slug(task)),
        ("GRAFTR_HOOK_PHASE", phase.into()),
        ("GRAFTR_HOOK_NAME", string(hook, "name").into()),
        ("GRAFTR_DEPENDENCIES", dependencies.into()),
        ("GRAFTR_SETUP_RUN_ID", run_id.into()),
    ] {
        env.insert(key.into(), json!(value));
    }
    let args: Vec<_> = hook["command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let timeout = hook["timeoutMs"].as_f64().unwrap() as u64;
    let execution = system::run_logged(
        &args,
        cwd,
        &env,
        Duration::from_millis(timeout),
        &log.file,
        true,
    );
    let mut result = json!({"phase": phase, "name": hook["name"], "status": "succeeded", "log": log.path.to_string_lossy()});
    let (reason, summary) = match execution {
        Err(_) => (
            Some("command-unavailable"),
            "command could not start".into(),
        ),
        Ok(execution) => {
            if let Some(code) = execution.exit_code {
                result["exitCode"] = json!(code);
            }
            if execution.timed_out {
                (Some("timeout"), format!("timed out after {timeout} ms"))
            } else if execution.exit_code != Some(0) {
                (
                    Some("command-failed"),
                    format!(
                        "exit {}",
                        execution
                            .exit_code
                            .map_or("unknown".into(), |c| c.to_string())
                    ),
                )
            } else {
                (None, "exit 0".into())
            }
        }
    };
    result["elapsedMs"] = json!(started.elapsed().as_secs_f64() * 1000.0);
    if let Err(error) = log.finish(&summary) {
        result["status"] = json!("failed");
        result["reason"] = json!("context-changed");
        return Err(HookFailure { result, error });
    }
    if let Some(reason) = reason {
        result["status"] = json!("failed");
        result["reason"] = json!(reason);
        let label = match phase {
            "cleanup" => "Cleanup",
            "retire" => "Retire",
            _ => "Setup",
        };
        let exit = result["exitCode"]
            .as_i64()
            .map_or(String::new(), |code| format!(", exit {code}"));
        let message = format!(
            "{label} hook {phase}/{} failed ({reason}{exit}); its output is in the private log {} (graftr logs --latest)",
            string(hook, "name"),
            log.path.display()
        );
        Err(HookFailure {
            result,
            error: message.into(),
        })
    } else {
        Ok(result)
    }
}
pub fn install(
    command: &Value,
    cwd: &Path,
    overrides: &Value,
    mut log: RunLog,
    retry: &str,
) -> Result<()> {
    let args: Vec<_> = command
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let execution = system::run_logged(
        &args,
        cwd,
        overrides.as_object().unwrap(),
        Duration::from_secs(600),
        &log.file,
        false,
    );
    let Ok(execution) = execution else {
        log.finish("installer could not start")?;
        return Err("Configured dependency installer could not start; check its executable on the command's PATH".into());
    };
    let code = execution
        .exit_code
        .unwrap_or(128 + execution.signal.unwrap_or(0));
    log.finish(&format!("exit {code}"))?;
    ensure(
        code == 0 && !execution.timed_out,
        format!(
            "Dependency install failed (exit {code}); its output is in the private log {} (graftr logs --latest), then explicitly retry {retry}",
            log.path.display()
        ),
    )
}
