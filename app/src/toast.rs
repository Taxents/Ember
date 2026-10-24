//! The toast: one short message above the player bar, maybe with an action (Undo, Retry, Sign in).
//!
//! One shows at a time; a newer one replaces it. It goes after 5 s (8 s with an action), waits
//! while the pointer is over it, and Esc puts it away. An undoable change is shown at once but
//! sent only when its toast goes (`commit`); Undo cancels it instead. Quitting sends what's left.

use crate::{App, Ui, with};
use slint::{ComponentHandle, Timer, TimerMode};
use std::time::Duration;

const PLAIN: Duration = Duration::from_secs(5);
const WITH_ACTION: Duration = Duration::from_secs(8);

/// What runs on the UI thread when a toast's action is chosen or it goes.
pub type Then = Box<dyn FnOnce(&mut App)>;

#[derive(Default)]
pub struct Toast {
    timer: Timer,
    /// The action button's label and what it does.
    action: Option<(String, Then)>,
    /// Runs when the toast goes without its action being chosen.
    commit: Option<Then>,
    /// Shown for this long, from the last time the pointer left it.
    shown_for: Duration,
}

impl App {
    /// A message alone.
    pub fn notify(&mut self, msg: impl Into<String>) {
        self.show_toast(msg.into(), None, None);
    }

    /// A message with an action button ("Retry", "Sign in", "Add anyway").
    pub fn notify_with(
        &mut self,
        msg: impl Into<String>,
        label: &str,
        act: impl FnOnce(&mut App) + 'static,
    ) {
        self.show_toast(msg.into(), Some((label.to_string(), Box::new(act))), None);
    }

    /// A change already shown, sent by `commit` when the toast goes; Undo runs `undo` instead.
    pub fn notify_undoable(
        &mut self,
        msg: impl Into<String>,
        undo: impl FnOnce(&mut App) + 'static,
        commit: impl FnOnce(&mut App) + 'static,
    ) {
        self.show_toast(
            msg.into(),
            Some(("Undo".to_string(), Box::new(undo))),
            Some(Box::new(commit)),
        );
    }

    fn show_toast(&mut self, msg: String, action: Option<(String, Then)>, commit: Option<Then>) {
        // The one it replaces is done: its change goes through.
        self.close_toast();
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_toast_action(
            action
                .as_ref()
                .map(|a| a.0.clone())
                .unwrap_or_default()
                .into(),
        );
        g.set_toast_text(msg.into());
        g.set_toast_serial(g.get_toast_serial().wrapping_add(1));
        self.toast.shown_for = if action.is_some() { WITH_ACTION } else { PLAIN };
        self.toast.action = action;
        self.toast.commit = commit;
        self.toast_timer();
    }

    fn toast_timer(&mut self) {
        self.toast
            .timer
            .start(TimerMode::SingleShot, self.toast.shown_for, || {
                with(|a| a.close_toast());
            });
    }

    /// The toast's action was chosen.
    pub fn toast_act(&mut self) {
        let action = self.toast.action.take();
        self.toast.commit = None;
        self.close_toast();
        if let Some((_, act)) = action {
            act(self);
        }
    }

    /// Puts the toast away (time up, Esc, its close button, a newer one, quitting).
    pub fn close_toast(&mut self) {
        self.toast.timer.stop();
        self.toast.action = None;
        if let Some(ui) = self.ui.upgrade() {
            ui.global::<Ui>().set_toast_text("".into());
        }
        // Last, so a toast the change shows (it failed) stays up.
        if let Some(commit) = self.toast.commit.take() {
            commit(self);
        }
    }

    /// The pointer is over the toast (it waits) or left it (the time starts again).
    pub fn toast_hover(&mut self, over: bool) {
        if over {
            self.toast.timer.stop();
        } else if self.ui().global::<Ui>().get_toast_text() != "" {
            self.toast_timer();
        }
    }
}

/// Connects the toast's callbacks.
pub fn wire(g: &Ui) {
    g.on_toast_act(|| {
        with(|a| a.toast_act());
    });
    g.on_toast_dismiss(|| {
        with(|a| a.close_toast());
    });
    g.on_toast_hover(|over| {
        with(|a| a.toast_hover(over));
    });
}
