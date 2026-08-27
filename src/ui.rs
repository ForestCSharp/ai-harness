//! Rendering. The prompt is pinned to the bottom; the transcript takes the rest.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, Choice, Direction, Entry, Openness, Pending, Status};
use crate::diff::Change;
use crate::highlight;
use crate::protocol::Action;
use crate::wrap;

/// Prompt box grows with the text, up to this many rows of content.
const MAX_INPUT_ROWS: u16 = 10;
/// Rows of transcript the bottom panel must leave behind.
///
/// The point of docking the modals rather than floating them is that you can
/// still read what you are deciding about, so a tall one gives way rather than
/// squeezing the conversation to nothing.
const MIN_TRANSCRIPT_ROWS: u16 = 6;
/// Smallest usable panel: borders, one row of content, one of footer.
const MIN_PANEL_ROWS: u16 = 4;
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// The highlight on a selected row, wherever there is a list to select in.
///
/// One style rather than one per list. It is the same idea every time — "this is
/// the one" — and it was written out six times, in two different colours, which
/// made the same answer look like different answers. What the panels are *for*
/// is already carried by their border colour; the bar only has to be legible.
///
/// Black on white rather than black on a hue: it has to stay readable across
/// terminal themes, and a saturated background leaves too little contrast
/// against black text on several of the common ones.
fn selected_row() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::White)
        .add_modifier(Modifier::BOLD)
}

/// The dim half of a row — the model beside a name, the price beside an id.
///
/// It has to carry the highlight's background when the row is selected, or the
/// bar stops partway across and reads as ragged rather than as a selected row.
/// Only the foreground dims, which is what "dimmer than the name" meant all
/// along. `model_rows` avoids the question entirely by building one span; where
/// the aside has to be its own span, this is how.
fn selected_aside(focused: bool) -> Style {
    if focused {
        selected_row().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// How the last frame was laid out. The event loop needs this to clamp
/// scrolling to the content that was actually rendered.
#[derive(Debug, Default, Clone)]
pub struct Metrics {
    pub transcript_height: u16,
    pub content_height: u16,
    /// Button areas from the approval modal, for mouse hit-testing. `None` when
    /// the modal is not up.
    pub allow_button: Option<Rect>,
    pub deny_button: Option<Rect>,
    /// The session picker's row area, and which session each rendered row
    /// belongs to. `None` when the picker is closed.
    ///
    /// A table rather than an offset because a picker entry spans several rows:
    /// a click on a preview line or a divider has to select that entry, which
    /// adding a first-visible-index to a row number cannot express.
    pub picker_list: Option<Rect>,
    pub picker_rows: Vec<usize>,
    /// The same, for the model's question modal.
    pub question_list: Option<Rect>,
    pub question_offset: usize,
    /// The same, for the `/model` picker. Uniform rows, so an offset is enough.
    pub models_list: Option<Rect>,
    pub models_offset: usize,
    /// The same again, for the `/rewind` list.
    pub rewind_list: Option<Rect>,
    pub rewind_offset: usize,
    /// And for the sessions view, which is a screen rather than a panel. A row
    /// map rather than an offset, since a session's entry spans several rows —
    /// the same reason `picker_rows` is a table.
    pub sessions_list: Option<Rect>,
    pub sessions_rows: Vec<usize>,
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

/// Draw the focused session.
///
/// `sessions` is `(how many, how many waiting on a person)`, for the status bar:
/// the other conversations are invisible from here, and a reply waiting in one
/// of them should not be discovered by chance.
pub fn draw(
    frame: &mut Frame,
    app: &mut App,
    cache: &mut TranscriptCache,
    sessions: (usize, usize),
) -> Metrics {
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

    // Whatever the harness needs from you takes the prompt's place rather than
    // floating over the transcript: it is the same slot because it is the same
    // thing — where you answer. Built before the split, since the layout needs
    // its height, and the height comes from its contents.
    //
    // The menu and a panel never coexist: the menu only opens while typing a
    // slash command, and every panel owns the keyboard.
    let panel = prepare_panel(app, area);
    let panel_rows = panel.as_ref().map_or(input_rows + 2, |p| p.height);

    // `Min(0)`, not `Min(1)`: the session picker takes the whole screen, and the
    // transcript is not what you are reading while you choose which conversation
    // to be in. Every other panel is capped well short of that, so the floor
    // this gives up is one nothing else was standing on.
    let [transcript_area, menu_area, status_area, panel_area] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(menu_rows),
        Constraint::Length(1),
        Constraint::Length(panel_rows),
    ])
    .areas(area);

    let mut metrics = draw_transcript(frame, app, cache, transcript_area);
    if menu_rows > 0 {
        draw_completions(frame, &completions, app.completion_index(), menu_area);
    }
    draw_status(frame, app, status_area, sessions);

    match panel {
        Some(panel) => draw_prepared_panel(frame, app, panel, panel_area, &mut metrics),
        None => draw_input(frame, panel_area, &input_layout, input_rows),
    }
    metrics
}

/// Which panel is showing, for the geometry each one reports back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PanelKind {
    Approval,
    Question,
    Picker,
    Execute,
    Undo,
    Rewind,
    Models,
    Stats,
}

/// A panel measured and laid out, ready to be drawn into the bottom slot.
struct Panel {
    kind: PanelKind,
    title: &'static str,
    colour: Color,
    body: Vec<Line<'static>>,
    /// The footer line. `None` leaves it to the caller, which is how the
    /// approval panel puts its buttons there.
    hint: Option<String>,
    height: u16,
    /// Rows of `body` before the selectable list starts, so a click can be
    /// mapped back to a row.
    header: u16,
    /// Index of the first visible list row, for uniform-height lists.
    offset: usize,
    /// For the picker, which session each rendered row belongs to. Empty for
    /// panels whose rows map to entries one-for-one.
    owners: Vec<usize>,
}

/// Build the bottom panel, or `None` when the prompt has the slot.
///
/// Heights are derived from content and then capped, the same way the prompt
/// grows to fit and stops at [`MAX_INPUT_ROWS`]. A list that no longer fits
/// scrolls a window around the selection rather than pushing the transcript out.
fn prepare_panel(app: &App, area: Rect) -> Option<Panel> {
    // Borders take two columns, and the content is padded by one either side.
    let inner_width = area.width.saturating_sub(4).max(1) as usize;
    let max = area
        .height
        .saturating_sub(MIN_TRANSCRIPT_ROWS + 1)
        .max(MIN_PANEL_ROWS);

    if let Some(pending) = app.pending() {
        let (prompt, title, body) = approval_body(pending, inner_width);
        let mut lines = vec![
            Line::from(Span::styled(prompt, Style::default().fg(Color::Gray))),
            Line::default(),
        ];
        lines.extend(body);
        // A blank before the footer, so the buttons are not flush against what
        // they are deciding about. The old centred box got this from its slack.
        lines.push(Line::default());
        // Borders (2) + footer (1).
        let height = (lines.len() as u16 + 3).min(max).max(MIN_PANEL_ROWS);
        return Some(Panel {
            kind: PanelKind::Approval,
            title,
            colour: Color::Yellow,
            body: lines,
            hint: None,
            height,
            header: 0,
            offset: 0,
            owners: Vec::new(),
        });
    }

    if let Some(question) = app.question() {
        let asked = body_lines(&question.text, Style::default(), inner_width);
        // Borders (2) + footer (1) + the question + a blank after it.
        let chrome = asked.len() as u16 + 4;
        let height = (chrome + question.rows() as u16)
            .min(max)
            .max(chrome + 1)
            .max(MIN_PANEL_ROWS);
        let visible = height.saturating_sub(chrome).max(1) as usize;
        let offset = question
            .selected
            .saturating_sub(visible.saturating_sub(1))
            .min(question.rows().saturating_sub(visible));

        let header = asked.len() as u16 + 1;
        let mut body = asked;
        body.push(Line::default());
        body.extend(question_rows(question, offset, visible));

        return Some(Panel {
            kind: PanelKind::Question,
            title: " the model is asking ",
            colour: Color::Yellow,
            body,
            hint: Some(
                if question.on_other() {
                    "type your answer · Enter send · ↑/↓ choose · Esc dismiss"
                } else {
                    "j/k or ↑/↓ or 1-9 choose · Enter answer · Esc dismiss"
                }
                .into(),
            ),
            height,
            header,
            offset,
            owners: Vec::new(),
        });
    }

    if let Some(undo) = app.pending_undo() {
        let dim = Style::default().fg(Color::Gray);
        let mut lines = body_lines(
            &format!("Undo turn {}: {}", undo.turn, undo.prompt),
            dim,
            inner_width,
        );
        lines.push(Line::default());

        // Restores and deletions are listed apart and labelled differently. They
        // are not the same promise: one puts bytes back, the other takes a file
        // away, and a single merged list would let the second hide in the first.
        for (label, files, colour) in [
            ("restore", &undo.plan.restored, Color::Green),
            ("delete", &undo.plan.removed, Color::Red),
        ] {
            if files.is_empty() {
                continue;
            }
            lines.push(Line::from(Span::styled(
                format!("{label} {} file(s):", files.len()),
                Style::default().fg(colour).add_modifier(Modifier::BOLD),
            )));
            for path in files.iter().take(UNDO_FILES_SHOWN) {
                lines.push(Line::from(Span::styled(
                    format!("  {}", truncate(path, inner_width.saturating_sub(2))),
                    Style::default().fg(colour),
                )));
            }
            if let Some(rest) = files.len().checked_sub(UNDO_FILES_SHOWN).filter(|n| *n > 0) {
                lines.push(Line::from(Span::styled(
                    format!("  ⋯ and {rest} more"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            lines.push(Line::default());
        }

        if let Some(reason) = &undo.partial {
            lines.extend(body_lines(
                &format!(
                    "This checkpoint is partial — the workspace was {reason} when it \
                     was taken, so some changes cannot be undone."
                ),
                Style::default().fg(Color::Yellow),
                inner_width,
            ));
            lines.push(Line::default());
        }
        lines.extend(body_lines(
            "The conversation rewinds to before this turn as well, so the model \
             stops believing it made these changes.",
            dim,
            inner_width,
        ));
        lines.push(Line::default());

        let height = (lines.len() as u16 + 3).min(max).max(MIN_PANEL_ROWS);
        return Some(Panel {
            kind: PanelKind::Undo,
            title: " undo this turn? ",
            colour: Color::Yellow,
            body: lines,
            hint: None,
            height,
            header: 0,
            offset: 0,
            owners: Vec::new(),
        });
    }

    if app.executing().is_some() {
        // The plan itself is in the transcript above, rendered as markdown, so
        // this asks the question and names the file rather than repeating it.
        let plan = app
            .plan_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let mut lines = vec![
            Line::from(Span::styled(
                "The plan is ready:",
                Style::default().fg(Color::Gray),
            )),
            Line::default(),
        ];
        lines.extend(body_lines(
            &plan,
            Style::default().add_modifier(Modifier::BOLD),
            inner_width,
        ));
        lines.push(Line::default());
        lines.extend(body_lines(
            "Executing leaves plan mode, lifting the write restriction.",
            Style::default().fg(Color::Gray),
            inner_width,
        ));
        lines.push(Line::default());
        let height = (lines.len() as u16 + 3).min(max).max(MIN_PANEL_ROWS);
        return Some(Panel {
            kind: PanelKind::Execute,
            title: " execute this plan? ",
            colour: Color::Green,
            body: lines,
            hint: None,
            height,
            header: 0,
            offset: 0,
            owners: Vec::new(),
        });
    }
    if let Some(rewind) = app.rewind() {
        // Borders (2) + footer (1) + the summary row + a blank after it: the
        // model picker's shape, with the summary where its query row goes.
        let chrome = 5u16;
        let height = (chrome + rewind.rows.len() as u16)
            .min(max)
            .max(MIN_PANEL_ROWS);
        let visible = height.saturating_sub(chrome).max(1) as usize;
        // The selection opens on the last row, so this opens scrolled to the
        // bottom — the newest prompt — with older ones above, as in the
        // transcript.
        let offset = rewind
            .selected
            .saturating_sub(visible.saturating_sub(1))
            .min(rewind.rows.len().saturating_sub(visible));

        let (turns, plan) = app.rewind_plan().unwrap_or_default();
        let summary = format!(
            "  undo {turns} turn(s) · {} file(s) restored · {} deleted",
            plan.restored.len(),
            plan.removed.len()
        );

        let mut body = vec![
            Line::from(Span::styled(
                summary,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::default(),
        ];
        body.extend(rewind_rows(rewind, offset, visible, inner_width));

        return Some(Panel {
            kind: PanelKind::Rewind,
            title: " rewind to ",
            colour: Color::Yellow,
            body,
            hint: Some("j/k or ↑/↓ · Enter rewind · Esc cancel".into()),
            height,
            // The summary row and the blank under it.
            header: 2,
            offset,
            owners: Vec::new(),
        });
    }

    if let Some(picker) = app.picker() {
        let matches = app.picker_matches();
        // Borders (2) + footer (1) + the query row + a blank after it, the same
        // shape the model picker has.
        let chrome = 5u16;
        // The one panel whose height is fixed rather than derived from its
        // contents. Every other panel says one thing and sizing to it keeps the
        // transcript visible; this one is a list you filter, and a list that
        // resized on every keystroke moved the row you were reading towards. So
        // it takes the whole screen bar the status line, and rows appear and
        // disappear inside a frame that does not move.
        let height = area.height.saturating_sub(1).max(MIN_PANEL_ROWS);
        let visible = height.saturating_sub(chrome).max(1) as usize;
        let (rows, owners) =
            picker_rows(picker, &matches, app.picker_index(), visible, inner_width);

        let mut body = vec![
            query_row(&picker.query, picker.searching, inner_width),
            Line::default(),
        ];
        if matches.is_empty() {
            body.push(Line::from(Span::styled(
                "  no session matches",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            body.extend(rows);
        }

        // Same list, different verb. `Enter` here either replaces the
        // conversation you are in or opens the session beside it, and the title
        // and hint are the only place that difference is visible.
        return Some(Panel {
            kind: PanelKind::Picker,
            title: if picker.open_as_new {
                " open session "
            } else {
                " load session "
            },
            colour: Color::Blue,
            body,
            hint: Some(picker_hint(picker, &matches, app.picker_index())),
            height,
            // The query row and the blank under it.
            header: 2,
            offset: 0,
            owners,
        });
    }

    if let Some(picker) = app.model_picker() {
        let matches = app.model_matches();
        let selected = app.model_index();
        // Borders (2) + footer (1) + the query row + a blank after it.
        let chrome = 5u16;
        // The catalog stands in for the list until it arrives, so the panel is
        // the same shape whether or not the fetch has landed.
        let placeholder = match &*app.catalog {
            crate::app::Catalog::Loading => Some(vec![Line::from(Span::styled(
                "  loading models…",
                Style::default().fg(Color::DarkGray),
            ))]),
            crate::app::Catalog::Failed(error) => {
                let mut lines = body_lines(
                    &format!("  could not load the model list: {error}"),
                    Style::default().fg(Color::Red),
                    inner_width,
                );
                lines.push(Line::from(Span::styled(
                    "  /model <id> still sets it by hand.",
                    Style::default().fg(Color::DarkGray),
                )));
                Some(lines)
            }
            crate::app::Catalog::Ready(_) if matches.is_empty() => Some(vec![Line::from(
                Span::styled("  no model matches", Style::default().fg(Color::DarkGray)),
            )]),
            crate::app::Catalog::Ready(_) => None,
        };

        let wanted = placeholder.as_ref().map_or(matches.len(), Vec::len);
        let height = (chrome + wanted as u16).min(max).max(MIN_PANEL_ROWS);
        let visible = height.saturating_sub(chrome).max(1) as usize;
        let offset = selected
            .saturating_sub(visible.saturating_sub(1))
            .min(matches.len().saturating_sub(visible));

        let mut body = vec![
            query_row(&picker.query, picker.searching, inner_width),
            Line::default(),
        ];
        match placeholder {
            Some(lines) => body.extend(lines),
            None => body.extend(model_rows(&matches, selected, offset, visible, inner_width)),
        }

        return Some(Panel {
            kind: PanelKind::Models,
            title: " choose a model ",
            colour: Color::Blue,
            body,
            hint: Some(
                if picker.searching {
                    "typing filters · Enter select · Esc back to the list"
                } else {
                    "/ search · j/k or ↑/↓ · Enter select · Esc cancel"
                }
                .into(),
            ),
            height,
            // The query row and the blank under it.
            header: 2,
            offset,
            owners: Vec::new(),
        });
    }

    // Last, so anything that needs an answer takes the slot from it. A page you
    // are reading loses to a modal that is waiting on you — and it is still
    // there when the modal is gone.
    if app.stats_open() {
        let dim = Style::default().fg(Color::DarkGray);
        let mut lines = app.stats_lines();

        // Trim to what there is room for, then drop any heading or blank left
        // dangling by the trim, and only then measure. A short terminal cutting
        // the page is expected — the sections are ordered so the least important
        // goes first — but a heading with nothing under it reads as a bug rather
        // than as a page that ran out of room, and measuring afterwards keeps
        // the gap the popped rows would otherwise leave above the footer.
        lines.truncate(max.saturating_sub(3) as usize);
        while lines
            .last()
            .is_some_and(|line| line.is_empty() || !line.starts_with(' '))
        {
            lines.pop();
        }
        let height = (lines.len() as u16 + 3).min(max).max(MIN_PANEL_ROWS);

        let body: Vec<Line<'static>> = lines
            .into_iter()
            .map(|line| {
                // Section headings carry the weight; the numbers under them are
                // indented and read as detail, the division `picker_entry` makes
                // between a name and its aside.
                let style = if line.starts_with(' ') {
                    Style::default().fg(Color::Gray)
                } else if line.is_empty() {
                    dim
                } else {
                    Style::default().add_modifier(Modifier::BOLD)
                };
                Line::from(Span::styled(truncate(&line, inner_width), style))
            })
            .collect();
        return Some(Panel {
            kind: PanelKind::Stats,
            title: " session stats ",
            colour: Color::Blue,
            body,
            hint: Some("Esc close".into()),
            height,
            header: 0,
            offset: 0,
            owners: Vec::new(),
        });
    }

    None
}

/// Draw a prepared panel and record the geometry its clicks need.
fn draw_prepared_panel(
    frame: &mut Frame,
    app: &App,
    mut panel: Panel,
    area: Rect,
    metrics: &mut Metrics,
) {
    let kind = panel.kind;
    let header = panel.header;
    let offset = panel.offset;
    let owners = std::mem::take(&mut panel.owners);
    let (content, footer) = draw_panel(frame, area, panel);

    // The list starts below whatever header the body carries.
    let list = Rect::new(
        content.x,
        content.y + header,
        content.width,
        content.height.saturating_sub(header),
    );

    match kind {
        // Nothing to record: no list, no selection, no buttons. The one panel
        // that is only something to read.
        PanelKind::Stats => {}
        PanelKind::Approval => {
            if let Some(pending) = app.pending() {
                let (allow, deny) = draw_buttons(frame, footer, pending.selected, APPROVE_LABELS);
                metrics.allow_button = Some(allow);
                metrics.deny_button = Some(deny);
            }
        }
        // Same two-button footer as an approval, so the same rects carry the
        // clicks — the event loop tells them apart by which panel is up.
        PanelKind::Execute => {
            if let Some(selected) = app.executing() {
                let (allow, deny) = draw_buttons(frame, footer, selected, EXECUTE_LABELS);
                metrics.allow_button = Some(allow);
                metrics.deny_button = Some(deny);
            }
        }
        PanelKind::Rewind => {
            metrics.rewind_list = Some(list);
            metrics.rewind_offset = offset;
        }
        PanelKind::Undo => {
            if let Some(selected) = app.undo_choice() {
                let (allow, deny) = draw_buttons(frame, footer, selected, UNDO_LABELS);
                metrics.allow_button = Some(allow);
                metrics.deny_button = Some(deny);
            }
        }
        PanelKind::Question => {
            metrics.question_list = Some(list);
            metrics.question_offset = offset;
            // A real cursor, the same call `draw_input` makes. A panel in the
            // prompt's slot with the terminal cursor sitting in it reads as the
            // prompt having grown, which is the whole point.
            if let Some(question) = app.question()
                && question.on_other()
            {
                let row = question.selected.saturating_sub(offset) as u16;
                if row < list.height {
                    let col = question.other.layout(list.width.saturating_sub(2)).cursor.1;
                    frame.set_cursor_position(Position::new(list.x + 2 + col, list.y + row));
                }
            }
        }
        PanelKind::Picker => {
            metrics.picker_list = Some(list);
            metrics.picker_rows = owners;
            // The cursor sits in the query row above the list, the same as the
            // model picker's — see the `Models` arm below.
            // Only while searching: a cursor in a row nobody is typing into
            // points at the wrong thing.
            // Column 3, not 2: `query_row` spends two cells on the indent and one
            // on the `/`, so that is where the next character lands.
            if let Some(picker) = app.picker().filter(|p| p.searching) {
                let col = picker
                    .query
                    .layout(content.width.saturating_sub(3))
                    .cursor
                    .1;
                frame.set_cursor_position(Position::new(content.x + 3 + col, content.y));
            }
        }
        PanelKind::Models => {
            metrics.models_list = Some(list);
            metrics.models_offset = offset;
            // The cursor lives in the query row, which sits in the header above
            // the list — the same trick the question panel uses for its
            // free-text row, so typing here reads as typing at the prompt.
            if let Some(picker) = app.model_picker().filter(|p| p.searching) {
                let col = picker
                    .query
                    .layout(content.width.saturating_sub(3))
                    .cursor
                    .1;
                frame.set_cursor_position(Position::new(content.x + 3 + col, content.y));
            }
        }
    }
}

/// Draw the bottom panel: the input box grown, not a window opened.
///
/// Same bordered block in the same slot, with nothing centred and nothing
/// cleared behind it — the transcript above stays readable, which is the reason
/// these are not overlays. Returns the content and footer rects.
fn draw_panel(frame: &mut Frame, area: Rect, panel: Panel) -> (Rect, Rect) {
    let block = Block::bordered()
        .title(Line::from(panel.title).bold())
        .border_style(Style::default().fg(panel.colour));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [content, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    frame.render_widget(Paragraph::new(Text::from(panel.body)), content);

    if let Some(hint) = panel.hint {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(Color::DarkGray),
            ))),
            footer,
        );
    }
    (content, footer)
}

/// The picker's footer, which says what `Enter` will do to the highlighted row.
///
/// Dynamic rather than a fixed legend because the answer genuinely differs per
/// row: a running session is somewhere to go, a saved one is something to load
/// or open, and the one you are in is neither. A static footer would have to
/// describe all three and so describe none.
fn picker_hint(picker: &crate::app::Picker, matches: &[usize], index: usize) -> String {
    let openness = matches
        .get(index)
        .and_then(|&i| picker.open.get(i).copied())
        .unwrap_or(Openness::Closed);
    let verb = match openness {
        Openness::Current => "● you are here",
        Openness::Open => "● running · Enter switches to it",
        Openness::Closed if picker.open_as_new => "Enter open",
        Openness::Closed => "Enter load",
    };
    if picker.searching {
        format!("typing filters · {verb} · Esc back to the list")
    } else {
        format!("/ search · j/k or ↑/↓ · {verb} · Esc cancel")
    }
}

/// Rendered rows for one session: its name, a rule, and its last few lines.
///
/// A blank row trails each entry, because with three lines under every name a
/// flat run of rows stops being scannable — the gap is what makes an entry read
/// as one thing.
fn picker_entry(
    name: &str,
    model: &str,
    lines: &[String],
    focused: bool,
    open: Openness,
    width: usize,
) -> Vec<Line<'static>> {
    let title = if focused {
        selected_row()
    } else {
        Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::BOLD)
    };
    // Dimmer than the name, on the same reasoning as the preview lines below:
    // the name is what you are choosing, the model is what you are choosing
    // between. Only slightly, though — loading adopts it, so it is not trivia.
    let aside = selected_aside(focused);
    let marker = if focused { "› " } else { "  " };
    // Two leading columns, focus then state — the shape `session_entry` already
    // has, so the picker and the sessions view read the same way. The dot aligns
    // down the list, which is what makes "which of these are running" a glance
    // rather than a read.
    let mark = match open {
        Openness::Closed => "  ",
        Openness::Open | Openness::Current => "● ",
    };
    // `‹current›` where the sessions view puts it, and for the same reason.
    let here = if open == Openness::Current {
        "  ‹current›"
    } else {
        ""
    };

    // The name gives way to the model rather than the other way round: a name
    // truncated in the middle of a timestamp is still recognisable, a price of
    // admission cut in half is not. Both leading columns and the suffix come out
    // of its budget, or a wide model pushes the row past the panel.
    let head = marker.chars().count() + mark.chars().count() + here.chars().count();
    let room = width.saturating_sub(model.chars().count() + head + 1);
    let shown = truncate(name, room.max(1));
    let mut title_row = vec![
        Span::styled(marker.to_string(), title),
        // Coloured rather than merely drawn, so it reads as a state and not as
        // punctuation. Green for "running", the yellow having been spent on
        // "wants you" in the sessions view.
        Span::styled(
            mark.to_string(),
            if focused {
                title
            } else {
                Style::default().fg(Color::Green)
            },
        ),
        Span::styled(format!("{shown}{here}"), title),
    ];
    if !model.is_empty() {
        let gap = width
            .saturating_sub(head + shown.chars().count() + model.chars().count())
            .max(1);
        title_row.push(Span::styled(format!("{}{model}", " ".repeat(gap)), aside));
    }

    let mut rows = vec![Line::from(title_row)];
    if !lines.is_empty() {
        rows.push(Line::from(Span::styled(
            format!("  {}", "─".repeat(width.saturating_sub(2))),
            Style::default().fg(Color::DarkGray),
        )));
        // Dimmer than the name throughout, so the eye lands on names first and
        // the previews read as detail under them.
        let body = if focused {
            Color::Gray
        } else {
            Color::DarkGray
        };
        for line in lines {
            // Truncated to the panel, not just to what the file stores: one is
            // bounded for size and the other for the screen, and they are
            // different numbers — without this a long line runs into the border.
            rows.push(Line::from(Span::styled(
                format!("  {}", truncate(line, width.saturating_sub(2))),
                Style::default().fg(body),
            )));
        }
    }
    rows.push(Line::default());
    rows
}

/// Lay out the picker to fit `visible` rows, keeping the selection on screen.
///
/// `matches` are the sessions the query left, and `selected` is a position in
/// *that* list. Returns the rows and, for each of them, the position it belongs
/// to — entries are no longer one row tall, so a click has to be mapped back
/// through a table rather than by adding an offset. Positions rather than
/// session indices so a click reaches [`crate::app::App::picker_select`]
/// unchanged, exactly as the model picker's rows do.
fn picker_rows(
    picker: &crate::app::Picker,
    matches: &[usize],
    selected: usize,
    visible: usize,
    width: usize,
) -> (Vec<Line<'static>>, Vec<usize>) {
    // Nothing matched: the caller renders a placeholder instead, and the walk
    // below would index an empty list.
    if matches.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let entries: Vec<Vec<Line<'static>>> = matches
        .iter()
        .enumerate()
        .map(|(pos, &i)| {
            let name = &picker.sessions[i];
            let lines = picker.previews.get(i).map(Vec::as_slice).unwrap_or(&[]);
            let model = picker.models.get(i).map_or("", String::as_str);
            let open = picker.open.get(i).copied().unwrap_or(Openness::Closed);
            picker_entry(name, model, lines, pos == selected, open, width)
        })
        .collect();

    // Walk back from the selection while the entries still fit, so the selection
    // is visible by construction rather than by arithmetic that assumed every
    // entry was the same height.
    let mut first = selected;
    let mut used = entries[selected].len();
    while first > 0 {
        let next = used + entries[first - 1].len();
        if next > visible {
            break;
        }
        used = next;
        first -= 1;
    }

    let mut rows = Vec::new();
    let mut owners = Vec::new();
    for (i, entry) in entries.iter().enumerate().skip(first) {
        for line in entry {
            if rows.len() == visible {
                return (rows, owners);
            }
            rows.push(line.clone());
            owners.push(i);
        }
    }
    (rows, owners)
}

/// One row per visible choice, plus the free-text row at the end.
fn question_rows(
    question: &crate::app::Question,
    offset: usize,
    visible: usize,
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for i in offset..question.rows().min(offset + visible) {
        let focused = i == question.selected;
        let style = if focused {
            selected_row()
        } else {
            Style::default().fg(Color::Gray)
        };
        let marker = if focused { "› " } else { "  " };
        let label = match question.choices.get(i) {
            // Numbered so a single keypress can pick one.
            Some(choice) => format!("{marker}{}. {choice}", i + 1),
            // Focused, this row is an editor and the terminal cursor sits in
            // it; unfocused it has to read as an invitation.
            None if focused => format!("{marker}{}", question.other.text()),
            None => format!("{marker}something else…"),
        };
        rows.push(Line::from(Span::styled(label, style)));
    }
    rows
}

/// The model picker's query row: what has been typed, or the invitation to type.
///
/// The terminal cursor sits in this row (see [`draw_prepared_panel`]), so it
/// reads as the prompt with a list under it rather than a box with a search
/// field bolted on.
fn query_row(query: &crate::input::Input, searching: bool, width: usize) -> Line<'static> {
    let text = query.text();
    let dim = Style::default().fg(Color::DarkGray);
    // The `/` is shown while searching, the way a pager shows it, so the row
    // says which mode the keyboard is in rather than leaving it to be discovered
    // by pressing a letter and watching what happens.
    if searching {
        return Line::from(vec![
            Span::styled("  /", Style::default().fg(Color::Yellow)),
            Span::styled(
                truncate(text, width.saturating_sub(3)),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]);
    }
    if text.is_empty() {
        return Line::from(Span::styled("  / to search", dim));
    }
    // A filter still in force while navigating: shown, so a list that is short
    // for a reason does not look like a list that is short.
    Line::from(vec![
        Span::styled("  /", dim),
        Span::styled(
            truncate(text, width.saturating_sub(3)),
            Style::default().fg(Color::Gray),
        ),
        Span::styled("  · / to edit", dim),
    ])
}

/// One row per visible rewind point: the prompt, and what that turn changed.
///
/// The prompt is what you are choosing between, so the file count is dim and
/// right-aligned beside it — the same division `model_rows` makes between an id
/// and its price.
fn rewind_rows(
    rewind: &crate::app::Rewind,
    offset: usize,
    visible: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for i in offset..rewind.rows.len().min(offset + visible) {
        let row = &rewind.rows[i];
        let focused = i == rewind.selected;
        let style = if focused {
            selected_row()
        } else {
            Style::default().fg(Color::Gray)
        };
        let aside = selected_aside(focused);
        let note = match row.changed {
            0 => String::new(),
            n => format!("{n} file(s)  "),
        };
        let marker = if focused { "› " } else { "  " };
        let room = width.saturating_sub(note.chars().count() + 2);
        let prompt = truncate(row.prompt.trim(), room);
        let pad = width
            .saturating_sub(prompt.chars().count() + note.chars().count() + 2)
            .max(1);
        rows.push(Line::from(vec![
            Span::styled(format!("{marker}{prompt}"), style),
            Span::styled(" ".repeat(pad), style),
            Span::styled(note, aside),
        ]));
    }
    rows
}

/// One row per visible model: the id, and what it costs to use.
///
/// The metadata is right-aligned and dim so the ids stay a scannable column —
/// the id is what you are choosing, the rest is what you are choosing between.
fn model_rows(
    matches: &[&crate::openrouter::ModelInfo],
    selected: usize,
    offset: usize,
    visible: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for (i, model) in matches.iter().enumerate().skip(offset).take(visible) {
        let focused = i == selected;
        let style = if focused {
            selected_row()
        } else {
            Style::default().fg(Color::Gray)
        };
        let marker = if focused { "› " } else { "  " };
        let detail = model_detail(model);

        // Truncate the id first, so a long one loses its own tail rather than
        // pushing the price off the edge.
        let room = width.saturating_sub(detail.chars().count() + 3);
        let id = truncate(&model.id, room.max(1));
        let gap = width
            .saturating_sub(2 + id.chars().count() + detail.chars().count())
            .max(1);
        let label = format!("{marker}{id}{}{detail}", " ".repeat(gap));

        // One span so the highlight covers the whole row, including the gap.
        rows.push(Line::from(Span::styled(label, style)));
    }
    rows
}

/// Context window and price, as one dim column: `200K   $5.00/$25.00`.
fn model_detail(model: &crate::openrouter::ModelInfo) -> String {
    let context = match model.context_length {
        Some(n) if n >= 1_000_000 => format!("{}M", n / 1_000_000),
        Some(n) if n >= 1_000 => format!("{}K", n / 1_000),
        Some(n) => n.to_string(),
        None => String::new(),
    };
    let price = match model.price_per_million() {
        // A free model is worth saying outright rather than as two zeroes.
        Some((0.0, 0.0)) => "free".to_string(),
        Some((prompt, completion)) => format!("${prompt:.2}/${completion:.2}"),
        None => String::new(),
    };
    match (context.is_empty(), price.is_empty()) {
        (true, true) => String::new(),
        (false, false) => format!("{context}  {price}"),
        _ => format!("{context}{price}"),
    }
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
                selected_row()
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

/// A modal body headed by the file path, which is the thing being decided about.
fn path_then(path: &str, rest: Vec<Line<'static>>) -> Vec<Line<'static>> {
    std::iter::once(Line::from(Span::styled(
        path.to_string(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )))
    .chain(rest)
    .collect()
}

/// What is being approved: the lead-in line, the panel title, and the body.
///
/// A command is shown in full; a write or edit as its path plus the same diff
/// the transcript will show, so what you approve and what you scroll back to
/// cannot differ.
fn approval_body(
    pending: &Pending,
    inner_width: usize,
) -> (&'static str, &'static str, Vec<Line<'static>>) {
    let (prompt, title, body): (&str, &str, Vec<Line>) = match &pending.action {
        // Only reachable under `--confirm-reads`; reads are otherwise silent.
        Action::Read {
            path,
            offset,
            limit,
        } => (
            "The model wants to read:",
            " read this file? ",
            vec![Line::from(Span::styled(
                crate::protocol::read_label(path, *offset, *limit),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ))],
        ),
        // Only reachable under `--confirm-reads`, like a read. The pattern is
        // all there is to show: which files it will open is not knowable until
        // the walk runs.
        Action::Grep { pattern, dir, glob } => (
            "The model wants to search for:",
            " run this search? ",
            vec![Line::from(Span::styled(
                crate::protocol::search_label(pattern, dir.as_deref(), glob.as_deref()),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ))],
        ),
        Action::Glob { pattern, dir } => (
            "The model wants to list files matching:",
            " run this search? ",
            vec![Line::from(Span::styled(
                crate::protocol::search_label(pattern, dir.as_deref(), None),
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
        // The same `code_block` the transcript uses, fed the same pre-flight
        // diff, so what you approve and what you scroll back to cannot differ.
        Action::Write { path, contents } => (
            "The model wants to write:",
            " write this file? ",
            path_then(
                path,
                code_block(
                    highlight::detect(path),
                    match &pending.diff {
                        Some(changes) => CodeBody::Diff(changes),
                        None => CodeBody::Contents(contents),
                    },
                    inner_width,
                ),
            ),
        ),
        Action::Edit { path, new, .. } => {
            let body = match &pending.diff {
                Some(changes) => CodeBody::Diff(changes),
                None => CodeBody::Contents(new),
            };
            (
                "The model wants to edit:",
                " apply this edit? ",
                path_then(path, code_block(highlight::detect(path), body, inner_width)),
            )
        }
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

    (prompt, title, body)
}

/// Labels for the two-button footer: the accepting one, then the refusing one.
/// Padding is part of the label so each panel controls how wide its buttons sit.
type Labels = [&'static str; 2];
const APPROVE_LABELS: Labels = ["  Allow  ", "  Deny  "];
const EXECUTE_LABELS: Labels = ["  Execute  ", "  Keep planning  "];
const UNDO_LABELS: Labels = ["  Undo  ", "  Cancel  "];

/// Paths listed per group in the undo panel before the rest are summarised. A
/// turn that touched forty files needs to be recognisable, not enumerated.
const UNDO_FILES_SHOWN: usize = 6;

/// The two buttons on a panel's footer row. Returns their areas so the event loop
/// can hit-test mouse clicks against them.
///
/// Widths come from the labels rather than a constant, because "Keep planning" is
/// not "Deny" — and are clamped to the row, so a narrow terminal clips the text
/// instead of drawing buttons outside the panel.
fn draw_buttons(
    frame: &mut Frame,
    button_area: Rect,
    selected: Choice,
    labels: Labels,
) -> (Rect, Rect) {
    let gap = 2;
    let natural = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16 + 2;
    let button = natural
        .min(button_area.width.saturating_sub(gap) / 2)
        .max(1);
    let total = button * 2 + gap;
    let start = button_area.x + button_area.width.saturating_sub(total) / 2;
    let allow = Rect::new(start, button_area.y, button, 1);
    let deny = Rect::new(start + button + gap, button_area.y, button, 1);

    for (rect, label, choice, colour) in [
        (allow, labels[0], Choice::Allow, Color::Green),
        (deny, labels[1], Choice::Deny, Color::Red),
    ] {
        let focused = selected == choice;
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

/// Wrapped transcript rows, kept between frames.
///
/// Rendering an entry is pure in `(entry, width, debug)` and the transcript is
/// append-only, so the rows an entry produced last frame are still the rows it
/// produces this frame. Without this every frame re-parsed every markdown
/// reply, re-highlighted every code block, and re-wrapped every message in the
/// conversation — work that grew with the history and was thrown away
/// immediately. Now a frame costs what has *changed*, plus the screenful it
/// actually draws.
#[derive(Default)]
pub struct TranscriptCache {
    /// What `lines` was built for. A change to either invalidates all of it.
    width: usize,
    debug: bool,
    /// Entries already folded into `lines`.
    entries: usize,
    /// Flattened rows, blank separators included: one `Line` is one screen row.
    lines: Vec<Line<'static>>,
}

impl TranscriptCache {
    /// Bring the cache up to date with the transcript, rendering only what is
    /// new. Rebuilds from scratch when the width or debug flag changed, or when
    /// the transcript got shorter — which only `/clear` does.
    fn sync(&mut self, app: &App, width: usize) {
        if self.width != width || self.debug != app.debug || app.transcript.len() < self.entries {
            self.width = width;
            self.debug = app.debug;
            self.entries = 0;
            self.lines.clear();
        }

        for entry in &app.transcript[self.entries..] {
            // Rendered per entry so a hidden one contributes nothing at all —
            // not even the blank separator line.
            let mut block: Vec<Line> = Vec::new();
            render_entry(&app.model, app.debug, entry, width, &mut block);
            if block.is_empty() {
                continue;
            }
            if !self.lines.is_empty() {
                self.lines.push(Line::default());
            }
            self.lines.append(&mut block);
        }
        self.entries = app.transcript.len();
    }
}

/// Re-point a cached row at the frame being drawn.
///
/// The widget wants an owned `Vec<Line>`, but cloning one would deep-copy every
/// string in it and undo the caching. Only the spans are rebuilt, borrowing the
/// text that stays put in the cache — and only for rows actually on screen.
fn borrowed<'a>(line: &'a Line<'static>) -> Line<'a> {
    Line {
        style: line.style,
        alignment: line.alignment,
        spans: line
            .spans
            .iter()
            .map(|span| Span {
                style: span.style,
                content: std::borrow::Cow::Borrowed(span.content.as_ref()),
            })
            .collect(),
    }
}

fn draw_transcript(
    frame: &mut Frame,
    app: &mut App,
    cache: &mut TranscriptCache,
    area: Rect,
) -> Metrics {
    let block = Block::bordered()
        .title(Line::from(" ai-harness ").bold())
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    let width = inner.width as usize;

    cache.sync(app, width);
    // Live state changes on its own every frame, so it is rebuilt rather than
    // cached. It is a screenful at most, unlike the history above it.
    let tail = tail_lines(app, width);

    // Wrapping up front means the row count is the true rendered height, which
    // is what scroll clamping and "stick to the bottom" both depend on. The
    // cache keeps that count without re-deriving it.
    let total = cache.lines.len() + tail.len();
    let metrics = Metrics {
        transcript_height: inner.height,
        // Saturating, so a conversation past 65,535 rows pins to the bottom
        // rather than wrapping around to the top.
        content_height: u16::try_from(total).unwrap_or(u16::MAX),
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

    // Take only the rows the viewport shows. Indexed rather than skipped, so
    // scrolling to the bottom of a long transcript does not walk it first.
    let start = app.scroll as usize;
    let wanted = inner.height as usize;
    let mut visible: Vec<Line> = Vec::with_capacity(wanted);
    if start < cache.lines.len() {
        let end = (start + wanted).min(cache.lines.len());
        visible.extend(cache.lines[start..end].iter().map(borrowed));
    }
    let from_tail = start.saturating_sub(cache.lines.len());
    if visible.len() < wanted && from_tail < tail.len() {
        let end = (from_tail + wanted - visible.len()).min(tail.len());
        visible.extend(tail[from_tail..end].iter().map(borrowed));
    }

    // The slice already applied the offset, so the widget scrolls no further.
    let paragraph = Paragraph::new(Text::from(visible)).block(block);
    frame.render_widget(paragraph, area);
    metrics
}

/// The rows below the transcript proper: what is happening right now, and the
/// opening hint when nothing has happened yet. Rebuilt every frame by design.
fn tail_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    if app.transcript.is_empty() {
        lines.extend(body_lines(
            "Type a prompt and press Enter. Alt+Enter inserts a newline; Ctrl+C twice quits.",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
            width,
        ));
    }

    // Reasoning sits above the reply, in the order the two arrive: the model
    // thinks, then answers. It is live state like the running window below —
    // shown while it is happening and gone when the turn ends.
    if let Some(reasoning) = app.reasoning.as_ref().filter(|_| app.show_reasoning) {
        lines.push(Line::default());
        lines.extend(reasoning_window(app, reasoning, width));
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
        let mut body = body_lines(text, Style::default(), width);
        // A block cursor on the last line signals the reply is still arriving.
        // Appended to the wrapped row rather than to the text, so a reply that
        // is already at the width does not re-wrap on the strength of it.
        match body.last_mut() {
            Some(last) => last.spans.push(Span::raw("▌")),
            None => lines.push(Line::from(Span::raw("▌"))),
        }
        lines.append(&mut body);
    } else if let Some(running) = &app.running {
        // Live state, not a transcript entry: it belongs after the entries and
        // is replaced by the real `Entry::CommandResult` when the command exits.
        lines.push(Line::default());
        lines.extend(running_window(app, running, width));
    } else {
        let activity = match app.status {
            Status::Waiting => Some("thinking…"),
            Status::Running => Some("running…"),
            Status::Compacting => Some("compacting…"),
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

/// Output lines shown in the running window. Enough to see what a command is
/// doing without the window swallowing the transcript above it.
const RUNNING_WINDOW_ROWS: usize = 12;

/// The live command window: an outlined box that fills as output arrives.
///
/// Drawn as text rather than a `Block` because it lives inside the scrolling
/// transcript paragraph, where a real widget cannot go — and because it has to
/// scroll away with the rest of the history once the command finishes.
fn running_window(
    app: &App,
    running: &crate::app::RunningCommand,
    width: usize,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let edge = Style::default().fg(Color::Yellow);
    // Border verticals take a column each, plus a space of padding inside.
    let inner = width.saturating_sub(4).max(1);
    let spinner = SPINNER[(app.tick / 2) % SPINNER.len()];

    let mut lines = Vec::new();
    let title = truncate(&running.command, inner.saturating_sub(4));
    lines.push(rule(
        vec![
            Span::styled("┌─ ", edge),
            Span::styled(format!("{spinner} "), Style::default().fg(Color::Yellow)),
            Span::styled(
                title,
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", edge),
        ],
        width,
        edge,
    ));

    // Wrap everything, then keep the last screenful: a command's newest output
    // is the part worth seeing, and wrapping first means the count is in rows
    // rather than logical lines that might each take three.
    let mut body: Vec<Line> = Vec::new();
    for (stderr, text) in running.lines() {
        let style = if stderr {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        };
        for row in wrap::text(text, inner) {
            body.push(Line::from(vec![
                Span::styled("│ ", edge),
                Span::styled(row, style),
            ]));
        }
    }
    let hidden = body.len().saturating_sub(RUNNING_WINDOW_ROWS);
    if hidden > 0 {
        body.drain(..hidden);
        lines.push(Line::from(vec![
            Span::styled("│ ", edge),
            Span::styled(format!("⋯ {hidden} earlier line(s)"), dim),
        ]));
    }
    lines.append(&mut body);

    lines.push(rule(
        vec![
            Span::styled("└─ ", edge),
            Span::styled("Esc cancels".to_string(), dim),
            Span::styled(" ", edge),
        ],
        width,
        edge,
    ));
    lines
}

/// Reasoning rows shown in the live window. Fewer than a command's: this is the
/// model working up to an answer, not the answer, and it should say what is
/// happening without taking the screen to say it.
const REASONING_WINDOW_ROWS: usize = 8;

/// The live reasoning window: what the model is thinking, while it thinks it.
///
/// The same shape as [`running_window`] — a spinner header, a capped tail, and a
/// count of what scrolled past — because it is the same problem: output that
/// arrives faster than it can be read and may be far larger than the screen.
/// Dim and italic throughout, so it reads as the margin note it is; the reply
/// renders below it in ordinary text and stays the thing you are looking at.
///
/// The text is never parsed, never sent back, and thrown away when the turn
/// ends. See [`crate::app::App::reasoning`].
fn reasoning_window(app: &App, text: &str, width: usize) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    // Border verticals take a column each, plus a space of padding inside.
    let inner = width.saturating_sub(4).max(1);
    let spinner = SPINNER[(app.tick / 2) % SPINNER.len()];

    let mut lines = vec![rule(
        vec![
            Span::styled("┌─ ", dim),
            Span::styled(format!("{spinner} "), dim),
            Span::styled("reasoning", dim.add_modifier(Modifier::BOLD)),
            Span::styled(" ", dim),
        ],
        width,
        dim,
    )];

    // Wrapped first, then capped, so the count is in rows rather than logical
    // lines that might each take three — the same reasoning as `running_window`.
    let body_style = dim.add_modifier(Modifier::ITALIC);
    let mut body: Vec<Line> = Vec::new();
    for line in text.lines() {
        for row in wrap::text(line, inner) {
            body.push(Line::from(vec![
                Span::styled("│ ", dim),
                Span::styled(row, body_style),
            ]));
        }
    }
    let hidden = body.len().saturating_sub(REASONING_WINDOW_ROWS);
    if hidden > 0 {
        body.drain(..hidden);
        lines.push(Line::from(vec![
            Span::styled("│ ", dim),
            Span::styled(format!("⋯ {hidden} earlier line(s)"), dim),
        ]));
    }
    lines.append(&mut body);

    lines.push(rule(vec![Span::styled("└─", dim)], width, dim));
    lines
}

/// Close a header or footer with a horizontal rule out to `width`, so the box
/// reads as a box rather than as a stray corner.
fn rule(mut spans: Vec<Span<'static>>, width: usize, style: Style) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;
    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    if let Some(fill) = width.checked_sub(used).filter(|f| *f > 0) {
        spans.push(Span::styled("─".repeat(fill), style));
    }
    Line::from(spans)
}

/// Shorten to `width` cells, marking the cut.
fn truncate(text: &str, width: usize) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() <= width {
        return flat;
    }
    flat.chars()
        .take(width.saturating_sub(1))
        .collect::<String>()
        + "…"
}

fn render_entry(
    model: &str,
    debug: bool,
    entry: &Entry,
    width: usize,
    lines: &mut Vec<Line<'static>>,
) {
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
        Entry::Action {
            action,
            usage,
            diff,
        } => {
            // Label the action type, since that is the whole point of the
            // protocol: the user should see which branch the model chose.
            let (label, colour) = match action {
                Action::Shell(_) => ("shell", Color::Magenta),
                // Its own label rather than "shell": the two are approved the
                // same way and differ entirely in what happens after, which is
                // exactly the kind of thing the user should not have to infer.
                Action::ShellBackground(_) => ("job", Color::Magenta),
                Action::Read { .. } => ("read", Color::Blue),
                Action::Grep { .. } => ("grep", Color::Blue),
                Action::Glob { .. } => ("glob", Color::Blue),
                Action::Fetch { .. } => ("fetch", Color::Blue),
                Action::Write { .. } => ("write", Color::Cyan),
                Action::Edit { .. } => ("edit", Color::Cyan),
                Action::Options { .. } => ("question", Color::Yellow),
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
                format!("  {model}"),
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
                Action::Shell(cmd) | Action::ShellBackground(cmd) => {
                    lines.extend(body_lines(cmd, Style::default().fg(Color::Magenta), width))
                }
                // The path is the whole action; the contents arrive as a result.
                Action::Read {
                    path,
                    offset,
                    limit,
                } => lines.extend(body_lines(
                    &crate::protocol::read_label(path, *offset, *limit),
                    Style::default().fg(Color::Blue),
                    width,
                )),
                // The pattern is the whole action; the hits arrive as a result.
                Action::Grep { pattern, dir, glob } => lines.extend(body_lines(
                    &crate::protocol::search_label(pattern, dir.as_deref(), glob.as_deref()),
                    Style::default().fg(Color::Blue),
                    width,
                )),
                Action::Glob { pattern, dir } => lines.extend(body_lines(
                    &crate::protocol::search_label(pattern, dir.as_deref(), None),
                    Style::default().fg(Color::Blue),
                    width,
                )),
                // Likewise the URL; the page text arrives as a result.
                Action::Fetch { url } => {
                    lines.extend(body_lines(url, Style::default().fg(Color::Blue), width))
                }
                // The one place markdown is rendered: this is the model writing
                // prose for a person, and prose is what it formats.
                Action::Response(text) => lines.extend(render_markdown(text, width)),
                // The question stays in the transcript once answered, so the
                // answer below it has something to refer to.
                Action::Options { question, choices } => {
                    lines.extend(body_lines(question, Style::default(), width));
                    for (i, choice) in choices.iter().enumerate() {
                        lines.extend(body_lines(
                            &format!("{}. {choice}", i + 1),
                            Style::default().fg(Color::DarkGray),
                            width,
                        ));
                    }
                }
                // A diff of what the write changes, when the pre-flight could
                // read the file it replaces; otherwise a bounded preview of the
                // new contents, which is all there is to show for a new file.
                Action::Write { path, contents } => {
                    let body = match diff {
                        Some(changes) => CodeBody::Diff(changes),
                        None => CodeBody::Contents(contents),
                    };
                    lines.extend(code_block(highlight::detect(path), body, width));
                }
                // The same stored diff a write uses. Computed once when the edit
                // arrived rather than here, because here runs every frame.
                Action::Edit { path, new, .. } => {
                    let body = match diff {
                        Some(changes) => CodeBody::Diff(changes),
                        None => CodeBody::Contents(new),
                    };
                    lines.extend(code_block(highlight::detect(path), body, width));
                }
            }
        }
        Entry::Malformed { reason, raw, .. } => {
            // Red, like every other failure in the transcript. Yellow here meant
            // this read as a warning beside the `!` in the sessions view and the
            // partial-checkpoint note, when a rejected reply is a failure: the
            // action did not happen and the turn spent a round-trip on nothing.
            //
            // The raw reply stays dim, as a failed write's body does. The reason
            // is the error; the text is the evidence for it.
            lines.push(Line::from(vec![
                Span::styled(
                    "protocol error",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {reason}"), Style::default().fg(Color::Red)),
            ]));
            lines.extend(body_lines(raw, Style::default().fg(Color::DarkGray), width));
        }
        // One rendering for both: a check is a command result, and the only
        // thing the reader needs told apart is who asked for it — the model, or
        // the project's `--check`.
        Entry::CommandResult(output) | Entry::CheckResult(output) => {
            let ok = output.succeeded();
            let label = match entry {
                Entry::CheckResult(_) => "check",
                _ => "result",
            };
            lines.push(Line::from(vec![
                Span::styled(
                    label,
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
                lines.extend(bounded_body(
                    output.stdout.trim_end(),
                    dim,
                    width,
                    MAX_OUTPUT_PREVIEW,
                ));
            }
            if !output.stderr.trim().is_empty() {
                lines.extend(bounded_body(
                    output.stderr.trim_end(),
                    Style::default().fg(Color::Red),
                    width,
                    MAX_OUTPUT_PREVIEW,
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
        Entry::SearchResult(outcome) => {
            let ok = outcome.succeeded();
            lines.push(Line::from(vec![
                Span::styled(
                    "result",
                    Style::default()
                        .fg(if ok { Color::Blue } else { Color::Red })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}  {}", outcome.pattern, outcome.summary()),
                    Style::default().fg(if ok { Color::DarkGray } else { Color::Red }),
                ),
            ]));
            match &outcome.error {
                Some(error) => {
                    lines.extend(body_lines(error, Style::default().fg(Color::Red), width))
                }
                // Shown far more generously than a read's contents, on the same
                // reasoning [`MAX_OUTPUT_PREVIEW`] already carries: a read is
                // something you asked to see and can ask for again, while a hit
                // list *is* the reason the search was run. Eight lines of it
                // would be a tease.
                None => lines.extend(bounded_body(
                    &outcome.preview(),
                    Style::default().fg(Color::DarkGray),
                    width,
                    MAX_OUTPUT_PREVIEW,
                )),
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
        Entry::Answer { text, free } => {
            let mut header = vec![Span::styled(
                "you",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )];
            // Worth marking: an answer the model never offered is a different
            // thing from one it did, both to read back and to the model.
            if *free {
                header.push(Span::styled(
                    "  (your own answer)",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            lines.push(Line::from(header));
            lines.extend(body_lines(text, Style::default(), width));
        }
        Entry::Dismissed => {
            lines.push(Line::from(Span::styled(
                "dismissed the question",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )));
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
        Entry::Frame { direction, body } if debug => {
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
            lines.extend(bounded_body(
                body,
                Style::default().fg(Color::DarkGray),
                width,
                MAX_FRAME_PREVIEW,
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

/// Wrap at most `max` lines, then say how many were left out.
///
/// Only the shown lines are wrapped. The rest are counted — a scan — but never
/// styled, allocated, or stored, which is the difference between rendering
/// eight lines of a 64 KB file and rendering the file.
fn bounded_body(contents: &str, style: Style, width: usize, max: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    let mut rest = contents.lines();
    for line in rest.by_ref().take(max) {
        lines.extend(body_lines(line, style, width));
    }
    let remaining = rest.count();
    if remaining > 0 {
        lines.push(elision(remaining, "more"));
    }
    lines
}

/// A bounded excerpt of text with no file behind it — a read's contents, a
/// fetched page — for callers whose own header already carries the size.
fn preview_body(contents: &str, width: usize) -> Vec<Line<'static>> {
    bounded_body(
        contents,
        Style::default().fg(Color::DarkGray),
        width,
        MAX_PREVIEW,
    )
}

/// Render a model response as markdown.
///
/// Only responses go through here. A read or a fetch stays literal: when you ask
/// to see a file you want its source, not a rendering of it.
fn render_markdown(source: &str, width: usize) -> Vec<Line<'static>> {
    let blocks = crate::markdown::parse(source);
    let mut lines = Vec::new();

    for (i, block) in blocks.iter().enumerate() {
        // Separate blocks, except between consecutive list items — a list reads
        // as one thing, and double-spacing it would break it apart.
        let tight = matches!(
            (blocks.get(i.wrapping_sub(1)), block),
            (
                Some(crate::markdown::Block::Item { .. }),
                crate::markdown::Block::Item { .. }
            )
        );
        if i > 0 && !tight {
            lines.push(Line::default());
        }
        match block {
            crate::markdown::Block::Heading { level, text } => {
                // The weight carries the level, so the `#` does not have to.
                let style = Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(match level {
                        1 => Color::White,
                        2 => Color::Cyan,
                        _ => Color::Blue,
                    });
                lines.extend(inline_lines(text, style, 0, width));
            }
            crate::markdown::Block::Paragraph(text) => {
                lines.extend(inline_lines(text, Style::default(), 0, width));
            }
            crate::markdown::Block::Code { language, text } => {
                let lang = highlight::from_fence(language.as_deref().unwrap_or(""));
                // Full, not a preview: the model chose to include exactly this
                // much, so eliding it would cut off the answer.
                lines.extend(code_block(lang, CodeBody::Full(text), width));
            }
            crate::markdown::Block::Item {
                depth,
                ordinal,
                text,
            } => {
                let indent = depth * 2;
                let marker = match ordinal {
                    Some(n) => format!("{n}. "),
                    None => "• ".to_string(),
                };
                let mut rows = inline_lines(
                    text,
                    Style::default(),
                    indent + marker.chars().count(),
                    width,
                );
                // Overwrite the first row's indent with the marker, so wrapped
                // continuations line up under the text rather than the bullet.
                if let Some(first) = rows.first_mut() {
                    first.spans[0] = Span::styled(
                        format!("{}{marker}", " ".repeat(indent)),
                        Style::default().fg(Color::DarkGray),
                    );
                }
                lines.extend(rows);
            }
            crate::markdown::Block::Quote(text) => {
                for line in inline_lines(text, Style::default().fg(Color::Gray), 2, width) {
                    let mut spans = line.spans;
                    spans[0] = Span::styled("│ ", Style::default().fg(Color::DarkGray));
                    lines.push(Line::from(spans));
                }
            }
            crate::markdown::Block::Rule => {
                lines.push(Line::from(Span::styled(
                    "─".repeat(width),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }
    lines
}

/// Wrap and style one run of inline markdown, indented by `indent` cells.
///
/// Every row opens with an indent span the caller may overwrite — that is how a
/// list marker sits on the first row while continuations align under the text.
fn inline_lines(source: &str, base: Style, indent: usize, width: usize) -> Vec<Line<'static>> {
    let parsed = crate::markdown::inline(source);
    let inner = width.saturating_sub(indent).max(1);
    let pad = " ".repeat(indent);

    wrap::line(&parsed.text, inner)
        .into_iter()
        .map(|row| {
            let mut spans = vec![Span::raw(pad.clone())];
            spans.extend(slice_runs(
                &row.text,
                row.start,
                &parsed.runs,
                base,
                emphasis_style,
            ));
            Line::from(spans)
        })
        .collect()
}

/// How each inline style looks. Code borrows the syntax palette's string colour
/// so an inline snippet and a fenced one do not read as different things.
fn emphasis_style(emphasis: crate::markdown::Emphasis, base: Style) -> Style {
    match emphasis {
        crate::markdown::Emphasis::Plain => base,
        crate::markdown::Emphasis::Strong => base.add_modifier(Modifier::BOLD),
        crate::markdown::Emphasis::Italic => base.add_modifier(Modifier::ITALIC),
        crate::markdown::Emphasis::Code => base.fg(Color::Yellow),
        crate::markdown::Emphasis::Link => base.fg(Color::Blue).add_modifier(Modifier::UNDERLINED),
    }
}

/// What a code block is showing.
enum CodeBody<'a> {
    /// File contents with nothing to compare against — a new file, or one that
    /// could not be read. Bounded, so a large write never floods the transcript.
    Contents(&'a str),
    /// Contents shown in full. For a fenced block in a response: the model chose
    /// to include exactly this much, so eliding it would cut off the answer.
    Full(&'a str),
    Diff(&'a [Change]),
}

/// Lines beyond which plain contents are elided. A diff arrives already bounded
/// by [`crate::diff`], which has to cap it anyway to keep session files small.
const MAX_PREVIEW: usize = 8;

/// The same, for a command's output. Far more generous than a read's preview:
/// a read is something you asked to see and can ask for again, while output is
/// usually the reason the command was run — a failing build's errors have to
/// stay on screen. The model is sent the full text either way.
const MAX_OUTPUT_PREVIEW: usize = 50;

/// The same, for a raw protocol frame under `/debug`. These duplicate content
/// shown properly elsewhere, so a glance at the shape of one is the point.
const MAX_FRAME_PREVIEW: usize = 20;

/// Width of the `+ ` / `- ` gutter, and of the indent wrapped rows sit under.
const GUTTER: usize = 2;

/// Render file contents or a diff as a labelled, syntax-highlighted block.
///
/// Takes the language rather than deriving it: a file has a path to detect from,
/// a fenced code block in a response has only its info string. One renderer
/// serves both, so a snippet looks the same wherever it appears.
fn code_block(lang: highlight::Language, body: CodeBody<'_>, width: usize) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = Vec::new();

    let summary = match &body {
        CodeBody::Contents(contents) | CodeBody::Full(contents) => {
            format!("{} line(s)", contents.lines().count())
        }
        CodeBody::Diff(changes) => {
            let (added, removed) = crate::diff::summary(changes);
            format!("+{added} -{removed}")
        }
    };
    lines.push(Line::from(Span::styled(
        format!("{} · {summary}", highlight::label(lang)),
        dim.add_modifier(Modifier::ITALIC),
    )));

    match body {
        CodeBody::Contents(contents) => {
            let mut rest = contents.lines();
            for line in rest.by_ref().take(MAX_PREVIEW) {
                lines.extend(code_line(line, lang, ' ', None, width));
            }
            // Counted, not highlighted: the elided lines cost a scan, not a
            // wrap and a tokenise apiece.
            let remaining = rest.count();
            if remaining > 0 {
                lines.push(elision(remaining, "more"));
            }
        }
        CodeBody::Full(contents) => {
            for line in contents.lines() {
                lines.extend(code_line(line, lang, ' ', None, width));
            }
        }
        CodeBody::Diff(changes) => {
            for change in changes {
                match change {
                    Change::Context(text) => lines.extend(code_line(text, lang, ' ', None, width)),
                    Change::Removed(text) => {
                        lines.extend(code_line(text, lang, '-', Some(Color::Red), width))
                    }
                    Change::Added(text) => {
                        lines.extend(code_line(text, lang, '+', Some(Color::Green), width))
                    }
                    Change::Elided(n) => lines.push(elision(*n, "unchanged")),
                }
            }
        }
    }
    lines
}

fn elision(count: usize, what: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  ⋯ {count} {what} line(s)"),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    ))
}

/// One source line: a gutter marker, then the line highlighted and wrapped.
///
/// `marker_colour` is `Some` for a changed line, which also tints the row so the
/// change reads at a glance without painting over the syntax colours — those are
/// most worth reading on exactly the lines that changed.
fn code_line(
    text: &str,
    lang: highlight::Language,
    marker: char,
    marker_colour: Option<Color>,
    width: usize,
) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(GUTTER).max(1);
    let spans = highlight::spans(text, lang);
    let tint = marker_colour.map(|c| Style::default().bg(tinted(c)));

    // `wrap::line` rather than `wrap::text`: its rows carry the byte offset they
    // start at, which is what lets a highlight run be split across a wrap
    // boundary and keep its colour on both halves.
    let rows = wrap::line(text, inner);
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let gutter = if i == 0 {
            format!("{marker} ")
        } else {
            " ".repeat(GUTTER)
        };
        let mut rendered = vec![Span::styled(
            gutter,
            match marker_colour {
                Some(colour) => Style::default().fg(colour).add_modifier(Modifier::BOLD),
                None => Style::default().fg(Color::DarkGray),
            },
        )];
        rendered.extend(highlighted(&row.text, row.start, &spans, tint));
        out.push(Line::from(rendered));
    }
    out
}

/// Slice the line's highlight runs down to the piece of it this row holds.
fn highlighted(
    row: &str,
    start: usize,
    spans: &[(std::ops::Range<usize>, highlight::Token)],
    tint: Option<Style>,
) -> Vec<Span<'static>> {
    slice_runs(
        row,
        start,
        spans,
        tint.unwrap_or_default(),
        |token, base| base.fg(token_colour(token)),
    )
}

/// Cut styled runs down to the piece of the line this wrapped row holds.
///
/// Shared by syntax highlighting and inline markdown, which face the same
/// problem: both style runs over a whole logical line, and both need a run split
/// by a wrap to keep its style on either side of the break. `start` is the row's
/// byte offset into that line, which is what [`wrap::line`] returns it for.
fn slice_runs<T: Copy>(
    row: &str,
    start: usize,
    runs: &[(std::ops::Range<usize>, T)],
    base: Style,
    style_of: impl Fn(T, Style) -> Style,
) -> Vec<Span<'static>> {
    let end = start + row.len();
    let mut out = Vec::new();
    for (range, token) in runs {
        // The overlap between this run and this row, in line coordinates.
        let (from, to) = (range.start.max(start), range.end.min(end));
        if from >= to {
            continue;
        }
        out.push(Span::styled(
            row[from - start..to - start].to_string(),
            style_of(*token, base),
        ));
    }
    // A blank row still has to occupy its line, and a tinted one has to show its
    // tint across the full width rather than collapsing to nothing.
    if out.is_empty() {
        out.push(Span::styled(row.to_string(), base));
    }
    out
}

/// Red and green belong to the diff gutter, so no syntax token may claim them.
fn token_colour(token: highlight::Token) -> Color {
    match token {
        highlight::Token::Comment => Color::DarkGray,
        highlight::Token::Str => Color::Yellow,
        highlight::Token::Number => Color::Magenta,
        highlight::Token::Keyword => Color::Cyan,
        highlight::Token::Plain => Color::Reset,
    }
}

/// A background dark enough to sit behind ordinary foreground colours. Indexed
/// rather than RGB so it degrades sensibly on a 256-colour terminal.
fn tinted(colour: Color) -> Color {
    match colour {
        Color::Red => Color::Indexed(52),
        _ => Color::Indexed(22),
    }
}

/// Draw the sessions view: every conversation running, and what each is doing.
///
/// A whole screen rather than a panel in the prompt's slot. Every other overlay
/// in this harness is about the conversation it sits under, and is drawn beside
/// it for exactly that reason; this one is about the harness, and there is no
/// one conversation it belongs beside.
pub fn draw_sessions(
    frame: &mut Frame,
    view: &crate::sessions::View,
    rows: &[crate::sessions::Row],
    tick: usize,
    quit_armed: bool,
) -> Metrics {
    let area = frame.area();
    let block = Block::bordered()
        .title(Line::from(" sessions ").bold())
        .border_style(Style::default().fg(Color::Blue));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [query, list, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let width = list.width as usize;
    let visible = list.height as usize;

    frame.render_widget(
        Paragraph::new(query_row(&view.query, view.searching, query.width as usize)),
        query,
    );
    // Only while searching, for the reason the pickers place it that way: a
    // cursor in a row nobody is typing into says the keyboard is somewhere it
    // is not.
    if view.searching {
        let col = view.query.layout(query.width.saturating_sub(3)).cursor.1;
        frame.set_cursor_position(Position::new(query.x + 3 + col, query.y));
    }

    // The highlight is a position in the filtered rows, and the filter can shrink
    // under it when a background session's status changes.
    let selected = view.selected.min(rows.len().saturating_sub(1));

    // An entry is a header and its activity, so it is several rows tall and a
    // click maps back through a table rather than by adding an offset — the same
    // problem the `/load` picker's entries have, solved the same way.
    let entries: Vec<Vec<Line<'static>>> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| session_entry(row, i == selected, tick, width))
        .collect();

    // Walk back from the selection while the entries still fit, so the
    // highlighted session is on screen by construction rather than by arithmetic
    // that assumed every entry was the same height.
    let mut first = selected.min(entries.len().saturating_sub(1));
    let mut used = entries.get(first).map_or(0, Vec::len);
    while first > 0 {
        let next = used + entries[first - 1].len();
        if next > visible {
            break;
        }
        used = next;
        first -= 1;
    }

    let mut lines = Vec::new();
    let mut owners = Vec::new();
    'outer: for (i, entry) in entries.iter().enumerate().skip(first) {
        for line in entry {
            if lines.len() == visible {
                break 'outer;
            }
            lines.push(line.clone());
            owners.push(i);
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no session matches",
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), list);
    // The hints name the keys that are live *now*: while searching the letters
    // are the query, so offering `n new` would be a lie.
    let hint = if view.searching {
        "typing filters · Enter switch · Esc back to the list"
    } else {
        "/ search · j/k · Enter switch · n new · l open saved · x shut down · Esc close"
    };
    // The armed quit takes the footer, as it takes the status bar on the other
    // screen. Quitting from here takes every session with it, so this is the
    // screen where it most needs saying.
    let footer_line = if quit_armed {
        Line::from(Span::styled(
            "Press Ctrl+C again to quit — every session closes",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray)))
    };
    frame.render_widget(Paragraph::new(footer_line), footer);

    Metrics {
        sessions_list: Some(list),
        sessions_rows: owners,
        ..Metrics::default()
    }
}

/// One session in the view: a header naming it, then what it is doing.
///
/// The activity is the reason to open this list at all. A column of names and
/// the word "streaming" tells you which session is busy; it does not tell you
/// which one is busy with the thing you came back for.
fn session_entry(
    row: &crate::sessions::Row,
    highlighted: bool,
    tick: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let style = if highlighted {
        selected_row()
    } else if row.focused {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    // Three states worth telling apart at a glance: working, waiting on you, and
    // neither. A spinner says "come back later"; a `!` says "come now".
    let mark = if row.blocked {
        "! ".to_string()
    } else if row.busy {
        format!("{} ", SPINNER[(tick / 2) % SPINNER.len()])
    } else {
        "  ".to_string()
    };
    let marker = if highlighted { "› " } else { "  " };
    // `‹current›` on the session the prompt belongs to, so switching away and
    // back is never disorienting — the highlight is where you are *looking*.
    let here = if row.focused { " ‹current›" } else { "" };
    let aside = format!("{}  {}  {} turns", row.status, row.model, row.turns);
    let name = format!("{marker}{mark}{}{here}", row.name);
    let room = width.saturating_sub(aside.chars().count() + 2);
    let name = truncate(&name, room.max(1));
    // Exactly the remaining width, so the three spans together fill the row. A
    // highlight that stopped short of the edge would read as a ragged bar rather
    // than as a selected row.
    let pad = width
        .saturating_sub(name.chars().count() + aside.chars().count())
        .max(1);
    // The aside carries the highlight's background too, for the same reason: it
    // is the right-hand end of the same row, not a separate thing beside it. A
    // blocked session keeps its yellow while it is not the highlighted one; on
    // the bar the `!` beside the name already says it.
    let aside_style = if !highlighted && row.blocked {
        Style::default().fg(Color::Yellow)
    } else {
        selected_aside(highlighted)
    };

    let mut lines = vec![Line::from(vec![
        Span::styled(name, style),
        Span::styled(" ".repeat(pad), style),
        Span::styled(aside, aside_style),
    ])];

    // Dim and indented under the name: the header is what you are choosing
    // between, the activity is what tells you which to choose.
    let dim = Style::default().fg(Color::DarkGray);
    for line in &row.activity {
        lines.push(Line::from(Span::styled(
            format!("      {}", truncate(line, width.saturating_sub(7).max(1))),
            dim,
        )));
    }
    // A blank between entries, so several sessions read as several things.
    lines.push(Line::default());
    lines
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect, sessions: (usize, usize)) {
    let (label, colour) = match app.status {
        Status::Idle => (" ready ", Color::Green),
        Status::Waiting => (" waiting ", Color::Yellow),
        Status::Streaming => (" streaming ", Color::Cyan),
        Status::AwaitingApproval(_) => (" approve ", Color::Magenta),
        Status::AwaitingChoice(_) => (" answer ", Color::Yellow),
        Status::AwaitingExecute { .. } => (" execute ", Color::Green),
        Status::AwaitingUndo { .. } => (" undo ", Color::Yellow),
        Status::Running => (" running ", Color::Blue),
        Status::Compacting => (" compacting ", Color::Cyan),
    };

    let mut spans = vec![
        Span::styled(label, Style::default().fg(Color::Black).bg(colour)),
        Span::raw(" "),
        Span::styled(app.model.clone(), Style::default().fg(Color::Gray)),
    ];
    // The other sessions, when there are any. Hidden at one, which is the shape
    // the harness has always had — and a session waiting on a person is called
    // out, since that is the one thing that will not resolve itself while you
    // are looking elsewhere.
    let (count, blocked) = sessions;
    if count > 1 {
        spans.push(Span::styled(
            format!("  {count} sessions"),
            Style::default().fg(Color::Gray),
        ));
        if blocked > 0 {
            spans.push(Span::styled(
                format!(" · {blocked} need you"),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }
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
    // Red where debug is yellow: `/debug` changes what you see, this changes
    // what happens without you.
    if app.auto_approve {
        spans.push(Span::styled(
            "  auto-approve",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    // Cyan rather than red: plan mode takes capability away, so it is a state
    // worth seeing but not a warning.
    if app.planning() {
        spans.push(Span::styled("  plan", Style::default().fg(Color::Cyan)));
    }
    // Blue like `running`, because that is what it is — work going on that the
    // prompt is not waiting for. Shown whenever there is any: a job you have
    // forgotten about is exactly the one worth a reminder.
    if app.live_jobs() > 0 {
        spans.push(Span::styled(
            format!("  {} job(s)", app.live_jobs()),
            Style::default().fg(Color::Blue),
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
    } else if app.rewind().is_some() {
        // Coexists with `Idle`, like the pickers, so without this the bar would
        // offer to send a prompt while the list below is asking you to choose.
        "  ↑/↓ choose · Enter rewind · Esc cancel"
    } else if app.pending_undo().is_some() {
        "  ←/→ choose · Enter confirm · Esc cancel"
    } else if app.executing().is_some() {
        "  ←/→ choose · Enter confirm · Esc keep planning"
    } else if app.question().is_some() {
        "  j/k or ↑/↓ or 1-9 choose · Enter answer · Esc dismiss"
    } else if app.stats_open() {
        // Coexists with `Idle` like the pickers, so without this the bar would
        // offer to send a prompt while a page is covering the prompt box.
        "  Esc close"
    } else if let Some(picker) = app.picker() {
        // The picker coexists with `Idle`, so without this the bar offers to
        // send a prompt while the panel below it is asking you to choose.
        match (picker.searching, picker.open_as_new) {
            (true, false) => "  typing filters · Enter load · Esc back to the list",
            (false, false) => "  / search · j/k or ↑/↓ · Enter load · Esc cancel",
            (true, true) => "  typing filters · Enter open · Esc back to the list",
            (false, true) => "  / search · j/k or ↑/↓ · Enter open · Esc cancel",
        }
    } else if app.model_picker().is_some() {
        if app.model_searching() {
            "  typing filters · Enter select · Esc back to the list"
        } else {
            "  / search · j/k or ↑/↓ · Enter select · Esc cancel"
        }
    } else if matches!(
        app.status,
        Status::Waiting | Status::Streaming | Status::Running | Status::Compacting
    ) {
        // Busy without a modal. The prompt still takes typing, so the hint says
        // what it is good for — otherwise the only discoverable key is Esc and
        // the box looks like it is there for nothing.
        "  Esc cancel · slash commands still run · Ctrl+C quit"
    } else {
        // `/clear` where `Ctrl+L` used to be: the hint advertised clearing, and
        // the command is now the only way to do it.
        "  Enter send · Alt+Enter newline · /clear · Ctrl+C quit"
    };
    // An armed Ctrl+C replaces every other hint and is the one thing on this bar
    // that is not dim: it is a question with a deadline, and a hint nobody
    // notices would make the second press feel like the first one did nothing.
    if app.quit_armed() {
        spans.push(Span::styled(
            "  Press Ctrl+C again to quit",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::styled(hints, Style::default().fg(Color::DarkGray)));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_input(frame: &mut Frame, area: Rect, layout: &crate::input::Layout, visible_rows: u16) {
    // Live whether or not a turn is running: the box takes typing either way, and
    // a dim border said the opposite. Which state the *session* is in is the
    // status bar's job, and the spinner above says it too.
    let block = Block::bordered().border_style(Style::default().fg(Color::Blue));
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
                Span::styled(marker, Style::default().fg(Color::Blue)),
                Span::raw(row.clone()),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(Text::from(lines)), inner);

    frame.set_cursor_position(Position::new(
        inner.x + 2 + cursor_col,
        inner.y + cursor_row.saturating_sub(offset),
    ));
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    /// Render the sessions view and return the screen as one string per row.
    fn render_sessions(
        rows: &[crate::sessions::Row],
        selected: usize,
        width: u16,
        height: u16,
    ) -> (Vec<String>, Metrics) {
        render_sessions_view(
            rows,
            &crate::sessions::View {
                selected,
                ..Default::default()
            },
            width,
            height,
        )
    }

    /// The same, with the view's search state under the caller's control.
    fn render_sessions_view(
        rows: &[crate::sessions::Row],
        view: &crate::sessions::View,
        width: u16,
        height: u16,
    ) -> (Vec<String>, Metrics) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut metrics = Metrics::default();
        terminal
            .draw(|frame| metrics = draw_sessions(frame, view, rows, 0, false))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        (rows, metrics)
    }

    /// A session row, with everything but what the test is about defaulted.
    fn session_row(name: &str, status: &'static str, focused: bool) -> crate::sessions::Row {
        crate::sessions::Row {
            id: 1,
            name: name.into(),
            model: "test/model".into(),
            status,
            blocked: status == "needs you",
            busy: matches!(status, "streaming" | "thinking" | "running"),
            turns: 3,
            focused,
            activity: Vec::new(),
        }
    }

    /// The same, with a few lines of activity under it.
    fn active_row(name: &str, status: &'static str, activity: &[&str]) -> crate::sessions::Row {
        crate::sessions::Row {
            activity: activity.iter().map(|s| (*s).to_string()).collect(),
            ..session_row(name, status, false)
        }
    }

    /// Every row of `buffer` that carries the selection background, as
    /// `(y, how many cells)`.
    fn highlighted_rows(buffer: &ratatui::buffer::Buffer) -> Vec<(u16, usize)> {
        let bar = selected_row().bg;
        (0..buffer.area.height)
            .filter_map(|y| {
                let n = (0..buffer.area.width)
                    .filter(|x| buffer[(*x, y)].style().bg == bar)
                    .count();
                (n > 0).then_some((y, n))
            })
            .collect()
    }

    /// The bar has to reach both edges of its row in *every* list, not just the
    /// one that was reported. Each of these builds its row from several spans,
    /// and a span that forgets the background leaves the highlight ragged —
    /// which is exactly how this was first noticed in the sessions view.
    #[test]
    fn a_selected_row_is_highlighted_edge_to_edge_in_every_list() {
        let width = 60u16;
        // What `prepare_panel` builds its lines to: borders take two columns and
        // the content is padded by one either side. The bar is as wide as the
        // content, so this is the full width of a panel row.
        let inner = (width - 4) as usize;

        // The `/load` picker, whose entry is a name and the model beside it.
        let (mut app, dir) = app_with_picker(&["alpha", "beta"]);
        app.picker_move(1);
        let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
        let mut cache = TranscriptCache::default();
        terminal
            .draw(|frame| {
                draw(frame, &mut app, &mut cache, (1, 0));
            })
            .unwrap();
        for (y, n) in highlighted_rows(terminal.backend().buffer()) {
            assert_eq!(n, inner, "the /load picker's bar is ragged on row {y}");
        }
        let _ = std::fs::remove_dir_all(&dir);

        // The `/rewind` list, whose row is a prompt and a file count beside it.
        let mut app = with_rewind(&[("first", 2), ("second", 0)]);
        app.rewind_move(-1);
        let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
        let mut cache = TranscriptCache::default();
        terminal
            .draw(|frame| {
                draw(frame, &mut app, &mut cache, (1, 0));
            })
            .unwrap();
        for (y, n) in highlighted_rows(terminal.backend().buffer()) {
            assert_eq!(n, inner, "the /rewind list's bar is ragged on row {y}");
        }
    }

    /// The same for the sessions view, which is a screen rather than a panel.
    #[test]
    fn the_selected_session_is_highlighted_the_whole_width() {
        let rows = vec![
            active_row("first", "ready", &["you: one"]),
            active_row("second", "streaming", &["you: two"]),
        ];
        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
        let view = crate::sessions::View {
            selected: 1,
            ..Default::default()
        };
        terminal
            .draw(|frame| {
                draw_sessions(frame, &view, &rows, 0, false);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();

        // The header row of the highlighted session, found by its name.
        let y = (0..buffer.area.height)
            .find(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("second")
            })
            .expect("the highlighted session is on screen");

        // Inside the border, every cell of that row carries the highlight —
        // whatever `selected_row` currently is, so a restyle cannot quietly
        // break the geometry this test is actually about.
        let bar = selected_row().bg;
        let highlighted: Vec<bool> = (1..buffer.area.width - 1)
            .map(|x| buffer[(x, y)].style().bg == bar)
            .collect();
        assert!(
            highlighted.iter().all(|on| *on),
            "the bar stops short at column {:?} of {}",
            highlighted.iter().position(|on| !on),
            highlighted.len()
        );

        // And the row above it, belonging to another session, carries none.
        let plain = (1..buffer.area.width - 1).all(|x| buffer[(x, y - 1)].style().bg != bar);
        assert!(plain, "only the selected row is a bar");
    }

    /// The reason to open the list at all: which session is busy is a column of
    /// names, but *what with* is the thing you came back for.
    #[test]
    fn each_session_shows_what_it_is_doing() {
        let rows = vec![
            active_row(
                "builder",
                "streaming",
                &["you: add a checkpoint module", "cargo test 2>&1 | tail -20"],
            ),
            active_row(
                "cleaner",
                "needs you",
                &["you: clean up tmp", "rm -rf tmp/*"],
            ),
        ];
        let (screen, _) = render_sessions(&rows, 0, 78, 20);
        let text = screen.join("\n");

        for line in [
            "you: add a checkpoint module",
            "cargo test 2>&1 | tail -20",
            "rm -rf tmp/*",
        ] {
            assert!(text.contains(line), "missing {line:?}:\n{text}");
        }
        // The activity is why `needs you` is actionable rather than alarming:
        // the command waiting for approval is right there under the name.
        let blocked_at = text.find("cleaner").unwrap();
        let command_at = text.find("rm -rf tmp/*").unwrap();
        assert!(blocked_at < command_at, "under its own session:\n{text}");
    }

    /// An entry is several rows tall, so a click anywhere in one has to pick
    /// that session — including on its activity, which is most of its height.
    #[test]
    fn a_click_on_any_line_of_a_session_picks_that_session() {
        let rows = vec![
            active_row("first", "ready", &["you: one", "did one"]),
            active_row("second", "ready", &["you: two", "did two"]),
        ];
        let (screen, metrics) = render_sessions(&rows, 0, 78, 20);
        let list = metrics.sessions_list.expect("list rect");

        // The row holding "did two" belongs to the second session.
        let row = screen.iter().position(|r| r.contains("did two")).unwrap() as u16;
        let owner = metrics
            .sessions_rows
            .get((row - list.y) as usize)
            .copied()
            .expect("every rendered row has an owner");
        assert_eq!(owner, 1, "an activity line belongs to its session");
    }

    #[test]
    fn the_sessions_view_lists_every_session_and_what_it_is_doing() {
        let rows = vec![
            session_row("session-alpha", "streaming", false),
            session_row("session-beta", "ready", true),
            session_row("session-gamma", "needs you", false),
        ];
        let (screen, metrics) = render_sessions(&rows, 1, 78, 10);
        let text = screen.join("\n");

        assert!(text.contains("sessions"), "missing title:\n{text}");
        for name in ["session-alpha", "session-beta", "session-gamma"] {
            assert!(text.contains(name), "missing {name}:\n{text}");
        }
        assert!(text.contains("n new"), "missing footer:\n{text}");
        assert!(metrics.sessions_list.is_some(), "list rect for clicks");
    }

    /// Three states worth telling apart without reading: working, wanting you,
    /// and neither. The second is the one that will not resolve on its own.
    #[test]
    fn a_working_session_spins_and_a_blocked_one_is_marked() {
        let rows = vec![
            session_row("busy-one", "streaming", false),
            session_row("blocked-one", "needs you", false),
            session_row("idle-one", "ready", false),
        ];
        let (screen, _) = render_sessions(&rows, 0, 78, 10);

        let busy = screen.iter().find(|r| r.contains("busy-one")).unwrap();
        let blocked = screen.iter().find(|r| r.contains("blocked-one")).unwrap();
        let idle = screen.iter().find(|r| r.contains("idle-one")).unwrap();
        assert!(
            SPINNER.iter().any(|frame| busy.contains(frame)),
            "a working session should spin: {busy:?}"
        );
        assert!(
            blocked.contains('!'),
            "a blocked one should shout: {blocked:?}"
        );
        assert!(
            !idle.contains('!') && !SPINNER.iter().any(|f| idle.contains(f)),
            "and an idle one should do neither: {idle:?}"
        );
    }

    /// Switching away and back should never leave you wondering which one you
    /// are in, so the session the prompt belongs to says so.
    #[test]
    fn the_view_marks_the_session_you_are_in_apart_from_the_highlight() {
        let rows = vec![
            session_row("where-you-are", "ready", true),
            session_row("merely-highlighted", "ready", false),
        ];
        let (screen, _) = render_sessions(&rows, 1, 78, 10);
        let current = screen.iter().find(|r| r.contains("where-you-are")).unwrap();
        let highlighted = screen
            .iter()
            .find(|r| r.contains("merely-highlighted"))
            .unwrap();
        assert!(current.contains("‹current›"), "{current:?}");
        assert!(!current.contains('›') || current.contains("‹current›"));
        assert!(
            highlighted.contains('›') && !highlighted.contains("‹current›"),
            "the highlight is where you are looking, not where you are: {highlighted:?}"
        );
    }

    /// The query row says which mode the keyboard is in, and the hints below it
    /// name only the keys that are live — while searching, `n` is a letter.
    #[test]
    fn the_sessions_view_says_whether_you_are_navigating_or_searching() {
        let rows = vec![session_row("one", "ready", true)];
        let navigating = crate::sessions::View::default();
        let (screen, _) = render_sessions_view(&rows, &navigating, 78, 10);
        assert!(screen[1].contains("/ to search"), "{:?}", screen[1]);
        assert!(
            screen.iter().any(|r| r.contains("n new")),
            "navigating offers the list's keys: {screen:?}"
        );

        let mut searching = crate::sessions::View {
            searching: true,
            ..Default::default()
        };
        searching.query.insert_char('b');
        let (screen, _) = render_sessions_view(&rows, &searching, 78, 10);
        assert!(screen[1].contains("/b"), "{:?}", screen[1]);
        assert!(
            screen.iter().any(|r| r.contains("typing filters"))
                && !screen.iter().any(|r| r.contains("n new")),
            "searching does not offer keys the query has taken: {screen:?}"
        );
    }

    /// A filter can leave nothing, and an empty screen would read as a broken
    /// one rather than as a query that is too narrow.
    #[test]
    fn a_filtered_out_sessions_view_says_so() {
        let view = crate::sessions::View {
            searching: true,
            ..Default::default()
        };
        let (screen, metrics) = render_sessions_view(&[], &view, 78, 10);
        assert!(
            screen.iter().any(|r| r.contains("no session matches")),
            "{screen:?}"
        );
        assert!(
            metrics.sessions_rows.is_empty(),
            "and a click on the notice lands on nothing"
        );
    }

    #[test]
    fn the_status_bar_counts_the_other_sessions_and_who_needs_you() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        let (screen, _) = render_with_sessions(&mut app, 70, 12, (1, 0));
        assert!(
            !screen.join("\n").contains("sessions"),
            "one session is the shape the harness has always had"
        );

        let (screen, _) = render_with_sessions(&mut app, 70, 12, (3, 1));
        let text = screen.join("\n");
        assert!(text.contains("3 sessions"), "{text}");
        assert!(text.contains("1 need you"), "{text}");
    }

    /// Render into a fake terminal and return the screen as one string per row.
    /// Starts from a cold cache, which is what most tests want to pin down.
    fn render(app: &mut App, width: u16, height: u16) -> (Vec<String>, Metrics) {
        render_cached(app, &mut TranscriptCache::default(), width, height)
    }

    /// The two pickers are one list with two meanings for `Enter`, and the
    /// title and hint are the only place that difference shows.
    #[test]
    fn the_picker_says_whether_enter_loads_or_opens() {
        let dir = std::env::temp_dir().join(format!("ai-harness-ui-verb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = App::new("test/model".into(), None, 10, dir.clone());
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_response("<ai-harness-response>ok</ai-harness-response>".into(), None);
        app.input.insert_str("/save alpha");
        app.submit();

        app.open_load_picker();
        let (screen, _) = render(&mut app, 78, 20);
        let joined = screen.join("\n");
        assert!(joined.contains("load session"), "{joined}");
        assert!(joined.contains("Enter load"), "{joined}");

        app.picker_cancel();
        app.open_session_picker();
        let (screen, _) = render(&mut app, 78, 20);
        let joined = screen.join("\n");
        assert!(joined.contains("open session"), "{joined}");
        assert!(joined.contains("Enter open"), "{joined}");
        assert!(!joined.contains("Enter load"), "{joined}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sessions_footer_offers_the_saved_sessions_key() {
        let rows = vec![session_row("one", "ready", true)];
        let (screen, _) = render_sessions(&rows, 0, 90, 10);
        assert!(
            screen.iter().any(|r| r.contains("l open saved")),
            "{screen:?}"
        );
    }

    /// A page you are reading loses the slot to a modal that is waiting on you —
    /// and gets it back when the modal is gone.
    #[test]
    fn an_approval_takes_the_slot_from_the_stats_page() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("do a thing");
        app.submit().unwrap();
        app.run_command(crate::command::Command::Stats);

        let (screen, _) = render(&mut app, 78, 24);
        assert!(
            screen.iter().any(|r| r.contains("session stats")),
            "{screen:?}"
        );

        app.push_response("<ai-harness-shell>ls</ai-harness-shell>".into(), None);
        let (screen, _) = render(&mut app, 78, 24);
        assert!(
            !screen.iter().any(|r| r.contains("session stats")),
            "the approval should have the slot:\n{}",
            screen.join("\n")
        );
        assert!(screen.iter().any(|r| r.contains("Allow")), "{screen:?}");
    }

    /// The page coexists with `Idle`, so without its own arm the bar would offer
    /// to send a prompt while covering the prompt box.
    #[test]
    fn the_status_bar_does_not_offer_the_prompt_under_the_stats_page() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.run_command(crate::command::Command::Stats);

        let (screen, _) = render(&mut app, 78, 24);
        let joined = screen.join("\n");
        assert!(joined.contains("session stats"), "{joined}");
        assert!(!joined.contains("Enter send"), "{joined}");
        assert!(joined.contains("Esc close"), "{joined}");
    }

    /// A first Ctrl+C that changed nothing on screen would read as a key that
    /// did nothing, and the second press would come as a surprise.
    #[test]
    fn an_armed_quit_takes_over_the_status_bar() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        let (before, _) = render(&mut app, 78, 12);
        assert!(
            before.iter().any(|r| r.contains("Enter send")),
            "the ordinary hints: {before:?}"
        );

        app.request_quit();
        let (armed, _) = render(&mut app, 78, 12);
        assert!(
            armed
                .iter()
                .any(|r| r.contains("Press Ctrl+C again to quit")),
            "the armed quit should say so: {armed:?}"
        );
        assert!(
            !armed.iter().any(|r| r.contains("Enter send")),
            "and should replace the hints rather than crowd in beside them"
        );
    }

    /// Quitting from the sessions view closes every conversation, so this is the
    /// screen where the second press most needs offering.
    #[test]
    fn an_armed_quit_takes_over_the_sessions_footer() {
        let rows = vec![session_row("one", "ready", true)];
        let view = crate::sessions::View::default();
        let mut terminal = Terminal::new(TestBackend::new(78, 10)).unwrap();
        terminal
            .draw(|frame| {
                draw_sessions(frame, &view, &rows, 0, true);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let screen: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        assert!(
            screen.iter().any(|r| r.contains("Press Ctrl+C again")),
            "{screen:#?}"
        );
        assert!(
            !screen.iter().any(|r| r.contains("n new")),
            "the footer is the offer now, not the key list"
        );
    }

    /// The prompt is usable with a turn in flight, so it has to look usable:
    /// a live border, a real cursor, and a hint that says what it is for.
    #[test]
    fn the_prompt_stays_live_while_a_turn_is_running() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("something slow");
        app.submit().unwrap();
        assert!(app.is_busy());
        app.input.insert_str("/co");

        let mut terminal = Terminal::new(TestBackend::new(70, 12)).unwrap();
        let mut cache = TranscriptCache::default();
        terminal
            .draw(|frame| {
                draw(frame, &mut app, &mut cache, (1, 0));
            })
            .unwrap();

        assert!(
            terminal.get_cursor_position().is_ok_and(|p| p.x > 0),
            "the cursor should sit in the prompt, not be parked at the origin"
        );

        let buffer = terminal.backend().buffer().clone();
        let screen: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        assert!(
            screen
                .iter()
                .any(|r| r.contains("slash commands still run")),
            "the busy hint should say the prompt is good for something:\n{screen:#?}"
        );
        // The border below the status bar is the input box's.
        let bottom = buffer.area.height - 1;
        assert_eq!(
            buffer[(0, bottom)].style().fg,
            Some(Color::Blue),
            "a dim border would say the box is inert, which it is not"
        );
    }

    /// The same, with a session count for the status bar.
    fn render_with_sessions(
        app: &mut App,
        width: u16,
        height: u16,
        sessions: (usize, usize),
    ) -> (Vec<String>, Metrics) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut metrics = Metrics::default();
        let mut cache = TranscriptCache::default();
        terminal
            .draw(|frame| metrics = draw(frame, app, &mut cache, sessions))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        (rows, metrics)
    }

    /// The same, against a cache that survives between calls — the way the real
    /// loop draws, and the only way to exercise cache reuse.
    fn render_cached(
        app: &mut App,
        cache: &mut TranscriptCache,
        width: u16,
        height: u16,
    ) -> (Vec<String>, Metrics) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut metrics = Metrics::default();
        terminal
            .draw(|frame| metrics = draw(frame, app, cache, (1, 0)))
            .unwrap();

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
    /// An edit entry as the app builds one: with its diff already computed.
    /// See `App::push_response` — the diff is stored when the edit arrives,
    /// because rendering it repeats every frame.
    fn edit_entry(path: &str, old: &str, new: &str) -> Entry {
        Entry::Action {
            action: crate::protocol::Action::Edit {
                path: path.into(),
                old: old.into(),
                new: new.into(),
            },
            usage: None,
            diff: crate::diff::lines(old, new),
        }
    }

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
    /// An app whose `/rewind` list is open over `prompts`, the last of which is
    /// the newest. `changed` says how many files each turn touched.
    fn with_rewind(prompts: &[(&str, usize)]) -> App {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.open_rewind_over(
            prompts
                .iter()
                .enumerate()
                .map(|(i, (prompt, changed))| crate::app::RewindRow {
                    turn: i + 1,
                    history_index: i * 2 + 1,
                    transcript_index: Some(i * 3),
                    changed: *changed,
                    prompt: (*prompt).to_string(),
                })
                .collect(),
        );
        app
    }

    #[test]
    fn the_rewind_list_opens_on_the_newest_prompt() {
        let mut app = with_rewind(&[("first thing", 1), ("second thing", 0), ("newest", 2)]);
        let (rows, metrics) = render(&mut app, 70, 20);
        let screen = rows.join("\n");

        assert!(screen.contains("rewind to"), "missing title:\n{screen}");
        assert!(
            rows.iter().any(|r| r.contains("› newest")),
            "the newest prompt carries the marker:\n{screen}"
        );
        assert!(screen.contains("first thing"), "older rows show:\n{screen}");
        assert!(metrics.rewind_list.is_some(), "list rect must be reported");
    }

    /// The summary is what makes Enter an informed decision, so it has to track
    /// the highlight rather than the list.
    #[test]
    fn the_rewind_summary_follows_the_highlight() {
        let mut app = with_rewind(&[("first thing", 1), ("second thing", 0), ("newest", 2)]);
        let (rows, _) = render(&mut app, 70, 20);
        assert!(
            rows.join("\n").contains("undo 1 turn(s)"),
            "the newest row undoes the last turn, like /undo:\n{}",
            rows.join("\n")
        );

        app.rewind_move(-2);
        let (rows, _) = render(&mut app, 70, 20);
        let screen = rows.join("\n");
        assert!(screen.contains("undo 3 turn(s)"), "{screen}");
        assert!(
            rows.iter().any(|r| r.contains("› first thing")),
            "the highlight moved:\n{screen}"
        );
    }

    #[test]
    fn a_rewind_row_shows_what_its_turn_changed() {
        let mut app = with_rewind(&[("changed two", 2), ("changed none", 0)]);
        let (rows, _) = render(&mut app, 70, 20);
        let screen = rows.join("\n");
        let changed = rows.iter().find(|r| r.contains("changed two")).unwrap();
        assert!(changed.contains("2 file(s)"), "{screen}");
        let untouched = rows.iter().find(|r| r.contains("changed none")).unwrap();
        assert!(
            !untouched.contains("file(s)"),
            "a turn that changed nothing says nothing:\n{screen}"
        );
    }

    /// The undo panel, put up directly. Building it through a real turn would be
    /// a test of the checkpoint module, which has its own.
    fn awaiting_undo(restored: &[&str], removed: &[&str], partial: Option<&str>) -> App {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.status = crate::app::Status::AwaitingUndo {
            selected: crate::app::Choice::Deny,
            undo: Box::new(crate::app::PendingUndo {
                turn: 3,
                prompt: "rename the parser".into(),
                partial: partial.map(str::to_string),
                plan: crate::checkpoint::Restored {
                    restored: restored.iter().map(|s| s.to_string()).collect(),
                    removed: removed.iter().map(|s| s.to_string()).collect(),
                    failed: Vec::new(),
                },
            }),
        };
        app
    }

    /// Restores and deletions are separate promises, and the panel must not let
    /// the second hide inside the first.
    #[test]
    fn the_undo_panel_lists_deletions_apart_from_restores() {
        let mut app = awaiting_undo(&["src/a.rs"], &["src/new.rs"], None);
        let (rows, _) = render(&mut app, 70, 24);
        let screen = rows.join("\n");

        assert!(screen.contains("undo this turn?"), "{screen}");
        assert!(screen.contains("rename the parser"), "names it:\n{screen}");
        assert!(screen.contains("restore 1 file(s)"), "{screen}");
        assert!(screen.contains("delete 1 file(s)"), "{screen}");
        let restore_at = screen.find("restore 1").unwrap();
        let delete_at = screen.find("delete 1").unwrap();
        let new_at = screen.find("src/new.rs").unwrap();
        assert!(
            restore_at < delete_at && delete_at < new_at,
            "the deletion must be under its own heading:\n{screen}"
        );
        assert!(
            screen.contains("Undo") && screen.contains("Cancel"),
            "buttons"
        );
    }

    #[test]
    fn the_undo_panel_says_when_a_checkpoint_was_capped() {
        let mut app = awaiting_undo(&["a.rs"], &[], Some("too many files"));
        let (rows, _) = render(&mut app, 70, 24);
        assert!(
            rows.join("\n").contains("partial"),
            "a capped checkpoint must say so before it is trusted:\n{}",
            rows.join("\n")
        );
    }

    /// A turn that touched forty files is recognised, not enumerated.
    #[test]
    fn the_undo_panel_summarises_a_long_list() {
        let many: Vec<String> = (0..40).map(|i| format!("src/f{i:02}.rs")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let mut app = awaiting_undo(&refs, &[], None);
        let (rows, _) = render(&mut app, 70, 24);
        let screen = rows.join("\n");
        assert!(screen.contains("restore 40 file(s)"), "{screen}");
        assert!(screen.contains("and 34 more"), "{screen}");
    }

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
                offset: None,
                limit: None,
            },
            usage: None,
            diff: None,
        });
        app.transcript
            .push(Entry::ReadResult(crate::files::ReadOutcome::whole_file(
                "src/app.rs",
                "alpha\nbeta\n",
            )));

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
            diff: None,
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
            .push(Entry::ReadResult(crate::files::ReadOutcome::whole_file(
                "big.txt", contents,
            )));

        let (rows, _) = render(&mut app, 70, 24);
        let screen = transcript_only(&rows);
        assert!(screen.contains("line 0"), "missing preview head:\n{screen}");
        assert!(screen.contains("more line"), "missing summary:\n{screen}");
        assert!(
            !screen.contains("line 39"),
            "the whole file leaked into the transcript:\n{screen}"
        );
    }

    /// A `SearchOutcome` with `n` hits, for the render tests.
    fn search_result(kind: crate::search::SearchKind, n: usize) -> crate::search::SearchOutcome {
        crate::search::SearchOutcome {
            kind,
            pattern: "needle".into(),
            dir: None,
            glob: None,
            hits: (0..n)
                .map(|i| crate::search::Hit {
                    path: format!("src/f{i}.rs"),
                    line: matches!(kind, crate::search::SearchKind::Grep).then_some(i + 1),
                    text: if matches!(kind, crate::search::SearchKind::Grep) {
                        format!("let needle{i} = 1;")
                    } else {
                        String::new()
                    },
                })
                .collect(),
            files_matched: n,
            files_scanned: 19,
            files_skipped: 0,
            capped: None,
            error: None,
        }
    }

    #[test]
    fn grep_action_and_its_result_are_rendered() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.transcript.push(Entry::Action {
            action: crate::protocol::Action::Grep {
                pattern: "needle".into(),
                dir: Some("src".into()),
                glob: Some("*.rs".into()),
            },
            usage: None,
            diff: None,
        });
        app.transcript
            .push(Entry::SearchResult(Box::new(search_result(
                crate::search::SearchKind::Grep,
                1,
            ))));

        let (rows, _) = render(&mut app, 70, 20);
        let screen = transcript_only(&rows);
        assert!(screen.contains("grep"), "missing grep label:\n{screen}");
        assert!(screen.contains("needle"), "missing pattern:\n{screen}");
        assert!(screen.contains("in src"), "missing scope:\n{screen}");
        assert!(
            screen.contains("matching *.rs"),
            "missing filter:\n{screen}"
        );
        assert!(
            screen.contains("src/f0.rs:1:"),
            "missing the hit itself:\n{screen}"
        );
    }

    #[test]
    fn glob_action_and_its_result_are_rendered() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.transcript.push(Entry::Action {
            action: crate::protocol::Action::Glob {
                pattern: "**/*.rs".into(),
                dir: None,
            },
            usage: None,
            diff: None,
        });
        app.transcript
            .push(Entry::SearchResult(Box::new(search_result(
                crate::search::SearchKind::Glob,
                2,
            ))));

        let (rows, _) = render(&mut app, 70, 20);
        let screen = transcript_only(&rows);
        assert!(screen.contains("glob"), "missing glob label:\n{screen}");
        assert!(screen.contains("**/*.rs"), "missing pattern:\n{screen}");
        assert!(screen.contains("src/f0.rs"), "missing a path:\n{screen}");
        assert!(screen.contains("2 file(s)"), "missing the count:\n{screen}");
    }

    /// A hit list is the reason the search was run, so it gets the generous
    /// output cap rather than a read's eight-line taste — but it is still
    /// bounded, and says how much it held back.
    #[test]
    fn a_long_search_result_is_bounded_not_dumped() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        let outcome = search_result(crate::search::SearchKind::Grep, 200);
        app.transcript.push(Entry::SearchResult(Box::new(outcome)));

        let (rows, _) = render(&mut app, 70, 400);
        let screen = transcript_only(&rows);
        assert!(screen.contains("src/f0.rs"), "the head is shown:\n{screen}");
        assert!(
            !screen.contains("src/f199.rs"),
            "the tail must be elided, not dumped:\n{screen}"
        );
        assert!(screen.contains("more"), "missing the elision:\n{screen}");
    }

    #[test]
    fn a_failed_search_is_rendered_with_its_reason() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        let request = crate::search::Request::grep("fn (");
        app.transcript.push(Entry::SearchResult(Box::new(
            crate::search::SearchOutcome::failed(&request, "unclosed group"),
        )));

        let (rows, _) = render(&mut app, 70, 20);
        let screen = transcript_only(&rows);
        assert!(screen.contains("failed"), "missing status:\n{screen}");
        assert!(
            screen.contains("unclosed group"),
            "missing reason:\n{screen}"
        );
    }

    #[test]
    fn search_approval_modal_appears_under_confirm_reads() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.confirm_reads = true;
        app.input.insert_str("find a thing");
        app.submit().unwrap();
        app.push_response("<ai-harness-grep>needle</ai-harness-grep>".into(), None);
        assert!(
            app.pending().is_some(),
            "should be awaiting search approval"
        );

        let (rows, _) = render(&mut app, 70, 18);
        let screen = rows.join("\n");
        assert!(
            screen.contains("run this search?"),
            "missing title:\n{screen}"
        );
        assert!(screen.contains("needle"), "missing pattern:\n{screen}");
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
        app.transcript
            .push(edit_entry("src/app.rs", "let x = 1;", "let x = 2;"));

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
        app.sandbox = Some(crate::sandbox::Sandbox::for_tests(&dir));
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

    /// An app showing one model response.
    fn responded(text: &str) -> App {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("explain");
        app.submit().unwrap();
        app.push_response(
            format!("<ai-harness-response>{text}</ai-harness-response>"),
            None,
        );
        app
    }

    #[test]
    fn a_response_renders_markdown_without_leaving_markers() {
        let mut app = responded(
            "# Title\n\nSome **bold** and `code` here.\n\n- first\n- second\n\n\
             ```rust\nfn main() {}\n```",
        );
        let (rows, _) = render(&mut app, 70, 26);
        let screen = transcript_only(&rows);

        assert!(screen.contains("Title"), "{screen}");
        assert!(screen.contains("bold"), "{screen}");
        assert!(screen.contains("• first"), "bullets render:\n{screen}");
        assert!(screen.contains("fn main"), "the fence renders:\n{screen}");
        assert!(screen.contains("rust"), "the fence is labelled:\n{screen}");

        // The point of rendering at all: no syntax left on screen.
        for marker in ["# Title", "**bold**", "`code`", "```"] {
            assert!(!screen.contains(marker), "{marker:?} survived:\n{screen}");
        }
    }

    #[test]
    fn blocks_are_separated_but_list_items_stay_tight() {
        // Without separation everything runs together and the structure markdown
        // was expressing is lost; with it between every item, a list falls apart.
        let mut app = responded("A paragraph.\n\n- one\n- two\n\nAnother paragraph.");
        let (rows, _) = render(&mut app, 60, 20);
        let screen = transcript_only(&rows);
        let body: Vec<&str> = screen
            .lines()
            .skip_while(|l| !l.contains("A paragraph"))
            .take(5)
            // Strip the transcript's own border before comparing.
            .map(|l| l.trim().trim_matches('│').trim())
            .collect();

        assert_eq!(
            body,
            vec!["A paragraph.", "", "• one", "• two", ""],
            "blocks separated, items adjacent"
        );
    }

    #[test]
    fn a_fenced_block_in_a_response_is_not_truncated() {
        // The file-preview cap is right for a write, where the file could be
        // huge; here the model chose exactly this much and eliding it would cut
        // off the answer.
        let body: String = (1..=20).map(|i| format!("let x{i} = {i};\n")).collect();
        let mut app = responded(&format!("```rust\n{body}```"));

        let (rows, _) = render(&mut app, 60, 40);
        let screen = transcript_only(&rows);
        assert!(screen.contains("let x1 ="), "{screen}");
        assert!(
            screen.contains("let x20 ="),
            "the tail must survive:\n{screen}"
        );
        assert!(
            !screen.contains("more line(s)"),
            "nothing elided:\n{screen}"
        );
    }

    #[test]
    fn plain_prose_renders_exactly_as_before() {
        // Most responses have no markdown in them; they must not change.
        let mut app = responded("There are 8 Rust source files.");
        let (rows, _) = render(&mut app, 70, 12);
        assert!(transcript_only(&rows).contains("There are 8 Rust source files."));
    }

    #[test]
    fn a_numbered_list_keeps_its_numbers_and_aligns_continuations() {
        let mut app = responded(
            "1. a short one\n2. a much longer item that will certainly need to wrap \
             across more than one row of the terminal",
        );
        let (rows, _) = render(&mut app, 46, 20);
        let screen = transcript_only(&rows);
        assert!(screen.contains("1. a short one"), "{screen}");
        assert!(screen.contains("2. a much longer"), "{screen}");

        // The wrapped remainder sits under the text, not under the marker.
        let wrapped = rows
            .iter()
            .find(|r| r.contains("terminal"))
            .expect("the long item should wrap");
        assert!(
            wrapped.trim_start_matches('│').starts_with("   "),
            "continuation should be indented: {wrapped:?}"
        );
    }

    #[test]
    fn a_quote_and_a_rule_are_marked() {
        let mut app = responded("> quoted advice\n\n---\n\nafter");
        let (rows, _) = render(&mut app, 60, 16);
        let screen = transcript_only(&rows);
        assert!(screen.contains("│ quoted advice"), "{screen}");
        assert!(screen.contains("──────"), "the rule renders:\n{screen}");
    }

    #[test]
    fn rendered_markdown_never_overflows_its_width() {
        let mut app = responded(
            "# A heading long enough to need wrapping on a narrow terminal\n\n\
             A paragraph with **bold that goes on** and `some_inline_code_here` \
             and a [link](https://example.com/a/fairly/long/path) in it.\n\n\
             - an item that is also long enough to wrap more than once in a narrow view\n\n\
             ```rust\nlet x = \"a very long string literal that will not fit\";\n```",
        );
        for width in [40u16, 55, 80] {
            let (rows, _) = render(&mut app, width, 40);
            for row in &rows {
                assert!(
                    row.chars().count() <= width as usize,
                    "row overflows {width}: {row:?}"
                );
            }
        }
    }

    #[test]
    fn a_code_block_is_labelled_with_the_detected_language() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.transcript.push(Entry::Action {
            action: crate::protocol::Action::Write {
                path: "src/lib.rs".into(),
                contents: "fn main() {}\n".into(),
            },
            usage: None,
            diff: None,
        });
        let (rows, _) = render(&mut app, 70, 12);
        let screen = transcript_only(&rows);
        assert!(screen.contains("rust"), "missing language label:\n{screen}");
        assert!(screen.contains("1 line(s)"), "missing summary:\n{screen}");
    }

    #[test]
    fn an_edit_shows_unchanged_neighbours_as_context() {
        // The whole point of the diff: five lines in, one changed. The four
        // that did not change must not appear as removals and additions.
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.transcript
            .push(edit_entry("m.rs", "a\nb\nOLD\nd\ne", "a\nb\nNEW\nd\ne"));

        let (rows, _) = render(&mut app, 70, 20);
        let screen = transcript_only(&rows);
        assert!(screen.contains("+1 -1"), "one line each way:\n{screen}");
        assert!(screen.contains("- OLD"), "{screen}");
        assert!(screen.contains("+ NEW"), "{screen}");
        for unchanged in ["a", "b", "d", "e"] {
            assert!(
                !screen.contains(&format!("- {unchanged}")),
                "{unchanged} did not change but is shown as removed:\n{screen}"
            );
            assert!(
                !screen.contains(&format!("+ {unchanged}")),
                "{unchanged} did not change but is shown as added:\n{screen}"
            );
        }
    }

    #[test]
    fn a_write_over_an_existing_file_renders_its_diff() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.transcript.push(Entry::Action {
            action: crate::protocol::Action::Write {
                path: "conf.json".into(),
                contents: "{\n  \"n\": 2\n}\n".into(),
            },
            usage: None,
            diff: crate::diff::lines("{\n  \"n\": 1\n}\n", "{\n  \"n\": 2\n}\n"),
        });

        let (rows, _) = render(&mut app, 70, 16);
        let screen = transcript_only(&rows);
        assert!(screen.contains("json"), "missing language label:\n{screen}");
        assert!(screen.contains("+1 -1"), "missing summary:\n{screen}");
        // Marked in the gutter, with the source line's own indentation intact.
        let marked = |marker: char, text: &str| {
            screen.lines().any(|l| {
                l.trim_start().trim_start_matches('│').starts_with(marker) && l.contains(text)
            })
        };
        assert!(marked('-', r#""n": 1"#), "{screen}");
        assert!(marked('+', r#""n": 2"#), "{screen}");
    }

    #[test]
    fn a_token_split_across_a_wrap_boundary_keeps_its_colour() {
        // The reason this uses `wrap::line` and its byte offsets rather than
        // `wrap::text`: a long string literal wraps, and both halves must still
        // read as a string.
        let line = r#"let s = "aaaaaaaaaabbbbbbbbbb";"#;
        let spans = highlight::spans(line, highlight::Language::Rust);
        let rows = wrap::line(line, 16);
        assert!(
            rows.len() > 1,
            "the line should wrap for this to mean anything"
        );

        let string_colour = token_colour(highlight::Token::Str);
        let mut halves = 0;
        for row in &rows {
            for span in highlighted(&row.text, row.start, &spans, None) {
                if span.style.fg == Some(string_colour) {
                    halves += 1;
                    assert!(
                        span.content.contains('a') || span.content.contains('b'),
                        "only the literal should be string-coloured: {span:?}"
                    );
                }
            }
        }
        assert!(halves >= 2, "the literal should be coloured on both rows");

        // And the rows still reconstruct the original line exactly.
        let rebuilt: String = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(rebuilt, line);
    }

    #[test]
    fn a_code_block_never_overflows_its_width() {
        // Highlighting splits a line into spans; the gutter and the wrap width
        // have to stay in agreement or long lines spill past the edge.
        let long = format!("let s = \"{}\"; // {}", "x".repeat(120), "y".repeat(80));
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.transcript.push(Entry::Action {
            action: crate::protocol::Action::Edit {
                path: "m.rs".into(),
                old: long.clone(),
                new: format!("{long} more"),
            },
            usage: None,
            diff: None,
        });

        for width in [40u16, 55, 80] {
            let (rows, _) = render(&mut app, width, 40);
            for row in &rows {
                assert!(
                    row.chars().count() <= width as usize,
                    "row overflows {width}: {row:?}"
                );
            }
        }
    }

    #[test]
    fn a_deletion_edit_shows_removals_and_nothing_added() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.transcript.push(edit_entry("x", "remove me\n", ""));
        let (rows, _) = render(&mut app, 60, 12);
        let screen = transcript_only(&rows);
        assert!(
            screen.contains("- remove me"),
            "missing removed line:\n{screen}"
        );
        // The header carries what the old "(deleted)" marker used to: a diff
        // with removals and no additions is a deletion, and says so by counting.
        assert!(
            screen.contains("+0 -1"),
            "the summary should show nothing added:\n{screen}"
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
        // Entries from the head of the table: the menu is height-capped, so
        // asserting on one further down would be a test of the cap instead —
        // which `the_menu_scrolls_to_keep_a_deep_selection_visible` already is.
        for name in ["/debug", "/auto", "/plan"] {
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
        let plan_row = rows.iter().position(|r| r.contains("/plan")).unwrap();
        assert!(debug_row < plan_row, "menu order should follow the table");

        // Moving the highlight must not reorder or drop entries.
        app.move_completion(1);
        let (rows, _) = render(&mut app, 70, 20);
        let screen = rows.join("\n");
        assert!(screen.contains("/debug") && screen.contains("/plan"));
    }

    #[test]
    fn menu_is_absent_for_an_ordinary_prompt() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("what is 2+2");
        let (rows, _) = render(&mut app, 70, 20);
        // The menu's bordered title, not the bare word — which the status bar
        // also says, and matching that would pass or fail for the wrong reason.
        assert!(
            !rows.join("\n").contains(" commands "),
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

    /// The point of the feature: a reasoning model streams for a long time
    /// before its first content token, and a spinner says less than the text
    /// the API is already sending.
    #[test]
    fn reasoning_replaces_the_spinner_and_sits_above_the_reply() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_reasoning("weighing the options");

        let (rows, _) = render(&mut app, 60, 20);
        let screen = transcript_only(&rows);
        assert!(screen.contains("reasoning"), "missing header:\n{screen}");
        assert!(screen.contains("weighing the options"), "{screen}");
        assert!(
            !screen.contains("thinking…"),
            "the window says what the spinner said, with more:\n{screen}"
        );

        // And the reply lands underneath it, not in place of it.
        app.push_delta("Hello.");
        let (rows, _) = render(&mut app, 60, 20);
        let screen = transcript_only(&rows);
        let thought = screen.find("weighing the options").expect("trace");
        let said = screen.find("Hello.").expect("reply");
        assert!(thought < said, "reasoning goes above the reply:\n{screen}");
    }

    #[test]
    fn a_long_trace_is_capped_and_says_what_it_hid() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        for i in 0..40 {
            app.push_reasoning(&format!("step {i}\n"));
        }

        let (rows, _) = render(&mut app, 60, 24);
        let screen = transcript_only(&rows);
        assert!(
            screen.contains("earlier line(s)"),
            "a long trace must report what scrolled past:\n{screen}"
        );
        assert!(
            screen.contains("step 39"),
            "the newest must show:\n{screen}"
        );
        assert!(
            !screen.contains("step 0\n"),
            "the oldest must not:\n{screen}"
        );
    }

    #[test]
    fn reasoning_is_not_drawn_when_it_is_turned_off() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.show_reasoning = false;
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_reasoning("weighing the options");

        let (rows, _) = render(&mut app, 60, 20);
        let screen = transcript_only(&rows);
        assert!(!screen.contains("weighing the options"), "{screen}");
        assert!(!screen.contains("reasoning"), "{screen}");
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

    /// An app watching a command, as the event loop leaves it.
    fn running(command: &str) -> App {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("do it");
        app.submit().unwrap();
        app.start_running(command.into());
        app.status = crate::app::Status::Running;
        app
    }

    #[test]
    fn a_running_command_gets_an_outlined_window() {
        let mut app = running("cargo build");
        app.push_command_chunk(false, "compiling\n");

        let (rows, _) = render(&mut app, 60, 20);
        let screen = transcript_only(&rows);
        // `┌─` is the window's own corner; the transcript's is `┌ ai-harness`.
        assert!(screen.contains("┌─"), "no outline:\n{screen}");
        assert!(screen.contains("cargo build"), "missing command:\n{screen}");
        assert!(screen.contains("compiling"), "missing output:\n{screen}");
        assert!(screen.contains("Esc cancels"), "missing hint:\n{screen}");
    }

    #[test]
    fn the_window_replaces_the_running_spinner() {
        // Both occupy the same slot; showing them together would be two claims
        // about the same state.
        let mut app = running("sleep 5");
        let (rows, _) = render(&mut app, 60, 16);
        let screen = transcript_only(&rows);
        assert!(screen.contains("sleep 5"));
        assert!(
            !screen.contains("running…"),
            "spinner should be gone:\n{screen}"
        );
    }

    #[test]
    fn the_window_keeps_the_newest_output_and_says_what_it_dropped() {
        let mut app = running("spew");
        for i in 0..60 {
            app.push_command_chunk(false, &format!("line {i}\n"));
        }

        let (rows, _) = render(&mut app, 60, 30);
        let screen = transcript_only(&rows);
        assert!(
            screen.contains("line 59"),
            "newest output must show:\n{screen}"
        );
        assert!(
            screen.contains("earlier line(s)"),
            "missing marker:\n{screen}"
        );
        assert!(!screen.contains("line 0\n"), "oldest should be dropped");
    }

    #[test]
    fn the_window_never_overflows_its_width() {
        let mut app = running(&"a-very-long-command ".repeat(10));
        app.push_command_chunk(false, &format!("{}\n", "x".repeat(300)));

        for width in [40u16, 60, 100] {
            let (rows, _) = render(&mut app, width, 30);
            for row in &rows {
                assert!(
                    row.chars().count() <= width as usize,
                    "row overflows {width}: {row:?}"
                );
            }
        }
    }

    #[test]
    fn the_window_goes_away_when_the_command_finishes() {
        let mut app = running("ls");
        app.push_command_chunk(false, "a.txt\n");
        app.push_command_result(crate::exec::CommandOutput {
            command: "ls".into(),
            exit_code: Some(0),
            stdout: "a.txt".into(),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
            cancelled: false,
        });

        let (rows, _) = render(&mut app, 60, 20);
        let screen = transcript_only(&rows);
        assert!(
            !screen.contains("┌─"),
            "the window should be gone:\n{screen}"
        );
        assert!(
            !screen.contains("Esc cancels"),
            "and its hint with it:\n{screen}"
        );
        assert!(
            screen.contains("exit 0"),
            "the result replaces it:\n{screen}"
        );
    }

    /// An app sitting on a question from the model.
    fn asked(question: &str, choices: &[&str]) -> App {
        let mut body =
            format!("<ai-harness-option-question>{question}</ai-harness-option-question>");
        for choice in choices {
            body.push_str(&format!(
                "<ai-harness-option-choice>{choice}</ai-harness-option-choice>"
            ));
        }
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("build it");
        app.submit().unwrap();
        app.push_response(
            format!("<ai-harness-option>{body}</ai-harness-option>"),
            None,
        );
        app
    }

    /// An app with a finished plan, waiting to be told whether to carry it out.
    ///
    /// Writes a real plan file: the panel only appears for a plan that exists,
    /// which is the point of that rule.
    fn plan_ready() -> (App, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "ai-harness-uiplan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut app = App::new("test/model".into(), None, 10, dir.clone());
        app.run_command(crate::command::Command::Plan(None));
        std::fs::write(app.plan_path().unwrap(), "# Plan\n").unwrap();
        app.input.insert_str("plan it");
        app.submit().unwrap();
        app.push_response(
            "<ai-harness-response>Plan written.</ai-harness-response>".into(),
            None,
        );
        assert!(app.executing().is_some(), "should be awaiting the decision");
        (app, dir)
    }

    /// The row index of the panel's top border, and of the transcript's bottom.
    fn panel_top(rows: &[String], title: &str) -> usize {
        rows.iter()
            .position(|r| r.contains(title))
            .unwrap_or_else(|| panic!("no panel titled {title:?} in:\n{}", rows.join("\n")))
    }

    #[test]
    fn every_panel_sits_at_the_bottom_in_the_prompts_place() {
        let (planned, plan_dir) = plan_ready();
        let cases: Vec<(App, &str)> = vec![
            (asked("Which?", &["a", "b"]), "the model is asking"),
            (awaiting_approval("ls -la"), "run this command?"),
            (planned, "execute this plan?"),
        ];
        for (mut app, title) in cases {
            let (rows, _) = render(&mut app, 70, 20);
            let top = panel_top(&rows, title);
            let transcript_bottom = rows
                .iter()
                .position(|r| r.starts_with('└'))
                .expect("the transcript closes");
            assert!(
                top > transcript_bottom,
                "{title} should be below the transcript:\n{}",
                rows.join("\n")
            );
            // And in the prompt's slot: its own bottom border is the last row on
            // screen, with nothing drawn after it.
            assert!(
                rows.last().is_some_and(|r| r.starts_with('└')),
                "{title} should close out the screen:\n{}",
                rows.join("\n")
            );
        }
        let _ = std::fs::remove_dir_all(&plan_dir);
    }

    #[test]
    fn the_execute_panel_names_the_plan_and_offers_both_ways_out() {
        let (mut app, dir) = plan_ready();
        let (rows, metrics) = render(&mut app, 80, 20);
        let screen = rows.join("\n");

        assert!(screen.contains("plan.md"), "name the file:\n{screen}");
        assert!(screen.contains("Execute"), "missing Execute:\n{screen}");
        assert!(
            screen.contains("Keep planning"),
            "the way out must be as visible as the way on:\n{screen}"
        );
        // Same rects as an approval, so clicks work without new plumbing.
        assert!(metrics.allow_button.is_some());
        assert!(metrics.deny_button.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_status_bar_marks_plan_mode() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        let (rows, _) = render(&mut app, 70, 12);
        assert!(!rows.join("\n").contains("plan"));

        app.run_command(crate::command::Command::Plan(None));
        let (rows, _) = render(&mut app, 70, 12);
        assert!(rows.join("\n").contains("plan"), "{}", rows.join("\n"));
    }

    #[test]
    fn the_transcript_gives_up_rows_to_a_panel_rather_than_being_covered() {
        let mut idle = App::new("test/model".into(), None, 10, std::env::temp_dir());
        let (_, plain) = render(&mut idle, 70, 22);

        let mut app = asked("Which?", &["a", "b", "c"]);
        let (_, with_panel) = render(&mut app, 70, 22);

        assert!(
            with_panel.transcript_height < plain.transcript_height,
            "the transcript should shrink: {} vs {}",
            with_panel.transcript_height,
            plain.transcript_height
        );
    }

    #[test]
    fn a_long_panel_caps_rather_than_filling_the_screen() {
        // 25 choices on a short terminal: the panel gives way, the list scrolls.
        let many: Vec<String> = (1..=25).map(|i| format!("choice {i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let mut app = asked("Which?", &refs);

        let (rows, metrics) = render(&mut app, 60, 16);
        assert!(
            metrics.transcript_height >= MIN_TRANSCRIPT_ROWS.saturating_sub(2),
            "the transcript kept {} rows:\n{}",
            metrics.transcript_height,
            rows.join("\n")
        );
        assert!(
            rows.iter().any(|r| r.contains("choice 1")),
            "the selection stays visible:\n{}",
            rows.join("\n")
        );
    }

    #[test]
    fn a_question_modal_shows_the_question_and_numbered_choices() {
        let mut app = asked("Which database?", &["Postgres", "SQLite"]);
        let (rows, _) = render(&mut app, 70, 20);
        let screen = rows.join("\n");

        assert!(screen.contains("the model is asking"), "{screen}");
        assert!(screen.contains("Which database?"), "{screen}");
        assert!(screen.contains("1. Postgres"), "{screen}");
        assert!(screen.contains("2. SQLite"), "{screen}");
        assert!(
            screen.contains("something else"),
            "the free-text row should be offered:\n{screen}"
        );
    }

    #[test]
    fn the_question_modal_marks_the_selection_and_moves_it() {
        let mut app = asked("Which?", &["alpha", "beta"]);
        let (rows, _) = render(&mut app, 70, 20);
        let marked =
            |rows: &[String], text: &str| rows.iter().any(|r| r.contains('›') && r.contains(text));
        assert!(marked(&rows, "alpha"), "first choice starts focused");

        app.question_move(1);
        let (rows, _) = render(&mut app, 70, 20);
        assert!(marked(&rows, "beta"), "the marker follows the selection");
        assert!(!marked(&rows, "alpha"));
    }

    #[test]
    fn the_free_text_row_becomes_an_editor_when_focused() {
        let mut app = asked("Which?", &["a", "b"]);
        app.question_move(-1);
        app.question_input(|input| input.insert_str("something of my own"));

        let (rows, _) = render(&mut app, 70, 20);
        let screen = rows.join("\n");
        assert!(screen.contains("something of my own"), "{screen}");
        assert!(
            screen.contains("type your answer"),
            "the footer should change with the row:\n{screen}"
        );
    }

    #[test]
    fn a_question_modal_never_overflows_its_width() {
        let mut app = asked(
            &"a really quite long question that will certainly need wrapping ".repeat(3),
            &["a very long choice ".repeat(6).as_str(), "short"],
        );
        for width in [40u16, 60, 90] {
            let (rows, _) = render(&mut app, width, 30);
            for row in &rows {
                assert!(
                    row.chars().count() <= width as usize,
                    "row overflows {width}: {row:?}"
                );
            }
        }
    }

    #[test]
    fn an_answered_question_stays_in_the_transcript_with_its_answer() {
        let mut app = asked("Which database?", &["Postgres", "SQLite"]);
        app.answer_question().unwrap();

        let (rows, _) = render(&mut app, 70, 24);
        let screen = transcript_only(&rows);
        assert!(screen.contains("question"), "the ask is kept:\n{screen}");
        assert!(screen.contains("Which database?"), "{screen}");
        assert!(
            screen.contains("Postgres"),
            "the answer is shown:\n{screen}"
        );
        assert!(!screen.contains("the model is asking"), "the modal is gone");
    }

    #[test]
    fn status_bar_shows_the_auto_approve_marker() {
        // With no modal to interrupt you, the status bar is the only standing
        // signal that the harness will act on its own.
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        let (rows, _) = render(&mut app, 70, 12);
        assert!(!rows.join("\n").contains("auto-approve"));

        app.auto_approve = true;
        let (rows, _) = render(&mut app, 70, 12);
        assert!(
            rows.join("\n").contains("auto-approve"),
            "auto-approve must be visible in the status bar:\n{}",
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
        for (i, name) in names.iter().enumerate() {
            let mut session = crate::session::Session::new(
                "m".into(),
                vec![],
                vec![],
                vec![],
                Default::default(),
            );
            session.saved_at = pinned_saved_at(i);
            crate::session::save(&dir, name, &session).unwrap();
        }
        let mut app = App::new("test/model".into(), None, 10, dir.clone());
        app.open_load_picker();
        (app, dir)
    }

    /// A running session should be findable, marked as somewhere to go — hiding
    /// it would make one you know exists read as one that is gone.
    #[test]
    fn the_picker_dots_a_running_session_and_names_the_current_one() {
        let (mut app, dir) = app_with_picker(&["running", "here", "archived"]);
        app.set_open_elsewhere(vec!["running".into()]);
        // The picker snapshots on open, so it has to be reopened to see this.
        app.open_load_picker();

        let (rows, _) = render(&mut app, 70, 20);
        let row = |needle: &str| {
            rows.iter()
                .find(|r| r.contains(needle))
                .unwrap_or_else(|| panic!("no row for {needle}:\n{}", rows.join("\n")))
                .clone()
        };
        assert!(row("running").contains("● running"), "{:?}", row("running"));
        assert!(
            !row("archived").contains('●'),
            "a saved session is not running: {:?}",
            row("archived")
        );

        // And the session the picker was opened from, marked the way the
        // sessions view marks it.
        app.picker_cancel();
        app.input.insert_str("/save here");
        app.submit();
        app.open_load_picker();
        let (rows, _) = render(&mut app, 70, 20);
        let here = rows
            .iter()
            .find(|r| r.contains("here"))
            .expect("the current session is listed");
        assert!(
            here.contains("● here") && here.contains("‹current›"),
            "{here:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The footer answers "what will Enter do", which differs per row — a static
    /// legend would have to describe all three cases and so describe none.
    #[test]
    fn the_picker_footer_follows_the_highlighted_row() {
        let (mut app, dir) = app_with_picker(&["running", "archived"]);
        app.set_open_elsewhere(vec!["running".into()]);
        app.open_load_picker();

        let (rows, _) = render(&mut app, 70, 20);
        assert!(
            rows.iter().any(|r| r.contains("Enter switches to it")),
            "on the running row:\n{}",
            rows.join("\n")
        );

        app.picker_move(1);
        let (rows, _) = render(&mut app, 70, 20);
        assert!(
            rows.iter().any(|r| r.contains("Enter load")),
            "on the saved row:\n{}",
            rows.join("\n")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two leading columns plus a suffix come out of the name's budget, or a
    /// wide model runs off the panel. That arithmetic has broken twice here.
    #[test]
    fn a_marked_picker_row_still_fits_its_panel() {
        let (mut app, dir) = app_with_picker(&["a-fairly-long-session-name"]);
        app.set_open_elsewhere(vec!["a-fairly-long-session-name".into()]);
        app.open_load_picker();

        for width in [40, 52, 70] {
            let (rows, _) = render(&mut app, width, 20);
            for row in &rows {
                assert!(
                    row.chars().count() <= width as usize,
                    "row overflows at width {width}: {row:?}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A save time that puts `names[i]` at position `i` in the picker.
    ///
    /// The picker orders by recency, so without this the fixtures depend on a
    /// loop of `Session::new` calls all landing in the same second — and when one
    /// straddles a boundary the order flips and the test flakes. Descending, so
    /// the first name is the most recent and the listed order is the given one.
    fn pinned_saved_at(i: usize) -> u64 {
        2_000_000_000 - i as u64
    }

    /// A picker over sessions that each have a one-line preview.
    fn app_with_previews(names: &[&str]) -> (App, std::path::PathBuf) {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ai-harness-ui-preview-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (i, name) in names.iter().enumerate() {
            let mut session = crate::session::Session::new(
                "m".into(),
                vec![],
                vec![Entry::User(format!("what {name} was about"))],
                vec![],
                Default::default(),
            );
            session.saved_at = pinned_saved_at(i);
            crate::session::save(&dir, name, &session).unwrap();
        }
        let mut app = App::new("test/model".into(), None, 10, dir.clone());
        app.open_load_picker();
        (app, dir)
    }

    #[test]
    fn a_picker_entry_names_the_model_it_would_load() {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ai-harness-ui-picker-model-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (name, model) in [
            ("alpha", "anthropic/claude-opus-5"),
            ("beta", "deepseek/deepseek-v4-pro"),
        ] {
            let session = crate::session::Session::new(
                model.into(),
                vec![],
                vec![Entry::User(format!("what {name} was about"))],
                vec![],
                Default::default(),
            );
            crate::session::save(&dir, name, &session).unwrap();
        }
        let mut app = App::new("test/model".into(), None, 10, dir.clone());
        app.open_load_picker();

        let (rows, _) = render(&mut app, 70, 24);
        let screen = rows.join("\n");
        let alpha = rows
            .iter()
            .find(|r| r.contains("alpha"))
            .expect("alpha's row");
        assert!(
            alpha.contains("anthropic/claude-opus-5"),
            "the model belongs on the name's row:\n{screen}"
        );
        assert!(
            alpha
                .trim_end()
                .trim_end_matches('│')
                .trim_end()
                .ends_with("anthropic/claude-opus-5"),
            "and right-aligned against the panel's inner edge:\n{alpha:?}"
        );
        assert!(
            rows.iter()
                .any(|r| r.contains("beta") && r.contains("deepseek/deepseek-v4-pro")),
            "every session names its own model:\n{screen}"
        );
        for row in &rows {
            assert!(row.chars().count() <= 70, "row overflowed: {row:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_picker_entry_shows_its_name_a_rule_and_its_last_lines() {
        let (mut app, dir) = app_with_previews(&["alpha", "beta"]);
        let (rows, _) = render(&mut app, 70, 24);
        let screen = rows.join("\n");

        assert!(screen.contains("alpha"), "{screen}");
        assert!(screen.contains("what alpha was about"), "{screen}");
        assert!(screen.contains('─'), "a rule under the name:\n{screen}");
        // The gap is what makes each entry read as one thing.
        assert!(
            screen.contains("what alpha was about"),
            "entries should be separated:\n{screen}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_click_anywhere_in_an_entry_selects_that_entry() {
        // The regression the row map exists to prevent: an entry spans a name, a
        // rule, its lines, and a gap, so a click on any of them means that
        // entry — not the neighbour an offset would have computed.
        let (mut app, dir) = app_with_previews(&["alpha", "beta", "gamma"]);
        let (_, metrics) = render(&mut app, 70, 30);
        let rows = &metrics.picker_rows;

        // Checked at two entries, so an off-by-one in the map cannot pass.
        for target in [1usize, 2] {
            let owned: Vec<usize> = rows
                .iter()
                .enumerate()
                .filter(|(_, owner)| **owner == target)
                .map(|(row, _)| row)
                .collect();
            assert!(
                owned.len() > 1,
                "entry {target} should span several rows, got {owned:?}"
            );
            for row in owned {
                assert_eq!(
                    rows[row], target,
                    "row {row} should belong to entry {target}: {rows:?}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_without_a_preview_renders_as_a_bare_name() {
        // Sessions saved before previews existed are not backfilled; they must
        // not break the layout.
        let (mut app, dir) = app_with_previews(&["alpha"]);
        std::fs::remove_file(dir.join("alpha").join(crate::session::PREVIEW_FILE)).unwrap();
        app.open_load_picker();

        let (rows, _) = render(&mut app, 70, 20);
        let screen = rows.join("\n");
        assert!(screen.contains("alpha"), "{screen}");
        assert!(
            !screen.contains("what alpha was about"),
            "no stale preview:\n{screen}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unlike every other panel, this one takes the screen rather than sizing to
    /// its contents — see `prepare_panel`. A deep selection still has to be in
    /// view, which is the part the height was originally derived to guarantee.
    #[test]
    fn a_previewed_picker_fills_the_screen_with_the_selection_visible() {
        let names: Vec<String> = (0..12).map(|i| format!("s{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let (mut app, dir) = app_with_previews(&refs);
        for _ in 0..11 {
            app.picker_move(1);
        }

        let (rows, metrics) = render(&mut app, 60, 20);
        assert!(
            rows.iter().any(|r| r.contains("s11")),
            "the deep selection must be in view:\n{}",
            rows.join("\n")
        );
        assert_eq!(
            metrics.transcript_height, 0,
            "the picker takes the whole screen bar the status line"
        );
        for row in &rows {
            assert!(row.chars().count() <= 60, "row overflows: {row:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The reason the height is fixed: rows come and go as you type, and a panel
    /// that resized under them moved the row you were reading towards.
    #[test]
    fn filtering_the_picker_does_not_move_it() {
        let names: Vec<String> = (0..12).map(|i| format!("s{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let (mut app, dir) = app_with_previews(&refs);

        let height = |app: &mut App| {
            let (rows, _) = render(app, 60, 24);
            // The panel's top border, wherever it has ended up.
            let top = rows
                .iter()
                .position(|r| r.contains("load session"))
                .unwrap();
            rows.len() - top
        };

        let full = height(&mut app);
        app.picker_query_input(|input| input.insert_str("s0"));
        assert_eq!(height(&mut app), full, "narrowing must not resize it");
        app.picker_query_input(|input| input.insert_str("zzz"));
        assert_eq!(height(&mut app), full, "and neither must matching nothing");
        let _ = std::fs::remove_dir_all(&dir);
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
        assert_eq!(
            metrics.picker_rows.first().copied(),
            Some(0),
            "the first rendered row belongs to the first session"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn picker_shows_the_query_and_narrows_to_it() {
        let (mut app, dir) = app_with_picker(&["alpha", "beta", "gamma"]);
        let (rows, _) = render(&mut app, 60, 20);
        // Below the panel's top edge, so this is the query row rather than the
        // status bar — which says something similar and would otherwise let
        // this pass without the row being drawn at all.
        let top = panel_top(&rows, "load session");
        assert!(
            rows[top..].join("\n").contains("/ to search"),
            "an empty query must say how to start one:\n{}",
            rows.join("\n")
        );

        app.picker_query_input(|input| input.insert_str("bet"));
        let (rows, _) = render(&mut app, 60, 20);
        let screen = rows.join("\n");
        assert!(screen.contains("bet"), "the query must show:\n{screen}");
        assert!(screen.contains("beta"), "the match must show:\n{screen}");
        assert!(!screen.contains("alpha"), "filtered out:\n{screen}");
        assert!(!screen.contains("gamma"), "filtered out:\n{screen}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The row has to say which mode the keyboard is in, or you find out by
    /// pressing a letter and watching what happens.
    #[test]
    fn the_query_row_says_whether_it_is_taking_keystrokes() {
        let (mut app, dir) = app_with_picker(&["alpha", "beta"]);
        let (rows, _) = render(&mut app, 60, 20);
        let top = panel_top(&rows, "load session");
        assert!(rows[top..].join("\n").contains("/ to search"));
        assert!(
            rows.join("\n").contains("/ search"),
            "and so does the footer:\n{}",
            rows.join("\n")
        );

        app.picker_search(true);
        app.picker_query_input(|input| input.insert_str("bet"));
        let (rows, _) = render(&mut app, 60, 20);
        let screen = rows[panel_top(&rows, "load session")..].join("\n");
        assert!(
            screen.contains("/bet"),
            "the search shows its own `/`:\n{screen}"
        );
        assert!(
            rows.join("\n").contains("typing filters"),
            "and the footer changes with it:\n{}",
            rows.join("\n")
        );

        // Back to navigating: the filter stays, and the row says it is still on.
        app.picker_search(false);
        let (rows, _) = render(&mut app, 60, 20);
        let screen = rows[panel_top(&rows, "load session")..].join("\n");
        assert!(
            screen.contains("/bet"),
            "a filter in force is shown:\n{screen}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert!(screen.contains("beta") && !screen.contains("alpha"));
    }

    #[test]
    fn picker_says_when_nothing_matches() {
        let (mut app, dir) = app_with_picker(&["alpha", "beta"]);
        app.picker_query_input(|input| input.insert_str("zzz"));
        let (rows, _) = render(&mut app, 60, 20);
        assert!(
            rows.join("\n").contains("no session matches"),
            "an empty list must say so rather than render blank:\n{}",
            rows.join("\n")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The row map holds positions in the filtered list, so a click on the one
    /// remaining row reaches the session the filter left rather than the one
    /// that happens to sit at that ordinal in the whole list.
    #[test]
    fn a_click_on_a_filtered_picker_maps_to_the_match() {
        let (mut app, dir) = app_with_picker(&["alpha", "beta", "gamma"]);
        app.picker_query_input(|input| input.insert_str("gamma"));
        let (_, metrics) = render(&mut app, 60, 20);

        assert_eq!(
            metrics.picker_rows.first().copied(),
            Some(0),
            "the only rendered row is position 0 of the matches"
        );
        assert!(app.picker_select(0), "and it is selectable");
        assert_eq!(
            app.picker_matches(),
            vec![2],
            "position 0 is the third session"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn picker_highlights_the_selection_and_a_click_maps_to_it() {
        let (mut app, dir) = app_with_picker(&["one", "two", "three"]);
        app.picker_move(1); // second row
        // Whichever entry that is: the fixtures are saved in the same second, so
        // the recency order they are listed in falls back to the name.
        let selected = app.picker().unwrap().sessions[1].clone();
        let (rows, metrics) = render(&mut app, 60, 20);
        // The focus marker leads, then the state column — two leading columns,
        // as in the sessions view. These fixtures are not running, so the state
        // column is blank.
        assert!(
            rows.iter().any(|r| r.contains(&format!("›   {selected}"))),
            "the selected row should carry the marker:\n{}",
            rows.join("\n")
        );

        // A click anywhere in that entry maps back to index 1 via the row map.
        let row = metrics
            .picker_rows
            .iter()
            .position(|owner| *owner == 1)
            .expect("the selected entry should own some rows");
        assert_eq!(
            metrics.picker_rows[row], 1,
            "the row map recovers the index"
        );
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
        assert!(
            metrics.picker_rows.first().copied().unwrap_or(0) > 0,
            "a long list must scroll past the first session"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An app with a small catalog and the model picker open.
    fn app_with_models() -> App {
        let mut app = App::new("alpha/one".into(), None, 10, std::env::temp_dir());
        app.set_catalog(Ok(vec![
            crate::openrouter::ModelInfo {
                id: "alpha/one".into(),
                name: "Alpha: One".into(),
                context_length: Some(200_000),
                pricing: Some(crate::openrouter::Pricing {
                    prompt: "0.000005".into(),
                    completion: "0.000025".into(),
                }),
            },
            crate::openrouter::ModelInfo {
                id: "beta/free-model".into(),
                name: "Beta: Free".into(),
                context_length: Some(32_768),
                pricing: Some(crate::openrouter::Pricing {
                    prompt: "0".into(),
                    completion: "0".into(),
                }),
            },
        ]));
        app.open_model_picker();
        app
    }

    #[test]
    fn model_picker_lists_ids_with_context_and_price() {
        let mut app = app_with_models();
        let (rows, metrics) = render(&mut app, 60, 20);
        let screen = rows.join("\n");

        assert!(
            screen.contains("choose a model"),
            "missing title:\n{screen}"
        );
        assert!(screen.contains("alpha/one"), "missing id:\n{screen}");
        assert!(
            screen.contains("200K") && screen.contains("$5.00/$25.00"),
            "a row should carry its context and price:\n{screen}"
        );
        assert!(
            screen.contains("32K") && screen.contains("free"),
            "a free model should say so:\n{screen}"
        );
        assert!(screen.contains("Enter select"), "missing footer:\n{screen}");
        assert!(
            metrics.models_list.is_some(),
            "the list rect must be reported for clicks"
        );
    }

    #[test]
    fn model_picker_marks_the_selection_and_shows_the_query() {
        let mut app = app_with_models();
        app.model_query_input(|input| input.insert_str("beta"));
        let (rows, _) = render(&mut app, 60, 20);
        let screen = rows.join("\n");

        assert!(screen.contains("beta"), "the query should show:\n{screen}");
        assert!(
            rows.iter().any(|r| r.contains("› beta/free-model")),
            "the single match should be marked:\n{screen}"
        );
        // Scoped to the panel: the status bar names the model in use, which is
        // the one being filtered out here.
        let panel = rows
            .iter()
            .skip_while(|r| !r.contains("choose a model"))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !panel.contains("alpha/one"),
            "a filtered-out model should be gone from the list:\n{panel}"
        );
    }

    #[test]
    fn model_picker_says_when_nothing_matches_and_while_loading() {
        let mut app = app_with_models();
        app.model_query_input(|input| input.insert_str("zzz"));
        let (rows, _) = render(&mut app, 60, 20);
        assert!(
            rows.join("\n").contains("no model matches"),
            "an empty result should say so:\n{}",
            rows.join("\n")
        );

        let mut app = App::new("alpha/one".into(), None, 10, std::env::temp_dir());
        app.open_model_picker();
        let (rows, _) = render(&mut app, 60, 20);
        assert!(
            rows.join("\n").contains("loading models"),
            "the picker should stand in for the catalog until it lands:\n{}",
            rows.join("\n")
        );

        app.set_catalog(Err("network unreachable".into()));
        let (rows, _) = render(&mut app, 60, 20);
        let screen = rows.join("\n");
        assert!(
            screen.contains("network unreachable") && screen.contains("/model <id>"),
            "a failed catalog should show the reason and the way round it:\n{screen}"
        );
    }

    #[test]
    fn model_picker_scrolls_to_keep_a_deep_selection_visible() {
        let mut app = App::new("m/0".into(), None, 10, std::env::temp_dir());
        let models = (0..40)
            .map(|i| crate::openrouter::ModelInfo {
                id: format!("m/{i:02}"),
                name: String::new(),
                context_length: None,
                pricing: None,
            })
            .collect();
        app.set_catalog(Ok(models));
        app.open_model_picker();
        app.model_move(39);

        let (rows, metrics) = render(&mut app, 60, 16);
        assert!(
            rows.iter().any(|r| r.contains("› m/39")),
            "the deep selection must be scrolled into view:\n{}",
            rows.join("\n")
        );
        assert!(
            metrics.models_offset > 0,
            "a long list must scroll past the first model"
        );
    }

    #[test]
    fn a_long_model_id_does_not_overflow_the_panel() {
        let mut app = App::new("x".into(), None, 10, std::env::temp_dir());
        app.set_catalog(Ok(vec![crate::openrouter::ModelInfo {
            id: "some-very-long-provider/an-extremely-long-model-identifier-v3".into(),
            name: String::new(),
            context_length: Some(1_000_000),
            pricing: Some(crate::openrouter::Pricing {
                prompt: "0.000005".into(),
                completion: "0.000025".into(),
            }),
        }]));
        app.open_model_picker();

        let (rows, _) = render(&mut app, 40, 16);
        for row in &rows {
            assert!(
                row.chars().count() <= 40,
                "a row overflowed the screen: {row:?}"
            );
        }
        assert!(
            rows.iter().any(|r| r.contains("$5.00/$25.00")),
            "the price must survive a long id:\n{}",
            rows.join("\n")
        );
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

    /// A rejected reply is a failure, not a warning: the action did not happen
    /// and the turn spent a round-trip on nothing. It was drawn in yellow, which
    /// put it beside the sessions view's `!` and the partial-checkpoint note.
    #[test]
    fn a_protocol_error_is_red_like_every_other_failure() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_response("Sure, I can help with that!".into(), None);

        let mut terminal = Terminal::new(TestBackend::new(70, 16)).unwrap();
        let mut cache = TranscriptCache::default();
        terminal
            .draw(|frame| {
                draw(frame, &mut app, &mut cache, (1, 0));
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();

        // Sample the header's own cells, not the row's first column — that is
        // the transcript border.
        let (row, at) = (0..buffer.area.height)
            .find_map(|y| {
                let text: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                text.find("protocol error").map(|at| (y, at as u16))
            })
            .expect("the header is on screen");
        assert_eq!(
            buffer[(at, row)].style().fg,
            Some(Color::Red),
            "the header should read as a failure"
        );
    }

    /// A transcript of the shape a real coding session produces: prose replies
    /// with fenced code, edits, writes, command output, and protocol frames.
    fn heavy_transcript(app: &mut App, turns: usize) {
        // Roughly the size of a real edited span; the diff cost is quadratic in
        // this, so a toy four-liner would flatter the measurement.
        let body: String = (0..30)
            .map(|n| format!("    let item_{n} = collect(source, {n})?;\n"))
            .collect();
        let code = format!("fn main() -> Result<()> {{\n{body}    Ok(())\n}}\n");
        let code = code.as_str();
        for i in 0..turns {
            app.transcript.push(Entry::User(format!(
                "turn {i}: please refactor the collector and explain what changed"
            )));
            app.transcript.push(Entry::Action {
                action: Action::Response(format!(
                    "## Turn {i}\n\nHere is what I changed, and *why* it matters:\n\n\
                     - the collector no longer allocates twice\n\
                     - the error path is now explicit\n\n\
                     ```rust\n{code}```\n\nLet me know if you want it split further."
                )),
                usage: None,
                diff: None,
            });
            // A few lines changed out of many, which is what a real edit looks
            // like: the diff is cheap to *show* and expensive to *compute*.
            app.transcript.push(edit_entry(
                &format!("src/collect_{i}.rs"),
                code,
                &code.replace("collect(source, 7)", "collect(&source, 7)"),
            ));
            app.transcript.push(Entry::Frame {
                direction: Direction::Received,
                body: format!("<edit><path>src/collect_{i}.rs</path><old>{code}</old></edit>"),
            });
        }
    }

    /// A chatty command must not push the conversation off the top of the
    /// scrollback, or sit in the render cache as thousands of styled rows.
    #[test]
    fn long_command_output_is_elided() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        let stdout: String = (0..400).map(|i| format!("stdout line {i}\n")).collect();
        app.transcript
            .push(Entry::CommandResult(Box::new(crate::exec::CommandOutput {
                command: "make".into(),
                exit_code: Some(0),
                stdout,
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                cancelled: false,
            })));

        let (_, metrics) = render(&mut app, 60, 20);
        assert!(
            (metrics.content_height as usize) < 400,
            "output should be bounded, got {} rows",
            metrics.content_height
        );

        // The head of the output stays…
        app.follow = false;
        app.scroll = 0;
        let top = transcript_only(&render(&mut app, 60, 20).0);
        assert!(top.contains("stdout line 0"), "{top}");
        assert!(!top.contains("stdout line 399"), "{top}");

        // …and the count of what was dropped closes it out.
        app.follow = true;
        let bottom = transcript_only(&render(&mut app, 60, 20).0);
        assert!(
            bottom.contains(&format!("{}", 400 - MAX_OUTPUT_PREVIEW)),
            "expected a count of the elided lines:\n{bottom}"
        );
    }

    /// A cached frame and a cold frame must be the same frame. This is the
    /// whole contract of the cache: it may only save work, never change what is
    /// on screen.
    #[test]
    fn a_warm_cache_draws_what_a_cold_one_draws() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        let mut cache = TranscriptCache::default();

        // Grow the transcript a piece at a time, the way a session does, so the
        // cache is appended to rather than built in one go.
        for turn in 0..6 {
            heavy_transcript(&mut app, 1);
            app.transcript.push(Entry::Notice(format!("notice {turn}")));
            let (warm, warm_metrics) = render_cached(&mut app, &mut cache, 80, 24);

            let mut cold_app = App::new("test/model".into(), None, 10, std::env::temp_dir());
            cold_app.transcript = app.transcript.clone();
            cold_app.scroll = app.scroll;
            cold_app.follow = app.follow;
            let (cold, cold_metrics) = render(&mut cold_app, 80, 24);

            assert_eq!(warm, cold, "warm cache diverged from a cold render");
            assert_eq!(warm_metrics.content_height, cold_metrics.content_height);
        }
    }

    /// The cache is keyed on what it was built for. A resize, a `/debug` toggle,
    /// or a `/clear` all mean the stored rows no longer describe the screen.
    #[test]
    fn the_cache_rebuilds_when_its_key_changes() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        heavy_transcript(&mut app, 3);
        // A line long enough to wrap at one width and not the other — otherwise
        // a stale cache and a rewrapped one agree and the resize proves nothing.
        app.transcript.push(Entry::User(
            "this sentence is deliberately long enough that it must wrap onto a \
             second row at a narrow width and stay on one row at a wide one"
                .into(),
        ));
        let mut cache = TranscriptCache::default();
        render_cached(&mut app, &mut cache, 80, 24);

        // A narrower terminal rewraps everything.
        let (narrow, _) = render_cached(&mut app, &mut cache, 48, 24);
        let mut fresh = App::new("test/model".into(), None, 10, std::env::temp_dir());
        fresh.transcript = app.transcript.clone();
        fresh.scroll = app.scroll;
        fresh.follow = app.follow;
        assert_eq!(
            narrow,
            render(&mut fresh, 48, 24).0,
            "resize was not honoured"
        );
        for row in &narrow {
            assert!(row.chars().count() <= 48, "row overflows width: {row:?}");
        }

        // Turning on debug reveals protocol frames that were rendering to nothing.
        let before = render_cached(&mut app, &mut cache, 80, 24).1.content_height;
        app.debug = true;
        let after = render_cached(&mut app, &mut cache, 80, 24).1.content_height;
        assert!(
            after > before,
            "debug frames should appear once /debug is on: {before} -> {after}"
        );

        // Clearing shortens the transcript, which nothing else does.
        app.transcript.clear();
        app.follow = true;
        let (cleared, metrics) = render_cached(&mut app, &mut cache, 80, 24);
        assert!(
            transcript_only(&cleared).contains("Type a prompt"),
            "a cleared transcript should show the opening hint:\n{}",
            cleared.join("\n")
        );
        assert_eq!(metrics.max_scroll(), 0, "nothing left to scroll through");
    }

    /// Scrolling reads out of the middle of the cache rather than off the end of
    /// it, so an offset must land on the same rows a cold render would show.
    #[test]
    fn a_scrolled_warm_cache_shows_the_same_rows_as_a_cold_one() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        heavy_transcript(&mut app, 8);
        let mut cache = TranscriptCache::default();
        let (_, metrics) = render_cached(&mut app, &mut cache, 80, 20);

        for offset in [0, 5, metrics.max_scroll() / 2, metrics.max_scroll()] {
            app.follow = false;
            app.scroll = offset;
            let (warm, _) = render_cached(&mut app, &mut cache, 80, 20);

            let mut cold_app = App::new("test/model".into(), None, 10, std::env::temp_dir());
            cold_app.transcript = app.transcript.clone();
            cold_app.follow = false;
            cold_app.scroll = offset;
            let (cold, _) = render(&mut cold_app, 80, 20);

            assert_eq!(warm, cold, "warm and cold disagree at scroll {offset}");
        }
    }

    /// The live reply is not cached — it changes every frame — but it still has
    /// to sit below the cached history and carry the cursor.
    #[test]
    fn a_streaming_reply_renders_below_the_cached_history() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.transcript.push(Entry::User("earlier turn".into()));
        let mut cache = TranscriptCache::default();
        render_cached(&mut app, &mut cache, 60, 16);

        app.status = Status::Streaming;
        for chunk in ["Here ", "is ", "the answer"] {
            app.push_delta(chunk);
        }
        let (rows, _) = render_cached(&mut app, &mut cache, 60, 16);
        let screen = transcript_only(&rows);

        assert!(screen.contains("earlier turn"), "history lost:\n{screen}");
        assert!(
            screen.contains("Here is the answer▌"),
            "the live reply should show with a cursor:\n{screen}"
        );
    }

    /// A reply taller than the viewport scrolls *into* the live tail: the rows
    /// on screen start partway through it, past the cached history entirely.
    /// Following a long answer as it arrives depends on this.
    #[test]
    fn following_a_reply_taller_than_the_screen_shows_its_end() {
        let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
        app.transcript.push(Entry::User("short question".into()));
        app.status = Status::Streaming;
        for i in 0..60 {
            app.push_delta(&format!("reply line {i}\n"));
        }
        app.push_delta("reply line 60");

        let mut cache = TranscriptCache::default();
        let (rows, _) = render_cached(&mut app, &mut cache, 60, 20);
        let screen = transcript_only(&rows);

        assert!(
            screen.contains("reply line 60▌"),
            "following should show the end of the reply:\n{screen}"
        );
        assert!(
            !screen.contains("reply line 0"),
            "the start of the reply should have scrolled off:\n{screen}"
        );
    }

    /// Not an assertion — a stopwatch. Run with
    /// `cargo test --release render_cost_scales_with_history -- --ignored --nocapture`
    /// to see per-frame cost against transcript size.
    #[test]
    #[ignore = "timing measurement, not a correctness check"]
    fn render_cost_scales_with_history() {
        for turns in [25usize, 50, 100, 200] {
            let mut app = App::new("test/model".into(), None, 10, std::env::temp_dir());
            heavy_transcript(&mut app, turns);

            // The real loop keeps one cache across frames; a fresh one per frame
            // would measure the cold path the cache exists to avoid.
            let mut cache = TranscriptCache::default();
            // One warm-up frame, so the one-time build is not charged to the timing.
            render_cached(&mut app, &mut cache, 100, 40);

            let frames = 20;
            let start = std::time::Instant::now();
            for _ in 0..frames {
                render_cached(&mut app, &mut cache, 100, 40);
            }
            let per_frame = start.elapsed() / frames;
            println!(
                "{:>4} entries -> {per_frame:?} per frame",
                app.transcript.len()
            );
        }
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
