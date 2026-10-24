//! `herse`: Herse from the terminal.
//!
//! ```text
//! herse [PATH...]                      open files and folders in Herse
//! herse cat FILE                       a notebook, or a data file's table, as text
//! herse run NOTEBOOK [-o OUT]          run every code cell and save the outputs
//! herse export NOTEBOOK --to FORMAT    html, py or md
//! ```
//!
//! Opening goes through Launch Services, so a running Herse gets the paths
//! (as Finder's Open With does) and a closed one starts. The other commands
//! work without the app: the same notebook reader and writer, kernels and
//! HTML renderer, linked in.

mod cat;
mod export;
mod open;
mod run;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

const HELP: &str = "\
herse: notebooks and data from the terminal

Usage:
  herse [PATH...]                     Open files or folders in Herse (\"herse .\" for this folder)
  herse cat FILE [--no-outputs]       Print a notebook, or a data file's columns and first rows
  herse run NOTEBOOK [options]        Run every code cell in the notebook's kernel and save it
      -o, --out FILE                  Save to FILE instead of over the notebook
      --kernel NAME                   Use this kernel instead of the notebook's
      --timeout SECONDS               Stop a cell that runs longer (default: no limit)
      --keep-going                    Run the rest after a cell fails
  herse export NOTEBOOK --to FORMAT [-o FILE]
                                      FORMAT is html (a page like Herse's), py (a script with
                                      # %% cells) or md (Markdown with outputs as text);
                                      without -o, it's printed
  herse --version
  herse --help

Run and export never change anything but the file they're told to write.";

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match dispatch(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(why)) => {
            eprintln!("herse: {why}\nSee herse --help.");
            ExitCode::from(2)
        }
        Err(Failure::Failed(why)) => {
            if !why.is_empty() {
                eprintln!("herse: {why}");
            }
            ExitCode::FAILURE
        }
    }
}

pub enum Failure {
    /// The command line itself was wrong.
    Usage(String),
    /// It ran and didn't work (already explained when the text is empty).
    Failed(String),
}

impl From<String> for Failure {
    fn from(why: String) -> Self {
        Failure::Failed(why)
    }
}

fn dispatch(args: Vec<OsString>) -> Result<(), Failure> {
    let first = args.first().and_then(|a| a.to_str()).unwrap_or("");
    match first {
        "-h" | "--help" | "help" => {
            println!("{HELP}");
            Ok(())
        }
        "-V" | "--version" | "version" => {
            println!("herse {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "cat" => cat::main(Args::new(&args[1..])),
        "run" => run::main(Args::new(&args[1..])),
        "export" => export::main(Args::new(&args[1..])),
        // An option we don't know, rather than a file called "--foo".
        s if s.starts_with('-') => Err(Failure::Usage(format!("unknown option {s}"))),
        _ => open::main(args.iter().map(PathBuf::from).collect()),
    }
}

/// A subcommand's arguments: options with or without a value, and the rest.
pub struct Args {
    rest: Vec<OsString>,
}

impl Args {
    fn new(args: &[OsString]) -> Args {
        Args { rest: args.to_vec() }
    }

    /// Whether `--name` (or one of `names`) was given; it's taken out.
    pub fn flag(&mut self, names: &[&str]) -> bool {
        let before = self.rest.len();
        self.rest.retain(|a| !a.to_str().is_some_and(|a| names.contains(&a)));
        self.rest.len() != before
    }

    /// The value after `--name` (or `--name=value`); both are taken out.
    pub fn value(&mut self, names: &[&str]) -> Result<Option<OsString>, Failure> {
        for (i, arg) in self.rest.iter().enumerate() {
            let Some(text) = arg.to_str() else { continue };
            if names.contains(&text) {
                if i + 1 >= self.rest.len() {
                    return Err(Failure::Usage(format!("{text} needs a value")));
                }
                let value = self.rest.remove(i + 1);
                self.rest.remove(i);
                return Ok(Some(value));
            }
            if let Some((name, value)) = text.split_once('=')
                && names.contains(&name)
            {
                let value = OsString::from(value);
                self.rest.remove(i);
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    /// The one path left, after the options.
    pub fn path(self, what: &str) -> Result<PathBuf, Failure> {
        if let Some(unknown) = self.rest.iter().filter_map(|a| a.to_str()).find(|a| a.starts_with('-') && *a != "-") {
            return Err(Failure::Usage(format!("unknown option {unknown}")));
        }
        match self.rest.as_slice() {
            [one] => Ok(PathBuf::from(one)),
            [] => Err(Failure::Usage(format!("which {what}?"))),
            _ => Err(Failure::Usage(format!("one {what} at a time"))),
        }
    }
}

/// Read and parse a notebook, with the reason when it can't be.
pub fn read_notebook(path: &std::path::Path) -> Result<herse_nb::Notebook, Failure> {
    let bytes = std::fs::read(path).map_err(|e| Failure::Failed(format!("couldn't read {}: {e}", path.display())))?;
    herse_nb::parse(&bytes).map_err(|e| Failure::Failed(format!("{} isn't a notebook Herse can read: {e}", path.display())))
}

/// Write `text` to `path` in one step: a reader never sees half a file.
pub fn write_atomically(path: &std::path::Path, text: &str) -> Result<(), Failure> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "notebook".into());
    let tmp = dir.join(format!(".{name}.herse-{}", std::process::id()));
    std::fs::write(&tmp, text)
        .and_then(|_| std::fs::rename(&tmp, path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            Failure::Failed(format!("couldn't write {}: {e}", path.display()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Args {
        Args::new(&list.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn reads_options_in_either_spelling() {
        let mut a = args(&["nb.ipynb", "--to=html", "-o", "out.html", "--keep-going"]);
        assert!(a.flag(&["--keep-going"]));
        assert_eq!(a.value(&["--to"]).ok().flatten(), Some("html".into()));
        assert_eq!(a.value(&["-o", "--out"]).ok().flatten(), Some("out.html".into()));
        assert_eq!(a.path("notebook").ok(), Some(PathBuf::from("nb.ipynb")));
    }

    #[test]
    fn says_what_is_wrong_with_a_command_line() {
        assert!(matches!(args(&["--bogus", "nb.ipynb"]).path("notebook"), Err(Failure::Usage(_))));
        assert!(matches!(args(&[]).path("notebook"), Err(Failure::Usage(_))));
        assert!(matches!(args(&["-o"]).value(&["-o"]), Err(Failure::Usage(_))));
    }
}
