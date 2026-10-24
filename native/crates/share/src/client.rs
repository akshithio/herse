//! A small blocking client for the parts of GitHub's REST API that sharing
//! needs: the user, gists and gist comments.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Serialize};
use ureq::http::HeaderMap;

use crate::auth::Token;
use crate::error::{Result, ShareError};
use crate::links::{is_valid_gist_id, nbviewer_url};

pub const API_BASE: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = concat!("Herse/", env!("CARGO_PKG_VERSION"), " (herse-share)");
/// Gist responses echo file contents (up to 1 MB per file); be generous.
const MAX_RESPONSE_BYTES: u64 = 64 << 20;
const PER_PAGE: usize = 100;
/// At most this many comment pages (10,000 comments) are fetched.
pub const MAX_COMMENT_PAGES: usize = 100;

/// Per-request timeouts. Every call is bounded: at most `connect` to resolve
/// and connect (including TLS), `upload` to send the body, `response` to wait
/// for the response head, and `upload` again to read the body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    pub connect: Duration,
    pub response: Duration,
    pub upload: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            connect: Duration::from_secs(10),
            response: Duration::from_secs(30),
            upload: Duration::from_secs(120),
        }
    }
}

/// The signed-in user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub html_url: Option<String>,
    /// Classic and OAuth tokens: their scopes (`X-OAuth-Scopes`). `None` for
    /// fine-grained tokens, which don't report permissions.
    pub scopes: Option<Vec<String>>,
}

impl Identity {
    /// `Some(false)` if the token certainly can't write gists (a classic
    /// token without `gist`), `None` if unknown until tried (fine-grained).
    pub fn can_write_gists(&self) -> Option<bool> {
        self.scopes
            .as_ref()
            .map(|s| s.iter().any(|scope| scope == "gist"))
    }
}

/// A gist (file contents are not kept).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gist {
    pub id: String,
    pub html_url: String,
    pub public: bool,
    pub description: Option<String>,
    /// Owner login.
    pub owner: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Number of revisions, from the gist's `history` (0 in responses that
    /// don't include it).
    pub revisions: usize,
    /// Number of comments.
    pub comments: u64,
    /// Files, sorted by name.
    pub files: Vec<GistFile>,
}

impl Gist {
    pub fn nbviewer_url(&self) -> String {
        nbviewer_url(&self.id)
    }

    /// The first `.ipynb` file.
    pub fn notebook_file(&self) -> Option<&GistFile> {
        self.files
            .iter()
            .find(|f| f.filename.to_ascii_lowercase().ends_with(".ipynb"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GistFile {
    pub filename: String,
    /// Raw URL of this revision of the file.
    pub raw_url: Option<String>,
    pub size: u64,
    /// GitHub truncates file contents over 1 MB in API responses (the raw
    /// URL still has all of it).
    pub truncated: bool,
}

/// A gist comment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    pub id: u64,
    /// Markdown, as written.
    pub body: String,
    /// Author login (`None` for deleted accounts).
    pub author: Option<String>,
    pub author_avatar_url: Option<String>,
    /// `OWNER`, `COLLABORATOR`, `NONE`, ...
    pub author_association: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A gist to create.
#[derive(Clone, Copy, Debug)]
pub struct NewGist<'a> {
    pub filename: &'a str,
    pub content: &'a str,
    pub description: &'a str,
    /// `false` = secret (unlisted: not on the profile or in search, but anyone
    /// with the link can see it).
    pub public: bool,
}

/// New content for an existing gist (a new revision).
#[derive(Clone, Copy, Debug)]
pub struct GistUpdate<'a> {
    pub filename: &'a str,
    pub content: &'a str,
    /// `None` keeps the current description.
    pub description: Option<&'a str>,
    /// Rename this file of the gist to `filename` (the notebook was renamed).
    /// `None` updates (or adds) `filename`.
    pub rename_from: Option<&'a str>,
}

/// GitHub REST client. `Clone + Send + Sync`; clones share the connection
/// pool. Every method blocks the calling thread for one HTTP round trip
/// (`list_comments`: one per 100 comments), bounded by [`Timeouts`]; call it
/// from a background thread or executor, never the UI thread.
#[derive(Clone)]
pub struct GistClient {
    agent: ureq::Agent,
    base: String,
    token: Token,
    login: Arc<Mutex<Option<String>>>,
}

impl fmt::Debug for GistClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GistClient")
            .field("base", &self.base)
            .field("token", &self.token)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verb {
    Get,
    Post,
    Patch,
    Delete,
}

impl Verb {
    fn is_write(self) -> bool {
        self != Verb::Get
    }
}

struct RawResponse {
    headers: HeaderMap,
    body: String,
}

impl GistClient {
    /// A client for api.github.com.
    pub fn new(token: Token) -> GistClient {
        GistClient::with_options(token, API_BASE, Timeouts::default())
    }

    /// A client for another API base (GitHub Enterprise's
    /// `https://host/api/v3`, or a local mock server in tests).
    pub fn with_base_url(token: Token, base: &str) -> GistClient {
        GistClient::with_options(token, base, Timeouts::default())
    }

    pub fn with_options(token: Token, base: &str, timeouts: Timeouts) -> GistClient {
        let base = base.trim_end_matches('/').to_string();
        let loopback = ["http://127.", "http://localhost", "http://[::1]"]
            .iter()
            .any(|p| base.starts_with(p));
        let mut config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            // The token must never travel in clear text (loopback is for tests).
            .https_only(!loopback)
            .user_agent(USER_AGENT)
            .accept("application/vnd.github+json")
            .timeout_resolve(Some(timeouts.connect))
            .timeout_connect(Some(timeouts.connect))
            .timeout_send_request(Some(timeouts.connect))
            .timeout_send_body(Some(timeouts.upload))
            .timeout_recv_response(Some(timeouts.response))
            .timeout_recv_body(Some(timeouts.upload))
            .max_redirects(5);
        if loopback {
            config = config.proxy(None);
        }
        GistClient {
            agent: config.build().into(),
            base,
            token,
            login: Arc::new(Mutex::new(None)),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// Validate the token: `GET /user`. Gives the login and, for classic
    /// tokens, the scopes (check [`Identity::can_write_gists`]).
    pub fn whoami(&self) -> Result<Identity> {
        let resp = self.request(Verb::Get, &self.url("/user"), None)?;
        let user: RawUser = decode(&resp.body)?;
        let scopes = header(&resp.headers, "x-oauth-scopes").map(parse_scopes);
        *self.login.lock().unwrap_or_else(|e| e.into_inner()) = Some(user.login.clone());
        Ok(Identity {
            login: user.login,
            name: user.name,
            avatar_url: user.avatar_url,
            html_url: user.html_url,
            scopes,
        })
    }

    /// The signed-in login, from the last [`GistClient::whoami`] (calls it
    /// the first time).
    pub fn login(&self) -> Result<String> {
        if let Some(login) = self.login.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(login);
        }
        Ok(self.whoami()?.login)
    }

    /// `POST /gists`.
    pub fn create_gist(&self, gist: &NewGist<'_>) -> Result<Gist> {
        let body = CreateBody {
            description: gist.description,
            public: gist.public,
            files: BTreeMap::from([(
                gist.filename,
                FileBody {
                    filename: None,
                    content: gist.content,
                },
            )]),
        };
        let resp = self.request(Verb::Post, &self.url("/gists"), Some(encode(&body)?))?;
        Ok(decode::<RawGist>(&resp.body)?.into())
    }

    /// `PATCH /gists/{id}`: a new revision of the same gist (same links).
    pub fn update_gist(&self, gist_id: &str, update: &GistUpdate<'_>) -> Result<Gist> {
        check_gist_id(gist_id)?;
        let (key, rename) = match update.rename_from {
            Some(old) if old != update.filename => (old, Some(update.filename)),
            _ => (update.filename, None),
        };
        let body = UpdateBody {
            description: update.description,
            files: BTreeMap::from([(
                key,
                FileBody {
                    filename: rename,
                    content: update.content,
                },
            )]),
        };
        let resp = self.request(
            Verb::Patch,
            &self.url(&format!("/gists/{gist_id}")),
            Some(encode(&body)?),
        )?;
        Ok(decode::<RawGist>(&resp.body)?.into())
    }

    /// `GET /gists/{id}`: html_url, visibility, revision count, updated_at.
    pub fn get_gist(&self, gist_id: &str) -> Result<Gist> {
        check_gist_id(gist_id)?;
        let resp = self.request(Verb::Get, &self.url(&format!("/gists/{gist_id}")), None)?;
        Ok(decode::<RawGist>(&resp.body)?.into())
    }

    /// `DELETE /gists/{id}`. The links stop working.
    pub fn delete_gist(&self, gist_id: &str) -> Result<()> {
        check_gist_id(gist_id)?;
        self.request(Verb::Delete, &self.url(&format!("/gists/{gist_id}")), None)?;
        Ok(())
    }

    /// All comments, oldest first (`GET /gists/{id}/comments`, 100 per page,
    /// following `Link: rel="next"`, at most [`MAX_COMMENT_PAGES`] pages).
    pub fn list_comments(&self, gist_id: &str) -> Result<Vec<Comment>> {
        check_gist_id(gist_id)?;
        let page_url = |page: usize| {
            self.url(&format!(
                "/gists/{gist_id}/comments?per_page={PER_PAGE}&page={page}"
            ))
        };
        let mut url = self.url(&format!("/gists/{gist_id}/comments?per_page={PER_PAGE}"));
        let mut comments = Vec::new();
        for page in 2..=MAX_COMMENT_PAGES + 1 {
            let resp = self.request(Verb::Get, &url, None)?;
            let batch: Vec<RawComment> = decode(&resp.body)?;
            let got = batch.len();
            comments.extend(batch.into_iter().map(Comment::from));
            let Some(next) = link_next(&resp.headers) else {
                break;
            };
            if got == 0 || page > MAX_COMMENT_PAGES {
                break;
            }
            // Only send the token to our own API host; otherwise page by number.
            let next = if self.is_own_url(&next) {
                next
            } else {
                page_url(page)
            };
            if next == url {
                break;
            }
            url = next;
        }
        Ok(comments)
    }

    /// `POST /gists/{id}/comments`.
    pub fn create_comment(&self, gist_id: &str, body: &str) -> Result<Comment> {
        check_gist_id(gist_id)?;
        let payload = encode(&CommentBody {
            body: check_comment(body)?,
        })?;
        let resp = self.request(
            Verb::Post,
            &self.url(&format!("/gists/{gist_id}/comments")),
            Some(payload),
        )?;
        Ok(decode::<RawComment>(&resp.body)?.into())
    }

    /// `PATCH /gists/{id}/comments/{comment_id}` (own comments only).
    pub fn update_comment(&self, gist_id: &str, comment_id: u64, body: &str) -> Result<Comment> {
        check_gist_id(gist_id)?;
        let payload = encode(&CommentBody {
            body: check_comment(body)?,
        })?;
        let url = self.url(&format!("/gists/{gist_id}/comments/{comment_id}"));
        let resp = self.request(Verb::Patch, &url, Some(payload))?;
        Ok(decode::<RawComment>(&resp.body)?.into())
    }

    /// `DELETE /gists/{id}/comments/{comment_id}` (own comments, or any
    /// comment on the user's own gist).
    pub fn delete_comment(&self, gist_id: &str, comment_id: u64) -> Result<()> {
        check_gist_id(gist_id)?;
        self.request(
            Verb::Delete,
            &self.url(&format!("/gists/{gist_id}/comments/{comment_id}")),
            None,
        )?;
        Ok(())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn is_own_url(&self, url: &str) -> bool {
        url.strip_prefix(self.base.as_str())
            .is_some_and(|rest| rest.starts_with('/'))
    }

    fn request(&self, verb: Verb, url: &str, body: Option<Vec<u8>>) -> Result<RawResponse> {
        log::debug!("GitHub {verb:?} {url}");
        let auth = format!("Bearer {}", self.token.expose());
        macro_rules! with_headers {
            ($builder:expr) => {
                $builder
                    .header("Authorization", auth.as_str())
                    .header("X-GitHub-Api-Version", API_VERSION)
            };
        }
        let body = body.unwrap_or_default();
        let result = match verb {
            Verb::Get => with_headers!(self.agent.get(url)).call(),
            Verb::Delete => with_headers!(self.agent.delete(url)).call(),
            Verb::Post => with_headers!(self.agent.post(url))
                .header("Content-Type", "application/json")
                .send(&body[..]),
            Verb::Patch => with_headers!(self.agent.patch(url))
                .header("Content-Type", "application/json")
                .send(&body[..]),
        };
        let mut resp = result.map_err(transport_error)?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .lossy_utf8(true)
            .read_to_string()
            .map_err(transport_error)?;
        if (200..300).contains(&status) {
            Ok(RawResponse { headers, body })
        } else {
            log::debug!("GitHub {verb:?} {url} -> {status}");
            Err(status_error(verb.is_write(), status, &headers, &body))
        }
    }
}

fn check_gist_id(id: &str) -> Result<()> {
    if is_valid_gist_id(id) {
        Ok(())
    } else {
        Err(ShareError::InvalidInput(format!("{id:?} isn't a gist id")))
    }
}

fn check_comment(body: &str) -> Result<&str> {
    if body.trim().is_empty() {
        return Err(ShareError::InvalidInput("The comment is empty.".into()));
    }
    Ok(body)
}

fn header<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn parse_scopes(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// `Link: <url>; rel="next", <url>; rel="last"` → the next URL.
fn link_next(headers: &HeaderMap) -> Option<String> {
    let link = header(headers, "link")?;
    link.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        let is_next = params.split(';').any(|p| {
            let p = p.trim();
            p.strip_prefix("rel=")
                .is_some_and(|v| v.trim_matches('"').split_whitespace().any(|r| r == "next"))
        });
        let url = url.trim().strip_prefix('<')?.strip_suffix('>')?;
        is_next.then(|| url.to_string())
    })
}

fn transport_error(e: ureq::Error) -> ShareError {
    use std::io::ErrorKind;
    let network = |message: String, timed_out: bool| ShareError::Network { message, timed_out };
    match e {
        ureq::Error::Timeout(_) => network("timed out".into(), true),
        ureq::Error::HostNotFound => network("couldn't resolve the server's address".into(), false),
        ureq::Error::Io(io) if matches!(io.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
            network("timed out".into(), true)
        }
        ureq::Error::Io(io) => network(io.to_string(), false),
        ureq::Error::Tls(msg) => network(format!("secure connection failed: {msg}"), false),
        ureq::Error::ConnectionFailed => network("connection failed".into(), false),
        ureq::Error::Protocol(p) => ShareError::BadResponse(p.to_string()),
        ureq::Error::BodyExceedsLimit(limit) => {
            ShareError::BadResponse(format!("response larger than {} MB", limit >> 20))
        }
        ureq::Error::BadUri(uri) => ShareError::InvalidInput(format!("bad API URL {uri}")),
        ureq::Error::RequireHttpsOnly(uri) => ShareError::InvalidInput(format!(
            "refusing to send the token over plain http to {uri}"
        )),
        other => network(other.to_string(), false),
    }
}

/// Map an error status to a [`ShareError`]. `write` = POST/PATCH/DELETE.
pub(crate) fn status_error(
    write: bool,
    status: u16,
    headers: &HeaderMap,
    body: &str,
) -> ShareError {
    let parsed: Option<ApiError> = serde_json::from_str(body).ok();
    let message = match &parsed {
        Some(ApiError {
            message: Some(m), ..
        }) => m.clone(),
        _ => body
            .chars()
            .take(300)
            .collect::<String>()
            .trim()
            .to_string(),
    };
    let scopes = header(headers, "x-oauth-scopes").map(parse_scopes);
    let accepted = header(headers, "x-accepted-oauth-scopes").map(parse_scopes);
    let num = |name: &str| header(headers, name).and_then(|v| v.trim().parse::<u64>().ok());
    let remaining = num("x-ratelimit-remaining");
    let retry_after = num("retry-after");
    let lower = message.to_ascii_lowercase();
    // Classic tokens report their scopes: a write without `gist` can't work.
    let lacks_gist = scopes
        .as_ref()
        .is_some_and(|s| !s.iter().any(|scope| scope == "gist"))
        && (write
            || accepted
                .as_ref()
                .is_some_and(|a| a.iter().any(|scope| scope == "gist")));
    match status {
        401 => ShareError::BadToken,
        403 | 429
            if status == 429
                || remaining == Some(0)
                || retry_after.is_some()
                || lower.contains("rate limit") =>
        {
            ShareError::RateLimited {
                reset_at: if remaining == Some(0) || retry_after.is_none() {
                    num("x-ratelimit-reset")
                } else {
                    None
                },
                retry_after,
            }
        }
        403 | 404 if lacks_gist => ShareError::MissingGistScope { scopes },
        // Fine-grained tokens without the Gists permission.
        403 if lower.contains("not accessible by") => ShareError::MissingGistScope { scopes },
        403 => ShareError::Forbidden { message },
        404 => ShareError::NotFound,
        422 => {
            let details: Vec<String> = parsed
                .map(|p| p.errors)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|e| match e {
                    serde_json::Value::String(s) => Some(s),
                    serde_json::Value::Object(o) => {
                        let text = |k: &str| o.get(k).and_then(|v| v.as_str()).map(str::to_string);
                        text("message").or_else(|| {
                            let code = text("code")?;
                            Some(match text("field") {
                                Some(field) => format!("{field}: {code}"),
                                None => code,
                            })
                        })
                    }
                    _ => None,
                })
                .collect();
            let message = if details.is_empty() {
                message
            } else {
                format!("{message} ({})", details.join("; "))
            };
            ShareError::Rejected { message }
        }
        _ => ShareError::Http { status, message },
    }
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value)
        .map_err(|e| ShareError::InvalidInput(format!("couldn't encode the request: {e}")))
}

fn decode<T: DeserializeOwned>(body: &str) -> Result<T> {
    serde_json::from_str(body).map_err(|e| {
        let snippet: String = body.chars().take(80).collect();
        ShareError::BadResponse(format!("{e} (got {snippet:?})"))
    })
}

#[derive(Serialize)]
struct FileBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    filename: Option<&'a str>,
    content: &'a str,
}

#[derive(Serialize)]
struct CreateBody<'a> {
    description: &'a str,
    public: bool,
    files: BTreeMap<&'a str, FileBody<'a>>,
}

#[derive(Serialize)]
struct UpdateBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    files: BTreeMap<&'a str, FileBody<'a>>,
}

#[derive(Serialize)]
struct CommentBody<'a> {
    body: &'a str,
}

#[derive(Deserialize)]
struct ApiError {
    message: Option<String>,
    #[serde(default)]
    errors: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct RawUser {
    login: String,
    name: Option<String>,
    avatar_url: Option<String>,
    html_url: Option<String>,
}

#[derive(Deserialize)]
struct RawFile {
    filename: Option<String>,
    raw_url: Option<String>,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    truncated: bool,
}

#[derive(Deserialize)]
struct RawGist {
    id: String,
    html_url: String,
    #[serde(default)]
    public: bool,
    description: Option<String>,
    owner: Option<RawUser>,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    files: BTreeMap<String, Option<RawFile>>,
    #[serde(default)]
    history: Vec<IgnoredAny>,
    #[serde(default)]
    comments: u64,
}

impl From<RawGist> for Gist {
    fn from(raw: RawGist) -> Gist {
        let files = raw
            .files
            .into_iter()
            .map(|(name, file)| match file {
                Some(f) => GistFile {
                    filename: f.filename.unwrap_or(name),
                    raw_url: f.raw_url,
                    size: f.size,
                    truncated: f.truncated,
                },
                None => GistFile {
                    filename: name,
                    raw_url: None,
                    size: 0,
                    truncated: false,
                },
            })
            .collect();
        Gist {
            id: raw.id,
            html_url: raw.html_url,
            public: raw.public,
            description: raw.description.filter(|d| !d.is_empty()),
            owner: raw.owner.map(|o| o.login),
            created_at: raw.created_at,
            updated_at: raw.updated_at,
            revisions: raw.history.len(),
            comments: raw.comments,
            files,
        }
    }
}

#[derive(Deserialize)]
struct RawComment {
    id: u64,
    #[serde(default)]
    body: String,
    user: Option<RawUser>,
    author_association: Option<String>,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
}

impl From<RawComment> for Comment {
    fn from(raw: RawComment) -> Comment {
        let (author, author_avatar_url) = match raw.user {
            Some(u) => (Some(u.login), u.avatar_url),
            None => (None, None),
        };
        Comment {
            id: raw.id,
            body: raw.body,
            author,
            author_avatar_url,
            author_association: raw.author_association,
            created_at: raw.created_at,
            updated_at: raw.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ureq::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        map
    }

    #[test]
    fn link_header() {
        let h = headers(&[(
            "link",
            r#"<https://api.github.com/gists/1/comments?per_page=100&page=2>; rel="next", <https://api.github.com/gists/1/comments?per_page=100&page=5>; rel="last""#,
        )]);
        assert_eq!(
            link_next(&h).as_deref(),
            Some("https://api.github.com/gists/1/comments?per_page=100&page=2")
        );
        let h = headers(&[(
            "link",
            r#"<https://api.github.com/x?page=1>; rel="prev", <https://api.github.com/x?page=1>; rel="first""#,
        )]);
        assert_eq!(link_next(&h), None);
        assert_eq!(link_next(&HeaderMap::new()), None);
    }

    #[test]
    fn handles_cross_threads() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<GistClient>();
        assert_send_sync::<ShareError>();
        assert_send_sync::<Gist>();
        assert_send_sync::<Comment>();
    }

    #[test]
    fn own_url_check() {
        let token = Token::new("ghp_test").unwrap();
        let client = GistClient::with_base_url(token, "https://api.github.com/");
        assert_eq!(client.base_url(), "https://api.github.com");
        assert!(client.is_own_url("https://api.github.com/gists/1/comments?page=2"));
        assert!(!client.is_own_url("https://api.github.com.evil.example/gists"));
        assert!(!client.is_own_url("https://evil.example/https://api.github.com/"));
        // The token isn't in Debug output.
        assert!(!format!("{client:?}").contains("ghp_test"));
    }

    #[test]
    fn status_mapping() {
        let none = HeaderMap::new();
        assert_eq!(
            status_error(false, 401, &none, r#"{"message":"Bad credentials"}"#),
            ShareError::BadToken
        );
        assert_eq!(
            status_error(false, 404, &none, r#"{"message":"Not Found"}"#),
            ShareError::NotFound
        );
        // A read of a deleted gist with a gist-less classic token is still "not found".
        let classic = headers(&[("x-oauth-scopes", "repo, read:org")]);
        assert_eq!(
            status_error(false, 404, &classic, "{}"),
            ShareError::NotFound
        );
        assert_eq!(
            status_error(true, 404, &classic, "{}"),
            ShareError::MissingGistScope {
                scopes: Some(vec!["repo".into(), "read:org".into()])
            }
        );
        let with_gist = headers(&[("x-oauth-scopes", "gist, repo")]);
        assert_eq!(
            status_error(true, 404, &with_gist, "{}"),
            ShareError::NotFound
        );
        assert_eq!(
            status_error(true, 403, &headers(&[("x-oauth-scopes", "")]), "{}"),
            ShareError::MissingGistScope {
                scopes: Some(vec![])
            }
        );
        assert_eq!(
            status_error(
                true,
                403,
                &none,
                r#"{"message":"Resource not accessible by personal access token"}"#
            ),
            ShareError::MissingGistScope { scopes: None }
        );
        assert_eq!(
            status_error(true, 403, &with_gist, r#"{"message":"Nope"}"#),
            ShareError::Forbidden {
                message: "Nope".into()
            }
        );
        let limited = headers(&[
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-reset", "1700000000"),
        ]);
        assert_eq!(
            status_error(
                false,
                403,
                &limited,
                r#"{"message":"API rate limit exceeded"}"#
            ),
            ShareError::RateLimited {
                reset_at: Some(1_700_000_000),
                retry_after: None
            }
        );
        assert_eq!(
            status_error(true, 429, &headers(&[("retry-after", "60")]), ""),
            ShareError::RateLimited {
                reset_at: None,
                retry_after: Some(60)
            }
        );
        assert_eq!(
            status_error(
                true,
                422,
                &none,
                r#"{"message":"Validation Failed","errors":[{"resource":"Gist","code":"missing_field","field":"files"}]}"#
            ),
            ShareError::Rejected {
                message: "Validation Failed (files: missing_field)".into()
            }
        );
        assert_eq!(
            status_error(false, 502, &none, "<html>Bad gateway</html>"),
            ShareError::Http {
                status: 502,
                message: "<html>Bad gateway</html>".into()
            }
        );
    }
}
