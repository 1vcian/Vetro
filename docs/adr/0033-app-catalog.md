# ADR 0033 — App catalog: suggested apps installed from the page

- Status: accepted (M6, 2026-09-27). Builds on ADR 0028 (Android in the
  browser: ADB client, dropped APKs) and ADR 0031 (prebuilt snapshot). Plan:
  `docs/PLAN.md`, "Suggested apps"; measurements: `docs/progress/M6.md`.

## Context
The owner wants a few apps offered to every user (Jenny, Chromium, a couple
of open-source games) without baking them into the image: one small base
snapshot for everyone, apps added on request. The page can already install a
dropped APK through its own ADB client and the Worker saves a snapshot after
an install. The origins (GitHub releases, F-Droid, Google Cloud Storage) send
no CORS headers, so the page can't download from them directly.

## Decision
1. **Catalog on R2.** `catalog/v1.json` in the public bucket (the same one as
   the images, whose CORS already allows the site's origins), with the APKs
   and icons next to it: `catalog/apks/<id>/<package>-<code>.apk` and
   `catalog/icons/<id>-<code>.<ext>`, immutable; the JSON is `no-cache` and
   its URLs are relative, so the catalog can move to `assets.vetro.lol`
   unchanged. Format 1, one object per app: `id`, `name`, `package`,
   `version`, `versionCode`, `apk`, `size`, `sha256`, `license` (SPDX),
   `source`, `icon`, `description`, `minImage`, `advanced`, `origin`.
   Unknown fields are ignored; a bad entry is dropped (and named in the
   console) without hiding the others; another `format` hides the panel, so
   an incompatible change means `v2.json`, not an edit of `v1.json`.
2. **Pinned and verified twice.** The tool (`tools/catalog/add.mjs`) fetches
   each APK from its official origin and checks it against what that origin
   publishes (GitHub's asset digest, F-Droid's index reached from
   `entry.json`, the MD5 Google Cloud Storage keeps for Chromium's snapshot
   zip), reads package, version, SDK levels, ABIs and icon with
   `web/node/apk.mjs` (a small `resources.arsc` reader resolves the icon to
   its densest PNG/WebP, recognised by content because shrunk APKs drop the
   extensions; an adaptive icon with a raster foreground, like Jenny's and
   Chromium's, becomes an SVG holding the two layers, cropped to the visible
   72 dp and rounded; vector-only icons get none unless `--icon` is given),
   refuses what the image can't run (no arm64 code,
   minSdk above 35, targetSdk below 24), uploads, and writes the size and
   SHA-256 into the catalog. The page downloads from R2, stops if the body is
   longer than `size`, and installs only bytes whose SHA-256 matches, and
   whose manifest names the catalog's package.
3. **Same install path as drag and drop.** The page hands the verified APK to
   the Worker's `install` request with `open: false`; the push reports
   progress (the ADB client drains every 1 MiB), then `pm install -r`. The
   card then offers **Open**: a new `open` request resolves the launcher
   activity and runs `am start`. The Worker saves the snapshot after the
   install, as for a dropped APK, so installed apps persist with the user's
   local snapshot in OPFS (for Android the snapshot is also the disk
   overlay, ADR 0028). Nothing is preinstalled.
4. **States come from the device.** A card is `absent`, `downloading`,
   `installing`, `installed` or `failed` (a pure `nextState` in
   `web/node/catalog.mjs`, unit tested); whenever adb (re)connects, also
   after a snapshot restore, `pm list packages --show-versioncode` says what
   is installed, and an older version code turns the button into Update.
5. **Minimum image version by AOSP release.** Image versions are
   `android-<x.y.z>_r<n>-<build>-<commit>`; only the release part is
   ordered, so `minImage` is a release (`android-15.0.0_r36`) and entries
   above the running image's release are hidden. An image version that is not
   a release (a local build) accepts every entry: pm has the last word.
6. **Advanced apps apart.** Large or heavy apps (`advanced: true`, e.g.
   Chromium, 364 MiB) sit in a closed "Advanced apps" section.
7. **The panel is optional.** If the catalog can't be fetched or read, the
   panel stays hidden; drag and drop is unaffected. `&catalog=URL` points the
   page to another catalog (tests, staging).

## Where a Free-plan limit would hook in (not enforced)
The plan (`docs/PLAN.md`, "Plans") gives Free the basic catalog and Pro the
curated analysis sets. No limit exists today. When one is needed:
- the catalog entry gets a field such as `plan: "pro"` (ignored by format-1
  pages, which keeps them working);
- the check goes in `CatalogPanel.install` (`web/app/catalog.mjs`), before
  the download starts, asking the account state from the future API
  (`api.vetro.lol`), and the card shows why the button is disabled;
- a client check is advisory (the code runs in the user's browser); a real
  limit needs Pro APKs served from a signed, expiring URL issued by the API
  instead of the public bucket.

## Consequences
- Adding or updating an app is one command and needs no site or snapshot
  change; the entry records where the APK came from (`origin`).
- The APK sits in page memory while downloading and is transferred to the
  Worker: Chromium's 364 MiB is the practical ceiling for now (streaming it
  through OPFS would lift it).
- AGPL/GPL apps are redistributed unmodified with a link to the exact
  source (the release tag, or F-Droid's source tarball of that version),
  shown on the card.
- Chromium comes from the snapshot builds at the stable branch point, not
  from the stable branch itself (no official stable arm64 APK of Chromium
  exists); its card says so. Name: Chromium, never Chrome.
