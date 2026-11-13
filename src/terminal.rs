use icu_properties::{CodePointMapData, props::EastAsianWidth};
use std::{env, io::IsTerminal};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn columns() -> usize {
    if std::io::stdout().is_terminal() {
        let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
        // stdout is a live descriptor; ioctl writes exactly a winsize on success.
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) } == 0 {
            let columns = unsafe { size.assume_init() }.ws_col;
            if columns > 0 {
                return columns.into();
            }
            // Bun exposes a zero-sized TTY as columns=0, so it falls back to 80,
            // rather than using COLUMNS as redirected output does.
            return 80;
        }
    }
    env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 1.0)
        .map(|v| v.floor() as usize)
        .unwrap_or(80)
}

pub fn heading(value: &str) -> String {
    if colors_enabled() {
        format!("\x1b[1m{value}\x1b[0m")
    } else {
        value.to_owned()
    }
}

fn colors_enabled() -> bool {
    std::io::stdout().is_terminal()
        && env::var_os("NO_COLOR").is_none()
        && env::var("TERM").as_deref() != Ok("dumb")
}

pub(crate) fn status_color(value: &str) -> String {
    if !colors_enabled() {
        return value.into();
    }
    match status_code(value) {
        Some(color) => format!("\x1b[{color}m{value}\x1b[0m"),
        None => value.into(),
    }
}

fn status_code(value: &str) -> Option<u8> {
    let normalized = value.to_lowercase();
    let code = |key: &str| match key {
        "ready" | "prepared" | "healthy" | "thawed" | "complete" | "completed" | "installed"
        | "refreshed" | "ok" | "applied" | "same" | "managed" | "setup completed" => Some(32),
        "closed" | "pending" | "preparing" | "closing" | "removing" | "freezing" | "thawing"
        | "deferred" | "cleanup pending" | "cleanup queued" | "cleanup running" | "unknown"
        | "unmanaged" | "external" | "not recorded" | "needs review" | "preview" | "missing"
        | "different" | "add" | "replace" | "delete" | "not recognized" | "setting up"
        | "setup pending" | "setup unknown" | "setup needs review" | "setup unmanaged"
        | "retirement queued" | "retirement running" => Some(33),
        "frozen" => Some(34),
        "adopted" => Some(36),
        "failed" | "incomplete" | "unhealthy" | "attention" | "blocked" | "cleanup failed"
        | "retirement failed" | "setup incomplete" => Some(31),
        "cleaned" | "removed" | "nuked" | "skipped" | "unchanged" | "not-configured"
        | "not-checked" | "local-only" | "preserve" | "kept" | "deleted" | "resources cleaned"
        | "setup skipped" => Some(90),
        _ => None,
    };
    normalized
        .split(" — ")
        .filter_map(|label| {
            code(label)
                .or_else(|| code(label.split(" (").next().unwrap_or(label)))
                .or_else(|| code(label.split(' ').next().unwrap_or("")))
        })
        .min_by_key(|color| match color {
            31 => 0,
            33 => 1,
            _ => 2,
        })
}

fn sgr_len(text: &str) -> Option<usize> {
    let rest = text.strip_prefix("\x1b[")?;
    let count = rest
        .bytes()
        .take_while(|b| b.is_ascii_digit() || *b == b';')
        .count();
    (rest.as_bytes().get(count) == Some(&b'm')).then_some(count + 3)
}

fn units(text: &str) -> Vec<(&str, usize, bool)> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < text.len() {
        let rest = &text[offset..];
        if let Some(length) = sgr_len(rest) {
            result.push((&rest[..length], 0, true));
            offset += length;
        } else {
            let segment = rest.graphemes(true).next().unwrap();
            // Bun does not canonically fold ambiguous scalars (é is wide, e + ◌́ is not).
            let emoji_sequence = segment.chars().any(|c| matches!(c, '\u{200d}' | '\u{fe0e}' | '\u{fe0f}' | '\u{20e3}' | '\u{1f3fb}'..='\u{1f3ff}' | '\u{1f1e6}'..='\u{1f1ff}'));
            let width = if emoji_sequence {
                segment.width_cjk()
            } else {
                segment
                    .chars()
                    .map(|c| {
                        let width = c.width_cjk().unwrap_or(0);
                        if width > 0
                            && CodePointMapData::<EastAsianWidth>::new().get(c)
                                == EastAsianWidth::Ambiguous
                        {
                            2
                        } else {
                            width
                        }
                    })
                    .sum()
            };
            result.push((segment, width, false));
            offset += segment.len();
        }
    }
    result
}

pub fn width(text: &str) -> usize {
    if text.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return text.len();
    }
    units(text)
        .iter()
        .filter(|(_, _, style)| !style)
        .map(|(_, width, _)| *width)
        .sum()
}

pub fn wrap(text: &str, columns: usize) -> String {
    let columns = columns.max(1);
    let mut output = Vec::new();
    for line in text.split('\n') {
        if width(line) <= columns {
            output.push(line.to_owned());
            continue;
        }
        let indent = " ".repeat(
            line.bytes()
                .take_while(|b| *b == b' ')
                .count()
                .min(columns.saturating_sub(4)),
        );
        let items = units(line.trim_start());
        let mut start = 0;
        let mut active_style = String::new();
        while start < items.len() {
            let (mut end, mut used, mut space) = (start, indent.len(), None);
            while end < items.len() && (used + items[end].1 <= columns || end == start) {
                if items[end].0 == " " {
                    space = Some(end);
                }
                used += items[end].1;
                end += 1;
            }
            if end < items.len()
                && let Some(space) = space.filter(|index| *index > start)
            {
                end = space;
            }
            let before = active_style.clone();
            let chunk = &items[start..end];
            for (segment, _, style) in chunk {
                if *style {
                    if *segment == "\x1b[0m" || *segment == "\x1b[m" {
                        active_style.clear();
                    } else {
                        active_style.push_str(segment);
                    }
                }
            }
            let content = chunk.iter().map(|(s, _, _)| *s).collect::<String>();
            output.push(format!(
                "{indent}{before}{}{}",
                content.trim_end(),
                if active_style.is_empty() {
                    ""
                } else {
                    "\x1b[0m"
                }
            ));
            start = end;
            while start < items.len() && items[start].0 == " " {
                start += 1;
            }
        }
    }
    output.join("\n")
}

pub fn display(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}' | '\u{2028}' | '\u{2029}' => {
                format!("\\u{:04x}", u32::from(c))
            }
            _ => c.to_string(),
        })
        .collect()
}

pub struct Row {
    pub cells: Vec<String>,
    pub details: Vec<String>,
}

pub fn table(headers: &[&str], rows: &[Vec<String>], columns: usize) -> String {
    let rows: Vec<_> = rows
        .iter()
        .map(|row| Row {
            cells: row.clone(),
            details: vec![],
        })
        .collect();
    format_table(headers, &rows, columns, 2, &[])
}

pub fn format_table(
    headers: &[&str],
    rows: &[Row],
    columns: usize,
    indent: usize,
    wrap_columns: &[&str],
) -> String {
    let rows: Vec<Row> = rows
        .iter()
        .map(|row| Row {
            cells: row.cells.iter().map(|cell| display(cell)).collect(),
            details: row.details.iter().map(|detail| display(detail)).collect(),
        })
        .collect();
    let padding = " ".repeat(indent);
    let natural_widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(i, header)| {
            rows.iter()
                .map(|row| width(&row.cells[i]))
                .chain([width(header)])
                .max()
                .unwrap()
        })
        .collect();
    let required =
        |sizes: &[usize]| indent + sizes.iter().sum::<usize>() + sizes.len().saturating_sub(1) * 2;
    let mut widths = natural_widths.clone();
    let minimum: Vec<_> = natural_widths
        .iter()
        .enumerate()
        .map(|(i, size)| {
            if wrap_columns.contains(&headers[i]) {
                width(headers[i]).max((*size).min(12))
            } else {
                *size
            }
        })
        .collect();
    if required(&widths) > columns && required(&minimum) <= columns {
        let fitted = |limit| {
            natural_widths
                .iter()
                .enumerate()
                .map(|(i, size)| minimum[i].max((*size).min(limit)))
                .collect::<Vec<_>>()
        };
        let (mut low, mut high) = (0, *natural_widths.iter().max().unwrap());
        while low < high {
            let middle = (low + high).div_ceil(2);
            if required(&fitted(middle)) <= columns {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        widths = fitted(low);
        for i in 0..widths.len() {
            if required(&widths) >= columns {
                break;
            }
            if widths[i] < natural_widths[i] {
                widths[i] += 1;
            }
        }
    }
    let mut visible = headers.len();
    while visible > 1 && required(&widths[..visible]) > columns {
        visible -= 1;
    }
    let styled = |value: &str, index: usize| {
        if ["STATE", "SETUP", "STATUS", "RESULT"].contains(&headers[index]) {
            status_color(value)
        } else {
            value.into()
        }
    };
    let lines = |cells: &[String], status: bool| {
        let wrapped: Vec<Vec<String>> = cells[..visible]
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                wrap(
                    &if status {
                        styled(cell, i)
                    } else {
                        cell.clone()
                    },
                    widths[i],
                )
                .split('\n')
                .map(str::to_owned)
                .collect()
            })
            .collect();
        (0..wrapped.iter().map(Vec::len).max().unwrap_or(0))
            .map(|line| {
                let fields: Vec<String> = wrapped
                    .iter()
                    .enumerate()
                    .map(|(i, cell)| {
                        let value = cell.get(line).map(String::as_str).unwrap_or("");
                        if i + 1 == visible {
                            value.to_owned()
                        } else {
                            format!("{value}{}", " ".repeat(widths[i] - width(value)))
                        }
                    })
                    .collect();
                format!("{padding}{}", fields.join("  "))
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
    };
    let label = |header: &str| {
        let value = header.to_lowercase().replace("(utc)", "(UTC)");
        format!("{}{}", value[..1].to_uppercase(), &value[1..])
    };
    let mut output = if visible > 1 {
        lines(
            &headers.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            false,
        )
    } else {
        vec![]
    };
    for (i, row) in rows.iter().enumerate() {
        if visible == 1 {
            if i > 0 {
                output.push(String::new());
            }
            output.push(format!(
                "{padding}{}: {}",
                label(headers[0]),
                styled(&row.cells[0], 0)
            ));
        } else {
            output.extend(lines(&row.cells, true));
        }
        for (field, header) in headers.iter().enumerate().skip(visible) {
            output.push(format!(
                "{padding}  {}: {}",
                label(header),
                styled(&row.cells[field], field)
            ));
        }
        output.extend(
            row.details
                .iter()
                .map(|detail| format!("{padding}  {detail}")),
        );
    }
    wrap(&output.join("\n"), columns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combined_status_colors_prioritize_problems_over_lifecycle() {
        for (label, color) in [
            ("ready", 32),
            ("setting up (dependencies)", 33),
            ("setup unknown", 33),
            ("setup needs review", 33),
            ("setup skipped", 90),
            ("setup incomplete (dependencies)", 31),
            ("closed — setup incomplete (dependencies)", 31),
            ("closed — cleanup failed", 31),
            ("setup unknown — cleanup failed", 31),
            ("cleanup failed — retirement running", 31),
            ("adopted — needs review", 33),
            ("adopted — external setup", 33),
            ("resources cleaned", 90),
            ("frozen", 34),
        ] {
            assert_eq!(status_code(label), Some(color), "{label}");
        }
    }

    #[test]
    fn wrapping_preserves_graphemes_indentation_and_styles() {
        assert_eq!(width("é"), 2);
        assert_eq!(width("e\u{301}"), 1);
        let emoji = "👩‍👩‍👧‍👦";
        let text = wrap(
            &format!("    {}", format!("日本語 {emoji} e\u{301} ").repeat(12)),
            28,
        );
        assert_eq!(text.matches(emoji).count(), 12);
        assert!(
            text.lines()
                .all(|line| line.starts_with("    ") && width(line) <= 28)
        );
        let colored = wrap("\x1b[1mHistory with a long title\x1b[0m", 16);
        assert!(
            colored
                .lines()
                .all(|line| line.starts_with("\x1b[1m") && line.ends_with("\x1b[0m"))
        );
    }

    #[test]
    fn tables_stack_and_escape_without_losing_values() {
        let rows = vec![vec!["name\nspoof".into(), "\x1b[2J".into()]];
        let text = table(&["CATEGORY", "STATE"], &rows, 20);
        assert!(!text.contains('\x1b'));
        assert!(
            text.replace(char::is_whitespace, "")
                .contains("name\\u000aspoof")
        );
        assert!(text.contains("State:"));
        assert!(text.lines().all(|line| width(line) <= 20));
    }
}
