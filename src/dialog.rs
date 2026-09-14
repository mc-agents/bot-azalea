//! The dialog the server has open, and what a vanilla client keeps of it while it is up.
//!
//! A vanilla client builds a screen out of a dialog, and from then on the screen's controls hold
//! the inputs' values and its buttons know what to send. There is no screen here, so this holds
//! both halves: the definition as the server sent it, and what each input holds, starting from the
//! values the definition gives. What a button sends is built from these the way the client builds
//! it from its controls, because a server reads a template's `$(key)` and cannot tell the two apart.

use std::collections::HashMap;

use azalea::FormattedText;
use serde_json::{Map, Value, json};
use simdnbt::owned::{NbtCompound, NbtList, NbtTag};

pub struct Open {
    pub definition: Value,
    held: HashMap<String, Held>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Held {
    Text(String),
    Checkbox(bool),
    Choice(String),
    Slider(f32),
}

pub enum Kind {
    Text { max_length: usize, multiline: bool },
    Checkbox { on_true: String, on_false: String },
    Choice { entries: Vec<Entry> },
    Slider(Range),
    Unknown,
}

pub struct Entry {
    pub id: String,
    pub display: String,
}

/// A slider's range, as `NumberRangeInput.RangeInfo` holds it.
#[derive(Clone, Copy)]
pub struct Range {
    pub start: f32,
    pub end: f32,
    pub initial: Option<f32>,
    pub step: Option<f32>,
}

pub struct Input {
    pub key: String,
    pub label: Value,
    pub kind: Kind,
}

pub struct Button {
    pub label: String,
    pub action: Option<Value>,
}

/// What a button's action comes to, once its inputs are read into it.
pub enum Action {
    Command(String),
    Custom { id: String, payload: Option<NbtTag> },
    /// Something the client does on its own screen -- a link, the clipboard, another dialog -- and
    /// sends the server nothing for.
    Local,
}

impl Open {
    pub fn new(definition: Value) -> Open {
        let mut open = Open { definition, held: HashMap::new() };
        for input in open.inputs() {
            let start = match &input.kind {
                Kind::Text { .. } => Held::Text(open.field(&input.key, "initial").and_then(Value::as_str).unwrap_or_default().to_owned()),
                Kind::Checkbox { .. } => Held::Checkbox(truthy(open.field(&input.key, "initial"))),
                Kind::Choice { entries } => {
                    let chosen = open.options(&input.key).find(|(_, initial)| *initial).map(|(entry, _)| entry.id);
                    Held::Choice(chosen.or_else(|| entries.first().map(|entry| entry.id.clone())).unwrap_or_default())
                }
                Kind::Slider(range) => Held::Slider(range.scaled(range.initial_slider())),
                Kind::Unknown => continue,
            };
            open.held.insert(input.key, start);
        }
        open
    }

    pub fn title(&self) -> (String, Value) {
        let component = self.definition.get("title").cloned().unwrap_or(Value::Null);
        (flatten(&component), component)
    }

    /// The name the client's screen class has for this kind of dialog, which is how the other kind of
    /// bot names the screen in a sentence.
    pub fn screen(&self) -> &'static str {
        match kind(&self.definition) {
            "multi_action" => "MultiButtonDialogScreen",
            "dialog_list" => "DialogListDialogScreen",
            "server_links" => "ServerLinksDialogScreen",
            _ => "SimpleDialogScreen",
        }
    }

    pub fn closes_on_escape(&self) -> bool {
        self.definition.get("can_close_with_escape").is_none_or(|value| truthy(Some(value)))
    }

    /// What the client does with its screen once a button has run: closes it unless told otherwise.
    pub fn stays_open_after(&self) -> bool {
        self.definition.get("after_action").and_then(Value::as_str).is_some_and(|after| after.ends_with("none"))
    }

    pub fn inputs(&self) -> Vec<Input> {
        each(self.definition.get("inputs"))
            .into_iter()
            .map(|input| {
                let key = input.get("key").and_then(Value::as_str).unwrap_or_default().to_owned();
                let label = input.get("label").cloned().unwrap_or(Value::Null);
                let kind = match kind(input) {
                    "text" => Kind::Text {
                        max_length: input.get("max_length").and_then(Value::as_u64).unwrap_or(32) as usize,
                        multiline: input.get("multiline").is_some(),
                    },
                    "boolean" => Kind::Checkbox {
                        on_true: input.get("on_true").and_then(Value::as_str).unwrap_or("true").to_owned(),
                        on_false: input.get("on_false").and_then(Value::as_str).unwrap_or("false").to_owned(),
                    },
                    "single_option" => Kind::Choice { entries: entries(input).map(|(entry, _)| entry).collect() },
                    "number_range" => Kind::Slider(Range {
                        start: float(input.get("start")).unwrap_or(0.0),
                        end: float(input.get("end")).unwrap_or(0.0),
                        initial: float(input.get("initial")),
                        step: float(input.get("step")),
                    }),
                    _ => Kind::Unknown,
                };
                Input { key, label, kind }
            })
            .collect()
    }

    pub fn held(&self, key: &str) -> Option<&Held> {
        self.held.get(key)
    }

    pub fn hold(&mut self, key: &str, value: Held) {
        self.held.insert(key.to_owned(), value);
    }

    /// Every input and what it holds, the way the dialog feed carries it.
    pub fn values(&self) -> Value {
        let values: Map<String, Value> = self
            .held
            .iter()
            .map(|(key, held)| {
                let value = match held {
                    Held::Text(text) | Held::Choice(text) => json!(text),
                    Held::Checkbox(ticked) => json!(ticked),
                    Held::Slider(number) => json!(number),
                };
                (key.clone(), value)
            })
            .collect();
        Value::Object(values)
    }

    /// The buttons in the order the client lays them out, the exit button last.
    pub fn buttons(&self) -> Vec<Button> {
        ["actions", "yes", "no", "action", "exit_action"]
            .iter()
            .flat_map(|field| each(self.definition.get(*field)))
            .map(|button| Button {
                label: flatten(button.get("label").unwrap_or(&Value::Null)),
                action: button.get("action").cloned(),
            })
            .collect()
    }

    pub fn exit(&self) -> Option<Button> {
        each(self.definition.get("exit_action")).first().map(|button| Button {
            label: flatten(button.get("label").unwrap_or(&Value::Null)),
            action: button.get("action").cloned(),
        })
    }

    /// What pressing a button sends, read from the inputs as they are held now.
    pub fn action(&self, action: &Value) -> Action {
        match kind(action) {
            "run_command" => Action::Command(action.get("command").and_then(Value::as_str).unwrap_or_default().to_owned()),
            "dynamic/run_command" => {
                Action::Command(self.substitute(action.get("template").and_then(Value::as_str).unwrap_or_default()))
            }
            "custom" => Action::Custom {
                id: action.get("id").and_then(Value::as_str).unwrap_or_default().to_owned(),
                payload: action.get("payload").map(tag),
            },
            "dynamic/custom" => {
                /* The additions first, and each input under its key over them, as the client builds it. */
                let mut payload = match action.get("additions").map(tag) {
                    Some(NbtTag::Compound(additions)) => additions,
                    _ => NbtCompound::new(),
                };
                for input in self.inputs() {
                    let value = match self.held.get(&input.key) {
                        Some(Held::Text(text) | Held::Choice(text)) => NbtTag::String(text.as_str().into()),
                        Some(Held::Checkbox(ticked)) => NbtTag::Byte(i8::from(*ticked)),
                        Some(Held::Slider(number)) => NbtTag::Float(*number),
                        None => continue,
                    };
                    payload.insert(input.key.as_str(), value);
                }
                Action::Custom {
                    id: action.get("id").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    payload: Some(NbtTag::Compound(payload)),
                }
            }
            _ => Action::Local,
        }
    }

    /// `$(key)` replaced by what the input holds, in the form the client substitutes it.
    fn substitute(&self, template: &str) -> String {
        let mut command = template.to_owned();
        for input in self.inputs() {
            let Some(held) = self.held.get(&input.key) else { continue };
            let value = match (held, &input.kind) {
                (Held::Checkbox(ticked), Kind::Checkbox { on_true, on_false }) => {
                    if *ticked { on_true.clone() } else { on_false.clone() }
                }
                (Held::Text(text) | Held::Choice(text), _) => text.clone(),
                (Held::Slider(number), _) => slider_text(*number),
                _ => continue,
            };
            command = command.replace(&format!("$({})", input.key), &value);
        }
        command
    }

    fn field(&self, key: &str, field: &str) -> Option<&Value> {
        each(self.definition.get("inputs"))
            .into_iter()
            .find(|input| input.get("key").and_then(Value::as_str) == Some(key))
            .and_then(|input| input.get(field))
    }

    fn options(&self, key: &str) -> impl Iterator<Item = (Entry, bool)> + '_ {
        each(self.definition.get("inputs"))
            .into_iter()
            .find(|input| input.get("key").and_then(Value::as_str) == Some(key))
            .into_iter()
            .flat_map(entries)
    }
}

impl Range {
    /// Where the handle starts: at the initial value, or halfway when there is none.
    fn initial_slider(&self) -> f32 {
        self.to_slider(self.initial.unwrap_or((self.start + self.end) / 2.0))
    }

    pub fn to_slider(&self, value: f32) -> f32 {
        if self.start == self.end { 0.5 } else { (value - self.start) / (self.end - self.start) }
    }

    /// `RangeInfo.computeScaledValue`: the handle's place as a number, moved onto a step counted
    /// from the initial value, and one step back towards it when that step leaves the range.
    pub fn scaled(&self, slider: f32) -> f32 {
        let value = self.start + slider * (self.end - self.start);
        let Some(step) = self.step else { return value };

        let initial = self.initial.unwrap_or((self.start + self.end) / 2.0);
        let steps = java_round((value - initial) / step);
        let stepped = initial + steps as f32 * step;
        let slider = self.to_slider(stepped);

        if (0.0..=1.0).contains(&slider) {
            stepped
        } else {
            initial + (steps - steps.signum()) as f32 * step
        }
    }
}

/// `Math.round(float)`: half up, towards positive infinity.
fn java_round(value: f32) -> i32 {
    (value + 0.5).floor() as i32
}

/// A slider's number as the client writes it, into a template or a sentence: whole numbers without a
/// decimal point, the rest as Java's `Float.toString`.
pub fn slider_text(value: f32) -> String {
    if value == value.trunc() { format!("{}", value as i32) } else { format!("{value:?}") }
}

/// A component as the words it reads, whether the definition wrote it as a string or as JSON.
pub fn flatten(component: &Value) -> String {
    match component {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => serde_json::from_value::<FormattedText>(other.clone()).map(|text| text.to_string()).unwrap_or_default(),
    }
}

fn kind(node: &Value) -> &str {
    let kind = node.get("type").and_then(Value::as_str).unwrap_or_default();
    kind.strip_prefix("minecraft:").unwrap_or(kind)
}

fn entries(input: &Value) -> impl Iterator<Item = (Entry, bool)> + '_ {
    each(input.get("options")).into_iter().map(|option| match option {
        Value::String(id) => (Entry { id: id.clone(), display: id.clone() }, false),
        _ => {
            let id = option.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let display = option.get("display").map(flatten).unwrap_or_else(|| id.clone());
            (Entry { id, display }, truthy(option.get("initial")))
        }
    })
}

/// NBT writes a list of one as a bare compound, and a boolean as a byte.
fn each(node: Option<&Value>) -> Vec<&Value> {
    match node {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(one) => vec![one],
    }
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        _ => false,
    }
}

fn float(value: Option<&Value>) -> Option<f32> {
    value.and_then(Value::as_f64).map(|number| number as f32)
}

/// A definition's own NBT, back from the JSON it was read into. Whole numbers go back as ints and
/// the rest as doubles: the JSON has lost which they were, and a payload a server reads as a number
/// reads either.
fn tag(value: &Value) -> NbtTag {
    match value {
        Value::Bool(flag) => NbtTag::Byte(i8::from(*flag)),
        Value::Number(number) => match number.as_i64() {
            Some(whole) if i32::try_from(whole).is_ok() => NbtTag::Int(whole as i32),
            Some(whole) => NbtTag::Long(whole),
            None => NbtTag::Double(number.as_f64().unwrap_or_default()),
        },
        Value::String(text) => NbtTag::String(text.as_str().into()),
        Value::Array(items) => NbtTag::List(NbtList::Compound(
            items
                .iter()
                .map(|item| match tag(item) {
                    NbtTag::Compound(compound) => compound,
                    other => NbtCompound::from_values(vec![("".into(), other)]),
                })
                .collect(),
        )),
        Value::Object(fields) => {
            let mut compound = NbtCompound::new();
            for (name, field) in fields {
                compound.insert(name.as_str(), tag(field));
            }
            NbtTag::Compound(compound)
        }
        Value::Null => NbtTag::Compound(NbtCompound::new()),
    }
}
