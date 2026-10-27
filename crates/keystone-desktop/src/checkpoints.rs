//! Checkpoints of each session's repository (see [`keystone_agent::checkpoint`]). A
//! turn's start is read before its first message goes, and its end as it ends, both on
//! the thread that sends the session's messages, so each sees the files as the
//! provider left them. A message sent during a turn that starts the next one waits
//! for that read ([`Connection::hold_follow_ups`]). Restore to here runs on a thread of
//! its own, so a slow restore holds no provider's other sessions, and no message to a
//! session in the same repository goes until it ends.

use super::AppState;
use futures::channel::oneshot;
use gpui::Context;
use keystone_agent::Connection;
use keystone_agent::checkpoint::{self, Checkpoints, Restored, Snapshot};
use keystone_agent::git::Cancel;
use keystone_core::SessionContext;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

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

/// A restore under way in a repository: what stops it, and its thread.
pub(crate) struct InFlight {
    cancel: Cancel,
    thread: JoinHandle<()>,
}

/// A restore started for the Restore to here dialog.
pub(crate) struct Restoring {
    /// Stops it and puts every file back, until its files are kept.
    pub(crate) cancel: Cancel,
    /// Arrives once every file is in place.
    pub(crate) placed: oneshot::Receiver<Placed>,
    /// How it ended, once messages to its repository may go again.
    pub(crate) done: oneshot::Receiver<checkpoint::Result<Restored>>,
}

/// Every file of a restore is in place. Dropped without [`Placed::keep`], they are all
/// put back.
pub(crate) struct Placed(mpsc::SyncSender<bool>);

impl Placed {
    pub(crate) fn keep(self) {
        let _ = self.0.send(true);
    }
}

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
                .is_some_and(|folder| self.restoring.keys().any(|root| within(&folder, root)))
    }

    /// Stops every restore under way and waits until each has put its files back.
    pub(crate) fn stop_restores(&mut self) {
        for (_, restore) in self.restoring.drain() {
            restore.cancel.cancel();
            let _ = restore.thread.join();
        }
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

    /// Refuses, with the reason, while a restore or a turn runs in the repository at
    /// `root`.
    fn idle_in(&self, root: &Path) -> Result<(), String> {
        let busy = self.restoring.contains_key(root)
            || self.conversations.iter().any(|(id, conversation)| {
                (conversation.running
                    || self.saving.contains_key(id)
                    || self.started(*id).is_some_and(|c| c.is_running(*id)))
                    && match self.checkpoints.get(id) {
                        Some(State::Saved(other)) => other.root == root,
                        _ => self
                            .checkpoint_folder(*id)
                            .is_some_and(|folder| within(&folder, root)),
                    }
            });
        match busy {
            true => Err("A turn is running here. Stop it first.".into()),
            false => Ok(()),
        }
    }

    /// Says in each session's checkpoints in the repository at `root` whether a restore
    /// there is unfinished: the offer to put files back is the repository's.
    fn unfinished_in(&mut self, root: &Path, unfinished: bool) {
        let sessions: Vec<u64> = self
            .checkpoints
            .iter_mut()
            .filter_map(|(id, state)| match state {
                State::Saved(saved) if saved.root == root && saved.unfinished != unfinished => {
                    saved.unfinished = unfinished;
                    Some(*id)
                }
                _ => None,
            })
            .collect();
        for session in sessions {
            if let Some(conversation) = self.conversations.get_mut(&session) {
                conversation.show_checkpoints();
                self.commit(session);
            }
        }
    }

    /// Puts back the files of the session's restore that Keystone ended before it
    /// finished, on a thread of its own, as a cancelled restore puts them back. No
    /// message to a session in that repository goes until it ends.
    pub(crate) fn put_back(&mut self, session: u64, cx: &mut Context<Self>) {
        let Some(State::Saved(saved)) = self.checkpoints.get(&session) else {
            return;
        };
        let root = saved.root.clone();
        let started = self.idle_in(&root).and_then(|()| {
            let key = self.checkpoint_session(session)?;
            let (ended, end) = oneshot::channel();
            let folder = root.clone();
            let thread = std::thread::Builder::new()
                .name("keystone-put-back".into())
                .spawn(move || {
                    let put = checkpoint::put_back(&folder);
                    let _ = ended.send((put, checkpoint::list(&folder, &key)));
                })
                .map_err(|error| error.to_string())?;
            Ok((end, thread))
        });
        let (end, thread) = match started {
            Ok(started) => started,
            Err(error) => return self.checkpoint_notice(session, &error, cx),
        };
        self.restoring.insert(
            root.clone(),
            InFlight {
                cancel: Cancel::default(),
                thread,
            },
        );
        self.publish(cx);
        cx.spawn(async move |this, cx| {
            let ended = end.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(put) = this.restoring.remove(&root) {
                    let _ = put.thread.join();
                }
                this.changes.files.push(session);
                this.deliveries_due = true;
                let Ok((put, listed)) = ended else {
                    return this.publish(cx);
                };
                match (&listed, &put) {
                    (Ok(listed), _) => this.unfinished_in(&root, listed.unfinished),
                    (Err(_), Ok(())) => this.unfinished_in(&root, false),
                    (Err(_), Err(_)) => {}
                }
                if let Err(error) = put {
                    this.conversation(session).error = Some(error.to_string());
                    this.commit(session);
                }
                this.checkpoints_read(
                    session,
                    listed.map(Some).map_err(|e| e.to_string()),
                    false,
                    cx,
                );
            });
        })
        .detach();
    }

    /// Starts putting the session's repository's working tree back the way its
    /// checkpoint for `turn` saved it. Refused at once, with the reason, while a turn runs
    /// in that repository or when the checkpoint is not saved. No message to a session
    /// in that repository goes, and no launched session's result, until it ends.
    pub(crate) fn restore_checkpoint(
        &mut self,
        session: u64,
        turn: u64,
        cx: &mut Context<Self>,
    ) -> Result<Restoring, String> {
        let root = match self.checkpoints.get(&session) {
            Some(State::Saved(saved)) if saved.turns.contains_key(&turn) => saved.root.clone(),
            Some(State::Unavailable) => return Err(UNAVAILABLE.into()),
            Some(State::Failed(error)) => return Err(error.clone()),
            _ => return Err("This checkpoint is no longer saved.".into()),
        };
        self.idle_in(&root)?;
        let key = self.checkpoint_session(session)?;
        let cancel = Cancel::default();
        let (placed_sender, placed) = oneshot::channel();
        let (ended, end) = oneshot::channel();
        let (stop, restored_root) = (cancel.clone(), root.clone());
        let thread = std::thread::Builder::new()
            .name("keystone-restore".into())
            .spawn(move || {
                // Asked once every file is in place: the dialog keeps them by opening the
                // fork. A cancel, or a dialog that is gone, puts them back.
                let keep = || {
                    let (answer, asked) = mpsc::sync_channel(1);
                    if placed_sender.send(Placed(answer)).is_err() {
                        return false;
                    }
                    loop {
                        match asked.recv_timeout(Duration::from_millis(50)) {
                            Ok(keep) => return keep,
                            // An answer sent as the cancel came still counts.
                            Err(RecvTimeoutError::Timeout) if stop.is_cancelled() => {
                                return asked.try_recv().unwrap_or(false);
                            }
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => return false,
                        }
                    }
                };
                let _ = ended.send(checkpoint::restore(&restored_root, &key, turn, &stop, keep));
            })
            .map_err(|error| error.to_string())?;
        self.restoring.insert(
            root.clone(),
            InFlight {
                cancel: cancel.clone(),
                thread,
            },
        );
        let (finished, done) = oneshot::channel();
        cx.spawn(async move |this, cx| {
            let restored = end.await.unwrap_or_else(|_| {
                Err(checkpoint::Error::Io(std::io::Error::other(
                    "Keystone stopped before restoring.",
                )))
            });
            let _ = this.update(cx, |this, cx| {
                if let Some(restore) = this.restoring.remove(&root) {
                    let _ = restore.thread.join();
                }
                this.changes.files.push(session);
                // A launched session's result held meanwhile may go.
                this.deliveries_due = true;
                if matches!(restored, Err(checkpoint::Error::Unfinished)) {
                    this.unfinished_in(&root, true);
                }
                this.publish(cx);
            });
            let _ = finished.send(restored);
        })
        .detach();
        Ok(Restoring {
            cancel,
            placed,
            done,
        })
    }
}
