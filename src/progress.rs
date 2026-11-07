use serde_json::{Value, json};
use std::{cell::RefCell, time::Instant};

#[derive(Default)]
pub struct Progress(RefCell<Vec<Value>>);
impl Progress {
    pub fn steps(&self) -> Value {
        json!(*self.0.borrow())
    }
    pub fn run<T, E>(
        &self,
        action: &str,
        target: &str,
        operation: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        self.record(action, target, false, operation)
    }
    pub fn run_timed<T, E>(
        &self,
        action: &str,
        target: &str,
        operation: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        self.record(action, target, true, operation)
    }
    fn record<T, E>(
        &self,
        action: &str,
        target: &str,
        timed: bool,
        operation: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        let index = self.0.borrow().len();
        self.0
            .borrow_mut()
            .push(json!({"action": action, "target": target, "status": "uncertain"}));
        let started = timed.then(Instant::now);
        let result = operation();
        let mut steps = self.0.borrow_mut();
        if result.is_ok() {
            steps[index]["status"] = json!("completed");
        }
        if let Some(started) = started {
            steps[index]["elapsedMs"] = json!(started.elapsed().as_secs_f64() * 1000.0);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untimed_steps_keep_their_existing_shape_and_error_status() {
        let progress = Progress::default();
        progress.run("done", "task", || Ok::<_, &str>(())).unwrap();
        assert_eq!(
            progress.run("failed", "task", || Err::<(), _>("private error")),
            Err("private error")
        );
        assert_eq!(
            progress.steps(),
            json!([
                {"action":"done","target":"task","status":"completed"},
                {"action":"failed","target":"task","status":"uncertain"}
            ])
        );
    }

    #[test]
    fn timings_cover_nested_steps_and_errors_without_recording_private_errors() {
        let progress = Progress::default();
        progress
            .run_timed("outer", "task", || {
                progress.run_timed("inner", "task", || Ok::<_, &str>(()))
            })
            .unwrap();
        assert_eq!(
            progress.run_timed("failed", "task", || Err::<(), _>("private error")),
            Err("private error")
        );
        let steps = progress.steps();
        assert!(steps[0]["elapsedMs"].as_f64().unwrap() >= steps[1]["elapsedMs"].as_f64().unwrap());
        for step in steps.as_array().unwrap() {
            assert!(step["elapsedMs"].as_f64().unwrap() >= 0.0);
        }
        assert_eq!(steps[2]["status"], "uncertain");
        assert!(!steps.to_string().contains("private error"));
    }
}
