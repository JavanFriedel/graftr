use crate::terminal::{width, wrap};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Catalog {
    schema_version: u32,
    overview: Overview,
    footer: String,
    topics: BTreeMap<String, String>,
    pages: BTreeMap<String, Page>,
    pub cli_options: BTreeMap<String, CliOption>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliOption {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "short")]
    pub _short: Option<String>,
    #[serde(rename = "multiple")]
    pub _multiple: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Overview {
    intro: Vec<String>,
    groups: Vec<Group>,
    footer: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Group {
    heading: String,
    entries: Vec<(String, String)>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Page {
    summary: String,
    usage: String,
    arguments: Option<Vec<(String, String)>>,
    options: Vec<(String, String)>,
    examples: Vec<String>,
    notes: Option<String>,
    target_note: Option<String>,
}

impl Catalog {
    pub fn load() -> Self {
        let catalog: Self = serde_json::from_str(include_str!("../help-catalog.json"))
            .expect("Shared help catalog must be valid");
        assert_eq!(catalog.schema_version, 1);
        catalog
    }

    pub fn is_command(&self, topic: &str) -> bool {
        self.pages.contains_key(topic)
    }
    pub fn public_commands(&self) -> Vec<String> {
        self.pages.keys().cloned().chain(["help".into()]).collect()
    }
    pub fn command_flags(&self, command: &str) -> Vec<String> {
        let Some(page) = self.pages.get(command) else {
            return vec![];
        };
        page.options
            .iter()
            .filter(|(label, _)| label != "-h, --help")
            .map(|(label, _)| label.split(' ').next().unwrap().into())
            .chain(["--help".into(), "-h".into()])
            .collect()
    }
    pub fn target_command(&self, command: &str) -> bool {
        self.pages
            .get(command)
            .is_some_and(|page| page.target_note.is_some())
    }

    pub fn render(&self, topic: Option<&str>, columns: usize) -> Result<String, String> {
        let Some(topic) = topic.filter(|topic| *topic != "help") else {
            let mut sections = self.overview.intro.clone();
            let width = self
                .overview
                .groups
                .iter()
                .flat_map(|g| &g.entries)
                .map(|(name, _)| name.len())
                .max()
                .unwrap_or(0);
            for group in &self.overview.groups {
                sections.push(format!(
                    "{}:\n{}",
                    group.heading,
                    rows(&group.entries, width, columns)
                ));
            }
            sections.push(self.overview.footer.clone());
            return Ok(wrap(&sections.join("\n\n"), columns));
        };
        if let Some(page) = self.topics.get(topic) {
            return Ok(page.clone());
        }
        let page = self
            .pages
            .get(topic)
            .ok_or_else(|| format!("Unknown command: {topic}"))?;
        let width = page
            .arguments
            .iter()
            .flatten()
            .chain(&page.options)
            .map(|(name, _)| name.len())
            .max()
            .unwrap_or(0);
        let mut sections = vec![
            format!("graftr {topic} — {}", page.summary),
            format!(
                "Usage:\n  graftr {topic}{}{}",
                if page.usage.is_empty() { "" } else { " " },
                page.usage
            ),
        ];
        if let Some(arguments) = &page.arguments {
            sections.push(format!("Arguments:\n{}", rows(arguments, width, columns)));
        }
        sections.push(format!("Options:\n{}", rows(&page.options, width, columns)));
        sections.push(format!(
            "Examples:\n{}",
            page.examples
                .iter()
                .map(|example| format!("  {example}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
        if let Some(notes) = &page.notes {
            sections.push(notes.clone());
        }
        if let Some(note) = &page.target_note {
            sections.push(note.clone());
        }
        sections.push(self.footer.clone());
        Ok(sections.join("\n\n"))
    }
}

fn rows(entries: &[(String, String)], label_width: usize, columns: usize) -> String {
    let description_column = label_width + 4;
    let inline = columns >= description_column + 24;
    let indent = " ".repeat(if inline { description_column } else { 4 });
    entries
        .iter()
        .map(|(name, description)| {
            let wrapped = wrap(&format!("{indent}{description}"), columns);
            if !inline {
                return format!("{}\n{wrapped}", wrap(&format!("  {name}"), columns));
            }
            let mut lines: Vec<String> = wrapped.split('\n').map(str::to_owned).collect();
            lines[0] = format!(
                "  {name}{}  {}",
                " ".repeat(label_width - width(name)),
                &lines[0][description_column..]
            );
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_catalog_has_valid_option_kinds_and_declares_every_displayed_flag() {
        let catalog = Catalog::load();
        for option in catalog.cli_options.values() {
            assert!(["string", "boolean"].contains(&option.kind.as_str()));
        }
        for page in catalog.pages.values() {
            for (label, _) in &page.options {
                if label == "-h, --help" {
                    continue;
                }
                let flag = label.split(' ').next().unwrap().strip_prefix("--").unwrap();
                assert!(
                    catalog.cli_options.contains_key(flag),
                    "Undeclared option: {flag}"
                );
            }
        }
    }
}
