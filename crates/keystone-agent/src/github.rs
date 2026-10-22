//! The user's own GitHub CLI, `gh`, signed in with the user's own login. Keystone never
//! sees or stores a GitHub token. Every call blocks, so callers run them on a
//! [`crate::worker::Worker`].

use crate::git::{self, Output};
use keystone_core::PullRequest;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// gh's exit code when a command needs a login it does not have.
const AUTH_REQUIRED: i32 = 4;
/// What to run in a terminal to sign in.
pub const SIGN_IN: &str = "gh auth login";

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
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("gh");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done >> \"{0}/argv\"\n[ \"$1\" = pr ] && cat >> \"{0}/argv\"\necho --- >> \"{0}/argv\"\n{script}",
                dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, PermissionsExt::from_mode(0o755)).unwrap();
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
}
