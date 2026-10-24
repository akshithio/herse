//! Golden checks against notebooks executed with real ipywidgets 8 and tqdm
//! (tests/data/generate.py) and the fixture corpus.

use herse_widgets::{Direction, Kind, Tone, VIEW_MIME, Widget, WidgetState, model_id};
use serde_json::Value;

fn load(path: &str) -> (WidgetState, Vec<Widget>) {
    let nb: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let state = WidgetState::from_notebook_metadata(nb["metadata"].as_object().unwrap()).unwrap();
    let mut views = Vec::new();
    for cell in nb["cells"].as_array().unwrap() {
        for out in cell.get("outputs").and_then(Value::as_array).into_iter().flatten() {
            if let Some(view) = out.get("data").and_then(|d| d.get(VIEW_MIME)) {
                views.push(state.view(model_id(view).unwrap()).expect("saved model"));
            }
        }
    }
    (state, views)
}

fn find<'a>(views: &'a [Widget], description: &str) -> &'a Widget {
    views.iter().find(|w| w.description == description).unwrap_or_else(|| panic!("no widget {description:?}"))
}

#[test]
fn gallery_controls() {
    let (_, views) = load(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/gallery.ipynb"));
    assert!(views.len() > 40, "{}", views.len());
    assert!(views.iter().all(|w| !matches!(w.kind, Kind::Unsupported { .. })));

    let Kind::Slider { end, readout, .. } = &find(&views, "Int").kind else { panic!() };
    assert_eq!((*end, readout.as_deref()), (0.7, Some("7")));
    let Kind::Slider { readout, .. } = &find(&views, "Float").kind else { panic!() };
    assert_eq!(readout.as_deref(), Some("0.3"));
    let Kind::Slider { end, readout, .. } = &find(&views, "Log").kind else { panic!() };
    assert!((end - 0.6).abs() < 1e-9);
    assert_eq!(readout.as_deref(), Some("1.00e+3"));
    let Kind::Slider { start, end, range, readout, .. } = &find(&views, "Months").kind else { panic!() };
    assert_eq!((*start, *end, *range, readout.as_deref()), (0.25, 0.75, true, Some("Feb – Apr")));

    let Kind::Progress { fraction, tone, .. } = &find(&views, "warning").kind else { panic!() };
    assert_eq!((*fraction, *tone), (0.6, Tone::Warning));
    let Kind::Progress { color, .. } = &find(&views, "Magenta").kind else { panic!() };
    assert_eq!(color.as_deref(), Some("#ff00ff"));

    let Kind::Text { value, .. } = &find(&views, "Password").kind else { panic!() };
    assert_eq!(value, "••••••");
    let Kind::Dropdown { options, selected } = &find(&views, "Number").kind else { panic!() };
    assert_eq!(selected.map(|i| options[i].as_str()), Some("2"));
    let Kind::Select { selected, .. } = &find(&views, "Fruits").kind else { panic!() };
    assert_eq!(selected, &[1, 2]);
    let Kind::DateTime { value } = &find(&views, "Date").kind else { panic!() };
    assert_eq!(value.as_deref(), Some("2024-03-14"));

    // Buttons carry their label in the kind, not as a description.
    let row = views
        .iter()
        .find(|w| matches!(&w.kind, Kind::Box { children, .. } if children.len() == 6 && matches!(children[0].kind, Kind::Button { .. })))
        .unwrap();
    let Kind::Box { direction: Direction::Row, children, .. } = &row.kind else { panic!() };
    assert!(children.iter().all(|c| c.description.is_empty()));
    assert!(matches!(&children[5].kind, Kind::Button { label, tone: Tone::Danger, .. } if label == "danger"));

    let image = views.iter().find(|w| matches!(w.kind, Kind::Image { .. })).unwrap();
    let Kind::Image { data, width, .. } = &image.kind else { panic!() };
    assert!(data.starts_with(b"\x89PNG"));
    assert_eq!(*width, Some(64.));
}

#[test]
fn gallery_containers_and_tqdm() {
    let (_, views) = load(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/gallery.ipynb"));
    let tabs = views.iter().find(|w| matches!(w.kind, Kind::Tabs { .. })).unwrap();
    let Kind::Tabs { titles, selected, children } = &tabs.kind else { panic!() };
    assert_eq!((titles.as_slice(), *selected, children.len()), (&["One".to_string(), "Two".to_string()][..], Some(1), 2));
    let stack = views.iter().find(|w| matches!(w.kind, Kind::Stack { .. })).unwrap();
    let Kind::Stack { child: Some(child) } = &stack.kind else { panic!() };
    assert!(matches!(&child.kind, Kind::Label { text } if text.contains("selected")));
    let grid = views.iter().find(|w| matches!(w.kind, Kind::Box { direction: Direction::Grid, .. })).unwrap();
    assert_eq!(grid.layout.grid_columns, Some(3));

    // tqdm: HBox(HTML "desc: pct", FloatProgress, HTML stats).
    let bars: Vec<_> = views
        .iter()
        .filter_map(|w| match &w.kind {
            Kind::Box { children, .. } if children.len() == 3 && matches!(children[1].kind, Kind::Progress { .. }) => Some(children),
            _ => None,
        })
        .collect();
    assert_eq!(bars.len(), 3);
    let Kind::Html { html, .. } = &bars[1][0].kind else { panic!() };
    assert_eq!(herse_widgets::html::as_plain_text(html).as_deref(), Some("Stopped\u{2007}early:\u{2007}\u{2007}37%"));
    let Kind::Progress { fraction, color, .. } = &bars[1][1].kind else { panic!() };
    assert_eq!((*fraction, color.as_deref()), (0.37, Some("green")));

    // interact: VBox(slider, text, Output with the printed line and the figure).
    let interact = views
        .iter()
        .find(|w| matches!(&w.kind, Kind::Box { children, .. } if children.iter().any(|c| matches!(c.kind, Kind::Output { .. }))))
        .unwrap();
    let Kind::Box { children, .. } = &interact.kind else { panic!() };
    let Kind::Output { outputs } = &children[2].kind else { panic!() };
    assert!(outputs[1]["data"].get("image/png").is_some());
    // Keys number the tree in pre-order.
    let keys: Vec<u32> = std::iter::once(interact.key).chain(children.iter().map(|c| c.key)).collect();
    assert_eq!(keys, [0, 1, 2, 3]);
}

#[test]
fn fixture_widgets() {
    let (_, views) = load(concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/formats/widgets.ipynb"));
    assert!(views.iter().all(|w| !matches!(w.kind, Kind::Unsupported { .. })), "{views:#?}");
    let hidden = |w: &Widget| w.layout.hidden;
    assert!(!views.iter().any(hidden));
}

#[test]
fn ipywidgets7_state() {
    // Colab / ipywidgets 7 shape: `_titles` dicts, module version 1.5.0, no
    // nested mime key on older saves.
    let meta = serde_json::json!({"widgets": {"state": {
        "t": {"model_name": "TabModel", "model_module": "@jupyter-widgets/controls", "model_module_version": "1.5.0",
              "state": {"_titles": {"0": "First", "1": "Second"}, "children": ["IPY_MODEL_a", "IPY_MODEL_b"], "selected_index": null}},
        "a": {"model_name": "LabelModel", "model_module": "@jupyter-widgets/controls", "state": {"value": "A"}},
        "b": {"model_name": "LabelModel", "model_module": "@jupyter-widgets/controls", "state": {"value": "B"}},
        "f": {"model_name": "FigureModel", "model_module": "jupyterlab-plotly", "state": {}}
    }, "version": "1.1.1"}});
    let state = WidgetState::from_notebook_metadata(meta.as_object().unwrap()).unwrap();
    let Kind::Tabs { titles, selected, .. } = state.view("t").unwrap().kind else { panic!() };
    assert_eq!((titles, selected), (vec!["First".to_string(), "Second".to_string()], Some(0)));
    assert!(matches!(state.view("f").unwrap().kind, Kind::Unsupported { .. }));
    assert!(state.view("missing").is_none());
}

#[test]
fn garbage_never_panics() {
    for meta in [
        serde_json::json!({"widgets": 3}),
        serde_json::json!({"widgets": {"state": []}}),
        serde_json::json!({"widgets": {"state": {"x": 1, "y": {"model_name": 7, "state": "s"}}}}),
        serde_json::json!({"widgets": {"state": {"y": {"model_name": "IntSliderModel", "model_module": "@jupyter-widgets/controls",
            "state": {"value": "nan", "min": 5, "max": 1, "readout_format": "%%%", "layout": "IPY_MODEL_y", "style": 3}}}}}),
        serde_json::json!({"widgets": {"state": {"y": {"model_name": "ImageModel", "model_module": "@jupyter-widgets/controls",
            "state": {}, "buffers": [{"path": ["value"], "encoding": "base64", "data": "!!!"}]}}}}),
    ] {
        if let Some(state) = WidgetState::from_notebook_metadata(meta.as_object().unwrap()) {
            for id in ["x", "y", "IPY_MODEL_y", ""] {
                let _ = state.view(id);
            }
        }
    }
}

#[test]
fn live_comms() {
    use serde_json::json;
    let mut state = WidgetState::default();
    let open = |id: &str, state: Value| json!({"comm_id": id, "target_name": "jupyter.widget", "data": {"state": state, "buffer_paths": []}});
    // tqdm.notebook: a FloatProgress and two HTMLs in an HBox.
    for (id, s) in [
        ("bar", json!({"_model_name": "FloatProgressModel", "_model_module": "@jupyter-widgets/controls", "value": 0.0, "max": 10.0, "bar_style": ""})),
        ("pct", json!({"_model_name": "HTMLModel", "_model_module": "@jupyter-widgets/controls", "value": "  0%"})),
        ("box", json!({"_model_name": "HBoxModel", "_model_module": "@jupyter-widgets/controls", "children": ["IPY_MODEL_pct", "IPY_MODEL_bar"]})),
    ] {
        assert_eq!(state.apply_comm("comm_open", &open(id, s), None).as_deref(), Some(id));
    }
    let update = json!({"comm_id": "bar", "data": {"method": "update", "state": {"value": 10.0, "bar_style": "success"}, "buffer_paths": []}});
    assert_eq!(state.apply_comm("comm_msg", &update, None).as_deref(), Some("bar"));
    let Kind::Box { children, .. } = state.view("box").unwrap().kind else { panic!() };
    assert_eq!(children[1].model_id, "bar");
    assert!(matches!(children[1].kind, Kind::Progress { fraction, tone: Tone::Success, .. } if fraction == 1.0));
    // Other comm targets and custom messages are not widgets.
    assert!(state.apply_comm("comm_open", &json!({"comm_id": "m", "target_name": "matplotlib", "data": {}}), None).is_none());
    let custom = json!({"comm_id": "bar", "data": {"method": "custom", "content": {}}});
    assert!(state.apply_comm("comm_msg", &custom, None).is_none());
    // leave=False closes the bar: its view draws nothing.
    state.apply_comm("comm_close", &json!({"comm_id": "box", "data": {}}), None);
    assert!(state.view("box").unwrap().layout.hidden);

    // An Output widget captures the request it's told to.
    state.apply_comm("comm_open", &open("out", json!({"_model_name": "OutputModel", "_model_module": "@jupyter-widgets/output", "outputs": [], "msg_id": ""})), None);
    assert!(!state.is_capturing("req1"));
    let capture = json!({"comm_id": "out", "data": {"method": "update", "state": {"msg_id": "req1"}}});
    state.apply_comm("comm_msg", &capture, None);
    assert!(state.is_capturing("req1"));
    let print = |t: &str| json!({"name": "stdout", "text": t});
    assert_eq!(state.capture("req1", "stream", &print("a\n")).as_deref(), Some("out"));
    state.capture("req1", "stream", &print("b\n"));
    state.capture("req1", "clear_output", &json!({"wait": true}));
    let Kind::Output { outputs } = state.view("out").unwrap().kind else { panic!() };
    assert_eq!(outputs, vec![json!({"output_type": "stream", "name": "stdout", "text": "a\nb\n"})]);
    state.capture("req1", "display_data", &json!({"data": {"text/plain": "7"}, "metadata": {}}));
    let Kind::Output { outputs } = state.view("out").unwrap().kind else { panic!() };
    assert_eq!(outputs.len(), 1, "wait=True cleared before the next output");
    assert_eq!(outputs[0]["output_type"], "display_data");
    assert!(state.capture("other", "stream", &print("x")).is_none());
    state.apply_comm("comm_msg", &json!({"comm_id": "out", "data": {"method": "update", "state": {"msg_id": ""}}}), None);
    assert!(!state.is_capturing("req1"));
}

#[test]
fn saved_state_round_trips() {
    // Re-saving the gallery's reachable state and parsing it back draws the same widgets.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/gallery.ipynb");
    let nb: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let (state, views) = load(path);
    let roots: Vec<String> = views.iter().map(|w| w.model_id.clone()).collect();
    let saved = state.to_metadata(roots.iter().map(String::as_str)).unwrap();
    let meta = serde_json::json!({"widgets": saved});
    let again = WidgetState::from_notebook_metadata(meta.as_object().unwrap()).unwrap();
    for (root, view) in roots.iter().zip(&views) {
        assert_eq!(again.view(root).as_ref(), Some(view));
    }
    // Only reachable models are written, and layouts lose their null defaults.
    let original = nb["metadata"]["widgets"][herse_widgets::STATE_MIME]["state"].as_object().unwrap().len();
    let written = saved[herse_widgets::STATE_MIME]["state"].as_object().unwrap();
    assert!(written.len() <= original);
    let layout = written.values().find(|m| m["model_name"] == "LayoutModel").unwrap();
    assert!(layout["state"].as_object().unwrap().iter().all(|(k, v)| k.starts_with('_') || !v.is_null()));
    assert!(state.to_metadata(["nope"]).is_none());
}

#[test]
fn interaction() {
    use serde_json::json;
    let mut state = WidgetState::default();
    let open = |id: &str, s: Value| json!({"comm_id": id, "target_name": "jupyter.widget", "data": {"state": s}});
    state.apply_comm("comm_open", &open("s", json!({"_model_name": "IntSliderModel", "_model_module": "@jupyter-widgets/controls", "value": 5, "min": 0, "max": 10, "step": 2})), None);
    state.apply_comm("comm_open", &open("f", json!({"_model_name": "FloatSliderModel", "_model_module": "@jupyter-widgets/controls", "value": 0.5, "min": 0.0, "max": 1.0, "step": 0.1})), None);
    state.apply_comm("comm_open", &open("r", json!({"_model_name": "IntRangeSliderModel", "_model_module": "@jupyter-widgets/controls", "value": [2, 8], "min": 0, "max": 10, "step": 1})), None);
    state.apply_comm("comm_open", &open("e", json!({"_model_name": "SelectionSliderModel", "_model_module": "@jupyter-widgets/controls", "index": 0, "_options_labels": ["a", "b", "c"]})), None);
    assert!(state.is_live("s"));
    assert_eq!(state.slider_update("s", 0.73), Some(json!({"value": 8}).as_object().unwrap().clone()));
    // 5 isn't on the step grid from 0; the drag snaps to it.
    assert_eq!(state.slider_update("s", 0.5).unwrap()["value"], json!(6));
    // No change, no message.
    assert_eq!(state.slider_update("r", 0.2), None);
    assert_eq!(state.slider_update("f", 0.33).unwrap()["value"], json!(0.3));
    assert_eq!(state.slider_update("r", 0.1).unwrap()["value"], json!([1, 8]));
    assert_eq!(state.slider_update("r", 0.9).unwrap()["value"], json!([2, 9]));
    assert_eq!(state.slider_update("e", 1.0).unwrap()["index"], json!(2));

    // A drag sends 6 then 8; the echo of 6 arriving late must not undo 8.
    let update = |v: i64, msg: &str, state: &mut WidgetState| {
        let u = json!({"value": v}).as_object().unwrap().clone();
        state.set_local("s", &u);
        state.note_sent("s", &u, msg);
    };
    update(6, "m1", &mut state);
    update(8, "m2", &mut state);
    let echo = |v: i64| json!({"comm_id": "s", "data": {"method": "echo_update", "state": {"value": v}}});
    state.apply_comm("comm_msg", &echo(6), Some("m1"));
    let Kind::Slider { readout, .. } = state.view("s").unwrap().kind else { panic!() };
    assert_eq!(readout.as_deref(), Some("8"));
    state.apply_comm("comm_msg", &echo(8), Some("m2"));
    // Settled: the kernel's own later updates apply again.
    let kernel_update = json!({"comm_id": "s", "data": {"method": "update", "state": {"value": 2}}});
    state.apply_comm("comm_msg", &kernel_update, Some("x"));
    let Kind::Slider { readout, .. } = state.view("s").unwrap().kind else { panic!() };
    assert_eq!(readout.as_deref(), Some("2"));
    state.kernel_gone();
    assert!(!state.is_live("s"));
}
