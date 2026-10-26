# Herse.app: macOS packaging

Turns the `herse` binary into a normal, double-clickable macOS application.

| File           | What it is                                                                 |
| -------------- | -------------------------------------------------------------------------- |
| `bundle.sh`    | Builds the release binary and assembles, signs and (optionally) zips the app |
| `Info.plist`   | Bundle metadata template (`@VERSION@` is filled in from the crate)          |
| `make_icon.py` | Draws the icon (the logo in `native/brand/`) with Pillow                    |
| `AppIcon.icns` | Generated icon, checked in so a build needs neither Python nor Pillow       |
| `AppIcon-1024.png` | The 1024 px master, for reference                                       |

## Build

From anywhere:

```sh
native/packaging/macos/bundle.sh          # -> native/target/release/Herse.app
native/packaging/macos/bundle.sh --dmg    # also -> native/target/release/Herse-<version>-<arch>.dmg
native/packaging/macos/bundle.sh --no-build   # re-package the existing target/release/herse
```

The script runs `nice -n 10 cargo build --release -p herse`, then rebuilds
`Herse.app` from scratch (so it is safe to run repeatedly), ad-hoc signs it
(`codesign --force --deep -s -`) and verifies the signature. The version comes
from the `herse` crate (`[workspace.package]` in `native/Cargo.toml`).
It stops at the first error. It never installs anything and never touches
Launch Services. Environment knobs are listed in `bundle.sh --help`.

Bundle layout:

```
Herse.app/Contents/
  Info.plist
  PkgInfo
  MacOS/herse
  Resources/AppIcon.icns
```

## Install

Drag `Herse.app` (or the one inside the DMG) to `/Applications`, or:

```sh
ditto native/target/release/Herse.app /Applications/Herse.app
```

The signature is ad-hoc, which is enough for Gatekeeper to run an app built on
the same machine. If you copy the app to another Mac (or it picks up a
quarantine flag from a download), right-click, Open the first time, or run
`xattr -dr com.apple.quarantine /Applications/Herse.app`. Distributing
beyond your own machines needs a Developer ID signature and notarization,
which this script does not do.

Optional command-line launcher:

```sh
ln -s /Applications/Herse.app/Contents/MacOS/herse /usr/local/bin/herse
herse notebook.ipynb some/folder
```

## Opening notebooks from Finder

`Info.plist` declares `.ipynb` (UTI `org.jupyter.ipynb`, conforming to
`public.json`) and folders as document types with handler rank **Alternate**.
That means Herse appears under right-click, Open With, and notebooks or folders
can be dropped on the Dock icon, but Herse does not take over any default.

If Open With does not list Herse right after installing, Launch Services has
not indexed it yet. Either wait, or run (your call, nothing here does it for you):

```sh
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f /Applications/Herse.app
```

### Make Herse the default for .ipynb (optional)

In Finder: select any `.ipynb`, press Cmd-I, under **Open with** choose Herse,
then click **Change All...**.

Or from the shell, with [duti](https://github.com/moretension/duti)
(`brew install duti`):

```sh
duti -s dev.herse.app org.jupyter.ipynb all
# if another app owns a different UTI for .ipynb on your machine:
duti -s dev.herse.app .ipynb all
```

## Notes

- **Startup timing:** the binary reads `HERSE_TIMING=1` and prints startup
  timings to stderr. Run the binary directly to see them:
  `HERSE_TIMING=1 /Applications/Herse.app/Contents/MacOS/herse some.ipynb`.
  (`HERSE_NO_RECENTS` and `HERSE_DEBUG_FONTS` are also read by the app.)
- **How Finder delivers files:** Finder, the Dock and `open -a Herse file`
  hand files to a running or starting app as Apple Events, not as argv. GPUI
  surfaces them through `Application::on_open_urls` (`file://` URLs, percent-
  encoded). The app already registers that callback in `herse::run` and funnels
  the paths into the workspace. `herse [path...]` argv is still honoured when
  the binary is run directly.
- **Environment:** an app launched from Finder or the Dock gets a minimal
  `PATH` and no activated Python environment, unlike a shell launch. Kernel
  discovery already scans the common conda/Homebrew prefixes for this reason.
  A kernelspec that relies on a bare `python`/`python3` from `PATH` may
  therefore resolve to a different interpreter than it does in a terminal.
- **Folder access prompts:** the app is not sandboxed. The first time it reads
  from Desktop, Documents, Downloads or a removable volume, macOS shows the
  usual prompt, with the wording from the `NS*UsageDescription` keys in
  `Info.plist`. Because the signature is ad-hoc, it is tied to the exact
  binary; after each rebuild macOS may ask again.
- **Bundle id:** `dev.herse.app`, the same id as the Electron build
  (`electron-builder.yml`), so both builds share Launch Services preferences.
  Pick one to be the default if you keep both installed.

## Regenerating the icon

```sh
/opt/anaconda3/bin/python3 native/packaging/macos/make_icon.py        # rewrites AppIcon.icns and AppIcon-1024.png
```

Needs Pillow and macOS `iconutil`. `bundle.sh` calls it automatically if
`AppIcon.icns` is missing or older than `make_icon.py`. The design is on the
Big Sur grid (824 px rounded square, 185 px corners, soft shadow on a 1024 px
canvas): graphite background, a stack of three notebook cells with a `[ ]`
prompt mark, hierarchy from value contrast only.
