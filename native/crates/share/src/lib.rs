//! `herse-share`: publish a notebook as a GitHub gist, share it through
//! nbviewer, and read / write the gist's comments. No Herse server.
//!
//! ```no_run
//! use herse_share::*;
//! # fn main() -> Result<(), ShareError> {
//! let mut nb = herse_nb::parse(&std::fs::read("analysis.ipynb").unwrap()).unwrap();
//!
//! // Before publishing: show these to the user.
//! let report = scan(&nb, &ScanOptions::default());
//! for w in &report.warnings {
//!     println!("{}", w.message());
//! }
//!
//! let (token, _source) = discover_token()?.ok_or(ShareError::NoToken)?;
//! let client = GistClient::new(token);
//! let me = client.whoami()?;                       // validates the token
//! let published = publish(&client, &mut nb, &PublishOptions::new("analysis.ipynb", false))?;
//! println!("{} (as @{})", published.info.nbviewer_url(), me.login);
//! // nb now carries metadata.herse.share: save it so re-publishing updates this gist.
//!
//! for c in client.list_comments(&published.info.gist_id)? {
//!     println!("@{}: {}", c.author.unwrap_or_default(), c.body);
//! }
//! client.create_comment(&published.info.gist_id, "Thanks for reading!")?;
//! # Ok(()) }
//! ```
//!
//! # Threading
//!
//! Everything is synchronous. [`scan`], [`prepare_for_publish`],
//! [`publish_content`], the metadata and link helpers are pure and fast (the
//! scan is a few ms for a typical notebook, ~100 ms for tens of MB of text).
//! Network calls ([`GistClient`] methods, [`publish`], [`unpublish`]) and
//! token lookups ([`gh_token`], [`discover_token`], [`Keychain`]) block: run
//! them on a background thread / executor. Network calls are bounded by
//! [`Timeouts`] (10 s connect, 30 s waiting for a response, 120 s
//! upload/download); subprocesses by 10–15 s.
//!
//! # Links
//!
//! A published notebook has three URLs: the nbviewer rendering
//! ([`nbviewer_url`], the one to share), the gist page ([`gist_html_url`],
//! where revisions and comments live) and the raw file ([`raw_url`]). A
//! secret gist is unlisted, not private: anyone with a link can read it.

mod auth;
mod client;
mod error;
mod links;
mod meta;
mod prepare;
mod publish;
mod scan;

pub use auth::{
    GH_EXTRA_DIRS, GH_TIMEOUT, KEYCHAIN_SERVICE, Keychain, Token, TokenKind, TokenSource,
    discover_token, find_gh, find_gh_in, gh_token, gh_token_with,
};
pub use client::{
    API_BASE, Comment, Gist, GistClient, GistFile, GistUpdate, Identity, MAX_COMMENT_PAGES,
    NewGist, Timeouts,
};
pub use error::{Result, ShareError};
pub use links::{
    GIST_RAW_BASE, GIST_WEB_BASE, NBVIEWER_BASE, gist_filename, gist_html_url, is_valid_gist_id,
    nbviewer_url, parse_timestamp, raw_url,
};
pub use meta::{
    META_KEY, META_NAMESPACE, ShareInfo, clear_share_info, read_share_info, write_share_info,
};
pub use prepare::{PrepareOptions, prepare_for_publish, publish_content, strip_outputs};
pub use publish::{PublishOptions, Published, publish, unpublish};
pub use scan::{Location, Place, ScanOptions, ScanReport, SecretKind, Warning, WarningKind, scan};
