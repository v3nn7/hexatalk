//! Minimal replacement for iced's `Task`/`Subscription` runtime. Its whole
//! purpose is to let `App::update` (src/update.rs), `App::new` (src/app.rs)
//! and the various `*_task`/`*_subscription` functions (src/subscriptions.rs,
//! src/session_store.rs, src/update_check.rs) keep exactly the shape they
//! had under iced -- `Task::none()`, `Task::perform(fut, f)`,
//! `Task::batch([...])`, `Task::done(msg)` -- while the view layer moves to
//! Slint, which has no Elm-style runtime of its own.
//!
//! - `Task<M>` is "spawn these futures on the tokio runtime, forward
//!   whatever `M` they resolve to back into the update loop".
//! - `SubscriptionRegistry` replaces `Subscription::run_with_id`'s
//!   dedup-by-id semantics: call `reconcile()` with the freshly desired set
//!   of (id, background job) pairs after every `update()`; jobs whose id
//!   disappears get aborted, jobs with a new id get spawned, jobs whose id
//!   is still present are left running untouched.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

pub(crate) type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

pub(crate) enum Task<M> {
    None,
    Perform(BoxFuture<M>),
    Batch(Vec<Task<M>>),
}

impl<M: Send + 'static> Task<M> {
    pub(crate) fn none() -> Self {
        Task::None
    }

    pub(crate) fn perform<A, F, Fut>(future: Fut, f: F) -> Self
    where
        Fut: Future<Output = A> + Send + 'static,
        F: FnOnce(A) -> M + Send + 'static,
        A: Send + 'static,
    {
        Task::Perform(Box::pin(async move { f(future.await) }))
    }

    /// Resolves immediately to `message` -- used where iced code built a
    /// `Task` purely to inject one message without any real async work.
    pub(crate) fn done(message: M) -> Self {
        Task::Perform(Box::pin(async move { message }))
    }

    pub(crate) fn batch(tasks: impl IntoIterator<Item = Task<M>>) -> Self {
        Task::Batch(tasks.into_iter().collect())
    }

    /// Spawns every leaf future on the current tokio runtime, forwarding
    /// its result into `tx`. Called once per `update()` cycle by the main
    /// pump loop in `main.rs`.
    pub(crate) fn spawn(self, tx: &UnboundedSender<M>) {
        match self {
            Task::None => {}
            Task::Perform(fut) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let message = fut.await;
                    let _ = tx.send(message);
                });
            }
            Task::Batch(tasks) => {
                for task in tasks {
                    task.spawn(tx);
                }
            }
        }
    }
}

/// A long-lived background job -- former `Subscription::run_with_id`.
/// `id` dedups the same way iced's did: identical id across two
/// `reconcile()` calls means "still wanted, leave it running"; a
/// previously-seen id that's missing this time means "cancel it".
pub(crate) struct Job {
    id: String,
    future: BoxFuture<()>,
}

pub(crate) fn job(id: impl Into<String>, future: impl Future<Output = ()> + Send + 'static) -> Job {
    Job {
        id: id.into(),
        future: Box::pin(future),
    }
}

/// Window-level side effect requested by `App::update` (tray minimize,
/// restore-and-focus, real quit). `App::update` only knows *that* one of
/// these should happen -- it has no window handle -- so it stashes the
/// request here and the Slint pump loop in `main.rs` performs it against
/// the real `AppWindow` right after the `update()` call that set it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowAction {
    HideToTray,
    ShowAndFocus,
    Exit,
}

pub(crate) fn write_clipboard_text(text: impl Into<String>) {
    use copypasta::ClipboardProvider;
    if let Ok(mut ctx) = copypasta::ClipboardContext::new() {
        let _ = ctx.set_contents(text.into());
    }
}

#[derive(Default)]
pub(crate) struct SubscriptionRegistry {
    running: HashMap<String, JoinHandle<()>>,
}

impl SubscriptionRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn reconcile(&mut self, desired: Vec<Job>) {
        let desired_ids: HashSet<&str> = desired.iter().map(|j| j.id.as_str()).collect();
        self.running.retain(|id, handle| {
            // Finished jobs (e.g. a peer worker that exhausted its retry
            // budget or died on a relay drop) must not count as running --
            // otherwise the secure channel never respawns until app restart.
            let keep = desired_ids.contains(id.as_str()) && !handle.is_finished();
            if !keep {
                handle.abort();
            }
            keep
        });
        for j in desired {
            if !self.running.contains_key(&j.id) {
                let handle = tokio::spawn(j.future);
                self.running.insert(j.id, handle);
            }
        }
    }
}
