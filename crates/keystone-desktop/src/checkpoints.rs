//! Checkpoints of each session's repository (see [`keystone_agent::checkpoint`]). A
//! turn's start is read before its first message goes, and its end as it ends, both on
//! the thread that sends the session's messages, so each sees the files as the
//! provider left them. A message sent during a turn that starts the next one waits
//! for that read ([`Connection::hold_follow_ups`]). Restore to here runs on that
//! thread too, and no message to a session in the same repository goes until it ends.

use super::AppState;
use futures::channel::oneshot;
use gpui::Context;
use keystone_agent::Connection;
use keystone_agent::checkpoint::{self, Checkpoints, Restored, Snapshot};
use keystone_core::SessionContext;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// What a non-git session says where a checkpoint would be.
pub(crate) const UNAVAILABLE: &str = "Checkpoints need a git repository.";
/// Why a message waits while a restore runs in its repository.
pub(crate) const RESTORING: &str = "Not sent. Files are being restored.";

/// What Keystone knows of a session's checkpoints.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum State {
    /// The session's folder is in no git repository.
    Unavailable,
    Saved(Checkpoints),
    /// Reading or saving them failed.
    Failed(String),
}

/// The tree each session's next or running turn starts from, by session.
pub(crate) type Starts = Arc<Mutex<HashMap<u64, String>>>;

/// How many checkpoints each session keeps; scripted native runs may set fewer.
fn keep() -> usize {
    #[cfg(debug_assertions)]
    if let Some(keep) = std::env::var("KEYSTONE_TEST_CHECKPOINT_KEEP")
        .ok()
        .and_then(|keep| keep.parse().ok())
    {
        return keep;
    }
    checkpoint::KEEP
}

/// Whether `folder`, with its links resolved as git resolves a repository's root, is in
/// `root`.
fn within(folder: &Path, root: &Path) -> bool {
    folder.canonicalize().map_or_else(
        |_| folder.starts_with(root),
        |folder| folder.starts_with(root),
    )
}

/// The working tree of the repository holding `folder`, if one does.
fn snapshot(folder: &Path) -> checkpoint::Result<Option<(PathBuf, Snapshot)>> {
    let Some(root) = checkpoint::root(folder)? else {
        return Ok(None);
    };
    let snapshot = checkpoint::snapshot(&root)?;
    Ok(Some((root, snapshot)))
}

/// The job that reads the tree a session's turn starts from in `folder`, for the
/// thread that sends the message starting it.
pub(crate) fn begin(session: u64, folder: PathBuf, starts: Starts) -> Box<dyn FnOnce() + Send> {
    Box::new(move || {
        let start = snapshot(&folder).ok().flatten();
        let mut starts = starts.lock().unwrap();
        match start {
            Some((_, snapshot)) => starts.insert(session, snapshot.tree),
            None => starts.remove(&session),
        };
    })
}

impl AppState {
    pub(crate) fn checkpoints_on(&self) -> bool {
        #[cfg(test)]
        return self.test_checkpoints;
        #[cfg(not(test))]
        true
    }

    pub(crate) fn checkpoint_folder(&self, session: u64) -> Option<PathBuf> {
        match self.session(session)?.context() {
            SessionContext::Recorded { directory, .. } => Some(directory.path().to_owned()),
            _ => None,
        }
    }

    pub(crate) fn checkpoints(&self, session: u64) -> Option<&State> {
        self.checkpoints.get(&session)
    }

    /// Whether a restore is putting back the files of the repository holding
    /// `session`'s folder.
    pub(crate) fn restoring(&self, session: u64) -> bool {
        !self.restoring.is_empty()
            && self
                .checkpoint_folder(session)
                .is_some_and(|folder| self.restoring.iter().any(|root| within(&folder, root)))
    }

    /// The session's checkpoints, named for this workspace.
    fn checkpoint_session(&mut self, session: u64) -> Result<checkpoint::Session, String> {
        let store = match &self.checkpoint_store {
            Some(store) => store.clone(),
            None => {
                let fresh = checkpoint::new_store().map_err(|e| e.to_string())?;
                let store = self
                    .store
                    .database()
                    .checkpoint_store(&fresh)
                    .map_err(|e| e.to_string())?;
                self.checkpoint_store = Some(store.clone());
                store
            }
        };
        Ok(checkpoint::Session { store, id: session })
    }

    fn session_connection(&mut self, session: u64) -> Option<&dyn Connection> {
        let saved = self.session(session)?;
        let (provider, account) = (saved.provider(), saved.account().cloned());
        self.connection(provider, account.as_ref()).ok()
    }

    /// Runs `job` in order with the session's provider commands, then hands its result
    /// to `done`. Returns false, running nothing, when the provider cannot be reached.
    fn in_order<T: Send + 'static>(
        &mut self,
        session: u64,
        job: impl FnOnce() -> T + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(connection) = self.session_connection(session) else {
            return false;
        };
        let (sender, receiver) = oneshot::channel();
        connection.run_in_order(
            session,
            Box::new(move || {
                let _ = sender.send(job());
            }),
        );
        cx.spawn(async move |this, cx| {
            if let Ok(result) = receiver.await {
                let _ = this.update(cx, |this, cx| done(this, result, cx));
            }
        })
        .detach();
        true
    }

    /// Before a message to the session goes: reads the tree a new turn starts from,
    /// and has messages sent during a turn wait for its end to be read.
    pub(crate) fn begin_checkpoint(&mut self, session: u64, cwd: &str, steer: bool) {
        if !self.checkpoints_on() || self.checkpoints.get(&session) == Some(&State::Unavailable) {
            return;
        }
        let begin = (!steer).then(|| begin(session, PathBuf::from(cwd), self.turn_starts.clone()));
        let Some(connection) = self.session_connection(session) else {
            return;
        };
        connection.hold_follow_ups(session);
        if let Some(begin) = begin {
            connection.run_in_order(session, begin);
        }
    }

    /// Reads the session's saved checkpoints, unless a turn's save already did.
    pub(crate) fn read_checkpoints(&mut self, session: u64, cx: &mut Context<Self>) {
        if self.checkpoints.contains_key(&session) || !self.checkpoints_on() {
            return;
        }
        let Some(folder) = self.checkpoint_folder(session) else {
            return;
        };
        let key = match self.checkpoint_session(session) {
            Ok(key) => key,
            Err(error) => return self.checkpoints_read(session, Err(error), false, cx),
        };
        let read = cx.background_executor().spawn(async move {
            match checkpoint::root(&folder) {
                Ok(Some(root)) => checkpoint::list(&root, &key).map(Some),
                Ok(None) => Ok(None),
                Err(error) => Err(error),
            }
        });
        cx.spawn(async move |this, cx| {
            let read = read.await;
            let _ = this.update(cx, |this, cx| {
                if !this.checkpoints.contains_key(&session) {
                    this.checkpoints_read(session, read.map_err(|e| e.to_string()), false, cx);
                }
            });
        })
        .detach();
    }

    /// Once one of the session's turns has ended in its conversation: saves the
    /// checkpoint it was given, if it was, then lets messages sent during it start the
    /// next turn. Nothing happens for an end still held during a load.
    pub(crate) fn turn_ended(&mut self, session: u64, cx: &mut Context<Self>) {
        let ended = self
            .conversations
            .get_mut(&session)
            .is_some_and(|conversation| conversation.take_turn_end());
        if !ended {
            return;
        }
        self.save_checkpoint(session, cx);
        if self.checkpoints_on()
            && let Some(connection) = self.session_connection(session)
        {
            connection.release_follow_ups(session);
        }
    }

    /// After a restore's fork opened: a launched session's result held meanwhile may go.
    pub(crate) fn restore_finished(&mut self, cx: &mut Context<Self>) {
        self.deliveries_due = true;
        self.publish(cx);
    }

    /// Says `message` in the session's conversation, as a checkpoint's own error is.
    pub(crate) fn checkpoint_notice(
        &mut self,
        session: u64,
        message: &str,
        cx: &mut Context<Self>,
    ) {
        self.conversation(session).error = Some(message.to_owned());
        self.commit(session);
        self.publish(cx);
    }

    /// Saves the checkpoint a turn of the session was just given, if one was.
    pub(crate) fn save_checkpoint(&mut self, session: u64, cx: &mut Context<Self>) {
        let Some(turn) = self
            .conversations
            .get_mut(&session)
            .and_then(|conversation| conversation.take_checkpoint_due())
        else {
            return;
        };
        let Some(folder) = self
            .checkpoint_folder(session)
            .filter(|_| self.checkpoints_on())
        else {
            return;
        };
        let key = match self.checkpoint_session(session) {
            Ok(key) => key,
            Err(error) => return self.checkpoints_read(session, Err(error), true, cx),
        };
        let starts = self.turn_starts.clone();
        let keep = keep();
        *self.saving.entry(session).or_default() += 1;
        let queued = self.in_order(
            session,
            move || {
                let Some((root, snapshot)) = snapshot(&folder)? else {
                    return Ok(None);
                };
                // The files this turn left are where the next one starts.
                let start = starts
                    .lock()
                    .unwrap()
                    .insert(session, snapshot.tree.clone());
                checkpoint::record(&root, &key, turn, &snapshot, start.as_deref(), keep).map(Some)
            },
            move |this, saved: checkpoint::Result<_>, cx| {
                this.saved(session);
                this.checkpoints_read(session, saved.map_err(|e| e.to_string()), true, cx)
            },
            cx,
        );
        if !queued {
            self.saved(session);
        }
    }

    fn saved(&mut self, session: u64) {
        if let Some(count) = self.saving.get_mut(&session) {
            *count -= 1;
            if *count == 0 {
                self.saving.remove(&session);
            }
        }
    }

    fn checkpoints_read(
        &mut self,
        session: u64,
        read: Result<Option<Checkpoints>, String>,
        saving: bool,
        cx: &mut Context<Self>,
    ) {
        let state = match read {
            Ok(Some(checkpoints)) => State::Saved(checkpoints),
            Ok(None) => State::Unavailable,
            Err(error) => {
                // Said once, as the save fails, without hiding the turn's own error.
                if saving && self.conversation(session).error.is_none() {
                    let reason = error.lines().rev().find(|line| !line.trim().is_empty());
                    self.conversation(session).error = Some(format!(
                        "Checkpoint not saved. {}",
                        reason.unwrap_or_default()
                    ));
                    self.commit(session);
                }
                // The checkpoints saved before stay offered.
                if matches!(self.checkpoints.get(&session), Some(State::Saved(_))) {
                    return self.publish(cx);
                }
                State::Failed(error)
            }
        };
        if let (State::Saved(saved), Some(conversation)) =
            (&state, self.conversations.get_mut(&session))
            && let Some(&last) = saved.turns.keys().next_back()
        {
            conversation.checkpoints_saved_through(last);
        }
        if self.checkpoints.get(&session) == Some(&state) {
            return self.publish(cx);
        }
        self.checkpoints.insert(session, state);
        if let Some(conversation) = self.conversations.get_mut(&session) {
            conversation.show_checkpoints();
            self.commit(session);
        }
        self.publish(cx);
    }

    /// Puts the session's repository's working tree back the way its checkpoint for
    /// `turn` saved it. Refused at once, with the reason, while a turn runs in that
    /// repository or when the checkpoint is not saved; otherwise the result arrives
    /// through the returned channel. No message to a session in that repository goes
    /// until it ends.
    pub(crate) fn restore_checkpoint(
        &mut self,
        session: u64,
        turn: u64,
        cx: &mut Context<Self>,
    ) -> Result<oneshot::Receiver<Result<Restored, String>>, String> {
        let root = match self.checkpoints.get(&session) {
            Some(State::Saved(saved)) if saved.turns.contains_key(&turn) => saved.root.clone(),
            Some(State::Unavailable) => return Err(UNAVAILABLE.into()),
            Some(State::Failed(error)) => return Err(error.clone()),
            _ => return Err("This checkpoint is no longer saved.".into()),
        };
        let busy = self.restoring.iter().any(|other| other == &root)
            || self.conversations.iter().any(|(id, conversation)| {
                (conversation.running
                    || self.saving.contains_key(id)
                    || self.started(*id).is_some_and(|c| c.is_running(*id)))
                    && match self.checkpoints.get(id) {
                        Some(State::Saved(other)) => other.root == root,
                        _ => self
                            .checkpoint_folder(*id)
                            .is_some_and(|folder| within(&folder, &root)),
                    }
            });
        if busy {
            return Err("A turn is running here. Stop it first.".into());
        }
        let key = self.checkpoint_session(session)?;
        let (sender, receiver) = oneshot::channel();
        self.restoring.insert(root.clone());
        let (reserved, restored_root) = (root.clone(), root.clone());
        let queued = self.in_order(
            session,
            move || checkpoint::restore(&restored_root, &key, turn).map_err(|e| e.to_string()),
            move |this, restored, cx| {
                this.restoring.remove(&reserved);
                this.changes.files.push(session);
                this.publish(cx);
                let _ = sender.send(restored);
            },
            cx,
        );
        if !queued {
            self.restoring.remove(&root);
            return Err("This session's provider is unavailable.".into());
        }
        Ok(receiver)
    }
}
