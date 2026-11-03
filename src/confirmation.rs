use crate::validation::*;
use serde_json::Value;
use std::{
    io::{IsTerminal, Write},
    sync::atomic::{AtomicBool, Ordering},
};

static CANCELLED: AtomicBool = AtomicBool::new(false);
extern "C" fn interrupt(_: libc::c_int) {
    CANCELLED.store(true, Ordering::Relaxed);
}
struct Interrupt(libc::sigaction);
impl Interrupt {
    fn install() -> Result<Self> {
        CANCELLED.store(false, Ordering::Relaxed);
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        let mut previous = action;
        action.sa_sigaction = interrupt as *const () as usize;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }
        ensure(
            unsafe { libc::sigaction(libc::SIGINT, &action, &mut previous) } == 0,
            "Could not establish safe terminal confirmation",
        )?;
        Ok(Self(previous))
    }
}
impl Drop for Interrupt {
    fn drop(&mut self) {
        unsafe {
            libc::sigaction(libc::SIGINT, &self.0, std::ptr::null_mut());
        }
    }
}
fn ask(prompt: &str) -> Result<Option<String>> {
    eprint!("{prompt}");
    std::io::stderr()
        .flush()
        .map_err(|_| Error::from("Could not display terminal confirmation"))?;
    let mut bytes = Vec::new();
    loop {
        if CANCELLED.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let mut item = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut item, 1, 100) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err("Could not read terminal confirmation".into());
        }
        if ready == 0 {
            continue;
        }
        let mut byte = 0_u8;
        let count = unsafe { libc::read(libc::STDIN_FILENO, (&mut byte as *mut u8).cast(), 1) };
        if count == 0 {
            return Ok(None);
        }
        if count < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err("Could not read terminal confirmation".into());
        }
        if byte == b'\n' {
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            return Ok(Some(String::from_utf8_lossy(&bytes).into_owned()));
        }
        bytes.push(byte);
    }
}
pub fn confirm(target: &Value, kind: &str) -> Result<bool> {
    let nuke = kind == "nuke";
    let freeze = kind == "freeze";
    ensure(
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
        Error {
            code: "CONFIRMATION_REQUIRED",
            message: if nuke {
                "Nuke requires interactive terminal confirmation; there is no --yes or --force bypass".into()
            } else {
                format!(
                    "{} requires terminal confirmation; use --yes only after reviewing the target and saving any ignored files",
                    if freeze { "Freeze" } else { "Removal" }
                )
            },
        },
    )?;
    let _interrupt = Interrupt::install()?;
    if nuke {
        let branch = target["branch"].as_str().unwrap();
        let prompt = format!(
            "Permanently remove worktree {} AND its local branch?\n  Folder: {}\n  Delete branch: {}\n  Branch commit: {}\n  Merged into: {} ({}){}\nDeletes ignored .env, node_modules and caches. Remote branches are untouched.\nType the exact branch name to continue: ",
            target["id"],
            target["path"],
            target["branch"],
            target["branchCommit"]
                .as_str()
                .unwrap_or("already absent; finish cleanup only"),
            serde_json::json!(
                target["mergeRef"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("refs/heads/")
                    .unwrap_or(target["mergeRef"].as_str().unwrap())
            ),
            target["mergeCommit"].as_str().unwrap(),
            if target["mergedBy"] == "content" {
                "\n  Merged by content: its commits are not in that history (e.g. a squash merge), but merging it would change no files"
            } else {
                ""
            }
        );
        if ask(&prompt)?.as_deref() != Some(branch) {
            return Ok(false);
        }
        return Ok(
            ask("Delete this worktree and branch? [y/N] ")?.is_some_and(|s| {
                ["y", "yes"].contains(&s.trim_matches(js_space).to_lowercase().as_str())
            }),
        );
    }
    let prompt = format!(
        "{} worktree {}?\n  Folder: {}\n  Recorded branch: {}\n  Current branch: {}\n  Current commit: {}\nKeeps all existing Git branches; no branch is deleted.\nRuns recorded cleanup hooks and closes its Herdr workspace/panes.\nDeletes the entire folder, including ignored .env, node_modules and caches.\n{}Continue? [y/N] ",
        if freeze { "Freeze" } else { "Remove" },
        target["id"],
        target["path"],
        target["branch"],
        if target["currentBranch"].is_null() {
            "unknown (checkout and registration already absent)".into()
        } else {
            target["currentBranch"].to_string()
        },
        target["currentCommit"].as_str().unwrap_or("not observed"),
        if freeze {
            "Runs retire hooks too; task databases or other configured resources may be deleted.\nKeeps a frozen recovery record, not a backup. Thaw regenerates the environment from current configuration.\n"
        } else {
            ""
        }
    );
    Ok(ask(&prompt)?
        .is_some_and(|s| ["y", "yes"].contains(&s.trim_matches(js_space).to_lowercase().as_str())))
}
