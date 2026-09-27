# Product track — progress log

## 2026-09-27 — B1: public repository preparation (ADR 0034)
- **Done:** ADR 0034 (licence of the public repository, AGPL-3.0 vs
  Apache-2.0, open/closed boundary), **proposed, decision by the owner**
  (proposal AGPL-3.0-only + CLA). `tools/export/public.sh` + allow-list
  `tools/export/allow.txt` (public / transitional / private rules, deny
  patterns; official licence texts in `tools/export/licenses`, checked by
  sha256): clean tree without history, LICENSE/NOTICE/README/Cargo.toml/guest
  LICENSE files switched, SPDX header on 324 of our files, coupling report.
  On the build VM (`tools/remote/test.sh -- tools/export/public.sh --license
  AGPL-3.0-only --check --force ../vetro-public-b1`): 492 files exported, fmt
  ok, `cargo build --workspace --all-targets` ok, wasm build ok, web unit
  tests 25 ok, `cargo test --workspace` 675 passed, 0 SKIP, **14 failed**:
  all in `vetro-linux-tests --test busybox`, and the same 14 fail in the
  unmodified tree on the VM (`tools/remote/test.sh -- cargo test -p
  vetro-linux-tests --test busybox`): QEMU's side runs busybox applet
  symlinks through the host shell ("syntax error: unexpected ')'"), because
  the VM has no binfmt_misc entry for qemu-aarch64 (only python3.10 in
  `/proc/sys/fs/binfmt_misc`), which CI's `qemu-user` package provides. Not
  caused by the export. `--strict` fails as expected with 17 links public ->
  private (4 path dependencies on `vetro-analysis`, 13 file-level), plan in
  ADR 0034. `tools/aosp/common.sh` no longer has the VM's address built in
  (reads `VETRO_AOSP_HOST` or `target/aosp/vm-host`, also from the main
  checkout of a worktree). Drafts in `docs/legal/`: CLA, acceptable use
  policy, trademark note.
- **Missing:** the owner's decision on ADR 0034 (licence, the four open
  points); the plugin boundary ADR; the coupling cut (B3, before the first
  real export); a lawyer's review of the legal drafts and the trademark
  clearance search; the public README rewrite (48 exported files mention
  private paths).
- **Blocked:** a fully green `--check` on the VM needs binfmt_misc for
  qemu-aarch64 there (`sudo apt-get install qemu-user-binfmt` or
  equivalent for the VM's QEMU 10 build): the owner's call.
