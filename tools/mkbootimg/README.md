# AOSP mkbootimg (reference for the tests)

**Unmodified** copy of the tool AOSP uses to build `boot.img`,
`vendor_boot.img` and `init_boot.img`. The tests of the Android image
loader (`crates/vetro-machine/src/android`, `docs/specs/android-boot.md`)
build their images with this script, so the format is not what we
believe it is but what AOSP produces.

- Source: `https://android.googlesource.com/platform/system/tools/mkbootimg`
- Commit: `d2bb0af5ba6d3198a3e99529c97eda1be0b5a093` (2025-03-02, branch `main`)
- License: Apache 2.0 (file headers).

| File | git blob | sha256 |
|---|---|---|
| `mkbootimg.py` | `ec2958179691a434df917cd1b6f196edaa80e31d` | `37d84b3d162e0bc62e36c1f4e1c63c85ea0caa9f29be023eb2f8efe006ad948c` |
| `gki/generate_gki_certificate.py` | `739c61b04a9dbd95cafa5196533e3a472c31f2d9` | `1bb1feec68a13da18d581aa2c631798f86f6bc10b55d587b2dd31446a0f8a203` |

`gki/generate_gki_certificate.py` is needed only because `mkbootimg.py` imports it
(the deprecated GKI 2.0 signature is not used).

Verification: `git hash-object tools/mkbootimg/mkbootimg.py` must give the blob
in the table, equal to the one in the stated commit. To update: download
the two files from the same commit (`?format=TEXT`, base64) and update the
table. Requires `python3` (standard library only).
