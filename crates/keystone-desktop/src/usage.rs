//! Usage displays: each account's limit windows in Settings → Accounts, and a meter in
//! the composer footer once the session's account or context nears its limit. Only
//! windows, percentages, reset times and the age of each reading are shown; never an
//! email address, organisation or plan.

use crate::Keystone;
use crate::relative_time::{LocalTime, clock, relative};
use gpui::{AnyElement, Context, Hsla, div, prelude::*, px};
use keystone_core::{AccountId, AccountUsage, Provider, Reading};
use keystone_ui::prelude::*;

/// The percentage at or above which the composer footer shows the meter. Toby may
/// change it.
pub(crate) const THRESHOLD: f64 = 80.0;

const BAR_WIDTH: f32 = 36.;

/// A reading older than this shows its age on the meter itself, not only in its
/// tooltip.
const FRESH_MS: u64 = 15 * 60_000;

fn percent(used: f64) -> String {
    format!("{}%", used.round() as i64)
}

/// How long ago a reading was taken: `just now`, `5m ago`, `3h ago`, then a day.
pub(crate) fn age(now: u64, observed: u64) -> String {
    match relative(LocalTime::at(now), LocalTime::at(observed)).as_str() {
        "now" => "just now".into(),
        label if label.ends_with(['m', 'h']) => format!("{label} ago"),
        label => label.to_owned(),
    }
}

fn resets(now: u64, reading: &Reading) -> Option<String> {
    let at = reading.window.resets_at?.saturating_mul(1000);
    Some(clock(LocalTime::at(now), LocalTime::at(at)))
}

/// One window in words: `5-hour 85% · resets 15:40`, or `5-hour reset 14:00` once its
/// reset has passed, since its percentage no longer applies.
pub(crate) fn describe(now: u64, reading: &Reading) -> String {
    let label = &reading.window.label;
    match resets(now, reading) {
        Some(at) if reading.is_reset(now) => format!("{label} reset {at}"),
        Some(at) => format!(
            "{label} {} · resets {at}",
            percent(reading.window.used_percent)
        ),
        None => format!("{label} {}", percent(reading.window.used_percent)),
    }
}

/// Each reading in words, with its own age when that differs from the newest's, and
/// the newest reading's age.
fn describe_all(now: u64, readings: &[Reading]) -> (Vec<String>, Option<String>) {
    let Some(newest) = readings.iter().map(|r| r.observed_at).max() else {
        return (Vec::new(), None);
    };
    let newest = age(now, newest);
    let texts = readings
        .iter()
        .map(|reading| {
            let own = age(now, reading.observed_at);
            let text = describe(now, reading);
            if own == newest {
                text
            } else {
                format!("{text} · {own}")
            }
        })
        .collect();
    (texts, Some(newest))
}

/// The share of the context window the session last filled, when both are known.
fn context_percent(context: Option<(u64, Option<u64>)>) -> Option<f64> {
    let (used, window) = context?;
    let window = window.filter(|w| *w > 0)?;
    Some(used as f64 * 100.0 / window as f64)
}

fn tokens(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..1_000_000 => format!("{}K", count / 1_000),
        _ => format!("{:.1}M", count as f64 / 1_000_000.0),
    }
}

/// What the footer meter shows, if anything: the account's most used window or the
/// context, whichever is higher (the account on a tie), once one reaches [`THRESHOLD`].
/// An account reading older than [`FRESH_MS`] carries its age.
pub(crate) fn meter_reading(
    usage: Option<&AccountUsage>,
    context: Option<(u64, Option<u64>)>,
    now: u64,
) -> Option<(String, f64)> {
    let account = usage.and_then(|usage| usage.peak(now)).map(|reading| {
        let label = &reading.window.label;
        let label = if now.saturating_sub(reading.observed_at) >= FRESH_MS {
            format!("{label} · {}", age(now, reading.observed_at))
        } else {
            label.clone()
        };
        (label, reading.window.used_percent)
    });
    let context = context_percent(context).map(|used| ("Context".to_owned(), used));
    // On a tie the account wins: `max_by` keeps the last of equal readings.
    [context, account]
        .into_iter()
        .flatten()
        .filter(|(_, used)| *used >= THRESHOLD)
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

fn bar(used: f64, colors: &keystone_ui::Colors) -> impl IntoElement + use<> {
    let fill: Hsla = if used >= 100.0 {
        colors.danger
    } else if used >= THRESHOLD {
        colors.warning
    } else {
        colors.text_muted
    };
    div()
        .flex_none()
        .w(px(BAR_WIDTH))
        .h(px(4.))
        .rounded_full()
        .bg(colors.element_disabled)
        .child(
            div()
                .h_full()
                .rounded_full()
                .w(px(BAR_WIDTH * (used.clamp(0.0, 100.0) / 100.0) as f32))
                .bg(fill),
        )
}

impl Keystone {
    /// The footer meter for `session`, shown only at or above [`THRESHOLD`]. It opens
    /// Settings → Accounts.
    pub(crate) fn usage_meter(
        &self,
        session: u64,
        narrow: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.state.read(cx);
        let saved = state.session(session)?;
        let (provider, account) = (saved.provider(), saved.account().cloned());
        let usage = state.usage(provider, account.as_ref());
        let context = state.conversations().get(&session).and_then(|c| c.context);
        let now = crate::now_ms();
        let (label, used) = meter_reading(usage, context, now)?;
        let name = account
            .as_ref()
            .and_then(|id| state.store().workspace().account(id))
            .map_or("Default".to_owned(), |a| a.label().to_owned());
        let mut details = vec![format!("{provider} · {name}")];
        let (windows, newest) = describe_all(now, usage.map_or(&[][..], |u| &u.readings[..]));
        details.extend(windows);
        if let Some(newest) = newest {
            details.push(format!("Updated {newest}"));
        }
        if let (Some(share), Some((count, Some(window)))) = (context_percent(context), context) {
            details.push(format!(
                "Context {} of {} tokens ({})",
                tokens(count),
                tokens(window),
                percent(share)
            ));
        }
        let colors = cx.theme().colors;
        let text = if narrow {
            percent(used)
        } else {
            format!("{label} {}", percent(used))
        };
        Some(
            h_flex()
                .id("usage-meter")
                .automation_id("usage-meter")
                .role(gpui::Role::Button)
                .aria_label(format!(
                    "{label} usage {}. Open Settings, Accounts",
                    percent(used)
                ))
                .track_focus(&self.usage_focus)
                .flex_none()
                .gap(px(6.))
                .px(px(6.))
                .h(px(24.))
                .rounded(px(6.))
                .border_2()
                .border_color(colors.border_focused.opacity(0.))
                .focus(|s| s.border_color(colors.border_focused))
                .cursor_pointer()
                .hover(|s| s.bg(colors.element_hover))
                .child(bar(used, &colors))
                .child(Label::new(text).size(TextSize::Sm).color(if used >= 100.0 {
                    Color::Danger
                } else {
                    Color::Warning
                }))
                .tooltip(Tooltip::text(details.join("\n")).builder())
                .on_click(
                    self.scoped_listener(cx, |this, _, window, cx| this.open_accounts(window, cx)),
                )
                .on_key_down(self.scoped_listener(
                    cx,
                    |this, event: &gpui::KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            cx.stop_propagation();
                            this.open_accounts(window, cx);
                        }
                    },
                ))
                .into_any_element(),
        )
    }

    /// An account's windows for its Settings → Accounts row, with the age of the
    /// newest reading and of any older one.
    pub(crate) fn usage_line(
        &self,
        key: &str,
        provider: Provider,
        account: Option<&AccountId>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let now = crate::now_ms();
        let colors = cx.theme().colors;
        let usage = self.state.read(cx).usage(provider, account);
        let readings = usage.map_or(&[][..], |usage| &usage.readings[..]);
        let (texts, Some(newest_age)) = describe_all(now, readings) else {
            return div().into_any_element();
        };
        h_flex()
            .automation_id(format!("account-usage-{key}"))
            .flex_wrap()
            .gap_x(spacing::MD)
            .gap_y(px(2.))
            .children(readings.iter().zip(texts).map(|(reading, text)| {
                let reset = reading.is_reset(now);
                h_flex()
                    .automation_id(format!("account-usage-{key}-{}", reading.window.id))
                    .gap(px(6.))
                    .when(!reset, |d| {
                        d.child(bar(reading.window.used_percent, &colors))
                    })
                    .child(Label::new(text).size(TextSize::Sm).color(if reset {
                        Color::Faint
                    } else {
                        Color::Muted
                    }))
            }))
            .child(
                Label::new(format!("Updated {newest_age}"))
                    .size(TextSize::Sm)
                    .color(Color::Faint),
            )
            .into_any_element()
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "usage_tests.rs"]
mod ui_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use keystone_core::LimitWindow;

    const NOW: u64 = 1_800_000_000_000;

    fn usage(windows: &[(&str, f64, Option<u64>)]) -> AccountUsage {
        let mut usage = AccountUsage::default();
        usage.record(
            windows
                .iter()
                .map(|(id, used, resets_at)| LimitWindow {
                    id: (*id).into(),
                    label: (*id).into(),
                    used_percent: *used,
                    resets_at: *resets_at,
                })
                .collect(),
            NOW - 120_000,
        );
        usage
    }

    #[test]
    fn the_meter_appears_only_at_the_threshold_and_shows_the_higher_reading() {
        let later = Some(NOW / 1000 + 3_600);
        let low = usage(&[("5-hour", 40.0, later)]);
        assert_eq!(meter_reading(Some(&low), None, NOW), None);
        assert_eq!(
            meter_reading(Some(&low), Some((90_000, Some(200_000))), NOW),
            None
        );
        let high = usage(&[("5-hour", 85.0, later), ("Weekly", 20.0, later)]);
        assert_eq!(
            meter_reading(Some(&high), None, NOW),
            Some(("5-hour".into(), 85.0))
        );
        assert_eq!(
            meter_reading(Some(&high), Some((180_000, Some(200_000))), NOW),
            Some(("Context".into(), 90.0))
        );
        assert_eq!(meter_reading(None, Some((180_000, None)), NOW), None);
        let undated = usage(&[("5-hour", 85.0, None)]);
        let stale = meter_reading(Some(&undated), None, NOW + 3 * 3_600_000 - 120_000).unwrap();
        assert_eq!(stale.0, "5-hour · 3h ago");
        assert_eq!(
            meter_reading(Some(&high), Some((170_000, Some(200_000))), NOW),
            Some(("5-hour".into(), 85.0))
        );
        assert_eq!(
            meter_reading(Some(&high), None, (NOW / 1000 + 3_600) * 1000),
            None
        );
    }

    #[test]
    fn readings_say_how_old_they_are_and_when_they_reset() {
        let reading = &usage(&[("5-hour", 85.4, Some(NOW / 1000 + 600))]).readings[0];
        assert!(describe(NOW, reading).starts_with("5-hour 85% · resets "));
        assert!(describe(NOW + 600_000, reading).starts_with("5-hour reset "));
        assert_eq!(age(NOW, NOW - 30_000), "just now");
        assert_eq!(age(NOW, NOW - 5 * 60_000), "5m ago");
        assert_eq!(age(NOW, NOW - 3 * 3_600_000), "3h ago");
        assert!(!age(NOW, NOW - 3 * 86_400_000).ends_with("ago"));
        let mut mixed = usage(&[("5-hour", 95.0, None)]);
        mixed.record(
            vec![LimitWindow {
                id: "Weekly".into(),
                label: "Weekly".into(),
                used_percent: 20.0,
                resets_at: None,
            }],
            NOW,
        );
        let (texts, newest) = describe_all(NOW, &mixed.readings);
        assert_eq!(texts, ["5-hour 95% · 2m ago", "Weekly 20%"]);
        assert_eq!(newest.as_deref(), Some("just now"));
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(200_000), "200K");
        assert_eq!(tokens(1_500_000), "1.5M");
    }
}
