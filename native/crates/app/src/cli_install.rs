//! Herse ▸ Install Command Line Tool: a `herse` link, in a folder on the
//! shell's PATH, to the command that ships inside the app
//! (`Herse.app/Contents/MacOS/herse-cli`, see `crates/cli`).

use std::path::{Path, PathBuf};

/// Where the link goes, best first: the user's own bin folder needs no
/// password, Homebrew's usually doesn't either, /usr/local/bin last.
fn candidates(home: &Path) -> Vec<PathBuf> {
    vec![home.join(".local/bin"), home.join("bin"), PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/local/bin")]
}

/// The command inside this copy of the app.
pub fn bundled_tool() -> Option<PathBuf> {
    let exe = std::fs::canonicalize(std::env::current_exe().ok()?).ok()?;
    let tool = exe.with_file_name("herse-cli");
    tool.is_file().then_some(tool)
}

/// What happened, for the reader: where `herse` now is.
#[derive(Debug)]
pub struct Installed {
    pub link: PathBuf,
    /// It was already there, pointing here.
    pub already: bool,
}

/// Link `herse` to `tool` in the first of the PATH's usual folders that can
/// take it. `path` is the login shell's PATH.
pub fn install(tool: &Path, path: &str, home: &Path) -> Result<Installed, String> {
    let on_path: Vec<PathBuf> = std::env::split_paths(path).collect();
    let dirs: Vec<PathBuf> = candidates(home).into_iter().filter(|d| on_path.contains(d)).collect();
    let manual = format!("sudo ln -sf '{}' /usr/local/bin/herse", tool.display());
    if dirs.is_empty() {
        return Err(format!(
            "None of the usual folders for commands (~/.local/bin, /opt/homebrew/bin, /usr/local/bin) is on your PATH. \
             Add one to it, or link the command yourself:\n{manual}"
        ));
    }
    let mut refused = Vec::new();
    for dir in dirs {
        let link = dir.join("herse");
        match std::fs::symlink_metadata(&link) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if std::fs::read_link(&link).is_ok_and(|to| to == tool) {
                    return Ok(Installed { link, already: true });
                }
                // An older Herse's link (another copy of the app): replace it.
                if std::fs::read_link(&link).is_ok_and(|to| to.ends_with("Contents/MacOS/herse-cli")) {
                    if std::fs::remove_file(&link).is_err() {
                        refused.push(link);
                        continue;
                    }
                } else {
                    return Err(format!(
                        "There's already a herse command at {}, and it isn't Herse's. Remove it and try again.",
                        link.display()
                    ));
                }
            }
            Ok(_) => {
                return Err(format!(
                    "There's already a file called herse at {}. Remove it and try again.",
                    link.display()
                ));
            }
            Err(_) => {}
        }
        match std::os::unix::fs::symlink(tool, &link) {
            Ok(()) => return Ok(Installed { link, already: false }),
            Err(_) => refused.push(link),
        }
    }
    let tried: Vec<String> = refused.iter().map(|p| p.display().to_string()).collect();
    Err(format!("Herse couldn't write {} (they need an administrator). Link it yourself:\n{manual}", tried.join(" or ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("herse-cli-install-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".local/bin")).unwrap();
        std::fs::create_dir_all(dir.join("App.app/Contents/MacOS")).unwrap();
        std::fs::write(dir.join("App.app/Contents/MacOS/herse-cli"), "").unwrap();
        dir
    }

    #[test]
    fn links_into_the_first_folder_on_the_path() {
        let home = scratch("first");
        let tool = home.join("App.app/Contents/MacOS/herse-cli");
        let path = format!("/usr/bin:{}", home.join(".local/bin").display());
        let done = install(&tool, &path, &home).unwrap();
        assert_eq!(done.link, home.join(".local/bin/herse"));
        assert_eq!(std::fs::read_link(&done.link).unwrap(), tool);
        // Again: nothing to do.
        assert!(install(&tool, &path, &home).unwrap().already);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn leaves_someone_elses_herse_alone() {
        let home = scratch("other");
        let tool = home.join("App.app/Contents/MacOS/herse-cli");
        std::fs::write(home.join(".local/bin/herse"), "#!/bin/sh\n").unwrap();
        let path = home.join(".local/bin").display().to_string();
        let err = install(&tool, &path, &home).unwrap_err();
        assert!(err.contains("already a file called herse"), "{err}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn says_how_when_nothing_on_the_path_will_do() {
        let home = scratch("none");
        let err = install(&home.join("App.app/Contents/MacOS/herse-cli"), "/usr/bin:/bin", &home).unwrap_err();
        assert!(err.contains("sudo ln -sf"), "{err}");
        let _ = std::fs::remove_dir_all(&home);
    }
}
