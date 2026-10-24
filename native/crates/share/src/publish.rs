//! Publish / re-publish / unpublish a notebook as a gist.

use herse_nb::Notebook;

use crate::client::{Gist, GistClient, GistUpdate, NewGist};
use crate::error::{Result, ShareError};
use crate::links::gist_filename;
use crate::meta::{ShareInfo, clear_share_info, read_share_info, write_share_info};
use crate::prepare::{PrepareOptions, publish_content};

/// How to publish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishOptions {
    /// The notebook's name ("analysis", "analysis.ipynb" or its path); the
    /// gist file is [`gist_filename`] of it.
    pub name: String,
    /// Gist description. `None`: the notebook name for a new gist, unchanged
    /// for an update.
    pub description: Option<String>,
    /// Public, or secret (unlisted). For a notebook already shared this must
    /// match ([`ShareInfo::public`]): GitHub can't change it.
    pub public: bool,
    pub strip_outputs: bool,
    pub strip_widget_state: bool,
    /// Create a new gist even if the notebook was published before (e.g. to
    /// change visibility). The old gist is left as is.
    pub new_gist: bool,
}

impl PublishOptions {
    pub fn new(name: impl Into<String>, public: bool) -> PublishOptions {
        PublishOptions {
            name: name.into(),
            description: None,
            public,
            strip_outputs: false,
            strip_widget_state: false,
            new_gist: false,
        }
    }
}

/// The result of [`publish`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Published {
    /// Also written to the notebook's metadata.
    pub info: ShareInfo,
    pub gist: Gist,
    /// A new gist (new links) rather than a new revision of the old one.
    pub created: bool,
}

/// Upload the notebook. If its metadata names a gist that still exists, is
/// owned by the signed-in user and has the requested visibility, that gist
/// gets a new revision (same links; a renamed notebook renames the gist
/// file). Otherwise a new gist is created. Either way `herse.share` is
/// written to `nb` (in memory: the caller marks it dirty and saves).
///
/// Blocks: a first publish is one request (the upload); a re-publish is
/// `GET` + `PATCH`, plus one `GET /user` the first time per client.
/// [`ShareError::VisibilityMismatch`] is returned before anything is
/// uploaded.
pub fn publish(client: &GistClient, nb: &mut Notebook, opts: &PublishOptions) -> Result<Published> {
    let content = publish_content(
        nb,
        &PrepareOptions {
            strip_outputs: opts.strip_outputs,
            strip_widget_state: opts.strip_widget_state,
        },
    );
    let filename = gist_filename(&opts.name);
    let existing = if opts.new_gist {
        None
    } else {
        read_share_info(nb)
    };
    let updated = match existing {
        Some(info) => update_existing(client, &info.gist_id, &filename, &content, opts)?,
        None => None,
    };
    let (gist, created) = match updated {
        Some(gist) => (gist, false),
        None => {
            let stem = filename.strip_suffix(".ipynb").unwrap_or(&filename);
            let description = opts.description.as_deref().unwrap_or(stem);
            let gist = client.create_gist(&NewGist {
                filename: &filename,
                content: &content,
                description,
                public: opts.public,
            })?;
            (gist, true)
        }
    };
    let info = ShareInfo::from_gist(&gist, &filename);
    write_share_info(nb, &info);
    Ok(Published {
        info,
        gist,
        created,
    })
}

/// `Ok(None)`: the old gist is gone or not ours, so create a new one.
fn update_existing(
    client: &GistClient,
    gist_id: &str,
    filename: &str,
    content: &str,
    opts: &PublishOptions,
) -> Result<Option<Gist>> {
    let current = match client.get_gist(gist_id) {
        Ok(gist) => gist,
        Err(ShareError::NotFound) => return Ok(None),
        Err(e) => return Err(e),
    };
    // A notebook someone else published (its file carries their metadata).
    if current.owner.is_none() || current.owner != Some(client.login()?) {
        return Ok(None);
    }
    if current.public != opts.public {
        return Err(ShareError::VisibilityMismatch {
            gist_public: current.public,
        });
    }
    let rename_from = if current.files.iter().any(|f| f.filename == filename) {
        None
    } else {
        current.notebook_file().map(|f| f.filename.as_str())
    };
    let update = GistUpdate {
        filename,
        content,
        description: opts.description.as_deref(),
        rename_from,
    };
    match client.update_gist(gist_id, &update) {
        Ok(gist) => Ok(Some(gist)),
        // Deleted between the two calls.
        Err(ShareError::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Delete the notebook's gist (its links stop working) and remove
/// `herse.share`. A gist that is already gone counts as deleted. Returns
/// what was removed, `None` if the notebook wasn't published.
pub fn unpublish(client: &GistClient, nb: &mut Notebook) -> Result<Option<ShareInfo>> {
    let Some(info) = read_share_info(nb) else {
        return Ok(None);
    };
    match client.delete_gist(&info.gist_id) {
        Ok(()) | Err(ShareError::NotFound) => {}
        Err(e) => return Err(e),
    }
    clear_share_info(nb);
    Ok(Some(info))
}
