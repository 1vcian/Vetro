# Snapshots and saved data

## What a snapshot is

A snapshot is the whole machine frozen at one moment: the memory, the
processor, every device, and all the changes the phone has made to its disk.
Restoring it brings the phone back exactly as it was, running apps
included, without booting again.

## When Vetro saves

With the phone, Vetro saves a snapshot:

- about five seconds (of phone time) after the home screen first appears;
- after you install an app;
- whenever you click **Save state** under the screen.

The line next to the buttons says when the last snapshot was saved, how big
it is and how long it took.

**The next visit resumes from the last snapshot.** Anything you did after
it (a setting you changed, a file an app wrote) is not in it. Before closing
the tab, click **Save state** if you want to keep your latest changes.

## Where it is kept

Everything is kept in your browser's private storage for the site (the
Origin Private File System, OPFS). It is not visible as normal files on your
computer, and it is not sent anywhere.

| What | Why | Size |
|---|---|---|
| Snapshots | resume the phone as you left it | about 1 GB for the phone, more as you install apps |
| Disk pieces | the parts of the phone's disk already read, so they are not downloaded again | grows as you use the phone, up to a few GB |
| Recordings | keyframes and logs of [record and replay](record-and-replay.md) | depends on the recording |
| Boot images | only after a cold boot | about 136 MiB |

Each [device profile](device-profiles.md) has its own snapshot: the phone
you set up with the Phone profile and the one with the Tablet profile are
two separate machines, and switching between them does not lose either.
The same is true for a new version of the system image or of Vetro that
changes the machine: the old snapshot is not used, and the first start
downloads (or boots) a fresh one.

## Starting over

To throw away the saved phone and start from the ready-made snapshot again:

1. Reload the page, so the machine is not running.
2. Click **Delete saved data** in the setup form (under **Start**).

It deletes the snapshots, the saved disk changes, the disk pieces and the
recordings. The next start is a first visit again: it downloads the
ready-made snapshot.

To remove absolutely everything Vetro stored, including the boot images of
a cold boot, clear the site's data in the browser: in Chrome, click the
icon to the left of the address, then **Site settings** and
**Delete data**, or go to `chrome://settings/content/all` and remove the
site.

## Options in the setup form

These are on by default and are best left on:

- **OPFS cache**: keep the disk pieces already read;
- **persistent disks**: keep the disk changes (with the phone they are part
  of the snapshot);
- **cached snapshot**: download the ready-made snapshot, save snapshots, and
  resume from them.

With **cached snapshot** off (or `snapshot=0` in the address) nothing is
saved and nothing is resumed: the phone does a cold boot of about 45
minutes. It is meant for development, not for everyday use.
