#!/bin/sh
# Costruisce vetro-probe.apk con gli strumenti dell'SDK Android (aapt2, d8,
# apksigner), senza Gradle. Serve ANDROID_HOME (o ~/Library/Android/sdk),
# build-tools (BT, default 35.0.0) e una platform (PLATFORM, default il
# android.jar più alto installato). Java 17+.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
sdk="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
bt="$sdk/build-tools/${BT:-35.0.0}"
jar="${PLATFORM:-$(ls -d "$sdk"/platforms/android-* | sort -V | tail -1)/android.jar}"
out="$here/out"
rm -rf "$out"
mkdir -p "$out/gen" "$out/classes"
# Risorse (nessuna oltre il manifest): compila e linka l'APK di base.
"$bt/aapt2" link -o "$out/base.apk" -I "$jar" \
  --manifest "$here/AndroidManifest.xml" --java "$out/gen" --min-sdk-version 29 --target-sdk-version 34
# Compila il Java e converte in dex.
javac -source 17 -target 17 -cp "$jar" -d "$out/classes" \
  "$here"/src/com/vetro/probe/*.java $(find "$out/gen" -name '*.java')
"$bt/d8" --min-api 29 --output "$out" $(find "$out/classes" -name '*.class')
# Aggiunge il dex all'APK e allinea.
(cd "$out" && zip -q base.apk classes.dex)
"$bt/zipalign" -f 4 "$out/base.apk" "$out/vetro-probe-unsigned.apk"
# Firma con una chiave di debug generata al volo.
ks="$out/debug.keystore"
keytool -genkeypair -keystore "$ks" -storepass android -keypass android \
  -alias probe -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Vetro Probe" >/dev/null 2>&1
"$bt/apksigner" sign --ks "$ks" --ks-pass pass:android --key-pass pass:android \
  --out "$here/vetro-probe.apk" "$out/vetro-probe-unsigned.apk"
"$bt/apksigner" verify "$here/vetro-probe.apk"
echo "fatto: $here/vetro-probe.apk"
