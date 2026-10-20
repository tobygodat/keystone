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
