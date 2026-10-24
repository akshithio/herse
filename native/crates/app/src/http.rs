//! The HTTP client GPUI loads image URIs through. Notebooks reference images
//! three ways — remote URLs, files next to the notebook, and embedded data —
//! and the first two both arrive here as URIs: `https://…`, and
//! `file://localhost/…` (relative paths are rewritten to absolute file URIs
//! before Markdown parses; `HERSE_DEBUG_IMAGES=1` logs each fetch).

use std::{sync::Arc, time::Duration};

use futures::future::BoxFuture;
use gpui_kit::http_client::{self, AsyncBody, HttpClient, Response, Url, http::HeaderValue};

/// Images larger than this aren't worth stalling a notebook for.
const MAX_BYTES: u64 = 25 * 1024 * 1024;

pub struct ImageClient {
    /// Built on first use: TLS setup isn't worth paying at launch.
    agent: std::sync::OnceLock<ureq::Agent>,
    user_agent: HeaderValue,
}

impl ImageClient {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            agent: std::sync::OnceLock::new(),
            user_agent: HeaderValue::from_static(concat!("Herse/", env!("CARGO_PKG_VERSION"))),
        })
    }

    fn agent(&self) -> ureq::Agent {
        self.agent
            .get_or_init(|| {
                ureq::AgentBuilder::new()
                    .timeout_connect(Duration::from_secs(10))
                    .timeout(Duration::from_secs(30))
                    .redirects(5)
                    .build()
            })
            .clone()
    }
}

impl HttpClient for ImageClient {
    fn user_agent(&self) -> Option<&HeaderValue> {
        Some(&self.user_agent)
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }

    fn send(&self, req: http_client::Request<AsyncBody>) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        let uri = req.uri().to_string();
        let agent = if uri.starts_with("file://") { None } else { Some(self.agent()) };
        Box::pin(async move {
            // Blocking I/O on its own thread; the future just waits for it.
            let (tx, rx) = futures::channel::oneshot::channel();
            std::thread::spawn(move || {
                let result = fetch(agent, &uri);
                if std::env::var_os("HERSE_DEBUG_IMAGES").is_some() {
                    match &result {
                        Ok(bytes) => eprintln!("image {uri}: {} bytes", bytes.len()),
                        Err(e) => eprintln!("image {uri}: {e:#}"),
                    }
                }
                let _ = tx.send(result);
            });
            let bytes = rx.await.map_err(|_| anyhow::anyhow!("fetch thread vanished"))??;
            Ok(Response::builder().status(200).body(AsyncBody::from(bytes))?)
        })
    }
}

fn fetch(agent: Option<ureq::Agent>, uri: &str) -> anyhow::Result<Vec<u8>> {
    if let Some(path) = uri.strip_prefix("file://") {
        let path = percent_decode(path.strip_prefix("localhost").unwrap_or(path));
        let meta = std::fs::metadata(&path)?;
        anyhow::ensure!(meta.len() <= MAX_BYTES, "image too large");
        return Ok(std::fs::read(&path)?);
    }
    anyhow::ensure!(uri.starts_with("http://") || uri.starts_with("https://"), "unsupported URI");
    let agent = agent.ok_or_else(|| anyhow::anyhow!("no HTTP agent"))?;
    let response = agent.get(uri).call()?;
    let mut bytes = Vec::new();
    use std::io::Read as _;
    response.into_reader().take(MAX_BYTES).read_to_end(&mut bytes)?;
    Ok(bytes)
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

pub fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '/' | '-' | '_' | '.' | '~' => out.push(ch),
            c => {
                let mut buf = [0u8; 4];
                for byte in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
        }
    }
    out
}
