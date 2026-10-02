# Troubleshooting

The progress line at the top of the page, next to "Vetro", always says
what the machine is doing, and shows errors. The console, under **Tools**,
shows the system's own log; the full setup form (`app/?advanced=1`) also
shows a detailed status line. Start from those.

## Browser support

| Browser | Status |
|---|---|
| Google Chrome, desktop | supported |
| Microsoft Edge, desktop | supported |
| Other Chromium-based browsers (Brave, Opera, Vivaldi) | usually work; not tested |
| Firefox | not supported yet |
| Safari, and any browser on iPhone or iPad | not supported |
| Browsers on Android phones and tablets | not supported |

Use a recent version: Vetro needs WebAssembly with SIMD, Web Workers and
the Origin Private File System, all present in current Chrome and Edge.
When something is missing the page shows a notice above the screen: **This
browser cannot run Vetro** (the phone is not started) or **This browser may
not run Vetro well** (another browser, a phone or tablet, or less memory
than the phone needs: the phone starts anyway).

Private (incognito) windows are not recommended: browsers give them much
less storage, which may not fit the saved phone, and they forget everything
when closed, so every visit downloads the ready-made snapshot again.

## Memory

The phone has 2 GiB of RAM of its own (1.5 GiB with the Small phone
profile), and the tab needs close to 3 GiB in total while it runs.

- **"Aw, Snap!" or "Out of memory"**: the browser stopped the tab. Close
  other tabs and programs, then reload. A computer with 8 GB of memory is
  the practical minimum; 16 GB is comfortable.
- The browser keeps a single tab below 4 GiB of WebAssembly memory, so the
  phone's RAM cannot go above 3 GiB.
- Run only one Vetro tab at a time: two phones need twice the memory, and
  they would share (and fight over) the same saved data.

## Storage

The saved phone takes about 1 GB, and the disk pieces grow as you use it.

- **"QuotaExceededError"** or a save that fails: the browser refused more
  storage. Free some disk space. Chrome gives a site a share of the free
  space on the disk, so a nearly full disk leaves Vetro very little.
- To reclaim the space, see
  [Starting over](snapshots-and-data.md#starting-over).

## The first start is slow, or stops

- The download of the ready-made snapshot shows its progress on the line
  at the top. If it stops ("The download stopped"), reload the page: it
  resumes where it stopped, and pieces already checked are not downloaded
  again.
- **"No ready-made snapshot for this version"** means there is none for
  this exact version of Vetro, system image and device profile, so the phone
  boots from scratch (about 45 minutes). This happens right after a new
  version is published, before its snapshot is uploaded, and always with a
  profile other than the default. Leaving it to finish is fine: the next
  start is fast.
- A cold boot shows each phase under **Tools**. Long pauses in
  "system_server" and before the home screen are normal on an emulated
  processor.

## The phone is slow

Vetro emulates an ARM processor instruction by instruction, in WebAssembly;
it is much slower than a real phone, and a single emulated processor does
all the work.

- The JIT is always on in the normal page (only the developer form,
  `?advanced=1`, can turn it off): it translates the phone's code into
  WebAssembly and is several times faster.
- Keep the tab in the foreground: browsers slow down background tabs.
- The first time an app opens, its files are still arriving: the second
  time is faster.
- Smaller screens are cheaper to draw: the default **Light** profile draws
  960 x 600 (see [Device profiles](device-profiles.md)).
- Window animations are off and app animations run at half length by
  default; `?graphics=full` in the address (or **full animations and
  blurs** in the setup form, `?advanced=1`) brings the system's own back,
  at the cost of more frames to draw.

## The screen stays black

- During a cold boot the screen stays off until the graphics start; the
  boot phases under **Tools** say which phase the system is in.
- After a restore the screen may show "Phone is starting…" for a while:
  the system is finishing its start-up.
- Click the screen and press a key: if the display went to sleep, it wakes
  up. The **Power** button turns it on too.

## Keys or clicks do nothing

- A white ripple shows where you touched: the press arrived. The phone can
  take a few seconds to redraw, especially right after a start, while the
  system is still busy.
- While the machine saves its state (a note at the bottom of the screen
  says so) the phone is stopped; your touch is handled when it is done. The
  automatic save after an install waits until you stop touching the screen
  for a few seconds.
- Click the screen first, so it has the keyboard (a coloured outline shows
  it).
- During a replay, your inputs are ignored on purpose: wait for the replay
  to end.
- Characters come out wrong: the phone uses a US English keyboard layout and
  reads the position of the keys, not the characters of your layout (see
  [Using the phone](using-the-phone.md#keyboard)).

## Installing an APK fails

The error from the installer is shown under the drop box. The usual ones:

- `INSTALL_FAILED_NO_MATCHING_ABIS`: the app has only 32-bit native code;
  the phone is 64-bit only.
- `INSTALL_FAILED_OLDER_SDK`: the app needs a newer system than Android 15
  (API 35).
- **waiting for adbd**: the system has not finished starting; the install
  begins by itself when adb connects.

## Clearing data

- **Start over with the saved phone**: **Delete saved data** under
  **Tools**, then reload ([details](snapshots-and-data.md#starting-over)).
- **Remove everything**: clear the site's data in the browser settings.
- If the page itself seems stuck after an update, reload it while holding
  Shift, so the browser fetches the new version of the app.

## Reporting a problem

Open an issue on [GitHub](https://github.com/1vcian/Vetro/issues) with the
browser and its version, your computer's memory, the device profile, the
text of the progress line (and of the notice, if there is one), and the
last lines of the console.
