//! Shareable URLs for a gist, and the gist file name for a notebook.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const NBVIEWER_BASE: &str = "https://nbviewer.org";
pub const GIST_WEB_BASE: &str = "https://gist.github.com";
pub const GIST_RAW_BASE: &str = "https://gist.githubusercontent.com";

/// Gist ids are hex (32 chars today, 20 for older gists, digits for the
/// oldest). Anything else is rejected before it goes into a URL path.
pub fn is_valid_gist_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The nbviewer rendering of the gist: the link to share.
/// `https://nbviewer.org/gist/<id>`
pub fn nbviewer_url(gist_id: &str) -> String {
    format!("{NBVIEWER_BASE}/gist/{gist_id}")
}

/// The gist page on GitHub (revisions, comments, download).
/// `https://gist.github.com/<id>` (GitHub redirects to `/<owner>/<id>`).
pub fn gist_html_url(gist_id: &str) -> String {
    format!("{GIST_WEB_BASE}/{gist_id}")
}

/// The raw `.ipynb` of the latest revision:
/// `https://gist.githubusercontent.com/<owner>/<id>/raw/<filename>`.
pub fn raw_url(owner: &str, gist_id: &str, filename: &str) -> String {
    format!(
        "{GIST_RAW_BASE}/{}/{gist_id}/raw/{}",
        encode_path_segment(owner),
        encode_path_segment(filename)
    )
}

/// The gist file name for a notebook: `<name>.ipynb`. Accepts a bare name,
/// a file name or a path ("analysis", "analysis.ipynb", "/x/analysis.ipynb"
/// all give "analysis.ipynb"); control characters become spaces and an empty
/// name becomes "notebook.ipynb".
pub fn gist_filename(notebook_name: &str) -> String {
    let base = notebook_name.rsplit(['/', '\\']).next().unwrap_or("");
    let stem = match base.len().checked_sub(6) {
        Some(cut) if base.is_char_boundary(cut) && base[cut..].eq_ignore_ascii_case(".ipynb") => {
            &base[..cut]
        }
        _ => base,
    };
    let cleaned: String = stem
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut stem = cleaned.trim().to_string();
    if stem.chars().count() > 200 {
        stem = stem
            .chars()
            .take(200)
            .collect::<String>()
            .trim_end()
            .to_string();
    }
    if stem.is_empty() || stem.chars().all(|c| c == '.') {
        stem = "notebook".into();
    }
    format!("{stem}.ipynb")
}

/// Percent-encode everything but RFC 3986 unreserved characters.
pub(crate) fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Parse GitHub's UTC timestamps (`2024-05-01T12:34:56Z`, as in `created_at`
/// / `updated_at` / `published_at`), e.g. to show "updated 3 hours ago".
pub fn parse_timestamp(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = s.get(r)?;
        if !part.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Optional fraction, then `Z` (GitHub always uses UTC).
    let rest = &s[19..];
    let rest = rest.strip_prefix('.').map_or(rest, |frac| {
        frac.trim_start_matches(|c: char| c.is_ascii_digit())
    });
    if rest != "Z"
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second;
    let secs = u64::try_from(secs).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let id = "aa5a315d61ae9438b18d";
        assert_eq!(
            nbviewer_url(id),
            "https://nbviewer.org/gist/aa5a315d61ae9438b18d"
        );
        assert_eq!(
            gist_html_url(id),
            "https://gist.github.com/aa5a315d61ae9438b18d"
        );
        assert_eq!(
            raw_url("octocat", id, "My Analysis (v2).ipynb"),
            "https://gist.githubusercontent.com/octocat/aa5a315d61ae9438b18d/raw/My%20Analysis%20%28v2%29.ipynb"
        );
        assert!(is_valid_gist_id(id));
        assert!(is_valid_gist_id("1234"));
        for bad in ["", "../x", "a/b", "id?x=1", "a b", "é"] {
            assert!(!is_valid_gist_id(bad), "{bad}");
        }
    }

    #[test]
    fn filenames() {
        assert_eq!(gist_filename("analysis"), "analysis.ipynb");
        assert_eq!(gist_filename("analysis.ipynb"), "analysis.ipynb");
        assert_eq!(gist_filename("Analysis.IPYNB"), "Analysis.ipynb");
        assert_eq!(
            gist_filename("/Users/me/work/clustering (5).ipynb"),
            "clustering (5).ipynb"
        );
        assert_eq!(gist_filename("C:\\work\\nb.ipynb"), "nb.ipynb");
        assert_eq!(gist_filename("  "), "notebook.ipynb");
        assert_eq!(gist_filename(".ipynb"), "notebook.ipynb");
        assert_eq!(gist_filename("a\tb\n"), "a b.ipynb");
        assert_eq!(gist_filename("données"), "données.ipynb");
        assert_eq!(gist_filename(&"x".repeat(500)).len(), 200 + ".ipynb".len());
    }

    #[test]
    fn timestamps() {
        let t = parse_timestamp("2024-05-01T12:34:56Z").unwrap();
        assert_eq!(
            t.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_714_566_896
        );
        let epoch = parse_timestamp("1970-01-01T00:00:00Z").unwrap();
        assert_eq!(epoch, UNIX_EPOCH);
        let leap = parse_timestamp("2024-02-29T00:00:00.123Z").unwrap();
        assert_eq!(
            leap.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_709_164_800
        );
        for bad in [
            "",
            "2024-05-01",
            "2024-05-01T12:34:56",
            "2024-13-01T00:00:00Z",
            "2024-05-01T12:34:56+02:00",
        ] {
            assert_eq!(parse_timestamp(bad), None, "{bad}");
        }
    }
}
