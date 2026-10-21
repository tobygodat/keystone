use serde::{Deserialize, Serialize};

/// One entry in a turn: a message, reasoning, a command, or another tool call.
/// Completed items never change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub kind: ItemKind,
    /// Message text, a reasoning summary, or command output capped by [`cap_output`].
    pub text: String,
    pub status: ItemStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ItemKind {
    UserMessage,
    AgentMessage,
    Reasoning,
    Command {
        command: String,
        exit_code: Option<i32>,
    },
    FileChange {
        paths: Vec<String>,
    },
    Tool {
        name: String,
    },
    Other {
        name: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
    /// The turn ended before the item did, so its outcome is unknown: Keystone or the
    /// provider stopped mid-item, or a tool call has no recorded result.
    Interrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
}

/// A JSON-RPC request ID, kept as its raw JSON text so the answer echoes it exactly.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(pub String);

/// The provider is waiting for the user to allow or deny an action.
#[derive(Clone, Debug, PartialEq)]
pub struct Approval {
    pub request: RequestId,
    pub item_id: Option<String>,
    /// What the agent wants to do, such as the command line.
    pub action: String,
    /// The working directory or the agent's stated reason, when given.
    pub detail: Option<String>,
}

/// One model a provider offers.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelInfo {
    /// What is sent to run it: Claude's `value`, Codex's `model`.
    pub model: String,
    pub label: String,
    /// The model ID the provider says `model` runs as, when it says: Claude's
    /// `resolvedModel`, `None` for Codex. Reports of a running model use this form.
    pub resolved: Option<String>,
}

/// The models a provider reports it can run, or why it could not say.
#[derive(Clone, Debug, PartialEq)]
pub enum Catalog {
    Ready {
        models: Vec<ModelInfo>,
        /// The model ID the provider's default resolves to, when it says.
        default_model: Option<String>,
    },
    Failed(String),
}

/// What a driver reports for one session.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The provider conversation exists. Save the ID to resume it later.
    ThreadReady {
        thread_id: String,
    },
    /// The provider's answer to a catalog request ([`Catalog`]).
    Catalog(Catalog),
    /// The commands and skills the provider lists for a catalog request's folder.
    /// Claude sends it only after a ready [`Event::Catalog`], from the same answer;
    /// Codex asks for it separately and may fail one without the other.
    Commands(crate::Commands),
    /// The model the provider says this session runs on.
    ModelReported {
        model: String,
    },
    /// The process or conversation that reported the model is gone, so the report no
    /// longer describes anything running.
    ModelLost,
    TurnStarted,
    ItemStarted(Item),
    /// Text to append to an in-progress item.
    ItemDelta {
        item_id: String,
        text: String,
    },
    ItemCompleted(Item),
    ApprovalRequested(Approval),
    /// The provider no longer waits for this request, whether answered here or not.
    ApprovalResolved {
        request: RequestId,
    },
    TurnCompleted {
        status: TurnStatus,
        error: Option<String>,
        /// The user ended this turn: with Stop, or with a denial the provider ends the
        /// turn on. Its interruption is then not a failure.
        stopped: bool,
    },
    /// A request failed outside any turn, such as starting the conversation.
    Failed {
        message: String,
    },
    /// Something went wrong that does not end the turn, such as a rejected steer.
    Warning {
        message: String,
    },
    ProviderNotice {
        message: Option<String>,
    },
    /// The provider's stored history, read for crash recovery; `ticket` names the
    /// request. Never a live event: only the app consumes it.
    HistoryRead {
        ticket: u64,
        result: Result<crate::recovery::Snapshot, String>,
    },
    /// The provider process exited during a turn, so its store may hold more than
    /// streamed. Arrives in the same batch as the turn's end; act on it once the
    /// whole batch is applied.
    RecoveryNeeded,
    /// An agent showed a widget through `show_ui`. `waiting` says the provider's tool
    /// call waits for a Button press as its result ([`crate::Command::Act`]).
    SurfaceShown {
        surface: crate::surface::Surface,
        waiting: bool,
    },
    /// A press could not be returned as the tool's result, because the provider no
    /// longer waits for it. `text` is the action, to send as a steer or new message.
    ActionUndelivered {
        call: String,
        text: String,
    },
    /// The session's process is proven idle. The app saves everything it got before
    /// this, then may end the process with [`crate::Connection::evict_idle`]. Never a
    /// transcript event.
    IdleCandidate(crate::idle::IdleCandidate),
}

/// Bytes kept from each end of long tool output.
pub const OUTPUT_CAP: usize = 32 * 1024;

/// Keeps the first and last [`OUTPUT_CAP`] bytes and notes how much was dropped.
pub fn cap_output(text: &str) -> String {
    if text.len() <= OUTPUT_CAP * 2 {
        return text.to_owned();
    }
    let head = floor_char_boundary(text, OUTPUT_CAP);
    let tail = ceil_char_boundary(text, text.len() - OUTPUT_CAP);
    format!(
        "{}\n… {} bytes omitted …\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_output_keeps_both_ends_on_char_boundaries() {
        let short = "ok";
        assert_eq!(cap_output(short), "ok");
        let long = "é".repeat(OUTPUT_CAP * 2);
        let capped = cap_output(&long);
        assert!(capped.starts_with('é') && capped.ends_with('é'));
        assert!(capped.contains("bytes omitted"));
        assert!(capped.len() < long.len());
    }
}
