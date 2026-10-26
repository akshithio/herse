//! Assets: GPUI Kit's component icons, plus the few Lucide icons Herse adds
//! (`assets/icons/`, ISC licensed like the rest of Lucide).

use std::borrow::Cow;

use gpui_kit::{AssetSource, SharedString};

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        &[$((concat!("icons/", $name, ".svg"), include_bytes!(concat!("../assets/icons/", $name, ".svg")))),*]
    };
}

const EXTRA: &[(&str, &[u8])] = icons!["files", "list-tree", "circle-alert", "folder", "table-of-contents", "copy", "check", "herse-mark", "plus", "fast-forward", "square", "rotate-ccw", "image-off", "command", "notebook", "hash", "share-2", "users", "message-square", "link", "keyboard", "folder-open", "herse-notebook", "git-compare", "package", "lightbulb", "sparkles", "arrow-up", "minus", "shield", "file-spreadsheet", "file-braces", "database", "funnel", "maximize-2", "table-2"];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = EXTRA.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut all = gpui_kit::assets::Assets.list(path)?;
        all.extend(EXTRA.iter().filter(|(p, _)| p.starts_with(path)).map(|(p, _)| SharedString::from(*p)));
        Ok(all)
    }
}
