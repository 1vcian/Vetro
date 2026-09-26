# ADR 0004 — PolyForm Noncommercial 1.0.0 license

- Status: accepted (2026-09-24). Replaces the plan's Apache-2.0 choice.

## Context
Vetro's code must not be usable commercially by third parties.
Apache-2.0 (foreseen by the plan) allows it.

## Decision
Our code is released under **PolyForm Noncommercial 1.0.0**
(`LICENSE.md`, SPDX `PolyForm-Noncommercial-1.0.0`). The `NOTICE` file
contains the `Required Notice:` line that the license requires to be propagated.
Commercial use requires a separate written agreement with the author.

## Consequences
- Vetro is *source-available*, not open source under the OSI definition:
  it must be described that way in the README and in announcements.
- External contributions: to be able to grant commercial licenses in the future,
  a CLA or an inbound license clause in PRs is needed. To be defined
  before accepting the first external PR.
- Third-party components keep their license: Linux kernel GPL-2.0
  (sources published with every image), AOSP and microG Apache-2.0. The
  guest images are distributed as an aggregate, with their respective licenses.
