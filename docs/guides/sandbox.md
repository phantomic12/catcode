# Sandbox Guide: Microsandbox

Catalyst Code can run every agent-controlled workload — `bash`, `git`,
diagnostics, plugin scripts, and subagent commands — inside a **Microsandbox
microVM** instead of directly on your host. This is a real, OS-enforced
isolation boundary: a separate Linux kernel, a separate filesystem root, and an
independent network stack. It is *not* a denylist, a wrapper script, or a
chroot.

> Microsandbox is **beta software**. Catalyst Code pins a specific released
> version (`microsandbox` 0.6.6) and isolates its API behind CatCode-owned
> abstractions in `core/src/sandbox/`. This guide describes what that release
> supports today; see [Current upstream beta limitations](#current-upstream-beta-limitations)
> for the known gaps.

---

## Table of Contents

- [At a glance](#at-a-glance)
- [What Microsandbox protects](#what-microsandbox-protects)
- [What stays in the trusted control plane](#what-stays-in-the-trusted-control-plane)
- [Enabling the sandbox](#enabling-the-sandbox)
- [Linux setup (KVM)](#linux-setup-kvm)
- [macOS setup (Apple Silicon)](#macos-setup-apple-silicon)
- [Windows setup (WHP)](#windows-setup-whp)
- [Nested virtualization](#nested-virtualization)
- [First-run runtime & image download](#first-run-runtime--image-download)
- [Offline image import](#offline-image-import)
- [Custom project images](#custom-project-images)
- [Resource settings](#resource-settings)
- [Network policy](#network-policy)
- [Environment-variable policy](#environment-variable-policy)
- [Workspace mapping & file confinement](#workspace-mapping--file-confinement)
- [Shell behavior](#shell-behavior)
- [Git behavior](#git-behavior)
- [Diagnostics behavior](#diagnostics-behavior)
- [Plugin behavior](#plugin-behavior)
- [Subagent isolation](#subagent-isolation)
- [Cancellation & timeouts](#cancellation--timeouts)
- [Slash commands](#slash-commands)
- [Troubleshooting](#troubleshooting)
- [Resetting a broken sandbox](#resetting-a-broken-sandbox)
- [Explicitly disabling sandboxing](#explicitly-disabling-sandboxing)
- [Current upstream beta limitations](#current-upstream-beta-limitations)

---

## At a glance

| Property | Value |
|----------|-------|
| Backend | Official Microsandbox **Rust SDK** (crate `microsandbox` 0.6.6, pinned) |
| Linux | KVM (`/dev/kvm`) — x86_64 and aarch64 |
| macOS | Apple Silicon (arm64) only |
| Windows | Windows Hypervisor Platform (WHP) — x86_64 (preview) |
| Intel macOS | **Unsupported** — no workaround |
| External CLI required? | No. No `msb`, Docker, Podman, Firejail, WSL, or daemon. |
| Runtime download | The SDK downloads its own runtime (`msb` + `libkrunfw`) into a CatCode cache on first use. Admin rights not required. |
| Default image | `ghcr.io/catalystctl/catcode-sandbox:0.1` (polyglot developer image) |
| Modes | `none` (off) · `microsandbox` (on) |
| Fail behavior | **Fail closed** — never falls back to the host |

---

## What Microsandbox protects

When sandboxing is enabled, agent-controlled process execution is lifted out
of your host OS entirely and run inside a lightweight virtual machine:

- **Separate kernel.** A guest Linux kernel boots per sandbox. The agent cannot
  reach host syscalls, host kernel modules, or host devices.
- **Separate filesystem root.** The guest has its own root filesystem drawn from
  the sandbox image. Only your workspace is mounted in (writable, at `/workspace`).
  The host home directory, `~/.ssh`, `~/.gitconfig`, cloud credentials, browser
  data, Docker sockets, and arbitrary host paths are **not** mounted.
- **Separate network stack.** Network egress is governed by a Microsandbox
  network policy (`none` / `restricted` / `allowlist`), not by host firewall
  rules the agent could try to reconfigure.
- **No host environment inheritance.** The guest receives a minimal, explicit
  environment. Secrets in your host environment (`*_TOKEN`, `*_SECRET`,
  `AWS_*`, `GITHUB_TOKEN`, `SSH_AUTH_SOCK`, …) are denied by default.
- **No credential forwarding.** The host SSH agent socket and Docker socket are
  never mounted into the guest.

This means that even a fully compromised agent — one that has been steered into
running hostile commands — is contained: it cannot read your `~/.ssh/id_rsa`,
cannot exfiltrate `~/.aws/credentials`, cannot reach your host Docker daemon,
and cannot persist anything outside the mounted workspace.

The approval gate and the sandbox are **independent security layers**. The
sandbox does not replace the approval gate; the approval gate does not replace
the sandbox. See [File confinement](#workspace-mapping--file-confinement) for
the critical interaction with `Approval::Never`.

---

## What stays in the trusted control plane

Not everything can run inside the guest — the guest itself has to be created and
managed. These operations are **trusted CatCode control-plane** work and run on
the host. They are narrowly scoped and audited:

| Operation | Why it stays on the host |
|-----------|--------------------------|
| Starting the CatCode core / TUI / web service | These *are* the host process. |
| Opening the host browser for an OAuth authorization URL | Only the URL open is host-side; the plugin that produced the URL ran in the guest. |
| Preparing Microsandbox runtime assets | Downloading/verifying the `msb` + `libkrunfw` runtime into the CatCode cache. |
| Pulling or importing the configured sandbox image | Fetching the OCI image the guest will boot from. |

Everything that an agent tool call can reach — `bash`, `git_*`, `diagnostics`,
plugin hooks/tools/commands/memory-providers, and subagent command execution —
runs inside the microVM when sandboxing is enabled.

---

## Enabling the sandbox

```bash
catcode --sandbox microsandbox
```

or via environment variable:

```bash
export CATALYST_CODE_SANDBOX=microsandbox
```

or in a settings file (`settings.json`):

```json
{
  "settings": {
    "sandbox": "microsandbox"
  }
}
```

Accepted values:

| Value | Effect |
|-------|--------|
| `none`, `off`, `false`, `disabled` | Sandbox off. Commands run on the host (legacy behavior). |
| `microsandbox`, `msb`, `on`, `true`, `enabled` | Sandbox on. Commands run in the microVM. |

### Migrating from the old backends

The legacy Firejail, Seatbelt, and `sandbox-exec` backends have been removed.
Their config values are still accepted so existing config files keep working,
but they are migrated to **Microsandbox** (never silently to `none`):

| Old value | Migrated to | Behavior |
|-----------|-------------|----------|
| `firejail`, `fj` | `microsandbox` | A deprecation notice is emitted; Microsandbox preflight runs. |
| `seatbelt`, `macos`, `sandbox-exec` | `microsandbox` | A deprecation notice is emitted; Microsandbox preflight runs. |

The user's intention to *enable* sandboxing is preserved. If the environment
cannot run Microsandbox, CatCode **fails closed** rather than running on the
host — see [Fail-closed behavior](#troubleshooting).

`--no-network` / `CATALYST_CODE_NO_NETWORK` are kept for backward
compatibility and are now enforced through Microsandbox's network policy
(equivalent to `sandbox_network_mode: none`) instead of `unshare`.

---

## Linux setup (KVM)

Microsandbox on Linux uses **KVM** for hardware-accelerated virtualization.

### Requirements

- A CPU with Intel VT-x or AMD-V virtualization.
- `/dev/kvm` present and readable+writable by your user.
- x86_64 or aarch64 architecture.

### 1. Enable virtualization in BIOS/UEFI

Reboot into BIOS/UEFI and enable **Intel VT-x** or **AMD-V** (sometimes called
"SVM Mode" on AMD). Save and reboot into Linux.

### 2. Load the KVM kernel modules

```bash
sudo modprobe kvm
sudo modprobe kvm_intel    # on Intel CPUs
# — or —
sudo modprobe kvm_amd       # on AMD CPUs
```

To make this persistent across reboots, ensure the modules are not blacklisted
(distributions usually load `kvm` automatically once VT-x/AMD-V is on).

### 3. Grant your user access to `/dev/kvm`

`/dev/kvm` is usually owned by `root:kvm`. Add yourself to the `kvm` group:

```bash
sudo usermod -aG kvm "$USER"
```

You **must sign out and back in** (or reboot) for the new group membership to
take effect in your session.

### 4. Verify

```bash
test -r /dev/kvm && test -w /dev/kvm && echo "KVM is ready"
```

If that prints `KVM is ready`, you are set. The first time you enable the
sandbox, CatCode runs a [preflight](#troubleshooting) that confirms all of this
and reports exact remediation if anything is missing.

### AppImage note

If you run CatCode as an **AppImage**, ensure the AppImage process can access
`/dev/kvm`. AppImages run as your user, so the `kvm` group membership above
covers the normal case. In restricted/sandboxed container environments where
`/dev/kvm` is not passed through, the sandbox cannot start — use the standalone
binary instead, or pass `/dev/kvm` into the container.

---

## macOS setup (Apple Silicon)

Microsandbox on macOS uses the Apple Silicon virtualization framework. **No
external package is required** — no Homebrew formula, no QEMU, no separate
hypervisor.

### Requirements

- **Apple Silicon** Mac (M1/M2/M3/M4 family). This is the only supported macOS
  architecture.
- macOS 13 (Ventura) or newer.

### Intel Macs

Intel macOS is **unsupported**, and installing another package will not fix
that:

> Microsandbox requires Apple Silicon on macOS. This Intel Mac cannot run the
> local Microsandbox backend.

On an Intel Mac, leave the sandbox at `none`. File confinement and the approval
gate still apply.

---

## Windows setup (WHP)

Microsandbox on Windows uses the **Windows Hypervisor Platform (WHP)** —
native, no WSL required.

> Windows support in the pinned Microsandbox 0.6.6 release is **preview**-grade.
> Linux and Apple Silicon macOS are the most mature paths.

### 1. Enable hardware virtualization in BIOS/UEFI

Enable Intel VT-x / AMD-V in your firmware, same as the Linux section.

### 2. Enable the Windows Hypervisor Platform feature

Open an **elevated** terminal (PowerShell as Administrator, or elevated CMD)
and enable the feature. Daily `catcode` use does **not** need admin — only this
one-time WHP enable does.

```powershell
Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform -All
```

CMD equivalent:

```bat
DISM /Online /Enable-Feature /FeatureName:HypervisorPlatform /All
```

- The enable step must run **elevated**.
- A **restart** is usually required after enabling the feature.
- Hardware virtualization must also be enabled in BIOS/UEFI (step 1).

### 3. Verify

```powershell
Get-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform
```

`State` should read `Enabled`. If a restart is pending, complete the restart
first.

CatCode will not install WSL for you and does not require it. Prefer native WHP.

---

## Nested virtualization

If CatCode itself runs inside a VM (a cloud instance, a CI runner, a Parallels
VM, etc.), Microsandbox needs **nested virtualization** to be enabled by the
outer hypervisor.

- **Cloud providers:** enable nested virtualization on the instance type
  (e.g. AWS bare-metal, GCP nested-KVM-enabled instances, Azure nested-supported
  sizes). Not all instance types expose `/dev/kvm`.
- **Local VMs (Parallels/VMware/Hyper-V):** enable "nested virtualization" /
  "hypervisor applications" in the VM's CPU settings.

If nested virtualization is unavailable, the preflight reports
`nested_virtualization_unavailable` and the sandbox will not start. CatCode
does **not** automatically fall back to host execution in this case.

---

## First-run runtime & image download

The first time you enable Microsandbox, CatCode downloads two kinds of
user-space assets into an OS-appropriate CatCode cache directory (no admin
rights, no system install):

1. **The Microsandbox runtime** — the `msb` supervisor and `libkrunfw` kernel
   bundle the SDK needs to boot a microVM. Downloaded and verified by the SDK.
2. **The sandbox image** — the OCI image the guest boots from (default
   `ghcr.io/catalystctl/catcode-sandbox:0.1`).

This is a one-time download. Progress is surfaced to the TUI and web UI. Failed
downloads are retried automatically. **An administrator-level software
installation is never performed.** CatCode will never:

- change BIOS settings,
- enable Windows features without explicit user action,
- run `sudo`,
- modify group membership,
- install system packages, or
- reboot the computer.

After setup is complete, use **Recheck environment** (the `/sandbox recheck`
command or the button in the TUI/web settings) to re-run preflight.

---

## Offline image import

On an air-gapped host, pull the image on a connected machine and import it:

```bash
# On a connected host:
docker pull ghcr.io/catalystctl/catcode-sandbox:0.1
docker save ghcr.io/catalystctl/catcode-sandbox:0.1 -o catcode-sandbox.tar

# Transfer catcode-sandbox.tar to the offline host, then import it into the
# Microsandbox image store (consult the pinned SDK's import API; the core
# exposes this through the /sandbox setup flow).
```

CatCode's preflight treats an already-present image as ready and skips the
network pull. The runtime assets (`msb` + `libkrunfw`) can likewise be staged
in the CatCode cache directory ahead of time.

---

## Custom project images

The default image is a polyglot developer image (see [Image contents](#resource-settings)).
If your project needs a different or larger toolchain set, point at your own
image:

```json
{
  "settings": {
    "sandbox": "microsandbox",
    "sandbox_image": "ghcr.io/yourorg/your-sandbox:1.2"
  }
```

or via the CLI / env (where supported by the harness's config conventions):

```bash
catcode --sandbox microsandbox   # plus sandbox_image in settings.json
```

Requirements for a custom image:

- It must be an OCI image runnable by Microsandbox (a Linux image).
- It must contain the tools your diagnostics need (`cargo`, `tsc`, `go`, `python`,
  etc.) — CatCode does **not** fall back to host compilers when a tool is
  missing from the guest.
- Pin by digest where practical rather than a mutable `latest` tag.

### Missing-tool errors

When a required executable is unavailable in the active image, CatCode returns
an actionable error explaining:

- which executable is missing,
- which sandbox image is active,
- how to select a suitable image, and
- how to configure a project-specific image (`sandbox_image`).

You do not need Docker installed locally to use or build a custom image — any
OCI registry that serves the image works.

---

## Resource settings

The sandbox is configured with bounded resource limits appropriate for a
lightweight local agent:

| Field | Default | Description |
|-------|---------|-------------|
| `sandbox_image` | `ghcr.io/catalystctl/catcode-sandbox:0.1` | OCI image the guest boots from. |
| `sandbox_cpus` | `2` | vCPUs allocated to the microVM (must be positive, capped). |
| `sandbox_memory_mb` | `2048` | Guest RAM in MiB (safe minimum enforced). |
| `sandbox_disk_mb` | `8192` | Writable guest disk in MiB (safe minimum enforced). |
| `sandbox_idle_timeout_secs` | `900` | Idle sandbox is torn down after this many seconds. |

All values are validated: CPUs must be positive and capped, memory and disk
must be within safe minimums and maximums, and image references must be
non-empty.

### Default image contents

`ghcr.io/catalystctl/catcode-sandbox:0.1` is a multi-arch (amd64 + arm64)
polyglot developer image built from a Debian/Ubuntu base, including at least:

- `bash`, `coreutils`, `findutils`, `grep`, `sed`, `awk`, `git`
- `curl`, `ca-certificates`, `jq`, `ripgrep`
- `tar`, `gzip`, `xz`, `unzip`
- `build-essential` (common build utilities)
- **Rust** (via rustup), **Node.js** (via NodeSource), **Python**, and **Go**
  toolchains, so existing diagnostics continue to work inside the sandbox

The image definition lives at [`sandbox/Dockerfile`](../../sandbox/Dockerfile).
See [Image update process](../../sandbox/Dockerfile) for how to rebuild and
record the digest.

---

## Network policy

Network egress is governed by `sandbox_network_mode`:

| Mode | Behavior |
|------|----------|
| `none` | All guest network access denied. Equivalent to the legacy `--no-network`. |
| `restricted` (default) | Public package registries and source hosts permitted; cloud metadata addresses, host-only services, and private network ranges blocked by default. DNS-rebinding protection where supported. |
| `allowlist` | Only the hosts in `sandbox_network_allowlist` are reachable. |

Related fields:

| Field | Default | Description |
|-------|---------|-------------|
| `sandbox_network_allowlist` | `[]` | Hosts permitted under `allowlist` mode. |
| `sandbox_allow_private_networks` | `false` | When `true`, permit private network ranges (RFC 1918 / link-local). Off by default for safety. |

The domain/CIDR semantics are exactly those the Microsandbox SDK exposes —
CatCode does not invent divergent network semantics between the UI and the
backend.

### Interaction with `--no-network`

The legacy `--no-network` / `CATALYST_CODE_NO_NETWORK=1` flags map to
`sandbox_network_mode: none`. The host-side `fetch` and `web_search` tools keep
their own explicit allowlist behavior (`fetch_allowlist`) and are governed by
the same no-network intent when it is set — they are not an undocumented bypass.

### Trusted host-side network exceptions

The core itself makes network requests on the host (provider API streaming,
`fetch`, `web_search`, OAuth token exchanges). These are **trusted control-plane
operations** and are not routed through the microVM. The sandbox network policy
governs only agent-controlled guest processes.

---

## Environment-variable policy

The guest does **not** inherit your host process environment. It receives a
minimal, explicit environment:

```text
PATH
HOME
LANG
LC_ALL
TERM
CI
CATCODE_SANDBOX=1
CATCODE_WORKSPACE=/workspace
```

Additional variables can be allowlisted through `sandbox_env_allowlist` (entries
must be valid variable names):

```json
{
  "settings": {
    "sandbox_env_allowlist": ["RUST_BACKTRACE", "GOPROXY", "PYTHONPATH"]
  }
}
```

### Secrets are denied by default

Variables matching these patterns are **never** forwarded to the guest:

```text
*_TOKEN
*_SECRET
*_PASSWORD
*_API_KEY
AWS_*
AZURE_*
GOOGLE_*
GITHUB_TOKEN
NPM_TOKEN
SSH_AUTH_SOCK
```

Where the Microsandbox SDK supports destination-bound secret substitution,
CatCode exposes it through an explicit configuration structure. The full host
environment is never injected.

The host SSH agent socket (`SSH_AUTH_SOCK`) and Docker socket are not mounted.

---

## Workspace mapping & file confinement

The active workspace is mounted into the guest as the only writable host path:

```text
Host workspace  ──►  /workspace   (writable)
Guest cwd       ──►  /workspace   (or a validated subdirectory)
Guest /tmp      ──►  private guest tmp
Guest home      ──►  /home/catcode  (private, not your host home)
```

- `.git` is preserved inside the mounted workspace.
- The host home directory, `~/.ssh`, cloud credentials, browser data, the Docker
  socket, and system configuration are **not** mounted.
- Absolute host paths and `..` escapes are rejected — they cannot be turned into
  guest mount configuration through command input.
- Plugin global files are mounted read-only only when a plugin execution
  requires them, and plugin state gets a dedicated narrowly scoped mount rather
  than the entire CatCode config directory.

### `Approval::Never` does not weaken sandbox confinement

`Approval::Never` means "do not prompt" — it controls the **approval gate**,
not the **sandbox**. The two systems are independent:

- Under `Approval::Never`, host-side file tools still resolve against the
  workspace root; absolute host paths, `..`, and symlink escapes are still
  rejected.
- `Approval::Never` must **not** be read as "allow arbitrary host filesystem
  access."
- The host home directory, `.ssh`, and credentials remain unavailable.

When sandboxing is enabled, the sandbox is an additional layer on top of this:
even a path the agent could otherwise reach on the host is confined to the
guest's mounted workspace.

---

## Shell behavior

The tool stays named **`bash`** for protocol compatibility (the TUI, web, SDK,
and model prompt all reference `bash`).

- **Sandbox disabled:** the existing host-specific behavior is preserved — `bash`
  on Linux/macOS, PowerShell on Windows by default (`pwsh` if present). Override
  with `CATALYST_CODE_SHELL` (full path to Git Bash, `cmd.exe`, `pwsh`, etc.).
  `cmd` uses `/d /c`; PowerShell uses `-NoProfile -NonInteractive -Command`.
- **Microsandbox enabled:** commands run inside the Linux guest using the
  guest's `bash`. The model-facing tool description **and** system-prompt shell
  guidance are updated so Windows users are **not** told to generate PowerShell.
  A Windows host shell path is never passed into the Linux guest.

The effective shell description is part of the core `ready` state so the TUI,
web UI, SDK, and model prompt all agree.

---

## Git behavior

Built-in Git tools (`git_status`, `git_diff`, `git_log`, `git_show`, `git_add`, `git_push`, `git_pull`, `git_branch`,
`git_commit`) run through the shared sandbox execution layer when sandboxing is
enabled. Inside the guest:

```text
workspace ──► /workspace
GIT_PAGER=cat
PAGER=cat
HOME=/home/catcode
```

Git identity is handled without exposing host credentials. `user.name` and
`user.email` may be passed from a narrowly scoped CatCode setting. The host
`~/.gitconfig`, `~/.ssh`, `SSH_AUTH_SOCK`, and credential stores are **not**
mounted by default.

Because Git hooks, filters, diff drivers, and credential helpers can execute
code, structured Git arguments are **not** treated as safe-by-default — they run
inside the guest along with everything else. Existing timeout and output
truncation behavior is preserved.

---

## Diagnostics behavior

All diagnostics (`cargo check`, `tsc --noEmit`, `go build`, `py_compile`, …)
run inside the active sandbox. The default polyglot image includes the
Rust/Node/Python/Go toolchains so these continue to work.

- Cargo project detection, Node/TypeScript detection, Go detection, and Python
  detection are preserved.
- Timeouts, exit status, output truncation, and abort behavior are preserved.
- CatCode does **not** run a host compiler when sandboxing is enabled.
- If the image lacks the required language toolchain, you get an actionable
  sandbox-image error (see [Missing-tool errors](#custom-project-images)),
  never a silent host fallback.

---

## Plugin behavior

Project plugins are untrusted repository content and are especially important to
isolate. When sandboxing is enabled:

- Plugin hook scripts, tool scripts, slash-command scripts, and memory-provider
  scripts execute inside Microsandbox.
- Only the script/plugin directory needed for that execution is mounted, and it
  is mounted **read-only** unless the plugin explicitly requires a dedicated
  state directory (which gets a narrowly scoped writable mount).
- Hook JSON is passed through stdin. Existing input/output size limits,
  timeouts, and fail-open/fail-closed hook policies are enforced.

For OAuth plugins:

- The plugin may return an authorization URL from **inside** the sandbox.
- The trusted CatCode control plane may open that URL on the host browser.
- Token storage remains controlled by CatCode.
- An OAuth plugin is never given unrestricted host process access.

Provider keys and the full parent environment are not exposed to plugins.

---

## Subagent isolation

Sandboxes are keyed by workspace + session/run identity + worktree + effective
sandbox policy + image. The model:

- One sandbox per main session/workspace.
- Separate sandboxes for parallel subagent worktrees.
- Shared read-only image layers.
- Independent writable root filesystems.
- Explicitly mounted writable workspaces.

Parallel agents do **not** accidentally share a writable root filesystem unless
that sharing is deliberate and tested. Package installs and build caches persist
within a healthy, reused sandbox during a session; sandboxes are created lazily
on the first agent-controlled process request.

---

## Cancellation & timeouts

A timed-out or aborted command:

- terminates the guest process,
- terminates its descendants,
- stops producing output, and
- returns a deterministic timeout/cancellation result.

Simply dropping the Rust future is **not** relied upon to terminate the guest
process — CatCode explicitly tears it down. If process termination cannot be
confirmed, the sandbox is reset (see [Resetting a broken sandbox](#resetting-a-broken-sandbox)).

---

## Slash commands

```text
/sandbox                 # open the sandbox settings / status view
/sandbox status          # show current sandbox status, platform, image, limits
/sandbox enable          # enable Microsandbox (runs preflight first)
/sandbox disable         # explicitly disable sandboxing (set to none)
/sandbox setup           # prepare user-space runtime/image assets
/sandbox recheck         # re-run preflight after completing setup
/sandbox reset           # destroy and recreate an unhealthy sandbox
```

Changes that require a core restart prompt for confirmation (matching the
existing TUI restart conventions). The session is **not** marked sandboxed
while commands still run on the host.

---

## Troubleshooting

CatCode runs a **preflight** when you enable Microsandbox. It inspects the
environment and reports a structured report: requested, supported, ready,
platform, architecture, a list of checks, and a list of setup actions with
exact commands.

### Fail-closed behavior

If sandboxing is requested but unavailable, CatCode **does not** execute the
command on the host, does **not** silently switch to `none`, and does **not**
just log a warning and continue. It returns a structured setup-required error
and blocks agent-controlled process execution until the environment is ready or
you explicitly disable sandboxing.

This applies to: startup failures, VM failures, missing virtualization,
permission failures, image failures, runtime failures, and unsupported
platforms.

### Common preflight results

| Code | Meaning | Fix |
|------|---------|-----|
| `unsupported_platform` | The OS cannot run Microsandbox | Leave sandbox at `none`. |
| `unsupported_architecture` | e.g. Intel macOS | Use Apple Silicon, or `none`. |
| `virtualization_disabled` | VT-x/AMD-V off in firmware | Enable in BIOS/UEFI. |
| `kvm_device_missing` | `/dev/kvm` absent (Linux) | `sudo modprobe kvm` + vendor module; enable VT-x/AMD-V. |
| `kvm_permission_denied` | Can't read/write `/dev/kvm` | `sudo usermod -aG kvm "$USER"`, then sign out/in. |
| `nested_virtualization_unavailable` | Running in a VM without nested virt | Enable nested virt on the outer hypervisor. |
| `whp_disabled` | Windows Hypervisor Platform off | `Enable-WindowsOptionalFeature ... HypervisorPlatform` (admin) + restart. |
| `runtime_missing` / `runtime_download_failed` | Runtime assets absent/unreachable | Run `/sandbox setup`; check network. |
| `image_pull_required` / `image_pull_failed` | Image absent/unreachable | Run `/sandbox setup`; check registry/network. |
| `sandbox_boot_failed` | The microVM did not boot | `/sandbox reset`; check resources. |
| `guest_agent_unavailable` | Guest agent not responding | `/sandbox reset`. |

### Linux quick checks

```bash
# Is KVM present and usable?
test -r /dev/kvm && test -w /dev/kvm && echo "KVM is ready"

# Which vendor module to load?
grep -E 'vmx|svm' /proc/cpuinfo >/dev/null && echo "virt-capable CPU"
```

### Verifying what the agent can and cannot reach

When the sandbox is up, the agent can read/write `/workspace` but cannot reach
host-only paths: `/etc/passwd` is the **guest** file, the host home directory is
unavailable, host environment secrets are unavailable, and `.ssh` / Docker /
SSH-agent sockets are unavailable.

---

## Resetting a broken sandbox

If a sandbox becomes unhealthy (boot failures, stuck processes, a guest agent
that stops responding), reset it:

```text
/sandbox reset
```

This destroys the current microVM and recreates a fresh one from the image. The
mounted workspace is unaffected (only the guest root filesystem and runtime
state are reset). A reset is also triggered automatically if process
termination after a timeout cannot be confirmed.

---

## Explicitly disabling sandboxing

```bash
catcode --sandbox none
```

or at runtime:

```text
/sandbox disable
```

With the sandbox off, commands run on the host with the legacy behavior: the
host-specific shell, workspace file confinement, and the approval gate still
apply, but there is no microVM isolation. This is the right choice when your
environment cannot run Microsandbox or you do not want the overhead.

---

## Current upstream beta limitations

Microsandbox is beta software. With the pinned 0.6.6 release, be aware of:

- **Windows is preview-grade.** Linux (KVM) and Apple Silicon macOS are the most
  mature paths; Windows WHP support is usable but earlier in maturity.
- **Intel macOS is unsupported** with no planned workaround — it requires
  Apple Silicon.
- **First-run downloads** of the runtime (`msb` + `libkrunfw`) and the sandbox
  image require network access and take time proportional to their size. Plan
  for this on first enable; subsequent runs reuse cached assets.
- **Resource limits are bounded.** Very large builds may exceed the default
  `sandbox_memory_mb` / `sandbox_disk_mb`; raise them in settings if needed.
- The SDK's API may change between releases. CatCode isolates all Microsandbox
  SDK calls behind the `core/src/sandbox/` abstractions (a `microsandbox` cargo
  feature, default on) so future SDK updates do not spread through the codebase.

If a limitation blocks you, the fail-closed guarantee means CatCode tells you
exactly what is wrong rather than silently weakening isolation.
