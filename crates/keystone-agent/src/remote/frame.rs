//! The wire protocol between the app and `keystone-host`: one JSON object per line,
//! in UTF-8, at most [`MAX_FRAME`] bytes. Parsing and writing only, no IO policy.

use crate::idle::IdleCandidate;
use crate::{Approval, ApprovalState, Command, Event};
use keystone_core::{ModelChoice, Provider};
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Read};

/// The protocol both sides must speak. A host that speaks another refuses the link.
pub const PROTOCOL: u32 = 1;

/// The largest frame, newline excluded.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Frame {
    /// Opens the link. `after` is the last event the app received, or `None` on the
    /// first link of an app's run, which also starts call ids again from 1.
    Hello {
        protocol: u32,
        client: String,
        after: Option<u64>,
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
    /// Its place in the event sequence is `to`.
    Gap {
        provider: Provider,
        account: Option<String>,
        from: u64,
        to: u64,
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Call {
    pub id: u64,
    pub provider: Provider,
    pub account: Option<String>,
    #[serde(flatten)]
    pub op: Op,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", content = "args", rename_all = "snake_case")]
pub enum Op {
    Send {
        session: u64,
        cwd: String,
        resume: Option<String>,
        choice: ModelChoice,
        approval: ApprovalState,
        text: String,
    },
    Command(Command),
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

/// What a call returned: `"ok": null`, or `"err"` with the reason.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    #[serde(rename = "ok")]
    Ok(()),
    #[serde(rename = "err")]
    Err(String),
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
            op: Op::Send {
                session: 12,
                cwd: "/home/dev/src/app".into(),
                resume: None,
                choice: ModelChoice::default(),
                approval: ApprovalState {
                    mode: ApprovalMode::Ask,
                    overridden: false,
                },
                text: "hello".into(),
            },
        }));
        assert!(call.starts_with(r#"{"t":"call","id":7,"provider":"Claude","account":null,"op":"send","args":{"session":12,"#));
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
            op: Op::Command(Command::Answer {
                session: 12,
                request: RequestId("\"r1\"".into()),
                allow: true,
            }),
        }));
        let event = round_trip(Frame::Event(Routed {
            seq: 1042,
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
