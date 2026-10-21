//! The `show_ui` calls both fake providers make. A prompt containing "show a widget"
//! asks for them: with "checklist" or "form" it shows that example, and with "invalid" it first
//! sends a call that fails validation, then the checklist, as an agent fixing its call
//! would. Without the tool, the reply says so in plain text.

use keystone_agent::surface::examples;
use serde_json::Value;

pub fn calls(prompt: &str) -> Vec<Value> {
    let mut calls = Vec::new();
    if !prompt.contains("show a widget") {
        return calls;
    }
    if prompt.contains("invalid") {
        calls.push(examples::invalid());
    }
    if prompt.contains("checklist") || prompt.contains("invalid") {
        calls.push(examples::checklist());
    }
    if prompt.contains("form") {
        calls.push(examples::form());
    }
    calls
}

/// The reply after the calls: what each one returned, so a test can read the action
/// the user sent back.
pub fn reply(results: &[String]) -> String {
    if results.is_empty() {
        return "The show_ui tool is not available here, so this is plain text (test data).".into();
    }
    let mut reply = String::from("Widget results (test data):");
    for result in results {
        reply.push(' ');
        reply.push_str(result);
    }
    reply
}
