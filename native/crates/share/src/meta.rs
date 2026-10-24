//! The notebook remembers its gist in its metadata, so publishing again
//! updates the same gist:
//!
//! ```json
//! "metadata": {
//!   "herse": {
//!     "share": {
//!       "gist_id": "aa5a315d61ae9438b18d",
//!       "public": false,
//!       "html_url": "https://gist.github.com/aa5a315d61ae9438b18d",
//!       "published_at": "2026-10-16T09:30:00Z",
//!       "filename": "analysis.ipynb",
//!       "owner": "octocat"
//!     }
//!   }
//! }
//! ```
//!
//! `gist_id` is required; `public` defaults to `false`, `html_url` to the
//! gist page for `gist_id`, `published_at` (GitHub's `updated_at` of the last
//! publish) to `""`; `filename` and `owner` are optional. Other keys under
//! `herse` and `herse.share` are preserved. The published copy never carries
//! `herse.share` (see [`crate::prepare_for_publish`]). For a secret gist the
//! id *is* the secret, so the saved `.ipynb` reveals the link to anyone it's
//! sent to.

use herse_nb::{Map, Notebook, Value};

use crate::client::Gist;
use crate::links::{gist_html_url, is_valid_gist_id, nbviewer_url, raw_url};

/// `metadata[META_NAMESPACE][META_KEY]`.
pub const META_NAMESPACE: &str = "herse";
pub const META_KEY: &str = "share";

/// Where a notebook was published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareInfo {
    pub gist_id: String,
    /// Public (listed on the user's profile, searchable) or secret
    /// (unlisted: anyone with the link can see it).
    pub public: bool,
    pub html_url: String,
    /// GitHub's `updated_at` for the last publish (ISO 8601 UTC).
    pub published_at: String,
    /// The `.ipynb` file name in the gist.
    pub filename: Option<String>,
    /// The gist owner's login.
    pub owner: Option<String>,
}

impl ShareInfo {
    /// From a gist that was just created or updated.
    pub fn from_gist(gist: &Gist, filename: &str) -> ShareInfo {
        ShareInfo {
            gist_id: gist.id.clone(),
            public: gist.public,
            html_url: gist.html_url.clone(),
            published_at: gist.updated_at.clone(),
            filename: Some(filename.to_string()),
            owner: gist.owner.clone(),
        }
    }

    /// The nbviewer link (the one to share).
    pub fn nbviewer_url(&self) -> String {
        nbviewer_url(&self.gist_id)
    }

    /// The raw `.ipynb` URL, when owner and filename are known.
    pub fn raw_url(&self) -> Option<String> {
        Some(raw_url(
            self.owner.as_deref()?,
            &self.gist_id,
            self.filename.as_deref()?,
        ))
    }
}

/// The notebook's `herse.share` metadata, if present and valid.
pub fn read_share_info(nb: &Notebook) -> Option<ShareInfo> {
    let share = nb
        .metadata
        .get(META_NAMESPACE)?
        .get(META_KEY)?
        .as_object()?;
    let gist_id = share.get("gist_id")?.as_str()?;
    if !is_valid_gist_id(gist_id) {
        return None;
    }
    let text = |key: &str| share.get(key).and_then(Value::as_str).map(str::to_string);
    Some(ShareInfo {
        gist_id: gist_id.to_string(),
        public: share
            .get("public")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        html_url: text("html_url").unwrap_or_else(|| gist_html_url(gist_id)),
        published_at: text("published_at").unwrap_or_default(),
        filename: text("filename"),
        owner: text("owner"),
    })
}

/// Set `herse.share`, keeping other keys under `herse` (and unknown keys
/// under `herse.share`). The caller marks the notebook dirty and saves it.
pub fn write_share_info(nb: &mut Notebook, info: &ShareInfo) {
    let ns = nb
        .metadata
        .entry(META_NAMESPACE)
        .or_insert_with(|| Value::Object(Map::new()));
    if !ns.is_object() {
        *ns = Value::Object(Map::new());
    }
    let Value::Object(ns) = ns else {
        unreachable!()
    };
    let share = ns
        .entry(META_KEY)
        .or_insert_with(|| Value::Object(Map::new()));
    if !share.is_object() {
        *share = Value::Object(Map::new());
    }
    let Value::Object(share) = share else {
        unreachable!()
    };
    share.insert("gist_id".into(), Value::String(info.gist_id.clone()));
    share.insert("public".into(), Value::Bool(info.public));
    share.insert("html_url".into(), Value::String(info.html_url.clone()));
    share.insert(
        "published_at".into(),
        Value::String(info.published_at.clone()),
    );
    for (key, value) in [("filename", &info.filename), ("owner", &info.owner)] {
        match value {
            Some(v) => {
                share.insert(key.into(), Value::String(v.clone()));
            }
            None => {
                share.shift_remove(key);
            }
        }
    }
}

/// Remove `herse.share` (and `herse` if nothing else is left in it).
/// Returns whether there was anything to remove.
pub fn clear_share_info(nb: &mut Notebook) -> bool {
    let Some(Value::Object(ns)) = nb.metadata.get_mut(META_NAMESPACE) else {
        return false;
    };
    let removed = ns.shift_remove(META_KEY).is_some();
    if ns.is_empty() {
        nb.metadata.shift_remove(META_NAMESPACE);
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> ShareInfo {
        ShareInfo {
            gist_id: "aa5a315d61ae9438b18d".into(),
            public: false,
            html_url: "https://gist.github.com/aa5a315d61ae9438b18d".into(),
            published_at: "2026-10-16T09:30:00Z".into(),
            filename: Some("analysis.ipynb".into()),
            owner: Some("octocat".into()),
        }
    }

    #[test]
    fn round_trip_through_file() {
        let mut nb = Notebook::new_empty("python3", "Python 3", "python");
        assert_eq!(read_share_info(&nb), None);
        write_share_info(&mut nb, &info());
        assert_eq!(read_share_info(&nb), Some(info()));

        // Survives a save + parse in both the file's own and Jupyter's format.
        for text in [
            herse_nb::to_string(&nb),
            herse_nb::to_string_with(&nb, &herse_nb::WriteOptions::jupyter()),
        ] {
            let back = herse_nb::parse(text.as_bytes()).unwrap();
            assert_eq!(read_share_info(&back), Some(info()));
            let share = &back.metadata["herse"]["share"];
            assert_eq!(share["gist_id"], "aa5a315d61ae9438b18d");
            assert_eq!(share["public"], false);
        }
    }

    #[test]
    fn update_keeps_other_keys_and_clear_removes() {
        let mut nb = Notebook::new_empty("python3", "Python 3", "python");
        nb.metadata.insert(
            "herse".into(),
            serde_json::json!({"theme": "dark", "share": {"gist_id": "old1", "extra": 1}}),
        );
        let mut new = info();
        new.filename = None;
        write_share_info(&mut nb, &new);
        assert_eq!(nb.metadata["herse"]["theme"], "dark");
        assert_eq!(nb.metadata["herse"]["share"]["extra"], 1);
        assert!(nb.metadata["herse"]["share"].get("filename").is_none());
        assert_eq!(read_share_info(&nb), Some(new));

        assert!(clear_share_info(&mut nb));
        assert_eq!(read_share_info(&nb), None);
        assert_eq!(nb.metadata["herse"], serde_json::json!({"theme": "dark"}));
        assert!(!clear_share_info(&mut nb));

        // Removing the last key removes the namespace.
        write_share_info(&mut nb, &info());
        nb.metadata["herse"]
            .as_object_mut()
            .unwrap()
            .shift_remove("theme");
        assert!(clear_share_info(&mut nb));
        assert!(!nb.metadata.contains_key("herse"));
    }

    #[test]
    fn lenient_read_and_invalid_ids() {
        let mut nb = Notebook::new_empty("python3", "Python 3", "python");
        nb.metadata.insert(
            "herse".into(),
            serde_json::json!({"share": {"gist_id": "abc123"}}),
        );
        let got = read_share_info(&nb).unwrap();
        assert_eq!(got.html_url, "https://gist.github.com/abc123");
        assert!(!got.public);
        assert_eq!(got.published_at, "");
        assert_eq!(got.raw_url(), None);

        for bad in [
            serde_json::json!({"share": {"gist_id": "../../user"}}),
            serde_json::json!({"share": {"gist_id": 12}}),
            serde_json::json!({"share": "abc"}),
            serde_json::json!("abc"),
        ] {
            nb.metadata.insert("herse".into(), bad.clone());
            assert_eq!(read_share_info(&nb), None, "{bad}");
        }
        // A non-object namespace is replaced on write.
        write_share_info(&mut nb, &info());
        assert_eq!(read_share_info(&nb), Some(info()));
        assert_eq!(
            info().raw_url().unwrap(),
            "https://gist.githubusercontent.com/octocat/aa5a315d61ae9438b18d/raw/analysis.ipynb"
        );
        assert_eq!(
            info().nbviewer_url(),
            "https://nbviewer.org/gist/aa5a315d61ae9438b18d"
        );
    }
}
