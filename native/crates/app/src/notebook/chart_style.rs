//! Herse's look for matplotlib. In kernels Herse starts, figures come out on
//! a transparent page in the theme's ink, with quiet grids, no top or right
//! spines, a palette led by dew, and at retina resolution, so a plot reads
//! as part of the notebook rather than a pasted image. A matplotlibrc of the
//! user's own wins (Herse then leaves matplotlib's defaults alone), and so
//! does any style set in code. Settings ▸ Charts turns it off.

use std::path::PathBuf;

/// The kernel's environment: point matplotlib at Herse's rc for the theme.
pub fn kernel_env(dark: bool) -> Vec<(String, String)> {
    if !crate::settings::blend_charts() || user_has_rc() {
        return Vec::new();
    }
    match rc_dir(dark) {
        Some(dir) => vec![("MATPLOTLIBRC".into(), dir.to_string_lossy().into_owned())],
        None => Vec::new(),
    }
}

/// Run silently once a Python kernel is ready: retina figures. (Sets
/// configuration only; matplotlib isn't imported until the notebook does.)
pub fn startup_code() -> Option<&'static str> {
    crate::settings::blend_charts().then_some(
        "try:\n    get_ipython().run_line_magic('config', \"InlineBackend.figure_formats = {'retina'}\")\nexcept Exception:\n    pass\n",
    )
}

/// Run silently when the theme changes: figures drawn from then on take the
/// new theme's ink (those already drawn keep theirs).
pub fn theme_code(dark: bool) -> Option<String> {
    if !crate::settings::blend_charts() || user_has_rc() {
        return None;
    }
    let file = rc_dir(dark)?.join("matplotlibrc");
    let file = file.to_string_lossy().replace('\\', "\\\\").replace('\'', "\\'");
    Some(format!(
        "import sys as _herse_sys\nif 'matplotlib' in _herse_sys.modules:\n    try:\n        import matplotlib as _herse_mpl\n        _herse_mpl.rcParams.update(_herse_mpl.rc_params_from_file('{file}', use_default_template=False))\n    except Exception:\n        pass\n"
    ))
}

/// A matplotlibrc the user set up themselves (matplotlib's own lookup).
fn user_has_rc() -> bool {
    if std::env::var_os("MATPLOTLIBRC").is_some() {
        return true;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let config = std::env::var_os("MPLCONFIGDIR").map(PathBuf::from).or_else(|| {
        std::env::var_os("XDG_CONFIG_HOME").map(|x| PathBuf::from(x).join("matplotlib"))
    });
    [config, home.as_ref().map(|h| h.join(".matplotlib")), home.map(|h| h.join(".config/matplotlib"))]
        .into_iter()
        .flatten()
        .any(|dir| dir.join("matplotlibrc").is_file())
}

/// `~/Library/Application Support/Herse/matplotlib/<theme>/`, holding the
/// rc for that theme (rewritten when Herse's style changes).
fn rc_dir(dark: bool) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = PathBuf::from(home)
        .join("Library/Application Support/Herse/matplotlib")
        .join(if dark { "dark" } else { "light" });
    let file = dir.join("matplotlibrc");
    let want = rc(dark);
    if std::fs::read_to_string(&file).ok().as_deref() != Some(want.as_str()) {
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(&file, want).ok()?;
    }
    Some(dir)
}

fn rc(dark: bool) -> String {
    // Ink and muted text from the theme (herse-theme.json); the palette
    // starts at dew and stays distinguishable on both pages.
    let (ink, muted, rule, palette) = if dark {
        ("ECEAE6", "9C968F", "5A5651", "5cc8be, f0a35e, 8ea0f2, e57f97, a5c65f, b48fe6, 61c3dd, d8b04a")
    } else {
        ("1C1A17", "78736C", "B8B2AA", "0b7a75, e0823d, 5b6fd6, c94f6d, 7a9e3e, 8c62c7, 2f9fbd, b58a2a")
    };
    let cycle = palette.split(", ").map(|c| format!("'{c}'")).collect::<Vec<_>>().join(", ");
    format!(
        "# Written by Herse for the kernels it starts. Your own matplotlibrc wins.\n\
figure.facecolor: none\nfigure.edgecolor: none\naxes.facecolor: none\nsavefig.facecolor: none\nsavefig.edgecolor: none\nsavefig.transparent: True\n\
text.color: {ink}\naxes.labelcolor: {ink}\naxes.titlecolor: {ink}\naxes.edgecolor: {rule}\n\
xtick.color: {muted}\nytick.color: {muted}\nxtick.labelcolor: {ink}\nytick.labelcolor: {ink}\n\
axes.spines.top: False\naxes.spines.right: False\naxes.grid: True\naxes.axisbelow: True\n\
grid.color: {ink}\ngrid.alpha: 0.09\ngrid.linewidth: 0.8\n\
axes.titlesize: large\naxes.titleweight: semibold\naxes.titlelocation: left\naxes.titlepad: 10\n\
legend.frameon: False\n\
font.family: sans-serif\nfont.sans-serif: Helvetica Neue, Helvetica, Arial, DejaVu Sans\n\
lines.linewidth: 1.8\npatch.edgecolor: none\n\
axes.prop_cycle: cycler('color', [{cycle}])\n"
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_rc_is_valid_matplotlibrc_shape() {
        for dark in [false, true] {
            let rc = super::rc(dark);
            for line in rc.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
                assert!(line.split_once(": ").is_some(), "{line}");
            }
            assert!(rc.contains("savefig.transparent: True"));
        }
    }
}
