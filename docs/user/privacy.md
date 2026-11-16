# Privacy

**Vetro runs entirely in your browser.** The emulated phone, the apps you
install, the analysis of what they do, your recordings and your saved data
never leave your computer. There is no account, no login and no server
that receives your sessions. Only the phone's own internet traffic goes out,
and only while you run Vetro Connect (below).

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

By itself the emulated phone is offline: its network is a sinkhole inside
the page and nothing it sends reaches the internet. It goes online, like a
real phone whose apps talk to their servers, only while you run **Vetro
Connect** ([Internet for the phone](internet.md)): a small program on your
own computer, not a server of ours, which opens the connections a browser
page cannot open. What it does and does not do:

- it listens only on your own computer and answers only the Vetro site;
- it carries only web traffic (HTTP and HTTPS, ports 80 and 443) and name
  lookups, and refuses private and local addresses, so the phone cannot
  reach your router or the other devices of your network;
- HTTPS stays encrypted from the app to the real server: Vetro Connect sees
  only encrypted bytes, never what an app sends or receives over HTTPS;
- it keeps no log of contents, sites or addresses: only counters (how many
  connections and bytes), shown in its window and gone when you quit;
- it does not see your files, the phone's screen, the analysis or your
  recordings: those never leave your browser.

The sites the phone contacts see your own internet address, as they do when
you browse yourself, and name lookups go to the DNS your computer already
uses. What apps try to send, including any personal data they collect, is
still visible to you first, in the **Network** tab (see
[Network inspector and timeline](network-and-timeline.md)).

To keep the phone completely offline, do not start Vetro Connect, or add
`?net=offline` to the app's address.

## Exports

HAR files, pcapng captures and `.vrec` recordings you download are saved on
your computer by your browser. They can contain everything the apps sent
and everything on the phone's screen and memory at that time: share them
with care.

## The source code

The emulator and the web app are published at
[github.com/1vcian/Vetro](https://github.com/1vcian/Vetro), so you can
check all of this yourself.
