#!/usr/bin/env bash
# Exports the public part of Vetro (ADR 0034) into a clean tree: no history,
# only the paths that tools/export/allow.txt marks public, the licence switched
# to the one given, then (with --check) builds and tests the exported tree.
#
#   tools/export/public.sh --license SPDX-ID [--strict] [--check] [--force] OUT
#
#   --license   AGPL-3.0-only | AGPL-3.0-or-later | Apache-2.0 (no default:
#               the owner decides, ADR 0034)
#   --strict    the target boundary: transitional paths (~) are left out,
#               unclassified files are an error; stops after the coupling
#               report if a public crate or module still needs a private one
#   --check     in OUT: cargo fmt --check, cargo build --workspace
#               --all-targets, cargo test --workspace --no-fail-fast
#               (VETRO_REQUIRE_ORACLE=1 unless set), the wasm build of CI,
#               node --check of the web modules and the web unit tests
#               (tests/web/unit.mjs); every step runs, failures are listed
#   --force     OUT may exist: it is emptied first (except target/)
#
# What the export does, in order:
#   1. lists the files (git ls-files; without git, e.g. on the build VM after
#      tools/remote/test.sh, every file except target/ and .git);
#   2. classifies each one with the longest matching rule of allow.txt;
#   3. copies the public (and, without --strict, transitional) ones to OUT;
#   4. licence: LICENSE (verbatim text from tools/export/licenses, checked by
#      sha256), a new NOTICE, the workspace `license` field, the README's
#      "License" section, the guest LICENSE files, an SPDX header on our
#      source files (never on third-party files);
#   5. checks: deny patterns (addresses, keys) are errors; references to
#      private paths and leftover "PolyForm" mentions are listed;
#   6. coupling report: path dependencies that leave the export (errors),
#      public crates and files that use transitional ones;
#   7. with --check, the build and the tests above.
# Exit: 0 ok, 1 failed check, 2 usage.
#
# Heavy: run --check on the build VM (the Mac only runs light checks):
#   tools/remote/test.sh -- tools/export/public.sh --license AGPL-3.0-only \
#     --check --force ../vetro-public
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
allow=$here/allow.txt

usage() { sed -n '6,17p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }
license= strict=0 check=0 force=0 out=
while [ $# -gt 0 ]; do
  case $1 in
    --license) [ $# -ge 2 ] || usage; license=$2; shift 2 ;;
    --strict) strict=1; shift ;;
    --check) check=1; shift ;;
    --force) force=1; shift ;;
    -h|--help) usage ;;
    -*) echo "unknown option $1" >&2; usage ;;
    *) [ -z "$out" ] || usage; out=$1; shift ;;
  esac
done
[ -n "$out" ] && [ -n "$license" ] || usage

case $license in
  AGPL-3.0-only|AGPL-3.0-or-later)
    text=$here/licenses/AGPL-3.0.txt
    sum=0d96a4ff68ad6d4b6f1f30f713b18d5184912ba8dd389f86aa7710db079abcb0
    name="GNU Affero General Public License v3.0"
    [ "$license" = AGPL-3.0-only ] && name="$name only" || name="$name or later" ;;
  Apache-2.0)
    text=$here/licenses/Apache-2.0.txt
    sum=cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30
    name="Apache License 2.0" ;;
  *) echo "unsupported licence $license (AGPL-3.0-only, AGPL-3.0-or-later, Apache-2.0)" >&2; exit 2 ;;
esac
sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }
[ "$(sha256 "$text")" = "$sum" ] || { echo "ERROR: $text is not the official text (sha256)" >&2; exit 1; }

mkdir -p "$out"
out=$(cd "$out" && pwd)
case $out/ in "$root"/*) echo "ERROR: OUT must be outside the repository ($root)" >&2; exit 2 ;; esac
if [ -n "$(ls -A "$out")" ]; then
  [ $force = 1 ] || { echo "ERROR: $out is not empty (--force empties it)" >&2; exit 2; }
  # target/ (build cache of a previous --check) survives, so a rerun is incremental.
  find "$out" -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} +
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
errors=0
err() { echo "ERROR: $*"; errors=$((errors + 1)); }
warn() { echo "WARN: $*"; }

# --- 1. Files -----------------------------------------------------------------
if git -C "$root" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  git -C "$root" ls-files > "$work/all"
  source_desc="git ls-files at $(git -C "$root" rev-parse --short HEAD)"
  [ -z "$(git -C "$root" status --porcelain --untracked-files=no)" ] || source_desc="$source_desc (with uncommitted changes)"
else
  (cd "$root" && find . \( -path ./target -o -path ./.git -o -name node_modules -o -path ./.claude/worktrees \) -prune \
     -o -type f ! -name .DS_Store ! -path ./.git -print | sed 's|^\./||' | LC_ALL=C sort) > "$work/all"
  source_desc="all files (no git): $(wc -l < "$work/all" | tr -d ' ') files"
fi
# Tracked but deleted in the working tree: skip.
while IFS= read -r f; do [ -f "$root/$f" ] && printf '%s\n' "$f"; done < "$work/all" > "$work/files"

# --- 2. Classification --------------------------------------------------------
awk -v rules="$allow" -v unused="$work/unused" -v deny="$work/deny" '
BEGIN {
  while ((getline line < rules) > 0) {
    ln++
    if (line ~ /^[ \t]*(#|$)/) continue
    k = substr(line, 1, 1); p = substr(line, 3)
    if (substr(line, 2, 1) != " " || (k != "+" && k != "~" && k != "-" && k != "!")) {
      print "allow.txt:" ln ": bad rule: " line > "/dev/stderr"; bad = 1; continue
    }
    if (k == "!") { print p > deny; continue }
    sub(/[ \t]+$/, "", p); sub(/\/$/, "", p)
    n++; kind[n] = k; path[n] = p; used[n] = 0
  }
  if (bad) exit 2
}
{
  f = $0; best = -1; bk = "?"; bi = 0
  for (i = 1; i <= n; i++) {
    p = path[i]
    if ((f == p || index(f, p "/") == 1) && length(p) > best) { best = length(p); bk = kind[i]; bi = i }
  }
  if (bi) used[bi] = 1
  print bk " " f
}
END { for (i = 1; i <= n; i++) if (!used[i]) print kind[i] " " path[i] > unused }
' "$work/files" > "$work/classes"
touch "$work/unused" "$work/deny"

sel() { grep "^$1 " "$work/classes" | cut -c3- || true; }
sel + > "$work/public"; sel '~' > "$work/transitional"; sel - > "$work/private"; sel '?' > "$work/unclassified"
cp "$work/public" "$work/export"
[ $strict = 1 ] || cat "$work/transitional" >> "$work/export"
LC_ALL=C sort -o "$work/export" "$work/export"

count() { wc -l < "$1" | tr -d ' '; }
echo "==> source: $source_desc"
echo "    public $(count "$work/public"), transitional $(count "$work/transitional") ($([ $strict = 1 ] && echo left out || echo exported)), private $(count "$work/private"), unclassified $(count "$work/unclassified")"
while IFS= read -r f; do
  if [ $strict = 1 ]; then err "unclassified (private by default): $f"; else warn "unclassified (private by default): $f"; fi
done < "$work/unclassified"
while IFS= read -r r; do warn "rule matches no file: $r"; done < "$work/unused"

# --- 3. Copy ------------------------------------------------------------------
(cd "$root" && tar -cf - -T "$work/export") | (cd "$out" && tar -xf -)
echo "==> exported $(count "$work/export") files to $out"

# --- 4. Licence ---------------------------------------------------------------
cp "$text" "$out/LICENSE"
year=$(date -u +%Y)
years=2026; [ "$year" = 2026 ] || years="2026-$year"
cat > "$out/NOTICE" <<EOF
Vetro
Copyright (c) $years Lucian Medrihan (https://github.com/1vcian/Vetro)

This is the community edition of Vetro, licensed under the $name
(SPDX: $license, see LICENSE). It is generated from the maintainer's
repository; contributions are accepted under the Contributor Licence
Agreement in docs/legal/CLA.md.

The licence does not grant permission to use the name "Vetro" or its logo,
except as needed to describe the origin of the software and to reproduce this
NOTICE.

Third-party components keep their own licences. In particular, the Linux
kernel shipped in guest images is GPL-2.0, and its exact sources are published
with every distributed image; AOSP and microG are Apache-2.0.
EOF

perl -pi -e "s/^license = \"PolyForm-Noncommercial-1\\.0\\.0\"/license = \"$license\"/" "$out/Cargo.toml"
grep -q "^license = \"$license\"" "$out/Cargo.toml" || err "workspace licence field not switched in Cargo.toml"

if [ -f "$out/README.md" ]; then
  case $license in
    Apache-2.0) terms="You may use, modify and redistribute it, also commercially, under the terms of the licence." ;;
    *) terms="You may use, modify and redistribute it under the terms of the licence; if you run a modified version as a network service, you must offer its users the corresponding source code." ;;
  esac
  perl -0pi -e 's/\n## License\n.*\z//s' "$out/README.md"
  cat >> "$out/README.md" <<EOF

## License

Vetro's community edition is open source under the
[$name](LICENSE) (SPDX \`$license\`). $terms
See [\`NOTICE\`](NOTICE). Contributions are accepted under the
[Contributor Licence Agreement](docs/legal/CLA.md); use of Vetro is subject to
the [acceptable use policy](docs/legal/acceptable-use.md).

Third-party components keep their own licenses — notably the Linux kernel
(GPL-2.0), whose exact sources are published with every distributed guest
image.
EOF
fi

# Guest configuration shipped inside AOSP builds: LICENSE files next to the
# Android.bp files that reference them (Soong `legacy_notice`).
for f in $(cd "$out" && find guest -name LICENSE -type f 2>/dev/null); do
  if grep -q "PolyForm" "$out/$f"; then
    printf 'These files are part of Vetro and are distributed under the\n%s (SPDX: %s):\nfull text in LICENSE at the root of the Vetro repository.\n' "$name" "$license" > "$out/$f"
  fi
done
for f in $(cd "$out" && find guest -name Android.bp -type f 2>/dev/null); do
  perl -0pi -e "s/PolyForm Noncommercial 1\\.0\\.0/$license/g; s/LICENSE\\.md/LICENSE/g" "$out/$f"
done

# SPDX header on our own source files. Third-party files (with their own
# copyright or SPDX line) and vendored folders are left untouched.
spdx_skip='^(tools/mkbootimg/|guest/aosp/patches/|guest/kernel/reference/)|/testdata/'
headers=0
while IFS= read -r f; do
  case $f in
    *.rs|*.mjs|*.js|*.java|*.kt) c='//' ;;
    *.sh|*.py) c='#' ;;
    *.c|*.h) c='/*' ;;
    *) continue ;;
  esac
  [[ $f =~ $spdx_skip ]] && continue
  p=$out/$f
  [ -s "$p" ] || continue
  awk 'NR > 15 { exit } tolower($0) ~ /copyright|spdx-license-identifier/ { f = 1; exit } END { exit !f }' "$p" && continue
  line="$c SPDX-License-Identifier: $license"
  [ "$c" = '/*' ] && line="$line */"
  L=$line perl -pi -e 'if ($. == 1) { if (/^#!/) { $_ .= "$ENV{L}\n" } else { $_ = "$ENV{L}\n$_" } } close ARGV if eof' "$p"
  headers=$((headers + 1))
done < "$work/export"
echo "==> licence $license: LICENSE, NOTICE, Cargo.toml, README, SPDX header on $headers files"

# --- 5. Content checks --------------------------------------------------------
: > "$work/text"
while IFS= read -r f; do grep -Iq . "$out/$f" 2>/dev/null && printf '%s\n' "$f" >> "$work/text"; done < "$work/export"
while IFS= read -r pat; do
  [ -n "$pat" ] || continue
  (cd "$out" && xargs grep -lE -- "$pat" < "$work/text" || true) > "$work/hits"
  while IFS= read -r f; do err "deny pattern /$pat/ in $f"; done < "$work/hits"
done < "$work/deny"

# References to private paths in exported text (comments, docs, scripts):
# not a build problem, but the public reader can't follow them.
priv_refs=0
for p in $(grep '^- ' "$allow" | cut -c3- | sed 's/[[:space:]]*$//'); do
  case $p in LICENSE.md|NOTICE|.gitignore) continue ;; esac
  hits=$(cd "$out" && xargs grep -lF -- "$p" < "$work/text" || true)
  [ -n "$hits" ] || continue
  n=$(printf '%s\n' "$hits" | wc -l | tr -d ' ')
  priv_refs=$((priv_refs + n))
  warn "private path $p mentioned in $n exported files: $(printf '%s\n' "$hits" | head -n 4 | tr '\n' ' ')$([ "$n" -gt 4 ] && echo ...)"
done
poly=$(cd "$out" && xargs grep -lF PolyForm < "$work/text" || true)
[ -z "$poly" ] || warn "\"PolyForm\" still mentioned (history or text to review): $(printf '%s\n' "$poly" | tr '\n' ' ')"

# --- 6. Coupling --------------------------------------------------------------
# a) path dependencies that point outside the exported tree: the build breaks.
couplings=0
for toml in $(cd "$out" && find . -name Cargo.toml -not -path './target/*' | sed 's|^\./||' | LC_ALL=C sort); do
  dir=$(dirname "$toml")
  { grep -Eo '^[a-zA-Z0-9_-]+ *= *\{[^}]*path *= *"[^"]+"' "$out/$toml" || true; } | while IFS= read -r dep; do
    crate=${dep%% *}
    rel=$(printf '%s' "$dep" | sed -E 's/.*path *= *"([^"]+)".*/\1/')
    target=$(cd "$root/$dir" 2>/dev/null && cd "$rel" 2>/dev/null && pwd || true)
    target=${target#"$root"/}
    if [ ! -f "$out/$dir/$rel/Cargo.toml" ]; then
      echo "BROKEN $dir -> $crate ($target, not exported)"
    elif grep -qx "$target/Cargo.toml" "$work/transitional"; then
      echo "TRANSITIONAL $dir -> $crate ($target)"
    fi
  done
done > "$work/deps"
while IFS= read -r l; do
  case $l in
    BROKEN*) err "path dependency leaves the export: ${l#BROKEN }" ;;
    TRANSITIONAL*) couplings=$((couplings + 1)); warn "public crate depends on a transitional crate: ${l#TRANSITIONAL }" ;;
  esac
done < "$work/deps"

# b) public Rust files that use a transitional crate, and public modules
#    declared on transitional files (`mod tls;` with tls.rs transitional).
trans_crates=$(grep -E '^crates/[^/]+/Cargo.toml$' "$work/transitional" | sed -E 's|crates/([^/]+)/Cargo.toml|\1|' | tr - _ || true)
for c in $trans_crates; do
  users=$(cd "$root" && grep -lF "$c::" $(grep "\.rs$" "$work/public") 2>/dev/null || true)
  for u in $users; do
    couplings=$((couplings + 1))
    warn "public file uses $c: $u ($(cd "$root" && grep -cF "$c::" "$u") references)"
  done
done
while IFS= read -r t; do
  case $t in *.rs) ;; *) continue ;; esac
  mod=$(basename "$t" .rs); dir=$(dirname "$t")
  for parent in "$dir/lib.rs" "$dir/main.rs" "$dir/mod.rs"; do
    grep -qx "$parent" "$work/public" || continue
    if grep -Eq "^[[:space:]]*(pub(\([a-z]+\))? )?mod $mod;" "$root/$parent"; then
      couplings=$((couplings + 1)); warn "public module tree declares transitional $t (in $parent)"
    fi
  done
done < "$work/transitional"

# c) public JavaScript modules that import transitional ones.
while IFS= read -r t; do
  case $t in *.mjs|*.js) ;; *) continue ;; esac
  b=$(basename "$t")
  for u in $(grep -E '\.(mjs|js)$' "$work/public"); do
    if grep -Eq "from ['\"]([^'\"]*/)?$b['\"]" "$root/$u"; then
      couplings=$((couplings + 1)); warn "public module $u imports transitional $t"
    fi
  done
done < "$work/transitional"

echo "==> coupling: $couplings public -> transitional links, $priv_refs files mentioning private paths"
if [ $strict = 1 ] && [ $couplings -gt 0 ]; then
  err "strict export: $couplings links from public code to private code (ADR 0034, \"Coupling\")"
fi
if [ $errors -gt 0 ]; then echo "FAILED: $errors errors"; exit 1; fi

# --- 7. Build and tests of the exported tree ---------------------------------
if [ $check = 1 ]; then
  cd "$out"
  # Guest artifacts the tests use, from this tree or the main checkout.
  main=$(git -C "$root" rev-parse --path-format=absolute --git-common-dir 2>/dev/null | sed 's|/\.git$||' || true)
  for t in guest-kernel guest-bins risu ltp aosp/out; do
    for base in "$root" "$main"; do
      [ -n "$base" ] && [ -d "$base/target/$t" ] || continue
      mkdir -p "target/$(dirname "$t")"; ln -sfn "$base/target/$t" "target/$t"; break
    done
  done
  export VETRO_REQUIRE_ORACLE=${VETRO_REQUIRE_ORACLE:-1}
  log=$work/test.log
  failed=
  step() { echo "==> $*"; "$@" 2>&1 | tee -a "$log" || failed="$failed$*\n"; }
  step cargo fmt --all --check
  step cargo build --workspace --all-targets
  step cargo test --workspace --no-fail-fast
  step cargo build --target wasm32-unknown-unknown --workspace --exclude vetro-cli --exclude vetro-diff --exclude vetro-isa-tests --exclude vetro-linux-tests --exclude vetro-jit-native
  echo "==> node --check of the web modules"
  for f in web/app/*.mjs web/node/*.mjs tools/web-serve.mjs tests/web/*.mjs; do step node --check "$f" >/dev/null; done
  step node tests/web/unit.mjs
  skips=$(grep -c 'SKIP' "$log" || true)
  if [ -n "$failed" ]; then
    echo "FAILED steps in the exported tree:"; printf "$failed" | sed "s/^/  /"
    grep -E "^test .* FAILED$" "$log" | sort -u | sed "s/^/  /" || true
    exit 1
  fi
  echo "==> exported tree OK ($license$([ $strict = 1 ] && echo ', strict')); SKIP lines in the test output: $skips"
  [ "$skips" = 0 ] || grep 'SKIP' "$log" | sort | uniq -c | sort -rn | head -n 20
fi
echo "OK"
