//! The user's own GitHub CLI, `gh`, signed in with the user's own login. Keystone never
//! sees or stores a GitHub token. Every call blocks, so callers run them on a
//! [`crate::worker::Worker`].

use crate::git::{self, Output};
use keystone_core::{
    Check, CheckState, MergeState, PullRequest, PullRequestReport, PullRequestState, Review,
    ReviewComment, ReviewDecision, ReviewState,
};
use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

/// gh's exit code when a command needs a login it does not have.
const AUTH_REQUIRED: i32 = 4;
/// What to run in a terminal to sign in, and what to say before it.
pub const SIGN_IN: &str = "gh auth login";
pub const INSTALL_HINT: &str = "Install gh (cli.github.com), then run:";
pub const SIGN_IN_HINT: &str = "Sign in to gh:";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// No `gh` on `PATH` or in a common install folder.
    Missing,
    /// gh has no working login. `output` is gh's own.
    SignedOut { output: String },
    /// The branch already has a pull request, which gh named.
    Exists(PullRequest),
    /// gh ran and failed, or could not start. `output` is gh's own.
    Failed { output: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("GitHub CLI (gh) is not installed."),
            Self::SignedOut { .. } => f.write_str("GitHub CLI is not signed in."),
            Self::Exists(pull_request) => write!(
                f,
                "This branch already has pull request #{}.",
                pull_request.number
            ),
            Self::Failed { output } => f.write_str(output.trim_end()),
        }
    }
}

impl std::error::Error for Error {}

/// What `gh pr create` is asked for. An empty `base` leaves the choice to gh, which
/// uses the repository's default branch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    pub title: String,
    pub body: String,
    pub base: String,
    pub draft: bool,
}

/// `gh` in `root`, with no prompts, no terminal and no update check. `GH_REPO` is
/// dropped, so gh works on the repository in `root` rather than one named elsewhere.
fn gh(path: &Path, root: &Path) -> Command {
    let mut command = git::program(path, root);
    command
        .env_remove("GH_REPO")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_SPINNER_DISABLED", "1");
    command
}

/// Where gh is, or [`Error::Missing`]. Whether it is signed in shows when it runs:
/// gh exits 4 when the repository's host needs a login.
pub fn locate() -> Result<PathBuf, Error> {
    crate::find::program("gh").ok_or(Error::Missing)
}

fn failed(error: git::Error) -> Error {
    match error {
        git::Error::Spawn(error) => Error::Failed {
            output: format!("Couldn't run gh: {error}"),
        },
        git::Error::Failed { output, code, .. } if code == Some(AUTH_REQUIRED) => {
            Error::SignedOut { output }
        }
        git::Error::Failed { output, .. } => Error::Failed { output },
        error @ git::Error::Cancelled => Error::Failed {
            output: error.to_string(),
        },
    }
}

/// `gh pr create` for the branch checked out in `root`, which must already be pushed.
/// The body goes on stdin, so its length is not limited by the argument list.
pub fn create(root: &Path, request: &Request) -> Result<PullRequest, Error> {
    create_with(&locate()?, root, request)
}

fn create_with(program: &Path, root: &Path, request: &Request) -> Result<PullRequest, Error> {
    let mut command = gh(program, root);
    command.args([
        "pr",
        "create",
        "--title",
        &request.title,
        "--body-file",
        "-",
    ]);
    if !request.base.trim().is_empty() {
        command.args(["--base", request.base.trim()]);
    }
    if request.draft {
        command.arg("--draft");
    }
    match git::run(
        command,
        Some(request.body.as_bytes().to_vec()),
        Output::Separate,
    ) {
        Ok(output) => {
            let output = String::from_utf8_lossy(&output);
            pull_request(&output).ok_or_else(|| Error::Failed {
                output: format!("gh did not print the new pull request's link.\n{output}"),
            })
        }
        Err(git::Error::Failed { output, .. })
            if output.contains("already exists") && pull_request(&output).is_some() =>
        {
            Err(Error::Exists(pull_request(&output).unwrap()))
        }
        Err(error) => Err(failed(error)),
    }
}

/// The last pull request link in `output`, such as gh prints on success and when one
/// already exists.
fn pull_request(output: &str) -> Option<PullRequest> {
    output
        .split_whitespace()
        .rev()
        .filter(|word| word.starts_with("https://") || word.starts_with("http://"))
        .find_map(|url| {
            let url = url.trim_end_matches(['.', ',', ')']);
            let (_, number) = url.rsplit_once("/pull/")?;
            Some(PullRequest {
                number: number.parse().ok()?,
                url: url.to_owned(),
            })
        })
}

/// How long gh may take to answer before Keystone gives up on it.
const ANSWER: Duration = Duration::from_secs(60);

/// Runs `program`, gh as [`locate`] found it, with `args` in `root`, and returns its
/// stdout.
fn run(program: &Path, root: &Path, args: &[&str]) -> Result<Vec<u8>, Error> {
    let mut command = gh(program, root);
    command.args(args);
    git::run_until(
        command,
        None,
        Output::Separate,
        git::Until::Deadline(ANSWER),
    )
    .map_err(failed)
}

fn parse<T: for<'de> Deserialize<'de>>(output: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(output).map_err(|error| Error::Failed {
        output: format!("Couldn't read gh's answer: {error}"),
    })
}

/// The host and `owner/name` of the repository a pull request link points into.
fn repository(pull_request: &PullRequest) -> Result<(&str, &str), Error> {
    pull_request
        .url
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/'))
        .and_then(|(host, path)| Some((host, path.rsplit_once("/pull/")?.0)))
        .filter(|(_, repository)| repository.split('/').count() == 2)
        .ok_or_else(|| Error::Failed {
            output: format!("{} is not a pull request link.", pull_request.url),
        })
}

/// The open pull request for the branch checked out in `root`, if it has one.
pub fn find(program: &Path, root: &Path) -> Result<Option<PullRequest>, Error> {
    #[derive(Deserialize)]
    struct Found {
        number: u64,
        url: String,
        state: String,
    }
    match run(program, root, &["pr", "view", "--json", "number,url,state"]) {
        Ok(output) => {
            let found: Found = parse(&output)?;
            Ok((found.state == "OPEN").then_some(PullRequest {
                number: found.number,
                url: found.url,
            }))
        }
        // No pull request, no GitHub remote, or no branch checked out.
        Err(Error::Failed { output })
            if [
                "no pull requests found",
                "none of the git remotes",
                "no git remotes found",
                "not a git repository",
                "could not determine current branch",
            ]
            .iter()
            .any(|none| output.contains(none)) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

const VIEW_FIELDS: &str = "title,state,isDraft,baseRefName,headRefName,headRefOid,reviewDecision,mergeStateStatus,statusCheckRollup,reviews";

/// What GitHub says about `pull_request` now: `gh pr view` for its state, checks and
/// reviews, and the REST API for its line comments, which `gh pr view` leaves out.
pub fn view(
    program: &Path,
    root: &Path,
    pull_request: &PullRequest,
) -> Result<PullRequestReport, Error> {
    let (host, repository) = repository(pull_request)?;
    let view = run(
        program,
        root,
        &["pr", "view", &pull_request.url, "--json", VIEW_FIELDS],
    )
    .inspect_err(|error| {
        // Signed out, the next login may be someone else.
        if matches!(error, Error::SignedOut { .. }) {
            VIEWERS.lock().unwrap().clear();
        }
    })?;
    let comments = run(
        program,
        root,
        &[
            "api",
            "--hostname",
            host,
            "--paginate",
            &format!(
                "repos/{repository}/pulls/{}/comments?per_page=100",
                pull_request.number
            ),
        ],
    )?;
    let mut report = report(&view, &comments)?;
    report.viewer = viewer(program, root, host)?;
    Ok(report)
}

/// Who each gh is signed in as on each host, as [`viewer`] found.
static VIEWERS: Mutex<Vec<(PathBuf, String, String)>> = Mutex::new(Vec::new());

/// Who `program` is signed in as on `host`, asked once per gh and host until gh
/// reports a missing login.
fn viewer(program: &Path, root: &Path, host: &str) -> Result<String, Error> {
    let known = |viewers: &[(PathBuf, String, String)]| {
        viewers
            .iter()
            .find(|(p, h, _)| p == program && h == host)
            .map(|(_, _, login)| login.clone())
    };
    if let Some(login) = known(&VIEWERS.lock().unwrap()) {
        return Ok(login);
    }
    let output = run(
        program,
        root,
        &["api", "--hostname", host, "user", "--jq", ".login"],
    )?;
    let login = String::from_utf8_lossy(&output).trim().to_owned();
    VIEWERS
        .lock()
        .unwrap()
        .push((program.to_owned(), host.to_owned(), login.clone()));
    Ok(login)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct View {
    title: String,
    state: String,
    #[serde(default)]
    is_draft: bool,
    base_ref_name: String,
    head_ref_name: String,
    head_ref_oid: String,
    #[serde(default)]
    review_decision: Option<String>,
    #[serde(default)]
    merge_state_status: Option<String>,
    #[serde(default)]
    status_check_rollup: Option<Vec<Rollup>>,
    #[serde(default)]
    reviews: Vec<GhReview>,
}

/// A check run or a commit status, as `statusCheckRollup` lists them.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rollup {
    #[serde(rename = "__typename", default)]
    typename: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    workflow_name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    conclusion: String,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    completed_at: Option<String>,
    #[serde(default)]
    details_url: String,
    #[serde(default)]
    context: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    target_url: String,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
struct GhReview {
    id: String,
    #[serde(default)]
    author: Option<Login>,
    state: String,
    #[serde(default)]
    body: String,
}

#[derive(Deserialize)]
struct GhComment {
    id: u64,
    #[serde(default)]
    user: Option<Login>,
    path: String,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    body: String,
}

/// GitHub's word for a state, such as `TIMED_OUT`, as a person writes it: "Timed out".
fn word(token: &str) -> String {
    let lower = token.replace('_', " ").to_lowercase();
    let mut chars = lower.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Unix seconds from an RFC 3339 UTC time such as gh prints. gh writes an unset time
/// as year 1, which is `None`.
fn seconds(time: Option<&str>) -> Option<u64> {
    let time = time?;
    let number = |range: std::ops::Range<usize>| time.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    // Days since 1970-01-01 from a civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second).ok()
}

fn check(item: Rollup) -> Check {
    if item.typename == "StatusContext" {
        let state = match item.state.as_str() {
            "SUCCESS" => CheckState::Passed,
            "FAILURE" | "ERROR" => CheckState::Failed,
            _ => CheckState::Pending,
        };
        return Check {
            name: item.context,
            workflow: String::new(),
            state,
            summary: word(&item.state),
            started_at: seconds(item.started_at.as_deref()),
            completed_at: None,
            url: item.target_url,
        };
    }
    let (state, summary) = if item.status != "COMPLETED" {
        (CheckState::Pending, &item.status)
    } else {
        let state = match item.conclusion.as_str() {
            "SUCCESS" => CheckState::Passed,
            "FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED" => CheckState::Failed,
            _ => CheckState::Neutral,
        };
        (state, &item.conclusion)
    };
    Check {
        name: item.name,
        workflow: item.workflow_name,
        state,
        summary: word(summary),
        started_at: seconds(item.started_at.as_deref()),
        completed_at: seconds(item.completed_at.as_deref()),
        url: item.details_url,
    }
}

/// The report from `gh pr view`'s JSON and the comments API's pages, which
/// `--paginate` prints one JSON array after another.
fn report(view: &[u8], comments: &[u8]) -> Result<PullRequestReport, Error> {
    let view: View = parse(view)?;
    let comments = serde_json::Deserializer::from_slice(comments)
        .into_iter::<Vec<GhComment>>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| Error::Failed {
            output: format!("Couldn't read gh's answer: {error}"),
        })?;
    let login = |login: Option<Login>| login.map_or_else(|| "ghost".into(), |l| l.login);
    Ok(PullRequestReport {
        title: view.title,
        viewer: String::new(),
        state: match view.state.as_str() {
            "MERGED" => PullRequestState::Merged,
            "CLOSED" => PullRequestState::Closed,
            _ if view.is_draft => PullRequestState::Draft,
            _ => PullRequestState::Open,
        },
        base: view.base_ref_name,
        head: view.head_ref_name,
        head_oid: view.head_ref_oid,
        review_decision: match view.review_decision.as_deref() {
            Some("APPROVED") => Some(ReviewDecision::Approved),
            Some("CHANGES_REQUESTED") => Some(ReviewDecision::ChangesRequested),
            Some("REVIEW_REQUIRED") => Some(ReviewDecision::ReviewRequired),
            _ => None,
        },
        merge_state: match view.merge_state_status.as_deref() {
            Some("CLEAN") => MergeState::Clean,
            Some("UNSTABLE") => MergeState::Unstable,
            Some("HAS_HOOKS") => MergeState::HasHooks,
            Some("BLOCKED") => MergeState::Blocked,
            Some("BEHIND") => MergeState::Behind,
            Some("DIRTY") => MergeState::Dirty,
            Some("DRAFT") => MergeState::Draft,
            _ => MergeState::Unknown,
        },
        checks: view
            .status_check_rollup
            .unwrap_or_default()
            .into_iter()
            .map(check)
            .collect(),
        reviews: view
            .reviews
            .into_iter()
            .filter_map(|review| {
                let state = match review.state.as_str() {
                    "APPROVED" => ReviewState::Approved,
                    "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
                    "COMMENTED" => ReviewState::Commented,
                    "DISMISSED" => ReviewState::Dismissed,
                    _ => return None,
                };
                Some(Review {
                    id: review.id,
                    author: login(review.author),
                    state,
                    body: review.body,
                })
            })
            .collect(),
        comments: comments
            .into_iter()
            .flatten()
            .map(|comment| ReviewComment {
                id: comment.id,
                author: login(comment.user),
                path: comment.path,
                line: comment.line,
                body: comment.body,
            })
            .collect(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMethod {
    Squash,
    Merge,
    Rebase,
}

impl MergeMethod {
    pub fn label(self) -> &'static str {
        match self {
            Self::Squash => "Squash and merge",
            Self::Merge => "Merge commit",
            Self::Rebase => "Rebase and merge",
        }
    }

    fn api(self) -> &'static str {
        match self {
            Self::Squash => "squash",
            Self::Merge => "merge",
            Self::Rebase => "rebase",
        }
    }
}

/// The merge methods `pull_request`'s repository allows, squash first.
pub fn merge_methods(
    program: &Path,
    root: &Path,
    pull_request: &PullRequest,
) -> Result<Vec<MergeMethod>, Error> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Allowed {
        squash_merge_allowed: bool,
        merge_commit_allowed: bool,
        rebase_merge_allowed: bool,
    }
    let (host, repository) = repository(pull_request)?;
    let output = run(
        program,
        root,
        &[
            "repo",
            "view",
            &format!("{host}/{repository}"),
            "--json",
            "squashMergeAllowed,mergeCommitAllowed,rebaseMergeAllowed",
        ],
    )?;
    let allowed: Allowed = parse(&output)?;
    Ok([
        (MergeMethod::Squash, allowed.squash_merge_allowed),
        (MergeMethod::Merge, allowed.merge_commit_allowed),
        (MergeMethod::Rebase, allowed.rebase_merge_allowed),
    ]
    .into_iter()
    .filter_map(|(method, allowed)| allowed.then_some(method))
    .collect())
}

/// Merges `pull_request` now with `method`, if its head is still `head`. It calls
/// GitHub's merge endpoint directly rather than `gh pr merge`, which joins a merge queue
/// and so merges later on its own where the base branch requires one. The endpoint
/// merges at once or refuses, and gh prints why. Unlike `gh pr merge` it does not
/// check the base branch's rules first, so callers merge only a report whose
/// [`MergeState::can_merge`] holds.
pub fn merge(
    program: &Path,
    root: &Path,
    pull_request: &PullRequest,
    method: MergeMethod,
    head: &str,
) -> Result<(), Error> {
    let (host, repository) = repository(pull_request)?;
    run(
        program,
        root,
        &[
            "api",
            "--hostname",
            host,
            "--method",
            "PUT",
            &format!("repos/{repository}/pulls/{}/merge", pull_request.number),
            "-f",
            &format!("merge_method={}", method.api()),
            "-f",
            &format!("sha={head}"),
        ],
    )
    .map(drop)
}

/// The Actions job a check ran as, named by the check's details link
/// (`https://<host>/<owner>/<repo>/actions/runs/<run>/job/<job>`). Other checks and
/// commit statuses have no job, and so no log gh is asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    host: String,
    repository: String,
    id: u64,
}

impl Job {
    /// The job `url` names, when it is in `pull_request`'s own repository, so gh never
    /// sends its login to a host a check's link names.
    pub fn of(url: &str, pull_request: &PullRequest) -> Option<Self> {
        let (_, rest) = url.split_once("://")?;
        let (host, path) = rest.split_once('/')?;
        let (repository, job) = path.split_once("/actions/runs/")?;
        let (_, id) = job.split_once("/job/")?;
        let id = id.split(['/', '?', '#']).next()?.parse().ok()?;
        let (own_host, own_repository) = self::repository(pull_request).ok()?;
        (host.eq_ignore_ascii_case(own_host) && repository.eq_ignore_ascii_case(own_repository))
            .then(|| Self {
                host: host.to_owned(),
                repository: repository.to_owned(),
                id,
            })
    }
}

/// The log of `job`'s failed steps, as `gh run view --log-failed` prints it, without
/// the terminal colour codes many workflows write.
pub fn failed_log(program: &Path, root: &Path, job: &Job) -> Result<String, Error> {
    let output = run(
        program,
        root,
        &[
            "run",
            "view",
            "--job",
            &job.id.to_string(),
            "--log-failed",
            "--repo",
            &format!("{}/{}", job.host, job.repository),
        ],
    )?;
    let mut log = String::new();
    let output = String::from_utf8_lossy(&output);
    let mut rest: &str = &output;
    while let Some(start) = rest.find("\x1b[") {
        log.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find(|c: char| ('@'..='~').contains(&c))
            .map_or(after.len(), |end| end + 1);
        rest = &after[end..];
    }
    log.push_str(rest);
    Ok(log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_link_gh_prints() {
        assert_eq!(
            pull_request("Warning: 1 uncommitted change\nhttps://github.com/o/r/pull/12\n"),
            Some(PullRequest {
                number: 12,
                url: "https://github.com/o/r/pull/12".into()
            })
        );
        assert_eq!(
            pull_request(
                "a pull request for branch \"feature\" into branch \"main\" already exists:\nhttps://github.example.com/o/r/pull/7\n"
            )
            .map(|pr| pr.number),
            Some(7)
        );
        assert_eq!(
            pull_request("http://git.example.internal/o/r/pull/5").map(|pr| pr.number),
            Some(5)
        );
        assert_eq!(pull_request("https://github.com/o/r/issues/3"), None);
        assert_eq!(pull_request(""), None);
    }

    /// A stand-in `gh` that records its arguments, one per line, and runs `script`.
    fn fake(dir: &Path, script: &str) -> PathBuf {
        let path = dir.join("gh");
        keystone_core::test_support::write_executable(
            &path,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done >> \"{0}/argv\"\n[ \"$1\" = pr ] && cat >> \"{0}/argv\"\necho --- >> \"{0}/argv\"\n{script}",
                dir.display()
            ),
        );
        path
    }

    fn argv(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("argv")).unwrap()
    }

    #[test]
    fn creates_with_title_body_base_and_draft_and_reads_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake(
            dir.path(),
            "[ \"$GH_PROMPT_DISABLED\" = 1 ] || exit 9\necho 'Warning: 1 uncommitted change' >&2\necho https://github.com/o/r/pull/41\n",
        );
        let request = Request {
            title: "Push from Keystone".into(),
            body: "Line one\n\n- two".into(),
            base: " main ".into(),
            draft: true,
        };
        let created = create_with(&gh, dir.path(), &request).unwrap();
        assert_eq!(created.number, 41);
        assert_eq!(created.url, "https://github.com/o/r/pull/41");
        assert_eq!(
            argv(dir.path()),
            "pr\ncreate\n--title\nPush from Keystone\n--body-file\n-\n--base\nmain\n--draft\nLine one\n\n- two---\n"
        );

        let plain = Request {
            title: "-starts with a dash".into(),
            ..Request::default()
        };
        create_with(&gh, dir.path(), &plain).unwrap();
        assert!(
            argv(dir.path())
                .ends_with("---\npr\ncreate\n--title\n-starts with a dash\n--body-file\n-\n---\n"),
            "no base or draft unless asked, and an empty body is still given: {}",
            argv(dir.path())
        );
    }

    #[test]
    fn reports_a_missing_login_an_existing_pull_request_and_other_failures() {
        let dir = tempfile::tempdir().unwrap();
        let signed_out = fake(
            dir.path(),
            "echo 'To get started with GitHub CLI, please run:  gh auth login' >&2\nexit 4\n",
        );
        assert_eq!(
            create_with(&signed_out, dir.path(), &Request::default()),
            Err(Error::SignedOut {
                output: "To get started with GitHub CLI, please run:  gh auth login\n".into()
            })
        );

        let exists = fake(
            dir.path(),
            "echo 'a pull request for branch \"f\" into branch \"main\" already exists:' >&2\necho https://github.com/o/r/pull/7 >&2\nexit 1\n",
        );
        assert_eq!(
            create_with(&exists, dir.path(), &Request::default()),
            Err(Error::Exists(PullRequest {
                number: 7,
                url: "https://github.com/o/r/pull/7".into()
            }))
        );

        let broken = fake(
            dir.path(),
            "echo \"GraphQL: Head sha can't be blank\" >&2\nexit 1\n",
        );
        assert_eq!(
            create_with(&broken, dir.path(), &Request::default()),
            Err(Error::Failed {
                output: "GraphQL: Head sha can't be blank\n".into()
            })
        );
    }

    #[test]
    fn finds_the_actions_job_a_check_ran_as_only_in_its_own_repository() {
        let pull = |url: &str| PullRequest {
            number: 4,
            url: url.into(),
        };
        let github = pull("https://github.com/o/r/pull/4");
        assert_eq!(
            Job::of("https://github.com/o/r/actions/runs/11/job/22", &github),
            Some(Job {
                host: "github.com".into(),
                repository: "o/r".into(),
                id: 22
            })
        );
        assert_eq!(
            Job::of(
                "https://ghe.example/o/r/actions/runs/11/job/22?pr=4",
                &pull("https://ghe.example/o/r/pull/4")
            )
            .map(|j| j.id),
            Some(22)
        );
        for other in [
            "",
            "https://github.com/o/r/runs/1",
            "https://github.com/o/r/actions/runs/11",
            "https://ci.example/1",
            "https://github.com/o/r/actions/runs/11/job/latest",
            "https://attacker.example/o/r/actions/runs/11/job/22",
            "https://github.com/someone/else/actions/runs/11/job/22",
        ] {
            assert_eq!(Job::of(other, &github), None, "{other}");
        }
    }

    #[test]
    fn reads_a_failed_job_log_from_its_repository() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake(
            dir.path(),
            "printf 'test\\tRun cargo test\\t\\033[1m\\033[31merror\\033[0m\\n'\n",
        );
        let job = Job::of(
            "https://ghe.example/o/r/actions/runs/11/job/22",
            &PullRequest {
                number: 4,
                url: "https://ghe.example/o/r/pull/4".into(),
            },
        )
        .unwrap();
        assert_eq!(
            failed_log(&gh, dir.path(), &job).unwrap(),
            "test\tRun cargo test\terror\n"
        );
        assert_eq!(
            argv(dir.path()),
            "run\nview\n--job\n22\n--log-failed\n--repo\nghe.example/o/r\n---\n"
        );
    }

    #[test]
    fn reads_times_as_gh_prints_them() {
        assert_eq!(seconds(Some("1970-01-01T00:00:00Z")), Some(0));
        assert_eq!(seconds(Some("2026-10-25T08:00:30Z")), Some(1_792_915_230));
        assert_eq!(seconds(Some("2000-03-01T00:00:00Z")), Some(951_868_800));
        assert_eq!(
            seconds(Some("0001-01-01T00:00:00Z")),
            None,
            "gh's unset time"
        );
        assert_eq!(seconds(Some("soon")), None);
        assert_eq!(seconds(None), None);
    }

    const VIEW: &str = r#"{"title":"Fix the build","state":"OPEN","isDraft":false,"baseRefName":"main","headRefName":"fix","headRefOid":"abc123","reviewDecision":"CHANGES_REQUESTED","mergeStateStatus":"BLOCKED","statusCheckRollup":[
        {"__typename":"CheckRun","name":"test","workflowName":"CI","status":"COMPLETED","conclusion":"TIMED_OUT","startedAt":"2026-10-25T08:00:00Z","completedAt":"2026-10-25T08:01:12Z","detailsUrl":"https://github.com/o/r/runs/1"},
        {"__typename":"CheckRun","name":"lint","workflowName":"CI","status":"IN_PROGRESS","conclusion":"","startedAt":"2026-10-25T08:00:00Z","completedAt":"0001-01-01T00:00:00Z","detailsUrl":""},
        {"__typename":"CheckRun","name":"docs","workflowName":"","status":"COMPLETED","conclusion":"CANCELLED","startedAt":"2026-10-25T08:00:00Z","completedAt":"2026-10-25T08:00:05Z","detailsUrl":""},
        {"__typename":"StatusContext","context":"ci/legacy","state":"ERROR","targetUrl":"https://ci.example/1","startedAt":"2026-10-25T08:00:00Z"}],
        "reviews":[{"id":"PRR_1","author":{"login":"ana"},"state":"CHANGES_REQUESTED","body":"Please rename","submittedAt":"2026-10-25T08:02:00Z"},
                   {"id":"PRR_2","author":{"login":"me"},"state":"PENDING","body":""},
                   {"id":"PRR_3","author":null,"state":"COMMENTED","body":""}]}"#;

    #[test]
    fn reads_state_checks_reviews_and_every_page_of_comments() {
        let comments = r#"[{"id":7,"user":{"login":"ana"},"path":"src/lib.rs","line":42,"body":"Rename this"}]
[{"id":8,"user":null,"path":"README.md","line":null,"body":"Outdated"}]"#;
        let report = report(VIEW.as_bytes(), comments.as_bytes()).unwrap();
        assert_eq!(report.title, "Fix the build");
        assert_eq!(report.state, PullRequestState::Open);
        assert_eq!(
            (report.base.as_str(), report.head.as_str()),
            ("main", "fix")
        );
        assert_eq!(report.head_oid, "abc123");
        assert_eq!(
            report.review_decision,
            Some(ReviewDecision::ChangesRequested)
        );
        assert_eq!(report.merge_state, MergeState::Blocked);
        let checks: Vec<_> = report
            .checks
            .iter()
            .map(|c| {
                (
                    c.name.as_str(),
                    c.workflow.as_str(),
                    c.state,
                    c.summary.as_str(),
                )
            })
            .collect();
        assert_eq!(
            checks,
            [
                ("test", "CI", CheckState::Failed, "Timed out"),
                ("lint", "CI", CheckState::Pending, "In progress"),
                ("docs", "", CheckState::Neutral, "Cancelled"),
                ("ci/legacy", "", CheckState::Failed, "Error"),
            ]
        );
        assert_eq!(
            (report.checks[0].started_at, report.checks[0].completed_at),
            (Some(1_792_915_200), Some(1_792_915_272))
        );
        assert_eq!(report.checks[1].completed_at, None);
        assert_eq!(report.checks[3].url, "https://ci.example/1");
        let reviews: Vec<_> = report
            .reviews
            .iter()
            .map(|r| (r.id.as_str(), r.author.as_str(), r.state))
            .collect();
        assert_eq!(
            reviews,
            [
                ("PRR_1", "ana", ReviewState::ChangesRequested),
                ("PRR_3", "ghost", ReviewState::Commented),
            ],
            "a pending review is the reviewer's unsent draft"
        );
        assert_eq!(report.comments.len(), 2);
        assert_eq!(
            (report.comments[0].path.as_str(), report.comments[0].line),
            ("src/lib.rs", Some(42))
        );
        assert_eq!(
            (report.comments[1].author.as_str(), report.comments[1].line),
            ("ghost", None)
        );

        let draft = VIEW.replace(r#""isDraft":false"#, r#""isDraft":true"#);
        let draft = super::report(draft.as_bytes(), b"[]").unwrap();
        assert_eq!(draft.state, PullRequestState::Draft);
        let merged = VIEW
            .replace(r#""state":"OPEN""#, r#""state":"MERGED""#)
            .replace(
                r#""reviewDecision":"CHANGES_REQUESTED""#,
                r#""reviewDecision":"""#,
            )
            .replace(
                r#""mergeStateStatus":"BLOCKED""#,
                r#""mergeStateStatus":"CLEAN""#,
            );
        let merged = super::report(merged.as_bytes(), b"").unwrap();
        assert_eq!(merged.state, PullRequestState::Merged);
        assert_eq!(merged.review_decision, None);
        assert_eq!(merged.merge_state, MergeState::Clean);
        assert!(matches!(
            super::report(b"{", b"[]"),
            Err(Error::Failed { .. })
        ));
    }

    fn link(url: &str) -> PullRequest {
        PullRequest {
            number: url.rsplit('/').next().unwrap().parse().unwrap(),
            url: url.into(),
        }
    }

    #[test]
    fn views_a_pull_request_by_its_link_on_its_own_host() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake(
            dir.path(),
            &format!(
                "[ \"$4\" = user ] && {{ echo me; exit 0; }}\n[ \"$1\" = api ] && {{ echo '[]'; exit 0; }}\ncat <<'EOF'\n{VIEW}\nEOF\n"
            ),
        );
        let report = view(
            &gh,
            dir.path(),
            &link("https://ghe.example.com/o/r/pull/12"),
        )
        .unwrap();
        assert_eq!(report.title, "Fix the build");
        assert_eq!(
            argv(dir.path()),
            format!(
                "pr\nview\nhttps://ghe.example.com/o/r/pull/12\n--json\n{VIEW_FIELDS}\n---\napi\n--hostname\nghe.example.com\n--paginate\nrepos/o/r/pulls/12/comments?per_page=100\n---\napi\n--hostname\nghe.example.com\nuser\n--jq\n.login\n---\n"
            )
        );
        assert_eq!(report.viewer, "me");
        assert!(matches!(
            view(&gh, dir.path(), &link("https://github.com/o/12")),
            Err(Error::Failed { .. })
        ));
    }

    #[test]
    fn finds_only_an_open_pull_request_for_the_branch() {
        let dir = tempfile::tempdir().unwrap();
        let open = fake(
            dir.path(),
            "echo '{\"number\":5,\"url\":\"https://github.com/o/r/pull/5\",\"state\":\"OPEN\"}'\n",
        );
        assert_eq!(
            find(&open, dir.path()),
            Ok(Some(link("https://github.com/o/r/pull/5")))
        );
        assert!(argv(dir.path()).starts_with("pr\nview\n--json\nnumber,url,state\n"));
        let merged = fake(
            dir.path(),
            "echo '{\"number\":5,\"url\":\"https://github.com/o/r/pull/5\",\"state\":\"MERGED\"}'\n",
        );
        assert_eq!(find(&merged, dir.path()), Ok(None));
        let none = fake(
            dir.path(),
            "echo 'no pull requests found for branch \"fix\"' >&2\nexit 1\n",
        );
        assert_eq!(find(&none, dir.path()), Ok(None));
        let elsewhere = fake(
            dir.path(),
            "echo 'none of the git remotes configured for this repository point to a known GitHub host' >&2\nexit 1\n",
        );
        assert_eq!(find(&elsewhere, dir.path()), Ok(None));
        for output in [
            "no git remotes found",
            "failed to run git: fatal: not a git repository (or any of the parent directories): .git",
        ] {
            let none = fake(dir.path(), &format!("echo '{output}' >&2\nexit 1\n"));
            assert_eq!(find(&none, dir.path()), Ok(None), "{output}");
        }
        let signed_out = fake(dir.path(), "exit 4\n");
        assert!(matches!(
            find(&signed_out, dir.path()),
            Err(Error::SignedOut { .. })
        ));
    }

    #[test]
    fn reads_the_allowed_merge_methods_and_merges_only_the_head_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake(
            dir.path(),
            "[ \"$1\" = repo ] && echo '{\"squashMergeAllowed\":false,\"mergeCommitAllowed\":true,\"rebaseMergeAllowed\":true}'\nexit 0\n",
        );
        let pr = link("https://github.com/o/r/pull/12");
        assert_eq!(
            merge_methods(&gh, dir.path(), &pr),
            Ok(vec![MergeMethod::Merge, MergeMethod::Rebase])
        );
        merge(&gh, dir.path(), &pr, MergeMethod::Squash, "abc123").unwrap();
        assert_eq!(
            argv(dir.path()),
            "repo\nview\ngithub.com/o/r\n--json\nsquashMergeAllowed,mergeCommitAllowed,rebaseMergeAllowed\n---\napi\n--hostname\ngithub.com\n--method\nPUT\nrepos/o/r/pulls/12/merge\n-f\nmerge_method=squash\n-f\nsha=abc123\n---\n"
        );

        let refused = fake(
            dir.path(),
            "echo 'gh: Head branch was modified. Review and try the merge again. (HTTP 409)' >&2\nexit 1\n",
        );
        assert_eq!(
            merge(&refused, dir.path(), &pr, MergeMethod::Merge, "abc123"),
            Err(Error::Failed {
                output:
                    "gh: Head branch was modified. Review and try the merge again. (HTTP 409)\n"
                        .into()
            })
        );
    }
}
