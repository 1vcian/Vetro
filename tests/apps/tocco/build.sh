#!/bin/sh
# Costruisce l'APK di prova tests/apps/tocco (una Activity, niente risorse)
# con l'SDK di Android: javac, d8, aapt2, zipalign, apksigner. Chiave di
# debug generata al volo (l'APK non si committa mai: target/apps/tocco.apk).
#   ANDROID_HOME (default ~/Library/Android/sdk), build-tools 35.0.0,
#   platforms/android-36 (o VETRO_ANDROID_PLATFORM), un JDK >= 11.
# Uso: tests/apps/tocco/build.sh  -> stampa il percorso dell'APK
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
sdk="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
bt="$sdk/build-tools/${VETRO_ANDROID_BUILD_TOOLS:-35.0.0}"
plat="$sdk/platforms/${VETRO_ANDROID_PLATFORM:-android-36}/android.jar"
[ -x "$bt/aapt2" ] && [ -f "$plat" ] || { echo "SDK di Android mancante ($bt, $plat)" >&2; exit 2; }
out="$root/target/apps/tocco"
rm -rf "$out"
mkdir -p "$out/obj" "$out/dex"
javac -nowarn --release 8 -Xlint:-options -classpath "$plat" -d "$out/obj" "$here"/src/it/vetro/tocco/*.java
"$bt/d8" --min-api 21 --lib "$plat" --output "$out/dex" "$out"/obj/it/vetro/tocco/*.class
"$bt/aapt2" link -o "$out/base.apk" -I "$plat" --manifest "$here/AndroidManifest.xml" \
  --min-sdk-version 21 --target-sdk-version 35
(cd "$out/dex" && zip -q -X "$out/base.apk" classes.dex)
"$bt/zipalign" -f 4 "$out/base.apk" "$out/aligned.apk"
keytool -genkeypair -keystore "$out/debug.keystore" -storepass android -keypass android \
  -alias vetro -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Vetro test" >/dev/null 2>&1
"$bt/apksigner" sign --ks "$out/debug.keystore" --ks-pass pass:android --out "$root/target/apps/tocco.apk" "$out/aligned.apk"
echo "$root/target/apps/tocco.apk"
