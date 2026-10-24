//! Widget HTML values are usually plain text with entities (tqdm writes
//! `10/10 [00:01&lt;00:00]`); those are cheaper to draw as a label than to
//! lay out as HTML.

/// The decoded text when `html` has no markup, `None` when it does.
pub fn as_plain_text(html: &str) -> Option<String> {
    if html.contains('<') {
        return None;
    }
    Some(decode_entities(html))
}

/// Text with tags removed and entities decoded (for descriptions that allow HTML).
pub fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode_entities(&out)
}

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest[1..].find(';').map(|j| j + 1).filter(|&j| j <= 12);
        let decoded = end.and_then(|j| entity(&rest[1..j]));
        match (end, decoded) {
            (Some(j), Some(c)) => {
                out.push(c);
                rest = &rest[j + 1..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return char::from_u32(code);
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "ensp" => '\u{2002}',
        "emsp" => '\u{2003}',
        "thinsp" => '\u{2009}',
        "ndash" => '–',
        "mdash" => '—',
        "hellip" => '…',
        "middot" => '·',
        "times" => '×',
        "deg" => '°',
        "copy" => '©',
        "reg" => '®',
        "laquo" => '«',
        "raquo" => '»',
        "larr" => '←',
        "rarr" => '→',
        "check" => '✓',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tqdm_stats() {
        assert_eq!(
            as_plain_text("\u{2007}10/10\u{2007}[00:00&lt;00:00,\u{2007}\u{2007}3.97it/s]").as_deref(),
            Some("\u{2007}10/10\u{2007}[00:00<00:00,\u{2007}\u{2007}3.97it/s]")
        );
        assert_eq!(as_plain_text("<b>x</b>"), None);
        assert_eq!(decode_entities("a &amp; b &#65;&#x42; &bogus; &"), "a & b AB &bogus; &");
        assert_eq!(strip_tags("<b>Bold</b> &gt; <i>it</i>"), "Bold > it");
    }
}
