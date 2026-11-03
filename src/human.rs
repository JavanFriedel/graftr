use crate::terminal::{display, status_color, wrap};

pub fn fields(
    title: &str,
    values: &[(&str, Option<String>)],
    message: Option<&str>,
    status: &str,
    columns: usize,
) -> String {
    let mut heading = display(title);
    let status = display(status);
    if let Some(index) = heading.rfind(&status) {
        heading.replace_range(index..index + status.len(), &status_color(&status));
    }
    let mut lines = vec![heading];
    for (label, value) in values {
        if let Some(value) = value {
            let value = display(value);
            lines.push(format!(
                "  {label}: {}",
                if ["Worktree", "Setup", "Dependencies"].contains(label) {
                    status_color(&value)
                } else {
                    value
                }
            ));
        }
    }
    if let Some(message) = message.filter(|s| !s.is_empty()) {
        lines.extend(["".into(), display(message)]);
    }
    wrap(&lines.join("\n"), columns)
}
