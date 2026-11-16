#!/usr/bin/env python3
"""Vetro Connect: internet for the Vetro phone, through this computer.

    python3 relay/py/connect.py

Starts the network gateway on 127.0.0.1 and opens the phone in the browser
(ADR 0050). Python 3.10 or newer, standard library only. `--help` lists the
options.
"""

import sys

if sys.version_info < (3, 10):
    sys.exit("Vetro Connect needs Python 3.10 or newer (this is %d.%d)." % sys.version_info[:2])

from vetro_connect.app import main  # noqa: E402 - after the version check

if __name__ == "__main__":
    sys.exit(main())
