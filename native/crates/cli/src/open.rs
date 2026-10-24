//! `herse PATH...`: open in the app.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Failure;

/// The app's bundle identifier (packaging/macos/Info.plist).
const BUNDLE_ID: &str = "dev.herse.app";

pub fn main(paths: Vec<PathBuf>) -> Result<(), Failure> {
    let mut absolute = Vec::new();
    for path in &paths {
        if !path.exists() {
            return Err(Failure::Failed(format!("there's no {}", path.display())));
        }
        absolute.push(std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()));
    }
    let mut open = Command::new("/usr/bin/open");
    // The Herse this command came with, else whichever Launch Services knows.
    match own_app() {
        Some(app) => open.arg("-a").arg(app),
        None => open.args(["-b", BUNDLE_ID]),
    };
    open.args(&absolute);
    let out = open.output().map_err(|e| format!("couldn't run open: {e}"))?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        return Err(Failure::Failed(format!(
            "couldn't open {} in Herse: {}",
            if absolute.is_empty() { "the app".to_string() } else { describe(&absolute) },
            said.trim()
        )));
    }
    Ok(())
}

/// `Herse.app` when this binary is the one inside it
/// (`Herse.app/Contents/MacOS/herse-cli`, reached through a `herse` link).
fn own_app() -> Option<PathBuf> {
    let exe = std::fs::canonicalize(std::env::current_exe().ok()?).ok()?;
    let app = exe.parent()?.parent()?.parent()?;
    (app.extension().is_some_and(|e| e == "app") && app.join("Contents/Info.plist").is_file()).then(|| app.to_path_buf())
}

fn describe(paths: &[PathBuf]) -> String {
    match paths {
        [one] => name(one),
        many => format!("{} items", many.len()),
    }
}

fn name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}
