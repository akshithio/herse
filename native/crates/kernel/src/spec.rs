//! Kernelspec discovery: a synchronous directory scan in Jupyter's precedence
//! order. No `jupyter` subprocess on the hot path.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde_json::{Map, Value};

/// How to interrupt a running kernel (`interrupt_mode` in kernel.json).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InterruptMode {
    /// SIGINT to the kernel's process group (the default).
    #[default]
    Signal,
    /// `interrupt_request` on the control channel.
    Message,
}

/// One installed kernel, from `<data dir>/kernels/<name>/kernel.json`.
#[derive(Clone, Debug, PartialEq)]
pub struct KernelSpec {
    /// Directory name, lowercased (Jupyter's kernel name, e.g. `python3`).
    pub name: String,
    pub display_name: String,
    pub language: String,
    /// Command line, unsubstituted (`{connection_file}`, `{resource_dir}`
    /// are replaced at launch).
    pub argv: Vec<String>,
    pub env: HashMap<String, String>,
    pub resource_dir: PathBuf,
    pub interrupt_mode: InterruptMode,
    /// The `metadata` object (e.g. `{"debugger": true}`).
    pub metadata: Map<String, Value>,
}

impl KernelSpec {
    /// Parse `<dir>/kernel.json`. The kernel name is the directory name.
    pub fn from_dir(dir: &Path) -> Result<KernelSpec> {
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .context("kernelspec directory has no name")?
            .to_ascii_lowercase();
        let bytes = std::fs::read(dir.join("kernel.json"))
            .with_context(|| format!("reading {}", dir.join("kernel.json").display()))?;
        Self::from_json(&name, dir, &bytes)
    }

    fn from_json(name: &str, dir: &Path, bytes: &[u8]) -> Result<KernelSpec> {
        let raw: Value = serde_json::from_slice(bytes).context("kernel.json is not valid JSON")?;
        let Value::Object(mut raw) = raw else {
            bail!("kernel.json is not an object")
        };
        let argv: Vec<String> = match raw.remove("argv") {
            Some(Value::Array(items)) => items
                .into_iter()
                .map(|v| match v {
                    Value::String(s) => Ok(s),
                    other => bail!("non-string argv entry {other}"),
                })
                .collect::<Result<_>>()?,
            _ => bail!("kernel.json has no argv"),
        };
        if argv.is_empty() {
            bail!("kernel.json has an empty argv");
        }
        let string = |raw: &mut Map<String, Value>, key: &str| match raw.remove(key) {
            Some(Value::String(s)) => Some(s),
            _ => None,
        };
        let display_name = string(&mut raw, "display_name").unwrap_or_else(|| name.to_owned());
        let language = string(&mut raw, "language").unwrap_or_default();
        let interrupt_mode = match string(&mut raw, "interrupt_mode").as_deref() {
            Some("message") => InterruptMode::Message,
            _ => InterruptMode::Signal,
        };
        let env = match raw.remove("env") {
            Some(Value::Object(env)) => env
                .into_iter()
                .filter_map(|(k, v)| match v {
                    Value::String(s) => Some((k, s)),
                    _ => None,
                })
                .collect(),
            _ => HashMap::new(),
        };
        let metadata = match raw.remove("metadata") {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        };
        Ok(KernelSpec {
            name: name.to_owned(),
            display_name,
            language,
            argv,
            env,
            resource_dir: dir.to_path_buf(),
            interrupt_mode,
            metadata,
        })
    }
}

/// Find all installed kernelspecs. Synchronous and fast (a few `readdir`s
/// and small file reads, typically ~1 ms). On a name collision the
/// higher-precedence directory wins, as in Jupyter. Sorted by display name.
pub fn discover() -> Vec<KernelSpec> {
    discover_in(&jupyter_data_dirs())
}

/// Like [`discover`] over explicit Jupyter data directories (each containing
/// a `kernels/` subdirectory), highest precedence first.
pub fn discover_in(data_dirs: &[PathBuf]) -> Vec<KernelSpec> {
    let mut seen = HashSet::new();
    let mut specs = Vec::new();
    for data_dir in data_dirs {
        let kernels = data_dir.join("kernels");
        let Ok(entries) = std::fs::read_dir(&kernels) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        dirs.sort();
        for dir in dirs {
            let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let name = name.to_ascii_lowercase();
            if !valid_kernel_name(&name) || seen.contains(&name) {
                continue;
            }
            let json = dir.join("kernel.json");
            let Ok(bytes) = std::fs::read(&json) else {
                continue;
            };
            match KernelSpec::from_json(&name, &dir, &bytes) {
                Ok(mut spec) => {
                    resolve_bare_python(&mut spec, data_dir);
                    seen.insert(name);
                    specs.push(spec);
                }
                Err(e) => log::warn!("skipping kernelspec {}: {e:#}", json.display()),
            }
        }
    }
    specs.sort_by_cached_key(|s| (s.display_name.to_lowercase(), s.name.clone()));
    specs
}

/// The kernel to use when the user has not picked one: `python3`, else the
/// first Python kernel, else the first kernel.
pub fn default_spec(specs: &[KernelSpec]) -> Option<&KernelSpec> {
    specs
        .iter()
        .find(|s| s.name == "python3")
        .or_else(|| {
            specs
                .iter()
                .find(|s| s.language.eq_ignore_ascii_case("python"))
        })
        .or_else(|| specs.first())
}

/// Find one kernelspec by name (case-insensitive).
pub fn find(name: &str) -> Option<KernelSpec> {
    let name = name.to_ascii_lowercase();
    discover().into_iter().find(|s| s.name == name)
}

/// Slow fallback: ask `jupyter kernelspec list --json` (spawns Python;
/// hundreds of ms). Useful when the app was launched with a minimal
/// environment and the user's kernels live somewhere we can't infer.
pub fn discover_via_jupyter() -> Result<Vec<KernelSpec>> {
    let out = std::process::Command::new("jupyter")
        .args(["kernelspec", "list", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .context("running `jupyter kernelspec list --json`")?;
    if !out.status.success() {
        bail!("`jupyter kernelspec list` failed: {}", out.status);
    }
    let parsed: Value = serde_json::from_slice(&out.stdout)?;
    let mut specs = Vec::new();
    if let Some(Value::Object(map)) = parsed.get("kernelspecs") {
        for (name, entry) in map {
            let (Some(dir), Some(spec)) = (
                entry.get("resource_dir").and_then(|d| d.as_str()),
                entry.get("spec"),
            ) else {
                continue;
            };
            let bytes = serde_json::to_vec(spec)?;
            if let Ok(spec) = KernelSpec::from_json(name, Path::new(dir), &bytes) {
                specs.push(spec);
            }
        }
    }
    specs.sort_by_cached_key(|s| (s.display_name.to_lowercase(), s.name.clone()));
    Ok(specs)
}

fn valid_kernel_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// A spec installed into an environment prefix with a bare `python`/`python3`
/// argv[0] means "this environment's Python" (jupyter_client substitutes its
/// own `sys.executable`). Point it at `<prefix>/bin/python` so we don't pick
/// up whatever Python happens to be first on PATH.
fn resolve_bare_python(spec: &mut KernelSpec, data_dir: &Path) {
    let exe = spec.argv[0].as_str();
    let is_bare_python =
        exe == "python" || exe == "python3" || (exe.starts_with("python3.") && !exe.contains('/'));
    if !is_bare_python {
        return;
    }
    // data_dir = <prefix>/share/jupyter
    let Some(prefix) = data_dir.parent().and_then(|p| p.parent()) else {
        return;
    };
    if data_dir.parent().and_then(|p| p.file_name()) != Some("share".as_ref()) {
        return;
    }
    let candidate = if cfg!(windows) {
        prefix.join(format!("{exe}.exe"))
    } else {
        prefix.join("bin").join(exe)
    };
    if is_executable(&candidate) {
        spec.argv[0] = candidate.to_string_lossy().into_owned();
    }
}

/// Jupyter data directories in precedence order (highest first), following
/// `jupyter_core.paths.jupyter_path()`:
///
/// 1. `JUPYTER_PATH` entries
/// 2. the user data dir (`JUPYTER_DATA_DIR`, else `~/Library/Jupyter`,
///    `$XDG_DATA_HOME/jupyter` / `~/.local/share/jupyter`, `%APPDATA%\jupyter`)
///    and the Python user-site data dirs
/// 3. `{sys.prefix}/share/jupyter` of the active environment(s): `VIRTUAL_ENV`,
///    `CONDA_PREFIX`, and the prefix of the first `python3` on `PATH`
///    (derived from the binary's location; nothing is spawned). Comes before
///    the user dir when `JUPYTER_PREFER_ENV_PATH` is truthy, or by default
///    inside a venv / non-base conda env, as Jupyter does.
/// 4. system dirs: `/usr/local/share/jupyter`, `/usr/share/jupyter`
///    (`%PROGRAMDATA%\jupyter` on Windows)
/// 5. last, well-known conda/Homebrew prefixes (`~/miniconda3`,
///    `/opt/anaconda3`, ...). Jupyter itself would not look there, but a GUI
///    app launched from Finder has no activated environment; being last,
///    these only ever add kernels, never shadow one.
pub fn jupyter_data_dirs() -> Vec<PathBuf> {
    let env = |k: &str| crate::shell_env::var_os(k).filter(|v| !v.is_empty());
    let home = home_dir();
    let mut dirs: Vec<PathBuf> = Vec::new();

    if let Some(paths) = env("JUPYTER_PATH") {
        dirs.extend(std::env::split_paths(&paths).filter(|p| !p.as_os_str().is_empty()));
    }

    let mut user = Vec::new();
    if let Some(dir) = env("JUPYTER_DATA_DIR") {
        user.push(PathBuf::from(dir));
    } else if cfg!(target_os = "macos") {
        if let Some(home) = &home {
            user.push(home.join("Library/Jupyter"));
        }
    } else if cfg!(windows) {
        if let Some(appdata) = env("APPDATA") {
            user.push(PathBuf::from(appdata).join("jupyter"));
        }
    } else if let Some(xdg) = env("XDG_DATA_HOME") {
        user.push(PathBuf::from(xdg).join("jupyter"));
    } else if let Some(home) = &home {
        user.push(home.join(".local/share/jupyter"));
    }
    // Python user site (`pip install --user ipykernel`): site.getuserbase()/share/jupyter.
    if let Some(home) = &home {
        if cfg!(target_os = "macos")
            && let Ok(entries) = std::fs::read_dir(home.join("Library/Python"))
        {
            let mut bases: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            // Newest Python first ("3.10" before "3.9", numerically).
            bases.sort_by_cached_key(|b| {
                let name = b
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let version: Vec<u32> = name.split('.').map(|p| p.parse().unwrap_or(0)).collect();
                std::cmp::Reverse(version)
            });
            user.extend(bases.into_iter().map(|b| b.join("share/jupyter")));
        }
        if !cfg!(windows) {
            user.push(home.join(".local/share/jupyter"));
        }
    }

    let mut envs = Vec::new();
    if let Some(venv) = env("VIRTUAL_ENV") {
        envs.push(PathBuf::from(venv).join("share/jupyter"));
    }
    if let Some(conda) = env("CONDA_PREFIX") {
        envs.push(PathBuf::from(conda).join("share/jupyter"));
    }
    for prefix in path_python_prefixes() {
        envs.push(prefix.join("share").join("jupyter"));
    }

    if prefer_env_path() {
        dirs.extend(envs);
        dirs.extend(user);
    } else {
        dirs.extend(user);
        dirs.extend(envs);
    }

    if cfg!(windows) {
        if let Some(pd) = env("PROGRAMDATA") {
            dirs.push(PathBuf::from(pd).join("jupyter"));
        }
    } else {
        dirs.push("/usr/local/share/jupyter".into());
        dirs.push("/usr/share/jupyter".into());
    }

    if !cfg!(windows) {
        if let Some(home) = &home {
            for name in ["miniforge3", "mambaforge", "miniconda3", "anaconda3"] {
                dirs.push(home.join(name).join("share/jupyter"));
            }
        }
        for prefix in [
            "/opt/homebrew",
            "/opt/miniforge3",
            "/opt/miniconda3",
            "/opt/anaconda3",
        ] {
            dirs.push(Path::new(prefix).join("share/jupyter"));
        }
    }

    // Dedupe, keeping the first (highest precedence) occurrence.
    let mut seen = HashSet::new();
    dirs.retain(|d| seen.insert(normalize(d)));
    dirs
}

fn normalize(p: &Path) -> PathBuf {
    // Cheap normalization: strip trailing separators. Canonicalizing would
    // cost a syscall per component and resolve symlinks Jupyter doesn't.
    let s = p.to_string_lossy();
    PathBuf::from(s.trim_end_matches(['/', '\\']))
}

fn prefer_env_path() -> bool {
    use crate::shell_env::var_os;
    if let Some(v) = var_os("JUPYTER_PREFER_ENV_PATH") {
        let v = v.to_string_lossy().to_ascii_lowercase();
        return !matches!(v.as_str(), "" | "0" | "false" | "no" | "off" | "n");
    }
    if var_os("VIRTUAL_ENV").is_some_and(|v| !v.is_empty()) {
        return true;
    }
    var_os("CONDA_PREFIX").is_some_and(|v| !v.is_empty())
        && var_os("CONDA_DEFAULT_ENV").is_some_and(|e| e != "base")
}

fn home_dir() -> Option<PathBuf> {
    #[allow(deprecated)]
    std::env::home_dir().filter(|h| !h.as_os_str().is_empty())
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// `sys.prefix` candidates for the first `python3` (or `python`) on PATH,
/// derived from the executable's location without running it:
/// `<prefix>/bin/python3` (`<prefix>\python.exe` on Windows). A venv's
/// `bin/python3` is a symlink to the base interpreter, but its prefix is the
/// venv (marked by `pyvenv.cfg`), so we use the unresolved path first and add
/// the resolved one (e.g. Homebrew's framework prefix) only outside venvs.
fn path_python_prefixes() -> Vec<PathBuf> {
    let Some(path) = crate::shell_env::var_os("PATH") else {
        return Vec::new();
    };
    let names: &[&str] = if cfg!(windows) {
        &["python.exe"]
    } else {
        &["python3", "python"]
    };
    let mut prefixes = Vec::new();
    for name in names {
        let Some(exe) = std::env::split_paths(&path)
            .map(|d| d.join(name))
            .find(|p| is_executable(p))
        else {
            continue;
        };
        let prefix_of = |exe: &Path| -> Option<PathBuf> {
            let dir = exe.parent()?;
            if cfg!(windows) {
                Some(dir.to_path_buf())
            } else {
                Some(dir.parent()?.to_path_buf())
            }
        };
        let Some(prefix) = prefix_of(&exe) else {
            continue;
        };
        let is_venv = prefix.join("pyvenv.cfg").exists();
        prefixes.push(prefix);
        if !is_venv
            && let Ok(real) = std::fs::canonicalize(&exe)
            && let Some(real_prefix) = prefix_of(&real)
        {
            prefixes.push(real_prefix);
        }
    }
    let mut seen = HashSet::new();
    prefixes.retain(|p| seen.insert(p.clone()));
    prefixes
}

/// Substitute `${VAR}` references in a kernelspec env value against `env`
/// (jupyter_client's `Template.safe_substitute`: unknown names stay as is).
pub(crate) fn expand_env_value(value: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                let valid = !name.is_empty()
                    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    && !name.as_bytes()[0].is_ascii_digit();
                match valid.then(|| lookup(name)).flatten() {
                    Some(v) => out.push_str(&v),
                    None => out.push_str(&rest[start..start + 3 + end]),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("herse-kernel-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_spec(data_dir: &Path, name: &str, json: &str) {
        let dir = data_dir.join("kernels").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("kernel.json"), json).unwrap();
    }

    #[test]
    fn precedence_dedupe_and_parsing() {
        let root = scratch("specs");
        let high = root.join("high");
        let low = root.join("low");
        write_spec(
            &high,
            "Py",
            r#"{"argv":["/x/python","-f","{connection_file}"],"display_name":"High","language":"python","interrupt_mode":"message","env":{"A":"1","B":2},"metadata":{"debugger":true}}"#,
        );
        write_spec(&low, "py", r#"{"argv":["/y/python"],"display_name":"Low"}"#);
        write_spec(&low, "other", r#"{"argv":["x"],"display_name":"Another"}"#);
        write_spec(&low, "broken", r#"{"display_name":"no argv"}"#);
        write_spec(&low, "bad name!", r#"{"argv":["x"]}"#);
        let specs = discover_in(&[high.clone(), low.clone()]);
        let names: Vec<_> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["other", "py"]); // sorted by display name: Another, High
        let py = &specs[1];
        assert_eq!(py.display_name, "High");
        assert_eq!(py.interrupt_mode, InterruptMode::Message);
        assert_eq!(py.env.get("A").map(String::as_str), Some("1"));
        assert!(!py.env.contains_key("B"));
        assert_eq!(py.metadata.get("debugger"), Some(&Value::Bool(true)));
        assert_eq!(py.resource_dir, high.join("kernels").join("Py"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bare_python_resolves_to_its_prefix() {
        let prefix = scratch("prefix");
        let data = prefix.join("share/jupyter");
        write_spec(
            &data,
            "python3",
            r#"{"argv":["python","-m","ipykernel_launcher","-f","{connection_file}"],"display_name":"P"}"#,
        );
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        let exe = prefix.join("bin/python");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let specs = discover_in(&[data]);
        assert_eq!(specs[0].argv[0], exe.to_string_lossy());
        let _ = std::fs::remove_dir_all(prefix);
    }

    #[test]
    fn env_expansion() {
        let lookup = |k: &str| (k == "HOME").then(|| "/h".to_string());
        assert_eq!(
            expand_env_value("${HOME}/x:${NOPE}:$HOME:${", lookup),
            "/h/x:${NOPE}:$HOME:${"
        );
        assert_eq!(expand_env_value("plain", lookup), "plain");
    }

    #[test]
    fn data_dirs_are_deduped_and_fast() {
        // Best of three: other tests spawn processes concurrently.
        let (mut dirs, mut specs, mut elapsed) = (Vec::new(), Vec::new(), std::time::Duration::MAX);
        for _ in 0..3 {
            let t = std::time::Instant::now();
            dirs = jupyter_data_dirs();
            specs = discover_in(&dirs);
            elapsed = elapsed.min(t.elapsed());
        }
        let unique: HashSet<_> = dirs.iter().collect();
        assert_eq!(unique.len(), dirs.len());
        assert!(
            dirs.iter()
                .any(|d| d.ends_with("share/jupyter") || d.ends_with("Jupyter"))
        );
        eprintln!(
            "discover: {} specs in {elapsed:?}: {:?}",
            specs.len(),
            specs.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(elapsed.as_millis() < 20, "discovery took {elapsed:?}");
    }
}
