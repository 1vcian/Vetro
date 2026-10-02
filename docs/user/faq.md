# FAQ

## Is this a real phone system?

Yes. Vetro runs an unmodified build of the Android Open Source Project 15
(API 35), compiled for 64-bit ARM, on an ARM processor that Vetro emulates
itself. Apps run their own code, instruction by instruction, as they would
on a device.

## Is it Android?

It is built from the Android Open Source Project, but Vetro is an
independent project, not affiliated with or endorsed by Google. It has no
Google apps and no Google Play Services; it ships microG, an open-source
replacement for the basics many apps expect.

## Can I install apps from Google Play?

Not from inside Vetro: there is no Play Store. You can install any APK file
you have by dragging it onto the page. Apps that require Play Integrity (or
the original Play Services) will not work.

## Why is it slower than my phone?

Every instruction of the emulated processor is translated and executed by
your browser, in WebAssembly, on one core. Vetro favours doing exactly what
a real processor does over speed. Snapshots avoid the slowest part (the
boot), and the JIT keeps the rest usable. See
[Troubleshooting](troubleshooting.md#the-phone-is-slow).

## Why do apps say they are offline?

The phone's network is a sinkhole: it looks real to the phone but does not
reach the internet, so apps never talk to their servers. This is what keeps
everything inside your browser. See
[Network inspector and timeline](network-and-timeline.md#what-the-phones-network-is).

## Can apps tell they are running in Vetro?

The analysis happens in the emulator, outside the phone, so nothing is
injected into the apps. The system itself does not pretend to be a
particular commercial phone, though: its model name is "Vetro", and an app
that looks for signs of an emulator can find some.

## Does anything I do get uploaded?

No. See [Privacy](privacy.md).

## Where are my apps and files kept?

In your browser's private storage for the site, as part of the saved phone.
See [Snapshots and saved data](snapshots-and-data.md).

## I closed the tab: did I lose my work?

You keep everything up to the last snapshot. Vetro saves at the home screen
and after each app install; **Save state**, under **Tools**, saves at any
other moment.

## Can I use it on my phone or tablet?

Not yet: Vetro needs a desktop version of Chrome or Edge and several GB of
memory.

## Can I run two phones at once?

Not in the same browser profile: they would share the same saved data. Each
[device profile](device-profiles.md) keeps its own saved phone, so you can
switch between them.

## How do I start again from a clean phone?

Open **Tools** under the screen and click **Delete saved data** (twice, to
confirm), then reload the page. See
[Starting over](snapshots-and-data.md#starting-over).

## Can I use Vetro for commercial work?

Vetro's code is under the PolyForm Noncommercial License 1.0.0: personal,
research and other noncommercial use is free; commercial use needs a
separate licence from the author. The system image contains open-source
components under their own licences (the Linux kernel's sources are
published with every image).

## How do I report a bug?

On [GitHub](https://github.com/1vcian/Vetro/issues). The
[troubleshooting page](troubleshooting.md#reporting-a-problem) lists what to
include.
