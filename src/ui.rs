use std::{
    io::IsTerminal,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;

pub fn worktree_table(
    repository: &str,
    rows: &[(Option<u64>, String, String)],
    detail_heading: &str,
) {
    let links = std::io::stdout().is_terminal()
        && !std::env::var("TERM").is_ok_and(|term| term == "dumb")
        && repository
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'));
    let issue = |value: Option<u64>| value.map_or_else(|| "-".to_owned(), |n| format!("#{n}"));
    let issue_width = rows
        .iter()
        .map(|row| issue(row.0).len())
        .max()
        .unwrap_or(5)
        .max(5);
    let terminal_width = console::Term::stdout()
        .size_checked()
        .map(|(_, width)| usize::from(width))
        .or_else(|| std::env::var("COLUMNS").ok()?.parse().ok())
        .unwrap_or(140);
    let available = terminal_width.saturating_sub(issue_width + 6);
    let branch_width = rows
        .iter()
        .map(|row| console::measure_text_width(&row.1))
        .max()
        .unwrap_or(6)
        .max(6)
        .min(available / 2);
    let detail_width = rows
        .iter()
        .map(|row| console::measure_text_width(&row.2))
        .max()
        .unwrap_or(0)
        .max(detail_heading.len())
        .min(available.saturating_sub(branch_width));
    let cell = |value: &str, width: usize| {
        let value = value.replace(['\n', '\r', '\t'], " ");
        let text = console::truncate_str(&value, width, "…");
        format!(
            "{}{}",
            text,
            " ".repeat(width.saturating_sub(console::measure_text_width(&text)))
        )
    };
    println!(
        "{}",
        stdout_style(
            &format!(
                "{} | {} | {}",
                cell("ISSUE", issue_width),
                cell("BRANCH", branch_width),
                detail_heading
            ),
            1
        )
    );
    println!(
        "{}",
        stdout_style(
            &format!(
                "{}-+-{}-+-{}",
                "-".repeat(issue_width),
                "-".repeat(branch_width),
                "-".repeat(detail_width)
            ),
            2
        )
    );
    for (number, branch, detail) in rows {
        let label = issue(*number);
        let padding = " ".repeat(issue_width.saturating_sub(label.len()));
        let issue_cell = match number {
            Some(number) if links => {
                let label = stdout_style(&stdout_style(&label, 34), 4);
                format!(
                    "\x1b]8;;https://github.com/{repository}/issues/{number}\x1b\\{label}\x1b]8;;\x1b\\{padding}"
                )
            }
            _ => format!("{label}{padding}"),
        };
        let detail =
            if detail_heading == "PATH" && console::measure_text_width(detail) > detail_width {
                detail
                    .rsplit_once('/')
                    .map_or_else(|| detail.clone(), |(_, name)| format!("…/{name}"))
            } else {
                detail.clone()
            };
        println!(
            "{} | {} | {}",
            issue_cell,
            cell(branch, branch_width),
            console::truncate_str(&detail.replace(['\n', '\r', '\t'], " "), detail_width, "…")
        );
    }
}

pub fn display_path(path: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(relative) = path.strip_prefix(home) {
            return std::path::Path::new("~")
                .join(relative)
                .display()
                .to_string();
        }
    }
    path.display().to_string()
}

pub fn terminal() -> bool {
    std::io::stderr().is_terminal()
}

pub fn stdout_style(text: &str, code: u8) -> String {
    if std::io::stdout().is_terminal() {
        style(text, code)
    } else {
        text.to_owned()
    }
}

fn interactive() -> bool {
    terminal() && !std::env::var("TERM").is_ok_and(|term| term == "dumb")
}

pub fn style(text: &str, code: u8) -> String {
    if !interactive() || std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        text.to_owned()
    } else {
        format!("\x1b[{code}m{text}\x1b[0m")
    }
}

pub fn heading(repository: &str, reference: &str) {
    if terminal() {
        eprintln!("\n┌ {}  {}\n│", style(repository, 1), style(reference, 2));
    }
}

pub fn ready(started: Instant, skipped: bool) {
    if terminal() {
        let label = if skipped {
            "Created · setup skipped"
        } else {
            "Ready"
        };
        eprintln!(
            "│\n└ {} {}\n",
            style(label, 32),
            style(&format!("in {:.1}s", started.elapsed().as_secs_f64()), 2)
        );
    }
}

fn line(mark: &str, label: &str, started: Instant, color: u8) -> String {
    format!(
        "│ {} {label:<24} {}",
        style(mark, color),
        style(&format!("{:>5.1}s", started.elapsed().as_secs_f64()), 2)
    )
}

pub fn progress<T>(
    running: &str,
    complete: &str,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if !terminal() {
        return operation();
    }
    let started = Instant::now();
    if !interactive() {
        eprintln!("│ {running}...");
        let result = operation();
        eprintln!(
            "{}",
            line(
                if result.is_ok() { "◇" } else { "×" },
                if result.is_ok() { complete } else { running },
                started,
                if result.is_ok() { 32 } else { 31 }
            )
        );
        return result;
    }
    // Only this short line is redrawn. No full-width padding or cursor hiding.
    thread::scope(|scope| {
        let (stop, stopped) = mpsc::channel();
        let spinner = scope.spawn(move || {
            let frames = ["◒", "◐", "◓", "◑"];
            for mark in frames.into_iter().cycle() {
                eprint!("\r\x1b[2K{}", line(mark, running, started, 36));
                if !matches!(
                    stopped.recv_timeout(Duration::from_millis(100)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    break;
                }
            }
        });
        let result = operation();
        let _ = stop.send(());
        let _ = spinner.join();
        let (mark, label, color) = if result.is_ok() {
            ("◇", complete, 32)
        } else {
            ("×", running, 31)
        };
        eprintln!("\r\x1b[2K{}", line(mark, label, started, color));
        result
    })
}

/// Keep setup and trust prompts in the same layout as command progress.
struct PanelTheme;

impl cliclack::Theme for PanelTheme {
    fn state_symbol(&self, state: &cliclack::ThemeState) -> String {
        match state {
            cliclack::ThemeState::Active => style("?", 36),
            cliclack::ThemeState::Submit => style("◇", 32),
            cliclack::ThemeState::Cancel => style("×", 31),
            cliclack::ThemeState::Error(_) => style("!", 33),
        }
    }

    fn format_intro(&self, title: &str) -> String {
        format!("\n┌ {}\n│\n", style(title.trim(), 1))
    }

    fn format_outro(&self, message: &str) -> String {
        format!("└ {} {message}\n", style("◇", 32))
    }

    fn format_header(&self, state: &cliclack::ThemeState, prompt: &str) -> String {
        format!(
            "│ {} {}\n",
            self.state_symbol(state),
            prompt.replace('\n', "\n│   ")
        )
    }

    fn format_footer_with_message(&self, state: &cliclack::ThemeState, message: &str) -> String {
        match state {
            cliclack::ThemeState::Cancel => format!("│ {} Cancelled\n", style("×", 31)),
            cliclack::ThemeState::Error(error) => format!("│   {}\n", style(error, 33)),
            _ if !message.is_empty() => format!("│   {message}\n"),
            _ => String::new(),
        }
    }

    fn format_confirm(&self, state: &cliclack::ThemeState, confirm: bool) -> String {
        let yes = self.radio_item(state, confirm, "Yes", "");
        let no = self.radio_item(state, !confirm, "No", "");
        let divider = if matches!(
            state,
            cliclack::ThemeState::Active | cliclack::ThemeState::Error(_)
        ) {
            " / "
        } else {
            ""
        };
        format!("│   {yes}{divider}{no}\n")
    }

    fn format_input(
        &self,
        state: &cliclack::ThemeState,
        cursor: &cliclack::StringCursor,
    ) -> String {
        let input_style = self.input_style(state);
        let input = if matches!(
            state,
            cliclack::ThemeState::Active | cliclack::ThemeState::Error(_)
        ) {
            self.cursor_with_style(cursor, &input_style)
        } else {
            cursor.to_string()
        };
        indented(&input_style.apply_to(input).to_string())
    }

    fn format_placeholder(
        &self,
        state: &cliclack::ThemeState,
        cursor: &cliclack::StringCursor,
    ) -> String {
        let placeholder_style = self.placeholder_style(state);
        let text = match state {
            cliclack::ThemeState::Active | cliclack::ThemeState::Error(_) => {
                self.cursor_with_style(cursor, &placeholder_style)
            }
            cliclack::ThemeState::Cancel => String::new(),
            cliclack::ThemeState::Submit => cursor.to_string(),
        };
        indented(&placeholder_style.apply_to(text).to_string())
    }

    fn format_note_with_symbol(
        &self,
        _is_outro: bool,
        _symbol: &str,
        prompt: &str,
        message: &str,
    ) -> String {
        format!(
            "│ {}\n{}│\n",
            style(prompt, 1),
            indented(&cliclack::termwrap(message, 4))
        )
    }
}

fn indented(text: &str) -> String {
    text.lines().map(|line| format!("│   {line}\n")).collect()
}

pub fn init() {
    cliclack::set_theme(PanelTheme);
}
