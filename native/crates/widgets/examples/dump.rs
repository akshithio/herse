//! Prints the widget tree of every widget output in a notebook.
//!
//!     cargo run --release -p herse-widgets --example dump -- notebook.ipynb

use herse_widgets::{Kind, VIEW_MIME, Widget, WidgetState, model_id};
use serde_json::Value;

fn main() {
    let path = std::env::args().nth(1).expect("usage: dump NOTEBOOK");
    let nb: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let t = std::time::Instant::now();
    let state = nb
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(WidgetState::from_notebook_metadata);
    let parsed = t.elapsed();
    let Some(state) = state else {
        println!("no widget state");
        return;
    };
    println!("{} models, parsed in {:.2} ms", state.len(), parsed.as_secs_f64() * 1e3);
    for (ci, cell) in nb["cells"].as_array().unwrap().iter().enumerate() {
        for out in cell.get("outputs").and_then(Value::as_array).into_iter().flatten() {
            let Some(view) = out.get("data").and_then(|d| d.get(VIEW_MIME)) else { continue };
            let id = model_id(view).unwrap_or_default();
            println!("cell {ci}:");
            match state.view(id) {
                Some(w) => print(&w, 1),
                None => println!("  (model {id} not saved)"),
            }
        }
    }
}

fn print(w: &Widget, depth: usize) {
    let pad = "  ".repeat(depth);
    let mut line = format!("{pad}#{} ", w.key);
    if !w.description.is_empty() {
        line += &format!("[{}] ", w.description);
    }
    if w.layout.hidden {
        line += "(hidden) ";
    }
    match &w.kind {
        Kind::Box { direction, wrap, children } => {
            println!("{line}Box {direction:?}{}", if *wrap { " wrap" } else { "" });
            children.iter().for_each(|c| print(c, depth + 1));
        }
        Kind::Tabs { titles, selected, children } | Kind::Accordion { titles, selected, children } => {
            let what = if matches!(w.kind, Kind::Tabs { .. }) { "Tabs" } else { "Accordion" };
            println!("{line}{what} {titles:?} selected={selected:?}");
            children.iter().for_each(|c| print(c, depth + 1));
        }
        Kind::Stack { child } => {
            println!("{line}Stack");
            if let Some(c) = child {
                print(c, depth + 1);
            }
        }
        Kind::Image { format, data, width, height } => {
            println!("{line}Image {format} {} bytes {width:?}x{height:?}", data.len())
        }
        Kind::Output { outputs } => {
            println!("{line}Output ({} outputs)", outputs.len());
            for o in outputs {
                println!("{pad}  - {}", o.to_string().chars().take(100).collect::<String>());
            }
        }
        other => println!("{line}{other:?}"),
    }
}
