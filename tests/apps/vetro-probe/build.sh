#!/bin/sh
# Builds vetro-probe.apk with the Android SDK tools (aapt2, d8,
# apksigner), without Gradle. Needs ANDROID_HOME (or ~/Library/Android/sdk),
# build-tools (BT, default 35.0.0) and a platform (PLATFORM, default the
# highest installed android.jar). Java 17+.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
sdk="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
bt="$sdk/build-tools/${BT:-35.0.0}"
jar="${PLATFORM:-$(ls -d "$sdk"/platforms/android-* | sort -V | tail -1)/android.jar}"
out="$here/out"
rm -rf "$out"
mkdir -p "$out/gen" "$out/classes"
# Resources (none beyond the manifest): compile and link the base APK.
"$bt/aapt2" link -o "$out/base.apk" -I "$jar" \
  --manifest "$here/AndroidManifest.xml" --java "$out/gen" --min-sdk-version 29 --target-sdk-version 34
# Compile the Java and convert to dex.
javac -source 17 -target 17 -cp "$jar" -d "$out/classes" \
  "$here"/src/com/vetro/probe/*.java $(find "$out/gen" -name '*.java')
"$bt/d8" --min-api 29 --output "$out" $(find "$out/classes" -name '*.class')
# Add the dex to the APK and align.
(cd "$out" && zip -q base.apk classes.dex)
"$bt/zipalign" -f 4 "$out/base.apk" "$out/vetro-probe-unsigned.apk"
# Sign with a debug key generated on the fly.
ks="$out/debug.keystore"
keytool -genkeypair -keystore "$ks" -storepass android -keypass android \
  -alias probe -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Vetro Probe" >/dev/null 2>&1
"$bt/apksigner" sign --ks "$ks" --ks-pass pass:android --key-pass pass:android \
  --out "$here/vetro-probe.apk" "$out/vetro-probe-unsigned.apk"
"$bt/apksigner" verify "$here/vetro-probe.apk"
echo "done: $here/vetro-probe.apk"
