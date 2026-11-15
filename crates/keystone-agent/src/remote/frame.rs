//! The wire protocol between the app and `keystone-host`: one JSON object per line,
//! in UTF-8, at most [`MAX_FRAME`] bytes. Parsing and writing only, no IO policy.

use crate::idle::IdleCandidate;
use crate::{Approval, ApprovalState, Command, Event};
use keystone_core::{ModelChoice, Provider};
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Read};

/// The protocol both sides must speak. A host that speaks another refuses the link.
/// 3 added Codex, accounts and images: a call's `login`, `send`'s `images`, and the
/// `put_attachment`, `read_history`, `list_threads` and `read_import` ops. 4 added
/// `hello`'s `name`, which the host names the machine by in what it says. 5 added the
/// `login` and `unknown` replies, which say how a call failed rather than leaving the
/// app to read it from the message. 6 added `send`'s `send`, the ID the app saved the
/// message under, the `recall` op that asks what became of one, and the `unrecorded`
/// reply. 7 added each event's and gap's `stamp`, the daemon run that gave its number,
/// and `hello`'s `places`, every place the app has with its stamp, so a host can tell a
/// journal that went back and grew again from one that never went back. `hello` keeps
/// the shape protocol 6 parses, so an older host still reads its protocol and says
/// which one it needs.
pub const PROTOCOL: u32 = 7;

/// The most of an image one `put_attachment` call carries, before base64, so a frame
/// stays well within [`MAX_FRAME`].
pub const CHUNK: usize = 8 * 1024 * 1024;

/// The largest frame, newline excluded.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Frame {
    /// Opens the link. `after` is the last event the app received, in the journal
    /// `journal` names, or `None` on the first link of an app's run, which also starts
    /// call ids again from 1. A host whose journal isn't `journal` treats `after` as
    /// `None`: that number counts in a journal it doesn't have. `seen` is the highest
    /// place the app has in each journal, as protocol 6 sent it, and `places` every
    /// place it has, `after` and each saved cursor, as its journal, number and stamp. A
    /// host that finds its own journal named there past its latest event, or past its
    /// acknowledged cursor with another stamp than it gave that number, went back, as
    /// a restored backup would, and takes a new identity before it answers. Every field
    /// before `protocol` is checked keeps the shape a protocol 6 host parses, and such a
    /// host ignores `places`.
    Hello {
        protocol: u32,
        client: String,
        after: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        journal: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        seen: Vec<(String, u64)>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        places: Vec<(String, u64, u64)>,
        /// What the app calls this machine, such as "desk".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Welcome(Welcome),
    Refused {
        reason: String,
    },
    Call(Call),
    Reply {
        id: u64,
        #[serde(flatten)]
        outcome: Outcome,
    },
    Event(Routed),
    /// The journal no longer holds the items of `sessions` between `from` and `to`.
    /// Its place in the event sequence is `to`, with the stamp that number has.
    Gap {
        provider: Provider,
        account: Option<String>,
        from: u64,
        to: u64,
        #[serde(default)]
        stamp: u64,
        sessions: Vec<u64>,
    },
    /// A warning for `session` outside the event sequence, such as events waiting
    /// because the host couldn't journal them. It isn't journaled or sent again.
    Warning {
        provider: Provider,
        account: Option<String>,
        session: u64,
        message: String,
    },
    /// The replay after a `hello` ended at `seq`. Live events follow.
    Synced {
        seq: u64,
    },
    /// Each session's state, after `synced` on an app's first link.
    Sessions {
        sessions: Vec<SessionState>,
    },
    Quit {
        keep_running: bool,
    },
    /// The app drained every event through `seq` and has the reply of every call
    /// through `calls`.
    Ack {
        seq: u64,
        calls: u64,
    },
    Ping,
    Pong,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol: u32,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub hostname: String,
    pub providers: Vec<Found>,
    pub oldest: u64,
    pub latest: u64,
    /// The journal the host's sequence numbers count in: a random ID made with the
    /// journal file, so a journal deleted, reinstalled or on another machine has
    /// another one, and an event's place means nothing without it. A host before
    /// protocol 2 sends none, and parses here as empty so that its protocol, not a
    /// parse error, is what the app reports.
    #[serde(default)]
    pub journal: String,
}

/// A provider CLI the host looked for: its `--version` output, or why there is none.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Found {
    pub provider: Provider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<String>,
}

/// One `Connection` method call, for the connection of `provider` and `account`.
/// A call for an account other than the machine's default login carries `login`,
/// from which the host builds the account's launch profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Call {
    pub id: u64,
    pub provider: Provider,
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<Login>,
    #[serde(flatten)]
    pub op: Op,
}

/// An account on the host's machine: its name, and its config home there, an
/// absolute path on that machine that the app never resolves.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Login {
    pub label: String,
    pub home: String,
}

/// An image a `send` carries, by the hash of its bytes, which an earlier
/// `put_attachment` put in the host's cache.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attached {
    pub media_type: String,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", content = "args", rename_all = "snake_case")]
pub enum Op {
    Send {
        session: u64,
        /// The ID the app saved the message under, which the host records with the
        /// call's reply so [`Op::Recall`] can find it after the app's next launch.
        send: String,
        cwd: String,
        resume: Option<String>,
        choice: ModelChoice,
        approval: ApprovalState,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<Attached>,
    },
    /// What became of the message the app sent as `send`, in an earlier run whose
    /// call the app no longer has: the reply is that call's, `unknown` when it was
    /// taken and has none, or `unrecorded` when it never reached the host.
    Recall {
        session: u64,
        send: String,
    },
    Command(Command),
    /// Part of an image's bytes, in base64, from `offset` of its `size`. The host
    /// keeps the image by its hash once the last part arrives and the bytes match it.
    PutAttachment {
        sha256: String,
        media_type: String,
        size: u64,
        offset: u64,
        data: String,
    },
    /// Codex's stored history of `thread`, answered as `HistoryRead` with `ticket`.
    ReadHistory {
        session: u64,
        thread: String,
        ticket: u64,
    },
    /// The Codex conversations that ran in `cwd`, answered as `ThreadsListed` with
    /// `ticket`, for session 0.
    ListThreads {
        cwd: String,
        ticket: u64,
    },
    /// The newest turns of `thread` for an import, answered as `HistoryRead` with
    /// `ticket`, for session 0.
    ReadImport {
        thread: String,
        ticket: u64,
    },
    RequestCatalog {
        session: u64,
        cwd: String,
    },
    SetChoice {
        session: u64,
        choice: ModelChoice,
    },
    SetApproval {
        session: u64,
        approval: ApprovalState,
    },
    HoldFollowUps {
        session: u64,
    },
    ReleaseFollowUps {
        session: u64,
    },
    EvictIdle {
        session: u64,
        candidate: IdleCandidate,
    },
    DeclineIdle {
        session: u64,
        candidate: IdleCandidate,
        reason: String,
    },
}

/// What a call returned: `"ok": null`, or why it failed. `"err"` and `"login"` are
/// refusals before the call ran, `"login"` because of the account's login (its home on
/// the machine, or none given); `"unknown"` is a call the host lost track of, which
/// may or may not have run, so the app never offers it again as unsent. `"unrecorded"`
/// answers a `recall` for a message the host never received.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    #[serde(rename = "ok")]
    Ok(()),
    #[serde(rename = "err")]
    Err(String),
    #[serde(rename = "login")]
    Login(String),
    #[serde(rename = "unknown")]
    Unknown(String),
    #[serde(rename = "unrecorded")]
    Unrecorded,
}

impl From<Result<(), String>> for Outcome {
    fn from(result: Result<(), String>) -> Self {
        match result {
            Ok(()) => Outcome::Ok(()),
            Err(reason) => Outcome::Err(reason),
        }
    }
}

/// One journaled event and the connection it belongs to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Routed {
    pub seq: u64,
    /// The daemon run that gave `seq`: a random number each run of `keystone-host
    /// serve` draws when it opens its journal. A journal restored from a backup keeps
    /// the stamps its events had, and numbers it gives again get the new run's. Events
    /// journaled before stamps were kept have 0.
    #[serde(default)]
    pub stamp: u64,
    pub provider: Provider,
    pub account: Option<String>,
    pub session: u64,
    pub event: Event,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub provider: Provider,
    pub account: Option<String>,
    pub session: u64,
    pub running: bool,
    pub approvals: Vec<Approval>,
}

impl SessionState {
    /// Follows `event`: a turn starting or ending, or an approval waiting or resolved.
    /// Returns whether the event is one of those. An approval the app answers is gone
    /// without an event, so callers also apply [`SessionState::answered`].
    pub(crate) fn apply(&mut self, event: &Event) -> bool {
        match event {
            Event::TurnStarted => self.running = true,
            Event::TurnCompleted { .. } | Event::Failed { .. } => {
                self.running = false;
                self.approvals.clear();
            }
            Event::ApprovalRequested(approval) => self.approvals.push(approval.clone()),
            Event::ApprovalResolved { request } => self.answered(request),
            _ => return false,
        }
        true
    }

    pub(crate) fn answered(&mut self, request: &crate::RequestId) {
        self.approvals
            .retain(|approval| approval.request != *request);
    }
}

/// The frame as one line, newline included, or why it can't be sent.
pub fn encode(frame: &Frame) -> Result<Vec<u8>, String> {
    let mut line = serde_json::to_vec(frame).map_err(|e| e.to_string())?;
    if line.len() > MAX_FRAME {
        return Err(format!("a {} byte frame is over 16 MiB", line.len()));
    }
    line.push(b'\n');
    Ok(line)
}

pub fn parse(line: &[u8]) -> io::Result<Frame> {
    serde_json::from_slice(line)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad frame: {e}")))
}

/// Reads the next frame into `line`, reused across calls. `Ok(None)` at a clean end of
/// input. A frame over the limit, a cut-off frame or one that doesn't parse is an
/// error, never skipped.
pub fn read(reader: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<Option<Frame>> {
    line.clear();
    let read = reader
        .by_ref()
        .take(MAX_FRAME as u64 + 1)
        .read_until(b'\n', line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.pop() != Some(b'\n') {
        let reason = if line.len() >= MAX_FRAME {
            "a frame is over 16 MiB"
        } else {
            "the stream ended inside a frame"
        };
        return Err(io::Error::new(io::ErrorKind::InvalidData, reason));
    }
    parse(line).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Item, ItemKind, ItemStatus, RequestId};
    use keystone_core::ApprovalMode;

    fn round_trip(frame: Frame) -> String {
        let line = encode(&frame).unwrap();
        let mut reader = io::Cursor::new(line.clone());
        let mut buffer = Vec::new();
        assert_eq!(read(&mut reader, &mut buffer).unwrap(), Some(frame));
        assert_eq!(read(&mut reader, &mut buffer).unwrap(), None);
        String::from_utf8(line).unwrap()
    }

    #[test]
    fn frames_round_trip_in_the_documented_shape() {
        let call = round_trip(Frame::Call(Call {
            id: 7,
            provider: Provider::Claude,
            account: None,
            login: None,
            op: Op::Send {
                session: 12,
                send: "local:42-0".into(),
                cwd: "/home/dev/src/app".into(),
                resume: None,
                choice: ModelChoice::default(),
                approval: ApprovalState {
                    mode: ApprovalMode::Ask,
                    overridden: false,
                },
                text: "hello".into(),
                images: Vec::new(),
            },
        }));
        assert!(call.starts_with(r#"{"t":"call","id":7,"provider":"Claude","account":null,"op":"send","args":{"session":12,"send":"local:42-0","#));
        assert_eq!(
            round_trip(Frame::Reply {
                id: 8,
                outcome: Outcome::Unrecorded
            }),
            "{\"t\":\"reply\",\"id\":8,\"unrecorded\":null}\n"
        );
        let value: serde_json::Value = serde_json::from_str(&call).unwrap();
        assert_eq!(
            value["args"]["choice"],
            serde_json::json!({"model": null, "effort": null, "speed": null})
        );
        assert!(value["args"].get("model").is_none());
        assert_eq!(
            round_trip(Frame::Reply {
                id: 7,
                outcome: Outcome::Ok(())
            }),
            "{\"t\":\"reply\",\"id\":7,\"ok\":null}\n"
        );
        assert_eq!(
            round_trip(Frame::Reply {
                id: 8,
                outcome: Outcome::Err("no".into())
            }),
            "{\"t\":\"reply\",\"id\":8,\"err\":\"no\"}\n"
        );
        round_trip(Frame::Call(Call {
            id: 9,
            provider: Provider::Claude,
            account: None,
            login: None,
            op: Op::Command(Command::Answer {
                session: 12,
                request: RequestId("\"r1\"".into()),
                allow: true,
            }),
        }));
        let event = round_trip(Frame::Event(Routed {
            seq: 1042,
            stamp: 77,
            provider: Provider::Claude,
            account: None,
            session: 12,
            event: Event::ItemCompleted(Item {
                id: "i".into(),
                kind: ItemKind::Command {
                    command: "ls".into(),
                    exit_code: Some(0),
                },
                text: "out".into(),
                status: ItemStatus::Completed,
            }),
        }));
        assert!(event.contains(r#""event":{"ItemCompleted":"#));
        assert_eq!(round_trip(Frame::Ping), "{\"t\":\"ping\"}\n");
        round_trip(Frame::Sessions {
            sessions: vec![SessionState {
                provider: Provider::Claude,
                account: None,
                session: 12,
                running: true,
                approvals: vec![Approval {
                    request: RequestId("1".into()),
                    item_id: None,
                    action: "ls".into(),
                    detail: None,
                }],
            }],
        });
    }

    #[test]
    fn a_login_and_images_cross_the_wire_only_when_there_are_some() {
        let send = |login, images| {
            round_trip(Frame::Call(Call {
                id: 3,
                provider: Provider::Codex,
                account: Some("codex-work".into()),
                login,
                op: Op::Send {
                    session: 4,
                    send: "local:43-1".into(),
                    cwd: "/srv/app".into(),
                    resume: None,
                    choice: ModelChoice::default(),
                    approval: ApprovalState {
                        mode: ApprovalMode::Ask,
                        overridden: false,
                    },
                    text: "look".into(),
                    images,
                },
            }))
        };
        let plain = send(None, Vec::new());
        assert!(
            !plain.contains("login") && !plain.contains("images"),
            "{plain}"
        );
        let full = send(
            Some(Login {
                label: "Work".into(),
                home: "/home/dev/.codex-work".into(),
            }),
            vec![Attached {
                media_type: "image/png".into(),
                sha256: "ab".repeat(32),
            }],
        );
        let value: serde_json::Value = serde_json::from_str(&full).unwrap();
        assert_eq!(value["login"]["home"], "/home/dev/.codex-work");
        assert_eq!(value["args"]["images"][0]["media_type"], "image/png");
        round_trip(Frame::Call(Call {
            id: 4,
            provider: Provider::Claude,
            account: None,
            login: None,
            op: Op::PutAttachment {
                sha256: "ab".repeat(32),
                media_type: "image/png".into(),
                size: 3,
                offset: 0,
                data: "AAEC".into(),
            },
        }));
    }

    #[test]
    fn a_question_and_its_reply_cross_the_wire() {
        use crate::{Answer, Bounds, Choice, Field, Input, Question};
        let field = |header: &str, input| Field {
            header: header.into(),
            prompt: Some(format!("{header}?")),
            input,
            required: true,
            bounds: Bounds {
                min: Some(1.),
                max: None,
            },
        };
        let question = Question {
            request: RequestId("\"q1\"".into()),
            server: Some("docs".into()),
            message: Some("Pick".into()),
            fields: vec![
                field(
                    "Color",
                    Input::Choice {
                        options: vec![Choice {
                            label: "Blue".into(),
                            description: None,
                        }],
                        multiple: true,
                        other: true,
                    },
                ),
                field("Note", Input::Text),
                field("Count", Input::Number { integer: true }),
            ],
        };
        round_trip(Frame::Event(Routed {
            seq: 3,
            stamp: 0,
            provider: Provider::Codex,
            account: None,
            session: 12,
            event: Event::QuestionAsked(question),
        }));
        for answers in [
            Some(vec![Answer {
                chosen: vec![0],
                text: "teal".into(),
            }]),
            None,
        ] {
            round_trip(Frame::Call(Call {
                id: 10,
                provider: Provider::Codex,
                account: None,
                login: None,
                op: Op::Command(Command::Reply {
                    session: 12,
                    request: RequestId("\"q1\"".into()),
                    answers,
                }),
            }));
        }
    }

    #[test]
    fn a_frame_that_breaks_the_rules_is_an_error() {
        let mut buffer = Vec::new();
        for input in [
            &b"{\"t\":\"ping\"}"[..],
            b"{\"t\":\"nope\"}\n",
            b"not json\n",
        ] {
            let mut reader = io::Cursor::new(input.to_vec());
            assert!(read(&mut reader, &mut buffer).is_err());
        }
        let mut huge = vec![b' '; MAX_FRAME + 2];
        huge.push(b'\n');
        assert!(read(&mut io::Cursor::new(huge), &mut buffer).is_err());
        let mut fits = b"{\"t\":\"ping\"}".to_vec();
        fits.resize(MAX_FRAME, b' ');
        fits.push(b'\n');
        assert_eq!(
            read(&mut io::Cursor::new(fits), &mut buffer).unwrap(),
            Some(Frame::Ping)
        );
    }
}
