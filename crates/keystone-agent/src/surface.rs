//! Agent-built widgets: the `show_ui` tool and Keystone's A2UI v0.9.1 catalog.
//!
//! An agent calls `show_ui` with a list of A2UI messages. [`parse`] checks them against
//! the catalog and builds one [`Surface`], or returns a `VALIDATION_FAILED` error the
//! agent can act on. The surface keeps its own data model; inputs write to it through
//! JSON Pointer bindings, and a Button reports [`Surface::action`] with its bound
//! context. Nothing here touches the UI, the disk or a provider. See
//! `docs/design/widgets.md`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

/// The tool's name as both providers list it.
pub const TOOL: &str = "show_ui";
/// The SDK-hosted MCP server Claude Code reaches the tool through.
pub const SERVER: &str = "keystone_ui";
/// The name Claude Code gives the tool in its own messages.
pub const CLAUDE_TOOL: &str = "mcp__keystone_ui__show_ui";
/// Keystone's catalog: a subset of the A2UI basic catalog.
pub const CATALOG: &str = "https://github.com/tobygodat/keystone/a2ui/catalogs/widgets/v1";
/// The A2UI version Keystone answers with.
pub const VERSION: &str = "v0.9.1";

const MAX_INPUT_BYTES: usize = 256 * 1024;
const MAX_MESSAGES: usize = 64;
const MAX_COMPONENTS: usize = 200;
const MAX_TEXT: usize = 8 * 1024;
const MAX_ID: usize = 128;
const MAX_DEPTH: usize = 16;
const MAX_OPTIONS: usize = 50;
const MAX_DATA_BYTES: usize = 64 * 1024;
const MAX_DATA_DEPTH: usize = 32;
const MAX_CONTEXT: usize = 16;
/// Slider bounds stay where the renderer's `f32` holds every whole number exactly.
const MAX_SLIDER: f64 = 1_000_000.0;

/// What the tool description teaches the agent. No system prompt is added.
pub fn description() -> String {
    format!(
        "Show an interactive widget (checklist, form, choices, slider) inline in Keystone's \
transcript and, when it has a Button, wait for the user to press one. The result is an A2UI \
`action` message: the button's `name` and its `context` with every data binding resolved, \
for example the boxes the user ticked or the text they typed. Use it when structured input \
from the user is clearer than prose; otherwise just reply in text.

Input: `messages`, a list of A2UI {VERSION} messages for ONE new surface. Each message has \
`\"version\": \"{VERSION}\"` and one of:
- `createSurface`: {{\"surfaceId\": \"...\", \"catalogId\": \"{CATALOG}\", \"sendDataModel\"?: bool}}. Must come first.
- `updateComponents`: {{\"surfaceId\": \"...\", \"components\": [...]}}. A flat list; one component has id `root`.
- `updateDataModel`: {{\"surfaceId\": \"...\", \"path\"?: \"/json/pointer\", \"value\"?: any}}. Sets initial state.

Components (every one has a unique `id` and `\"component\"`; children are referenced by id, \
never inline; `weight` sets flex grow inside a Row or Column):
- Column / Row: `children` (array of ids), `justify` (start|center|end|spaceBetween|spaceAround|spaceEvenly|stretch), `align` (start|center|end|stretch).
- Card: `child` (one id).
- Text: `text`, `variant` (h1|h2|h3|h4|h5|caption|body).
- Divider: `axis` (horizontal|vertical).
- Button: `child` (id of a Text), `variant` (default|primary|borderless), `action`: {{\"event\": {{\"name\": \"submit\", \"context\": {{\"key\": literal or {{\"path\": \"/pointer\"}}}}}}}}.
- CheckBox: `label`, `value` {{\"path\": \"/pointer\"}} to a boolean.
- TextField: `label`, `value` {{\"path\"}} to a string, `variant` (shortText|longText|number).
- ChoicePicker: `label`, `options` [{{\"label\", \"value\"}}], `value` {{\"path\"}} to a string array, `variant` (mutuallyExclusive|multipleSelection), `displayStyle` (checkbox|chips).
- Slider: `label`, `min`, `max`, `value` {{\"path\"}} to a number.
Text props (`text`, `label`, option labels) are a literal string or {{\"path\": \"/pointer\"}}. Inputs \
must bind `value` with a path; missing values start empty (false, \"\", [], min). Functions, \
`checks`, templates and other components are not in this catalog. An invalid call returns a \
VALIDATION_FAILED error naming the JSON Pointer of the problem; fix it and call again.

Example checklist: [{{\"version\":\"{VERSION}\",\"createSurface\":{{\"surfaceId\":\"todo\",\"catalogId\":\"{CATALOG}\"}}}}, \
{{\"version\":\"{VERSION}\",\"updateComponents\":{{\"surfaceId\":\"todo\",\"components\":[\
{{\"id\":\"root\",\"component\":\"Column\",\"children\":[\"title\",\"a\",\"b\",\"done\"]}},\
{{\"id\":\"title\",\"component\":\"Text\",\"text\":\"Pick what to ship\",\"variant\":\"h3\"}},\
{{\"id\":\"a\",\"component\":\"CheckBox\",\"label\":\"Docs\",\"value\":{{\"path\":\"/docs\"}}}},\
{{\"id\":\"b\",\"component\":\"CheckBox\",\"label\":\"Tests\",\"value\":{{\"path\":\"/tests\"}}}},\
{{\"id\":\"done\",\"component\":\"Button\",\"child\":\"done-label\",\"variant\":\"primary\",\"action\":{{\"event\":{{\"name\":\"submit\",\"context\":{{\"docs\":{{\"path\":\"/docs\"}},\"tests\":{{\"path\":\"/tests\"}}}}}}}}}},\
{{\"id\":\"done-label\",\"component\":\"Text\",\"text\":\"Done\"}}]}}}}]"
    )
}

/// The tool's input schema. [`parse`] checks the details and names what is wrong.
pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "messages": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_MESSAGES,
                "description": "A2UI v0.9.1 messages for one new surface: createSurface first, then updateComponents and updateDataModel.",
                "items": {"type": "object"}
            }
        },
        "required": ["messages"],
        "additionalProperties": false
    })
}

/// The tool as MCP `tools/list` reports it.
pub fn mcp_tool() -> Value {
    json!({"name": TOOL, "description": description(), "inputSchema": input_schema()})
}

/// The tool as a Codex dynamic tool spec.
pub fn codex_tool() -> Value {
    json!({"type": "function", "name": TOOL, "description": description(), "inputSchema": input_schema()})
}

/// Where a surface stands with the user.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// It has a Button and nothing was sent yet: it waits on the user.
    #[default]
    Waiting,
    /// A Button was pressed. Later presses still reach the agent.
    Answered,
    /// It has no Button, so nothing waits on the user.
    Shown,
    /// The user stopped the turn that asked for it before answering.
    Stopped,
}

/// A literal string, or a JSON Pointer into the surface's data model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Dynamic {
    Literal(String),
    Bound { path: String },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Justify {
    #[default]
    Start,
    Center,
    End,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
    Stretch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Align {
    Start,
    Center,
    End,
    Stretch,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TextVariant {
    H1,
    H2,
    H3,
    H4,
    H5,
    Caption,
    #[default]
    Body,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ButtonVariant {
    #[default]
    Default,
    Primary,
    Borderless,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FieldVariant {
    #[default]
    ShortText,
    LongText,
    Number,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub label: Dynamic,
    pub value: String,
}

/// A Button's event: its name and context, whose values are literals or `{"path"}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub name: String,
    #[serde(default)]
    pub context: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "component")]
pub enum Kind {
    Column {
        children: Vec<String>,
        #[serde(default)]
        justify: Justify,
        #[serde(default)]
        align: Option<Align>,
    },
    Row {
        children: Vec<String>,
        #[serde(default)]
        justify: Justify,
        #[serde(default)]
        align: Option<Align>,
    },
    Card {
        child: String,
    },
    Text {
        text: Dynamic,
        #[serde(default)]
        variant: TextVariant,
    },
    Divider {
        #[serde(default)]
        vertical: bool,
    },
    Button {
        child: String,
        #[serde(default)]
        variant: ButtonVariant,
        action: Event,
    },
    CheckBox {
        label: Dynamic,
        value: String,
    },
    TextField {
        label: Dynamic,
        value: String,
        #[serde(default)]
        variant: FieldVariant,
    },
    ChoicePicker {
        label: Option<Dynamic>,
        options: Vec<Choice>,
        value: String,
        #[serde(default)]
        multiple: bool,
        #[serde(default)]
        chips: bool,
    },
    Slider {
        label: Option<Dynamic>,
        min: f64,
        max: f64,
        value: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Component {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f32>,
    #[serde(flatten)]
    pub kind: Kind,
}

/// One agent-built widget: what `show_ui` asked for and the user's state in it. Saved
/// whole, keyed by `call`, so it survives a restart.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Surface {
    /// The provider's ID for the tool call that showed it.
    pub call: String,
    pub surface_id: String,
    pub components: Vec<Component>,
    /// The surface's data model, which inputs write to.
    pub data: Value,
    pub status: Status,
    /// Whether an action carries the whole data model too (`sendDataModel`).
    #[serde(default)]
    pub send_data_model: bool,
    /// The name of the last action sent, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_action: Option<String>,
}

/// Why `show_ui` arguments were refused, as A2UI's `VALIDATION_FAILED` error.
#[derive(Clone, Debug, PartialEq)]
pub struct Invalid {
    pub surface_id: String,
    /// A JSON Pointer into the tool's arguments.
    pub path: String,
    pub message: String,
}

impl Invalid {
    /// The error as the A2UI client-to-server message the agent receives.
    pub fn to_message(&self) -> String {
        json!({"version": VERSION, "error": {"code": "VALIDATION_FAILED",
            "surfaceId": self.surface_id, "path": self.path, "message": self.message}})
        .to_string()
    }
}

/// What the tool returns for a surface with no Button.
pub fn shown_message(surface: &Surface) -> String {
    format!(
        "Surface \"{}\" is shown to the user. It has no Button, so no action will be returned.",
        surface.surface_id
    )
}

/// What a press becomes when the provider no longer waits for the tool's result: a
/// steer or a new message that names the call.
pub fn message_for(call: &str, action: &str) -> String {
    format!("Keystone widget action (from show_ui call {call}):\n{action}")
}

struct Parser {
    surface_id: String,
    components: Vec<(Component, String)>,
    data: Value,
    send_data_model: bool,
}

fn fail<T>(
    surface_id: &str,
    path: impl Into<String>,
    message: impl Into<String>,
) -> Result<T, Invalid> {
    Err(Invalid {
        surface_id: surface_id.to_owned(),
        path: path.into(),
        message: message.into(),
    })
}

/// Checks `show_ui` arguments against Keystone's catalog and builds the surface for
/// tool call `call`.
pub fn parse(call: &str, arguments: &Value) -> Result<Surface, Invalid> {
    if serde_json::to_string(arguments).map_or(0, |s| s.len()) > MAX_INPUT_BYTES {
        return fail(
            "",
            "",
            format!("The arguments are larger than {MAX_INPUT_BYTES} bytes"),
        );
    }
    let Some(object) = arguments.as_object() else {
        return fail("", "", "Arguments must be an object with `messages`");
    };
    if let Some(key) = object.keys().find(|key| *key != "messages") {
        return fail(
            "",
            format!("/{}", escape(key)),
            format!("Unknown argument `{key}`; only `messages` is accepted"),
        );
    }
    let Some(messages) = object.get("messages").and_then(Value::as_array) else {
        return fail(
            "",
            "/messages",
            "`messages` must be an array of A2UI messages",
        );
    };
    if messages.is_empty() || messages.len() > MAX_MESSAGES {
        return fail(
            "",
            "/messages",
            format!("Send 1 to {MAX_MESSAGES} messages"),
        );
    }
    let mut parser = Parser {
        surface_id: String::new(),
        components: Vec::new(),
        data: Value::Object(Map::new()),
        send_data_model: false,
    };
    for (index, message) in messages.iter().enumerate() {
        parser.message(index, message)?;
    }
    parser.finish(call)
}

impl Parser {
    fn fail<T>(&self, path: impl Into<String>, message: impl Into<String>) -> Result<T, Invalid> {
        fail(&self.surface_id, path, message)
    }

    fn message(&mut self, index: usize, message: &Value) -> Result<(), Invalid> {
        let at = format!("/messages/{index}");
        let Some(object) = message.as_object() else {
            return self.fail(at, "Each message must be an object");
        };
        match object.get("version").and_then(Value::as_str) {
            Some("v0.9" | "v0.9.1") => {}
            _ => {
                return self.fail(
                    format!("{at}/version"),
                    format!("Set \"version\": \"{VERSION}\""),
                );
            }
        }
        let kinds: Vec<&String> = object.keys().filter(|key| *key != "version").collect();
        let [kind] = kinds.as_slice() else {
            return self.fail(
                at,
                "Each message has `version` and exactly one of createSurface, updateComponents or updateDataModel",
            );
        };
        let at = format!("{at}/{}", escape(kind));
        let body = &object[kind.as_str()];
        let Some(body) = body.as_object() else {
            return self.fail(at, format!("`{kind}` must be an object"));
        };
        if index == 0 && kind.as_str() != "createSurface" {
            return self.fail(at, "The first message must be createSurface");
        }
        if kind.as_str() != "createSurface" {
            match body.get("surfaceId").and_then(Value::as_str) {
                Some(id) if id == self.surface_id => {}
                Some(id) => {
                    return self.fail(
                        format!("{at}/surfaceId"),
                        format!(
                            "Unknown surfaceId \"{id}\"; this call creates \"{}\"",
                            self.surface_id
                        ),
                    );
                }
                None => return self.fail(format!("{at}/surfaceId"), "`surfaceId` is required"),
            }
        }
        match kind.as_str() {
            "createSurface" => self.create(&at, body, index),
            "updateComponents" => self.update_components(&at, body),
            "updateDataModel" => self.update_data(&at, body),
            "deleteSurface" => self.fail(
                at,
                "deleteSurface is not supported: each show_ui call shows one new surface",
            ),
            other => self.fail(at, format!("Unknown message type `{other}`")),
        }
    }

    fn create(&mut self, at: &str, body: &Map<String, Value>, index: usize) -> Result<(), Invalid> {
        if index != 0 {
            return self.fail(
                at,
                "Only one createSurface is allowed, as the first message",
            );
        }
        for key in body.keys() {
            if !matches!(
                key.as_str(),
                "surfaceId" | "catalogId" | "theme" | "sendDataModel"
            ) {
                return self.fail(
                    format!("{at}/{}", escape(key)),
                    format!("Unknown property `{key}`"),
                );
            }
        }
        let id = match body.get("surfaceId").and_then(Value::as_str) {
            Some(id) if !id.is_empty() && id.len() <= MAX_ID => id,
            _ => {
                return self.fail(
                    format!("{at}/surfaceId"),
                    format!("`surfaceId` must be a string of 1 to {MAX_ID} bytes"),
                );
            }
        };
        self.surface_id = id.to_owned();
        if body.get("catalogId").and_then(Value::as_str) != Some(CATALOG) {
            return self.fail(
                format!("{at}/catalogId"),
                format!("Keystone renders only its own catalog: set \"catalogId\": \"{CATALOG}\""),
            );
        }
        match body.get("sendDataModel") {
            None => {}
            Some(Value::Bool(send)) => self.send_data_model = *send,
            Some(_) => {
                return self.fail(
                    format!("{at}/sendDataModel"),
                    "`sendDataModel` must be a boolean",
                );
            }
        }
        if body.get("theme").is_some_and(|theme| !theme.is_object()) {
            return self.fail(format!("{at}/theme"), "`theme` must be an object");
        }
        Ok(())
    }

    fn update_components(&mut self, at: &str, body: &Map<String, Value>) -> Result<(), Invalid> {
        if let Some(key) = body
            .keys()
            .find(|key| !matches!(key.as_str(), "surfaceId" | "components"))
        {
            return self.fail(
                format!("{at}/{}", escape(key)),
                format!("Unknown property `{key}`"),
            );
        }
        let Some(list) = body
            .get("components")
            .and_then(Value::as_array)
            .filter(|list| !list.is_empty())
        else {
            return self.fail(
                format!("{at}/components"),
                "`components` must be a non-empty array",
            );
        };
        for (index, value) in list.iter().enumerate() {
            let path = format!("{at}/components/{index}");
            let component = self.component(&path, value)?;
            // A later definition of an ID replaces the earlier one, as an update does.
            match self
                .components
                .iter_mut()
                .find(|(c, _)| c.id == component.id)
            {
                Some(existing) => *existing = (component, path.clone()),
                None => self.components.push((component, path.clone())),
            }
            if self.components.len() > MAX_COMPONENTS {
                return self.fail(
                    path,
                    format!("A surface holds at most {MAX_COMPONENTS} components"),
                );
            }
        }
        Ok(())
    }

    fn update_data(&mut self, at: &str, body: &Map<String, Value>) -> Result<(), Invalid> {
        if let Some(key) = body
            .keys()
            .find(|key| !matches!(key.as_str(), "surfaceId" | "path" | "value"))
        {
            return self.fail(
                format!("{at}/{}", escape(key)),
                format!("Unknown property `{key}`"),
            );
        }
        let path = match body.get("path") {
            None => "",
            Some(Value::String(path)) if path == "/" => "",
            Some(Value::String(path)) => path,
            Some(_) => {
                return self.fail(format!("{at}/path"), "`path` must be a JSON Pointer string");
            }
        };
        let result = match body.get("value") {
            Some(value) if path.is_empty() => {
                if value.is_object() {
                    self.data = value.clone();
                    Ok(())
                } else {
                    Err("The whole data model must be an object".to_owned())
                }
            }
            Some(value) => set(&mut self.data, path, value.clone()),
            None if path.is_empty() => {
                self.data = Value::Object(Map::new());
                Ok(())
            }
            None => remove(&mut self.data, path),
        };
        if let Err(message) = result {
            return self.fail(format!("{at}/path"), message);
        }
        if let Err(message) = fits(&self.data) {
            return self.fail(at, message);
        }
        Ok(())
    }

    fn component(&self, at: &str, value: &Value) -> Result<Component, Invalid> {
        let Some(object) = value.as_object() else {
            return self.fail(at, "Each component must be an object");
        };
        let id = match object.get("id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() && id.len() <= MAX_ID => id.to_owned(),
            _ => {
                return self.fail(
                    format!("{at}/id"),
                    format!("`id` must be a string of 1 to {MAX_ID} bytes"),
                );
            }
        };
        let Some(name) = object.get("component").and_then(Value::as_str) else {
            return self.fail(
                format!("{at}/component"),
                "`component` must name a catalog component",
            );
        };
        let allowed: &[&str] = match name {
            "Column" | "Row" => &["children", "justify", "align"],
            "Card" => &["child"],
            "Text" => &["text", "variant"],
            "Divider" => &["axis"],
            "Button" => &["child", "variant", "action"],
            "CheckBox" => &["label", "value"],
            "TextField" => &["label", "value", "variant"],
            "ChoicePicker" => &[
                "label",
                "options",
                "value",
                "variant",
                "displayStyle",
                "filterable",
            ],
            "Slider" => &["label", "min", "max", "value"],
            other => {
                return self.fail(
                    format!("{at}/component"),
                    format!("`{other}` is not in Keystone's catalog. Use Column, Row, Card, Text, Divider, Button, CheckBox, TextField, ChoicePicker or Slider"),
                );
            }
        };
        for key in object.keys() {
            if matches!(
                key.as_str(),
                "id" | "component" | "weight" | "accessibility"
            ) || allowed.contains(&key.as_str())
            {
                continue;
            }
            let hint = match key.as_str() {
                "checks" => {
                    "`checks` (client-side functions) are not in Keystone's catalog".to_owned()
                }
                "validationRegexp" => {
                    "`validationRegexp` is not supported in Keystone's catalog".to_owned()
                }
                _ => format!("Unknown property `{key}` for {name}"),
            };
            return self.fail(format!("{at}/{}", escape(key)), hint);
        }
        let weight = match object.get("weight") {
            None => None,
            Some(weight) => match weight.as_f64().filter(|w| w.is_finite() && *w >= 0.0) {
                Some(weight) => Some(weight as f32),
                None => {
                    return self.fail(
                        format!("{at}/weight"),
                        "`weight` must be a non-negative number",
                    );
                }
            },
        };
        let field = |key: &str| (object.get(key), format!("{at}/{key}"));
        let kind = match name {
            "Column" | "Row" => {
                let (children, path) = field("children");
                let children = match children {
                    Some(Value::Array(ids)) => ids
                        .iter()
                        .enumerate()
                        .map(|(i, id)| match id.as_str() {
                            Some(id) if !id.is_empty() => Ok(id.to_owned()),
                            _ => self
                                .fail(format!("{path}/{i}"), "Each child is a component id string"),
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    Some(Value::Object(_)) => {
                        return self.fail(
                            path,
                            "Child templates are not supported; list the child ids",
                        );
                    }
                    _ => return self.fail(path, "`children` must be an array of component ids"),
                };
                let justify = self.enumerated(object, at, "justify")?.unwrap_or_default();
                let align = self.enumerated(object, at, "align")?;
                if name == "Column" {
                    Kind::Column {
                        children,
                        justify,
                        align,
                    }
                } else {
                    Kind::Row {
                        children,
                        justify,
                        align,
                    }
                }
            }
            "Card" => Kind::Card {
                child: self.reference(object, at, "child")?,
            },
            "Text" => Kind::Text {
                text: self.dynamic(object, at, "text")?,
                variant: self.enumerated(object, at, "variant")?.unwrap_or_default(),
            },
            "Divider" => {
                let (axis, path) = field("axis");
                let vertical = match axis.map(|a| a.as_str()) {
                    None | Some(Some("horizontal")) => false,
                    Some(Some("vertical")) => true,
                    _ => return self.fail(path, "`axis` must be horizontal or vertical"),
                };
                Kind::Divider { vertical }
            }
            "Button" => Kind::Button {
                child: self.reference(object, at, "child")?,
                variant: self.enumerated(object, at, "variant")?.unwrap_or_default(),
                action: self.action(object, at)?,
            },
            "CheckBox" => Kind::CheckBox {
                label: self.dynamic(object, at, "label")?,
                value: self.binding(object, at)?,
            },
            "TextField" => {
                let (variant, path) = field("variant");
                let variant = match variant.map(|v| v.as_str()) {
                    None | Some(Some("shortText")) => FieldVariant::ShortText,
                    Some(Some("longText")) => FieldVariant::LongText,
                    Some(Some("number")) => FieldVariant::Number,
                    Some(Some("obscured")) => {
                        return self.fail(path, "Obscured fields are not supported: Keystone does not collect secrets through widgets");
                    }
                    _ => return self.fail(path, "`variant` must be shortText, longText or number"),
                };
                Kind::TextField {
                    label: self.dynamic(object, at, "label")?,
                    value: self.binding(object, at)?,
                    variant,
                }
            }
            "ChoicePicker" => {
                let (options, path) = field("options");
                let Some(options) = options
                    .and_then(Value::as_array)
                    .filter(|o| !o.is_empty() && o.len() <= MAX_OPTIONS)
                else {
                    return self.fail(path, format!("`options` must be an array of 1 to {MAX_OPTIONS} {{label, value}} objects"));
                };
                let mut seen = HashSet::new();
                let mut choices = Vec::new();
                for (i, option) in options.iter().enumerate() {
                    let at = format!("{path}/{i}");
                    let Some(option) = option.as_object() else {
                        return self.fail(at, "Each option is an object with `label` and `value`");
                    };
                    if let Some(key) = option
                        .keys()
                        .find(|k| !matches!(k.as_str(), "label" | "value"))
                    {
                        return self.fail(
                            format!("{at}/{}", escape(key)),
                            format!("Unknown property `{key}`"),
                        );
                    }
                    let Some(value) = option.get("value").and_then(Value::as_str) else {
                        return self.fail(format!("{at}/value"), "`value` must be a string");
                    };
                    if !seen.insert(value.to_owned()) {
                        return self.fail(
                            format!("{at}/value"),
                            format!("Option value \"{value}\" is repeated"),
                        );
                    }
                    choices.push(Choice {
                        label: self.dynamic(option, &at, "label")?,
                        value: value.to_owned(),
                    });
                }
                let (variant, variant_path) = field("variant");
                let multiple = match variant.map(|v| v.as_str()) {
                    None | Some(Some("mutuallyExclusive")) => false,
                    Some(Some("multipleSelection")) => true,
                    _ => {
                        return self.fail(
                            variant_path,
                            "`variant` must be mutuallyExclusive or multipleSelection",
                        );
                    }
                };
                let (style, style_path) = field("displayStyle");
                let chips = match style.map(|v| v.as_str()) {
                    None | Some(Some("checkbox")) => false,
                    Some(Some("chips")) => true,
                    _ => return self.fail(style_path, "`displayStyle` must be checkbox or chips"),
                };
                if object.get("filterable").is_some_and(|f| !f.is_boolean()) {
                    return self.fail(format!("{at}/filterable"), "`filterable` must be a boolean");
                }
                Kind::ChoicePicker {
                    label: self.optional_dynamic(object, at, "label")?,
                    options: choices,
                    value: self.binding(object, at)?,
                    multiple,
                    chips,
                }
            }
            "Slider" => {
                let number = |key: &str| {
                    object
                        .get(key)
                        .map(|v| v.as_f64().filter(|n| n.abs() <= MAX_SLIDER))
                };
                let min = match number("min") {
                    None => 0.0,
                    Some(Some(min)) => min,
                    Some(None) => {
                        return self.fail(
                            format!("{at}/min"),
                            format!("`min` must be a number from -{MAX_SLIDER} to {MAX_SLIDER}"),
                        );
                    }
                };
                let max = match number("max") {
                    Some(Some(max)) if max as f32 > min as f32 => max,
                    _ => {
                        return self.fail(
                            format!("{at}/max"),
                            format!(
                                "`max` must be a number up to {MAX_SLIDER}, clearly greater than `min`"
                            ),
                        );
                    }
                };
                Kind::Slider {
                    label: self.optional_dynamic(object, at, "label")?,
                    min,
                    max,
                    value: self.binding(object, at)?,
                }
            }
            _ => unreachable!("checked above"),
        };
        Ok(Component { id, weight, kind })
    }

    fn enumerated<T: for<'de> Deserialize<'de>>(
        &self,
        object: &Map<String, Value>,
        at: &str,
        key: &str,
    ) -> Result<Option<T>, Invalid> {
        match object.get(key) {
            None => Ok(None),
            Some(value) => serde_json::from_value(value.clone())
                .map(Some)
                .or_else(|_| {
                    self.fail(
                        format!("{at}/{key}"),
                        format!("`{key}` has an unknown value {value}"),
                    )
                }),
        }
    }

    fn reference(
        &self,
        object: &Map<String, Value>,
        at: &str,
        key: &str,
    ) -> Result<String, Invalid> {
        match object.get(key).and_then(Value::as_str) {
            Some(id) if !id.is_empty() => Ok(id.to_owned()),
            _ => self.fail(
                format!("{at}/{key}"),
                format!("`{key}` must be the id of another component"),
            ),
        }
    }

    fn dynamic(
        &self,
        object: &Map<String, Value>,
        at: &str,
        key: &str,
    ) -> Result<Dynamic, Invalid> {
        match self.optional_dynamic(object, at, key)? {
            Some(dynamic) => Ok(dynamic),
            None => self.fail(format!("{at}/{key}"), format!("`{key}` is required")),
        }
    }

    fn optional_dynamic(
        &self,
        object: &Map<String, Value>,
        at: &str,
        key: &str,
    ) -> Result<Option<Dynamic>, Invalid> {
        let path = format!("{at}/{key}");
        match object.get(key) {
            None => Ok(None),
            Some(Value::String(text)) if text.len() <= MAX_TEXT => {
                Ok(Some(Dynamic::Literal(text.clone())))
            }
            Some(Value::String(_)) => {
                self.fail(path, format!("Text is limited to {MAX_TEXT} bytes"))
            }
            Some(Value::Object(binding)) if binding.contains_key("call") => self.fail(
                path,
                "Function calls are not in Keystone's catalog; use a literal or {\"path\"}",
            ),
            Some(Value::Object(binding)) => {
                match (binding.len(), binding.get("path").and_then(Value::as_str)) {
                    (1, Some(pointer)) => {
                        tokens(pointer)
                            .or_else(|message| self.fail(format!("{path}/path"), message))?;
                        Ok(Some(Dynamic::Bound {
                            path: pointer.to_owned(),
                        }))
                    }
                    _ => self.fail(path, "A binding is {\"path\": \"/json/pointer\"}"),
                }
            }
            Some(_) => self.fail(
                path,
                format!("`{key}` must be a string or {{\"path\": \"/json/pointer\"}}"),
            ),
        }
    }

    fn binding(&self, object: &Map<String, Value>, at: &str) -> Result<String, Invalid> {
        let path = format!("{at}/value");
        match object.get("value") {
            Some(Value::Object(binding)) if binding.len() == 1 => match binding.get("path").and_then(Value::as_str) {
                Some(pointer) if !pointer.is_empty() => {
                    tokens(pointer).or_else(|message| self.fail(format!("{path}/path"), message))?;
                    Ok(pointer.to_owned())
                }
                _ => self.fail(format!("{path}/path"), "Bind `value` to a non-root JSON Pointer"),
            },
            _ => self.fail(path, "Bind `value` to the data model with {\"path\": \"/json/pointer\"} so the user's input is kept"),
        }
    }

    fn action(&self, object: &Map<String, Value>, at: &str) -> Result<Event, Invalid> {
        let path = format!("{at}/action");
        let Some(action) = object.get("action").and_then(Value::as_object) else {
            return self.fail(
                path,
                "`action` must be {\"event\": {\"name\": ..., \"context\": {...}}}",
            );
        };
        if action.contains_key("functionCall") {
            return self.fail(
                format!("{path}/functionCall"),
                "functionCall actions are not in Keystone's catalog; use an event",
            );
        }
        let Some(event) = action
            .get("event")
            .and_then(Value::as_object)
            .filter(|_| action.len() == 1)
        else {
            return self.fail(
                path,
                "`action` must be {\"event\": {\"name\": ..., \"context\": {...}}}",
            );
        };
        let path = format!("{path}/event");
        if let Some(key) = event
            .keys()
            .find(|k| !matches!(k.as_str(), "name" | "context"))
        {
            return self.fail(
                format!("{path}/{}", escape(key)),
                format!("Unknown property `{key}`"),
            );
        }
        let name = match event.get("name").and_then(Value::as_str) {
            Some(name) if !name.is_empty() && name.len() <= MAX_ID => name.to_owned(),
            _ => {
                return self.fail(
                    format!("{path}/name"),
                    format!("`name` must be a string of 1 to {MAX_ID} bytes"),
                );
            }
        };
        let mut context = BTreeMap::new();
        match event.get("context") {
            None => {}
            Some(Value::Object(entries)) if entries.len() > MAX_CONTEXT => {
                return self.fail(
                    format!("{path}/context"),
                    format!("`context` holds at most {MAX_CONTEXT} entries"),
                );
            }
            Some(Value::Object(entries)) => {
                for (key, value) in entries {
                    let at = format!("{path}/context/{}", escape(key));
                    match value {
                        Value::Object(binding) if binding.contains_key("call") => {
                            return self.fail(at, "Function calls are not in Keystone's catalog");
                        }
                        Value::Object(binding) => {
                            match (binding.len(), binding.get("path").and_then(Value::as_str)) {
                                (1, Some(pointer)) => {
                                    tokens(pointer).or_else(|message| {
                                        self.fail(format!("{at}/path"), message)
                                    })?;
                                }
                                _ => return self.fail(
                                    at,
                                    "A context value is a literal or {\"path\": \"/json/pointer\"}",
                                ),
                            }
                        }
                        Value::Null => return self.fail(at, "A context value cannot be null"),
                        _ => {}
                    }
                    context.insert(key.clone(), value.clone());
                }
            }
            Some(_) => return self.fail(format!("{path}/context"), "`context` must be an object"),
        }
        Ok(Event { name, context })
    }

    fn finish(mut self, call: &str) -> Result<Surface, Invalid> {
        if self.components.is_empty() {
            return self.fail(
                "/messages",
                "Send updateComponents with a component whose id is `root`",
            );
        }
        let index: HashMap<&str, usize> = self
            .components
            .iter()
            .enumerate()
            .map(|(i, (c, _))| (c.id.as_str(), i))
            .collect();
        let Some(&root) = index.get("root") else {
            return self.fail("/messages", "No component has id `root`");
        };
        // Every component on the tree is reached once, from `root`, within the depth limit.
        let mut parent: HashMap<usize, usize> = HashMap::new();
        let mut stack = vec![(root, 0usize)];
        let mut order = Vec::new();
        while let Some((at, depth)) = stack.pop() {
            order.push(at);
            let (component, path) = &self.components[at];
            if depth > MAX_DEPTH {
                return self.fail(
                    path.clone(),
                    format!("Components nest at most {MAX_DEPTH} deep"),
                );
            }
            let children: Vec<(String, String)> = match &component.kind {
                Kind::Column { children, .. } | Kind::Row { children, .. } => children
                    .iter()
                    .enumerate()
                    .map(|(i, id)| (id.clone(), format!("{path}/children/{i}")))
                    .collect(),
                Kind::Card { child } | Kind::Button { child, .. } => {
                    vec![(child.clone(), format!("{path}/child"))]
                }
                _ => Vec::new(),
            };
            for (id, child_path) in children {
                let Some(&child) = index.get(id.as_str()) else {
                    return self.fail(child_path, format!("No component has id \"{id}\""));
                };
                if child == root || parent.insert(child, at).is_some() {
                    return self.fail(child_path, format!("Component \"{id}\" is used more than once; each component has one parent"));
                }
                if let Kind::Button { .. } = component.kind
                    && !matches!(self.components[child].0.kind, Kind::Text { .. })
                {
                    return self.fail(child_path, "A Button's child must be a Text");
                }
                stack.push((child, depth + 1));
            }
        }
        // Bindings are checked against the data model the messages built.
        for &at in &order {
            let (component, path) = &self.components[at];
            let display = |dynamic: &Dynamic, field: &str| -> Result<(), Invalid> {
                let Dynamic::Bound { path: pointer } = dynamic else {
                    return Ok(());
                };
                match get(&self.data, pointer) {
                    Lookup::Found(Value::String(_) | Value::Number(_) | Value::Bool(_)) => Ok(()),
                    Lookup::Found(_) => self.fail(
                        format!("{path}/{field}/path"),
                        format!("{pointer} must hold a string to display"),
                    ),
                    Lookup::Missing => self.fail(
                        format!("{path}/{field}/path"),
                        format!("{pointer} is not in the data model; set it with updateDataModel"),
                    ),
                    Lookup::Blocked(message) => self.fail(format!("{path}/{field}/path"), message),
                }
            };
            match &component.kind {
                Kind::Text { text, .. } => display(text, "text")?,
                Kind::CheckBox { label, .. } | Kind::TextField { label, .. } => {
                    display(label, "label")?
                }
                Kind::ChoicePicker { label, options, .. } => {
                    if let Some(label) = label {
                        display(label, "label")?;
                    }
                    for (i, option) in options.iter().enumerate() {
                        display(&option.label, &format!("options/{i}/label"))?;
                    }
                }
                Kind::Slider {
                    label: Some(label), ..
                } => display(label, "label")?,
                _ => {}
            }
            let (pointer, default, fits): (&str, Value, fn(&Value) -> bool) = match &component.kind
            {
                Kind::CheckBox { value, .. } => (value, json!(false), Value::is_boolean),
                Kind::TextField { value, .. } => (value, json!(""), Value::is_string),
                Kind::ChoicePicker { value, .. } => (value, json!([]), |v| {
                    v.as_array().is_some_and(|a| a.iter().all(Value::is_string))
                }),
                Kind::Slider { value, min, .. } => (value, json!(min), Value::is_number),
                _ => continue,
            };
            let at = format!("{path}/value/path");
            match get(&self.data, pointer) {
                Lookup::Found(value) if fits(value) => {}
                Lookup::Found(value) => {
                    return self.fail(
                        at,
                        format!(
                            "{pointer} holds {value}, which does not fit a {}",
                            kind_name(&component.kind)
                        ),
                    );
                }
                Lookup::Missing => {
                    if let Err(message) = set(&mut self.data, pointer, default) {
                        return self.fail(at, message);
                    }
                }
                Lookup::Blocked(message) => return self.fail(at, message),
            }
            if let Kind::Slider {
                value, min, max, ..
            } = &component.kind
                && let Some(number) = get(&self.data, value).found().and_then(Value::as_f64)
                && (number < *min || number > *max)
            {
                return self.fail(at, format!("{value} holds {number}, outside {min}..{max}"));
            }
        }
        // Actions bind paths too; a missing value resolves to null when pressed.
        for &at in &order {
            let (component, path) = &self.components[at];
            let Kind::Button { action, .. } = &component.kind else {
                continue;
            };
            for (key, value) in &action.context {
                if let Some(pointer) = value.get("path").and_then(Value::as_str)
                    && let Lookup::Blocked(message) = get(&self.data, pointer)
                {
                    return self.fail(
                        format!("{path}/action/event/context/{}/path", escape(key)),
                        message,
                    );
                }
            }
        }
        if let Err(message) = fits(&self.data) {
            return self.fail("/messages", message);
        }
        let interactive = order
            .iter()
            .any(|&at| matches!(self.components[at].0.kind, Kind::Button { .. }));
        // Only what the tree shows is kept, root first.
        let mut kept: Vec<Option<Component>> =
            self.components.into_iter().map(|(c, _)| Some(c)).collect();
        let components = order.iter().filter_map(|&at| kept[at].take()).collect();
        Ok(Surface {
            call: call.to_owned(),
            surface_id: self.surface_id,
            components,
            data: self.data,
            status: if interactive {
                Status::Waiting
            } else {
                Status::Shown
            },
            send_data_model: self.send_data_model,
            last_action: None,
        })
    }
}

fn kind_name(kind: &Kind) -> &'static str {
    match kind {
        Kind::CheckBox { .. } => "CheckBox (boolean)",
        Kind::TextField { .. } => "TextField (string)",
        Kind::ChoicePicker { .. } => "ChoicePicker (string array)",
        Kind::Slider { .. } => "Slider (number)",
        _ => "component",
    }
}

impl Surface {
    pub fn component(&self, id: &str) -> Option<&Component> {
        self.components.iter().find(|c| c.id == id)
    }

    /// Whether the surface waits on the user.
    pub fn waiting(&self) -> bool {
        self.status == Status::Waiting
    }

    /// The text a literal or binding shows.
    pub fn text(&self, dynamic: &Dynamic) -> String {
        match dynamic {
            Dynamic::Literal(text) => text.clone(),
            Dynamic::Bound { path } => match get(&self.data, path).found() {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Null) | None => String::new(),
                Some(other) => other.to_string(),
            },
        }
    }

    /// The value bound at `pointer`, or `None` when the data model has none.
    pub fn value(&self, pointer: &str) -> Option<&Value> {
        get(&self.data, pointer).found()
    }

    /// Writes an input's value. Fails, changing nothing, for a pointer the surface
    /// never validated or a value that makes the data model too large to keep.
    pub fn set_value(&mut self, pointer: &str, value: Value) -> Result<(), String> {
        let before = self.data.clone();
        let result = set(&mut self.data, pointer, value).and_then(|()| fits(&self.data));
        if result.is_err() {
            self.data = before;
        }
        result
    }

    /// The A2UI `action` message for a press of Button `component`, with its context
    /// resolved against the data model. `timestamp_ms` is milliseconds since the epoch.
    pub fn action(&self, component: &str, timestamp_ms: u64) -> Option<String> {
        let Kind::Button { action, .. } = &self.component(component)?.kind else {
            return None;
        };
        let context: Map<String, Value> = action
            .context
            .iter()
            .map(|(key, value)| {
                let resolved = match value
                    .as_object()
                    .and_then(|b| b.get("path"))
                    .and_then(Value::as_str)
                {
                    Some(pointer) => self.value(pointer).cloned().unwrap_or(Value::Null),
                    None => value.clone(),
                };
                (key.clone(), resolved)
            })
            .collect();
        let mut message = json!({"version": VERSION, "action": {"name": action.name,
            "surfaceId": self.surface_id, "sourceComponentId": component,
            "timestamp": iso8601(timestamp_ms), "context": context}});
        if self.send_data_model {
            message["metadata"] = json!({"a2uiClientDataModel": {"surfaceId": self.surface_id, "dataModel": self.data}});
        }
        Some(message.to_string())
    }

    /// Records that Button `component` was pressed.
    pub fn record_action(&mut self, component: &str) {
        if let Some(Kind::Button { action, .. }) = self.component(component).map(|c| &c.kind) {
            self.last_action = Some(action.name.clone());
        }
        if matches!(self.status, Status::Waiting | Status::Stopped) {
            self.status = Status::Answered;
        }
    }
}

/// UTC time as ISO 8601 with milliseconds.
pub fn iso8601(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    let rest = ms % 86_400_000;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    )
}

/// What a JSON Pointer finds.
#[derive(Debug, PartialEq)]
pub enum Lookup<'a> {
    Found(&'a Value),
    Missing,
    /// The pointer is malformed, or passes through a value that holds no children.
    Blocked(String),
}

impl<'a> Lookup<'a> {
    fn found(self) -> Option<&'a Value> {
        match self {
            Lookup::Found(value) => Some(value),
            _ => None,
        }
    }
}

/// Whether a data model is small and shallow enough to show, send and save.
fn fits(data: &Value) -> Result<(), String> {
    let mut stack = vec![(data, 1usize)];
    while let Some((value, depth)) = stack.pop() {
        if depth > MAX_DATA_DEPTH {
            return Err(format!(
                "The data model nests more than {MAX_DATA_DEPTH} levels deep"
            ));
        }
        match value {
            Value::Object(map) => stack.extend(map.values().map(|v| (v, depth + 1))),
            Value::Array(list) => stack.extend(list.iter().map(|v| (v, depth + 1))),
            _ => {}
        }
    }
    if data.to_string().len() > MAX_DATA_BYTES {
        return Err(format!(
            "The data model is larger than {MAX_DATA_BYTES} bytes"
        ));
    }
    Ok(())
}

fn escape(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// The reference tokens of a JSON Pointer (RFC 6901). The empty pointer is the root.
fn tokens(pointer: &str) -> Result<Vec<String>, String> {
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    let Some(rest) = pointer.strip_prefix('/') else {
        return Err(format!(
            "\"{pointer}\" is not a JSON Pointer: it must start with /"
        ));
    };
    if rest.split('/').count() > MAX_DATA_DEPTH {
        return Err(format!(
            "\"{pointer}\" is more than {MAX_DATA_DEPTH} levels deep"
        ));
    }
    rest.split('/')
        .map(|token| {
            let mut out = String::new();
            let mut chars = token.chars();
            while let Some(c) = chars.next() {
                match (c, c == '~') {
                    (_, true) => match chars.next() {
                        Some('0') => out.push('~'),
                        Some('1') => out.push('/'),
                        _ => return Err(format!("\"{pointer}\" has a ~ not followed by 0 or 1")),
                    },
                    (c, false) => out.push(c),
                }
            }
            Ok(out)
        })
        .collect()
}

fn index(token: &str) -> Option<usize> {
    (token == "0" || !token.starts_with('0'))
        .then(|| token.parse().ok())
        .flatten()
}

pub fn get<'a>(data: &'a Value, pointer: &str) -> Lookup<'a> {
    let tokens = match tokens(pointer) {
        Ok(tokens) => tokens,
        Err(message) => return Lookup::Blocked(message),
    };
    let mut at = data;
    for (n, token) in tokens.iter().enumerate() {
        let next = match at {
            Value::Object(map) => map.get(token),
            Value::Array(list) => match index(token) {
                Some(i) => list.get(i),
                None => {
                    return Lookup::Blocked(format!(
                        "{pointer}: \"{token}\" is not an array index"
                    ));
                }
            },
            _ => {
                let parent = tokens[..n]
                    .iter()
                    .map(|t| format!("/{}", escape(t)))
                    .collect::<String>();
                return Lookup::Blocked(format!(
                    "{pointer} passes through {parent}, which is not an object or array"
                ));
            }
        };
        match next {
            Some(value) => at = value,
            None => return Lookup::Missing,
        }
    }
    Lookup::Found(at)
}

/// Sets the value at `pointer`, creating objects for missing parents. `-` or the
/// length appends to an array.
pub fn set(data: &mut Value, pointer: &str, value: Value) -> Result<(), String> {
    let tokens = tokens(pointer)?;
    let Some((last, parents)) = tokens.split_last() else {
        *data = value;
        return Ok(());
    };
    let mut at = data;
    for token in parents {
        at = match at {
            Value::Object(map) => map
                .entry(token.clone())
                .or_insert_with(|| Value::Object(Map::new())),
            Value::Array(list) => match index(token) {
                Some(i) if i < list.len() => &mut list[i],
                _ => {
                    return Err(format!(
                        "{pointer}: \"{token}\" is not an index in the array"
                    ));
                }
            },
            _ => {
                return Err(format!(
                    "{pointer} passes through a value that is not an object or array"
                ));
            }
        };
    }
    match at {
        Value::Object(map) => {
            map.insert(last.clone(), value);
            Ok(())
        }
        Value::Array(list) => match (last.as_str(), index(last)) {
            ("-", _) => {
                list.push(value);
                Ok(())
            }
            (_, Some(i)) if i < list.len() => {
                list[i] = value;
                Ok(())
            }
            (_, Some(i)) if i == list.len() => {
                list.push(value);
                Ok(())
            }
            _ => Err(format!(
                "{pointer}: \"{last}\" is not an index in the array"
            )),
        },
        _ => Err(format!(
            "{pointer} passes through a value that is not an object or array"
        )),
    }
}

fn remove(data: &mut Value, pointer: &str) -> Result<(), String> {
    let tokens = tokens(pointer)?;
    let Some((last, parents)) = tokens.split_last() else {
        return Ok(());
    };
    let mut at = data;
    for token in parents {
        at = match at {
            Value::Object(map) => match map.get_mut(token) {
                Some(next) => next,
                None => return Ok(()),
            },
            Value::Array(list) => match index(token).and_then(|i| list.get_mut(i)) {
                Some(next) => next,
                None => return Ok(()),
            },
            _ => {
                return Err(format!(
                    "{pointer} passes through a value that is not an object or array"
                ));
            }
        };
    }
    match at {
        Value::Object(map) => {
            map.remove(last);
        }
        Value::Array(list) => {
            if let Some(i) = index(last).filter(|i| *i < list.len()) {
                list.remove(i);
            }
        }
        _ => {
            return Err(format!(
                "{pointer} passes through a value that is not an object or array"
            ));
        }
    }
    Ok(())
}

/// Scripted `show_ui` arguments for tests and the fake providers: a checklist, a form,
/// and a call that fails validation.
pub mod examples {
    use super::{CATALOG, VERSION};
    use serde_json::{Value, json};

    fn message(kind: &str, body: Value) -> Value {
        json!({"version": VERSION, kind: body})
    }

    pub fn checklist() -> Value {
        json!({"messages": [
            message("createSurface", json!({"surfaceId": "release-checklist", "catalogId": CATALOG})),
            message("updateComponents", json!({"surfaceId": "release-checklist", "components": [
                {"id": "root", "component": "Card", "child": "body"},
                {"id": "body", "component": "Column", "children": ["title", "hint", "docs", "tests", "changelog", "divider", "actions"]},
                {"id": "title", "component": "Text", "text": "Release checklist", "variant": "h3"},
                {"id": "hint", "component": "Text", "text": "Tick what is ready, then send it back.", "variant": "caption"},
                {"id": "docs", "component": "CheckBox", "label": "Docs updated", "value": {"path": "/items/docs"}},
                {"id": "tests", "component": "CheckBox", "label": "Tests pass", "value": {"path": "/items/tests"}},
                {"id": "changelog", "component": "CheckBox", "label": "Changelog written", "value": {"path": "/items/changelog"}},
                {"id": "divider", "component": "Divider"},
                {"id": "actions", "component": "Row", "justify": "end", "children": ["skip", "send"]},
                {"id": "skip", "component": "Button", "child": "skip-label", "variant": "borderless",
                 "action": {"event": {"name": "skip"}}},
                {"id": "skip-label", "component": "Text", "text": "Skip"},
                {"id": "send", "component": "Button", "child": "send-label", "variant": "primary",
                 "action": {"event": {"name": "submit", "context": {"items": {"path": "/items"}}}}},
                {"id": "send-label", "component": "Text", "text": "Send checklist"}
            ]})),
            message("updateDataModel", json!({"surfaceId": "release-checklist", "path": "/items",
                "value": {"docs": true, "tests": false, "changelog": false}}))
        ]})
    }

    pub fn form() -> Value {
        json!({"messages": [
            message("createSurface", json!({"surfaceId": "bug-report", "catalogId": CATALOG})),
            message("updateComponents", json!({"surfaceId": "bug-report", "components": [
                {"id": "root", "component": "Card", "child": "body"},
                {"id": "body", "component": "Column", "children": ["title", "summary", "severity", "areas", "confidence", "notify", "actions"]},
                {"id": "title", "component": "Text", "text": "File a bug", "variant": "h3"},
                {"id": "summary", "component": "TextField", "label": "Summary", "value": {"path": "/summary"}},
                {"id": "severity", "component": "ChoicePicker", "label": "Severity", "variant": "mutuallyExclusive",
                 "options": [{"label": "Low", "value": "low"}, {"label": "Medium", "value": "medium"}, {"label": "High", "value": "high"}],
                 "value": {"path": "/severity"}},
                {"id": "areas", "component": "ChoicePicker", "label": "Areas", "variant": "multipleSelection", "displayStyle": "chips",
                 "options": [{"label": "Sidebar", "value": "sidebar"}, {"label": "Transcript", "value": "transcript"}, {"label": "Composer", "value": "composer"}],
                 "value": {"path": "/areas"}},
                {"id": "confidence", "component": "Slider", "label": "Confidence", "min": 0, "max": 10, "value": {"path": "/confidence"}},
                {"id": "notify", "component": "CheckBox", "label": "Notify me when fixed", "value": {"path": "/notify"}},
                {"id": "actions", "component": "Row", "justify": "end", "children": ["submit"]},
                {"id": "submit", "component": "Button", "child": "submit-label", "variant": "primary",
                 "action": {"event": {"name": "file_bug", "context": {
                     "summary": {"path": "/summary"}, "severity": {"path": "/severity"},
                     "areas": {"path": "/areas"}, "confidence": {"path": "/confidence"},
                     "notify": {"path": "/notify"}, "form": "bug-report"}}}},
                {"id": "submit-label", "component": "Text", "text": "File bug"}
            ]})),
            message("updateDataModel", json!({"surfaceId": "bug-report", "value": {
                "severity": ["medium"], "confidence": 5}}))
        ]})
    }

    /// A checklist whose Button names a child that does not exist.
    pub fn invalid() -> Value {
        let mut value = checklist();
        value["messages"][1]["updateComponents"]["components"][12]["id"] = json!("send-text");
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_examples_parse_and_initialize_their_inputs() {
        let checklist = parse("call-1", &examples::checklist()).unwrap();
        assert_eq!(checklist.surface_id, "release-checklist");
        assert_eq!(checklist.status, Status::Waiting);
        assert_eq!(checklist.components[0].id, "root");
        assert_eq!(checklist.value("/items/docs"), Some(&json!(true)));
        let form = parse("call-2", &examples::form()).unwrap();
        assert_eq!(form.value("/summary"), Some(&json!("")));
        assert_eq!(form.value("/areas"), Some(&json!([])));
        assert_eq!(form.value("/notify"), Some(&json!(false)));
        assert_eq!(form.value("/confidence"), Some(&json!(5)));
    }

    #[test]
    fn an_action_resolves_its_context_from_the_data_model() {
        let mut surface = parse("call-1", &examples::checklist()).unwrap();
        surface.set_value("/items/tests", json!(true)).unwrap();
        let action: Value =
            serde_json::from_str(&surface.action("send", 1_760_000_000_123).unwrap()).unwrap();
        assert_eq!(action["version"], VERSION);
        assert_eq!(action["action"]["name"], "submit");
        assert_eq!(action["action"]["surfaceId"], "release-checklist");
        assert_eq!(action["action"]["sourceComponentId"], "send");
        assert_eq!(action["action"]["timestamp"], "2025-10-09T08:53:20.123Z");
        assert_eq!(
            action["action"]["context"]["items"],
            json!({"docs": true, "tests": true, "changelog": false})
        );
        assert!(action.get("metadata").is_none());
        surface.record_action("send");
        assert_eq!(surface.status, Status::Answered);
        assert_eq!(surface.last_action.as_deref(), Some("submit"));
    }

    #[test]
    fn literal_context_values_pass_through_and_missing_paths_resolve_to_null() {
        let mut args = examples::form();
        args["messages"][1]["updateComponents"]["components"][9]["action"]["event"]["context"]["extra"] =
            json!({"path": "/nothing"});
        let surface = parse("c", &args).unwrap();
        let action: Value = serde_json::from_str(&surface.action("submit", 0).unwrap()).unwrap();
        assert_eq!(action["action"]["context"]["form"], "bug-report");
        assert_eq!(action["action"]["context"]["extra"], Value::Null);
        assert_eq!(action["action"]["timestamp"], "1970-01-01T00:00:00.000Z");
    }

    fn error(args: Value) -> Invalid {
        parse("call", &args).unwrap_err()
    }

    #[test]
    fn unknown_children_name_the_pointer_of_the_bad_reference() {
        let invalid = error(examples::invalid());
        assert_eq!(
            invalid.path,
            "/messages/1/updateComponents/components/11/child"
        );
        assert!(
            invalid.message.contains("send-label"),
            "{}",
            invalid.message
        );
        let message: Value = serde_json::from_str(&invalid.to_message()).unwrap();
        assert_eq!(message["error"]["code"], "VALIDATION_FAILED");
        assert_eq!(message["error"]["surfaceId"], "release-checklist");
    }

    #[test]
    fn components_outside_the_catalog_are_refused_with_the_catalog_listed() {
        let mut args = examples::checklist();
        args["messages"][1]["updateComponents"]["components"][2]["component"] = json!("Image");
        let invalid = error(args);
        assert_eq!(
            invalid.path,
            "/messages/1/updateComponents/components/2/component"
        );
        assert!(invalid.message.contains("not in Keystone's catalog"));
    }

    #[test]
    fn bad_pointers_and_mismatched_types_are_validation_errors() {
        let mut args = examples::checklist();
        args["messages"][1]["updateComponents"]["components"][4]["value"] =
            json!({"path": "items/docs"});
        assert_eq!(
            error(args).path,
            "/messages/1/updateComponents/components/4/value/path"
        );

        let mut args = examples::checklist();
        args["messages"][1]["updateComponents"]["components"][4]["value"] =
            json!({"path": "/items/docs/x"});
        let invalid = error(args);
        assert!(
            invalid.message.contains("not an object or array"),
            "{}",
            invalid.message
        );

        let mut args = examples::checklist();
        args["messages"][2]["updateDataModel"]["value"]["docs"] = json!("yes");
        let invalid = error(args);
        assert_eq!(
            invalid.path,
            "/messages/1/updateComponents/components/4/value/path"
        );
        assert!(
            invalid.message.contains("does not fit"),
            "{}",
            invalid.message
        );

        let mut args = examples::checklist();
        args["messages"][1]["updateComponents"]["components"][2]["text"] =
            json!({"path": "/missing"});
        assert!(error(args).message.contains("not in the data model"));

        let mut args = examples::checklist();
        args["messages"][1]["updateComponents"]["components"][4]["value"] =
            json!({"path": "/a~2b"});
        assert!(error(args).message.contains("~"));
    }

    #[test]
    fn the_catalog_and_surface_rules_are_enforced() {
        let mut args = examples::checklist();
        args["messages"][0]["createSurface"]["catalogId"] =
            json!("https://a2ui.org/specification/v0_9/catalogs/basic/catalog.json");
        assert_eq!(error(args).path, "/messages/0/createSurface/catalogId");

        let mut args = examples::checklist();
        args["messages"][1]["updateComponents"]["surfaceId"] = json!("other");
        assert!(error(args).message.contains("Unknown surfaceId"));

        let mut args = examples::checklist();
        args["messages"].as_array_mut().unwrap().swap(0, 1);
        assert!(error(args).message.contains("first message"));

        let mut args = examples::checklist();
        args["messages"][1]["updateComponents"]["components"][1]["children"] =
            json!(["title", "title"]);
        assert!(error(args).message.contains("more than once"));

        let mut args = examples::checklist();
        args["messages"][1]["updateComponents"]["components"][1]["children"] =
            json!({"componentId": "x", "path": "/items"});
        assert!(error(args).message.contains("templates"));

        let mut args = examples::checklist();
        args["messages"][1]["updateComponents"]["components"][11]["action"] =
            json!({"functionCall": {"call": "openUrl"}});
        assert!(error(args).message.contains("functionCall"));

        let mut args = examples::form();
        args["messages"][1]["updateComponents"]["components"][3]["variant"] = json!("obscured");
        assert!(error(args).message.contains("secrets"));

        let mut args = examples::form();
        args["messages"][1]["updateComponents"]["components"][3]["value"] = json!("typed");
        assert!(error(args).message.contains("Bind `value`"));

        let mut args = examples::form();
        args["messages"][2]["updateDataModel"]["value"]["confidence"] = json!(11);
        assert!(error(args).message.contains("outside"));

        let mut args = examples::checklist();
        args["messages"][1]["updateComponents"]["components"][4]["checks"] = json!([]);
        assert!(error(args).message.contains("checks"));

        let mut args = examples::checklist();
        args["messages"][1]["updateComponents"]["components"]
            .as_array_mut()
            .unwrap()
            .remove(0);
        assert!(error(args).message.contains("root"));

        assert!(error(json!({"messages": []})).message.contains("1 to"));
        assert!(
            error(json!({"message": []}))
                .message
                .contains("Unknown argument")
        );
    }

    #[test]
    fn a_surface_without_a_button_does_not_wait() {
        let mut args = examples::checklist();
        let components = args["messages"][1]["updateComponents"]["components"]
            .as_array_mut()
            .unwrap();
        components.truncate(8);
        components[1]["children"] =
            json!(["title", "hint", "docs", "tests", "changelog", "divider"]);
        let surface = parse("c", &args).unwrap();
        assert_eq!(surface.status, Status::Shown);
        assert!(!surface.waiting());
    }

    #[test]
    fn json_pointers_follow_rfc_6901() {
        let mut data = json!({"a/b": {"m~n": [1, 2]}});
        assert_eq!(get(&data, "/a~1b/m~0n/1"), Lookup::Found(&json!(2)));
        assert_eq!(
            get(&data, "/a~1b/m~0n/01"),
            Lookup::Blocked("/a~1b/m~0n/01: \"01\" is not an array index".into())
        );
        assert_eq!(get(&data, "/x"), Lookup::Missing);
        set(&mut data, "/a~1b/m~0n/-", json!(3)).unwrap();
        set(&mut data, "/new/deep", json!(true)).unwrap();
        assert_eq!(
            data,
            json!({"a/b": {"m~n": [1, 2, 3]}, "new": {"deep": true}})
        );
        remove(&mut data, "/new").unwrap();
        assert!(set(&mut data, "/a~1b/m~0n/0/x", json!(1)).is_err());
    }

    /// Deep pointers and models are refused before they are built, so every surface
    /// that parses can be saved and restored.
    #[test]
    fn deep_pointers_and_data_models_are_refused() {
        let deep = "/a".repeat(MAX_DATA_DEPTH + 1);
        let mut args = examples::checklist();
        args["messages"][2]["updateDataModel"]["path"] = json!(deep);
        let error = parse("c", &args).unwrap_err();
        assert!(error.message.contains("levels deep"), "{error:?}");

        let mut nested = json!(true);
        for _ in 0..MAX_DATA_DEPTH {
            nested = json!({ "n": nested });
        }
        let mut args = examples::checklist();
        args["messages"][2]["updateDataModel"]["value"]["extra"] = nested;
        let error = parse("c", &args).unwrap_err();
        assert!(error.message.contains("levels deep"), "{error:?}");
        assert!(parse("c", &examples::checklist()).is_ok());
    }

    /// A Button's context is bounded, so a press cannot amplify the data model.
    #[test]
    fn a_context_holds_a_bounded_number_of_entries() {
        let mut args = examples::checklist();
        let context: Map<String, Value> = (0..=MAX_CONTEXT)
            .map(|i| (format!("k{i}"), json!({"path": "/items"})))
            .collect();
        args["messages"][1]["updateComponents"]["components"][11]["action"]["event"]["context"] =
            Value::Object(context);
        let error = parse("c", &args).unwrap_err();
        assert!(error.message.contains("at most"), "{error:?}");
    }

    /// What the user types is bounded like what the agent sends; a refused edit
    /// changes nothing.
    #[test]
    fn an_edit_that_overflows_the_data_model_is_refused_and_changes_nothing() {
        let mut surface = parse("call", &examples::form()).unwrap();
        surface.set_value("/summary", json!("short")).unwrap();
        let error = surface
            .set_value("/summary", json!("x".repeat(MAX_DATA_BYTES)))
            .unwrap_err();
        assert!(error.contains("larger than"), "{error}");
        assert_eq!(surface.value("/summary"), Some(&json!("short")));
    }

    /// A Slider's range is one the renderer can show and return exactly.
    #[test]
    fn slider_ranges_the_renderer_cannot_hold_are_refused() {
        for (min, max) in [(1e100, 1e101), (0.0, 1e7), (1e6, 1e6 - 0.01)] {
            let mut args = examples::form();
            let components = args["messages"][1]["updateComponents"]["components"]
                .as_array_mut()
                .unwrap();
            let slider = components
                .iter_mut()
                .find(|c| c["component"] == "Slider")
                .unwrap();
            slider["min"] = json!(min);
            slider["max"] = json!(max);
            assert!(parse("c", &args).is_err(), "{min}..{max}");
        }
        assert!(parse("c", &examples::form()).is_ok());
    }

    #[test]
    fn surfaces_round_trip_through_serde() {
        let surface = parse("call", &examples::form()).unwrap();
        let text = serde_json::to_string(&surface).unwrap();
        assert_eq!(serde_json::from_str::<Surface>(&text).unwrap(), surface);
    }

    #[test]
    fn the_description_example_is_valid() {
        let description = description();
        let start = description.find("Example checklist: ").unwrap() + "Example checklist: ".len();
        let messages: Value = serde_json::from_str(&description[start..]).unwrap();
        let surface = parse("c", &json!({"messages": messages})).unwrap();
        assert_eq!(surface.surface_id, "todo");
        assert!(surface.waiting());
    }
}
