//! Several conversations at once, and the view for moving between them.
//!
//! Each session is a whole [`App`] with its own conversation, settings, model
//! and in-flight work. They run at the same time: a session you are not looking
//! at goes on streaming and running commands, and its updates find their way
//! back to it by id rather than by being the only thing running.
//!
//! What a background session *cannot* do is ask you something. An approval modal
//! belongs to a screen, and it is not on screen — so a session that proposes
//! something needing approval parks in [`crate::app::Status::AwaitingApproval`]
//! and the view marks it as wanting you. Under `--auto-approve` it carries on
//! instead, and that falls out of the existing dispatch rather than being a case
//! anything here handles.
//!
//! They share one working directory. Nothing isolates them from each other — see
//! the note on checkpoints in the README, which is the sharp edge of that.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::oneshot;

use crate::app::{App, Catalog, Status};
use crate::input::Input;
use crate::session::OpenSet;
use crate::ui::TranscriptCache;

/// Sessions named in the record that are no longer on disk.
///
/// Named rather than passed over: a session that is gone is more likely to be a
/// surprise than a decision you remember making.
fn missing_notice(missing: &[String]) -> String {
    format!("Could not reopen {}: no longer saved.", missing.join(", "))
}

/// Most sessions kept at once.
///
/// Each holds an entire conversation in memory, and a screenful of them is
/// already more than anyone is tracking. The cap is a memory bound and a
/// usability one at the same time.
pub const MAX_SESSIONS: usize = 8;

/// A handle to a session's current background task.
///
/// Sending or dropping resolves the task's cancel future, stopping its work
/// cleanly. One per session rather than one for the harness: that is what lets
/// two sessions have something in flight at the same time.
pub struct InFlight {
    pub cancel: oneshot::Sender<()>,
}

impl InFlight {
    pub fn new(cancel: oneshot::Sender<()>) -> Self {
        Self { cancel }
    }
}

/// One running session: its conversation, its background work, its rendering.
pub struct Slot {
    /// Stable for the life of the session. An index is not: closing a session
    /// renumbers everything after it, and a reply already in flight would come
    /// back addressed to whichever session had shuffled into its place.
    pub id: u64,
    pub app: App,
    pub inflight: Option<InFlight>,
    /// Travels with the slot rather than the screen: it holds this
    /// conversation's wrapped rows, and rebuilding a long one on every switch is
    /// the cost it exists to avoid.
    pub cache: TranscriptCache,
}

/// What the view shows for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: u64,
    pub name: String,
    pub model: String,
    /// A short word for the session's state — `ready`, `streaming`, `needs you`.
    pub status: &'static str,
    /// Whether it is waiting on a *person* rather than on time. The one state
    /// worth marking, since it is the only one that will not resolve itself.
    pub blocked: bool,
    pub busy: bool,
    pub turns: usize,
    pub focused: bool,
    /// The last few things that happened, oldest first. What the session is
    /// *doing* is the reason to look at this list at all — a column of names and
    /// the word "streaming" says which one is busy but not what with.
    pub activity: Vec<String>,
}

/// Lines of activity shown per session.
///
/// Enough to tell what a session is in the middle of, few enough that eight of
/// them still fit on a screen.
pub const ACTIVITY_LINES: usize = 3;

/// The sessions view, while it is open.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct View {
    /// A position in the *matches*, not in the slots — the query can hide rows
    /// between the highlight and the session it names.
    pub selected: usize,
    /// What has been typed to narrow the list, on the same terms as the `/load`
    /// and `/model` pickers: the list is navigable, and `/` starts a search.
    pub query: Input,
    pub searching: bool,
}

pub struct Sessions {
    slots: Vec<Slot>,
    current: usize,
    view: Option<View>,
    next_id: u64,
    /// The project these sessions belong to, stamped on the open-set record so a
    /// shared `--sessions-dir` cannot resume another project's work.
    root: PathBuf,
    /// The open set as last written, so [`Sessions::maybe_record_open`] can tell
    /// whether anything changed.
    recorded: Option<OpenSet>,
}

impl Sessions {
    /// Start with one session, the way the harness has always started.
    pub fn new(app: App) -> Self {
        let root = app.sandbox.as_ref().map_or_else(
            || app.sessions_dir().to_path_buf(),
            |sandbox| sandbox.root().to_path_buf(),
        );
        Self {
            slots: vec![Slot {
                id: 1,
                app,
                inflight: None,
                cache: TranscriptCache::default(),
            }],
            current: 0,
            view: None,
            next_id: 2,
            root,
            recorded: None,
        }
    }

    /// Reopen the sessions that were open when the harness last quit.
    ///
    /// Falls back to [`Sessions::new`] whenever there is nothing usable to
    /// resume — restore turned off, no record, a record belonging to another
    /// project, or one whose sessions have all been deleted since. `app` is the
    /// template either way: it carries this launch's flags, and each restored
    /// session takes them through [`App::spawn_sibling`] before adopting its own
    /// saved model and conversation.
    pub fn restore(app: App, sessions_dir: &Path, root: &Path) -> Self {
        let Some(open) = crate::session::read_open(sessions_dir) else {
            return Self::new(app);
        };
        if open.root != root {
            // Another project's set, through a shared `--sessions-dir`. The next
            // quit overwrites it with this project's.
            return Self::new(app);
        }

        let mut sessions = Self::new(app);
        let mut missing = Vec::new();
        for name in open.names.iter().take(MAX_SESSIONS) {
            // Always from slot 0, which is the template. Spawning from the
            // previously restored session instead would chain each one's
            // adopted model into the next.
            let mut fresh = sessions.slots[0].app.spawn_sibling(name.clone());
            if !fresh.open_saved(name) {
                missing.push(name.clone());
                continue;
            }
            sessions.push(fresh);
        }

        let restored = sessions.slots.len() - 1;
        if restored == 0 {
            // Nothing came back, so the template stays as the fresh session the
            // harness has always started with — and says why, if it was meant to
            // be more than that.
            if !missing.is_empty() {
                sessions.app_mut().push_notice(missing_notice(&missing));
            }
            return sessions;
        }
        sessions.slots.remove(0);
        sessions.current = open.current.min(sessions.slots.len() - 1);

        sessions
            .app_mut()
            .push_notice(format!("Reopened {restored} session(s) from last time."));
        if !missing.is_empty() {
            sessions.app_mut().push_notice(missing_notice(&missing));
        }
        // Open on the view rather than on a conversation. Resuming work starts
        // with choosing which work, and dropping straight into whichever session
        // happened to have focus last would hide that there are others — the
        // same reason the view exists at all. `Esc` is one key away.
        sessions.open_view();
        sessions
    }

    /// Add a slot holding `app`, focused. The one place a session is appended.
    fn push(&mut self, app: App) {
        let id = self.next_id;
        self.next_id += 1;
        self.slots.push(Slot {
            id,
            app,
            inflight: None,
            cache: TranscriptCache::default(),
        });
        self.current = self.slots.len() - 1;
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn current(&self) -> &Slot {
        &self.slots[self.current]
    }

    pub fn current_mut(&mut self) -> &mut Slot {
        &mut self.slots[self.current]
    }

    /// The focused session's conversation, which is what the prompt and the
    /// transcript are about.
    pub fn app(&self) -> &App {
        &self.current().app
    }

    pub fn app_mut(&mut self) -> &mut App {
        &mut self.current_mut().app
    }

    /// Every session. The loop only ever needs `iter_mut`; this is what the
    /// tests below check the shared state with.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn iter(&self) -> impl Iterator<Item = &Slot> {
        self.slots.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Slot> {
        self.slots.iter_mut()
    }

    /// The slot an update belongs to, or `None` if that session has closed.
    ///
    /// A closed session's replies are dropped exactly as a stale generation's
    /// are: the work was abandoned, and there is nothing left to apply it to.
    pub fn route(&mut self, id: u64) -> Option<&mut Slot> {
        self.slots.iter_mut().find(|slot| slot.id == id)
    }

    /// Whether any session has work in flight, which is what the spinner and the
    /// redraw tick want to know — not whether the focused one does.
    pub fn any_busy(&self) -> bool {
        self.slots.iter().any(|slot| {
            matches!(
                slot.app.status,
                Status::Waiting | Status::Streaming | Status::Running | Status::Compacting
            )
        })
    }

    /// How many sessions are waiting on a person rather than on time.
    pub fn blocked(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| Self::is_blocked(&slot.app))
            .count()
    }

    fn is_blocked(app: &App) -> bool {
        matches!(
            app.status,
            Status::AwaitingApproval(_)
                | Status::AwaitingChoice(_)
                | Status::AwaitingExecute { .. }
        )
    }

    /// Hand the catalog to every session, and remember it for the next one.
    ///
    /// The fetch happens once at startup and belongs to no session, so its
    /// result goes to all of them.
    pub fn set_catalog(&mut self, result: Result<Vec<crate::openrouter::ModelInfo>, String>) {
        if let Some(first) = self.slots.first_mut() {
            first.app.set_catalog(result);
            let catalog = first.app.catalog();
            for slot in self.slots.iter_mut().skip(1) {
                slot.app.share_catalog(catalog.clone());
            }
        }
    }

    fn catalog(&self) -> Arc<Catalog> {
        self.current().app.catalog()
    }

    /// Open a new session beside the current one, and focus it.
    ///
    /// Inherits the current session's settings — see [`App::spawn_sibling`] —
    /// so a session spawned from one running unattended is also unattended, and
    /// one spawned from a careful session also asks.
    pub fn spawn(&mut self) {
        if self.slots.len() >= MAX_SESSIONS {
            return self.app_mut().push_notice(format!(
                "Already running {MAX_SESSIONS} sessions, the most at once. Shut one \
                 down first — Ctrl+Space, then x."
            ));
        }
        let name = self.unique_name();
        let mut app = self.current().app.spawn_sibling(name);
        app.share_catalog(self.catalog());
        self.push(app);
        let name = self.app().session_name().to_string();
        self.app_mut().push_notice(format!(
            "New session {name:?}. Ctrl+Space switches between them."
        ));
    }

    /// Put the focus in session `name`, opening it as a slot of its own if it is
    /// not already running.
    ///
    /// The one answer to "go to that session", however it was asked — the `l`
    /// shortcut in the view, a picker row, or `/load <name>`. Switching comes
    /// first and is not subject to the cap: a session that is already running
    /// takes no new slot, and refusing to *move* to one because eight are open
    /// would be refusing the wrong thing.
    pub fn reveal(&mut self, name: String) {
        if let Some(i) = self.slot_of(&name) {
            self.switch(i);
            return;
        }
        if self.slots.len() >= MAX_SESSIONS {
            return self.app_mut().push_notice(format!(
                "Already running {MAX_SESSIONS} sessions, the most at once. Shut one \
                 down first — Ctrl+Space, then x."
            ));
        }
        let mut app = self.current().app.spawn_sibling(name.clone());
        app.share_catalog(self.catalog());
        if !app.open_saved(&name) {
            return self
                .app_mut()
                .push_notice(format!("Could not open session {name:?}."));
        }
        self.push(app);
        self.app_mut()
            .push_notice(format!("Opened session {name:?}."));
    }

    /// Which slot is running this session, if any.
    ///
    /// Two slots on one session file auto-save over each other, so this is what
    /// every way of opening one asks first — and the answer is where to go
    /// rather than a reason to refuse.
    pub fn slot_of(&self, name: &str) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.app.session_name() == name)
    }

    /// The names running right now, for the pickers to leave out.
    pub fn open_names(&self) -> Vec<String> {
        self.slots
            .iter()
            .map(|slot| slot.app.session_name().to_string())
            .collect()
    }

    /// Make the set of open sessions known — to each session, and to disk.
    ///
    /// Both duties derive from the same list, and both are done by deriving and
    /// comparing rather than by hooking spawn, close, switch and rename: the
    /// reason [`App::maybe_autosave`] works from a fingerprint. One call site
    /// cannot be missed, and `/rename` is exactly the hook that would have been.
    pub fn sync_open_set(&mut self) {
        // What each session may not load. Its own name is excluded: `/load` of
        // the session you are already in is a no-op, not a collision.
        let names = self.open_names();
        for slot in self.slots.iter_mut() {
            let mine = slot.app.session_name();
            let others = names.iter().filter(|n| *n != mine).cloned().collect();
            slot.app.set_open_elsewhere(others);
        }
        self.maybe_record_open(names);
    }

    /// Write the open set when it has changed.
    fn maybe_record_open(&mut self, names: Vec<String>) {
        let open = OpenSet {
            version: crate::session::VERSION,
            root: self.root.clone(),
            current: self.current,
            names,
        };
        if self.recorded.as_ref() == Some(&open) {
            return;
        }
        // Best effort: failing to record which sessions were open must not be
        // reported as if the conversations themselves were at risk. They are
        // each auto-saved in their own right.
        if crate::session::write_open(self.sessions_dir(), &open).is_ok() {
            self.recorded = Some(open);
        }
    }

    fn sessions_dir(&self) -> &Path {
        self.current().app.sessions_dir()
    }

    /// A session name that is taken neither by a running session nor by a
    /// folder on disk.
    ///
    /// `session::default_name` is the clock to the second, so two sessions
    /// spawned in the same second would take the same name and overwrite each
    /// other's file on the next autosave.
    fn unique_name(&self) -> String {
        let base = crate::session::default_name();
        let dir = self.current().app.sessions_dir();
        let taken = |name: &str| {
            self.slots.iter().any(|s| s.app.session_name() == name)
                || crate::session::exists(dir, name)
        };
        if !taken(&base) {
            return base;
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|name| !taken(name))
            .unwrap_or(base)
    }

    /// Focus session `i`, if it exists.
    pub fn switch(&mut self, i: usize) -> bool {
        if i >= self.slots.len() {
            return false;
        }
        self.current = i;
        // The conversation being looked at should be at its end, not wherever
        // the last look left the scroll.
        self.current_mut().app.follow = true;
        true
    }

    /// Shut session `i` down: stop its work, save it, and drop it.
    ///
    /// Not destructive, and so not confirmed: the conversation is on disk and
    /// `/load` brings it back. What is lost is the reply that was in flight, if
    /// there was one, which cancelling would have lost anyway.
    pub fn close(&mut self, i: usize) {
        if i >= self.slots.len() {
            return;
        }
        // The last one is not closable: the harness with no session is a state
        // with no prompt and nothing to type into. `/quit` is how you leave.
        if self.slots.len() == 1 {
            return self
                .app_mut()
                .push_notice("This is the only session; /quit exits the harness.");
        }
        let mut slot = self.slots.remove(i);
        if let Some(inflight) = slot.inflight.take() {
            let _ = inflight.cancel.send(());
        }
        slot.app.cancel();
        slot.app.maybe_autosave();
        let name = slot.app.session_name().to_string();

        // Focus moves to the neighbour rather than to the start: closing the
        // session you were in should leave you next to where you were.
        self.current = self.current.min(self.slots.len() - 1);
        if i < self.current || (i == self.current && i > 0) {
            self.current = self.current.saturating_sub(1);
        }
        self.current = self.current.min(self.slots.len() - 1);
        self.app_mut().push_notice(format!(
            "Shut down {name:?}; it is saved and can be /loaded."
        ));
    }

    /// One row per session, in the order they were opened.
    ///
    /// Derived per call rather than stored, the same rule `rewind_rows` and
    /// `picker_matches` follow: the thing it describes changes under it
    /// constantly, and a stored copy would be a second source of truth.
    pub fn rows(&self) -> Vec<Row> {
        self.slots
            .iter()
            .enumerate()
            .map(|(i, slot)| {
                let app = &slot.app;
                let blocked = Self::is_blocked(app);
                Row {
                    id: slot.id,
                    name: app.session_name().to_string(),
                    model: app.model.clone(),
                    status: match app.status {
                        Status::Idle => "ready",
                        Status::Waiting => "thinking",
                        Status::Streaming => "streaming",
                        Status::Running => "running",
                        Status::Compacting => "compacting",
                        Status::AwaitingApproval(_) => "needs you",
                        Status::AwaitingChoice(_) => "asking you",
                        Status::AwaitingExecute { .. } => "needs you",
                        Status::AwaitingUndo { .. } => "needs you",
                    },
                    blocked,
                    busy: app.is_busy() && !blocked,
                    turns: app.turn_number,
                    focused: i == self.current,
                    activity: app.activity(ACTIVITY_LINES),
                }
            })
            .collect()
    }

    // --- the view ---

    pub fn view(&self) -> Option<&View> {
        self.view.as_ref()
    }

    pub fn view_open(&self) -> bool {
        self.view.is_some()
    }

    /// Open the view on the session you are in, so the list starts where you are.
    ///
    /// The query starts empty, so the highlight's position in the matches is its
    /// position in the slots.
    pub fn open_view(&mut self) {
        self.view = Some(View {
            selected: self.current,
            ..View::default()
        });
    }

    pub fn close_view(&mut self) {
        self.view = None;
    }

    /// Slot indices matching what has been typed, in view order.
    ///
    /// Derived per call for the reason [`Sessions::rows`] is: the sessions and
    /// their states change underneath it constantly.
    pub fn view_matches(&self) -> Vec<usize> {
        let Some(view) = &self.view else {
            return Vec::new();
        };
        let terms: Vec<String> = view
            .query
            .text()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        (0..self.slots.len())
            .filter(|&i| self.matches(i, &terms))
            .collect()
    }

    /// Whether every one of `terms` appears somewhere in the row for session
    /// `i`. `terms` are expected lowercase.
    ///
    /// Wider than the `/load` picker's rule, which matches on name and model
    /// only, and deliberately so: every session here is named
    /// `session-<timestamp>`, so the name is the least distinguishing thing
    /// about it. What tells two apart is what they are *doing*, which is why the
    /// activity lines are on the row — and so they are what you can search.
    fn matches(&self, i: usize, terms: &[String]) -> bool {
        let app = &self.slots[i].app;
        let mut haystack = format!("{} {}", app.session_name(), app.model).to_lowercase();
        for line in app.activity(ACTIVITY_LINES) {
            haystack.push(' ');
            haystack.push_str(&line.to_lowercase());
        }
        terms.iter().all(|term| haystack.contains(term.as_str()))
    }

    /// The rows the view actually shows: [`Sessions::rows`] narrowed to the
    /// matches.
    pub fn view_rows(&self) -> Vec<Row> {
        let rows = self.rows();
        self.view_matches()
            .into_iter()
            .filter_map(|i| rows.get(i).cloned())
            .collect()
    }

    /// The highlight, clamped to what is actually offered. The matches can shrink
    /// under a stationary highlight when a background session's status changes.
    pub fn view_index(&self) -> usize {
        let count = self.view_matches().len();
        let selected = self.view.as_ref().map_or(0, |view| view.selected);
        if count == 0 {
            0
        } else {
            selected.min(count - 1)
        }
    }

    pub fn view_move(&mut self, delta: isize) {
        let last = self.view_matches().len().saturating_sub(1) as isize;
        let current = self.view_index() as isize;
        if let Some(view) = &mut self.view {
            view.selected = (current + delta).clamp(0, last) as usize;
        }
    }

    /// Focus a row directly, for mouse hover and clicks. `i` is a position in the
    /// filtered list, which is what the row map holds.
    pub fn view_select(&mut self, i: usize) -> bool {
        if i >= self.view_matches().len() {
            return false;
        }
        if let Some(view) = &mut self.view {
            view.selected = i;
            return true;
        }
        false
    }

    /// Start typing a filter, the way `/` starts one in a pager.
    pub fn view_search(&mut self, on: bool) {
        if let Some(view) = &mut self.view {
            view.searching = on;
        }
    }

    pub fn view_searching(&self) -> bool {
        self.view.as_ref().is_some_and(|view| view.searching)
    }

    /// Edit the query. Any edit resets the highlight to the top: the list under
    /// it has just changed, so holding the old position would land it somewhere
    /// unrelated.
    pub fn view_query_input(&mut self, edit: impl FnOnce(&mut Input)) {
        if let Some(view) = &mut self.view {
            edit(&mut view.query);
            view.selected = 0;
        }
    }

    /// Switch to the highlighted session and close the view.
    pub fn view_confirm(&mut self) {
        let Some(&slot) = self.view_matches().get(self.view_index()) else {
            return;
        };
        self.view = None;
        self.switch(slot);
    }

    /// Shut the highlighted session down, leaving the view open.
    pub fn view_close_selected(&mut self) {
        let selected = self.view_index();
        let Some(&slot) = self.view_matches().get(selected) else {
            return;
        };
        self.close(slot);
        // Clamp against the matches after the close, not before: the filter may
        // hide the session that took the closed one's place.
        let last = self.view_matches().len().saturating_sub(1);
        if let Some(view) = &mut self.view {
            view.selected = selected.min(last);
        }
    }

    /// Spawn from the view, which closes it: a new session is one you want to
    /// start typing into.
    pub fn view_spawn(&mut self) {
        self.spawn();
        self.view = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions(label: &str) -> (Sessions, std::path::PathBuf) {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ai-harness-sessions-{label}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let app = App::new("m".into(), None, 10, dir.clone());
        (Sessions::new(app), dir)
    }

    #[test]
    fn spawning_inherits_the_settings_of_the_session_it_came_from() {
        let (mut sessions, dir) = sessions("inherit");
        sessions.app_mut().auto_approve = true;
        sessions.app_mut().show_reasoning = false;
        sessions.app_mut().keep_checkpoints = Some(4);
        sessions.app_mut().model = "other/model".into();

        sessions.spawn();
        let fresh = sessions.app();
        assert!(
            fresh.auto_approve,
            "an unattended session spawns unattended"
        );
        assert!(!fresh.show_reasoning);
        assert_eq!(fresh.keep_checkpoints, Some(4));
        assert_eq!(fresh.model, "other/model");
        // But not the conversation.
        assert_eq!(fresh.turn_number, 0);
        assert_eq!(fresh.history.len(), 1, "the contract and nothing else");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `default_name` is the clock to the second, so without a uniquifier two
    /// sessions spawned together would overwrite each other's file.
    #[test]
    fn sessions_spawned_in_the_same_second_get_different_names() {
        let (mut sessions, dir) = sessions("names");
        sessions.spawn();
        sessions.spawn();
        let names: Vec<String> = sessions
            .rows()
            .into_iter()
            .map(|row| row.name)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(names.len(), 3, "three sessions, three names: {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_already_on_disk_is_not_reused() {
        let (mut sessions, dir) = sessions("ondisk");
        // Claim the name the next spawn would otherwise take.
        let taken = crate::session::default_name();
        let session =
            crate::session::Session::new("m".into(), vec![], vec![], vec![], Default::default());
        crate::session::save(&dir, &taken, &session).unwrap();

        sessions.spawn();
        assert_ne!(
            sessions.app().session_name(),
            taken,
            "a spawn must not adopt a conversation already saved under that name"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cap_holds_and_says_so() {
        let (mut sessions, dir) = sessions("cap");
        for _ in 1..MAX_SESSIONS {
            sessions.spawn();
        }
        assert_eq!(sessions.len(), MAX_SESSIONS);
        sessions.spawn();
        assert_eq!(sessions.len(), MAX_SESSIONS, "the cap holds");
        assert!(
            matches!(
                sessions.app().transcript.last(),
                Some(crate::app::Entry::Notice(n)) if n.contains("most at once")
            ),
            "and it says why nothing happened"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ids outlive their positions, which is what keeps a reply in flight from
    /// landing in a session that merely shuffled into the closed one's index.
    #[test]
    fn ids_are_stable_when_a_session_in_the_middle_closes() {
        let (mut sessions, dir) = sessions("ids");
        sessions.spawn();
        sessions.spawn();
        let before: Vec<u64> = sessions.rows().iter().map(|r| r.id).collect();
        assert_eq!(before, vec![1, 2, 3]);

        sessions.close(1);
        let after: Vec<u64> = sessions.rows().iter().map(|r| r.id).collect();
        assert_eq!(after, vec![1, 3], "the survivors keep their ids");
        assert!(sessions.route(2).is_none(), "the closed one routes nowhere");
        assert!(sessions.route(3).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn closing_saves_the_conversation_so_it_can_be_loaded_back() {
        let (mut sessions, dir) = sessions("close-saves");
        sessions.spawn();
        let name = sessions.app().session_name().to_string();
        sessions
            .app_mut()
            .input
            .insert_str("something worth keeping");
        sessions.app_mut().submit().unwrap();
        sessions.app_mut().push_response(
            "<ai-harness-response>noted</ai-harness-response>".into(),
            None,
        );

        sessions.close(1);
        assert_eq!(sessions.len(), 1);
        let loaded = crate::session::load(&dir, &name).expect("it was saved on the way out");
        assert!(
            loaded
                .transcript
                .iter()
                .any(|e| matches!(e, crate::app::Entry::User(t) if t == "something worth keeping")),
            "and the conversation really is in it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_last_session_cannot_be_closed() {
        let (mut sessions, dir) = sessions("last");
        sessions.close(0);
        assert_eq!(sessions.len(), 1, "there is always somewhere to type");
        assert!(matches!(
            sessions.app().transcript.last(),
            Some(crate::app::Entry::Notice(n)) if n.contains("only session")
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn closing_the_focused_session_leaves_you_next_to_where_you_were() {
        let (mut sessions, dir) = sessions("focus");
        sessions.spawn();
        sessions.spawn();
        assert_eq!(sessions.current, 2, "spawning focuses the new one");

        sessions.close(2);
        assert_eq!(sessions.current, 1, "back to the neighbour, not the start");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_blocked_session_is_distinguished_from_a_busy_one() {
        let (mut sessions, dir) = sessions("blocked");
        sessions.spawn();
        sessions.app_mut().input.insert_str("do something");
        sessions.app_mut().submit().unwrap();
        assert!(sessions.any_busy(), "waiting on the model counts as busy");
        assert_eq!(sessions.blocked(), 0, "but not as wanting a person");

        sessions
            .app_mut()
            .push_response("<ai-harness-shell>ls</ai-harness-shell>".into(), None);
        assert_eq!(sessions.blocked(), 1, "an approval wants a person");
        let rows = sessions.rows();
        assert_eq!(rows[1].status, "needs you");
        assert!(rows[1].blocked && !rows[1].busy);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_catalog_reaches_every_session_including_later_ones() {
        let (mut sessions, dir) = sessions("catalog");
        sessions.spawn();
        sessions.set_catalog(Ok(vec![crate::openrouter::ModelInfo {
            id: "a/b".into(),
            name: "A B".into(),
            context_length: None,
            pricing: None,
        }]));
        for slot in sessions.iter() {
            assert_eq!(slot.app.catalog.models().len(), 1);
        }

        // And a session opened afterwards gets it without a second fetch.
        sessions.spawn();
        assert_eq!(sessions.app().catalog.models().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_view_opens_on_the_session_you_are_in() {
        let (mut sessions, dir) = sessions("view");
        sessions.spawn();
        sessions.spawn();
        sessions.switch(1);

        sessions.open_view();
        assert_eq!(sessions.view().unwrap().selected, 1);
        sessions.view_move(-1);
        sessions.view_confirm();
        assert_eq!(sessions.current, 0, "Enter switches to the highlight");
        assert!(!sessions.view_open(), "and closes the view");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Type a query into an open view.
    fn type_query(sessions: &mut Sessions, text: &str) {
        sessions.view_search(true);
        for c in text.chars() {
            sessions.view_query_input(|input| input.insert_char(c));
        }
    }

    #[test]
    fn the_view_narrows_to_the_query_and_switches_to_the_match() {
        let (mut sessions, dir) = sessions("search");
        sessions.spawn();
        sessions.spawn();
        sessions.iter_mut().nth(1).unwrap().app.model = "beta/two".into();

        sessions.open_view();
        type_query(&mut sessions, "beta");
        let rows = sessions.view_rows();
        assert_eq!(rows.len(), 1, "one session runs beta/two: {rows:?}");

        // The highlight is a position in the matches, so confirming takes the
        // session the row names rather than the slot at that ordinal.
        sessions.view_confirm();
        assert_eq!(sessions.current, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The names are all `session-<timestamp>`, so what tells two sessions apart
    /// is what they are doing — which is why the activity lines are searchable.
    #[test]
    fn the_query_reaches_what_a_session_is_doing() {
        let (mut sessions, dir) = sessions("activity");
        sessions.spawn();
        sessions
            .app_mut()
            .transcript
            .push(crate::app::Entry::User("refactor the parser".into()));

        sessions.open_view();
        type_query(&mut sessions, "parser");
        assert_eq!(sessions.view_matches(), vec![1]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_query_that_matches_nothing_leaves_nothing_to_confirm() {
        let (mut sessions, dir) = sessions("nomatch");
        sessions.spawn();
        sessions.open_view();
        type_query(&mut sessions, "nothing-matches-this");

        assert!(sessions.view_rows().is_empty());
        assert_eq!(sessions.view_index(), 0, "clamped, not out of range");
        sessions.view_confirm();
        assert!(sessions.view_open(), "Enter on nothing does nothing");
        sessions.view_close_selected();
        assert_eq!(sessions.len(), 2, "and neither does x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Leaving the search returns the keyboard to the list without undoing the
    /// narrowing — you filtered in order to walk what was left.
    #[test]
    fn leaving_the_search_keeps_the_filter() {
        let (mut sessions, dir) = sessions("keepfilter");
        sessions.spawn();
        sessions.iter_mut().nth(1).unwrap().app.model = "beta/two".into();

        sessions.open_view();
        type_query(&mut sessions, "beta");
        sessions.view_search(false);
        assert!(!sessions.view_searching());
        assert_eq!(sessions.view_matches(), vec![1]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The highlight indexes the matches, so `x` must close the session the row
    /// names — not the slot that happens to sit at that ordinal.
    #[test]
    fn closing_from_a_filtered_view_closes_the_session_on_the_row() {
        let (mut sessions, dir) = sessions("filteredclose");
        sessions.spawn();
        sessions.spawn();
        let doomed = sessions.rows()[2].id;
        sessions.iter_mut().nth(2).unwrap().app.model = "beta/two".into();

        sessions.open_view();
        type_query(&mut sessions, "beta");
        sessions.view_close_selected();

        assert_eq!(sessions.len(), 2);
        assert!(
            sessions.rows().iter().all(|row| row.id != doomed),
            "the filtered row's session is the one that went"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- reopening the set that was open ---

    /// Put a loadable session on disk under `name`.
    fn saved(dir: &Path, name: &str) {
        let session = crate::session::Session::new(
            format!("{name}/model"),
            vec![crate::openrouter::Message::system("contract")],
            Vec::new(),
            Vec::new(),
            crate::ledger::Ledger::default(),
        );
        crate::session::save(dir, name, &session).unwrap();
    }

    fn names_of(sessions: &Sessions) -> Vec<String> {
        sessions.open_names()
    }

    /// The template carries this launch's flags, so a restored session has to
    /// take them — and then its own saved model on top.
    #[test]
    fn restore_reopens_the_recorded_set_in_order_with_focus_where_it_was() {
        let (_, dir) = sessions("restore");
        saved(&dir, "alpha");
        saved(&dir, "beta");
        saved(&dir, "gamma");
        crate::session::write_open(
            &dir,
            &OpenSet {
                version: crate::session::VERSION,
                root: dir.clone(),
                current: 1,
                names: vec!["alpha".into(), "beta".into(), "gamma".into()],
            },
        )
        .unwrap();

        let mut template = App::new("m".into(), None, 10, dir.clone());
        template.auto_approve = true;
        let restored = Sessions::restore(template, &dir, &dir);

        assert_eq!(names_of(&restored), vec!["alpha", "beta", "gamma"]);
        assert_eq!(restored.current, 1, "focus where it was left");
        assert!(
            restored.view_open(),
            "resuming starts with choosing which work"
        );
        assert_eq!(restored.view().unwrap().selected, 1, "on the one you left");
        assert_eq!(restored.app().model, "beta/model", "its own saved model");
        assert!(
            restored.iter().all(|slot| slot.app.auto_approve),
            "and this launch's flags"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_names_a_session_that_is_no_longer_saved() {
        let (_, dir) = sessions("restore-missing");
        saved(&dir, "alpha");
        crate::session::write_open(
            &dir,
            &OpenSet {
                version: crate::session::VERSION,
                root: dir.clone(),
                current: 0,
                names: vec!["alpha".into(), "deleted-since".into()],
            },
        )
        .unwrap();

        let restored = Sessions::restore(App::new("m".into(), None, 10, dir.clone()), &dir, &dir);
        assert_eq!(names_of(&restored), vec!["alpha"]);
        assert!(
            transcript_says(&restored, "deleted-since"),
            "a session that is gone should be named, not passed over"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--sessions-dir` can point two projects at one directory, and the set is
    /// a fact about a project.
    #[test]
    fn restore_declines_a_record_from_another_project() {
        let (_, dir) = sessions("restore-elsewhere");
        saved(&dir, "alpha");
        crate::session::write_open(
            &dir,
            &OpenSet {
                version: crate::session::VERSION,
                root: PathBuf::from("/some/other/project"),
                current: 0,
                names: vec!["alpha".into()],
            },
        )
        .unwrap();

        let restored = Sessions::restore(App::new("m".into(), None, 10, dir.clone()), &dir, &dir);
        assert_eq!(restored.len(), 1);
        assert_ne!(names_of(&restored), vec!["alpha".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_with_nothing_to_resume_starts_the_one_fresh_session() {
        let (_, dir) = sessions("restore-empty");
        // No record at all.
        let restored = Sessions::restore(App::new("m".into(), None, 10, dir.clone()), &dir, &dir);
        assert_eq!(restored.len(), 1);
        assert!(
            !restored.view_open(),
            "one fresh session is a prompt to type at, not a list to choose from"
        );

        // A record whose sessions have all been deleted.
        crate::session::write_open(
            &dir,
            &OpenSet {
                version: crate::session::VERSION,
                root: dir.clone(),
                current: 0,
                names: vec!["gone".into()],
            },
        )
        .unwrap();
        let restored = Sessions::restore(App::new("m".into(), None, 10, dir.clone()), &dir, &dir);
        assert_eq!(restored.len(), 1);
        assert!(transcript_says(&restored, "gone"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_holds_the_cap() {
        let (_, dir) = sessions("restore-cap");
        let names: Vec<String> = (0..MAX_SESSIONS + 3).map(|i| format!("s{i}")).collect();
        for name in &names {
            saved(&dir, name);
        }
        crate::session::write_open(
            &dir,
            &OpenSet {
                version: crate::session::VERSION,
                root: dir.clone(),
                current: 0,
                names,
            },
        )
        .unwrap();

        let restored = Sessions::restore(App::new("m".into(), None, 10, dir.clone()), &dir, &dir);
        assert_eq!(restored.len(), MAX_SESSIONS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Derived and compared rather than hooked onto every mutation, so this is
    /// what checks the comparison actually notices.
    #[test]
    fn the_record_is_written_when_the_set_changes_and_not_otherwise() {
        let (mut sessions, dir) = sessions("record");
        sessions.sync_open_set();
        let first = crate::session::read_open(&dir).expect("recorded");
        assert_eq!(first.names.len(), 1);
        assert_eq!(first.root, dir);

        // Nothing changed, so nothing is written — the deleted file stays gone.
        std::fs::remove_file(dir.join(crate::session::OPEN_FILE)).unwrap();
        sessions.sync_open_set();
        assert!(crate::session::read_open(&dir).is_none());

        sessions.spawn();
        sessions.sync_open_set();
        assert_eq!(crate::session::read_open(&dir).unwrap().names.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn closing_a_session_drops_it_from_the_record() {
        let (mut sessions, dir) = sessions("record-close");
        sessions.spawn();
        let doomed = sessions.app().session_name().to_string();
        sessions.sync_open_set();
        assert!(
            crate::session::read_open(&dir)
                .unwrap()
                .names
                .contains(&doomed)
        );

        sessions.close(1);
        sessions.sync_open_set();
        let record = crate::session::read_open(&dir).unwrap();
        assert!(
            !record.names.contains(&doomed),
            "a session you shut down should not come back: {record:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- going to a session, opening it if it is not running ---

    #[test]
    fn revealing_a_saved_session_adds_it_rather_than_replacing_anything() {
        let (mut sessions, dir) = sessions("reveal-saved");
        saved(&dir, "archived");
        let already = sessions.app().session_name().to_string();

        sessions.reveal("archived".into());
        assert_eq!(sessions.len(), 2, "the running session is untouched");
        assert_eq!(sessions.app().session_name(), "archived");
        assert_eq!(sessions.app().model, "archived/model");
        assert!(sessions.slot_of(&already).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two slots on one session file auto-save over each other, so the second
    /// ask has to move you rather than duplicate it.
    #[test]
    fn revealing_a_running_session_switches_instead_of_opening_it_twice() {
        let (mut sessions, dir) = sessions("reveal-twice");
        saved(&dir, "archived");
        sessions.reveal("archived".into());
        let first = sessions.app().session_name().to_string();
        sessions.spawn();
        assert_ne!(sessions.app().session_name(), first);

        sessions.reveal("archived".into());
        assert_eq!(sessions.len(), 3, "no duplicate slot");
        assert_eq!(sessions.app().session_name(), "archived", "but you moved");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Switching takes no new slot, so refusing to *move* at the cap would be
    /// refusing the wrong thing.
    #[test]
    fn the_cap_stops_opening_a_session_but_not_going_to_one() {
        let (mut sessions, dir) = sessions("reveal-cap");
        saved(&dir, "archived");
        sessions.reveal("archived".into());
        while sessions.len() < MAX_SESSIONS {
            sessions.spawn();
        }
        saved(&dir, "not-running");

        sessions.reveal("not-running".into());
        assert_eq!(sessions.len(), MAX_SESSIONS, "the cap holds");
        assert!(transcript_says(&sessions, "the most at once"));

        sessions.reveal("archived".into());
        assert_eq!(sessions.app().session_name(), "archived");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whether the focused session's transcript mentions `needle`.
    fn transcript_says(sessions: &Sessions, needle: &str) -> bool {
        sessions.app().transcript.iter().any(|entry| match entry {
            crate::app::Entry::Notice(text) => text.contains(needle),
            _ => false,
        })
    }
}
