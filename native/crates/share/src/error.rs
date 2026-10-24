//! Typed errors. `Display` is a sentence fit to show the user as is.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

pub type Result<T, E = ShareError> = std::result::Result<T, E>;

/// Everything that can go wrong while signing in, publishing or commenting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShareError {
    /// No token: `gh` is missing or signed out and nothing is in the Keychain.
    NoToken,
    /// GitHub rejected the token (401): mistyped, expired or revoked.
    BadToken,
    /// The token is valid but may not write gists: a classic token without
    /// the `gist` scope (`scopes` lists the ones it has, from
    /// `X-OAuth-Scopes`), or a fine-grained token without the "Gists" account
    /// permission (`scopes` is `None`: fine-grained tokens report none).
    MissingGistScope { scopes: Option<Vec<String>> },
    /// Primary (`X-RateLimit-Remaining: 0`) or secondary (`Retry-After`)
    /// rate limit. `reset_at` is Unix seconds (`X-RateLimit-Reset`),
    /// `retry_after` is seconds from when the response arrived.
    RateLimited {
        reset_at: Option<u64>,
        retry_after: Option<u64>,
    },
    /// The gist or comment does not exist, or this token can't see it.
    NotFound,
    /// A 403 that is neither a rate limit nor a missing scope.
    Forbidden { message: String },
    /// 422: GitHub refused the request (empty or oversized file, bad name, ...).
    Rejected { message: String },
    /// The notebook is already shared with the other visibility. GitHub
    /// can't change a gist between public and secret; publish with
    /// [`crate::PublishOptions::new_gist`] to make a new link instead.
    VisibilityMismatch { gist_public: bool },
    /// Any other HTTP error status.
    Http { status: u16, message: String },
    /// GitHub couldn't be reached: offline, DNS, refused, TLS, timeout.
    Network { message: String, timed_out: bool },
    /// GitHub answered with something that isn't the expected JSON.
    BadResponse(String),
    /// `gh` was found but failed in an unexpected way.
    GhCli(String),
    /// The `security` tool failed to read, store or delete the token.
    Keychain(String),
    /// A caller-supplied value is malformed (gist id, token, login, ...).
    InvalidInput(String),
}

impl ShareError {
    /// The fix is a (new) token: show the sign-in / paste-token flow.
    pub fn is_auth_problem(&self) -> bool {
        matches!(
            self,
            ShareError::NoToken | ShareError::BadToken | ShareError::MissingGistScope { .. }
        )
    }

    /// Trying again later may work (network trouble, rate limits, 5xx).
    pub fn is_retryable(&self) -> bool {
        match self {
            ShareError::Network { .. } | ShareError::RateLimited { .. } => true,
            ShareError::Http { status, .. } => *status >= 500,
            _ => false,
        }
    }

    /// For [`ShareError::RateLimited`]: seconds to wait, measured from `now`.
    pub fn wait_secs(&self, now: SystemTime) -> Option<u64> {
        let ShareError::RateLimited {
            reset_at,
            retry_after,
        } = self
        else {
            return None;
        };
        if let Some(secs) = retry_after {
            return Some(*secs);
        }
        let now = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
        reset_at.map(|reset| reset.saturating_sub(now))
    }
}

fn wait_phrase(secs: u64) -> String {
    match secs {
        0..=59 => "in a minute".to_string(),
        60..=5399 => {
            let minutes = secs.div_ceil(60);
            format!("in {minutes} minute{}", if minutes == 1 { "" } else { "s" })
        }
        _ => {
            let hours = secs.div_ceil(3600);
            format!("in {hours} hours")
        }
    }
}

impl fmt::Display for ShareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShareError::NoToken => write!(
                f,
                "Not signed in to GitHub. Run `gh auth login` or paste a token with the gist scope."
            ),
            ShareError::BadToken => write!(
                f,
                "GitHub rejected the token (it may be mistyped, expired or revoked). Sign in again."
            ),
            ShareError::MissingGistScope {
                scopes: Some(scopes),
            } => {
                let has = if scopes.is_empty() {
                    "no scopes".to_string()
                } else {
                    scopes.join(", ")
                };
                write!(
                    f,
                    "The GitHub token can't create gists: it needs the `gist` scope (it has {has})."
                )
            }
            ShareError::MissingGistScope { scopes: None } => write!(
                f,
                "The GitHub token can't create gists. Give it the \"Gists: Read and write\" account permission, or use a classic token with the `gist` scope."
            ),
            ShareError::RateLimited { .. } => match self.wait_secs(SystemTime::now()) {
                Some(secs) => write!(
                    f,
                    "GitHub's rate limit was reached. Try again {}.",
                    wait_phrase(secs)
                ),
                None => write!(f, "GitHub's rate limit was reached. Try again later."),
            },
            ShareError::NotFound => write!(
                f,
                "GitHub couldn't find that gist or comment (it may have been deleted)."
            ),
            ShareError::Forbidden { message } => write!(f, "GitHub refused the request: {message}"),
            ShareError::Rejected { message } => write!(f, "GitHub rejected the upload: {message}"),
            ShareError::VisibilityMismatch { gist_public: true } => write!(
                f,
                "This notebook is already shared publicly. GitHub can't make an existing gist secret; publish a new link instead."
            ),
            ShareError::VisibilityMismatch { gist_public: false } => write!(
                f,
                "This notebook is already shared as an unlisted (secret) gist. GitHub can't make it public; publish a new link instead."
            ),
            ShareError::Http { status, message } if message.is_empty() => {
                write!(f, "GitHub returned HTTP {status}.")
            }
            ShareError::Http { status, message } => {
                write!(f, "GitHub returned HTTP {status}: {message}")
            }
            ShareError::Network {
                timed_out: true, ..
            } => {
                write!(
                    f,
                    "GitHub didn't respond in time. Check your connection and try again."
                )
            }
            ShareError::Network { message, .. } => {
                write!(
                    f,
                    "Couldn't reach GitHub ({message}). Check your connection and try again."
                )
            }
            ShareError::BadResponse(detail) => {
                write!(f, "Unexpected response from GitHub: {detail}")
            }
            ShareError::GhCli(detail) => write!(f, "The GitHub CLI (gh) failed: {detail}"),
            ShareError::Keychain(detail) => write!(f, "Keychain error: {detail}"),
            ShareError::InvalidInput(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for ShareError {}
