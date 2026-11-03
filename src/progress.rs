use serde_json::{Value, json};
use std::cell::RefCell;

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
        let index = self.0.borrow().len();
        self.0
            .borrow_mut()
            .push(json!({"action": action, "target": target, "status": "uncertain"}));
        let result = operation()?;
        self.0.borrow_mut()[index]["status"] = json!("completed");
        Ok(result)
    }
}
