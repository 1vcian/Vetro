# Internet for the phone

By itself the phone in your browser is offline: a web page cannot open
network connections on its own, so the apps have nobody to talk to. A small
program on your own computer, **Vetro Connect**, opens those connections
for the phone. While it runs, apps load and web pages open; when you quit
it, the phone is offline again.

Nothing of ours sits in between: the traffic goes from your browser to
Vetro Connect on the same computer, and from there to the sites, over your
own connection.

## Get Vetro Connect

- **Windows**: `vetro-connect-windows-x64.exe`
- **macOS (Apple silicon)**: `vetro-connect-macos-arm64.zip`, which holds
  the app "Vetro Connect"

Both are attached to the releases named "Vetro Connect" on the project's
[releases page](https://github.com/1vcian/Vetro/releases). The packaged
programs are new: if no such release is there yet, or on another system
(an Intel Mac, Linux), run it from the source code instead. It needs only
Python 3.10 or newer, nothing else to install:

```sh
python3 relay/py/connect.py
```

## Start it

Double-click it. There is nothing to choose:

1. Vetro Connect opens the phone's page in Chrome or Edge (in your default
   browser if neither is installed; Vetro itself needs Chrome or Edge, see
   [Getting started](getting-started.md)), and the phone starts by itself.
2. **Chrome asks whether the site may connect to apps on this device: click
   Allow.** It asks once. This is Chrome making sure you want a web page to
   talk to a program on your computer; without it the page cannot reach
   Vetro Connect and the phone stays offline.
3. A small window stays open. It says "Waiting for the page: allow the
   connection in Chrome if it asks" until the page has reached it, then "The
   phone's internet is on", with counters of connections and data.
   **Open Vetro** opens the phone's page again; **Quit** stops everything.

Keep the window open while you use the phone. Next time, start Vetro
Connect first: it opens the right page for you.

## Windows and macOS warn before opening it

The packaged programs are not signed (signing needs paid developer
accounts), so the system does not know who made them and says so:

- **Windows** shows "Windows protected your PC": click **More info**, then
  **Run anyway**.
- **macOS** says it could not verify the app. Close that message, open
  **System Settings → Privacy & Security**, scroll to the line about Vetro
  Connect and click **Open Anyway**. On macOS 14 and earlier you can
  instead Control-click the app, choose **Open**, and confirm.

You only do this the first time. If you would rather not, run it from the
source code as shown above: it is the same program. Each release also lists
the SHA-256 of its files, to check that a download is the one we built.

## What it does, and what it cannot do

- It listens **only on your own computer** (the address `127.0.0.1`): no
  other device on your network, and nobody on the internet, can use it.
- It answers **only the Vetro site**. Another web page open in your browser
  cannot use it.
- It carries **only web traffic**: connections to ports 80 (HTTP) and 443
  (HTTPS), and name lookups, which it answers from your computer's own DNS
  settings. Other ports and other kinds of traffic are refused, so apps
  fall back to ordinary HTTPS.
- It **refuses private and local addresses**: your router, the other
  devices at home or in the office, and your computer itself cannot be
  reached through it, and it does not tell the phone their addresses. The
  phone gets the public internet and nothing else.
- HTTPS stays encrypted from the app to the real server: Vetro Connect only
  passes encrypted bytes along.
- It keeps **no record** of sites, addresses or contents: only the counters
  you see in its window, which are gone when you quit.
- It installs nothing and leaves nothing running: **Quit** (or closing the
  window) ends it.

The sites the phone contacts see your own internet address, as they do when
you browse yourself. What apps send is still visible to you first, in the
**Network** tab (see [Network inspector and timeline](network-and-timeline.md)).

## If the phone stays offline

- **The window keeps saying "Waiting for the page".** Chrome has not been
  allowed to connect. Look for Chrome's question in the phone's tab and
  click Allow. If you blocked it earlier, open the site's settings from the
  icon at the left of the address bar, allow the connection there, and
  reload the page.
- **You opened the phone's page yourself.** The page uses Vetro Connect only
  when it is opened with its address: use the **Open Vetro** button.
- **You restarted Vetro Connect.** A page left open from before finds it
  again by itself within a few seconds. If it does not (the window shows
  another address than before), use the page it opens.
- **Only some apps fail.** An app that needs something other than web
  traffic cannot reach its server; see the
  [FAQ](faq.md#why-do-apps-say-they-are-offline).

To keep the phone offline on purpose, do not start Vetro Connect, or add
`?net=offline` to the app's address (see
[Network inspector and timeline](network-and-timeline.md#what-the-phones-network-is)).
