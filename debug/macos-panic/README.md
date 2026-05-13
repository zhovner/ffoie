# macOS kernel panics observed while developing FFOIE

These are **macOS kernel panic logs**, captured during FFOIE development on a
Mac mini M4 running macOS 26.4.1 (build 25E253). They are copies of the files
written by macOS to `/Library/Logs/DiagnosticReports/`.

**Userspace applications cannot legitimately cause a kernel panic.** Each of
these is a bug in Apple's kernel, drivers, or hardware, not in FFOIE. They are
preserved here so we can show them to ourselves later, file Feedback Assistant
reports with Apple, and recognise repeat signatures if they occur.

## The crashes

| File | Subsystem | Cause |
| --- | --- | --- |
| `panic-base+socd-2026-05-13-143705.000.panic` | SoC (`base+socd`) | Hardware-level System-on-Chip diagnostic panic. No software backtrace — the SoC itself raised the alarm. |
| `panic-full-2026-05-13-155906.0002.panic` | `AppleCS42L84Audio` | Audio codec driver: `setPowerState(... 1→0) timed out after 15393 ms`. Cirrus Logic CS42L84 codec power-state transition hung the kernel. |
| `panic-full-2026-05-13-220817.0002.panic` | `universalaccessd` / `AppleARMWatchdogTimer` | Watchdog timeout (90 s). macOS accessibility daemon stopped responding to the kernel watchdog. |
| `panic-full-2026-05-13-224145.0002.panic` | `com.apple.sptm` / `AppleARMWatchdogTimer` | Watchdog timeout (94 s). Kernel was inside the Secure Page Table Monitor (SPTM) when the watchdog fired. SPTM is a recent macOS security feature on Apple Silicon and has been a known source of panics. |
| `panic-full-2026-05-14-004542.0002.panic` | `fileproviderd` / `AppleARMWatchdogTimer` | Watchdog timeout (92 s). macOS iCloud-Drive / Files.app sync daemon stopped responding. Probably correlated with heavy git + small-file activity in the iCloud-Drive-hosted project directory; the daemon has known performance issues under such load. The proximate bug is still in Apple's kernel (a userspace daemon backing up shouldn't panic the system), but our project layout was the likely trigger. |

None of the panic backtraces contain `wgpu`, `Metal`, `AGX`, `IOSurface`, or
any other graphics-stack driver. The FFOIE process is not on any of them.

## Hypothesis on the iCloud-Drive connection

The May 14 panic blamed `fileproviderd` — the macOS iCloud Drive / Files.app
sync daemon. The FFOIE project is hosted inside `~/Library/Mobile
Documents/com~apple~CloudDocs/`, which `fileproviderd` continuously watches.
Heavy git activity (many small object files), source-file edits, and a
multi-megabyte `debug/` folder appearing in that path can stress
`fileproviderd` to the point where it stops checking in with the kernel
watchdog, which eventually panics the system.

Mitigations (in increasing order of disruption):

- **Build artefacts already redirected** out of iCloud Drive via
  `../.cargo/config.toml` (`target-dir = "/Users/a/.cargo-target/foie"`).
- Consider **moving the project itself** out of iCloud Drive (e.g. to
  `~/code/foie/`) and pushing it to the GitHub remote instead of relying on
  iCloud for backup. iCloud + git + Rust is a documented pain point on
  macOS.

## How macOS writes these files

After a full system reboot from a kernel panic, macOS writes the panic log to
`/Library/Logs/DiagnosticReports/`. To list and read them yourself:

```sh
# Kernel panics
ls -lt /Library/Logs/DiagnosticReports/*.panic

# User-level app crashes (would appear here if `ffoie` itself ever segfaulted —
# none of these are FFOIE crashes; FFOIE has never appeared in this directory)
ls -lt ~/Library/Logs/DiagnosticReports/
```

You can also open them in **Console.app → "Crash Reports"** in the sidebar.

## Reporting upstream

Anything in this folder should be filed with Apple via **Feedback Assistant**
(`feedbackassistant.apple.com` or the Feedback Assistant app):

1. Open Feedback Assistant
2. macOS → "Bug Report"
3. Attach the relevant `.panic` file from this folder
4. Describe what you were doing when the panic happened (e.g. "running an
   OpenGL/Metal application", "navigating Finder", etc.)

SPTM panics in particular get Apple's attention because they touch the
security model.
