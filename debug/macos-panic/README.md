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
| `panic-full-2026-05-14-004542.0002.panic` | `fileproviderd` / `AppleARMWatchdogTimer` | Watchdog timeout (92 s). The user reports the same crash signature reproduces with other games too — see "Updated hypothesis" below. |

None of the panic backtraces contain `wgpu`, `Metal`, `AGX`, `IOSurface`, or
any other graphics-stack driver. The FFOIE process is not on any of them.

## Updated hypothesis — macOS + games, not project-specific

The same crash signature reproduces with other games on this machine, not
just FFOIE. That rules out the project layout (iCloud Drive, our renderer,
our input handling, etc.) as the cause.

What unifies all five panics is **the watchdog timeout itself**. The kernel
panics not because of the named daemon's own bug, but because no daemon
could check in for 90+ seconds — i.e. the scheduler / interrupt path was
globally stalled. Whichever daemon happens to be due for a check-in when
the panic fires gets named in the log; the changing daemon name across
panics (audio / accessibility / SPTM / fileproviderd) is consistent with
that. They're victims, not perpetrators.

On Apple Silicon with games specifically, the usual suspects for this kind
of global kernel stall are:

1. **AGX (Apple GPU) driver hang** under sustained Metal command submission
   or specific blit patterns. The driver may hang *before* it would have
   appeared on a backtrace, so AGX doesn't always show up by name.
2. **Pointer-lock / HID overload** — cursor grabbing combined with high
   relative-motion event rates.
3. **Power-state transitions** under sustained CPU+GPU load (small enclosure
   thermal/power management, e.g. Mac mini).
4. **macOS 26.4.1 specifically** — recent Apple Silicon kernels have had
   more SPTM and scheduler bugs than older ones.

Recommended next steps for the user (not FFOIE-side):

- **Software Update**: ensure the latest macOS point release is installed.
  Many SPTM/scheduler bugs in 26.x have been fixed incrementally.
- **Apple Feedback Assistant**: file *one report* with all five `.panic`
  files attached and a note that the same signature reproduces with
  multiple games. Pattern-of-five carries more weight than isolated reports.
- **AppleCare diagnostics**: schedule a service appointment to rule out
  hardware (the May 13 14:39 SoC panic in particular is hardware-level).
- **Sanity check**: try a clean test-user account, or boot in safe mode and
  run a game. If the panic only happens in the normal account, suspect a
  third-party kext or login item.

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
