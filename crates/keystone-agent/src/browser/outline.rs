//! An agent's view of a page: Chrome's accessibility tree as an indented outline, with
//! a ref such as `e3` on each control so a later action can name it.

use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Longest outline returned; the rest is cut with a marker.
pub const MAX_OUTLINE: usize = 48 * 1024;
const MAX_NAME: usize = 200;
const MAX_DEPTH: usize = 64;
/// How deep the walk recurses, counting the unnamed wrappers it doesn't indent for, so a
/// page of thousands of nested elements can't overflow the stack.
const MAX_NESTING: usize = 512;

/// Roles an agent acts on. Any other focusable node also gets a ref.
const CONTROLS: &[&str] = &[
    "button",
    "link",
    "textbox",
    "searchbox",
    "combobox",
    "checkbox",
    "radio",
    "switch",
    "slider",
    "spinbutton",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "tab",
    "option",
    "listbox",
    "treeitem",
    "PopUpButton",
    "ListBoxOption",
    "MenuListOption",
];

/// Containers without a name that only add depth.
const TRANSPARENT: &[&str] = &[
    "generic",
    "none",
    "presentation",
    "GenericContainer",
    "group",
    "paragraph",
    "Section",
    "LayoutTable",
    "LayoutTableRow",
    "LayoutTableCell",
    "RootWebArea",
    "WebArea",
];

const SKIPPED: &[&str] = &["InlineTextBox", "LineBreak", "ListMarker"];

#[derive(Debug, Default, PartialEq)]
pub struct Outline {
    pub text: String,
    /// Each ref and the backend DOM node it names, in outline order.
    pub refs: Vec<(String, i64)>,
    pub truncated: bool,
}

/// Builds the outline from `Accessibility.getFullAXTree`'s nodes. `name` gives a
/// backend DOM node its ref.
pub fn outline(nodes: &[Value], name: &mut dyn FnMut(i64) -> String) -> Outline {
    let by_id: HashMap<&str, &Value> = nodes
        .iter()
        .filter_map(|node| Some((node["nodeId"].as_str()?, node)))
        .collect();
    let mut walk = Walk {
        name,
        by_id,
        seen: HashSet::new(),
        nesting: 0,
        out: Outline::default(),
    };
    if let Some(root) = nodes.iter().find(|node| node.get("parentId").is_none()) {
        walk.node(root, 0, "");
    }
    walk.out
}

struct Walk<'a> {
    name: &'a mut dyn FnMut(i64) -> String,
    by_id: HashMap<&'a str, &'a Value>,
    seen: HashSet<&'a str>,
    nesting: usize,
    out: Outline,
}

impl<'a> Walk<'a> {
    fn node(&mut self, node: &'a Value, depth: usize, parent_name: &str) {
        if self.nesting >= MAX_NESTING {
            return;
        }
        self.nesting += 1;
        self.visit(node, depth, parent_name);
        self.nesting -= 1;
    }

    fn visit(&mut self, node: &'a Value, depth: usize, parent_name: &str) {
        let Some(id) = node["nodeId"].as_str() else {
            return;
        };
        if depth > MAX_DEPTH || !self.seen.insert(id) || self.out.truncated {
            return;
        }
        let role = node["role"]["value"].as_str().unwrap_or("");
        let name = clean(node["name"]["value"].as_str().unwrap_or(""));
        if node["ignored"].as_bool() == Some(true)
            || (TRANSPARENT.contains(&role) && name.is_empty())
            || role == "RootWebArea"
        {
            return self.children(node, depth, parent_name);
        }
        if SKIPPED.contains(&role) {
            return;
        }
        if role == "StaticText" {
            if !name.is_empty() && name != parent_name {
                self.line(depth, &format!("- text {}", quote(&name)));
            }
            return;
        }
        let mut line = format!("- {role}");
        if !name.is_empty() {
            line.push(' ');
            line.push_str(&quote(&name));
        }
        if let Some(value) = node["value"]["value"]
            .as_str()
            .map(clean)
            .filter(|value| !value.is_empty())
        {
            line.push_str(&format!(" value={}", quote(&value)));
        }
        for (property, value) in properties(node) {
            match (property, value) {
                ("level", Value::Number(level)) if role == "heading" => {
                    line.push_str(&format!(" [level={level}]"))
                }
                ("checked" | "pressed", Value::String(state)) if state != "false" => {
                    line.push_str(&format!(" [{property}={state}]"))
                }
                (
                    "disabled" | "expanded" | "selected" | "focused" | "required",
                    Value::Bool(true),
                ) => line.push_str(&format!(" [{property}]")),
                _ => {}
            }
        }
        let focusable = properties(node)
            .any(|(property, value)| property == "focusable" && value == &Value::Bool(true));
        if let Some(backend) = node["backendDOMNodeId"].as_i64()
            && (CONTROLS.contains(&role) || focusable)
        {
            let reference = (self.name)(backend);
            line.push_str(&format!(" [ref={reference}]"));
            self.out.refs.push((reference, backend));
        }
        self.line(depth, &line);
        self.children(node, depth + 1, &name);
    }

    fn children(&mut self, node: &'a Value, depth: usize, parent_name: &str) {
        let parent_name = parent_name.to_owned();
        for child in node["childIds"].as_array().into_iter().flatten() {
            if let Some(child) = child.as_str().and_then(|id| self.by_id.get(id)) {
                self.node(child, depth, &parent_name);
            }
        }
    }

    fn line(&mut self, depth: usize, line: &str) {
        if self.out.text.len() + line.len() + depth * 2 + 1 > MAX_OUTLINE {
            self.out.truncated = true;
            self.out
                .text
                .push_str("- … (outline cut; scroll or act to see more)\n");
            return;
        }
        for _ in 0..depth {
            self.out.text.push_str("  ");
        }
        self.out.text.push_str(line);
        self.out.text.push('\n');
    }
}

fn properties(node: &Value) -> impl Iterator<Item = (&str, &Value)> {
    node["properties"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some((p["name"].as_str()?, &p["value"]["value"])))
}

/// One line, at most [`MAX_NAME`] characters.
fn clean(text: &str) -> String {
    let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match words.char_indices().nth(MAX_NAME) {
        Some((cut, _)) => format!("{}…", &words[..cut]),
        None => words,
    }
}

fn quote(text: &str) -> String {
    serde_json::to_string(text).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn numbered() -> impl FnMut(i64) -> String {
        let mut count = 0;
        move |_| {
            count += 1;
            format!("e{count}")
        }
    }

    fn node(id: &str, parent: Option<&str>, role: &str, name: &str, children: &[&str]) -> Value {
        let mut node = json!({
            "nodeId": id,
            "role": {"type": "role", "value": role},
            "name": {"type": "computedString", "value": name},
            "childIds": children,
            "backendDOMNodeId": id.parse::<i64>().unwrap() + 100,
            "ignored": false,
        });
        if let Some(parent) = parent {
            node["parentId"] = json!(parent);
        }
        node
    }

    #[test]
    fn controls_get_refs_and_text_and_headings_show_without_one() {
        let mut heading = node("2", Some("1"), "heading", "Sign in", &["3"]);
        heading["properties"] =
            json!([{"name": "level", "value": {"type": "integer", "value": 1}}]);
        let mut field = node("5", Some("4"), "textbox", "Email", &[]);
        field["value"] = json!({"type": "string", "value": "a@b.c"});
        field["properties"] =
            json!([{"name": "focusable", "value": {"type": "booleanOrUndefined", "value": true}}]);
        let mut hidden = node("8", Some("1"), "button", "Hidden", &[]);
        hidden["ignored"] = json!(true);
        let nodes = vec![
            node("1", None, "RootWebArea", "Page", &["2", "4", "8"]),
            heading,
            node("3", Some("2"), "StaticText", "Sign in", &[]),
            node("4", Some("1"), "generic", "", &["5", "6"]),
            field,
            node("6", Some("4"), "button", "Continue", &["7"]),
            node("7", Some("6"), "StaticText", "Continue", &[]),
            hidden,
        ];
        let outline = outline(&nodes, &mut numbered());
        assert_eq!(
            outline.text,
            "- heading \"Sign in\" [level=1]\n- textbox \"Email\" value=\"a@b.c\" [ref=e1]\n- button \"Continue\" [ref=e2]\n"
        );
        assert_eq!(outline.refs, vec![("e1".into(), 105), ("e2".into(), 106)]);
        assert!(!outline.truncated);
    }

    #[test]
    fn a_huge_page_is_cut_with_a_marker() {
        let ids: Vec<String> = (2..20_000).map(|i| i.to_string()).collect();
        let mut nodes = vec![node(
            "1",
            None,
            "RootWebArea",
            "",
            &ids.iter().map(String::as_str).collect::<Vec<_>>(),
        )];
        nodes.extend(
            ids.iter()
                .map(|id| node(id, Some("1"), "link", "A fairly long link label", &[])),
        );
        let outline = outline(&nodes, &mut numbered());
        assert!(outline.truncated);
        assert!(outline.text.len() <= MAX_OUTLINE + 64);
        assert!(
            outline
                .text
                .ends_with("(outline cut; scroll or act to see more)\n")
        );
    }

    #[test]
    fn thousands_of_nested_wrappers_stop_at_the_nesting_cap() {
        let count = 20_000;
        let mut nodes = vec![node("1", None, "RootWebArea", "", &["2"])];
        let ids: Vec<String> = (2..count + 3).map(|i| i.to_string()).collect();
        for pair in ids.windows(2) {
            nodes.push(node(
                &pair[0],
                Some("1"),
                "generic",
                "",
                &[pair[1].as_str()],
            ));
        }
        nodes.push(node(&ids[ids.len() - 1], Some("1"), "button", "Deep", &[]));
        let walked = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || outline(&nodes, &mut numbered()))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(walked.text, "", "the button past the cap is left out");
    }

    #[test]
    fn a_cycle_ends() {
        let nodes = vec![
            node("1", None, "RootWebArea", "", &["2"]),
            node("2", Some("1"), "list", "Items", &["2"]),
        ];
        assert_eq!(outline(&nodes, &mut numbered()).text, "- list \"Items\"\n");
    }
}
