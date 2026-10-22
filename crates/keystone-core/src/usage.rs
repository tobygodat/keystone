//! What providers report about their accounts' usage limits, as Keystone keeps it.
//! Only windows, percentages and reset times are kept: never an email address,
//! organisation or plan.

use serde::{Deserialize, Serialize};

/// One usage-limit window as a provider reported it, such as Claude's five-hour
/// window or Codex's weekly one.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LimitWindow {
    /// Stable for the provider, so a later report of the same window replaces this one.
    pub id: String,
    pub label: String,
    /// 0-100, and above 100 when usage ran past the limit.
    pub used_percent: f64,
    /// When the window resets, in unix seconds.
    pub resets_at: Option<u64>,
}

/// A window and when Keystone received it, in unix milliseconds.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Reading {
    #[serde(flatten)]
    pub window: LimitWindow,
    pub observed_at: u64,
}

impl Reading {
    /// Whether the window has reset since this reading, so its percentage is history.
    pub fn is_reset(&self, now_ms: u64) -> bool {
        self.window
            .resets_at
            .is_some_and(|at| at.saturating_mul(1000) <= now_ms)
    }
}

/// Why a turn failed when the account's usage limit refused it: which limit and when
/// it resets, as far as the provider said.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct UsageLimit {
    /// The limit's name, such as `5-hour`, when the provider named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// When the limit resets, in unix seconds, when the provider said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
    /// Recognised from a rejected rate-limit report beside a rate-limit error, not from
    /// the provider's own usage-limit error kind.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inferred: bool,
}

/// How long a window that reset stays listed, as "reset", before it is forgotten.
const KEEP_AFTER_RESET_MS: u64 = 24 * 60 * 60_000;

/// One account's latest reading of each window it reported.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct AccountUsage {
    pub readings: Vec<Reading>,
}

impl AccountUsage {
    /// Replaces each reported window's reading and keeps the windows a report left
    /// out, since providers send partial updates.
    pub fn record(&mut self, windows: Vec<LimitWindow>, now_ms: u64) {
        for window in windows {
            let reading = Reading {
                window,
                observed_at: now_ms,
            };
            match self
                .readings
                .iter_mut()
                .find(|r| r.window.id == reading.window.id)
            {
                Some(old) => *old = reading,
                None => self.readings.push(reading),
            }
        }
        self.prune(now_ms);
    }

    /// Forgets windows that reset more than a day ago, such as one a provider
    /// reported once and never again.
    pub fn prune(&mut self, now_ms: u64) {
        self.readings
            .retain(|r| !r.is_reset(now_ms.saturating_sub(KEEP_AFTER_RESET_MS)));
    }

    /// The most used window that has not reset since it was read.
    pub fn peak(&self, now_ms: u64) -> Option<&Reading> {
        self.readings
            .iter()
            .filter(|r| !r.is_reset(now_ms))
            .max_by(|a, b| a.window.used_percent.total_cmp(&b.window.used_percent))
    }

    /// The full window that resets last, among those that have not reset: the
    /// account has no room until then. `None` when no window is full.
    pub fn full_until(&self, now_ms: u64) -> Option<&Reading> {
        self.readings
            .iter()
            .filter(|r| r.window.used_percent >= 100.0 && !r.is_reset(now_ms))
            .max_by_key(|r| r.window.resets_at.unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: &str, used: f64, resets_at: Option<u64>) -> LimitWindow {
        LimitWindow {
            id: id.into(),
            label: id.into(),
            used_percent: used,
            resets_at,
        }
    }

    #[test]
    fn partial_reports_replace_their_windows_and_keep_the_rest() {
        let mut usage = AccountUsage::default();
        usage.record(
            vec![
                window("five_hour", 40.0, None),
                window("seven_day", 10.0, None),
            ],
            1_000,
        );
        usage.record(vec![window("five_hour", 85.0, None)], 2_000);
        assert_eq!(usage.readings.len(), 2);
        assert_eq!(usage.readings[0].window.used_percent, 85.0);
        assert_eq!(usage.readings[0].observed_at, 2_000);
        assert_eq!(usage.readings[1].window.used_percent, 10.0);
        assert_eq!(usage.readings[1].observed_at, 1_000);
    }

    #[test]
    fn the_peak_skips_windows_that_reset_since_they_were_read() {
        let mut usage = AccountUsage::default();
        usage.record(
            vec![
                window("five_hour", 95.0, Some(100)),
                window("seven_day", 30.0, Some(10_000)),
            ],
            0,
        );
        assert_eq!(usage.peak(99_999).unwrap().window.id, "five_hour");
        assert!(usage.readings[0].is_reset(100_000));
        assert_eq!(usage.peak(100_000).unwrap().window.id, "seven_day");
        assert!(usage.peak(10_000_000).is_none());
    }

    #[test]
    fn the_account_is_full_until_its_last_full_window_resets() {
        let mut usage = AccountUsage::default();
        usage.record(
            vec![
                window("five_hour", 100.0, Some(100)),
                window("seven_day", 100.0, Some(5_000)),
                window("seven_day_opus", 99.0, Some(9_000)),
            ],
            0,
        );
        assert_eq!(usage.full_until(0).unwrap().window.id, "seven_day");
        assert_eq!(usage.full_until(4_999_999).unwrap().window.id, "seven_day");
        assert!(usage.full_until(5_000_000).is_none());
        usage.record(vec![window("five_hour", 100.0, None)], 1);
        assert_eq!(
            usage.full_until(5_000_000).unwrap().window.id,
            "five_hour",
            "a full window with no reset time has no room in sight"
        );
    }

    #[test]
    fn a_usage_limit_saves_only_what_the_provider_said() {
        assert_eq!(serde_json::to_string(&UsageLimit::default()).unwrap(), "{}");
        let limit = UsageLimit {
            label: Some("5-hour".into()),
            resets_at: Some(1_800_000_000),
            inferred: true,
        };
        let text = serde_json::to_string(&limit).unwrap();
        assert_eq!(
            text,
            r#"{"label":"5-hour","resets_at":1800000000,"inferred":true}"#
        );
        assert_eq!(serde_json::from_str::<UsageLimit>(&text).unwrap(), limit);
    }

    #[test]
    fn a_window_that_reset_a_day_ago_is_forgotten() {
        let mut usage = AccountUsage::default();
        usage.record(
            vec![
                window("seven_day_opus", 100.0, Some(100)),
                window("seven_day", 30.0, None),
            ],
            0,
        );
        usage.record(vec![window("five_hour", 10.0, None)], 100_000);
        assert_eq!(
            usage.readings.len(),
            3,
            "a reset window stays listed a while"
        );
        usage.record(
            vec![window("five_hour", 12.0, None)],
            100_000 + KEEP_AFTER_RESET_MS,
        );
        let ids: Vec<_> = usage
            .readings
            .iter()
            .map(|r| r.window.id.as_str())
            .collect();
        assert_eq!(ids, ["seven_day", "five_hour"]);
    }
}
