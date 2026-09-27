# Vetro user guide

Vetro is a complete arm64 phone system that runs inside a browser tab. It
boots Vetro's own build of the Android Open Source Project (AOSP 15, with
microG instead of Google services) on an emulated ARM64 processor, and it
lets you watch what apps do from the outside: their network traffic, the
files they write, and a recording you can replay instruction by
instruction.

Nothing is installed on your computer and nothing is sent to a server: the
whole machine runs in your browser, and what it saves stays in your
browser's private storage.

> Vetro is an independent project. It is not affiliated with or endorsed by
> Google. Android is a trademark of Google LLC.

## Start here

1. [Getting started](getting-started.md): open the site, what the first
   visit downloads, and why later visits are fast.
2. [Using the phone](using-the-phone.md): touch, keyboard, the power
   button, installing an APK by drag and drop, and the adb line.

## Looking inside

- [Network inspector and timeline](network-and-timeline.md): the requests
  apps make, and which of your actions caused them.
- [File manager](file-manager.md): browse, view and edit the files in the
  guest while it runs.
- [Record and replay](record-and-replay.md): record a session, replay it
  exactly, and jump back to any instruction.

## Your data and your machine

- [Snapshots and saved data](snapshots-and-data.md): what is saved, when,
  and how to start over.
- [Device profiles](device-profiles.md): phone, small phone or tablet:
  screen size, density, memory and device name.
- [Privacy](privacy.md): what stays in your browser (everything) and the
  few things that are downloaded.

## When something goes wrong

- [Troubleshooting](troubleshooting.md): memory, browser support, slow
  starts, clearing data.
- [FAQ](faq.md): short answers to common questions.

## Status

Vetro is under active development. The phone system, drag and drop
installation, the network inspector, the file manager and record and
replay all work in the browser today; some pieces are still being finished
and are marked as such in these pages. The project plan and its progress
are in the repository: [github.com/1vcian/Vetro](https://github.com/1vcian/Vetro).
