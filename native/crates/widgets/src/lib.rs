//! Jupyter widgets as a notebook stores them, resolved into a tree a viewer
//! can draw without a kernel or a browser.
//!
//! A widget output holds only a model id
//! (`application/vnd.jupyter.widget-view+json`); the models live in the
//! notebook metadata (`widgets` → `application/vnd.jupyter.widget-state+json`)
//! when the notebook was saved with widget state, which Colab, nbclient and
//! JupyterLab's "Save Widget State Automatically" do. Each model's state
//! points at its children, layout and style as `IPY_MODEL_<id>` strings.
//! [`WidgetState::view`] follows those references and turns the
//! @jupyter-widgets/controls models (ipywidgets 7 and 8) and the Output
//! widget into [`Widget`]s. Anything else, such as third-party widgets,
//! comes back as [`Kind::Unsupported`] so the caller can fall back to the
//! output's `text/plain`.

pub mod format;
pub mod html;

use std::collections::HashMap;

use serde_json::{Map, Value};

pub const STATE_MIME: &str = "application/vnd.jupyter.widget-state+json";
pub const VIEW_MIME: &str = "application/vnd.jupyter.widget-view+json";

/// Nesting deeper than this is treated as a cycle.
const MAX_DEPTH: usize = 48;

/// All models of one notebook: those saved in its metadata, plus those a
/// running kernel opened over comms ([`WidgetState::apply_comm`]).
#[derive(Debug, Default, Clone)]
pub struct WidgetState {
    models: HashMap<String, Model>,
    /// Models the kernel closed: their views draw nothing, as in JupyterLab
    /// (tqdm's `leave=False`).
    closed: std::collections::HashSet<String>,
    /// Output widgets capturing a request's outputs (`with out:`), by the
    /// request's msg_id.
    capturing: HashMap<String, String>,
    /// Output widgets that were sent `clear_output(wait=True)`: cleared when
    /// their next output arrives.
    clear_pending: std::collections::HashSet<String>,
    /// Models the running kernel holds (opened over comms this session):
    /// changing these is sent back to it.
    live: std::collections::HashSet<String>,
    /// Keys we changed and sent, by model: the msg_id of the latest send. An
    /// `echo_update` from an older send would undo a newer local value.
    pending: HashMap<String, HashMap<String, String>>,
}

#[derive(Debug, Clone)]
struct Model {
    name: String,
    module: String,
    state: Map<String, Value>,
    /// Binary state (an Image's `value`), keyed by its top-level state key.
    buffers: HashMap<String, Vec<u8>>,
}

/// One drawable widget. `key` numbers the widgets of a resolved tree in
/// pre-order, so a caller can keep per-widget data (laid-out HTML, decoded
/// images) beside the tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Widget {
    pub key: u32,
    /// The model this widget was drawn from.
    pub model_id: String,
    pub kind: Kind,
    /// The label drawn before most controls (after a checkbox; inside a
    /// button, whose label is in `Kind::Button`).
    pub description: String,
    /// From the style model; `None` means the theme's default label width.
    pub description_width: Option<Length>,
    pub layout: Layout,
    /// Greyed out (`disabled` on the model).
    pub disabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Length {
    Px(f32),
    Percent(f32),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layout {
    /// `display: none` or `visibility: hidden`.
    pub hidden: bool,
    pub width: Option<Length>,
    pub height: Option<Length>,
    /// Any border was set (the Output widget's usual `1px solid black`).
    pub border: bool,
    /// `flex` (`"2"`, `"1 1 auto"`): the grow factor in a box.
    pub flex: Option<f32>,
    /// Column count of a GridBox's `grid_template_columns`.
    pub grid_columns: Option<usize>,
}

/// `button_style` / `bar_style`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tone {
    #[default]
    Default,
    Primary,
    Success,
    Info,
    Warning,
    Danger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Row,
    Column,
    Grid,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Box {
        direction: Direction,
        /// `flex_flow: row wrap` (tqdm's bar in ipywidgets 7).
        wrap: bool,
        children: Vec<Widget>,
    },
    Tabs {
        titles: Vec<String>,
        selected: Option<usize>,
        children: Vec<Widget>,
    },
    Accordion {
        titles: Vec<String>,
        selected: Option<usize>,
        children: Vec<Widget>,
    },
    /// A Stack shows only its selected child (absent when none is).
    Stack {
        child: Option<Box<Widget>>,
    },
    Progress {
        /// 0..=1 of the bar filled.
        fraction: f64,
        tone: Tone,
        /// `style.bar_color` (any CSS colour).
        color: Option<String>,
        vertical: bool,
    },
    Slider {
        /// The filled span as fractions of the track; `start` is 0 unless a range.
        start: f64,
        end: f64,
        range: bool,
        readout: Option<String>,
        vertical: bool,
        color: Option<String>,
    },
    Text {
        value: String,
        placeholder: String,
        multiline: bool,
    },
    Checkbox {
        value: bool,
        /// Leave the label column empty so the box lines up with other controls.
        indent: bool,
    },
    ToggleButton {
        label: String,
        value: bool,
        tone: Tone,
        icon: String,
    },
    Valid {
        value: bool,
        readout: String,
    },
    Dropdown {
        options: Vec<String>,
        selected: Option<usize>,
    },
    /// RadioButtons.
    Radio {
        options: Vec<String>,
        selected: Option<usize>,
    },
    /// Select and SelectMultiple: a list box.
    Select {
        options: Vec<String>,
        selected: Vec<usize>,
        rows: usize,
        /// SelectMultiple: `index` is a list.
        multiple: bool,
    },
    ToggleButtons {
        options: Vec<String>,
        selected: Option<usize>,
        tone: Tone,
    },
    Button {
        label: String,
        tone: Tone,
        icon: String,
        color: Option<String>,
    },
    Label {
        text: String,
    },
    /// HTML and HTMLMath; see [`html::as_plain_text`].
    Html {
        html: String,
        math: bool,
    },
    Image {
        /// `png`, `jpeg`, `gif`, `svg+xml`, ...
        format: String,
        data: Vec<u8>,
        width: Option<f32>,
        height: Option<f32>,
    },
    /// The Output widget: nbformat outputs, drawn like a cell's.
    Output {
        outputs: Vec<Value>,
    },
    ColorPicker {
        value: String,
        concise: bool,
    },
    /// DatePicker, TimePicker, DatetimePicker: the value already formatted.
    DateTime {
        value: Option<String>,
    },
    Play {
        value: i64,
        playing: bool,
    },
    Unsupported {
        model: String,
        module: String,
    },
}

impl WidgetState {
    /// The widget state saved in a notebook's metadata, if any.
    pub fn from_notebook_metadata(metadata: &Map<String, Value>) -> Option<WidgetState> {
        let widgets = metadata.get("widgets")?;
        // ipywidgets ≥ 7 nests it under the mime type; 6 stored it bare.
        let saved = widgets.get(STATE_MIME).unwrap_or(widgets);
        let state = saved.get("state")?.as_object()?;
        let mut models = HashMap::with_capacity(state.len());
        for (id, model) in state {
            let Some(model) = model.as_object() else { continue };
            let inner = model.get("state").and_then(Value::as_object);
            let name = model
                .get("model_name")
                .or_else(|| inner.and_then(|s| s.get("_model_name")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let module = model
                .get("model_module")
                .or_else(|| inner.and_then(|s| s.get("_model_module")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut buffers = HashMap::new();
            for buffer in model.get("buffers").and_then(Value::as_array).into_iter().flatten() {
                let key = buffer
                    .get("path")
                    .and_then(Value::as_array)
                    .and_then(|p| p.first())
                    .and_then(Value::as_str);
                let data = buffer.get("data").and_then(Value::as_str);
                let encoding = buffer.get("encoding").and_then(Value::as_str).unwrap_or("base64");
                if let (Some(key), Some(data)) = (key, data) {
                    let bytes = match encoding {
                        "hex" => decode_hex(data),
                        _ => decode_base64(data),
                    };
                    if let Some(bytes) = bytes {
                        buffers.insert(key.to_string(), bytes);
                    }
                }
            }
            models.insert(
                id.clone(),
                Model {
                    name,
                    module,
                    state: inner.cloned().unwrap_or_default(),
                    buffers,
                },
            );
        }
        Some(WidgetState { models, ..WidgetState::default() })
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// The widget tree for a widget-view output's `model_id`; `None` when the
    /// model wasn't saved.
    pub fn view(&self, model_id: &str) -> Option<Widget> {
        let id = strip_ref(model_id);
        if self.closed.contains(id) {
            return Some(Widget {
                key: 0,
                model_id: id.to_string(),
                kind: Kind::Box {
                    direction: Direction::Column,
                    wrap: false,
                    children: Vec::new(),
                },
                description: String::new(),
                description_width: None,
                layout: Layout {
                    hidden: true,
                    ..Layout::default()
                },
                disabled: false,
            });
        }
        let mut resolver = Resolver {
            state: self,
            next_key: 0,
            stack: Vec::new(),
        };
        resolver.widget(id)
    }

    /// Apply a `comm_open` / `comm_msg` / `comm_close` from a running kernel.
    /// Returns the id of the model it changed; `None` for other comms
    /// (ipympl, Plotly's own comm, custom messages).
    pub fn apply_comm(&mut self, msg_type: &str, content: &Value, parent_msg_id: Option<&str>) -> Option<String> {
        let id = content.get("comm_id")?.as_str()?.to_string();
        let data = content.get("data");
        match msg_type {
            "comm_open" => {
                let target = content.get("target_name").and_then(Value::as_str).unwrap_or_default();
                if target != "jupyter.widget" {
                    return None;
                }
                let state = data?.get("state")?.as_object()?.clone();
                let name = state.get("_model_name").and_then(Value::as_str).unwrap_or_default().to_string();
                let module = state.get("_model_module").and_then(Value::as_str).unwrap_or_default().to_string();
                self.closed.remove(&id);
                self.live.insert(id.clone());
                self.models.insert(
                    id.clone(),
                    Model {
                        name,
                        module,
                        state,
                        buffers: HashMap::new(),
                    },
                );
                self.track_capture(&id);
                Some(id)
            }
            "comm_msg" => {
                let data = data?;
                let method = data.get("method").and_then(Value::as_str).unwrap_or_default();
                if method != "update" && method != "echo_update" {
                    return None;
                }
                let update = data.get("state")?.as_object()?;
                let pending = self.pending.get_mut(&id);
                let model = self.models.get_mut(&id)?;
                let echo = method == "echo_update";
                let mut pending = pending;
                for (k, v) in update {
                    if echo && let Some(p) = pending.as_deref_mut() {
                        match p.get(k) {
                            // The echo of our latest send: settled.
                            Some(sent) if Some(sent.as_str()) == parent_msg_id => {
                                p.remove(k);
                            }
                            // An older send's echo: our newer value stands.
                            Some(_) => continue,
                            None => {}
                        }
                    }
                    model.state.insert(k.clone(), v.clone());
                }
                self.track_capture(&id);
                Some(id)
            }
            "comm_close" => {
                self.models.contains_key(&id).then(|| {
                    self.live.remove(&id);
                    self.closed.insert(id.clone());
                    self.capturing.retain(|_, m| *m != id);
                    id
                })
            }
            _ => None,
        }
    }

    /// An Output widget captures the outputs of the request named by its
    /// `msg_id` (set while code runs `with out:`).
    fn track_capture(&mut self, id: &str) {
        let Some(model) = self.models.get(id) else { return };
        if model.name != "OutputModel" {
            return;
        }
        self.capturing.retain(|_, m| m != id);
        if let Some(msg_id) = model.state.get("msg_id").and_then(Value::as_str).filter(|m| !m.is_empty()) {
            self.capturing.insert(msg_id.to_string(), id.to_string());
        }
    }

    /// Whether the running kernel holds this model, so changes go back to it.
    pub fn is_live(&self, model_id: &str) -> bool {
        self.live.contains(strip_ref(model_id))
    }

    /// The kernel restarted or died: its models are gone, and the widgets
    /// stay as they last were, read-only.
    pub fn kernel_gone(&mut self) {
        self.live.clear();
        self.pending.clear();
        self.capturing.clear();
        self.clear_pending.clear();
    }

    /// Apply a change made here (a click, a drag) to the model.
    pub fn set_local(&mut self, model_id: &str, update: &Map<String, Value>) {
        if let Some(model) = self.models.get_mut(strip_ref(model_id)) {
            for (k, v) in update {
                model.state.insert(k.clone(), v.clone());
            }
        }
    }

    /// Record that `update` went to the kernel as `msg_id` (see `pending`).
    pub fn note_sent(&mut self, model_id: &str, update: &Map<String, Value>, msg_id: &str) {
        let keys = self.pending.entry(strip_ref(model_id).to_string()).or_default();
        for k in update.keys() {
            keys.insert(k.clone(), msg_id.to_string());
        }
    }

    /// The state change a slider gets when dragged to `fraction` (0..=1) of
    /// its track: the value snapped to its step (or the option index); for a
    /// range, the nearer end moves. `None` when the model isn't a slider or
    /// nothing would change.
    pub fn slider_update(&self, model_id: &str, fraction: f64) -> Option<Map<String, Value>> {
        let model = self.models.get(strip_ref(model_id))?;
        let s = &model.state;
        let f = fraction.clamp(0., 1.);
        let mut out = Map::new();
        match model.name.as_str() {
            "SelectionSliderModel" | "SelectionRangeSliderModel" => {
                let n = options(s).len();
                if n == 0 {
                    return None;
                }
                let i = (f * (n - 1) as f64).round() as u64;
                let index = match s.get("index") {
                    Some(Value::Array(ix)) => {
                        let a = ix.first().and_then(Value::as_u64).unwrap_or(0);
                        let b = ix.get(1).and_then(Value::as_u64).unwrap_or(a);
                        let (a, b) = if i.abs_diff(a) <= i.abs_diff(b) && i <= b { (i, b) } else { (a, i.max(a)) };
                        serde_json::json!([a, b])
                    }
                    _ => Value::from(i),
                };
                if s.get("index") == Some(&index) {
                    return None;
                }
                out.insert("index".into(), index);
            }
            "IntSliderModel" | "FloatSliderModel" | "FloatLogSliderModel" | "IntRangeSliderModel"
            | "FloatRangeSliderModel" => {
                let (min, max) = (num(s, "min").unwrap_or(0.), num(s, "max").unwrap_or(100.));
                let int = model.name.starts_with("Int");
                let step = num(s, "step").filter(|st| *st > 0.).unwrap_or(if int { 1. } else { 0.1 });
                let raw = min + f * (max - min);
                // Snap to the step grid from min, as the browser slider does.
                let mut v = (min + ((raw - min) / step).round() * step).clamp(min, max);
                let log = model.name == "FloatLogSliderModel";
                if log {
                    v = num(s, "base").unwrap_or(10.).powf(v);
                }
                let encode = |v: f64| {
                    if int {
                        Value::from(v.round() as i64)
                    } else {
                        // Trim float noise from the step arithmetic.
                        serde_json::Number::from_f64((v * 1e12).round() / 1e12).map_or(Value::Null, Value::Number)
                    }
                };
                let value = match s.get("value") {
                    Some(Value::Array(pair)) => {
                        let a = pair.first().and_then(Value::as_f64).unwrap_or(min);
                        let b = pair.get(1).and_then(Value::as_f64).unwrap_or(a);
                        let (a, b) = if (v - a).abs() <= (v - b).abs() && v <= b { (v, b) } else { (a, v.max(a)) };
                        Value::Array(vec![encode(a), encode(b)])
                    }
                    _ => encode(v),
                };
                if s.get("value").and_then(Value::as_f64) == value.as_f64() && !value.is_array()
                    || s.get("value") == Some(&value)
                {
                    return None;
                }
                out.insert("value".into(), value);
            }
            _ => return None,
        }
        Some(out)
    }

    /// A model's `_model_name` (`IntTextModel`, ...).
    pub fn model_name(&self, model_id: &str) -> Option<&str> {
        self.models.get(strip_ref(model_id)).map(|m| m.name.as_str())
    }

    /// One key of a model's state.
    pub fn get(&self, model_id: &str, key: &str) -> Option<&Value> {
        self.models.get(strip_ref(model_id))?.state.get(key)
    }

    /// Whether the kernel closed this model (its view draws nothing).
    pub fn is_closed(&self, model_id: &str) -> bool {
        self.closed.contains(strip_ref(model_id))
    }

    /// Whether an Output widget is capturing the outputs of `parent_msg_id`.
    pub fn is_capturing(&self, parent_msg_id: &str) -> bool {
        self.capturing.contains_key(parent_msg_id)
    }

    /// Route an output message (`stream`, `display_data`, `execute_result`,
    /// `error`, `clear_output`) into the Output widget capturing its request.
    /// Returns that widget's id when it took the message.
    pub fn capture(&mut self, parent_msg_id: &str, msg_type: &str, content: &Value) -> Option<String> {
        let id = self.capturing.get(parent_msg_id)?.clone();
        if msg_type == "clear_output" {
            if content.get("wait").and_then(Value::as_bool).unwrap_or(false) {
                self.clear_pending.insert(id.clone());
            } else {
                self.set_outputs(&id, |outputs| outputs.clear());
            }
            return Some(id);
        }
        let output = nbformat_output(msg_type, content)?;
        let clear = self.clear_pending.remove(&id);
        self.set_outputs(&id, |outputs| {
            if clear {
                outputs.clear();
            }
            // Consecutive chunks of one stream are one output, as in JupyterLab.
            if output["output_type"] == "stream"
                && let Some(last) = outputs.last_mut()
                && last["output_type"] == "stream"
                && last["name"] == output["name"]
                && let (Some(Value::String(prev)), Some(more)) =
                    (last.get_mut("text"), output.get("text").and_then(Value::as_str))
            {
                prev.push_str(more);
                return;
            }
            outputs.push(output);
        });
        Some(id)
    }

    /// The `metadata.widgets` value that saves every model reachable from
    /// `roots` (the model ids of a notebook's widget views), the way
    /// JupyterLab's "Save Widget State" and nbclient write it. `None` when
    /// nothing is reachable.
    pub fn to_metadata<'r>(&self, roots: impl IntoIterator<Item = &'r str>) -> Option<Value> {
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<String> = roots.into_iter().map(|r| strip_ref(r).to_string()).collect();
        let mut state = Map::new();
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) || self.closed.contains(&id) {
                continue;
            }
            let Some(model) = self.models.get(&id) else { continue };
            for v in model.state.values() {
                collect_refs(v, &mut stack);
            }
            let mut saved = model.state.clone();
            if model.name == "LayoutModel" {
                // Every layout property defaults to null; ipywidgets drops defaults.
                saved.retain(|k, v| !v.is_null() || k.starts_with('_'));
            }
            let mut entry = Map::new();
            entry.insert("model_module".into(), Value::String(model.module.clone()));
            let version = saved.get("_model_module_version").cloned().unwrap_or_else(|| Value::String("2.0.0".into()));
            entry.insert("model_module_version".into(), version);
            entry.insert("model_name".into(), Value::String(model.name.clone()));
            entry.insert("state".into(), Value::Object(saved));
            if !model.buffers.is_empty() {
                let buffers = model
                    .buffers
                    .iter()
                    .map(|(key, data)| {
                        serde_json::json!({"encoding": "base64", "path": [key], "data": encode_base64(data)})
                    })
                    .collect();
                entry.insert("buffers".into(), Value::Array(buffers));
            }
            state.insert(id, Value::Object(entry));
        }
        if state.is_empty() {
            return None;
        }
        // Stable order, so saving twice writes the same file.
        state.sort_keys();
        let mut saved = Map::new();
        saved.insert("state".into(), Value::Object(state));
        saved.insert("version_major".into(), Value::from(2));
        saved.insert("version_minor".into(), Value::from(0));
        let mut widgets = Map::new();
        widgets.insert(STATE_MIME.into(), Value::Object(saved));
        Some(Value::Object(widgets))
    }

    fn set_outputs(&mut self, id: &str, f: impl FnOnce(&mut Vec<Value>)) {
        let Some(model) = self.models.get_mut(id) else { return };
        let entry = model.state.entry("outputs").or_insert_with(|| Value::Array(Vec::new()));
        if !entry.is_array() {
            *entry = Value::Array(Vec::new());
        }
        if let Value::Array(outputs) = entry {
            f(outputs);
        }
    }
}

/// Model ids a state value refers to: `IPY_MODEL_` strings, and the views
/// an Output widget's stored outputs display.
fn collect_refs(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            if let Some(id) = s.strip_prefix("IPY_MODEL_") {
                out.push(id.to_string());
            }
        }
        Value::Array(items) => items.iter().for_each(|i| collect_refs(i, out)),
        Value::Object(map) => {
            if let Some(id) = map.get(VIEW_MIME).and_then(model_id) {
                out.push(id.to_string());
            }
            map.values().for_each(|i| collect_refs(i, out));
        }
        _ => {}
    }
}

fn encode_base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// An IOPub output message as the nbformat output an Output widget stores.
fn nbformat_output(msg_type: &str, content: &Value) -> Option<Value> {
    let c = content.as_object()?;
    let get = |k: &str| c.get(k).cloned().unwrap_or(Value::Null);
    let mut out = Map::new();
    out.insert("output_type".into(), Value::String(msg_type.into()));
    match msg_type {
        "stream" => {
            out.insert("name".into(), get("name"));
            let text = match c.get("text") {
                Some(Value::Array(parts)) => Value::String(parts.iter().filter_map(Value::as_str).collect()),
                Some(t) => t.clone(),
                None => Value::String(String::new()),
            };
            out.insert("text".into(), text);
        }
        "display_data" | "execute_result" => {
            out.insert("data".into(), get("data"));
            out.insert("metadata".into(), c.get("metadata").cloned().unwrap_or_else(|| Value::Object(Map::new())));
            if msg_type == "execute_result" {
                out.insert("execution_count".into(), get("execution_count"));
            }
        }
        "error" => {
            for k in ["ename", "evalue", "traceback"] {
                out.insert(k.into(), get(k));
            }
        }
        _ => return None,
    }
    Some(Value::Object(out))
}

/// The `model_id` of a widget-view output's mime data.
pub fn model_id(view: &Value) -> Option<&str> {
    view.get("model_id").and_then(Value::as_str)
}

fn strip_ref(id: &str) -> &str {
    id.strip_prefix("IPY_MODEL_").unwrap_or(id)
}

struct Resolver<'a> {
    state: &'a WidgetState,
    next_key: u32,
    stack: Vec<&'a str>,
}

impl<'a> Resolver<'a> {
    fn widget(&mut self, id: &'a str) -> Option<Widget> {
        let model = self.state.models.get(id)?;
        if self.stack.len() >= MAX_DEPTH || self.stack.contains(&id) {
            return None;
        }
        self.stack.push(id);
        let key = self.next_key;
        self.next_key += 1;
        let s = &model.state;
        let layout = self.layout(s);
        let style = self.reference(s, "style");
        let description = description(s);
        let description_width = style
            .and_then(|st| st.state.get("description_width"))
            .and_then(Value::as_str)
            .and_then(parse_length);
        let kind = self.kind(model, style);
        // A button's description is its label, drawn inside it.
        let description = match kind {
            Kind::Button { .. } | Kind::ToggleButton { .. } => String::new(),
            _ => description,
        };
        self.stack.pop();
        Some(Widget {
            key,
            model_id: id.to_string(),
            kind,
            description,
            description_width,
            layout,
            disabled: bool_of(s, "disabled"),
        })
    }

    fn reference(&self, s: &Map<String, Value>, key: &str) -> Option<&'a Model> {
        let id = s.get(key)?.as_str()?;
        self.state.models.get(strip_ref(id))
    }

    fn children(&mut self, s: &'a Map<String, Value>) -> Vec<Widget> {
        s.get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_str())
            .filter_map(|id| self.widget(strip_ref(id)))
            .collect()
    }

    fn layout(&self, s: &Map<String, Value>) -> Layout {
        let Some(layout) = self.reference(s, "layout") else {
            return Layout::default();
        };
        let l = &layout.state;
        let get = |k: &str| l.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
        let border = ["border", "border_top", "border_right", "border_bottom", "border_left"]
            .iter()
            .filter_map(|k| get(k))
            .any(|b| !b.starts_with("none") && !b.starts_with('0'));
        Layout {
            hidden: get("display") == Some("none") || get("visibility") == Some("hidden"),
            width: get("width").and_then(parse_length),
            height: get("height").and_then(parse_length),
            border,
            flex: get("flex").and_then(|f| f.split_whitespace().next()?.parse().ok()),
            grid_columns: get("grid_template_columns").and_then(grid_columns),
        }
    }

    fn kind(&mut self, model: &'a Model, style: Option<&'a Model>) -> Kind {
        let s = &model.state;
        let style_str = |k: &str| {
            style
                .and_then(|st| st.state.get(k))
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let name = model.name.as_str();
        let known_module = matches!(
            model.module.as_str(),
            "@jupyter-widgets/controls" | "@jupyter-widgets/output" | "@jupyter-widgets/base" | "jupyter-js-widgets" | ""
        );
        if !known_module {
            return unsupported(model);
        }
        match name {
            "HBoxModel" | "VBoxModel" | "BoxModel" | "GridBoxModel" => {
                let flow = self
                    .reference(s, "layout")
                    .map(|l| {
                        let get = |k: &str| l.state.get(k).and_then(Value::as_str).unwrap_or_default();
                        format!("{} {}", get("flex_flow"), get("flex_direction"))
                    })
                    .unwrap_or_default();
                let direction = match name {
                    "GridBoxModel" => Direction::Grid,
                    _ if flow.contains("column") => Direction::Column,
                    _ if flow.contains("row") => Direction::Row,
                    // A plain Box is a flex row, like an HBox.
                    "HBoxModel" | "BoxModel" => Direction::Row,
                    _ => Direction::Column,
                };
                Kind::Box {
                    direction,
                    wrap: flow.contains("wrap") && !flow.contains("nowrap"),
                    children: self.children(s),
                }
            }
            "TabModel" | "AccordionModel" => {
                let children = self.children(s);
                let titles = titles(s, children.len());
                let selected = index(s, "selected_index").filter(|&i| i < children.len());
                if name == "TabModel" {
                    // A Tab always shows one page; ipywidgets 7 saved `null` for the first.
                    let selected = selected.or(if children.is_empty() { None } else { Some(0) });
                    Kind::Tabs { titles, selected, children }
                } else {
                    Kind::Accordion { titles, selected, children }
                }
            }
            "StackModel" | "StackedModel" => {
                let children = self.children(s);
                let selected = index(s, "selected_index");
                Kind::Stack {
                    child: selected.and_then(|i| children.into_iter().nth(i)).map(Box::new),
                }
            }
            "IntProgressModel" | "FloatProgressModel" => {
                let (min, max) = (num(s, "min").unwrap_or(0.), num(s, "max").unwrap_or(100.));
                let value = num(s, "value").unwrap_or(min);
                Kind::Progress {
                    fraction: fraction(value, min, max),
                    tone: tone(s, "bar_style"),
                    color: style_str("bar_color"),
                    vertical: str_of(s, "orientation") == "vertical",
                }
            }
            "IntSliderModel" | "FloatSliderModel" | "FloatLogSliderModel" | "IntRangeSliderModel"
            | "FloatRangeSliderModel" => self.slider(name, s, style_str("handle_color")),
            "SelectionSliderModel" | "SelectionRangeSliderModel" => {
                let labels = options(s);
                let n = labels.len().max(1);
                let span = (n - 1).max(1) as f64;
                let (a, b, range) = match s.get("index") {
                    Some(Value::Array(ix)) => {
                        let a = ix.first().and_then(Value::as_u64).unwrap_or(0) as usize;
                        let b = ix.get(1).and_then(Value::as_u64).unwrap_or(a as u64) as usize;
                        (a, b, true)
                    }
                    other => (0, other.and_then(Value::as_u64).unwrap_or(0) as usize, false),
                };
                let label = |i: usize| labels.get(i).cloned().unwrap_or_default();
                let readout = bool_or(s, "readout", true).then(|| {
                    if range {
                        format!("{} – {}", label(a), label(b))
                    } else {
                        label(b)
                    }
                });
                Kind::Slider {
                    start: if range { (a as f64 / span).clamp(0., 1.) } else { 0. },
                    end: (b as f64 / span).clamp(0., 1.),
                    range,
                    readout,
                    vertical: str_of(s, "orientation") == "vertical",
                    color: style_str("handle_color"),
                }
            }
            "TextModel" | "TextareaModel" | "PasswordModel" | "ComboboxModel" | "IntTextModel"
            | "FloatTextModel" | "BoundedIntTextModel" | "BoundedFloatTextModel" => {
                let value = match s.get("value") {
                    Some(Value::String(v)) => v.clone(),
                    Some(Value::Number(n)) => number_text(n),
                    _ => String::new(),
                };
                let value = if name == "PasswordModel" { "•".repeat(value.chars().count()) } else { value };
                let placeholder = str_of(s, "placeholder");
                Kind::Text {
                    value,
                    placeholder: if placeholder == "\u{200b}" { String::new() } else { placeholder.to_string() },
                    multiline: name == "TextareaModel",
                }
            }
            "CheckboxModel" => Kind::Checkbox {
                value: bool_of(s, "value"),
                indent: bool_or(s, "indent", true),
            },
            "ToggleButtonModel" => Kind::ToggleButton {
                label: str_of(s, "description").to_string(),
                value: bool_of(s, "value"),
                tone: tone(s, "button_style"),
                icon: str_of(s, "icon").to_string(),
            },
            "ValidModel" => Kind::Valid {
                value: bool_of(s, "value"),
                readout: str_of(s, "readout").to_string(),
            },
            "DropdownModel" => Kind::Dropdown {
                options: options(s),
                selected: index(s, "index"),
            },
            "RadioButtonsModel" => Kind::Radio {
                options: options(s),
                selected: index(s, "index"),
            },
            "SelectModel" | "SelectMultipleModel" => {
                let selected = match s.get("index") {
                    Some(Value::Array(ix)) => ix.iter().filter_map(|i| i.as_u64().map(|i| i as usize)).collect(),
                    Some(i) => i.as_u64().map(|i| vec![i as usize]).unwrap_or_default(),
                    None => Vec::new(),
                };
                Kind::Select {
                    options: options(s),
                    selected,
                    rows: s.get("rows").and_then(Value::as_u64).unwrap_or(5).clamp(1, 40) as usize,
                    multiple: name == "SelectMultipleModel",
                }
            }
            "ToggleButtonsModel" => Kind::ToggleButtons {
                options: options(s),
                selected: index(s, "index"),
                tone: tone(s, "button_style"),
            },
            "ButtonModel" => Kind::Button {
                label: str_of(s, "description").to_string(),
                tone: tone(s, "button_style"),
                icon: str_of(s, "icon").to_string(),
                color: style_str("button_color"),
            },
            "FileUploadModel" => {
                let count = s
                    .get("value")
                    .map(|v| match v {
                        Value::Array(a) => a.len(),
                        Value::Object(o) => o.len(),
                        _ => 0,
                    })
                    .or_else(|| s.get("_counter").and_then(Value::as_u64).map(|c| c as usize))
                    .unwrap_or(0);
                let label = str_of(s, "description");
                Kind::Button {
                    label: format!("{} ({count})", if label.is_empty() { "Upload" } else { label }),
                    tone: tone(s, "button_style"),
                    icon: "upload".into(),
                    color: style_str("button_color"),
                }
            }
            "LabelModel" => Kind::Label {
                text: str_of(s, "value").to_string(),
            },
            "HTMLModel" | "HTMLMathModel" => Kind::Html {
                html: str_of(s, "value").to_string(),
                math: name == "HTMLMathModel",
            },
            "ImageModel" => {
                let data = model.buffers.get("value").cloned().unwrap_or_default();
                if data.is_empty() {
                    return unsupported(model);
                }
                let size = |k: &str| match s.get(k) {
                    Some(Value::String(v)) => v.trim_end_matches("px").trim().parse().ok(),
                    Some(Value::Number(n)) => n.as_f64().map(|v| v as f32),
                    _ => None,
                };
                Kind::Image {
                    format: str_of(s, "format").to_string(),
                    data,
                    width: size("width"),
                    height: size("height"),
                }
            }
            "OutputModel" => Kind::Output {
                outputs: s.get("outputs").and_then(Value::as_array).cloned().unwrap_or_default(),
            },
            "ColorPickerModel" => Kind::ColorPicker {
                value: str_of(s, "value").to_string(),
                concise: bool_of(s, "concise"),
            },
            "DatePickerModel" | "TimePickerModel" | "DatetimeModel" | "DatetimePickerModel"
            | "NaiveDatetimeModel" | "NaiveDatetimePickerModel" => Kind::DateTime {
                value: s.get("value").and_then(date_text),
            },
            "PlayModel" => Kind::Play {
                value: num(s, "value").unwrap_or(0.) as i64,
                playing: bool_of(s, "playing") || bool_of(s, "_playing"),
            },
            _ => unsupported(model),
        }
    }

    fn slider(&self, name: &str, s: &Map<String, Value>, color: Option<String>) -> Kind {
        let (min, max) = (num(s, "min").unwrap_or(0.), num(s, "max").unwrap_or(100.));
        let log = name == "FloatLogSliderModel";
        let base = num(s, "base").unwrap_or(10.);
        let default_format = match name {
            "IntSliderModel" | "IntRangeSliderModel" => "d",
            "FloatLogSliderModel" => ".3g",
            _ => ".2f",
        };
        let spec = s
            .get("readout_format")
            .and_then(Value::as_str)
            .filter(|f| !f.is_empty())
            .unwrap_or(default_format);
        // A log slider's min/max are exponents; its value is the real number.
        let position = |v: f64| {
            if log {
                fraction(v.max(f64::MIN_POSITIVE).log(base), min, max)
            } else {
                fraction(v, min, max)
            }
        };
        let (start, end, readout, range) = match s.get("value") {
            Some(Value::Array(pair)) => {
                let a = pair.first().and_then(Value::as_f64).unwrap_or(min);
                let b = pair.get(1).and_then(Value::as_f64).unwrap_or(a);
                (position(a), position(b), format!("{} – {}", format::format(spec, a), format::format(spec, b)), true)
            }
            v => {
                let v = v.and_then(Value::as_f64).unwrap_or(min);
                (0., position(v), format::format(spec, v), false)
            }
        };
        Kind::Slider {
            start,
            end,
            range,
            readout: bool_or(s, "readout", true).then_some(readout),
            vertical: str_of(s, "orientation") == "vertical",
            color,
        }
    }
}

fn unsupported(model: &Model) -> Kind {
    Kind::Unsupported {
        model: model.name.clone(),
        module: model.module.clone(),
    }
}

fn description(s: &Map<String, Value>) -> String {
    let d = str_of(s, "description");
    if bool_of(s, "description_allow_html") || d.contains('<') && d.contains('>') {
        html::strip_tags(d)
    } else {
        d.to_string()
    }
}

fn str_of<'s>(s: &'s Map<String, Value>, k: &str) -> &'s str {
    s.get(k).and_then(Value::as_str).unwrap_or_default()
}

fn bool_of(s: &Map<String, Value>, k: &str) -> bool {
    s.get(k).and_then(Value::as_bool).unwrap_or(false)
}

fn bool_or(s: &Map<String, Value>, k: &str, default: bool) -> bool {
    s.get(k).and_then(Value::as_bool).unwrap_or(default)
}

fn num(s: &Map<String, Value>, k: &str) -> Option<f64> {
    s.get(k).and_then(Value::as_f64)
}

fn index(s: &Map<String, Value>, k: &str) -> Option<usize> {
    s.get(k).and_then(Value::as_u64).map(|i| i as usize)
}

fn fraction(v: f64, min: f64, max: f64) -> f64 {
    if max.partial_cmp(&min) != Some(std::cmp::Ordering::Greater) || !v.is_finite() {
        return 0.;
    }
    ((v - min) / (max - min)).clamp(0., 1.)
}

fn tone(s: &Map<String, Value>, k: &str) -> Tone {
    match str_of(s, k) {
        "primary" => Tone::Primary,
        "success" => Tone::Success,
        "info" => Tone::Info,
        "warning" => Tone::Warning,
        "danger" => Tone::Danger,
        _ => Tone::Default,
    }
}

/// Option labels: `_options_labels` (ipywidgets 7 and 8), else `options`.
fn options(s: &Map<String, Value>) -> Vec<String> {
    let list = s
        .get("_options_labels")
        .or_else(|| s.get("options"))
        .and_then(Value::as_array);
    list.into_iter()
        .flatten()
        .map(|v| match v {
            Value::String(t) => t.clone(),
            other => other.to_string(),
        })
        .collect()
}

/// Tab/Accordion titles: `titles` (ipywidgets 8) or `_titles` {"0": ..} (7).
fn titles(s: &Map<String, Value>, n: usize) -> Vec<String> {
    let mut out = vec![String::new(); n];
    match s.get("titles").or_else(|| s.get("_titles")) {
        Some(Value::Array(t)) => {
            for (slot, title) in out.iter_mut().zip(t) {
                *slot = title.as_str().unwrap_or_default().to_string();
            }
        }
        Some(Value::Object(t)) => {
            for (k, title) in t {
                if let Some(slot) = k.parse::<usize>().ok().and_then(|i| out.get_mut(i)) {
                    *slot = title.as_str().unwrap_or_default().to_string();
                }
            }
        }
        _ => {}
    }
    out
}

fn number_text(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    let f = n.as_f64().unwrap_or(0.);
    format::format("", f)
}

/// ipywidgets serializes dates as `{year, month (0-based), date}` and times as
/// `{hours, minutes, seconds, milliseconds}`; datetimes as ISO strings.
fn date_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.replace('T', " ").trim_end_matches('Z').to_string()),
        Value::Object(o) => {
            let get = |k: &str| o.get(k).and_then(Value::as_i64);
            let date = match (get("year"), get("month"), get("date")) {
                (Some(y), Some(m), Some(d)) => Some(format!("{y:04}-{:02}-{d:02}", m + 1)),
                _ => None,
            };
            let time = match (get("hours"), get("minutes")) {
                (Some(h), Some(m)) => Some(match get("seconds") {
                    Some(s) if s != 0 => format!("{h:02}:{m:02}:{s:02}"),
                    _ => format!("{h:02}:{m:02}"),
                }),
                _ => None,
            };
            match (date, time) {
                (Some(d), Some(t)) => Some(format!("{d} {t}")),
                (d, t) => d.or(t),
            }
        }
        _ => None,
    }
}

fn parse_length(v: &str) -> Option<Length> {
    let v = v.trim();
    if let Some(p) = v.strip_suffix('%') {
        return p.trim().parse().ok().map(Length::Percent);
    }
    let px = v.strip_suffix("px").unwrap_or(v).trim();
    px.parse().ok().filter(|p: &f32| p.is_finite() && *p >= 0.).map(Length::Px)
}

/// `repeat(3, 120px)` → 3; `100px 1fr auto` → 3.
fn grid_columns(template: &str) -> Option<usize> {
    let t = template.trim();
    if let Some(inner) = t.strip_prefix("repeat(") {
        let count = inner.split(',').next()?.trim();
        return count.parse().ok().filter(|&n: &usize| n > 0);
    }
    let n = t.split_whitespace().count();
    (n > 0).then_some(n)
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    b.chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

fn decode_base64(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        if c.is_ascii_whitespace() {
            continue;
        }
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_and_grids() {
        assert_eq!(parse_length("100%"), Some(Length::Percent(100.)));
        assert_eq!(parse_length("120px"), Some(Length::Px(120.)));
        assert_eq!(parse_length("auto"), None);
        assert_eq!(grid_columns("repeat(3, 120px)"), Some(3));
        assert_eq!(grid_columns("10% 40% 50%"), Some(3));
    }

    #[test]
    fn base64_and_dates() {
        assert_eq!(decode_base64("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
        for len in 0..8 {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 + 5) as u8).collect();
            assert_eq!(decode_base64(&encode_base64(&data)), Some(data));
        }
        assert_eq!(encode_base64(b"hello"), "aGVsbG8=");
        assert_eq!(decode_hex("6869").as_deref(), Some(&b"hi"[..]));
        let d = serde_json::json!({"year": 2024, "month": 2, "date": 14});
        assert_eq!(date_text(&d).as_deref(), Some("2024-03-14"));
        let t = serde_json::json!({"hours": 9, "minutes": 5, "seconds": 0, "milliseconds": 0});
        assert_eq!(date_text(&t).as_deref(), Some("09:05"));
    }

    #[test]
    fn cycles_terminate() {
        let meta = serde_json::json!({"widgets": {STATE_MIME: {"state": {
            "a": {"model_name": "VBoxModel", "model_module": "@jupyter-widgets/controls",
                  "state": {"children": ["IPY_MODEL_b"]}},
            "b": {"model_name": "VBoxModel", "model_module": "@jupyter-widgets/controls",
                  "state": {"children": ["IPY_MODEL_a"]}}
        }}}});
        let state = WidgetState::from_notebook_metadata(meta.as_object().unwrap()).unwrap();
        let w = state.view("a").unwrap();
        let Kind::Box { children, .. } = &w.kind else { panic!() };
        let Kind::Box { children: inner, .. } = &children[0].kind else { panic!() };
        assert!(inner.is_empty());
    }
}
