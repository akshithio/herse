//! Notebooks from the web. A link to a notebook's page on GitHub, a gist,
//! nbviewer, Colab, GitLab or Hugging Face (or any URL serving the JSON)
//! opens as an untitled notebook named after the file, and the relative
//! images and links in it resolve against where it came from.

use std::{io::Read as _, sync::OnceLock, time::Duration};

/// Bigger than any notebook worth reading over the network.
const MAX_BYTES: u64 = 256 * 1024 * 1024;

/// A notebook fetched from a link.
pub struct Fetched {
    /// Where the JSON came from (after rewriting a page link); relative
    /// links in the notebook resolve against it.
    pub url: String,
    /// The file name, for the tab and for Save.
    pub name: String,
    pub notebook: herse_nb::Notebook,
}

/// Whether pasted text is a web link (rather than a `herse://` session
/// link or a path).
pub fn is_web_link(text: &str) -> bool {
    let t = text.trim();
    t.starts_with("https://")
        || t.starts_with("http://")
        || ["github.com/", "gist.github.com/", "nbviewer.org/", "colab.research.google.com/", "gitlab.com/", "huggingface.co/"]
            .iter()
            .any(|host| t.starts_with(host) || t.strip_prefix("www.").is_some_and(|t| t.starts_with(host)))
}

/// Whether a link points at a notebook (its path ends in `.ipynb`, or it is
/// a gist, which holds one), so following it should open it here.
pub fn is_notebook_link(url: &str) -> bool {
    raw_url(url).is_ok_and(|raw| {
        let path = raw.split(['?', '#']).next().unwrap_or(&raw);
        path.to_ascii_lowercase().ends_with(".ipynb")
            || raw.starts_with("https://gist.githubusercontent.com/")
            || raw.starts_with("https://gist.github.com/")
    })
}

/// The URL serving a notebook's JSON, for a link to its page.
pub fn raw_url(link: &str) -> Result<String, String> {
    let link = link.trim();
    let url = if link.starts_with("https://") || link.starts_with("http://") {
        link.to_string()
    } else if is_web_link(link) {
        format!("https://{link}")
    } else {
        return Err("That isn't a web link.".into());
    };
    let (scheme, rest) = url.split_once("://").expect("has a scheme");
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    // Page links carry views in the query (`?plain=1`) and anchors in the
    // fragment; the raw file has neither.
    let bare = path.split(['?', '#']).next().unwrap_or(path).trim_end_matches('/');
    let parts: Vec<&str> = bare.split('/').collect();
    let github_raw = |parts: &[&str]| -> Result<String, String> {
        // owner / repo / (blob|raw) / ref… / path…
        match parts {
            [owner, repo, "blob" | "raw", rest @ ..] if rest.len() >= 2 => {
                Ok(format!("https://raw.githubusercontent.com/{owner}/{repo}/{}", rest.join("/")))
            }
            [_, _, "tree", ..] | [_, _] => Err("That's a folder, not a notebook. Open the notebook itself and copy its link.".into()),
            _ => Err("That GitHub link doesn't point at a file.".into()),
        }
    };
    let gist_raw = |parts: &[&str]| -> Result<String, String> {
        match parts {
            [user, id, ..] if is_gist_id(id) => Ok(format!("https://gist.githubusercontent.com/{user}/{id}/raw")),
            [id, ..] if is_gist_id(id) => Ok(format!("https://gist.github.com/{id}/raw")),
            _ => Err("That gist link has no gist in it.".into()),
        }
    };
    match host {
        "github.com" => github_raw(&parts),
        "gist.github.com" => match parts.as_slice() {
            // Already a raw link.
            [_, _, "raw", ..] | [_, "raw", ..] => Ok(format!("https://gist.github.com/{bare}")),
            _ => gist_raw(&parts),
        },
        "nbviewer.org" | "nbviewer.jupyter.org" => match parts.as_slice() {
            ["github", rest @ ..] => github_raw(rest),
            ["gist", rest @ ..] => gist_raw(rest),
            // nbviewer spells the scheme into the path.
            ["urls", rest @ ..] if !rest.is_empty() => Ok(format!("https://{}", rest.join("/"))),
            ["url", rest @ ..] if !rest.is_empty() => Ok(format!("http://{}", rest.join("/"))),
            _ => Err("That nbviewer link doesn't point at a notebook.".into()),
        },
        "colab.research.google.com" => match parts.as_slice() {
            ["github", rest @ ..] => github_raw(rest),
            ["gist", rest @ ..] => gist_raw(rest),
            ["drive", ..] => Err(
                "Colab notebooks kept in Google Drive can't be opened from a link. In Colab, choose File ▸ Download ▸ Download .ipynb, then open the file."
                    .into(),
            ),
            _ => Err("That Colab link doesn't point at a notebook.".into()),
        },
        "gitlab.com" => match bare.split_once("/-/blob/") {
            Some((project, file)) => Ok(format!("https://gitlab.com/{project}/-/raw/{file}")),
            None => Ok(format!("{scheme}://gitlab.com/{bare}")),
        },
        "huggingface.co" => match bare.split_once("/blob/") {
            Some((repo, file)) => Ok(format!("https://huggingface.co/{repo}/resolve/{file}")),
            None => Ok(format!("{scheme}://huggingface.co/{bare}")),
        },
        _ => Ok(url.split('#').next().unwrap_or(&url).to_string()),
    }
}

fn is_gist_id(s: &str) -> bool {
    s.len() >= 7 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The notebook's file name, from the URL it came from (the URL path's last
/// segment, percent-decoded), or `Notebook.ipynb`.
pub fn file_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').next().unwrap_or("");
    let name = percent_decode(last);
    if name.to_ascii_lowercase().ends_with(".ipynb") { name } else { "Notebook.ipynb".into() }
}

/// `rel` resolved against the document at `base`, as a browser would.
pub fn join(base: &str, rel: &str) -> String {
    if rel.contains("://") || rel.starts_with("data:") || rel.starts_with("mailto:") {
        return rel.to_string();
    }
    let base = base.split(['?', '#']).next().unwrap_or(base);
    let Some((scheme, rest)) = base.split_once("://") else { return rel.to_string() };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    if let Some(rest) = rel.strip_prefix("//") {
        return format!("{scheme}://{rest}");
    }
    let mut segments: Vec<&str> = if rel.starts_with('/') {
        Vec::new()
    } else {
        // The base's folder: drop its file name.
        let mut s: Vec<&str> = path.split('/').collect();
        s.pop();
        s
    };
    let (rel_path, suffix) = match rel.find(['?', '#']) {
        Some(i) => rel.split_at(i),
        None => (rel, ""),
    };
    for part in rel_path.trim_start_matches('/').split('/') {
        match part {
            "." | "" => {}
            ".." => {
                segments.pop();
            }
            p => segments.push(p),
        }
    }
    // A trailing slash (`dir/`) stays a folder.
    let slash = if rel_path.ends_with('/') && !segments.is_empty() { "/" } else { "" };
    format!("{scheme}://{host}/{}{slash}{suffix}", segments.join("/"))
}

/// Fetch and parse the notebook a link points at. Blocking: call it off the
/// UI thread.
pub fn fetch(link: &str) -> Result<Fetched, String> {
    let url = raw_url(link)?;
    let response = agent().get(&url).call().map_err(|err| match err {
        ureq::Error::Status(404, _) => {
            "Not found (404). If the repository is private, download the notebook and open the file.".to_string()
        }
        ureq::Error::Status(code, response) => format!("The server said {code} {}.", response.status_text()),
        ureq::Error::Transport(t) => format!("Couldn't reach {}: {t}", host_of(&url)),
    })?;
    // Redirects (a gist's `/raw`) end at the file itself, which names it.
    let final_url = response.get_url().to_string();
    let mut bytes = Vec::new();
    response.into_reader().take(MAX_BYTES).read_to_end(&mut bytes).map_err(|e| format!("Download failed: {e}"))?;
    let looks_like_html = bytes.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'<');
    if looks_like_html {
        return Err("That link is a web page, not a notebook. Copy the link of the notebook file itself.".into());
    }
    let notebook = herse_nb::parse(&bytes).map_err(|e| format!("That isn't a notebook Herse can read: {e}"))?;
    let name = match file_name(&final_url).as_str() {
        "Notebook.ipynb" => file_name(&url),
        name => name.to_string(),
    };
    Ok(Fetched { url: final_url, name, notebook })
}

fn agent() -> ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .timeout(Duration::from_secs(60))
                .redirects(5)
                .user_agent(concat!("Herse/", env!("CARGO_PKG_VERSION")))
                .build()
        })
        .clone()
}

/// The page a raw notebook URL belongs to, for opening in a browser.
pub fn page_url(raw: &str) -> String {
    let rest = raw.split_once("://").map_or(raw, |(_, r)| r);
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        ["raw.githubusercontent.com", owner, repo, file @ ..] if !file.is_empty() => {
            format!("https://github.com/{owner}/{repo}/blob/{}", file.join("/"))
        }
        ["gist.githubusercontent.com", user, id, ..] => format!("https://gist.github.com/{user}/{id}"),
        _ => raw.replacen("/-/raw/", "/-/blob/", 1).replace("/resolve/", "/blob/"),
    }
}

/// Where a notebook came from, in a few words: `github.com/owner/repo`, or
/// the host.
pub fn origin_label(raw: &str) -> String {
    let rest = raw.split_once("://").map_or(raw, |(_, r)| r);
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        ["raw.githubusercontent.com", owner, repo, ..] => format!("github.com/{owner}/{repo}"),
        ["gist.githubusercontent.com", user, ..] => format!("gist.github.com/{user}"),
        [host, ..] => host.to_string(),
        [] => raw.to_string(),
    }
}

/// `example.com` from `https://example.com/a/b`, for messages.
pub fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split('/').next().unwrap_or(rest)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_links_become_raw_links() {
        let cases = [
            (
                "https://github.com/jakevdp/PythonDataScienceHandbook/blob/master/notebooks/01.00-IPython-Beyond-Normal-Python.ipynb",
                "https://raw.githubusercontent.com/jakevdp/PythonDataScienceHandbook/master/notebooks/01.00-IPython-Beyond-Normal-Python.ipynb",
            ),
            ("github.com/o/r/blob/main/a%20b.ipynb?plain=1#L3", "https://raw.githubusercontent.com/o/r/main/a%20b.ipynb"),
            ("https://www.github.com/o/r/raw/feature/x/nb.ipynb", "https://raw.githubusercontent.com/o/r/feature/x/nb.ipynb"),
            (
                "https://nbviewer.org/github/o/r/blob/v1.0/docs/nb.ipynb",
                "https://raw.githubusercontent.com/o/r/v1.0/docs/nb.ipynb",
            ),
            ("https://nbviewer.jupyter.org/urls/example.com/x/nb.ipynb", "https://example.com/x/nb.ipynb"),
            ("https://nbviewer.org/url/example.com/nb.ipynb", "http://example.com/nb.ipynb"),
            (
                "https://nbviewer.org/gist/someone/0123456789abcdef0123",
                "https://gist.githubusercontent.com/someone/0123456789abcdef0123/raw",
            ),
            (
                "https://colab.research.google.com/github/o/r/blob/main/nb.ipynb",
                "https://raw.githubusercontent.com/o/r/main/nb.ipynb",
            ),
            ("https://gist.github.com/someone/0123456789abcdef", "https://gist.githubusercontent.com/someone/0123456789abcdef/raw"),
            ("https://gist.github.com/0123456789abcdef", "https://gist.github.com/0123456789abcdef/raw"),
            ("https://gitlab.com/g/sub/p/-/blob/main/nb.ipynb", "https://gitlab.com/g/sub/p/-/raw/main/nb.ipynb"),
            ("https://huggingface.co/datasets/o/d/blob/main/nb.ipynb", "https://huggingface.co/datasets/o/d/resolve/main/nb.ipynb"),
            ("https://raw.githubusercontent.com/o/r/main/nb.ipynb", "https://raw.githubusercontent.com/o/r/main/nb.ipynb"),
            ("https://example.com/files/nb.ipynb#top", "https://example.com/files/nb.ipynb"),
        ];
        for (link, raw) in cases {
            assert_eq!(raw_url(link).as_deref(), Ok(raw), "{link}");
        }
    }

    #[test]
    fn links_that_cannot_open_say_why() {
        for link in [
            "https://github.com/o/r",
            "https://github.com/o/r/tree/main/notebooks",
            "https://colab.research.google.com/drive/1AbC",
            "herse://join/1.2.3.4:5/abc#k",
            "~/nb.ipynb",
        ] {
            assert!(raw_url(link).is_err(), "{link}");
        }
    }

    #[test]
    fn notebook_links_are_recognised() {
        assert!(is_notebook_link("https://github.com/o/r/blob/main/a.ipynb"));
        assert!(is_notebook_link("https://gist.github.com/someone/0123456789abcdef"));
        assert!(!is_notebook_link("https://github.com/o/r/blob/main/README.md"));
        assert!(!is_notebook_link("https://example.com/"));
        assert!(is_web_link("github.com/o/r/blob/main/a.ipynb"));
        assert!(!is_web_link("herse://join/x"));
    }

    #[test]
    fn names_come_from_the_url() {
        assert_eq!(file_name("https://x.org/a/My%20Analysis.ipynb?raw=1"), "My Analysis.ipynb");
        assert_eq!(file_name("https://gist.github.com/abc/raw"), "Notebook.ipynb");
    }

    #[test]
    fn raw_urls_lead_back_to_their_pages() {
        let raw = "https://raw.githubusercontent.com/o/r/main/nbs/a.ipynb";
        assert_eq!(page_url(raw), "https://github.com/o/r/blob/main/nbs/a.ipynb");
        assert_eq!(origin_label(raw), "github.com/o/r");
        let gist = "https://gist.githubusercontent.com/someone/0123abcd/raw/fff/x.ipynb";
        assert_eq!(page_url(gist), "https://gist.github.com/someone/0123abcd");
        assert_eq!(origin_label(gist), "gist.github.com/someone");
        assert_eq!(page_url("https://gitlab.com/g/p/-/raw/main/a.ipynb"), "https://gitlab.com/g/p/-/blob/main/a.ipynb");
        assert_eq!(origin_label("https://example.com/x/a.ipynb"), "example.com");
    }

    #[test]
    fn relative_links_resolve_like_a_browser() {
        let base = "https://raw.githubusercontent.com/o/r/main/notebooks/nb.ipynb";
        assert_eq!(join(base, "img/a.png"), "https://raw.githubusercontent.com/o/r/main/notebooks/img/a.png");
        assert_eq!(join(base, "./a.png"), "https://raw.githubusercontent.com/o/r/main/notebooks/a.png");
        assert_eq!(join(base, "../figures/b.svg?x=1"), "https://raw.githubusercontent.com/o/r/main/figures/b.svg?x=1");
        assert_eq!(join(base, "/top.png"), "https://raw.githubusercontent.com/top.png");
        assert_eq!(join(base, "https://cdn.example/x.png"), "https://cdn.example/x.png");
        assert_eq!(join(base, "//cdn.example/x.png"), "https://cdn.example/x.png");
        assert_eq!(join(base, "other.ipynb#Intro"), "https://raw.githubusercontent.com/o/r/main/notebooks/other.ipynb#Intro");
    }
}
