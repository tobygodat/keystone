//! Settings → Scheduled: every scheduled task with its next run and last runs, with Run
//! now, Pause or Resume and Delete, and a form to add one. A run's Open shows its
//! session. See `docs/design/scheduled-tasks.md`.

use crate::Keystone;
use crate::relative_time::{LocalTime, clock};
use crate::settings_page::SettingsPage;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable, FontWeight, Window, div,
    prelude::*, px,
};
use keystone_core::{
    Days, Outcome, ProjectId, Provider, Run, Schedule, ScheduledTask, Target, TaskFields,
};
use keystone_ui::prelude::*;
use keystone_ui::text_input::{InputMode, TextInput};
use std::cell::RefCell;
use std::collections::HashMap;

/// How many of a task's runs the section shows, newest first.
const SHOWN_RUNS: usize = 5;
/// The longest interval the form accepts: a week.
const MAX_MINUTES: u32 = 7 * 24 * 60;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Every,
    Daily,
    Weekdays,
}

const KINDS: [(Kind, &str); 3] = [
    (Kind::Every, "Every N minutes"),
    (Kind::Daily, "Every day at"),
    (Kind::Weekdays, "Weekdays at"),
];

/// The section's own state, kept while the settings page is open.
pub(crate) struct ScheduledPage {
    title: Entity<TextInput>,
    prompt: Entity<TextInput>,
    /// Minutes for an interval, or a time of day as HH:MM.
    when: Entity<TextInput>,
    kind: Kind,
    /// The project the form saves to, or `None` for this window's project.
    project: Option<ProjectId>,
    target: Target,
    kind_focus: FocusHandle,
    project_focus: FocusHandle,
    target_focus: FocusHandle,
    add: FocusHandle,
    /// Focus for each row's buttons, made as rows first render.
    stops: RefCell<HashMap<String, FocusHandle>>,
}

fn field(placeholder: &str, mode: InputMode, cx: &mut App) -> Entity<TextInput> {
    let input = cx.new(|cx| TextInput::new(placeholder, mode, cx));
    let _ = input.read(cx).focus_handle(cx).tab_stop(true);
    input
}

impl ScheduledPage {
    pub(crate) fn new(cx: &mut App) -> Self {
        Self {
            title: field("Title", InputMode::SingleLine, cx),
            prompt: field("Prompt", InputMode::Multiline, cx),
            when: field("15 or 09:30", InputMode::SingleLine, cx),
            kind: Kind::Every,
            project: None,
            target: Target::New {
                provider: Provider::Claude,
            },
            kind_focus: cx.focus_handle().tab_stop(true),
            project_focus: cx.focus_handle().tab_stop(true),
            target_focus: cx.focus_handle().tab_stop(true),
            add: cx.focus_handle().tab_stop(true),
            stops: RefCell::default(),
        }
    }

    fn stop(&self, id: &str, cx: &App) -> FocusHandle {
        self.stops
            .borrow_mut()
            .entry(id.to_owned())
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone()
    }
}

/// `Every 15 minutes`, `Every day at 09:00`, `Weekdays at 09:00`.
fn describe(schedule: Schedule) -> String {
    match schedule {
        Schedule::Every { minutes: 1 } => "Every minute".into(),
        Schedule::Every { minutes: 60 } => "Every hour".into(),
        Schedule::Every { minutes } if minutes % 60 == 0 => {
            format!("Every {} hours", minutes / 60)
        }
        Schedule::Every { minutes } => format!("Every {minutes} minutes"),
        Schedule::At { hour, minute, days } => {
            let days = match days {
                Days::EVERY => "Every day",
                Days::WEEKDAYS => "Weekdays",
                _ => "Chosen days",
            };
            format!("{days} at {hour:02}:{minute:02}")
        }
    }
}

/// The schedule the form describes, or what to fix.
fn parse(kind: Kind, when: &str) -> Result<Schedule, &'static str> {
    let when = when.trim();
    if kind == Kind::Every {
        return match when.parse::<u32>() {
            Ok(minutes) if (1..=MAX_MINUTES).contains(&minutes) => Ok(Schedule::Every { minutes }),
            _ => Err("Enter the minutes between runs, from 1 to 10080."),
        };
    }
    let time = when.split_once(':').and_then(|(hour, minute)| {
        let hour = hour.parse::<u8>().ok().filter(|&h| h < 24)?;
        let minute = Some(minute)
            .filter(|m| m.len() == 2)?
            .parse::<u8>()
            .ok()
            .filter(|&m| m < 60)?;
        Some((hour, minute))
    });
    let Some((hour, minute)) = time else {
        return Err("Enter a time as HH:MM, such as 09:30.");
    };
    let days = if kind == Kind::Weekdays {
        Days::WEEKDAYS
    } else {
        Days::EVERY
    };
    Ok(Schedule::At { hour, minute, days })
}

fn outcome_text(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Started { .. } => "Started".into(),
        Outcome::Failed { reason, .. } => format!("Failed. {reason}"),
        Outcome::Skipped { reason } => format!("Skipped. {reason}"),
    }
}

fn run_session(outcome: &Outcome) -> Option<u64> {
    match outcome {
        Outcome::Started { session } => Some(*session),
        Outcome::Failed { session, .. } => *session,
        Outcome::Skipped { .. } => None,
    }
}

impl Keystone {
    fn scheduled_page(&mut self) -> Option<&mut ScheduledPage> {
        self.settings.as_mut().map(|page| &mut page.scheduled)
    }

    /// The project the form saves to: the one chosen, else this window's.
    fn schedule_project(&self, page: &ScheduledPage) -> Option<ProjectId> {
        page.project
            .clone()
            .or_else(|| self.presentation.project.as_ref().ok().cloned())
    }

    /// Every project the form offers: this window's first, then the rest listed.
    fn schedule_projects(&self, cx: &App) -> Vec<ProjectId> {
        let mut projects: Vec<ProjectId> = self
            .presentation
            .project
            .as_ref()
            .ok()
            .cloned()
            .into_iter()
            .collect();
        for project in self.state.read(cx).store().workspace().projects() {
            if !projects.contains(&project.id()) {
                projects.push(project.id());
            }
        }
        projects
    }

    /// Where a run may go: a new session on either provider, or one of the project's
    /// sessions, each with its label.
    fn schedule_targets(&self, project: Option<&ProjectId>, cx: &App) -> Vec<(String, Target)> {
        let mut targets = vec![
            (
                "New Claude session".to_owned(),
                Target::New {
                    provider: Provider::Claude,
                },
            ),
            (
                "New Codex session".to_owned(),
                Target::New {
                    provider: Provider::Codex,
                },
            ),
        ];
        if let Some(project) = project {
            targets.extend(
                self.state
                    .read(cx)
                    .store()
                    .workspace()
                    .sessions()
                    .iter()
                    .filter(|session| crate::projects::holds(project, session))
                    .map(|session| {
                        (
                            format!("In \u{201c}{}\u{201d}", session.title()),
                            Target::Session {
                                session: session.id(),
                            },
                        )
                    }),
            );
        }
        targets
    }

    fn describe_target(&self, target: Target, cx: &App) -> String {
        match target {
            Target::New {
                provider: Provider::Claude,
            } => "New Claude session".into(),
            Target::New {
                provider: Provider::Codex,
            } => "New Codex session".into(),
            Target::Session { session } => match self.state.read(cx).session(session) {
                Some(session) => format!("In \u{201c}{}\u{201d}", session.title()),
                None => "In a deleted session".into(),
            },
        }
    }

    /// The Scheduled section's content, below its title.
    pub(crate) fn scheduled_section(
        &self,
        page: &SettingsPage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.state.read(cx);
        let tasks = state.scheduled_tasks().to_vec();
        let problem = state.scheduler_error().map(str::to_owned);
        let rows: Vec<_> = tasks
            .iter()
            .map(|task| self.scheduled_row(&page.scheduled, task, cx))
            .collect();
        v_flex()
            .automation_id("settings-scheduled")
            .child(
                Label::new("Tasks run while Keystone is open. One that came due while it was closed runs when it next opens.")
                    .size(TextSize::Sm)
                    .color(Color::Muted),
            )
            .children(problem.map(|text| {
                div()
                    .pt(spacing::SM)
                    .automation_id("scheduled-problem")
                    .child(Label::new(text).size(TextSize::Sm).color(Color::Warning))
            }))
            .child(v_flex().pt(spacing::MD).children(rows))
            .child(self.scheduled_form(&page.scheduled, cx))
            .into_any_element()
    }

    fn scheduled_row(
        &self,
        page: &ScheduledPage,
        task: &ScheduledTask,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = task.id();
        let now = LocalTime::at(crate::now_ms());
        let paused = task.next_run().is_none();
        let button = |name: &str, label: &'static str| {
            let selector = format!("scheduled-{name}-{id}");
            Button::new(selector.clone(), label)
                .style(ButtonStyle::Subtle)
                .size(ButtonSize::Compact)
                .track_focus(&page.stop(&selector, cx))
                .debug_selector(move || selector.clone())
        };
        let actions = h_flex()
            .flex_none()
            .gap_1()
            .child(button("run", "Run now").on_click(self.scoped_listener(
                cx,
                move |this, _, _, cx| {
                    this.state
                        .update(cx, |state, cx| state.run_scheduled_task_now(id, cx));
                    this.refresh(cx);
                },
            )))
            .child(
                button("pause", if paused { "Resume" } else { "Pause" }).on_click(
                    self.scoped_listener(cx, move |this, _, _, cx| {
                        let result = this.state.update(cx, |state, cx| {
                            if paused {
                                state.resume_scheduled_task(id, cx)
                            } else {
                                state.pause_scheduled_task(id, cx)
                            }
                        });
                        this.error = result.err().map(|e| format!("Task not changed. {e}"));
                        this.refresh(cx);
                    }),
                ),
            )
            .child({
                let selector = format!("scheduled-delete-{id}");
                IconButton::new(selector.clone(), IconName::Close)
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .track_focus(&page.stop(&selector, cx))
                    .debug_selector(move || selector.clone())
                    .aria_label("Delete")
                    .tooltip(Tooltip::text("Delete"))
                    .on_click(self.scoped_listener(cx, move |this, _, _, cx| {
                        let result = this
                            .state
                            .update(cx, |state, cx| state.delete_scheduled_task(id, cx));
                        this.error = result.err().map(|e| format!("Task not deleted. {e}"));
                        this.refresh(cx);
                    }))
            });
        let next = match task.next_run() {
            Some(at) => format!("Next {}", clock(now, LocalTime::at(at))),
            None => "Paused".into(),
        };
        let detail = format!(
            "{} \u{b7} {} \u{b7} {} \u{b7} {next}",
            describe(task.schedule()),
            task.project().name(),
            self.describe_target(task.target(), cx),
        );
        let runs: Vec<_> = task
            .runs()
            .iter()
            .rev()
            .take(SHOWN_RUNS)
            .enumerate()
            .map(|(at, run)| self.scheduled_run(page, id, at, run, now, cx))
            .collect();
        v_flex()
            .automation_id(format!("scheduled-row-{id}"))
            .gap(px(2.))
            .py(spacing::SM)
            .hairline_t(cx)
            .child(
                h_flex()
                    .gap(spacing::MD)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                Label::new(task.title().to_owned())
                                    .weight(FontWeight::SEMIBOLD)
                                    .truncate(),
                            )
                            .child(
                                Label::new(detail)
                                    .size(TextSize::Sm)
                                    .color(Color::Faint)
                                    .truncate(),
                            ),
                    )
                    .child(actions),
            )
            .children(runs)
    }

    fn scheduled_run(
        &self,
        page: &ScheduledPage,
        task: u64,
        at: usize,
        run: &Run,
        now: LocalTime,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let when = clock(now, LocalTime::at(run.at));
        let late = run
            .due
            .filter(|&due| run.at.saturating_sub(due) >= 60_000)
            .map(|due| format!(", due {}", clock(now, LocalTime::at(due))))
            .unwrap_or_default();
        let label = match run.due {
            Some(_) => format!("{when}{late}: {}", outcome_text(&run.outcome)),
            None => format!("{when}, run now: {}", outcome_text(&run.outcome)),
        };
        let color = match run.outcome {
            Outcome::Started { .. } => Color::Muted,
            _ => Color::Warning,
        };
        let open = run_session(&run.outcome)
            .filter(|&session| self.state.read(cx).session(session).is_some())
            .map(|session| {
                let selector = format!("scheduled-open-{task}-{at}");
                Button::new(selector.clone(), "Open")
                    .style(ButtonStyle::Ghost)
                    .size(ButtonSize::Compact)
                    .track_focus(&page.stop(&selector, cx))
                    .debug_selector(move || selector.clone())
                    .on_click(move |_, _, cx| {
                        cx.defer(move |cx| crate::notifications::show(session, cx))
                    })
            });
        h_flex()
            .automation_id(format!("scheduled-history-{task}-{at}"))
            .gap(spacing::SM)
            .pl(spacing::MD)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(label).size(TextSize::Sm).color(color).truncate()),
            )
            .children(open)
    }

    fn scheduled_form(
        &self,
        page: &ScheduledPage,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let colors = cx.theme().colors;
        let boxed = |entity: &Entity<TextInput>, id: &'static str| {
            div()
                .flex_1()
                .min_w(px(120.))
                .px(px(8.))
                .rounded(px(6.))
                .border_1()
                .border_color(colors.border)
                .bg(colors.background)
                .automation_id(id)
                .child(entity.clone())
        };
        let projects = self.schedule_projects(cx);
        let project = self.schedule_project(page);
        let project_at = projects.iter().position(|p| Some(p) == project.as_ref());
        let project_labels: Vec<String> = projects.iter().map(|p| p.name().to_owned()).collect();
        let targets = self.schedule_targets(project.as_ref(), cx);
        let target_at = targets.iter().position(|(_, t)| *t == page.target);
        let target_labels: Vec<String> = targets.iter().map(|(label, _)| label.clone()).collect();
        let kind_at = KINDS.iter().position(|(kind, _)| *kind == page.kind);
        let kind_labels: Vec<&str> = KINDS.iter().map(|(_, label)| *label).collect();
        v_flex()
            .pt(spacing::LG)
            .gap(spacing::SM)
            .automation_id("scheduled-form")
            .child(boxed(&page.title, "scheduled-title"))
            .child({
                let prompt = page.prompt.clone();
                boxed(&page.prompt, "scheduled-prompt")
                    .min_h(px(88.))
                    .py(px(6.))
                    .cursor_text()
                    .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                        prompt.focus_handle(cx).focus(window, cx);
                        cx.stop_propagation();
                    })
            })
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(spacing::SM)
                    .child(
                        div().flex_none().child(
                            Select::new("scheduled-kind", kind_labels, kind_at)
                                .width(px(180.))
                                .track_focus(&page.kind_focus)
                                .on_change(self.scoped_listener(cx, |this, &at: &usize, _, cx| {
                                    let Some(page) = this.scheduled_page() else {
                                        return;
                                    };
                                    page.kind = KINDS[at].0;
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(
                        div()
                            .w(px(120.))
                            .flex_none()
                            .child(boxed(&page.when, "scheduled-when")),
                    )
                    .child(
                        div().flex_none().child(
                            Select::new("scheduled-project", project_labels, project_at)
                                .placeholder("Project")
                                .width(px(180.))
                                .track_focus(&page.project_focus)
                                .on_change(self.scoped_listener(
                                    cx,
                                    move |this, &at: &usize, _, cx| {
                                        if let Some(page) = this.scheduled_page() {
                                            page.project = projects.get(at).cloned();
                                            if matches!(page.target, Target::Session { .. }) {
                                                page.target = Target::New {
                                                    provider: Provider::Claude,
                                                };
                                            }
                                        }
                                        cx.notify();
                                    },
                                )),
                        ),
                    ),
            )
            .child(
                h_flex()
                    .gap(spacing::SM)
                    .child(
                        div().flex_none().child(
                            Select::new("scheduled-target", target_labels, target_at)
                                .width(px(260.))
                                .track_focus(&page.target_focus)
                                .on_change(self.scoped_listener(
                                    cx,
                                    move |this, &at: &usize, _, cx| {
                                        if let (Some(page), Some((_, target))) =
                                            (this.scheduled_page(), targets.get(at))
                                        {
                                            page.target = *target;
                                        }
                                        cx.notify();
                                    },
                                )),
                        ),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("scheduled-add", "Add")
                            .style(ButtonStyle::Filled)
                            .track_focus(&page.add)
                            .debug_selector(|| "scheduled-add".into())
                            .on_click(self.scoped_listener(cx, |this, _, window, cx| {
                                this.add_scheduled(window, cx)
                            })),
                    ),
            )
    }

    fn add_scheduled(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(settings) = &self.settings else {
            return;
        };
        let page = &settings.scheduled;
        let title = page.title.read(cx).value().trim().to_owned();
        let prompt = page.prompt.read(cx).value().trim().to_owned();
        let schedule = parse(page.kind, page.when.read(cx).value());
        let fields = match (self.schedule_project(page), schedule) {
            _ if title.is_empty() => Err("Give the task a title."),
            _ if prompt.is_empty() => Err("Write the prompt the task sends."),
            (None, _) => Err("Choose a project."),
            (_, Err(problem)) => Err(problem),
            (Some(project), _)
                if !self
                    .schedule_targets(Some(&project), cx)
                    .iter()
                    .any(|(_, target)| *target == page.target) =>
            {
                Err("Choose where the task runs in this project.")
            }
            (Some(project), Ok(schedule)) => Ok(TaskFields {
                title,
                prompt,
                project,
                schedule,
                target: page.target,
            }),
        };
        let result = fields.map_err(str::to_owned).and_then(|fields| {
            self.state
                .update(cx, |state, cx| state.add_scheduled_task(fields, cx))
                .map_err(|error| format!("Task not saved. {error}"))
        });
        match result {
            Ok(()) => {
                self.error = None;
                let Some(page) = self.scheduled_page() else {
                    return;
                };
                let inputs = [page.title.clone(), page.prompt.clone(), page.when.clone()];
                for input in &inputs {
                    input.update(cx, |input, cx| input.set_value(String::new(), cx));
                }
                inputs[0].focus_handle(cx).focus(window, cx);
            }
            Err(error) => self.error = Some(error),
        }
        self.refresh(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_form_reads_minutes_and_times_and_names_what_to_fix() {
        assert_eq!(
            parse(Kind::Every, " 15 "),
            Ok(Schedule::Every { minutes: 15 })
        );
        assert!(parse(Kind::Every, "0").is_err());
        assert!(parse(Kind::Every, "10081").is_err());
        assert_eq!(
            parse(Kind::Weekdays, "9:05"),
            Ok(Schedule::At {
                hour: 9,
                minute: 5,
                days: Days::WEEKDAYS
            })
        );
        for wrong in ["24:00", "9:5", "09", "nine", "09:60"] {
            assert!(parse(Kind::Daily, wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn schedules_read_as_sentences() {
        assert_eq!(describe(Schedule::Every { minutes: 1 }), "Every minute");
        assert_eq!(
            describe(Schedule::Every { minutes: 90 }),
            "Every 90 minutes"
        );
        assert_eq!(describe(Schedule::Every { minutes: 120 }), "Every 2 hours");
        assert_eq!(
            describe(Schedule::At {
                hour: 9,
                minute: 0,
                days: Days::EVERY
            }),
            "Every day at 09:00"
        );
    }
}
