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

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::app::{App, Catalog, Status};
use crate::ui::TranscriptCache;

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
    pub selected: usize,
}

pub struct Sessions {
    slots: Vec<Slot>,
    current: usize,
    view: Option<View>,
    next_id: u64,
}

impl Sessions {
    /// Start with one session, the way the harness has always started.
    pub fn new(app: App) -> Self {
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
        }
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
                 down first — Ctrl+T, then x."
            ));
        }
        let name = self.unique_name();
        let mut app = self.current().app.spawn_sibling(name);
        app.share_catalog(self.catalog());
        let id = self.next_id;
        self.next_id += 1;
        self.slots.push(Slot {
            id,
            app,
            inflight: None,
            cache: TranscriptCache::default(),
        });
        self.current = self.slots.len() - 1;
        let name = self.app().session_name().to_string();
        self.app_mut().push_notice(format!(
            "New session {name:?}. Ctrl+T switches between them."
        ));
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
    pub fn open_view(&mut self) {
        self.view = Some(View {
            selected: self.current,
        });
    }

    pub fn close_view(&mut self) {
        self.view = None;
    }

    pub fn view_move(&mut self, delta: isize) {
        let last = self.slots.len().saturating_sub(1) as isize;
        if let Some(view) = &mut self.view {
            view.selected = (view.selected as isize + delta).clamp(0, last) as usize;
        }
    }

    pub fn view_select(&mut self, i: usize) -> bool {
        if i >= self.slots.len() {
            return false;
        }
        if let Some(view) = &mut self.view {
            view.selected = i;
            return true;
        }
        false
    }

    /// Switch to the highlighted session and close the view.
    pub fn view_confirm(&mut self) {
        if let Some(view) = self.view.take() {
            self.switch(view.selected);
        }
    }

    /// Shut the highlighted session down, leaving the view open.
    pub fn view_close_selected(&mut self) {
        let Some(view) = self.view.as_ref() else {
            return;
        };
        let selected = view.selected;
        self.close(selected);
        let last = self.slots.len().saturating_sub(1);
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
}
