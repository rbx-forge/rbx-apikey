//! The checklist `doctor` prints.
//!
//! Two rules shape the type, both from "one actionable line per failure":
//!
//! 1. A failing line carries its own remedy. An `action` is required on
//!    [`Status::Fail`] by construction ([`Line::fail`] takes one), so a check
//!    cannot report a problem and leave the reader to work out what to do.
//! 2. Nothing is reported as passing that was not actually checked. A check
//!    that could not run is [`Status::Skipped`] and says why: the difference
//!    between "your scopes are fine" and "your scopes were never looked at" is
//!    the whole value of the command.

use colored::Colorize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Checked, and fine.
    Ok,
    /// Checked, fine, but worth knowing.
    Warn,
    /// Checked, and broken.
    Fail,
    /// Not a check: a fact the reader needs to interpret the rest.
    Info,
    /// Could not be checked. Never a pass.
    Skipped,
}

impl Status {
    fn glyph(self) -> colored::ColoredString {
        match self {
            Status::Ok => "✓".green(),
            Status::Warn => "!".yellow(),
            Status::Fail => "✗".red(),
            Status::Info => "·".dimmed(),
            Status::Skipped => "-".dimmed(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Line {
    pub status: Status,
    /// What was checked, in a couple of words.
    pub label: String,
    /// What was found.
    pub detail: String,
    /// What to do about it. Mandatory on a failure, and the reason `fail`
    /// takes it as an argument rather than leaving it to a builder method
    /// somebody can forget.
    pub action: Option<String>,
}

impl Line {
    pub fn ok(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::bare(Status::Ok, label, detail)
    }

    pub fn info(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::bare(Status::Info, label, detail)
    }

    pub fn warn(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::bare(Status::Warn, label, detail)
    }

    pub fn fail(
        label: impl Into<String>,
        detail: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Line {
            status: Status::Fail,
            label: label.into(),
            detail: detail.into(),
            action: Some(action.into()),
        }
    }

    /// A check that could not run. `why` doubles as the action: what would have
    /// to be true for it to run next time.
    pub fn skipped(label: impl Into<String>, why: impl Into<String>) -> Self {
        let why = why.into();
        Line {
            status: Status::Skipped,
            label: label.into(),
            detail: "not checked".to_string(),
            action: Some(why),
        }
    }

    /// Attach an action to a non-failing line. For a `Warn` that has something
    /// worth suggesting without being broken.
    pub fn with_action(mut self, action: impl Into<String>) -> Self {
        self.action = Some(action.into());
        self
    }

    fn bare(status: Status, label: impl Into<String>, detail: impl Into<String>) -> Self {
        Line {
            status,
            label: label.into(),
            detail: detail.into(),
            action: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct Section {
    pub title: String,
    pub lines: Vec<Line>,
}

impl Section {
    pub fn new(title: impl Into<String>) -> Self {
        Section {
            title: title.into(),
            lines: Vec::new(),
        }
    }

    pub fn push(&mut self, line: Line) {
        self.lines.push(line);
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub sections: Vec<Section>,
}

impl Report {
    pub fn push(&mut self, section: Section) {
        self.sections.push(section);
    }

    pub fn failures(&self) -> usize {
        self.lines().filter(|l| l.status == Status::Fail).count()
    }

    pub fn skipped(&self) -> usize {
        self.lines().filter(|l| l.status == Status::Skipped).count()
    }

    fn lines(&self) -> impl Iterator<Item = &Line> {
        self.sections.iter().flat_map(|s| s.lines.iter())
    }

    /// Widest label, so the detail column lines up within a section. Computed
    /// per section rather than per report: one long label in the last section
    /// should not push every earlier line across the terminal.
    fn label_width(section: &Section) -> usize {
        section
            .lines
            .iter()
            .map(|l| l.label.chars().count())
            .max()
            .unwrap_or(0)
    }

    pub fn print(&self) {
        for section in &self.sections {
            println!();
            println!("{}", section.title.cyan().bold());
            let width = Self::label_width(section);
            for line in &section.lines {
                println!(
                    "  {} {:<width$}  {}",
                    line.status.glyph(),
                    line.label,
                    line.detail,
                    width = width
                );
                if let Some(action) = &line.action {
                    // Indented under the finding it belongs to: an action
                    // floating at the same level as the checks reads like
                    // another check.
                    for (i, text) in wrapped(action).into_iter().enumerate() {
                        let lead = if i == 0 { "→" } else { " " };
                        println!("      {} {}", lead.dimmed(), text.dimmed());
                    }
                }
            }
        }
        println!();
        self.print_summary();
    }

    fn print_summary(&self) {
        let failures = self.failures();
        let skipped = self.skipped();
        let summary = match (failures, skipped) {
            (0, 0) => "Everything checked out.".green().bold(),
            (0, s) => format!("Nothing broken. {s} check(s) could not run.")
                .yellow()
                .bold(),
            (f, 0) => format!("{f} problem(s) found.").red().bold(),
            (f, s) => format!("{f} problem(s) found, {s} check(s) could not run.")
                .red()
                .bold(),
        };
        println!("{summary}");
    }
}

/// Break an action into ~86-column lines on word boundaries.
///
/// Actions are full sentences and some name a command, so a naive hard wrap in
/// the terminal splits them mid-token and makes the command unusable to copy.
fn wrapped(text: &str) -> Vec<String> {
    const WIDTH: usize = 86;
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > WIDTH {
            out.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_always_carries_an_action() {
        let line = Line::fail("key", "expired", "regenerate it");
        assert!(line.action.is_some());
    }

    #[test]
    fn a_skipped_check_is_not_counted_as_a_failure() {
        let mut section = Section::new("s");
        section.push(Line::skipped("scopes", "needs a cookie"));
        let mut report = Report::default();
        report.push(section);

        assert_eq!(report.failures(), 0);
        assert_eq!(report.skipped(), 1);
    }

    #[test]
    fn failures_are_counted_across_sections() {
        let mut report = Report::default();
        for _ in 0..2 {
            let mut section = Section::new("s");
            section.push(Line::fail("a", "b", "c"));
            section.push(Line::ok("d", "e"));
            report.push(section);
        }
        assert_eq!(report.failures(), 2);
    }

    #[test]
    fn wrapping_never_splits_a_word() {
        let action = format!("run {} now", "x".repeat(120));
        let lines = wrapped(&action);
        assert!(lines.iter().any(|l| l.contains(&"x".repeat(120))));
    }

    #[test]
    fn short_actions_stay_on_one_line() {
        assert_eq!(wrapped("set RBX_API_KEY").len(), 1);
    }

    #[test]
    fn an_empty_action_still_yields_one_line() {
        assert_eq!(wrapped(""), vec![String::new()]);
    }
}
