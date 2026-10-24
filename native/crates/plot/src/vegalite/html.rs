//! Finding the Vega-Lite spec Altair embeds in its `text/html` output.

use serde_json::Value;

/// Altair 4/5 write the spec as JSON inside a `<script>`, e.g.
/// `(function(spec, embedOpt){ ... })({"config": ..., "$schema": ".../vega-lite/v5.json", ...}, {"mode": "vega-lite"});`
/// or `var spec = {...}; vegaEmbed("#vis", spec, embedOpt)`.
pub fn extract(html: &str) -> Option<Value> {
    let bytes = html.as_bytes();
    let mut from = 0;
    while let Some(rel) = html.get(from..)?.find("vega.github.io/schema/vega-lite") {
        let pos = from + rel;
        if let Some(v) = enclosing_spec(html, bytes, pos) {
            return Some(v);
        }
        from = pos + 1;
    }
    // No $schema: try the object passed to vegaEmbed(...) directly.
    let mut from = 0;
    while let Some(rel) = html.get(from..)?.find("vegaEmbed(") {
        let pos = from + rel + "vegaEmbed(".len();
        let tail = &html[pos..];
        // second argument
        if let Some(comma) = tail.find(',') {
            let arg = tail[comma + 1..].trim_start();
            if arg.starts_with('{')
                && let Some(v) = parse_at(arg)
                && looks_like_vl(&v)
            {
                return Some(v);
            }
        }
        from = pos;
    }
    None
}

fn enclosing_spec(html: &str, bytes: &[u8], pos: usize) -> Option<Value> {
    let floor = pos.saturating_sub(2 * 1024 * 1024);
    let mut i = pos;
    let mut tries = 0;
    while i > floor && tries < 400 {
        i -= 1;
        if bytes[i] != b'{' {
            continue;
        }
        let prev = bytes[floor..i].iter().rev().find(|b| !b.is_ascii_whitespace()).copied();
        if !matches!(prev, Some(b'(' | b'=' | b',' | b':' | b'[' | b';' | b'>') | None) {
            continue;
        }
        tries += 1;
        let Some(s) = html.get(i..) else { continue };
        if let Some(v) = parse_at(s)
            && v.get("$schema").and_then(|s| s.as_str()).is_some_and(|s| s.contains("vega-lite"))
        {
            return Some(v);
        }
    }
    None
}

fn parse_at(s: &str) -> Option<Value> {
    let mut it = serde_json::Deserializer::from_str(s).into_iter::<Value>();
    match it.next()? {
        Ok(v) if v.is_object() => Some(v),
        _ => None,
    }
}

fn looks_like_vl(v: &Value) -> bool {
    let o = match v.as_object() {
        Some(o) => o,
        None => return false,
    };
    ["mark", "layer", "facet", "hconcat", "vconcat", "concat", "repeat"].iter().any(|k| o.contains_key(*k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn altair5_iife() {
        let html = r#"<div id="altair-viz-1"></div><script type="text/javascript">
  (function(spec, embedOpt){ vegaEmbed(outputDiv, spec, embedOpt) })({"config": {"view": {"continuousWidth": 300}}, "mark": {"type": "point"}, "encoding": {"x": {"field": "a", "type": "quantitative"}}, "$schema": "https://vega.github.io/schema/vega-lite/v5.8.0.json", "datasets": {"d": [{"a": 1}]}}, {"mode": "vega-lite"});
</script>"#;
        let v = extract(html).unwrap();
        assert_eq!(v["mark"]["type"], "point");
        assert_eq!(v["datasets"]["d"][0]["a"], 1);
    }

    #[test]
    fn var_spec() {
        let html = r##"<script>var spec = {"$schema": "https://vega.github.io/schema/vega-lite/v4.json", "mark": "bar", "data": {"values": [{"x": 1}]}};
        var embedOpt = {"mode": "vega-lite"}; vegaEmbed("#vis", spec, embedOpt);</script>"##;
        assert_eq!(extract(html).unwrap()["mark"], "bar");
    }

    #[test]
    fn no_spec() {
        assert!(extract("<p>hello</p>").is_none());
        assert!(extract("vega.github.io/schema/vega-lite {").is_none());
        assert!(extract("{\"$schema\": \"https://vega.github.io/schema/vega-lite/v5.json\"").is_none());
    }
}
