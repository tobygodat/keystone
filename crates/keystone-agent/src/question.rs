//! Questions a provider asks the user: Claude's `AskUserQuestion`, Codex's
//! `request_user_input`, and MCP form elicitations from either. Each driver turns its
//! request into one neutral [`Question`] and keeps what it needs to write the answer
//! back in its own wire format. See "Questions" in `docs/design/providers.md`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// The provider waits for the user to answer these fields, or to decline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub request: crate::RequestId,
    /// The MCP server that asks, or `None` when the agent itself asks.
    pub server: Option<String>,
    /// What the asker says before its fields.
    pub message: Option<String>,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Field {
    /// A short label, such as Claude's header or an MCP property's title.
    pub header: String,
    /// The full question, when it says more than the header.
    pub prompt: Option<String>,
    pub input: Input,
    /// The answer cannot go without this field.
    pub required: bool,
    /// What an answer given must keep to, as an MCP form's schema sets it.
    pub bounds: Bounds,
}

/// Limits on an answer: a number's value, text's length in characters, or how many
/// options a multiple choice takes. A field left empty is not held to them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

impl Bounds {
    fn read(schema: &Value, min: &str, max: &str) -> Self {
        Self {
            min: schema.get(min).and_then(Value::as_f64),
            max: schema.get(max).and_then(Value::as_f64),
        }
    }

    fn holds(self, value: f64) -> bool {
        self.min.is_none_or(|min| value >= min) && self.max.is_none_or(|max| value <= max)
    }

    /// "from 1 to 5", "at least 1" or "at most 5", with `unit` after each number.
    fn describe(self, unit: &str) -> String {
        let n = |value: f64| format!("{value}{unit}");
        match (self.min, self.max) {
            (Some(min), Some(max)) => format!("from {} to {}", n(min), n(max)),
            (Some(min), None) => format!("at least {}", n(min)),
            (None, Some(max)) => format!("at most {}", n(max)),
            (None, None) => String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Input {
    /// Choose one option, or any number when `multiple`. With `other`, typed text
    /// answers too.
    Choice {
        options: Vec<Choice>,
        multiple: bool,
        other: bool,
    },
    Text,
    Number {
        integer: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub label: String,
    pub description: Option<String>,
}

/// The user's answer to one field: the options chosen, by index, and typed text.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub chosen: Vec<usize>,
    pub text: String,
}

impl Field {
    fn typed<'a>(&self, answer: &'a Answer) -> Option<&'a str> {
        let text = answer.text.trim();
        let takes_text = match &self.input {
            Input::Choice { other, .. } => *other,
            Input::Text | Input::Number { .. } => true,
        };
        (takes_text && !text.is_empty()).then_some(text)
    }

    /// The answer's chosen labels in option order, then its typed text. Typed text
    /// answers a single choice in place of any option chosen.
    pub fn values(&self, answer: &Answer) -> Vec<String> {
        if let (
            Input::Choice {
                multiple: false, ..
            },
            Some(text),
        ) = (&self.input, self.typed(answer))
        {
            return vec![text.to_owned()];
        }
        let mut values: Vec<String> = match &self.input {
            Input::Choice { options, .. } => options
                .iter()
                .enumerate()
                .filter(|(index, _)| answer.chosen.contains(index))
                .map(|(_, option)| option.label.clone())
                .collect(),
            _ => Vec::new(),
        };
        values.extend(self.typed(answer).map(str::to_owned));
        values
    }

    /// Why `answer` cannot be sent for this field, if it cannot.
    pub fn problem(&self, answer: &Answer) -> Option<String> {
        let header = &self.header;
        let values = self.values(answer);
        if values.is_empty() {
            return self.required.then(|| format!("Answer {header}."));
        }
        let typed = self.typed(answer);
        match (&self.input, typed) {
            (Input::Number { integer }, Some(text)) => {
                let number = if *integer {
                    text.parse::<i64>().ok().map(|n| n as f64)
                } else {
                    text.parse::<f64>().ok().filter(|n| n.is_finite())
                };
                let kind = if *integer {
                    "a whole number"
                } else {
                    "a number"
                };
                match number {
                    None => Some(format!("{header} needs {kind}.")),
                    Some(n) if !self.bounds.holds(n) => Some(format!(
                        "{header} needs {kind} {}.",
                        self.bounds.describe("")
                    )),
                    Some(_) => None,
                }
            }
            (Input::Text, Some(text)) => (!self.bounds.holds(text.chars().count() as f64))
                .then(|| format!("{header} needs {} characters.", self.bounds.describe(""))),
            (Input::Choice { multiple: true, .. }, _) => (!self.bounds.holds(values.len() as f64))
                .then(|| format!("{header}: choose {}.", self.bounds.describe(""))),
            _ => None,
        }
    }
}

impl Question {
    /// Why these answers cannot be sent, naming the first field that needs work.
    pub fn problem(&self, answers: &[Answer]) -> Option<String> {
        let blank = Answer::default();
        self.fields
            .iter()
            .enumerate()
            .find_map(|(index, field)| field.problem(answers.get(index).unwrap_or(&blank)))
    }
}

/// How an MCP form property's answer is written back.
#[derive(Clone, Debug, PartialEq)]
enum Kind {
    One(Vec<Value>),
    Many(Vec<Value>),
    Flag,
    Text,
    Number { integer: bool },
}

/// An MCP form (`requestedSchema`) read as fields, with the property each one answers.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Form {
    pub fields: Vec<Field>,
    /// Each field's property, how its answer is written, and whether the schema
    /// requires it, so it is sent even when empty.
    properties: Vec<(String, Kind, bool)>,
}

impl Form {
    /// Reads a flat object schema of strings, numbers, booleans, and enums. `order`
    /// lists the properties in the order the server declared them, when the provider
    /// says. Anything else is refused with the reason, since Keystone could not show it.
    pub(crate) fn read(schema: &Value, order: Option<&[String]>) -> Result<Self, String> {
        let empty = Map::new();
        let properties = match schema.get("properties") {
            Some(Value::Object(properties)) => properties,
            None | Some(Value::Null) => &empty,
            Some(_) => return Err("its form has no readable fields".into()),
        };
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|keys| keys.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut keys: Vec<&String> = order
            .unwrap_or_default()
            .iter()
            .filter(|key| properties.contains_key(*key))
            .collect();
        // Sorted so undeclared fields read alphabetically whether or not serde_json
        // keeps insertion order, which GPUI turns on in a workspace build.
        let mut rest: Vec<&String> = properties
            .keys()
            .filter(|key| !keys.contains(key))
            .collect();
        rest.sort();
        keys.extend(rest);
        let mut form = Form {
            fields: Vec::new(),
            properties: Vec::new(),
        };
        for key in keys {
            let property = &properties[key];
            let text = |name: &str| {
                property
                    .get(name)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned)
            };
            let (input, kind) = property_input(property)
                .ok_or_else(|| format!("its field {key} has a type Keystone can't show"))?;
            let bounds = match &kind {
                Kind::Number { .. } => Bounds::read(property, "minimum", "maximum"),
                Kind::Text => Bounds::read(property, "minLength", "maxLength"),
                Kind::Many(_) => Bounds::read(property, "minItems", "maxItems"),
                Kind::One(_) | Kind::Flag => Bounds::default(),
            };
            let present = required.contains(&key.as_str());
            // An empty list is a valid answer to a required list unless it sets
            // `minItems`, so only then must the user choose.
            let needed = match &kind {
                Kind::Many(_) => present && bounds.min.is_some_and(|min| min >= 1.0),
                _ => present,
            };
            form.fields.push(Field {
                header: text("title").unwrap_or_else(|| key.clone()),
                prompt: text("description"),
                input,
                required: needed,
                bounds,
            });
            form.properties.push((key.clone(), kind, present));
        }
        Ok(form)
    }

    /// The `content` of an accepting answer: each answered property, in its type.
    pub(crate) fn content(&self, answers: &[Answer]) -> Value {
        let blank = Answer::default();
        let mut content = Map::new();
        for (index, (field, (key, kind, present))) in
            self.fields.iter().zip(&self.properties).enumerate()
        {
            let answer = answers.get(index).unwrap_or(&blank);
            let chosen = |values: &[Value]| -> Vec<Value> {
                (0..values.len())
                    .filter(|index| answer.chosen.contains(index))
                    .map(|index| values[index].clone())
                    .collect()
            };
            let typed = field.typed(answer);
            let value = match kind {
                Kind::One(values) => chosen(values).into_iter().next(),
                Kind::Many(values) => Some(Value::Array(chosen(values))),
                Kind::Flag => answer.chosen.first().map(|&index| json!(index == 0)),
                Kind::Text => typed.map(|text| json!(text)),
                Kind::Number { integer: true } => typed
                    .and_then(|text| text.parse::<i64>().ok())
                    .map(|n| json!(n)),
                Kind::Number { integer: false } => typed
                    .and_then(|text| text.parse::<f64>().ok())
                    .map(|n| json!(n)),
            };
            if let Some(value) = value.filter(|value| value != &json!([]) || *present) {
                content.insert(key.clone(), value);
            }
        }
        Value::Object(content)
    }
}

/// An enum's options: `oneOf`/`anyOf` entries with `const` and `title`, or `enum`
/// with optional `enumNames`.
fn options(schema: &Value) -> Option<(Vec<Choice>, Vec<Value>)> {
    let titled = schema
        .get("oneOf")
        .or_else(|| schema.get("anyOf"))
        .and_then(Value::as_array);
    let pairs: Vec<(String, Value)> = if let Some(entries) = titled {
        entries
            .iter()
            .map(|entry| {
                let value = entry.get("const")?.clone();
                let label = entry
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| plain(&value));
                Some((label, value))
            })
            .collect::<Option<_>>()?
    } else {
        let values = schema.get("enum")?.as_array()?;
        let names = schema.get("enumNames").and_then(Value::as_array);
        values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let name = names
                    .and_then(|names| names.get(index))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                (name.unwrap_or_else(|| plain(value)), value.clone())
            })
            .collect()
    };
    if pairs.is_empty() {
        return None;
    }
    let (labels, values) = pairs.into_iter().unzip::<_, _, Vec<_>, Vec<_>>();
    let choices = labels
        .into_iter()
        .map(|label| Choice {
            label,
            description: None,
        })
        .collect();
    Some((choices, values))
}

fn plain(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

fn property_input(property: &Value) -> Option<(Input, Kind)> {
    let kind = property.get("type").and_then(Value::as_str);
    if let Some((options, values)) = options(property).filter(|_| kind != Some("array")) {
        let input = Input::Choice {
            options,
            multiple: false,
            other: false,
        };
        return Some((input, Kind::One(values)));
    }
    match kind? {
        "string" => Some((Input::Text, Kind::Text)),
        "number" | "integer" => {
            let integer = kind == Some("integer");
            Some((Input::Number { integer }, Kind::Number { integer }))
        }
        "boolean" => {
            let choice = |label: &str| Choice {
                label: label.into(),
                description: None,
            };
            let input = Input::Choice {
                options: vec![choice("Yes"), choice("No")],
                multiple: false,
                other: false,
            };
            Some((input, Kind::Flag))
        }
        "array" => {
            let (options, values) = options(property.get("items")?)?;
            let input = Input::Choice {
                options,
                multiple: true,
                other: false,
            };
            Some((input, Kind::Many(values)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Value {
        json!({"type": "object", "properties": {
            "size": {"type": "string", "title": "Size", "description": "Which size to use", "enum": ["small", "large"], "enumNames": ["Small", "Large"]},
            "toppings": {"type": "array", "title": "Toppings", "items": {"type": "string", "enum": ["cheese", "olives", "basil"]}},
            "note": {"type": "string", "title": "Note", "description": "Anything else"},
            "rush": {"type": "boolean", "title": "Rush order"},
            "count": {"type": "integer"}
        }, "required": ["size"]})
    }

    fn answer(chosen: &[usize], text: &str) -> Answer {
        Answer {
            chosen: chosen.to_vec(),
            text: text.into(),
        }
    }

    #[test]
    fn a_form_keeps_the_declared_order_and_writes_each_type_back() {
        let order: Vec<String> = ["size", "toppings", "note", "rush"]
            .map(String::from)
            .to_vec();
        let form = Form::read(&plan(), Some(&order)).unwrap();
        let headers: Vec<_> = form.fields.iter().map(|f| f.header.as_str()).collect();
        assert_eq!(headers, ["Size", "Toppings", "Note", "Rush order", "count"]);
        assert!(form.fields[0].required && !form.fields[1].required);
        let Input::Choice { options, .. } = &form.fields[0].input else {
            panic!("an enum is a choice");
        };
        assert_eq!(options[1].label, "Large", "enumNames label the options");
        let answers = [
            answer(&[1], ""),
            answer(&[0, 2], ""),
            answer(&[], " extra crisp "),
            answer(&[0], ""),
            answer(&[], "3"),
        ];
        assert_eq!(
            form.content(&answers),
            json!({"size": "large", "toppings": ["cheese", "basil"], "note": "extra crisp", "rush": true, "count": 3})
        );
    }

    #[test]
    fn unanswered_optional_fields_are_left_out_and_required_ones_block() {
        let form = Form::read(&plan(), None).unwrap();
        let question = Question {
            request: crate::RequestId("1".into()),
            server: Some("probe".into()),
            message: None,
            fields: form.fields.clone(),
        };
        let size = form.fields.iter().position(|f| f.header == "Size").unwrap();
        let count = form
            .fields
            .iter()
            .position(|f| f.header == "count")
            .unwrap();
        let mut answers = vec![Answer::default(); form.fields.len()];
        assert_eq!(question.problem(&answers).as_deref(), Some("Answer Size."));
        answers[size] = answer(&[0], "");
        answers[count] = answer(&[], "2.5");
        assert_eq!(
            question.problem(&answers).as_deref(),
            Some("count needs a whole number.")
        );
        answers[count] = Answer::default();
        assert_eq!(question.problem(&answers), None);
        assert_eq!(form.content(&answers), json!({"size": "small"}));
    }

    #[test]
    fn typed_text_answers_a_choice_only_when_it_offers_other() {
        let field = |other| Field {
            header: "Color".into(),
            prompt: None,
            input: Input::Choice {
                options: vec![Choice {
                    label: "Red".into(),
                    description: None,
                }],
                multiple: false,
                other,
            },
            required: true,
            bounds: Bounds::default(),
        };
        assert_eq!(field(true).values(&answer(&[], "Teal")), ["Teal"]);
        assert_eq!(
            field(true).values(&answer(&[0], "Teal")),
            ["Teal"],
            "typed text replaces a single choice"
        );
        assert!(field(false).values(&answer(&[], "Teal")).is_empty());
        assert_eq!(
            field(false).problem(&answer(&[], "Teal")).as_deref(),
            Some("Answer Color.")
        );
    }

    #[test]
    fn a_forms_bounds_hold_answers_given_and_a_required_list_may_stay_empty() {
        let schema = json!({"properties": {
            "count": {"type": "integer", "title": "Count", "minimum": 1, "maximum": 5},
            "code": {"type": "string", "title": "Code", "maxLength": 3},
            "tags": {"type": "array", "title": "Tags", "maxItems": 1, "items": {"enum": ["a", "b"]}},
            "picks": {"type": "array", "title": "Picks", "minItems": 1, "items": {"enum": ["x", "y"]}}
        }, "required": ["tags", "picks"]});
        let order: Vec<String> = ["count", "code", "tags", "picks"]
            .map(String::from)
            .to_vec();
        let form = Form::read(&schema, Some(&order)).unwrap();
        let question = Question {
            request: crate::RequestId("1".into()),
            server: None,
            message: None,
            fields: form.fields.clone(),
        };
        let mut answers = vec![Answer::default(); 4];
        assert!(!form.fields[2].required, "an empty list answers it");
        assert_eq!(question.problem(&answers).as_deref(), Some("Answer Picks."));
        answers[3] = answer(&[1], "");
        assert_eq!(question.problem(&answers), None);
        assert_eq!(
            form.content(&answers),
            json!({"tags": [], "picks": ["y"]}),
            "a required list goes even when empty; optional fields left empty do not"
        );
        answers[0] = answer(&[], "100");
        assert_eq!(
            question.problem(&answers).as_deref(),
            Some("Count needs a whole number from 1 to 5.")
        );
        answers[0] = answer(&[], "5");
        answers[1] = answer(&[], "abcd");
        assert_eq!(
            question.problem(&answers).as_deref(),
            Some("Code needs at most 3 characters.")
        );
        answers[1] = answer(&[], "abc");
        answers[2] = answer(&[0, 1], "");
        assert_eq!(
            question.problem(&answers).as_deref(),
            Some("Tags: choose at most 1.")
        );
    }

    #[test]
    fn titled_enums_and_unreadable_forms() {
        let titled = json!({"properties": {
            "tier": {"type": "string", "oneOf": [{"const": "a", "title": "Alpha"}, {"const": "b", "title": "Beta"}]},
            "tags": {"type": "array", "items": {"anyOf": [{"const": "x", "title": "Ex"}]}}
        }});
        let form = Form::read(&titled, None).unwrap();
        assert_eq!(
            form.content(&[answer(&[0], ""), answer(&[1], "")]),
            json!({"tags": ["x"], "tier": "b"}),
            "properties without a declared order read alphabetically"
        );
        let nested = json!({"properties": {"address": {"type": "object"}}});
        assert_eq!(
            Form::read(&nested, None).unwrap_err(),
            "its field address has a type Keystone can't show"
        );
        assert!(
            Form::read(&json!({"type": "object"}), None)
                .unwrap()
                .fields
                .is_empty()
        );
    }
}
