//! The Pull request surface: the selected session's pull request as the user's `gh`
//! reports it, with its checks and reviews. Keystone asks gh again every 30 seconds
//! while a window shows the pull request and every 5 minutes otherwise, backing off
//! after failures, and stops once it is merged or closed. A failed check or a review
//! the user has not looked at puts a dot on the surface's tab and the session's row,
//! and may notify; it never makes the session need action. Merge always asks first,
//! and merges at once through the user's own gh or shows gh's refusal.

use crate::Keystone;
use crate::app_state::AppState;
use crate::notifications::Alert;
use futures::channel::oneshot;
use gpui::{
    AnyElement, App, ClipboardItem, Context, IntoElement, Task, Window, div, prelude::*, px,
};
use keystone_agent::github::{self, MergeMethod};
use keystone_agent::worker::Worker;
use keystone_core::{
    Check, CheckState, Component, MergeState, PullRequest, PullRequestAlert, PullRequestReport,
    PullRequestState, ReviewComment, ReviewDecision, ReviewState, WindowId,
};
use keystone_ui::prelude::*;
use std::collections::HashMap;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

gpui::actions!(pull_request, [TogglePullRequest]);

pub(crate) const PULL_REQUEST_WIDTH: keystone_core::Size =
    keystone_core::Size::Px(NonZeroU16::new(360).unwrap());

/// How long after asking gh Keystone asks again: while a window shows the pull
/// request, and while none does. Each failure in a row doubles it, up to `LONGEST`.
const SHOWN: Duration = Duration::from_secs(30);
const HIDDEN: Duration = Duration::from_secs(300);
const LONGEST: Duration = Duration::from_secs(1800);

/// gh's answers for every session, kept by the app so each window draws the same.
#[derive(Default)]
pub(crate) struct Monitor {
    /// Run gh one call at a time for polls, for merges and for failed checks' logs, so
    /// none of them waits behind another.
    polls: Option<Worker>,
    merges: Option<Worker>,
    logs: Option<Worker>,
    watches: HashMap<u64, Watch>,
    /// The session each window's Pull request surface showed in its last frame.
    shown: HashMap<WindowId, u64>,
    /// Whether the pull requests saved before launch have been asked about.
    started: bool,
    timer: Option<Task<()>>,
}

#[derive(Default)]
struct Watch {
    /// When gh was last asked, or `None` when it should be asked now.
    asked: Option<Instant>,
    fetching: bool,
    /// The user asked again while gh was answering.
    again: bool,
    failures: u32,
    error: Option<github::Error>,
    /// How many times gh has been asked, and how many times by when `merge` last
    /// changed, so only an answer asked for after a merge step clears it.
    asks: u64,
    merge_asks: u64,
    merge: MergeStep,
}

#[derive(Clone, Debug, Default, PartialEq)]
enum MergeStep {
    #[default]
    Idle,
    /// Reading which merge methods the repository allows.
    Asking,
    Merging,
    /// GitHub merged it, and gh has not been asked about it since.
    Merged,
    Failed(github::Error),
}

enum Lane {
    Poll,
    Merge,
    Log,
}

/// gh's report on the session's pull request, or `None` when the session's branch has
/// no open pull request.
type Answer = Option<(PullRequest, PullRequestReport)>;

/// The merge the open confirmation would make, as the user saw it.
pub(crate) struct MergeConfirm {
    session: u64,
    link: PullRequest,
    /// The head the user saw; GitHub refuses the merge once the branch has moved.
    head: String,
    base: String,
    branch: String,
    methods: Vec<MergeMethod>,
    method: MergeMethod,
    focus: DialogFocus,
}

impl Monitor {
    fn run<T: Send + 'static>(
        &mut self,
        lane: Lane,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> oneshot::Receiver<T> {
        let (sender, receiver) = oneshot::channel();
        let (worker, name) = match lane {
            Lane::Poll => (&mut self.polls, "keystone-gh"),
            Lane::Merge => (&mut self.merges, "keystone-gh-merge"),
            Lane::Log => (&mut self.logs, "keystone-gh-log"),
        };
        worker
            .get_or_insert_with(|| Worker::new(name))
            .run(move || {
                let _ = sender.send(job());
            });
        receiver
    }

    /// Stops asking gh. A call gh is still answering is left to finish on its own.
    pub(crate) fn stop(&mut self) {
        self.timer = None;
        for worker in [self.polls.take(), self.merges.take(), self.logs.take()]
            .into_iter()
            .flatten()
        {
            worker.detach();
        }
    }

    /// The session `window` showed in its Pull request surface in its last frame.
    pub(crate) fn shown(&self, window: WindowId) -> Option<u64> {
        self.shown.get(&window).copied()
    }

    pub(crate) fn window_closed(&mut self, window: WindowId) {
        self.shown.remove(&window);
    }
}

/// Asks gh about `link`, or about the branch checked out in `folder` when the session
/// has no pull request yet.
fn ask(folder: Option<PathBuf>, link: Option<PullRequest>) -> Result<Answer, github::Error> {
    let gh = github::locate()?;
    let link = match (link, &folder) {
        (Some(link), _) => link,
        (None, Some(folder)) => match github::find(&gh, folder)? {
            Some(link) => link,
            None => return Ok(None),
        },
        (None, None) => return Ok(None),
    };
    let report = github::view(&gh, &gh_folder(folder), &link)?;
    Ok(Some((link, report)))
}

/// Where to run gh for a pull request named by its link, which needs no checkout, so
/// one whose session's folder is gone, as a removed worktree's is, still answers.
fn gh_folder(folder: Option<PathBuf>) -> PathBuf {
    folder.unwrap_or_else(std::env::temp_dir)
}

/// The top folder of the repository holding `folder`, where review comments' paths
/// start.
fn repository_root(folder: &Path) -> &Path {
    folder
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(folder)
}

impl AppState {
    fn session_folder(&self, session: u64) -> Option<PathBuf> {
        match self.conversation_folder(session)? {
            crate::presentation::ConversationFolder::Recorded(directory) => {
                Some(directory.path().to_owned()).filter(|folder| folder.is_dir())
            }
            _ => None,
        }
    }

    /// The session that watches each linked pull request: the newest linking it, as a
    /// handoff or fork is, so one pull request is asked about, dotted and notified once.
    fn pull_request_watchers(&self) -> HashMap<&str, u64> {
        let mut watchers = HashMap::new();
        for session in self.store().workspace().sessions() {
            if let Some(link) = session.pull_request() {
                let newest = watchers.entry(link.url.as_str()).or_insert(session.id());
                *newest = (*newest).max(session.id());
            }
        }
        watchers
    }

    /// Asks gh about every pull request that is due, and wakes for the next one.
    pub(crate) fn poll_pull_requests(&mut self, cx: &mut Context<Self>) {
        if self.phase != crate::app_state::Phase::Running {
            return;
        }
        let now = cx.background_executor().now();
        self.pull_requests.started = true;
        let watchers = self.pull_request_watchers();
        let candidates: Vec<(u64, bool)> = self
            .store()
            .workspace()
            .sessions()
            .iter()
            .filter_map(|session| {
                let shown = self
                    .pull_requests
                    .shown
                    .values()
                    .any(|s| *s == session.id());
                let wanted = match session.pull_request() {
                    Some(link) => {
                        (shown || watchers.get(link.url.as_str()) == Some(&session.id()))
                            && session
                                .pull_request_watch()
                                .is_none_or(|watch| watch.report.state.is_open())
                    }
                    None => shown,
                };
                wanted.then_some((session.id(), shown))
            })
            .collect();
        let mut next: Option<Instant> = None;
        for (session, shown) in candidates {
            // A pull request not asked about yet is asked about at once, so the first
            // report, whose alerts count as seen, comes before checks run on a new one.
            let watch = self.pull_requests.watches.entry(session).or_default();
            if watch.fetching {
                continue;
            }
            let due = watch.asked.map_or(now, |asked| {
                let base = if shown { SHOWN } else { HIDDEN };
                asked + base.saturating_mul(1 << watch.failures.min(6)).min(LONGEST)
            });
            if due <= now {
                self.fetch_pull_request(session, cx);
            } else {
                next = Some(next.map_or(due, |next| next.min(due)));
            }
        }
        self.pull_requests.timer = next.map(|at| {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(at - now).await;
                let _ = this.update(cx, |state, cx| state.poll_pull_requests(cx));
            })
        });
    }

    /// Asks gh about `session`'s pull request now, or once the answer it is waiting
    /// for arrives.
    pub(crate) fn refresh_pull_request(&mut self, session: u64, cx: &mut Context<Self>) {
        let watch = self.pull_requests.watches.entry(session).or_default();
        if watch.fetching {
            watch.again = true;
        } else {
            self.fetch_pull_request(session, cx);
        }
        self.refresh(cx);
    }

    fn fetch_pull_request(&mut self, session: u64, cx: &mut Context<Self>) {
        let folder = self.session_folder(session);
        let link = self
            .session(session)
            .and_then(|s| s.pull_request().cloned());
        let now = cx.background_executor().now();
        let watch = self.pull_requests.watches.entry(session).or_default();
        watch.fetching = true;
        watch.asked = Some(now);
        watch.asks += 1;
        let asks = watch.asks;
        let answer = self
            .pull_requests
            .run(Lane::Poll, move || ask(folder, link));
        cx.spawn(async move |this, cx| {
            let Ok(answer) = answer.await else {
                return;
            };
            let Ok(alerts) = this.update(cx, |state, cx| {
                state.pull_request_answered(session, asks, answer, cx)
            }) else {
                return;
            };
            cx.update(|cx| crate::notifications::offer(alerts, cx));
        })
        .detach();
    }

    /// Keeps gh's answer and returns what to notify about: failed checks and reviews
    /// new in it that the user has not seen.
    fn pull_request_answered(
        &mut self,
        session: u64,
        asks: u64,
        answer: Result<Answer, github::Error>,
        cx: &mut Context<Self>,
    ) -> Vec<(u64, Alert)> {
        let mut alerts = Vec::new();
        let saved = match answer {
            Ok(None) => Ok(()),
            Ok(Some((link, report))) => {
                let unseen = |state: &Self| {
                    state
                        .session(session)
                        .and_then(|s| s.pull_request_watch())
                        .map(|watch| watch.unseen())
                        .unwrap_or_default()
                };
                let before = self
                    .session(session)
                    .and_then(|s| s.pull_request_watch())
                    .map(|watch| watch.report.alerts())
                    .unwrap_or_default();
                let saved = self.record_pull_request(session, link, report, cx);
                let fresh = if self.pull_request_unseen(session) {
                    unseen(self)
                } else {
                    Default::default()
                };
                let fresh: Vec<_> = fresh.difference(&before).collect();
                for (alert, kind) in [(Alert::CheckFailed, true), (Alert::Review, false)] {
                    if fresh
                        .iter()
                        .any(|fresh| matches!(fresh, PullRequestAlert::CheckFailed(_)) == kind)
                    {
                        alerts.push((session, alert));
                    }
                }
                saved.map_err(|output| github::Error::Failed { output })
            }
            Err(error) => Err(error),
        };
        let watch = self.pull_requests.watches.entry(session).or_default();
        watch.fetching = false;
        match saved {
            Ok(()) => {
                watch.failures = 0;
                watch.error = None;
                if matches!(watch.merge, MergeStep::Failed(_) | MergeStep::Merged)
                    && asks > watch.merge_asks
                {
                    watch.merge = MergeStep::Idle;
                }
            }
            Err(error) => {
                watch.failures += 1;
                watch.error = Some(error);
            }
        }
        if std::mem::take(&mut watch.again) {
            watch.asked = None;
        }
        self.refresh(cx);
        self.poll_pull_requests(cx);
        alerts
    }

    /// Notes which session `window` showed in its Pull request surface this frame, and
    /// that the user has seen its pull request while the window is active.
    pub(crate) fn pull_request_drawn(
        &mut self,
        window: WindowId,
        shown: Option<u64>,
        active: bool,
        cx: &mut Context<Self>,
    ) {
        let moved = match shown {
            Some(session) => self.pull_requests.shown.insert(window, session) != Some(session),
            None => self.pull_requests.shown.remove(&window).is_some(),
        };
        if let Some(session) = shown.filter(|_| active)
            && self.pull_request_unseen(session)
        {
            self.notifier.seen_pull_request(session);
            self.see_pull_request(session, cx);
        }
        if moved || !self.pull_requests.started {
            self.poll_pull_requests(cx);
        }
    }

    /// Whether `window`'s last frame leaves anything for
    /// [`AppState::pull_request_drawn`] to do, so a frame that changes nothing writes
    /// nothing.
    pub(crate) fn pull_request_drawn_changes(
        &self,
        window: WindowId,
        shown: Option<u64>,
        active: bool,
    ) -> bool {
        !self.pull_requests.started
            || self.pull_requests.shown.get(&window).copied() != shown
            || shown.is_some_and(|session| active && self.pull_request_unseen(session))
    }

    /// Whether `session`'s pull request has a failed check or a review the user has
    /// not looked at.
    /// Only the session watching it shows a dot for it.
    pub(crate) fn pull_request_unseen(&self, session: u64) -> bool {
        let Some(s) = self.session(session) else {
            return false;
        };
        let (Some(link), Some(watch)) = (s.pull_request(), s.pull_request_watch()) else {
            return false;
        };
        !watch.unseen().is_empty()
            && self.pull_request_watchers().get(link.url.as_str()) == Some(&session)
    }

    /// Reads which merge methods the repository allows, for the confirmation. The
    /// methods go to the window that asked, if it is still open.
    fn merge_methods(
        &mut self,
        session: u64,
        link: PullRequest,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Vec<MergeMethod>> {
        self.merge_step(session, MergeStep::Asking, cx);
        let folder = gh_folder(self.session_folder(session));
        let methods = self.pull_requests.run(Lane::Merge, move || {
            github::merge_methods(&github::locate()?, &folder, &link)
        });
        let (sender, receiver) = oneshot::channel();
        cx.spawn(async move |this, cx| {
            let Ok(methods) = methods.await else {
                return;
            };
            let _ = this.update(cx, |state, cx| match methods {
                Ok(methods) if !methods.is_empty() => {
                    state.merge_step(session, MergeStep::Idle, cx);
                    let _ = sender.send(methods);
                }
                Ok(_) => {
                    let error = github::Error::Failed {
                        output: "The repository allows no merge method.".into(),
                    };
                    state.merge_step(session, MergeStep::Failed(error), cx)
                }
                Err(error) => state.merge_step(session, MergeStep::Failed(error), cx),
            });
        })
        .detach();
        receiver
    }

    fn merge_step(&mut self, session: u64, step: MergeStep, cx: &mut Context<Self>) {
        let watch = self.pull_requests.watches.entry(session).or_default();
        watch.merge = step;
        watch.merge_asks = watch.asks;
        self.refresh(cx);
    }

    /// Merges what the user confirmed, then asks gh about the pull request again.
    fn merge_pull_request(&mut self, confirmed: MergeConfirm, cx: &mut Context<Self>) {
        let MergeConfirm {
            session,
            link,
            head,
            method,
            ..
        } = confirmed;
        self.merge_step(session, MergeStep::Merging, cx);
        let folder = gh_folder(self.session_folder(session));
        let merged = self.pull_requests.run(Lane::Merge, move || {
            let gh = github::locate()?;
            // GitHub's merge endpoint does not refuse what the base branch's rules
            // block when the user may bypass them, so this asks first, as gh does.
            let report = github::view(&gh, &folder, &link)?;
            let refusal = match report.merge_state {
                _ if report.state != PullRequestState::Open => "It is no longer open.",
                state if state.can_merge() => "",
                MergeState::Blocked => "The base branch's rules block it.",
                MergeState::Behind => "The branch is behind its base.",
                MergeState::Dirty => "The branch has conflicts.",
                MergeState::Draft => "It is a draft.",
                _ => "GitHub is still checking whether it can merge.",
            };
            if !refusal.is_empty() {
                return Err(github::Error::Failed {
                    output: refusal.into(),
                });
            }
            github::merge(&gh, &folder, &link, method, &head)
        });
        cx.spawn(async move |this, cx| {
            let Ok(merged) = merged.await else {
                return;
            };
            let _ = this.update(cx, |state, cx| match merged {
                Ok(()) => {
                    state.merge_step(session, MergeStep::Merged, cx);
                    state.refresh_pull_request(session, cx);
                }
                Err(error) => state.merge_step(session, MergeStep::Failed(error), cx),
            });
        })
        .detach();
    }
}

fn duration(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3600, seconds / 60 % 60),
    }
}

fn check_name(check: &Check) -> String {
    if check.workflow.is_empty() {
        check.name.clone()
    } else {
        format!("{} / {}", check.workflow, check.name)
    }
}

fn comment_place(comment: &ReviewComment) -> String {
    match comment.line {
        Some(line) => format!("{}:{line}", comment.path),
        None => comment.path.clone(),
    }
}

/// What "Ask … to repair" puts in the composer: the failed check, and the first and
/// last 32 KiB of its failed steps' log, as tool output keeps, or its link without one.
fn repair_prompt(link: &PullRequest, check: &Check, log: Option<&str>) -> String {
    let mut prompt = format!(
        "Fix the failing check `{}` on {}.",
        check_name(check),
        link.url
    );
    match log.map(str::trim_end).filter(|log| !log.is_empty()) {
        Some(log) => {
            let log = keystone_agent::cap_output(log);
            let longest = log.split(|c| c != '`').map(str::len).max().unwrap_or(0);
            let fence = "`".repeat(longest.max(2) + 1);
            prompt.push_str(&format!("\n\n{fence}\n{log}\n{fence}"));
        }
        None if !check.url.is_empty() => prompt.push_str(&format!("\n\n{}", check.url)),
        None => {}
    }
    prompt
}

/// What "Send to …" puts in the composer: where the comment is, who wrote it, and
/// what it says.
fn comment_prompt(link: &PullRequest, comment: &ReviewComment) -> String {
    let quoted: Vec<String> = comment
        .body
        .lines()
        .map(|line| format!("> {line}").trim_end().to_owned())
        .collect();
    format!(
        "Address {}'s review comment on {} in {}:\n\n{}",
        comment.author,
        comment_place(comment),
        link.url,
        quoted.join("\n")
    )
}

impl Keystone {
    pub(crate) fn pull_request_tab_dot(&self, cx: &App) -> bool {
        self.selected(cx)
            .is_some_and(|session| self.state.read(cx).pull_request_unseen(session))
    }

    pub(crate) fn focus_pull_request(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .shown_composition(cx)
            .views()
            .any(|(_, component)| component == Component::PullRequest)
        {
            window.focus(&self.pull_request_focus, cx);
        }
    }

    /// Reports this frame's Pull request surface to the app once the frame is built.
    /// One under the settings page or the narrow window's sidebar is not shown.
    pub(crate) fn note_pull_request(&mut self, window: &Window, cx: &mut Context<Self>) {
        let shown = self.pull_request_drawn.take().filter(|_| !self.covered(cx));
        let active = window.is_window_active();
        let id = self.window_id;
        if self
            .state
            .read(cx)
            .pull_request_drawn_changes(id, shown, active)
        {
            self.state.update(cx, |state, cx| {
                state.pull_request_drawn(id, shown, active, cx)
            });
        }
    }

    fn ask_to_merge(&mut self, session: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.state.read(cx).session(session) else {
            return;
        };
        let (Some(link), Some(watch)) = (s.pull_request().cloned(), s.pull_request_watch()) else {
            return;
        };
        let report = watch.report.clone();
        let methods = self.state.update(cx, |state, cx| {
            state.merge_methods(session, link.clone(), cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(methods) = methods.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.merge_confirm = Some(MergeConfirm {
                    session,
                    link,
                    head: report.head_oid,
                    base: report.base,
                    branch: report.head,
                    method: methods[0],
                    methods,
                    focus: DialogFocus::new(window, cx),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// "Ask … to repair": reads the failed check's log through gh, then puts the check
    /// and its log in the session's composer for the user to send. A log gh cannot
    /// read, as while other jobs of its run are still going, leaves the check's link
    /// in its place and gh's words in the error line. A click while a log is read does
    /// nothing.
    fn ask_to_repair(
        &mut self,
        session: u64,
        link: PullRequest,
        check: Check,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.repair.is_some() {
            return;
        }
        let job = github::Job::of(&check.url, &link);
        let log = self.state.update(cx, |state, _| {
            let folder = gh_folder(state.session_folder(session));
            state.pull_requests.run(Lane::Log, move || match job {
                Some(job) => github::failed_log(&github::locate()?, &folder, &job).map(Some),
                None => Ok(None),
            })
        });
        self.repair = Some(cx.spawn_in(window, async move |this, cx| {
            let log = log.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.repair = None;
                let Ok(log) = log else {
                    return this.refresh(cx);
                };
                let text =
                    repair_prompt(&link, &check, log.as_ref().ok().and_then(Option::as_deref));
                this.prefill(session, text, window, cx);
                if let Err(error) = log {
                    this.error = Some(format!("Log not read. {error}"));
                    this.refresh(cx);
                }
            });
        }));
    }

    /// Puts `text` in `session`'s draft for the user to send: in the composer of a
    /// window that shows the session, this one first, else after the draft saved for it.
    /// Another window's composer is filled once this update ends, since saving it shares
    /// the draft with this window.
    fn prefill(&mut self, session: u64, text: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected(cx) == Some(session) {
            return self.insert_into_composer(&text, window, cx);
        }
        let this = cx.entity();
        if let Some(other) = crate::open_windows(cx)
            .into_iter()
            .find(|other| *other != this && other.read(cx).selected(cx) == Some(session))
        {
            let handle = other.read(cx).handle;
            cx.defer(move |cx| {
                let _ = handle.update(cx, |_, window, cx| {
                    other.update(cx, |other, cx| {
                        other.insert_into_composer(&text, window, cx)
                    })
                });
            });
            return;
        }
        let Some(draft) = self
            .state
            .read(cx)
            .session(session)
            .map(|s| s.draft().to_owned())
        else {
            return;
        };
        let draft = format!("{draft}{}", crate::on_own_line(&draft, &text, ""));
        self.error = match self
            .state
            .update(cx, |state, cx| state.set_draft(session, draft, cx))
        {
            Ok(applied) => crate::durability_warning(applied.durability),
            Err(error) => Some(format!("Draft not saved. {error}")),
        };
        self.refresh(cx);
    }

    fn close_merge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.merge_confirm = None;
        window.focus(&self.pull_request_focus, cx);
        cx.notify();
    }

    pub(crate) fn merge_dialog(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let confirm = self.merge_confirm.as_ref()?;
        let methods = (confirm.methods.len() > 1).then(|| {
            h_flex()
                .gap_1()
                .children(confirm.methods.iter().map(|&method| {
                    let id = match method {
                        MergeMethod::Squash => "merge-method-squash",
                        MergeMethod::Merge => "merge-method-merge",
                        MergeMethod::Rebase => "merge-method-rebase",
                    };
                    Button::new(id, method.label())
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Compact)
                        .selected(method == confirm.method)
                        .debug_selector(|| id.into())
                        .on_click(self.scoped_listener(cx, move |this, _, _, cx| {
                            if let Some(confirm) = &mut this.merge_confirm {
                                confirm.method = method;
                                cx.notify();
                            }
                        }))
                }))
        });
        let cancel =
            self.scoped_listener(cx, |this, _: &(), window, cx| this.close_merge(window, cx));
        let merge = self.scoped_listener(cx, |this, _: &(), window, cx| {
            if let Some(confirmed) = this.merge_confirm.take() {
                this.state
                    .update(cx, |state, cx| state.merge_pull_request(confirmed, cx));
            }
            this.close_merge(window, cx)
        });
        Some(
            ConfirmDialog::new(
                "merge-dialog",
                &confirm.focus,
                format!("Merge #{}?", confirm.link.number),
                format!("{} into {}", confirm.branch, confirm.base),
            )
            .icon(IconName::PullRequest)
            .tone(Color::Accent)
            .confirm_label(confirm.method.label())
            .when_some(methods, |dialog, methods| dialog.body(methods))
            .on_cancel(move |window, cx| cancel(&(), window, cx))
            .on_confirm(move |window, cx| merge(&(), window, cx)),
        )
    }

    pub(crate) fn pull_request_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let session = self.selected(cx);
        self.pull_request_drawn = session;
        let body = match session {
            Some(session) => self.pull_request_body(session, cx),
            None => empty("No pull request."),
        };
        div()
            .id("pull-request")
            .track_focus(&self.pull_request_focus)
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .child(body)
            .into_any_element()
    }

    fn pull_request_body(&self, session: u64, cx: &mut Context<Self>) -> AnyElement {
        let state = self.state.read(cx);
        let watch = state.pull_requests.watches.get(&session);
        let fetching = watch.is_some_and(|w| w.fetching);
        let error = watch.and_then(|w| w.error.clone());
        let merge = watch.map(|w| w.merge.clone()).unwrap_or_default();
        let folder = state.session_folder(session);
        let Some(s) = state.session(session) else {
            return empty("No pull request.");
        };
        let link = s.pull_request().cloned();
        let report = s.pull_request_watch().map(|w| w.report.clone());
        let provider = s.provider();
        let asked = watch.is_some_and(|w| w.asked.is_some());
        let refresh = IconButton::new("pull-request-refresh", IconName::Refresh)
            .size(ButtonSize::Compact)
            .disabled(fetching)
            .debug_selector(|| "pull-request-refresh".into())
            .tooltip(Tooltip::text("Refresh"))
            .on_click(self.scoped_listener(cx, move |this, _, _, cx| {
                this.state
                    .update(cx, |state, cx| state.refresh_pull_request(session, cx));
            }));
        let read_notice = error
            .as_ref()
            .map(|error| notice("Could not read the pull request.", error, cx));
        let (Some(link), Some(report)) = (link, report) else {
            // Blank until gh first answers, so "No pull request" is never a guess.
            let text = if fetching || !asked || error.is_some() {
                ""
            } else {
                "No pull request."
            };
            return v_flex()
                .child(
                    h_flex()
                        .px(px(12.))
                        .py_2()
                        .gap_2()
                        .child(div().flex_1().child(empty(text)))
                        .child(refresh),
                )
                .children(read_notice)
                .into_any_element();
        };
        let merge_notice = match &merge {
            MergeStep::Failed(error) => Some(notice("Could not merge.", error, cx)),
            _ => None,
        };
        let (state_word, state_color) = match report.state {
            PullRequestState::Open => ("Open", Color::Success),
            PullRequestState::Draft => ("Draft", Color::Muted),
            PullRequestState::Merged => ("Merged", Color::Accent),
            PullRequestState::Closed => ("Closed", Color::Danger),
        };
        let decision = report.review_decision.map(|decision| match decision {
            ReviewDecision::Approved => ("Approved", Color::Success),
            ReviewDecision::ChangesRequested => ("Changes requested", Color::Danger),
            ReviewDecision::ReviewRequired => ("Review required", Color::Muted),
        });
        let conflicts = (report.state.is_open() && report.merge_state == MergeState::Dirty)
            .then_some(("Conflicts", Color::Warning));
        let busy = matches!(
            merge,
            MergeStep::Asking | MergeStep::Merging | MergeStep::Merged
        );
        let merge_button =
            (report.state == PullRequestState::Open && report.merge_state.can_merge()).then(|| {
                Button::new(
                    "pull-request-merge",
                    if matches!(merge, MergeStep::Merging | MergeStep::Merged) {
                        "Merging…"
                    } else {
                        "Merge"
                    },
                )
                .style(ButtonStyle::Subtle)
                .size(ButtonSize::Compact)
                .disabled(busy)
                .debug_selector(|| "pull-request-merge".into())
                .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                    this.ask_to_merge(session, window, cx)
                }))
            });
        let word = |(text, color): (&'static str, Color), id: &'static str| {
            div()
                .flex_none()
                .automation_id(id)
                .child(Label::new(text).size(TextSize::Sm).color(color))
        };
        let header = v_flex()
            .px(px(12.))
            .pt(px(10.))
            .pb_2()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .automation_id("pull-request-title")
                            .child(
                                Label::new(report.title.clone())
                                    .size(TextSize::Ui)
                                    .color(Color::Strong),
                            ),
                    )
                    .child(refresh),
            )
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(
                        Label::new(format!("#{}", link.number))
                            .size(TextSize::Sm)
                            .color(Color::Muted),
                    )
                    .child(word((state_word, state_color), "pull-request-state"))
                    .children(decision.map(|d| word(d, "pull-request-review-decision")))
                    .children(conflicts.map(|c| word(c, "pull-request-conflicts")))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .automation_id("pull-request-branches")
                            .child(
                                Label::new(format!("{} → {}", report.head, report.base))
                                    .size(TextSize::Sm)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .children(merge_button),
            );
        let now = crate::now_ms() / 1000;
        let hover = cx.theme().colors.element_hover;
        let open = report.state.is_open();
        let pending = Color::Warning.to_hsla(cx);
        let checks = (!report.checks.is_empty()).then(|| {
            section(
                report
                    .checks
                    .iter()
                    .enumerate()
                    .map(|(index, check)| {
                        let icon = |name: IconName, color: Color| {
                            Icon::new(name)
                                .size(px(14.))
                                .color(color)
                                .into_any_element()
                        };
                        let mark = match check.state {
                            CheckState::Passed => icon(IconName::Check, Color::Success),
                            CheckState::Failed => icon(IconName::Close, Color::Danger),
                            CheckState::Neutral => icon(IconName::Minus, Color::Muted),
                            CheckState::Pending => div()
                                .size(px(14.))
                                .flex()
                                .flex_none()
                                .items_center()
                                .justify_center()
                                .child(
                                    div()
                                        .size(px(8.))
                                        .rounded_full()
                                        .border_1()
                                        .border_color(pending),
                                )
                                .into_any_element(),
                        };
                        let end = match check.state {
                            CheckState::Pending => Some(now),
                            _ => check.completed_at,
                        };
                        let summary = match check.started_at.zip(end) {
                            Some((start, end)) => format!(
                                "{} · {}",
                                check.summary,
                                duration(end.saturating_sub(start))
                            ),
                            None => check.summary.clone(),
                        };
                        let name = check_name(check);
                        let url = check.url.clone();
                        let repair =
                            (open && check.state == CheckState::Failed).then(|| {
                                let (link, check) = (link.clone(), check.clone());
                                Button::new(
                                    ("pull-request-repair", index),
                                    format!("Ask {provider} to repair"),
                                )
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Compact)
                                .debug_selector(move || format!("pull-request-repair-{index}"))
                                .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                                    this.ask_to_repair(
                                        session,
                                        link.clone(),
                                        check.clone(),
                                        window,
                                        cx,
                                    )
                                }))
                            });
                        h_flex()
                            .id(("pull-request-check", index))
                            .automation_id(format!("pull-request-check-{index}"))
                            .h(px(26.))
                            .px_2()
                            .gap_2()
                            .rounded(px(6.))
                            .when(!url.is_empty(), |d| {
                                d.cursor_pointer()
                                    .hover(|s| s.bg(hover))
                                    .on_click(move |_, _, cx| cx.open_url(&url))
                            })
                            .child(mark)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Label::new(name).size(TextSize::Ui).truncate()),
                            )
                            .child(
                                Label::new(summary)
                                    .size(TextSize::Sm)
                                    .color(if check.state == CheckState::Failed {
                                        Color::Danger
                                    } else {
                                        Color::Muted
                                    })
                                    .tabular(),
                            )
                            .children(repair)
                            .into_any_element()
                    })
                    .collect(),
            )
        });
        let mut reviews: Vec<AnyElement> = report
            .reviews
            .iter()
            .filter(|review| review.state != ReviewState::Commented || !review.body.is_empty())
            .enumerate()
            .map(|(index, review)| {
                let verdict = match review.state {
                    ReviewState::Approved => Some(("Approved", Color::Success)),
                    ReviewState::ChangesRequested => Some(("Changes requested", Color::Danger)),
                    ReviewState::Commented => None,
                    ReviewState::Dismissed => Some(("Dismissed", Color::Faint)),
                };
                v_flex()
                    .automation_id(format!("pull-request-review-{index}"))
                    .px_2()
                    .py_1()
                    .gap(px(2.))
                    .child(
                        h_flex()
                            .gap(px(6.))
                            .child(Label::new(review.author.clone()).size(TextSize::Ui))
                            .children(verdict.map(|(verdict, color)| {
                                Label::new(verdict).size(TextSize::Sm).color(color)
                            })),
                    )
                    .when(!review.body.is_empty(), |d| {
                        d.child(
                            Label::new(review.body.clone())
                                .size(TextSize::Sm)
                                .color(Color::Muted),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        reviews.extend(report.comments.iter().enumerate().map(|(index, comment)| {
            let place = comment_place(comment);
            let path = folder.clone().map(|folder| (folder, comment.path.clone()));
            let send = open.then(|| {
                let text = comment_prompt(&link, comment);
                Button::new(("pull-request-send", index), format!("Send to {provider}"))
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .debug_selector(move || format!("pull-request-send-{index}"))
                    .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                        this.prefill(session, text.clone(), window, cx)
                    }))
            });
            v_flex()
                .id(("pull-request-comment", index))
                .automation_id(format!("pull-request-comment-{index}"))
                .px_2()
                .py_1()
                .gap(px(2.))
                .rounded(px(6.))
                .when_some(path, |d, (folder, path)| {
                    d.cursor_pointer()
                        .hover(|s| s.bg(hover))
                        .on_click(self.scoped_listener(cx, move |this, _, _, cx| {
                            let path = repository_root(&folder).join(&path);
                            this.error = crate::files::open_in_editor(&path).err();
                            cx.notify();
                        }))
                })
                .child(
                    h_flex()
                        .gap(px(6.))
                        .child(
                            div().min_w_0().child(
                                Label::new(place)
                                    .size(TextSize::Sm)
                                    .color(Color::Accent)
                                    .truncate(),
                            ),
                        )
                        .child(
                            Label::new(comment.author.clone())
                                .size(TextSize::Sm)
                                .color(Color::Muted),
                        )
                        .child(div().flex_1())
                        .children(send),
                )
                .child(Label::new(comment.body.clone()).size(TextSize::Ui))
                .into_any_element()
        }));
        let reviews = (!reviews.is_empty()).then(|| section(reviews));
        v_flex()
            .pb_2()
            .child(header)
            .children(read_notice)
            .children(merge_notice)
            .children(checks)
            .children(reviews)
            .into_any_element()
    }
}

fn empty(text: &'static str) -> AnyElement {
    div()
        .px(px(12.))
        .py_2()
        .automation_id("pull-request-empty")
        .child(Label::new(text).size(TextSize::Ui).color(Color::Muted))
        .into_any_element()
}

fn section(rows: Vec<AnyElement>) -> AnyElement {
    v_flex().px_1().pt_3().children(rows).into_any_element()
}

/// What to tell the user when gh can't answer: the command to run when gh is missing
/// or signed out, else gh's own words under `title`.
fn notice(title: &'static str, error: &github::Error, cx: &mut Context<Keystone>) -> AnyElement {
    let colors = cx.theme().colors;
    let (id, title, command, output, color) = match error {
        github::Error::Missing => (
            "pull-request-hint-gh-missing",
            github::INSTALL_HINT,
            Some(github::SIGN_IN),
            None,
            Color::Warning,
        ),
        github::Error::SignedOut { .. } => (
            "pull-request-hint-gh-signed-out",
            github::SIGN_IN_HINT,
            Some(github::SIGN_IN),
            None,
            Color::Warning,
        ),
        github::Error::Failed { output } => (
            "pull-request-failed",
            title,
            None,
            Some(output.clone()),
            Color::Danger,
        ),
        github::Error::Exists(_) => ("pull-request-failed", title, None, None, Color::Danger),
    };
    let boxed = |text: String, id: &'static str| {
        div()
            .flex_1()
            .min_w_0()
            .px_2()
            .py_1()
            .rounded(px(6.))
            .bg(colors.background)
            .border_1()
            .border_color(colors.border)
            .font_mono(cx)
            .text_size(TextSize::Sm.px())
            .text_color(colors.text)
            .automation_id(id)
            .child(text)
    };
    v_flex()
        .mx(px(12.))
        .mb_2()
        .gap_1()
        .automation_id(id)
        .child(Label::new(title).size(TextSize::Ui).color(color))
        .children(command.map(|command| {
            h_flex()
                .gap_1()
                .child(boxed(command.into(), "pull-request-hint-command"))
                .child(
                    IconButton::new("pull-request-hint-copy", IconName::Copy)
                        .size(ButtonSize::Compact)
                        .debug_selector(|| "pull-request-hint-copy".into())
                        .tooltip(Tooltip::text("Copy command"))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(command.into()))
                        }),
                )
        }))
        .children(
            output
                .filter(|o| !o.trim().is_empty())
                .map(|output| boxed(output.trim_end().to_owned(), "pull-request-output")),
        )
        .into_any_element()
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "pull_request_tests.rs"]
mod tests;
