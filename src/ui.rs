//! Rendering. The prompt is pinned to the bottom; the transcript takes the rest.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, Choice, Direction, Entry, Pending, Status};
use crate::protocol::Action;
use crate::wrap;

/// Prompt box grows with the text, up to this many rows of content.
const MAX_INPUT_ROWS: u16 = 10;
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// How the last frame was laid out. The event loop needs this to clamp
/// scrolling to the content that was actually rendered.
#[derive(Debug, Default, Clone, Copy)]
pub struct Metrics {
    pub transcript_height: u16,
    pub content_height: u16,
    /// Button areas from the approval modal, for mouse hit-testing. `None` when
    /// the modal is not up.
    pub allow_button: Option<Rect>,
    pub deny_button: Option<Rect>,
    /// The session picker's row area and the index of its first visible row, so
    /// a click can be mapped back to a session. `None` when the picker is closed.
    pub picker_list: Option<Rect>,
    pub picker_offset: usize,
}

/// Did `(column, row)` land inside `area`?
pub fn hit(area: Option<Rect>, column: u16, row: u16) -> bool {
    area.is_some_and(|a| {
        column >= a.x && column < a.x + a.width && row >= a.y && row < a.y + a.height
    })
}

impl Metrics {
    /// Largest valid scroll offset: the point where the last line sits at the
    /// bottom of the viewport.
    pub fn max_scroll(&self) -> u16 {
        self.content_height.saturating_sub(self.transcript_height)
    }
}

pub fn draw(frame: &mut Frame, app: &mut App) -> Metrics {
    let area = frame.area();

    // Lay the prompt out first so we know how tall it needs to be.
    // -2 for the box borders, -2 for the "> " gutter.
    let input_width = area.width.saturating_sub(4).max(1);
    let input_layout = app.input.layout(input_width);
    let input_rows = (input_layout.rows.len() as u16).clamp(1, MAX_INPUT_ROWS);

    // The completion menu sits directly above the status line, so it grows
    // towards the transcript and leaves the prompt where it is.
    let completions = app.completions();
    let menu_rows = if completions.is_empty() {
        0
    } else {
        (completions.len() as u16 + 2).min(area.height / 2)
    };

    let [transcript_area, menu_area, status_area, input_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(menu_rows),
        Constraint::Length(1),
        Constraint::Length(input_rows + 2),
    ])
    .areas(area);

    let mut metrics = draw_transcript(frame, app, transcript_area);
    if menu_rows > 0 {
        draw_completions(frame, &completions, app.completion_index(), menu_area);
    }
    draw_status(frame, app, status_area);
    draw_input(frame, app, input_area, &input_layout, input_rows);

    // Drawn last so it sits above everything else. The two modals are mutually
    // exclusive; approval takes precedence if both were ever set.
    if let Some(pending) = app.pending() {
        let (allow, deny) = draw_approval(frame, pending, area);
        metrics.allow_button = Some(allow);
        metrics.deny_button = Some(deny);
    } else if let Some(picker) = app.picker() {
        let (list, offset) = draw_picker(frame, picker, area);
        metrics.picker_list = Some(list);
        metrics.picker_offset = offset;
    }
    metrics
}

/// The `/load` session picker. Returns the inner row area and the index of the
/// first visible row, so a click can be mapped back to a session.
fn draw_picker(frame: &mut Frame, picker: &crate::app::Picker, area: Rect) -> (Rect, usize) {
    let width = area.width.saturating_sub(8).clamp(20, 60);
    // Border (2) + footer (1) + a blank (1), plus one row per session, capped.
    let max_list = (area.height.saturating_sub(2) / 2).max(1);
    let list_rows = (picker.sessions.len() as u16).clamp(1, max_list);
    let height = list_rows + 4;

    let modal = center(area, width, height);
    frame.render_widget(Clear, modal);
    let block = Block::bordered()
        .title(Line::from(" load session ").bold())
        .border_style(Style::default().fg(Color::Blue));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let [list_area, footer_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    // Scroll a window around the selection so it stays visible in a long list.
    let visible = list_area.height as usize;
    let offset = picker
        .selected
        .saturating_sub(visible.saturating_sub(1))
        .min(picker.sessions.len().saturating_sub(visible));

    let rows: Vec<Line> = picker
        .sessions
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(i, name)| {
            let focused = i == picker.selected;
            let style = if focused {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let marker = if focused { "› " } else { "  " };
            Line::from(Span::styled(format!("{marker}{name}"), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(rows)), list_area);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑/↓ choose · Enter load · Esc cancel",
            Style::default().fg(Color::DarkGray),
        ))),
        footer_area,
    );

    (list_area, offset)
}

/// The slash-command completion menu, shown above the status line.
fn draw_completions(
    frame: &mut Frame,
    completions: &[&'static crate::command::Spec],
    selected: usize,
    area: Rect,
) {
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(Line::from(" commands ").bold())
        .border_style(Style::default().fg(Color::Blue));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Align descriptions into a column so the list reads as a table.
    let name_width = completions.iter().map(|c| c.name.len()).max().unwrap_or(0);

    // The menu is capped so it cannot push the prompt off a short screen, which
    // means the list can outgrow it. Scroll a window around the selection — the
    // same treatment `draw_picker` gives a long session list — or entries past
    // the cap would be invisible and unreachable.
    let visible = inner.height as usize;
    let offset = selected
        .saturating_sub(visible.saturating_sub(1))
        .min(completions.len().saturating_sub(visible));

    let lines: Vec<Line> = completions
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(i, spec)| {
            let focused = i == selected;
            let name = format!(" /{:<name_width$} ", spec.name, name_width = name_width);
            let name_style = if focused {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Blue)
            };
            Line::from(vec![
                Span::styled(name, name_style),
                Span::styled(
                    format!("  {}", spec.description),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// The approval modal. Returns the Allow and Deny button areas so the event
/// loop can hit-test mouse clicks against them.
fn draw_approval(frame: &mut Frame, pending: &Pending, area: Rect) -> (Rect, Rect) {
    let width = area.width.saturating_sub(8).clamp(24, 76);

    // Build the body: a command shown in full, or a write shown as path + a
    // bounded preview so a large file never blows up the box.
    let inner_width = width.saturating_sub(4) as usize;
    let (prompt, title, mut body): (&str, &str, Vec<Line>) = match &pending.action {
        // Only reachable under `--confirm-reads`; reads are otherwise silent.
        Action::Read { path } => (
            "The model wants to read:",
            " read this file? ",
            vec![Line::from(Span::styled(
                path.clone(),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ))],
        ),
        // Only reachable under `--confirm-fetch`; fetches are otherwise silent.
        Action::Fetch { url } => (
            "The model wants to fetch:",
            " fetch this URL? ",
            vec![Line::from(Span::styled(
                url.clone(),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ))],
        ),
        Action::Write { path, contents } => (
            "The model wants to write:",
            " write this file? ",
            std::iter::once(Line::from(Span::styled(
                path.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )))
            .chain(preview_lines(contents, inner_width))
            .collect(),
        ),
        Action::Edit { path, old, new } => (
            "The model wants to edit:",
            " apply this edit? ",
            std::iter::once(Line::from(Span::styled(
                path.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )))
            .chain(diff_lines(old, new, inner_width))
            .collect(),
        ),
        action => (
            "The model wants to run:",
            " run this command? ",
            body_lines(
                action.body(),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
                inner_width,
            ),
        ),
    };

    // Border + prompt + blank + body + blank + buttons.
    let height = (body.len() as u16 + 7)
        .min(area.height.saturating_sub(2))
        .max(7);
    let modal = center(area, width, height);
    frame.render_widget(Clear, modal);

    let block = Block::bordered()
        .title(Line::from(title).bold())
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let [text_area, button_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    let mut lines = vec![Line::from(Span::styled(
        prompt,
        Style::default().fg(Color::Gray),
    ))];
    lines.push(Line::default());
    lines.append(&mut body);
    frame.render_widget(Paragraph::new(Text::from(lines)), text_area);

    // Two fixed-width buttons, centred as a pair.
    const BUTTON: u16 = 11;
    let gap = 2;
    let total = BUTTON * 2 + gap;
    let start = button_area.x + button_area.width.saturating_sub(total) / 2;
    let allow = Rect::new(start, button_area.y, BUTTON, 1);
    let deny = Rect::new(start + BUTTON + gap, button_area.y, BUTTON, 1);

    for (rect, label, choice, colour) in [
        (allow, "  Allow  ", Choice::Allow, Color::Green),
        (deny, "  Deny  ", Choice::Deny, Color::Red),
    ] {
        let focused = pending.selected == choice;
        let style = if focused {
            Style::default()
                .fg(Color::Black)
                .bg(colour)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colour)
        };
        let text = if focused {
            format!("[{label}]")
        } else {
            format!(" {label} ")
        };
        frame.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), rect);
    }

    (allow, deny)
}

/// Centre a `width` x `height` box inside `area`.
fn center(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn draw_transcript(frame: &mut Frame, app: &mut App, area: Rect) -> Metrics {
    let block = Block::bordered()
        .title(Line::from(" ai-harness ").bold())
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);

    // Wrapping up front means `lines.len()` is the true rendered height, which
    // is what scroll clamping and "stick to the bottom" both depend on.
    let lines = transcript_lines(app, inner.width as usize);
    let content_height = lines.len() as u16;
    let metrics = Metrics {
        transcript_height: inner.height,
        content_height,
        ..Metrics::default()
    };

    // Following means "pin to the bottom", which we can only resolve here,
    // once we know how tall the wrapped content turned out to be.
    let max_scroll = metrics.max_scroll();
    if app.follow {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
    }

    let paragraph = Paragraph::new(Text::from(lines))
        .block(block)
        .scroll((app.scroll, 0));
    frame.render_widget(paragraph, area);
    metrics
}

/// Build the transcript as already-wrapped lines, so one `Line` is one screen row.
fn transcript_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    if app.transcript.is_empty() {
        lines.extend(body_lines(
            "Type a prompt and press Enter. Alt+Enter inserts a newline; Ctrl+C quits.",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
            width,
        ));
    }

    for entry in app.transcript.iter() {
        // Rendered per entry so a hidden one contributes nothing at all — not
        // even the blank separator line.
        let mut block: Vec<Line> = Vec::new();
        render_entry(app, entry, width, &mut block);
        if block.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(block);
    }

    // The live reply renders in place of the spinner once tokens arrive.
    if let Some(text) = &app.streaming {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            app.model.clone(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )));
        // A block cursor on the last line signals the reply is still arriving.
        let mut body = body_lines(&format!("{text}▌"), Style::default(), width);
        lines.append(&mut body);
    } else {
        let activity = match app.status {
            Status::Waiting => Some("thinking…"),
            Status::Running => Some("running…"),
            _ => None,
        };
        if let Some(activity) = activity {
            let spinner = SPINNER[(app.tick / 2) % SPINNER.len()];
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                format!("{spinner} {activity}"),
                Style::default().fg(Color::Yellow),
            )));
        }
    }

    lines
}

fn render_entry(app: &App, entry: &Entry, width: usize, lines: &mut Vec<Line<'static>>) {
    match entry {
        Entry::User(content) => {
            lines.push(Line::from(Span::styled(
                "you",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.extend(body_lines(content, Style::default(), width));
        }
        Entry::Action { action, usage } => {
            // Label the action type, since that is the whole point of the
            // protocol: the user should see which branch the model chose.
            let (label, colour) = match action {
                Action::Shell(_) => ("shell", Color::Magenta),
                Action::Read { .. } => ("read", Color::Blue),
                Action::Fetch { .. } => ("fetch", Color::Blue),
                Action::Write { .. } => ("write", Color::Cyan),
                Action::Edit { .. } => ("edit", Color::Cyan),
                Action::Response(_) => ("response", Color::Green),
            };
            let mut header = vec![Span::styled(
                label,
                Style::default().fg(colour).add_modifier(Modifier::BOLD),
            )];
            // For a write or edit, the path is the headline; the body is below.
            if let Action::Write { path, .. } | Action::Edit { path, .. } = action {
                header.push(Span::styled(
                    format!("  {path}"),
                    Style::default().fg(Color::Cyan),
                ));
            }
            header.push(Span::styled(
                format!("  {}", app.model),
                Style::default().fg(Color::DarkGray),
            ));
            if let Some(u) = usage {
                header.push(Span::styled(
                    format!("  {} in / {} out", u.prompt_tokens, u.completion_tokens),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            lines.push(Line::from(header));

            match action {
                // Shell commands read as commands, not prose.
                Action::Shell(cmd) => {
                    lines.extend(body_lines(cmd, Style::default().fg(Color::Magenta), width))
                }
                // The path is the whole action; the contents arrive as a result.
                Action::Read { path } => {
                    lines.extend(body_lines(path, Style::default().fg(Color::Blue), width))
                }
                // Likewise the URL; the page text arrives as a result.
                Action::Fetch { url } => {
                    lines.extend(body_lines(url, Style::default().fg(Color::Blue), width))
                }
                Action::Response(text) => lines.extend(body_lines(text, Style::default(), width)),
                // A file could be huge; show a bounded preview, not the whole thing.
                Action::Write { contents, .. } => {
                    lines.extend(preview_lines(contents, width));
                }
                // An edit reads as a diff: what goes, what comes.
                Action::Edit { old, new, .. } => {
                    lines.extend(diff_lines(old, new, width));
                }
            }
        }
        Entry::Malformed { reason, raw } => {
            lines.push(Line::from(vec![
                Span::styled(
                    "protocol error",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {reason}"), Style::default().fg(Color::Yellow)),
            ]));
            lines.extend(body_lines(raw, Style::default().fg(Color::DarkGray), width));
        }
        Entry::CommandResult(output) => {
            let ok = output.succeeded();
            lines.push(Line::from(vec![
                Span::styled(
                    "result",
                    Style::default()
                        .fg(if ok { Color::Blue } else { Color::Red })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}", output.summary()),
                    Style::default().fg(if ok { Color::DarkGray } else { Color::Red }),
                ),
            ]));
            let dim = Style::default().fg(Color::DarkGray);
            if !output.stdout.trim().is_empty() {
                lines.extend(body_lines(output.stdout.trim_end(), dim, width));
            }
            if !output.stderr.trim().is_empty() {
                lines.extend(body_lines(
                    output.stderr.trim_end(),
                    Style::default().fg(Color::Red),
                    width,
                ));
            }
            if output.truncated {
                lines.push(Line::from(Span::styled(
                    "… output truncated",
                    dim.add_modifier(Modifier::ITALIC),
                )));
            }
        }
        Entry::ReadResult(outcome) => {
            let ok = outcome.succeeded();
            lines.push(Line::from(vec![
                Span::styled(
                    "result",
                    Style::default()
                        .fg(if ok { Color::Blue } else { Color::Red })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}  {}", outcome.path, outcome.summary()),
                    Style::default().fg(if ok { Color::DarkGray } else { Color::Red }),
                ),
            ]));
            match &outcome.error {
                Some(error) => {
                    lines.extend(body_lines(error, Style::default().fg(Color::Red), width))
                }
                // The model gets the whole file; the transcript gets a taste of
                // it, since a read happens without the user asking for it. The
                // counts are already in the header above.
                None => lines.extend(preview_body(&outcome.contents, width)),
            }
        }
        Entry::FetchResult(outcome) => {
            let ok = outcome.succeeded();
            lines.push(Line::from(vec![
                Span::styled(
                    "result",
                    Style::default()
                        .fg(if ok { Color::Blue } else { Color::Red })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}  {}", outcome.url, outcome.summary()),
                    Style::default().fg(if ok { Color::DarkGray } else { Color::Red }),
                ),
            ]));
            // Where it actually landed matters more than usual here — a fetch
            // the user never approved may have been redirected somewhere else.
            if let Some(final_url) = &outcome.final_url {
                lines.push(Line::from(Span::styled(
                    format!("  → {final_url}"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            match &outcome.error {
                Some(error) => {
                    lines.extend(body_lines(error, Style::default().fg(Color::Red), width))
                }
                None => lines.extend(preview_body(&outcome.text, width)),
            }
        }
        Entry::WriteResult(outcome) => {
            let ok = outcome.succeeded();
            let mut spans = vec![Span::styled(
                "result",
                Style::default()
                    .fg(if ok { Color::Blue } else { Color::Red })
                    .add_modifier(Modifier::BOLD),
            )];
            spans.push(Span::styled(
                format!("  {} {}", outcome.summary(), outcome.path),
                Style::default().fg(if ok { Color::DarkGray } else { Color::Red }),
            ));
            lines.push(Line::from(spans));
            if let Some(error) = &outcome.error {
                lines.extend(body_lines(error, Style::default().fg(Color::Red), width));
            }
        }
        Entry::Denied(command) => {
            lines.push(Line::from(Span::styled(
                "denied",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
            lines.extend(body_lines(
                command,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::CROSSED_OUT),
                width,
            ));
        }
        // Recorded always, shown only in debug mode, so toggling /debug
        // reveals traffic that already happened.
        Entry::Frame { direction, body } if app.debug => {
            let label = match direction {
                Direction::Sent => "sent",
                Direction::Received => "received",
            };
            lines.push(Line::from(Span::styled(
                format!("{} {label}", direction.arrow()),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::DIM | Modifier::BOLD),
            )));
            lines.extend(body_lines(
                body,
                Style::default().fg(Color::DarkGray),
                width,
            ));
        }
        // Frames stay hidden outside debug mode, contributing no lines.
        Entry::Frame { .. } => {}
        Entry::Error(message) => {
            lines.push(Line::from(Span::styled(
                "error",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
            lines.extend(body_lines(message, Style::default().fg(Color::Red), width));
        }
        Entry::Notice(message) => {
            lines.extend(body_lines(
                message,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
                width,
            ));
        }
    }
}

fn body_lines(content: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    wrap::text(content, width)
        .into_iter()
        .map(|row| Line::from(Span::styled(row, style)))
        .collect()
}

/// The first few lines of file contents, plus a size summary — so a large write
/// never floods the transcript or the approval modal.
fn preview_lines(contents: &str, width: usize) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "{} line(s), {} bytes",
            contents.lines().count(),
            contents.len()
        ),
        dim.add_modifier(Modifier::ITALIC),
    ))];
    lines.extend(preview_body(contents, width));
    lines
}

/// Just the bounded excerpt, for callers whose header already carries the size.
fn preview_body(contents: &str, width: usize) -> Vec<Line<'static>> {
    const MAX: usize = 8;
    let total = contents.lines().count();
    let dim = Style::default().fg(Color::DarkGray);

    let mut lines: Vec<Line> = Vec::new();
    for line in contents.lines().take(MAX) {
        lines.extend(body_lines(line, dim, width));
    }
    if total > MAX {
        lines.push(Line::from(Span::styled(
            format!("… {} more line(s)", total - MAX),
            dim.add_modifier(Modifier::ITALIC),
        )));
    }
    lines
}

/// A unified diff of an edit: removed lines in red with `-`, added lines in
/// green with `+`. Each side is capped like a preview so a large edit never
/// blows up the modal or the transcript.
fn diff_lines(old: &str, new: &str, width: usize) -> Vec<Line<'static>> {
    const MAX: usize = 6;

    fn side(text: &str, marker: char, colour: Color, width: usize, lines: &mut Vec<Line<'static>>) {
        // Line up wrapped continuations under the text, not the marker.
        let cont = "  ";
        let inner = width.saturating_sub(2).max(1);
        let total = text.lines().count();
        for line in text.lines().take(MAX) {
            let wrapped = wrap::text(line, inner);
            for (i, row) in wrapped.into_iter().enumerate() {
                let prefix = if i == 0 {
                    format!("{marker} ")
                } else {
                    cont.to_string()
                };
                lines.push(Line::from(Span::styled(
                    format!("{prefix}{row}"),
                    Style::default().fg(colour),
                )));
            }
        }
        if total > MAX {
            lines.push(Line::from(Span::styled(
                format!("{marker} … {} more line(s)", total - MAX),
                Style::default().fg(colour).add_modifier(Modifier::ITALIC),
            )));
        }
    }

    let mut lines = Vec::new();
    side(old, '-', Color::Red, width, &mut lines);
    // An empty replacement is a deletion; say so rather than showing nothing.
    if new.is_empty() {
        lines.push(Line::from(Span::styled(
            "+ (deleted)",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::ITALIC),
        )));
    } else {
        side(new, '+', Color::Green, width, &mut lines);
    }
    lines
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let (label, colour) = match app.status {
        Status::Idle => (" ready ", Color::Green),
        Status::Waiting => (" waiting ", Color::Yellow),
        Status::Streaming => (" streaming ", Color::Cyan),
        Status::AwaitingApproval(_) => (" approve ", Color::Magenta),
        Status::Running => (" running ", Color::Blue),
    };

    let mut spans = vec![
        Span::styled(label, Style::default().fg(Color::Black).bg(colour)),
        Span::raw(" "),
        Span::styled(app.model.clone(), Style::default().fg(Color::Gray)),
    ];
    // Cumulative spend, once there is any. Hidden on a fresh session so the bar
    // does not open with a row of zeroes.
    if !app.ledger.is_empty() {
        spans.push(Span::styled(
            format!("  {}", app.cost_status()),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if app.debug {
        spans.push(Span::styled(
            "  debug",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if !app.follow {
        spans.push(Span::styled(
            "  scrolled — End to resume",
            Style::default().fg(Color::DarkGray),
        ));
    }
    let hints = if app.pending().is_some() {
        "  ←/→ choose · Enter confirm · y allow · n/Esc deny"
    } else if matches!(
        app.status,
        Status::Waiting | Status::Streaming | Status::Running
    ) {
        // Busy without a modal: the one useful key is Esc.
        "  Esc cancel · Ctrl+C quit"
    } else {
        "  Enter send · Alt+Enter newline · Ctrl+L clear · Ctrl+C quit"
    };
    spans.push(Span::styled(hints, Style::default().fg(Color::DarkGray)));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_input(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    layout: &crate::input::Layout,
    visible_rows: u16,
) {
    let (border, gutter) = if app.is_busy() {
        (Color::DarkGray, Color::DarkGray)
    } else {
        (Color::Blue, Color::Blue)
    };

    let block = Block::bordered().border_style(Style::default().fg(border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Scroll the prompt so the cursor's row stays visible in a tall entry.
    let (cursor_row, cursor_col) = layout.cursor;
    let offset = cursor_row.saturating_sub(visible_rows.saturating_sub(1));

    let lines: Vec<Line> = layout
        .rows
        .iter()
        .skip(offset as usize)
        .take(visible_rows as usize)
        .enumerate()
        .map(|(i, row)| {
            let marker = if i == 0 && offset == 0 { "> " } else { "  " };
            Line::from(vec![
                Span::styled(marker, Style::default().fg(gutter)),
                Span::raw(row.clone()),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(Text::from(lines)), inner);

    if !app.is_busy() {
        frame.set_cursor_position(Position::new(
            inner.x + 2 + cursor_col,
            inner.y + cursor_row.saturating_sub(offset),
        ));
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    /// Render into a fake terminal and return the screen as one string per row.
    fn render(app: &mut App, width: u16, height: u16) -> (Vec<String>, Metrics) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut metrics = Metrics::default();
        terminal.draw(|frame| metrics = draw(frame, app)).unwrap();

        let buffer = terminal.backend().buffer().clone();
        let rows = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        (rows, metrics)
    }

    #[test]
    fn max_scroll_is_zero_when_content_fits() {
        let m = Metrics {
            transcript_height: 20,
            content_height: 5,
            ..Metrics::default()
        };
        assert_eq!(m.max_scroll(), 0);
    }

    #[test]
    fn max_scroll_exposes_the_overflow() {
        let m = Metrics {
            transcript_height: 10,
            content_height: 25,
            ..Metrics::default()
        };
        assert_eq!(m.max_scroll(), 15);
    }

    #[test]
    fn prompt_is_pinned_to_the_bottom_of_the_screen() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hello");
        let (rows, _) = render(&mut app, 40, 12);

        // Last three rows are the prompt box: top border, text, bottom border.
        assert!(
            rows[9].starts_with('┌'),
            "expected prompt top border, got {:?}",
            rows[9]
        );
        assert!(
            rows[10].contains("> hello"),
            "expected prompt text, got {:?}",
            rows[10]
        );
        assert!(
            rows[11].starts_with('└'),
            "expected prompt bottom border, got {:?}",
            rows[11]
        );
    }

    #[test]
    fn prompt_grows_downward_keeping_its_bottom_edge_fixed() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("one\ntwo\nthree");
        let (rows, _) = render(&mut app, 40, 12);

        assert!(
            rows[7].starts_with('┌'),
            "prompt should have grown upward from a fixed bottom"
        );
        assert!(rows[8].contains("> one"));
        assert!(rows[9].contains("two"));
        assert!(rows[10].contains("three"));
        assert!(
            rows[11].starts_with('└'),
            "bottom edge must stay on the last row"
        );
    }

    /// Only the transcript pane, so status-bar text cannot satisfy an assertion.
    fn transcript_only(rows: &[String]) -> String {
        rows.iter()
            .take_while(|r| !r.starts_with('└'))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn transcript_renders_both_turns() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("what is 2+2");
        app.submit().unwrap();
        app.push_response("<ai-harness-response>4</ai-harness-response>".into(), None);

        let (rows, _) = render(&mut app, 40, 14);
        let screen = transcript_only(&rows);
        assert!(screen.contains("you"), "missing user header:\n{screen}");
        assert!(
            screen.contains("what is 2+2"),
            "missing prompt echo:\n{screen}"
        );
        assert!(
            screen.contains("response"),
            "missing action label:\n{screen}"
        );
        assert!(screen.contains('4'), "missing reply body:\n{screen}");
    }

    /// Drive an app to the point where a shell command awaits approval.
    fn awaiting_approval(command: &str) -> App {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("list files");
        app.submit().unwrap();
        app.push_response(
            format!("<ai-harness-shell>{command}</ai-harness-shell>"),
            None,
        );
        assert!(app.pending().is_some(), "should be awaiting approval");
        app
    }

    #[test]
    fn shell_action_is_labelled_in_the_transcript_once_approved() {
        let mut app = awaiting_approval("ls -la");
        app.approve();
        let (rows, _) = render(&mut app, 60, 16);
        let screen = transcript_only(&rows);
        assert!(screen.contains("shell"), "missing shell label:\n{screen}");
        assert!(screen.contains("ls -la"), "missing command:\n{screen}");
    }

    /// Drive an app to a pending write approval.
    fn awaiting_write(path: &str, contents: &str) -> App {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("write a file");
        app.submit().unwrap();
        app.push_response(
            format!("<ai-harness-write file={path}>\n{contents}</ai-harness-write>"),
            None,
        );
        assert!(app.pending().is_some(), "should be awaiting write approval");
        app
    }

    #[test]
    fn write_approval_modal_shows_the_path_and_a_bounded_preview() {
        // Twenty lines — the preview must summarise, not dump them all.
        let contents: String = (0..20).map(|i| format!("line {i}\n")).collect();
        let mut app = awaiting_write("src/big.rs", &contents);
        let (rows, _) = render(&mut app, 70, 20);
        let screen = rows.join("\n");

        assert!(
            screen.contains("write this file?"),
            "missing title:\n{screen}"
        );
        assert!(screen.contains("src/big.rs"), "missing path:\n{screen}");
        assert!(screen.contains("line 0"), "missing preview head:\n{screen}");
        assert!(
            screen.contains("more line"),
            "missing truncation note:\n{screen}"
        );
        assert!(
            !screen.contains("line 19"),
            "the full file must not be dumped into the modal:\n{screen}"
        );
    }

    #[test]
    fn read_action_and_its_result_are_rendered() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.transcript.push(Entry::Action {
            action: crate::protocol::Action::Read {
                path: "src/app.rs".into(),
            },
            usage: None,
        });
        app.transcript
            .push(Entry::ReadResult(crate::files::ReadOutcome {
                path: "src/app.rs".into(),
                contents: "alpha\nbeta\n".into(),
                lines: 2,
                truncated: false,
                error: None,
            }));

        let (rows, _) = render(&mut app, 70, 20);
        let screen = transcript_only(&rows);
        assert!(screen.contains("read"), "missing read label:\n{screen}");
        assert!(screen.contains("src/app.rs"), "missing path:\n{screen}");
        assert!(screen.contains("2 line(s)"), "missing counts:\n{screen}");
        assert!(screen.contains("alpha"), "missing preview:\n{screen}");
    }

    #[test]
    fn fetch_action_and_its_result_are_rendered() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.transcript.push(Entry::Action {
            action: crate::protocol::Action::Fetch {
                url: "https://example.com/docs".into(),
            },
            usage: None,
        });
        app.transcript
            .push(Entry::FetchResult(Box::new(crate::fetch::FetchOutcome {
                url: "https://example.com/docs".into(),
                final_url: Some("https://example.com/en/docs".into()),
                status: Some(200),
                content_type: Some("text/html".into()),
                text: "Title\nBody text\n".into(),
                bytes: 900,
                truncated: false,
                error: None,
            })));

        let (rows, _) = render(&mut app, 70, 20);
        let screen = transcript_only(&rows);
        assert!(screen.contains("fetch"), "missing fetch label:\n{screen}");
        assert!(screen.contains("example.com"), "missing url:\n{screen}");
        assert!(screen.contains("Title"), "missing preview:\n{screen}");
        // Where it actually landed matters: nobody approved this request.
        assert!(
            screen.contains("/en/docs"),
            "the redirect target should be visible:\n{screen}"
        );
    }

    #[test]
    fn a_refused_fetch_is_rendered_with_its_reason() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.transcript.push(Entry::FetchResult(Box::new(
            crate::fetch::FetchOutcome::failed(
                "https://169.254.169.254/",
                "169.254.169.254 is a link-local address",
            ),
        )));

        let (rows, _) = render(&mut app, 70, 20);
        let screen = transcript_only(&rows);
        assert!(screen.contains("failed"), "missing failure:\n{screen}");
        assert!(screen.contains("link-local"), "missing reason:\n{screen}");
    }

    #[test]
    fn fetch_approval_modal_appears_under_confirm_fetch() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.confirm_fetches = true;
        app.push_response(
            "<ai-harness-fetch>https://example.com/docs</ai-harness-fetch>".into(),
            None,
        );

        let (rows, _) = render(&mut app, 70, 20);
        let screen = rows.join("\n");
        assert!(
            screen.contains("wants to fetch"),
            "the modal should name the action:\n{screen}"
        );
        assert!(
            screen.contains("example.com/docs"),
            "the modal must show the URL being fetched:\n{screen}"
        );
    }

    #[test]
    fn a_failed_read_is_rendered_with_its_reason() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.transcript
            .push(Entry::ReadResult(crate::files::ReadOutcome::failed(
                "gone.txt",
                "gone.txt: no such file",
            )));

        let (rows, _) = render(&mut app, 70, 12);
        let screen = transcript_only(&rows);
        assert!(screen.contains("gone.txt"), "missing path:\n{screen}");
        assert!(screen.contains("no such file"), "missing reason:\n{screen}");
    }

    /// A long file must not flood the transcript just because nobody approved it.
    #[test]
    fn a_long_read_result_is_previewed_not_dumped() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        let contents: String = (0..40).map(|i| format!("line {i}\n")).collect();
        app.transcript
            .push(Entry::ReadResult(crate::files::ReadOutcome {
                path: "big.txt".into(),
                lines: 40,
                contents,
                truncated: false,
                error: None,
            }));

        let (rows, _) = render(&mut app, 70, 24);
        let screen = transcript_only(&rows);
        assert!(screen.contains("line 0"), "missing preview head:\n{screen}");
        assert!(screen.contains("more line"), "missing summary:\n{screen}");
        assert!(
            !screen.contains("line 39"),
            "the whole file leaked into the transcript:\n{screen}"
        );
    }

    #[test]
    fn read_approval_modal_appears_under_confirm_reads() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.confirm_reads = true;
        app.input.insert_str("look at a file");
        app.submit().unwrap();
        app.push_response("<ai-harness-read>src/app.rs</ai-harness-read>".into(), None);
        assert!(app.pending().is_some(), "should be awaiting read approval");

        let (rows, _) = render(&mut app, 70, 18);
        let screen = rows.join("\n");
        assert!(
            screen.contains("read this file?"),
            "missing title:\n{screen}"
        );
        assert!(screen.contains("src/app.rs"), "missing path:\n{screen}");
    }

    #[test]
    fn edit_action_renders_as_a_diff() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.transcript.push(Entry::Action {
            action: crate::protocol::Action::Edit {
                path: "src/app.rs".into(),
                old: "let x = 1;".into(),
                new: "let x = 2;".into(),
            },
            usage: None,
        });

        let (rows, _) = render(&mut app, 70, 16);
        let screen = transcript_only(&rows);
        assert!(screen.contains("edit"), "missing edit label:\n{screen}");
        assert!(screen.contains("src/app.rs"), "missing path:\n{screen}");
        assert!(
            screen.contains("- let x = 1;"),
            "missing removed line:\n{screen}"
        );
        assert!(
            screen.contains("+ let x = 2;"),
            "missing added line:\n{screen}"
        );
    }

    #[test]
    fn edit_approval_modal_shows_a_bounded_diff() {
        // Drive an app to a pending edit against a seeded file.
        let dir = std::env::temp_dir().join(format!("ai-harness-ui-edit-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::write(dir.join("m.rs"), "let x = 1;\n").unwrap();

        let mut app = App::new("test/model".into(), None, 10, dir.join("sessions"));
        app.sandbox = Some(crate::sandbox::Sandbox::new(&dir).unwrap());
        app.input.insert_str("bump it");
        app.submit().unwrap();
        app.push_response(
            "<ai-harness-edit file=m.rs><ai-harness-old>let x = 1;</ai-harness-old>\
             <ai-harness-new>let x = 2;</ai-harness-new></ai-harness-edit>"
                .into(),
            None,
        );
        assert!(app.pending().is_some(), "should be awaiting edit approval");

        let (rows, _) = render(&mut app, 70, 18);
        let screen = rows.join("\n");
        assert!(
            screen.contains("apply this edit?"),
            "missing title:\n{screen}"
        );
        assert!(screen.contains("m.rs"), "missing path:\n{screen}");
        assert!(
            screen.contains("- let x = 1;"),
            "missing removed line:\n{screen}"
        );
        assert!(
            screen.contains("+ let x = 2;"),
            "missing added line:\n{screen}"
        );
    }

    #[test]
    fn a_deletion_edit_says_deleted_in_the_diff() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.transcript.push(Entry::Action {
            action: crate::protocol::Action::Edit {
                path: "x".into(),
                old: "remove me\n".into(),
                new: String::new(),
            },
            usage: None,
        });
        let (rows, _) = render(&mut app, 60, 12);
        let screen = transcript_only(&rows);
        assert!(
            screen.contains("- remove me"),
            "missing removed line:\n{screen}"
        );
        assert!(
            screen.contains("(deleted)"),
            "missing deletion marker:\n{screen}"
        );
    }

    #[test]
    fn write_result_is_rendered_with_its_path() {
        let mut app = awaiting_write("out.txt", "hi");
        app.approve();
        app.push_write_result(crate::exec::WriteOutcome {
            path: "out.txt".into(),
            bytes: 2,
            error: None,
            timed_out: false,
            cancelled: false,
        });
        let (rows, _) = render(&mut app, 60, 16);
        let screen = transcript_only(&rows);
        assert!(
            screen.contains("wrote 2 bytes"),
            "missing summary:\n{screen}"
        );
        assert!(screen.contains("out.txt"), "missing path:\n{screen}");
    }

    #[test]
    fn approval_modal_shows_the_command_and_both_buttons() {
        let mut app = awaiting_approval("rm -rf build");
        let (rows, metrics) = render(&mut app, 70, 18);
        let screen = rows.join("\n");

        assert!(
            screen.contains("run this command?"),
            "missing title:\n{screen}"
        );
        assert!(
            screen.contains("rm -rf build"),
            "missing command:\n{screen}"
        );
        assert!(screen.contains("Allow"), "missing Allow button:\n{screen}");
        assert!(screen.contains("Deny"), "missing Deny button:\n{screen}");
        assert!(
            metrics.allow_button.is_some(),
            "Allow rect must be reported"
        );
        assert!(metrics.deny_button.is_some(), "Deny rect must be reported");
    }

    #[test]
    fn modal_button_rects_are_disjoint_and_on_the_button_row() {
        let mut app = awaiting_approval("ls");
        let (_, metrics) = render(&mut app, 70, 18);
        let allow = metrics.allow_button.unwrap();
        let deny = metrics.deny_button.unwrap();

        assert_eq!(allow.y, deny.y, "buttons should share a row");
        assert!(
            allow.x + allow.width <= deny.x,
            "buttons must not overlap: {allow:?} vs {deny:?}"
        );
        // A click in one must never register as the other.
        assert!(hit(Some(allow), allow.x, allow.y));
        assert!(!hit(Some(deny), allow.x, allow.y));
        assert!(hit(Some(deny), deny.x, deny.y));
        assert!(!hit(Some(allow), deny.x, deny.y));
    }

    #[test]
    fn modal_selection_is_visible_and_moves() {
        let mut app = awaiting_approval("ls");
        let (rows, _) = render(&mut app, 70, 18);
        // Allow is focused by default and rendered with brackets.
        assert!(
            rows.join("\n").contains("[  Allow  ]"),
            "Allow should start focused:\n{}",
            rows.join("\n")
        );

        app.toggle_choice();
        let (rows, _) = render(&mut app, 70, 18);
        assert!(
            rows.join("\n").contains("[  Deny  ]"),
            "focus should move to Deny:\n{}",
            rows.join("\n")
        );
    }

    #[test]
    fn frames_are_hidden_until_debug_is_on() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("count the files");
        app.submit().unwrap();
        app.push_response(
            "<ai-harness-response>four</ai-harness-response>".into(),
            None,
        );

        let (rows, off) = render(&mut app, 70, 20);
        let hidden = transcript_only(&rows);
        assert!(
            !hidden.contains("ai-harness-query"),
            "frames must stay hidden with debug off:\n{hidden}"
        );

        // Toggling reveals traffic that already happened.
        app.debug = true;
        let (rows, on) = render(&mut app, 70, 20);
        let shown = transcript_only(&rows);
        assert!(
            shown.contains("ai-harness-query"),
            "the sent frame should appear:\n{shown}"
        );
        assert!(
            shown.contains("sent") && shown.contains("received"),
            "both directions should be labelled:\n{shown}"
        );
        assert!(
            on.content_height > off.content_height,
            "showing frames must grow the content so scrolling stays correct"
        );
    }

    #[test]
    fn hidden_frames_leave_no_blank_gap() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();

        let (rows, _) = render(&mut app, 60, 14);
        // The hidden frame sits between them in the transcript; the prompt must
        // still render immediately under its header, with no gap left behind.
        assert!(rows[1].contains("you"), "row 1 was {:?}", rows[1]);
        assert!(rows[2].contains("hi"), "row 2 was {:?}", rows[2]);
    }

    #[test]
    fn completion_menu_appears_above_the_prompt() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_char('/');
        let (rows, _) = render(&mut app, 70, 20);
        let screen = rows.join("\n");

        assert!(screen.contains("commands"), "missing menu title:\n{screen}");
        for name in ["/debug", "/clear", "/save"] {
            assert!(screen.contains(name), "missing {name}:\n{screen}");
        }
        // The prompt must still own the bottom rows.
        assert!(rows[19].starts_with('└'), "prompt lost the bottom edge");
        assert!(rows[18].contains("> /"), "prompt row was {:?}", rows[18]);
    }

    /// The menu is height-capped, so a list longer than the cap must scroll —
    /// otherwise the last commands are invisible and cannot be selected.
    #[test]
    fn the_menu_scrolls_to_keep_a_deep_selection_visible() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_char('/');
        let total = app.completions().len();

        // Walk to the final entry.
        let last = total - 1;
        app.move_completion(last as isize);
        assert_eq!(app.completion_index(), last);

        let name = app.completions()[last].name;
        let (rows, _) = render(&mut app, 70, 20);
        let screen = rows.join("\n");
        assert!(
            screen.contains(&format!("/{name}")),
            "the selected entry /{name} scrolled out of view:\n{screen}"
        );
    }

    #[test]
    fn menu_highlights_the_selected_entry() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_char('/');

        let (rows, _) = render(&mut app, 70, 20);
        let debug_row = rows.iter().position(|r| r.contains("/debug")).unwrap();
        let save_row = rows.iter().position(|r| r.contains("/save")).unwrap();
        assert!(debug_row < save_row, "menu order should follow the table");

        // Moving the highlight must not reorder or drop entries.
        app.move_completion(1);
        let (rows, _) = render(&mut app, 70, 20);
        let screen = rows.join("\n");
        assert!(screen.contains("/debug") && screen.contains("/clear"));
    }

    #[test]
    fn menu_is_absent_for_an_ordinary_prompt() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("what is 2+2");
        let (rows, _) = render(&mut app, 70, 20);
        assert!(
            !rows.join("\n").contains("commands"),
            "no menu for ordinary text"
        );
    }

    #[test]
    fn menu_narrows_to_the_typed_prefix() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("/c");
        let (rows, _) = render(&mut app, 70, 20);
        let screen = rows.join("\n");
        assert!(screen.contains("/clear"));
        assert!(
            !screen.contains("/quit"),
            "unmatched command shown:\n{screen}"
        );
    }

    #[test]
    fn menu_does_not_push_the_prompt_off_a_short_screen() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_char('/');
        // Tight enough that the menu must yield space to the prompt.
        let (rows, _) = render(&mut app, 60, 10);
        assert!(rows[9].starts_with('└'), "prompt bottom edge missing");
        assert!(rows[8].contains('>'), "prompt row was {:?}", rows[8]);
        for row in &rows {
            assert!(row.chars().count() <= 60, "row overflows: {row:?}");
        }
    }

    #[test]
    fn streaming_text_renders_live_below_the_transcript() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_delta("Hello, wor");

        let (rows, metrics) = render(&mut app, 60, 16);
        let screen = transcript_only(&rows);
        assert!(
            screen.contains("Hello, wor"),
            "live reply should be visible:\n{screen}"
        );
        assert!(screen.contains('▌'), "a cursor should mark the live reply");
        assert!(metrics.content_height > 0);
    }

    #[test]
    fn the_live_view_grows_as_tokens_arrive() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();

        app.push_delta("one line");
        let (_, before) = render(&mut app, 30, 16);
        app.push_delta("\nand a second\nand a third");
        let (_, after) = render(&mut app, 30, 16);

        assert!(
            after.content_height > before.content_height,
            "content must grow so scroll/follow stays correct: {} -> {}",
            before.content_height,
            after.content_height
        );
    }

    #[test]
    fn the_spinner_shows_before_the_first_token() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        // Waiting, no deltas yet.
        let (rows, _) = render(&mut app, 60, 16);
        assert!(transcript_only(&rows).contains("thinking"));

        app.push_delta("x");
        let (rows, _) = render(&mut app, 60, 16);
        let screen = transcript_only(&rows);
        assert!(
            !screen.contains("thinking"),
            "spinner should yield to the reply"
        );
        assert!(screen.contains('x'));
    }

    #[test]
    fn status_hint_offers_cancel_while_busy() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        // Idle: no cancel hint.
        let (rows, _) = render(&mut app, 80, 12);
        assert!(!rows.join("\n").contains("Esc cancel"));

        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_delta("partial");
        let (rows, _) = render(&mut app, 80, 12);
        assert!(
            rows.join("\n").contains("Esc cancel"),
            "a busy status bar should offer cancel:\n{}",
            rows.join("\n")
        );
    }

    #[test]
    fn status_bar_shows_streaming() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_delta("partial");
        let (rows, _) = render(&mut app, 70, 12);
        assert!(
            rows.join("\n").contains("streaming"),
            "status bar should show the streaming state:\n{}",
            rows.join("\n")
        );
    }

    #[test]
    fn status_bar_shows_the_debug_marker() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        let (rows, _) = render(&mut app, 70, 12);
        assert!(!rows.join("\n").contains("debug"));

        app.debug = true;
        let (rows, _) = render(&mut app, 70, 12);
        assert!(
            rows.join("\n").contains("debug"),
            "debug mode must be visible in the status bar:\n{}",
            rows.join("\n")
        );
    }

    #[test]
    fn no_button_rects_are_reported_without_a_modal() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        let (_, metrics) = render(&mut app, 60, 14);
        assert!(metrics.allow_button.is_none());
        assert!(metrics.deny_button.is_none());
        assert!(!hit(None, 0, 0), "a missing rect must never register a hit");
    }

    /// An app whose sessions dir holds `names`, with the load picker open.
    fn app_with_picker(names: &[&str]) -> (App, std::path::PathBuf) {
        // A per-call unique id so parallel tests never share a directory.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ai-harness-ui-picker-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for name in names {
            let session = crate::session::Session::new(
                "m".into(),
                vec![],
                vec![],
                vec![],
                Default::default(),
            );
            crate::session::save(&dir, name, &session).unwrap();
        }
        let mut app = App::new("test/model".into(), None, 10, dir.clone());
        app.open_load_picker();
        (app, dir)
    }

    #[test]
    fn picker_lists_sessions_and_reports_its_geometry() {
        let (mut app, dir) = app_with_picker(&["alpha", "beta", "gamma"]);
        let (rows, metrics) = render(&mut app, 60, 20);
        let screen = rows.join("\n");

        assert!(screen.contains("load session"), "missing title:\n{screen}");
        for name in ["alpha", "beta", "gamma"] {
            assert!(screen.contains(name), "missing {name}:\n{screen}");
        }
        assert!(screen.contains("Enter load"), "missing footer:\n{screen}");
        assert!(metrics.picker_list.is_some(), "list rect must be reported");
        assert_eq!(metrics.picker_offset, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn picker_highlights_the_selection_and_a_click_maps_to_it() {
        let (mut app, dir) = app_with_picker(&["one", "two", "three"]);
        app.picker_move(1); // second row (names are listed sorted)
        // The marker sits on whatever the second sorted entry is.
        let selected = app.picker().unwrap().sessions[1].clone();
        let (rows, metrics) = render(&mut app, 60, 20);
        assert!(
            rows.iter().any(|r| r.contains(&format!("› {selected}"))),
            "the selected row should carry the marker:\n{}",
            rows.join("\n")
        );

        // A click on that row maps back to index 1 via the reported geometry.
        let list = metrics.picker_list.unwrap();
        let clicked = metrics.picker_offset + (list.y + 1 - list.y) as usize;
        assert_eq!(clicked, 1, "click math recovers the row index");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn picker_scrolls_to_keep_a_deep_selection_visible() {
        let names: Vec<String> = (0..40).map(|i| format!("s{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let (mut app, dir) = app_with_picker(&refs);
        for _ in 0..39 {
            app.picker_move(1); // select the last one
        }
        let (rows, metrics) = render(&mut app, 60, 16);
        assert!(
            rows.iter().any(|r| r.contains("s39")),
            "the deep selection must be scrolled into view:\n{}",
            rows.join("\n")
        );
        assert!(metrics.picker_offset > 0, "a long list must scroll");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn modal_fits_a_long_command_without_overflowing() {
        let mut app = awaiting_approval(
            "find . -type f -name '*.rs' -exec grep -l TODO {} \\; | sort | uniq -c | head -40",
        );
        let (rows, _) = render(&mut app, 60, 20);
        for row in &rows {
            assert!(row.chars().count() <= 60, "row overflows width: {row:?}");
        }
    }

    #[test]
    fn command_result_is_rendered_with_its_exit_status() {
        let mut app = awaiting_approval("ls");
        app.approve();
        app.push_command_result(crate::exec::CommandOutput {
            command: "ls".into(),
            exit_code: Some(0),
            stdout: "app.rs\nui.rs".into(),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
            cancelled: false,
        });

        let (rows, _) = render(&mut app, 60, 20);
        let screen = transcript_only(&rows);
        assert!(screen.contains("result"), "missing result label:\n{screen}");
        assert!(screen.contains("exit 0"), "missing exit status:\n{screen}");
        assert!(screen.contains("app.rs"), "missing stdout:\n{screen}");
    }

    #[test]
    fn denied_command_is_shown_in_the_transcript() {
        let mut app = awaiting_approval("rm -rf /");
        app.deny();
        let (rows, _) = render(&mut app, 60, 18);
        let screen = transcript_only(&rows);
        assert!(screen.contains("denied"), "missing denied label:\n{screen}");
        assert!(screen.contains("rm -rf /"), "missing command:\n{screen}");
    }

    #[test]
    fn transcript_shows_the_typed_prompt_not_the_query_tag() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("list files");
        app.submit().unwrap();

        let (rows, _) = render(&mut app, 60, 14);
        let screen = transcript_only(&rows);
        assert!(screen.contains("list files"));
        assert!(
            !screen.contains("ai-harness-query"),
            "the wrapper is an implementation detail:\n{screen}"
        );
    }

    #[test]
    fn malformed_reply_is_shown_with_its_raw_text() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_response("Sure, I can help with that!".into(), None);

        let (rows, _) = render(&mut app, 70, 16);
        let screen = transcript_only(&rows);
        assert!(
            screen.contains("protocol error"),
            "missing protocol error header:\n{screen}"
        );
        assert!(
            screen.contains("Sure, I can help"),
            "raw reply should be visible for debugging:\n{screen}"
        );
    }

    #[test]
    fn long_transcript_sticks_to_the_bottom() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        for i in 0..30 {
            app.transcript.push(Entry::User(format!("message {i}")));
        }
        let (rows, metrics) = render(&mut app, 40, 12);

        assert!(
            metrics.max_scroll() > 0,
            "content should overflow the viewport"
        );
        assert_eq!(
            app.scroll,
            metrics.max_scroll(),
            "follow mode should pin to the bottom"
        );
        assert!(
            rows.join("\n").contains("message 29"),
            "newest message should be visible:\n{}",
            rows.join("\n")
        );
    }

    #[test]
    fn scrolled_back_view_shows_older_content_and_is_clamped() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        for i in 0..30 {
            app.transcript.push(Entry::User(format!("message {i}")));
        }
        // Establish metrics, then scroll to the very top.
        let (_, metrics) = render(&mut app, 40, 12);
        app.scroll_up(metrics.max_scroll());

        let (rows, _) = render(&mut app, 40, 12);
        let screen = rows.join("\n");
        assert_eq!(app.scroll, 0);
        assert!(
            screen.contains("message 0"),
            "oldest message should be visible:\n{screen}"
        );
        assert!(
            !screen.contains("message 29"),
            "newest should be off-screen:\n{screen}"
        );
    }

    #[test]
    fn narrow_terminal_wraps_instead_of_overflowing() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.transcript.push(Entry::User(
            "the quick brown fox jumps over the lazy dog".into(),
        ));
        let (rows, _) = render(&mut app, 24, 12);
        for row in &rows {
            assert!(row.chars().count() <= 24, "row overflows width: {row:?}");
        }
        assert!(rows.join("\n").contains("quick"));
    }

    #[test]
    fn waiting_state_shows_the_spinner() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        let (rows, _) = render(&mut app, 40, 12);
        let screen = rows.join("\n");
        assert!(
            screen.contains("thinking"),
            "expected spinner while waiting:\n{screen}"
        );
        assert!(
            screen.contains("waiting"),
            "expected waiting status:\n{screen}"
        );
    }
}
