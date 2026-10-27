//! What the user's `gh` last reported about a session's pull request, and which of its
//! failing checks and reviews the user has looked at since. Neither changes attention:
//! a pull request never makes a session need action.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestState {
    Open,
    Draft,
    Merged,
    Closed,
}

impl PullRequestState {
    /// Whether the pull request can still change, so it is worth asking gh again.
    pub fn is_open(self) -> bool {
        matches!(self, Self::Open | Self::Draft)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

/// Whether GitHub would merge the pull request now, as `mergeStateStatus` says.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeState {
    Clean,
    /// Mergeable, with checks that are not required failing.
    Unstable,
    HasHooks,
    /// The base branch's rules hold it back: required checks or reviews.
    Blocked,
    Behind,
    /// Conflicts with the base.
    Dirty,
    Draft,
    #[default]
    Unknown,
}

impl MergeState {
    /// Whether GitHub merges it for anyone with write access, without bypassing rules.
    pub fn can_merge(self) -> bool {
        matches!(self, Self::Clean | Self::Unstable | Self::HasHooks)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    /// Queued, waiting or running.
    Pending,
    Passed,
    Failed,
    /// Skipped, neutral, cancelled or stale: finished without passing or failing.
    Neutral,
}

/// One check run or commit status on the pull request's head.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Check {
    pub name: String,
    /// The Actions workflow the check ran in, empty for other checks and statuses.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workflow: String,
    pub state: CheckState,
    /// GitHub's own word for how it stands, such as "Timed out" or "Queued".
    pub summary: String,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<u64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Review {
    pub id: String,
    pub author: String,
    pub state: ReviewState,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
}

/// A comment on a line of the pull request's diff.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReviewComment {
    pub id: u64,
    pub author: String,
    /// Relative to the repository's top folder.
    pub path: String,
    /// The line in the head's version of the file; `None` once the line is outdated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PullRequestReport {
    pub title: String,
    /// Who gh is signed in as, whose own reviews and comments are no news to them.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub viewer: String,
    pub state: PullRequestState,
    pub base: String,
    pub head: String,
    /// The head commit, which a merge must still match.
    pub head_oid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<ReviewDecision>,
    #[serde(default)]
    pub merge_state: MergeState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Check>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviews: Vec<Review>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<ReviewComment>,
}

/// What the user is told about until they look: a check that failed, or a review or
/// review comment that arrived.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PullRequestAlert {
    /// A run of a check that failed; a rerun that fails again is another. Each holds a
    /// key unique to its run, review or comment.
    CheckFailed(String),
    Review(String),
}

impl PullRequestReport {
    pub fn alerts(&self) -> BTreeSet<PullRequestAlert> {
        let checks = self
            .checks
            .iter()
            .filter(|check| check.state == CheckState::Failed)
            .map(|check| {
                PullRequestAlert::CheckFailed(format!(
                    "check {}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
                    self.head_oid,
                    check.workflow,
                    check.name,
                    check.started_at.unwrap_or_default(),
                    check.completed_at.unwrap_or_default()
                ))
            });
        let others = |author: &str| author != self.viewer;
        let reviews = self
            .reviews
            .iter()
            .filter(|review| others(&review.author))
            // A reply in a review thread arrives as an empty review around its comment.
            .filter(|review| review.state != ReviewState::Commented || !review.body.is_empty())
            .map(|review| PullRequestAlert::Review(format!("review {}", review.id)));
        let comments = self
            .comments
            .iter()
            .filter(|comment| others(&comment.author))
            .map(|comment| PullRequestAlert::Review(format!("comment {}", comment.id)));
        checks.chain(reviews).chain(comments).collect()
    }
}

impl PullRequestAlert {
    fn key(&self) -> &str {
        match self {
            Self::CheckFailed(key) | Self::Review(key) => key,
        }
    }
}

/// A session's last report, and the alerts in it the user has seen.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PullRequestWatch {
    pub report: PullRequestReport,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    seen: BTreeSet<String>,
}

impl PullRequestWatch {
    /// The report's alerts the user has not seen, none once it is merged or closed.
    pub fn unseen(&self) -> BTreeSet<PullRequestAlert> {
        if !self.report.state.is_open() {
            return BTreeSet::new();
        }
        self.report
            .alerts()
            .into_iter()
            .filter(|alert| !self.seen.contains(alert.key()))
            .collect()
    }

    /// Takes a newer report, keeping what was seen of alerts still in it.
    pub(crate) fn replace(&mut self, report: PullRequestReport) {
        let alerts = report.alerts();
        self.seen
            .retain(|key| alerts.iter().any(|alert| alert.key() == key));
        self.report = report;
    }

    pub(crate) fn see(&mut self) {
        self.seen = self
            .report
            .alerts()
            .iter()
            .map(|alert| alert.key().to_owned())
            .collect();
    }

    /// The first report, whose alerts happened before Keystone watched, so they count
    /// as seen.
    pub(crate) fn new(report: PullRequestReport) -> Self {
        let mut watch = Self {
            report,
            seen: BTreeSet::new(),
        };
        watch.see();
        watch
    }
}
