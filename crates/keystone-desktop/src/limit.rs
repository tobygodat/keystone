//! Usage-limit recovery: the card under a turn an account's usage limit refused. It
//! says which limit and when it resets, and offers to continue after the reset, which
//! the user schedules and can cancel, or on another account through a handoff.
//! Nothing is sent until the user chooses.

use crate::model_selector::{account_label, provider_name};
use crate::relative_time::{LocalTime, clock};
use crate::view_state::ViewKey;
use crate::{Keystone, arrangement, now_ms};
use gpui::{AnyElement, App, Context, div, prelude::*};
use keystone_core::{AccountId, Provider, UsageLimit};
use keystone_ui::prelude::*;

/// How long after the stated reset a scheduled continue goes, so a provider clock a
/// little behind ours has reset too. Toby may change it.
pub(crate) const CONTINUE_GRACE_MS: u64 = 15_000;

/// The longest any limit timer waits before reading the wall clock again.
pub(crate) const CLOCK_CHECK_MS: u64 = 60_000;

/// When a limit resets, in unix milliseconds: as the error says, else as the account's
/// latest usage reading says.
pub(crate) fn reset_at(
    limit: &UsageLimit,
    reading: Option<&keystone_core::Reading>,
) -> Option<u64> {
    limit
        .resets_at
        .or_else(|| reading.and_then(|r| r.window.resets_at))
        .map(|at| at.saturating_mul(1000))
}

/// The account's full window that refused the turn: the one it waits on, else, once
/// every full window has reset, the latest full one, so the card keeps naming it and
/// its reset. A window named `label`, as the error named the limit, comes first.
pub(crate) fn refusing_window<'a>(
    usage: &'a keystone_core::AccountUsage,
    label: Option<&str>,
    now: u64,
) -> Option<&'a keystone_core::Reading> {
    let full = || {
        usage
            .readings
            .iter()
            .filter(|r| r.window.used_percent >= 100.0)
    };
    full()
        .filter(|r| Some(r.window.label.as_str()) == label)
        .max_by_key(|r| r.observed_at)
        .or_else(|| usage.full_until(now))
        .or_else(|| full().max_by_key(|r| r.observed_at))
}

/// The card's one line: the limit, its window, and its reset or the scheduled continue.
fn headline(
    inferred: bool,
    label: Option<&str>,
    scheduled: Option<u64>,
    reset: Option<u64>,
    now: u64,
    at: impl Fn(u64) -> String,
) -> String {
    let mut headline = if inferred {
        "Usage limit likely reached"
    } else {
        "Usage limit reached"
    }
    .to_owned();
    if let Some(label) = label {
        headline.push_str(&format!(" · {label}"));
    }
    headline.push_str(&match (scheduled, reset) {
        (Some(ms), _) => format!(" · continues at {}", at(ms)),
        (None, Some(ms)) if ms <= now => format!(" · reset {}", at(ms)),
        (None, Some(ms)) => format!(" · resets {}", at(ms)),
        (None, None) => String::new(),
    });
    headline
}

impl Keystone {
    /// Where "Continue on another account" goes: another account of the session's
    /// provider, else the other provider's, skipping any whose usage reading shows a
    /// full window. Each provider's default account comes first.
    pub(crate) fn limit_target(
        &self,
        session: u64,
        cx: &App,
    ) -> Option<(Provider, Option<AccountId>, String)> {
        let state = self.state.read(cx);
        let source = state.session(session)?;
        let accounts = state.store().workspace().accounts();
        let now = now_ms();
        let other = match source.provider() {
            Provider::Claude => Provider::Codex,
            Provider::Codex => Provider::Claude,
        };
        [source.provider(), other]
            .into_iter()
            .flat_map(|provider| {
                std::iter::once((provider, None)).chain(
                    accounts
                        .iter()
                        .filter(move |a| a.provider() == provider)
                        .map(move |a| (provider, Some(a.id().clone()))),
                )
            })
            .filter(|(provider, account)| {
                (*provider, account.as_ref()) != (source.provider(), source.account())
            })
            .find(|(provider, account)| {
                state
                    .usage(*provider, account.as_ref())
                    .is_none_or(|usage| usage.full_until(now).is_none())
            })
            .map(|(provider, account)| {
                let name = match account_label(provider, account.as_ref(), accounts) {
                    Some(label) => format!("{} · {label}", provider_name(provider)),
                    None => provider_name(provider).to_owned(),
                };
                (provider, account, name)
            })
    }

    /// Redraws once `at` passes, so the card's Continue reads as it should then.
    fn redraw_at(&self, at: u64, cx: &Context<Self>) {
        if !self.limit_wakes.borrow_mut().insert(at) {
            return;
        }
        // Capped, so a computer that slept still redraws soon after it wakes.
        let delay = std::time::Duration::from_millis(
            (at.saturating_sub(now_ms()) + 50).min(CLOCK_CHECK_MS),
        );
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                this.limit_wakes.borrow_mut().remove(&at);
                cx.notify();
            });
        })
        .detach();
    }

    /// The card under a turn the account's usage limit refused.
    pub(crate) fn limit_card(
        &self,
        key: ViewKey,
        limit: &UsageLimit,
        cx: &Context<Self>,
    ) -> AnyElement {
        let session = key.session;
        let colors = cx.theme().colors;
        let now = now_ms();
        let state = self.state.read(cx);
        let source = state.session(session);
        let scheduled = source.and_then(|s| s.continue_at());
        let reading = source
            .and_then(|s| state.usage(s.provider(), s.account()))
            .and_then(|usage| refusing_window(usage, limit.label.as_deref(), now));
        let reset = reset_at(limit, reading);
        let at = |ms: u64| clock(LocalTime::at(now), LocalTime::at(ms));
        let label = limit
            .label
            .clone()
            .or_else(|| reading.map(|r| r.window.label.clone()));
        let headline = headline(limit.inferred, label.as_deref(), scheduled, reset, now, at);
        // Before a known reset, Continue waits for it; after it, or with no reset known,
        // it sends now.
        let waits = reset.filter(|ms| *ms > now);
        if let Some(ms) = waits {
            self.redraw_at(ms, cx);
        }
        let target = self.limit_target(session, cx);
        let button = |id: &str, text: String, primary: bool| {
            let selector = arrangement::selector(id, key.view);
            Button::new(selector.clone(), text)
                .style(if primary {
                    ButtonStyle::Primary
                } else {
                    ButtonStyle::Subtle
                })
                .debug_selector(move || selector.to_string())
        };
        let mut buttons = h_flex().gap_2().flex_wrap();
        buttons = match (scheduled, waits) {
            (Some(_), _) => buttons.child(
                button("limit-cancel", "Cancel continue".into(), false).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.state
                            .update(cx, |state, cx| state.schedule_continue(session, None, cx))
                    },
                )),
            ),
            (None, Some(ms)) => buttons.child(
                button("limit-schedule", "Continue after reset".into(), true).on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.state.update(cx, |state, cx| {
                            state.schedule_continue(session, Some(ms + CONTINUE_GRACE_MS), cx)
                        })
                    }),
                ),
            ),
            (None, None) => {
                buttons.child(button("limit-continue", "Continue".into(), true).on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.state
                            .update(cx, |state, cx| state.continue_after_limit(session, cx));
                    }),
                ))
            }
        };
        if let Some((provider, account, name)) = target {
            buttons = buttons.child(
                button("limit-switch", format!("Continue on {name}"), false).on_click(cx.listener(
                    move |this, _, window, cx| {
                        this.select(session, cx);
                        this.hand_off_to(session, provider, Some(account.clone()), window, cx);
                    },
                )),
            );
        }
        v_flex()
            .id(arrangement::selector("limit", key.view))
            .automation_id(arrangement::selector("limit", key.view))
            .surface(Elevation::Raised, cx)
            .border_color(colors.border_strong)
            .p_3()
            .gap_2()
            .child(
                div()
                    .automation_id(arrangement::selector("limit-headline", key.view))
                    .child(
                        Label::new(headline)
                            .size(TextSize::Ui)
                            .color(Color::Warning),
                    ),
            )
            .child(buttons)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keystone_core::{LimitWindow, Reading};

    #[test]
    fn the_headline_says_the_limit_once_with_its_reset_or_scheduled_continue() {
        let at = |ms: u64| format!("t{ms}");
        let line =
            |inferred, label, scheduled, reset| headline(inferred, label, scheduled, reset, 10, at);
        assert_eq!(
            line(false, Some("5-hour"), None, Some(20)),
            "Usage limit reached · 5-hour · resets t20"
        );
        assert_eq!(
            line(false, Some("5-hour"), Some(35), Some(20)),
            "Usage limit reached · 5-hour · continues at t35"
        );
        assert_eq!(
            line(true, None, None, Some(5)),
            "Usage limit likely reached · reset t5"
        );
        assert_eq!(line(false, None, None, None), "Usage limit reached");
    }

    #[test]
    fn a_codex_limit_keeps_its_window_and_reset_once_it_passes() {
        let mut usage = keystone_core::AccountUsage::default();
        let window = |used: f64| LimitWindow {
            id: "codex/primary".into(),
            label: "5-hour".into(),
            used_percent: used,
            resets_at: Some(2_000),
        };
        usage.record(vec![window(100.0)], 0);
        let before = refusing_window(&usage, None, 1_999_000).unwrap();
        assert_eq!(before.window.label, "5-hour");
        let after = refusing_window(&usage, None, 2_000_000).unwrap();
        assert_eq!(
            reset_at(&UsageLimit::default(), Some(after)),
            Some(2_000_000),
            "the reset reads as passed, not as unreported"
        );
        usage.record(vec![window(10.0)], 2_100_000);
        assert!(refusing_window(&usage, None, 2_100_000).is_none());
        usage.record(
            vec![
                window(100.0),
                LimitWindow {
                    id: "seven_day".into(),
                    label: "Weekly".into(),
                    used_percent: 100.0,
                    resets_at: Some(9_000),
                },
            ],
            2_200_000,
        );
        assert_eq!(
            refusing_window(&usage, Some("5-hour"), 2_200_000)
                .unwrap()
                .window
                .label,
            "5-hour",
            "the window the error named, not the one that resets last"
        );
    }

    #[test]
    fn the_errors_reset_wins_over_the_usage_reading() {
        let reading = Reading {
            window: LimitWindow {
                id: "codex/primary".into(),
                label: "5-hour".into(),
                used_percent: 100.0,
                resets_at: Some(2_000),
            },
            observed_at: 0,
        };
        let named = UsageLimit {
            resets_at: Some(1_000),
            ..Default::default()
        };
        assert_eq!(reset_at(&named, Some(&reading)), Some(1_000_000));
        let unnamed = UsageLimit::default();
        assert_eq!(reset_at(&unnamed, Some(&reading)), Some(2_000_000));
        assert_eq!(reset_at(&unnamed, None), None);
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "limit_tests.rs"]
mod ui_tests;
