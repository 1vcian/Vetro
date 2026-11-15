# Privacy

**Vetro runs entirely in your browser.** The emulated phone, the apps you
install, the analysis of what they do, your recordings and your saved data
never leave your computer. There is no account, no login and no server
that receives your sessions. Only the phone's own internet traffic goes out,
through Vetro's network gateway (below).

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

The emulated phone is online: like a real phone, its apps talk to their
servers. Because a browser page cannot open network connections itself,
that traffic goes through **Vetro's network gateway**, a small server we
operate, which opens the connections for the phone. What it does and does
not do:

- it carries only web traffic (HTTP and HTTPS, ports 80 and 443) and name
  lookups, and refuses private and local addresses;
- HTTPS stays encrypted from the app to the real server: the gateway sees
  only encrypted bytes, never what an app sends or receives over HTTPS;
- it keeps no log of contents, sites or addresses: only counters (how many
  connections and bytes, how long), and each session is short-lived and
  limited in connections, speed and total data;
- it does not see your files, the phone's screen, the analysis or your
  recordings: those never leave your browser.

The sites the phone contacts see the gateway's address, not yours. What
apps try to send, including any personal data they collect, is still
visible to you first, in the **Network** tab (see
[Network inspector and timeline](network-and-timeline.md)).

To keep the phone completely offline, add `?net=offline` to the app's
address: then its network is a sinkhole inside the page and nothing it
sends reaches the internet.

## Exports

HAR files, pcapng captures and `.vrec` recordings you download are saved on
your computer by your browser. They can contain everything the apps sent
and everything on the phone's screen and memory at that time: share them
with care.

## The source code

The emulator and the web app are published at
[github.com/1vcian/Vetro](https://github.com/1vcian/Vetro), so you can
check all of this yourself.
