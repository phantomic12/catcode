# Building Catalyst Code from source

This guide covers building the Rust core and Go TUI from a checkout of the
repository. Most users should follow the [installation guide](installation.md)
and download prebuilt binaries — you only need this page if you're hacking on
the code or building for an unsupported platform.

---

## TL;DR — build the TUI-only core

```bash
git clone https://github.com/catalystctl/catcode
cd catcode
bash build.sh           # auto-detects; builds TUI-only core if WebKitGTK is missing
```

`build.sh` probes `pkg-config --exists gio-2.0` and skips the `native-browser`
feature when it's absent, so headless servers and CI containers build cleanly
without any GUI dependencies. To force one mode or the other:

| Flag         | Effect                                                                            |
|--------------|-----------------------------------------------------------------------------------|
| (no flag)    | Auto-detect: build `native-browser` when `gio-2.0` is on pkg-config search path    |
| `--with-web` | Force building `native-browser`. Fails if WebKitGTK system headers are missing    |
| `--no-web`   | Force a TUI-only build (no `native-browser`, no GUI dependencies)                 |
| `--run`      | After building, exec the freshly-built TUI with the new core                      |

Append `--run` (and any TUI args) to start the TUI immediately:

```bash
bash build.sh --run
```

---

## Prerequisites

The TUI-only build is intentionally lean — it has **no system dependencies
beyond the toolchain**. The web-enabled build needs GTK3 + WebKitGTK 4.1
because `core/Cargo.toml`'s `native-browser` feature pulls in `wry` (which
links to the host browser engine).

| Component        | Version     | Why                                                                |
|------------------|-------------|--------------------------------------------------------------------|
| Rust (stable)    | >= 1.78     | Builds `core` (`core/Cargo.toml`)                                  |
| Go               | >= 1.25     | Builds the `tui` binary (`tui/go.mod`)                             |
| pkg-config       | any         | Probed by `build.sh` for the WebKitGTK auto-detect                 |
| **Web build only:**                                                                 |
| GTK3 + WebKitGTK | Linux only  | Required by the `native-browser` cargo feature (see below)         |

### Linux: install GTK3 + WebKitGTK for the `native-browser` build

Debian / Ubuntu:

```bash
sudo apt-get install -y libgtk-3-dev libwebkit2gtk-4.1-dev libgio-2.0-dev
```

Fedora / RHEL:

```bash
sudo dnf install -y gtk3-devel webkit2gtk4.1-devel glib2-devel
```

Arch / Manjaro:

```bash
sudo pacman -S --needed gtk3 webkit2gtk-4.1 glib2
```

### macOS

The `native-browser` feature on macOS uses the system `WKWebView`, so **no
extra system packages are needed**. Make sure Xcode command-line tools are
installed:

```bash
xcode-select --install
```

### Windows

`native-browser` on Windows uses `WebView2` (bundled with recent Windows
10/11). No additional system packages required — install the
[WebView2 Runtime](https://developer.microsoft.com/en-us/microsoft-edge/webview2/)
if it isn't already present.

---

## Build modes

### Auto-detect (default)

`bash build.sh` without arguments probes for `gio-2.0` via `pkg-config`. When
it finds it, the build enables the `native-browser` cargo feature (matching
the behaviour of prebuilt binaries, which include the browser engine). When
it doesn't, the build emits a single notice line and produces a TUI-only
core:

```
notice: WebKitGTK system headers not found via pkg-config; skipping
        native-browser (pass --with-web once you've installed them)
[1/3] building core (cargo, TUI-only, -j24; native-browser skipped)...
```

### Force TUI-only (`--no-web`)

Use this on headless servers, in CI, or inside containers that don't have a
display server:

```bash
bash build.sh --no-web
```

The resulting `core` binary is fully functional for terminal workflows —
remote OAuth, file editing, shell, plugins, etc. — it just doesn't embed the
browser used by the Next.js web frontend.

### Force web-enabled (`--with-web`)

If you've installed the WebKitGTK headers in a non-standard location, set
`PKG_CONFIG_PATH` and pass `--with-web`:

```bash
PKG_CONFIG_PATH=/opt/gtk3/lib/pkgconfig bash build.sh --with-web
```

If WebKitGTK is missing, `--with-web` fails with the same `pkg-config`
errors you saw before this flag existed; install the dev packages listed
above and retry.

---

## What `build.sh` does

1. Build the Rust core (`core/target/release/core`) — with `native-browser`
   when available, TUI-only otherwise.
2. Build the Go TUI (`tui/tui`).
3. If a `catcode` binary is on `PATH`, replace it in place (and replace its
   companion `catcode-core` next to it). The TUI and core are always
   replaced together so the protocol versions stay in sync.

Use `--run` to exec the freshly-built TUI immediately:

```bash
bash build.sh --run -- some --tui flags
```

---

## Troubleshooting

### `pkg-config` can't find `atk` / `gio-2.0` / `webkit2gtk-4.1` / `pango`

You're trying to build with `native-browser` enabled on a host that lacks
the GTK3 / WebKitGTK development headers. Either install the packages from
[the table above](#linux-install-gtk3--webkitgtk-for-the-native-browser-build)
or pass `--no-web` to skip the GUI feature.

### `error: cannot replace <path> (directory is not writable and sudo is unavailable)`

`build.sh` tries to write the freshly-built binaries over whatever
`catcode` / `catcode-core` is on `PATH`. If those live under
`/usr/local/bin` and you can't `sudo`, either run `build.sh` as the user
that owns that directory or just leave the freshly-built binaries in place
(they are still at `core/target/release/core` and `tui/tui`).

### Sandbox / `microsandbox` errors on Linux without KVM

The default `cargo` features include `microsandbox`, which needs KVM on
Linux. Disable it for the build:

```bash
cargo build --release --no-default-features --features native-browser \
    --manifest-path core/Cargo.toml
```

`build.sh` doesn't expose this knob — use plain `cargo build` directly when
you need to override defaults.

---

## See also

- [installation.md](installation.md) — recommended path for end users
  (downloads prebuilt binaries; no compiler required).
- [quickstart.md](quickstart.md) — first 5 minutes after install.
- [CONTRIBUTING.md](../CONTRIBUTING.md) — dev workflow, test layout, commit
  style.
