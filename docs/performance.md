# CPU investigation

## RAM target and measurement

Nova's release build is kept below a 150 MB private working-set target in its
normal background state and while Settings is open. Private working set is the
closest programmatic counter to the per-process **Memory** value shown by Task
Manager. Total working set is also reported during profiling, but includes
shared Windows, OpenGL and GPU-driver pages that are not private to Nova.

The Settings GUI now avoids two permanent duplicates:

- eframe is built only with the Windows-relevant Glow renderer and
  accessibility support, without its unused default font and WGPU dependency
  trees;
- egui no longer reads and parses the complete 12.9 MB Segoe UI Emoji font or
  the legacy Plus Jakarta Sans UI face. The native notch still uses DirectWrite
  and therefore retains Windows system-font fallback for lyrics, notifications
  and user content.

Measured on the Windows development machine on 2026-10-30 using an optimized
release build and the current saved configuration:

| State | Private working set | Private commit | Total working set |
|:---|---:|---:|---:|
| Tray/notch idle, 20-second sample | 75.2–84.7 MB | 89.5–101.7 MB | 123.9–134.7 MB |
| Settings open and settled | 108.5 MB | 133.6 MB | 182.2 MB |

The Settings total includes shared graphics-driver pages; its private working
set and private commit remain below the 150 MB target. Results vary by GPU
driver, Windows version, enabled slides, artwork and wallpaper size, so the
repository includes a repeatable check rather than treating one machine's
number as universal:

```powershell
cargo build --release --locked
.\target\release\nova.exe --startup
.\scripts\measure-memory.ps1 -DurationSeconds 30 -LimitMB 150
```

Open Settings and run the last command again to measure the GUI state. Fully
exit Nova from its tray menu before switching binaries.

The performance change in PR #1 is present in main and v0.4.1. It gates notch
repaints, but three paths still cause unnecessary work:

- `NotchState::tick` advanced `marquee_offset` whenever the panel was open and
  the deck contained Moving text, including when another slide was visible.
  It also advanced the offset with marquee scrolling disabled. The offset is
  part of `FrameKey`, so these invisible changes caused a full notch repaint
  every tick, bypassing the gate. Visibility now matches the painter, including
  carousel transitions, and static marquees do not advance.
- The overlay loop rendered immediately after every message wake. Its wait
  timeout did not limit rendering when messages arrived early. The loop now
  pumps messages immediately but waits for a frame deadline before ticking
  the renderers. Input traffic cannot increase the intended render cadence.
- Scrolling edge strips still created a text format, shaped a probe, and shaped
  at least 50 repeated phrases per frame, per edge. Text layouts are now cached
  by text, font, spacing, dimensions and edge, and repetitions cover only the
  visible extent plus two copies. Color and speed changes reuse the layout;
  text/font/spacing/geometry changes rebuild it. Zero-speed strips repaint on
  changes and the two-second layout heartbeat instead of every frame.

These fixes do not remove the work needed for visible animation. Moving text,
notification glows and FlashScreen still animate. The notch's ambient pulse and
glass backdrop still repaint at roughly 10 fps, and each such frame still does
Direct2D drawing and a layered-window blit. A collapsed pulse-free slide used to
repaint on the one-second clock key even when it showed no clock; the clock key
now only counts while a Clock slide is visible (see below).
Those are further optimization candidates if profiling shows they dominate.

## Follow-up: brushes, clock key, GPU counter

- Every fill and stroke used to create its own `ID2D1SolidColorBrush`, a few
  hundred COM objects per painted frame. The painter now keeps one per render
  target and recolours it between draw calls.
- `FrameKey::clock` is zero unless a Clock slide is on screen, so a collapsed
  Status, Media or Notifications pill no longer repaints every second. The
  Usage slide had been relying on that repaint to show `/usage` posts, so it
  now has its own revision counter in the key.
- The `GPU Engine(*)` PDH wildcard is collected every other second instead of
  every second, into a reused buffer, without copying each instance name.
  CPU and RAM still sample every second.

Not measured yet; use the procedure below.

## Validation

Five tests exercise the actual state implementation: hidden slides, static
marquees, visible marquees in both sizes, carousel entry, and reverse scrolling.
The hidden-slide and static-marquee tests fail against the original main code;
all five pass with the fix. CI now runs `cargo test --locked` on Windows.

Development validation ran those state tests in a small Linux harness importing
the repository's `config.rs`, `anim.rs` and `state.rs`. All Windows Rust source,
including tests, passed `cargo check --locked --tests --target
x86_64-pc-windows-msvc` using a temporary manifest with resource embedding
excluded: the Linux environment lacks `llvm-rc`. Formatting and patch whitespace
checks passed. Windows linking, visual behavior and CPU usage have not been
measured here; no CPU percentage improvement is claimed.

## Measure on Windows

Build main and this branch with `cargo build --release --locked`. Fully exit Nova
using its tray menu before switching binaries; closing Settings only hides it,
and the single-instance mutex otherwise leaves the old executable running.
Keep the same saved configuration and use the same machine for both builds.

For each build, allow 10 seconds to settle and measure these cases separately:

1. Settings closed, notch collapsed on Status, no edge strips, no active toast
   or flash, pointer away from the notch.
2. The same, with Status pinned open and Moving text still in the deck.
3. Moving text active with its scrolling switch off, then with scrolling on.
4. One edge strip enabled, then all four, then speed set to zero.
5. Repeat with Frosted/Blurred/Acrylic, and with mouse movement over the notch.
6. Open Settings separately to distinguish its rendering cost.

This PowerShell sample reports average CPU over 30 seconds, normalized across
logical processors like Task Manager:

```powershell
$novaProcess = @(Get-Process -Name nova -ErrorAction Stop)
if ($novaProcess.Count -ne 1) { throw "Expected exactly one Nova process" }
$novaProcess = $novaProcess[0]
$logicalProcessors = (Get-CimInstance Win32_ComputerSystem).NumberOfLogicalProcessors
$cpuStart = $novaProcess.TotalProcessorTime.TotalSeconds
$timer = [Diagnostics.Stopwatch]::StartNew()
Start-Sleep -Seconds 30
$novaProcess.Refresh()
$elapsed = $timer.Elapsed.TotalSeconds
$cpuDelta = $novaProcess.TotalProcessorTime.TotalSeconds - $cpuStart
"Average CPU: {0:N2}%" -f (100 * $cpuDelta / $elapsed / $logicalProcessors)
```

Repeat each sample three times. If CPU remains high, record a CPU Usage sampling
trace with Windows Performance Recorder and inspect Nova's stacks in Windows
Performance Analyzer. In particular, distinguish the overlay thread's
DirectWrite/GDI/Direct2D work from the Settings thread's egui/OpenGL work and
the media poller's WinRT calls before choosing the next optimization.

## Split pill, satellite pods and shape settings

These were designed to cost nothing while they are off and as little as
possible while they are on.

- **No new windows.** Pods and split segments are drawn inside the notch's
  existing layered window. The window's layout frame was already sized to the
  open panel, several times wider than the resting pill; it now also reserves
  the pill's split and pod footprint once, in `compute_placement`, so nothing
  is reallocated while they animate. The blit is still trimmed to what is
  drawn (`visible_rect` now takes the pods in).
- **One backdrop capture.** Glass themes capture the desktop once over the
  bounding box of the notch and its pods, and fill every silhouette from that
  bitmap, instead of one capture per slab.
- **Gated like everything else.** Segments and pods are part of `FrameKey`
  (`scene`, `pod_content`). While they are still, no frame is painted for them.
- **Pods never animate on their own.** A timer or agent pod changes once a
  second; a calendar pod once a minute; the media pod only when the track or
  play state changes. None of them reports ambient motion, so a pod can never
  hold the notch at the ~11 fps ambient cadence.
- **Throttled readers.** What pods could show (timer, media, calendar, agent
  activity) is re-read at most twice a second (`POD_POLL`), and not at all with
  pods off; the media title and calendar lookups allocate, so they are kept off
  the per-tick path. The agent pod's frame key reads elapsed seconds without
  cloning anything.
- **Shape settings are free.** Corner radii and shoulders feed the same outline
  the notch always built; outline, sheen and shadow strengths scale the theme's
  palette once per painted frame.
- **Live data only while visible.** A face in a split segment counts as on
  screen for the frame key, so (for example) Stats sampling and the clock key
  run only while that segment is actually showing.

## Controls, Shelf, Clipboard and the live wave

- **Hover costs what it did.** Click/hover opening, the hover delay, the
  fly-by check and the peek grow are arithmetic on the cursor position the
  notch already read every tick. Full-screen state (`SHQueryUserNotificationState`)
  is read at most once a second.
- **The mouse hook stays one compare for moves.** The trigger and the
  outside-click watch only look at button presses; modifier keys are read only
  on a left press while a modifier trigger is set.
- **Shortcuts are `RegisterHotKey`**, delivered as window messages: no keyboard
  hook, nothing per keystroke.
- **The menu is drawn by the notch** inside its existing layered window: no
  extra window, surface or blit. It exists only while open or fading, and
  repaints only while it moves (open, close, the highlight sliding).
- **Hover and press feedback** reuse the targets clicks already hit-test; the
  highlight and ripple are springs in the frame key, so a still panel under a
  still cursor paints nothing.
- **Shelf work happens off the render thread.** One thread lists the peeked
  folder, checks pinned paths and asks the shell for icons, only while the
  Shelf is in the deck. It sleeps on a folder change notification, not a
  timer. Icons are uploaded to Direct2D once and cached per surface.
- **Clipboard history is event-driven.** A message-only window registered with
  `AddClipboardFormatListener` is told about each copy; nothing polls. Off by
  default, and the thread does not exist while off.
- **The live wave is the one new moving thing.** While music plays and a wave
  is on screen, the default output's peak meter (`IAudioMeterInformation`, a
  shared-memory read) is sampled every 40 ms and the bars repaint at about
  20–25 fps. Paused, hidden or switched off (Slides › Now Playing), it costs
  nothing and the meter is released.
- **The player's timeline repaints once a second** while the player is open,
  from a position the media poller anchors; the poll itself does not cause a
  repaint unless the track, state or position (a seek) actually changed.

## Closing keys, right-click and the pill's music controls

- **Esc, the arrows and Enter are `RegisterHotKey`s held only while needed**:
  Esc while the panel or menu is open, the arrows and Enter while the menu
  is. Nothing listens to the keyboard otherwise.
- **Right-click is read by the existing mouse hook** (a button press, never
  a move), so the menu works with click-through on.
- **The menu's glass** reuses the notch's backdrop capture and glass
  painter for the card's rectangle, only while the menu is on screen.
