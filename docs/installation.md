# Installation and Operations Guide

Catalyst Code runs on Linux, macOS, and Windows. The recommended install
downloads prebuilt binaries ��� no compiler, no clone. A single `catcode` binary
gives you the terminal UI; add the web frontend for browser-based access.

---

## Prerequisites

| Requirement | Notes |
|-------------|-------|
| **OS** | Linux (x86_64 or arm64), macOS (arm64 or x86_64), Windows (x86_64) |
| **curl + coreutils** | Needed by the Unix installers (`sha256sum` for integrity check) |
| **Node.js 22.13+** | Required to run or build the web frontend (`node:sqlite`) |
| **Rust (stable) + Go 1.24+** | Only needed when building from source |

The TUI has **zero host dependencies** beyond what the installer downloads.
The web service requires Node.js 22.13+.

---

## Quick Install

### Terminal-only (Linux & macOS)

```bash
curl -fsSL https://raw.githubusercontent.com/catalystctl/catcode/refs/heads/master/install.sh | bash
```

Installs the `catcode` TUI binary to `/usr/local/bin` (system-wide). The
prebuilt TUI embeds the Rust core and extracts it to
`~/.cache/catalyst-code/` on first run — a separate `catcode-core` on PATH is
**not** installed for terminal-only installs.

### Terminal + Web service (Linux & macOS)

```bash
curl -fsSL https://raw.githubusercontent.com/catalystctl/catcode/refs/heads/master/install-web.sh | bash
```

This is equivalent to `install.sh --with-web`. It downloads both the TUI
(`catcode`) and the headless core (`catcode-core`), extracts a prebuilt
Next.js web bundle, and configures a background service.

Open `http://localhost:49283` after the installer finishes.

### Terminal-only (Windows)

```powershell
irm https://raw.githubusercontent.com/catalystctl/catcode/refs/heads/master/install.ps1 | iex
```

Installs `catcode.exe` to `%LOCALAPPDATA%\Programs\catcode` and adds it to
your user PATH. Open a **new terminal window** (PowerShell, Command Prompt, or
Windows Terminal) so PATH reloads, then run `catcode`. The binary works from
CMD and PowerShell alike once PATH is refreshed.

### Terminal + Web service (Windows)

PowerShell's `iex` cannot forward script parameters, so use the scriptblock
form:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/catalystctl/catcode/refs/heads/master/install.ps1))) -WithWeb
```

This installs `catcode.exe`, `catcode-core.exe`, the prebuilt web bundle, and
configures it as an NSSM service (if `nssm` is on PATH) or a logon Scheduled
Task otherwise.

---

## Install Options (Linux & macOS)

Pass options after `bash -s --`:

```bash
curl -fsSL .../install.sh | bash -s -- --with-web --port 8080 --host 127.0.0.1
```

| Option | Default | Description |
|--------|---------|-------------|
| `--install` | — | Install (skip the interactive menu) |
| `--with-web` | off | Also install the web frontend service |
| `--add-web` | — | Add the web service to an existing install |
| `--update` | — | Re-download the latest release and reinstall |
| `--reinstall` | — | Reinstall the currently-installed version |
| `--uninstall` | — | Stop + remove binaries, service, and state |
| `--status` | — | Show current install state (version, paths, web status) |
| `--version <v>` | latest | Pin a release (e.g. `0.2.0`, `v0.2.0`, or a commit SHA) |
| `--base-url <url>` | GitHub Releases | Download from a mirror instead of GitHub |
| `--build-from-source` | off | Compile locally instead of downloading prebuilt binaries |
| `--repo <url>` | — | Clone `<url>` first, then install from the checkout |
| `--prefix <path>` | `/usr/local/bin` | Binary install directory |
| `--web-dir <path>` | `/opt/catalyst-code/web` (Linux)<br>`~/Library/Application Support/catalyst-code/web` (macOS) | Web bundle directory |
| `--port <n>` | `49283` | Web service port |
| `--host <h>` | `0.0.0.0` | Web service bind host |
| `--skip-service` | off | Install web files only (do not write/start the service unit) |
| `--force-web-service` | off | Replace a non-installer-managed service unit |
| `--log-file <path>` | `~/catalyst-code-install.log` | Log file path |
| `--no-log` | off | Disable logging |
| `--no-color` | off | Disable ANSI colors |
| `--dry-run` | off | Print the plan, execute nothing |
| `-h`, `--help` | — | Show help and exit |

All options also work in the **interactive menu** that appears when `install.sh`
is run with no arguments in a real terminal (no piping).

---

## Install Options (Windows)

### Execution policy and local scripts

- **Recommended one-liner** (`irm | iex` or the scriptblock form) runs as an
  expression and usually works under the default Restricted policy.
- **Saved `install.ps1`** may be blocked by Mark-of-the-Web or execution policy.
  Unblock then run with Bypass:
  ```powershell
  Unblock-File .\install.ps1
  pwsh -ExecutionPolicy Bypass -File .\install.ps1
  ```
- Per-user PATH install does **not** require Administrator. Elevation is only
  needed for one-time sandbox features (Windows Hypervisor Platform) or
  updating a protected system-wide install.


Pass options using the scriptblock form:

```powershell
& ([scriptblock]::Create((irm .../install.ps1))) -WithWeb -Port 8080 -BindHost 127.0.0.1
```

Or from a local clone:

```powershell
pwsh -ExecutionPolicy Bypass -File .\install.ps1 -WithWeb
```

| Option | Default | Description |
|--------|---------|-------------|
| `-Version <v>` | latest | Pin a release (e.g. `"0.2.0"` or `"v0.2.0"`) |
| `-BaseUrl <url>` | GitHub Releases | Download from a mirror |
| `-InstallDir <path>` | `%LOCALAPPDATA%\Programs\catcode` | Binary install directory |
| `-WithWeb` | off | Also install the web frontend service |
| `-Port <n>` | `49283` | Web service port |
| `-BindHost <h>` | `0.0.0.0` | Web service bind host |
| `-WebDir <path>` | `%LOCALAPPDATA%\catalyst-code\web` | Web bundle directory |
| `-AddWeb` | — | Add web service to an existing install |
| `-Update` | — | Re-download latest + reinstall |
| `-Reinstall` | — | Reinstall the current version |
| `-Uninstall` | — | Stop + remove binaries, service, and state |
| `-Status` | — | Show current install state |
| `-DryRun` | off | Print the plan, execute nothing |
| `-NoColor` | off | Disable colored output |
| `-Help` | — | Show help and exit |

When run with **no arguments** in an interactive terminal, the installer shows
a numbered menu (install, install with web, add web, update, reinstall,
uninstall, status).

---

## Prebuilt Binaries (Standalone)

Prebuilt artifacts are published to every [GitHub Release](https://github.com/catalystctl/catcode/releases).
No installer needed — download and run.

| Platform | Artifact | Run |
|----------|----------|-----|
| **Linux** | `catcode-<ver>-<arch>.AppImage` | `./catcode-<ver>-x86_64.AppImage` |
| **macOS** | `catcode-<ver>-macos-{arm64,x86_64}.dmg` | Mount → run "Install catcode.command" |
| **Windows** | `catcode-<ver>-windows.msi` | `msiexec /i catcode-<ver>-windows.msi` |

Each platform also ships a **standalone executable** with the Rust core embedded
(`-tags embed_core`) — one file, no install, run from any directory:

| Platform | Standalone executable |
|----------|-----------------------|
| **Linux** | `catcode-<ver>-linux-x86_64` |
| **macOS** | `catcode-<ver>-macos-arm64` (or `-macos-x86_64`) |
| **Windows** | `catcode-<ver>-windows-x86_64.exe` |

Windows MSI alternatives:

```powershell
# Per-user MSI — no admin, clean Add/Remove Programs entry
msiexec /i catcode-<ver>-windows.msi            # interactive
msiexec /i catcode-<ver>-windows.msi /quiet     # silent
```

---

## Service Management

When installed with the web frontend, a background service keeps the web UI
running. The installer creates and enables it automatically.

### Linux (systemd)

- **Unit file:** `/etc/systemd/system/catalyst-code-web.service`
- **Service name:** `catalyst-code-web.service`
- **Logs:** `journalctl -u catalyst-code-web.service -f`

```bash
# Manual control
sudo systemctl stop    catalyst-code-web.service
sudo systemctl start   catalyst-code-web.service
sudo systemctl restart catalyst-code-web.service
sudo systemctl status  catalyst-code-web.service
```

The unit runs the prebuilt Next.js standalone server (`start.js`) under the
detected Node.js runtime. Service user is the user who ran the installer.
Managed units are annotated with `# Managed-by: install.sh`.

### macOS (launchd)

- **Plist:** `~/Library/LaunchAgents/com.catalyst-code.web.plist`
- **Label:** `com.catalyst-code.web`
- **Logs:** `~/Library/Logs/catalyst-code-web.log`

```bash
# Manual control
launchctl unload ~/Library/LaunchAgents/com.catalyst-code.web.plist
launchctl load   ~/Library/LaunchAgents/com.catalyst-code.web.plist
```

The agent runs at login and auto-restarts on crash. It runs the prebuilt
standalone server (`start.js`) under Node.js 22.13+.

### Windows (NSSM or Scheduled Task)

The installer prefers **NSSM** (Non-Sucking Service Manager) for a true
boot-time Windows Service. If NSSM is not on PATH, it falls back to a **logon
Scheduled Task** with a restart-loop wrapper.

**NSSM service:**
- **Service name:** `CatalystCodeWeb`
- **Binary:** runs `node start.js` (or `bun`)
- **Auto-start:** boot
- **Logs:** `%LOCALAPPDATA%\catalyst-code\catalyst-code-web.log`

```powershell
nssm stop   CatalystCodeWeb
nssm start  CatalystCodeWeb
nssm status CatalystCodeWeb
```

**Scheduled Task (fallback):**
- **Task name:** `CatalystCodeWeb`
- **Trigger:** at logon (user must be logged in)
- **Logs:** `%LOCALAPPDATA%\catalyst-code\catalyst-code-web.log`

> Install NSSM (https://nssm.cc) and re-run the installer for a true boot-time
> service.

---

## Updating

### CLI self-update

```bash
catcode --update
# or: catcode -u
# or: catcode update
```

This performs the following steps (from `tui/update.go`):

1. Fetches the latest release tag from the GitHub API (`/repos/catalystctl/catcode/releases/latest`)
2. Matches the running platform's asset name (e.g. `catcode-<sha>-linux-x86_64`)
3. Downloads the asset with a progress meter and verifies its SHA-256 checksum
4. Atomically replaces the running executable (on Windows: renames the current
   binary to `.old`, then swaps the new one in)
5. If the web frontend was previously installed (detected via installer state
   file or well-known paths), also updates `catcode-core` and the web bundle,
   then restarts the service

**Privilege escalation:** If `catcode` is installed system-wide (e.g.
`/usr/local/bin`) and the current user is not root, the update re-executes
itself under `sudo` automatically. If sudo is unavailable, it prints a clear
error with the correct command.

### Update via the installer

```bash
# Linux / macOS
bash install.sh --update

# Windows
install.ps1 -Update
```

Both re-download the latest release and reinstall all components. The web
service is restarted automatically if it was previously installed.

### Update via the web UI

Open **Settings → About → Update CLI + frontend**. This triggers
`catcode --update` under the hood.

### Check for updates

```bash
catcode --check-update
```

Exits `0` regardless of answer (scripting-friendly). Reports whether a newer
release exists, and whether the web frontend is behind.

### Launch-time update check (TUI)

On every launch, `catcode` checks a cache file at
`~/.cache/catalyst-code/update-check.json`. If the cache is fresh (less than
6 hours old), the answer is instant — no network. Stale or missing caches
trigger an async fetch. When a newer release is found, a one-line banner
appears at the top of the TUI:

```
⚡ Update available: <latest>  (you're on <current>)  —  run catcode --update
```

Dev builds (`coreVersion == "dev"`) never nag.

---

## Uninstalling

### Linux / macOS

```bash
curl -fsSL https://raw.githubusercontent.com/catalystctl/catcode/refs/heads/master/install.sh | bash -s -- --uninstall
```

Or from a local checkout:

```bash
bash install.sh --uninstall
```

Removes (from code in `install.sh` `do_uninstall()`):

- Systemd unit (stop → disable → remove file) or launchd plist (unload → delete)
- `/usr/local/bin/catcode`
- `/usr/local/bin/catcode-core` (when present — only installed with `--with-web` / source builds)
- Web bundle directory (`/opt/catalyst-code/web` or `~/Library/Application Support/catalyst-code/web`)
- Installer state file (`/etc/catalyst-code/installer.state`)
- The git repo clone (if built from source) is left untouched

### Windows

```powershell
& ([scriptblock]::Create((irm .../install.ps1))) -Uninstall
```

Or from a local clone:

```powershell
pwsh -ExecutionPolicy Bypass -File .\install.ps1 -Uninstall
```

Removes (from code in `install.ps1` `Do-Uninstall()`):

- NSSM service or Scheduled Task (stop + delete)
- `catcode.exe` and `catcode-core.exe`
- Web bundle directory
- Installer state

Open a **new terminal window** (PowerShell, CMD, or Windows Terminal) after
uninstalling for a clean PATH. Script uninstalls also remove the install
directory from your user PATH.

---

## Building from Source

Build from source when you need the latest unreleased changes, or when you
cannot use prebuilt binaries. See [build.md](build.md) for the full guide,
including the GTK3 + WebKitGTK requirement for the `native-browser` build
and the `--no-web` flag for headless / CI environments.

### Quick build (core + TUI)

```bash
# From the repo root:
./build.sh
# Or build and launch the matching local core immediately:
./build.sh --run
```

This runs:

```bash
cargo build --release --manifest-path core/Cargo.toml   # → core/target/release/core
( cd tui && go build -o tui . )                           # → tui/tui
```

`build.sh` replaces the `catcode` binary found on your `PATH` and its sibling
`catcode-core` when present. If no `catcode` is on `PATH`, it leaves the built
artifacts in the repository. If `CATCODE_CORE` points at another binary,
`./build.sh --run` still uses the freshly built core explicitly.

Requires Rust (stable) and Go 1.24+.

### Web frontend

```bash
cd sdk && bun install && bun run build   # → sdk/dist/
cd ../web && npm install && npm run build # Next.js build
```

Requires Node.js 22.13+ and npm. Bun may still be used for dependency
installation and unit tests.

### Manual run (no service wrapper)

After building, run the dev core directly from the TUI:

```bash
./tui/catcode    # finds ../core/target/release/core automatically
```

For the web frontend:

```bash
cd web && PORT=49283 CATCODE_CORE=/path/to/catcode-core npm run start
```

### Full installer (source path)

```bash
bash install.sh --build-from-source --with-web
```

This clones the repo (or uses the current directory), builds everything, and
installs binaries + web service. Pass `--repo <url>` to clone from a fork.

---

## Docker

The repository contains a Dockerfile at `packaging/vm-images/linux/Dockerfile`
for an **ephemeral Linux test container** (Ubuntu 24.04 with Xvfb, Firefox,
noVNC). This is used for automated testing of the TUI and web frontend in a
headless environment — it is **not** a production deployment image.

There is no general-purpose Docker image for running the Catalyst Code service.

---

## Private Repos and Air-Gapped Install

If the repository is private (pre-publication), the `raw.githubusercontent.com`
one-liners won't work. Options:

- **Clone locally** and run `bash install.sh` / `pwsh -File install.ps1` from
  the checkout.
- **Use a mirror:** pass `--base-url <url>` (Unix) or `-BaseUrl <url>` (Windows)
  to point the installer at a public HTTP server hosting the release artifacts.

---

## State File

The installer records its state for use by update and uninstall commands:

| Platform | State file |
|----------|-----------|
| Linux | `/etc/catalyst-code/installer.state` |
| macOS | `/etc/catalyst-code/installer.state` |
| Windows | `%LOCALAPPDATA%\catalyst-code\installer.state` |

The state file is shell-sourcable (key=value format on Unix, JSON on Windows).
It records the install method, version, paths, port, host, and whether the web
service was installed. This file is read by `catcode --update` (via
`tui/update_web.go`) to determine companion components.

---

## Updating this Document

This document was written from these source files:

- `install.sh` — Unix installer (lines describing all `--option` flags, systemd
  and launchd service management, uninstall logic, build-from-source path)
- `install.ps1` — Windows installer (all `-Option` flags, NSSM/Scheduled Task
  service management, uninstall logic)
- `install-web.sh` — Thin wrapper that invokes `install.sh --with-web`
- `tui/update.go` — Self-update CLI (`catcode --update`), launch-time check,
  cache, platform asset resolution
- `tui/update_web.go` — Companion component updates (catcode-core + web bundle
  + service restart), installer state detection
- `tui/embed_core.go` — Embedded core extraction for standalone binaries
- `README.md` — Usage descriptions, architecture overview
- `build.md` — Building from source (GTK3 / WebKitGTK requirements, `--no-web`)
- `build.sh` — Minimal build script
- `packaging/vm-images/linux/Dockerfile` — Test Docker image

See [commands/](commands/) for CLI flag reference and
[configuration/](configuration/) for environment variables and settings.
