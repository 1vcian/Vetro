# Privacy

**Vetro runs entirely in your browser.** The emulated phone, the apps you
install, the analysis of what they do, your recordings and your saved data
never leave your computer. There is no account, no login and no server
that receives your sessions.

## What your browser downloads

Opening Vetro downloads, like any web page does:

- the web app itself and the emulator (a WebAssembly file), from the site;
- the system image: the ready-made snapshot on the first visit, the boot
  images for a cold boot, and the pieces of the phone's disk the system
  reads. These come from the project's file storage (Cloudflare R2), as
  plain files; each piece is checked against a published SHA-256 before it
  is used.

These are downloads **to** your browser. The requests carry what every web
request carries (your IP address, your browser's name) to the site's host
and to the file storage; Vetro adds no identifiers, cookies or analytics of
its own.

## What stays in your browser

Everything else, in your browser's private storage for the site (OPFS),
which other sites cannot read:

- the saved phone: its memory, its disk changes, the apps you installed and
  their data;
- the pieces of the disk already downloaded;
- your recordings.

See [Snapshots and saved data](snapshots-and-data.md) for how to delete
them.

## The APKs you drop

An APK you drag onto the page is read by the page and handed to the emulated
phone, inside your browser. It is not uploaded anywhere.

## The phone's own network traffic

The emulated phone's network is a sinkhole inside the page: the requests
apps make are answered (or not) by Vetro itself, in your browser, and **do
not reach the internet**. What apps try to send, including any personal
data they collect, stays on your computer where you can inspect it (see
[Network inspector and timeline](network-and-timeline.md)).

If an optional network relay is added in the future, it will be off unless
you turn it on, and these pages will say exactly what it sends.

## Exports

HAR files, pcapng captures and `.vrec` recordings you download are saved on
your computer by your browser. They can contain everything the apps sent
and everything on the phone's screen and memory at that time: share them
with care.

## The source code

The emulator and the web app are published at
[github.com/1vcian/Vetro](https://github.com/1vcian/Vetro), so you can
check all of this yourself.
