#!/usr/bin/env bash
# Build WordCraft for Android (arm64) as a signed APK: target/android-apk/wordcraft.apk.
#
# Needs: the Android SDK (ANDROID_HOME or ANDROID_SDK_ROOT, with build-tools and
# platforms;android-34), the NDK (ANDROID_NDK_HOME), a JDK (javac), `rustup target add
# aarch64-linux-android` and `cargo install cargo-ndk`. No Gradle: aapt2 links the manifest and
# icon, javac and d8 compile the one small activity class (java/), the library and classes.dex
# are added to the archive, then zipalign and apksigner finish it. (The same recipe as PdfCraft's
# Android port.)
#
# Signing: ANDROID_KEYSTORE (a path) with ANDROID_KEYSTORE_PASSWORD and ANDROID_KEY_ALIAS when
# set; otherwise the debug key next to this script (password "android"). Android only installs
# an update signed with the same key as the installed app.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
HERE="$ROOT/packaging/android"
OUT="$ROOT/target/android-apk"
SDK=${ANDROID_HOME:-${ANDROID_SDK_ROOT:?set ANDROID_HOME to the Android SDK}}
: "${ANDROID_NDK_HOME:?set ANDROID_NDK_HOME to the Android NDK}"

API=34       # targetSdk: 35 would force edge-to-edge drawing under the system bars.
MIN_API=30   # Android 11: All files access (MANAGE_EXTERNAL_STORAGE).
BUILD_TOOLS=$(ls -d "$SDK"/build-tools/* | sort -V | tail -1)
PLATFORM="$SDK/platforms/android-$API/android.jar"
if [ ! -f "$PLATFORM" ]; then
    yes | "$SDK/cmdline-tools/latest/bin/sdkmanager" "platforms;android-$API" >/dev/null
fi
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)
VERSION_CODE=${VERSION_CODE:-1}

rm -rf "$OUT"
mkdir -p "$OUT/jniLibs" "$OUT/res/mipmap-xxxhdpi" "$OUT/apk"

echo "== libwordcraft_android.so"
(cd "$ROOT" && cargo ndk -t arm64-v8a --platform "$MIN_API" -o "$OUT/jniLibs" build -p wordcraft-android --release)
test -f "$OUT/jniLibs/arm64-v8a/libwordcraft_android.so"

echo "== resources and manifest"
# The launcher icon: WordCraft's own app icon (assets/app-icon), as a legacy icon the launcher
# fits into its shape.
cp "$ROOT/assets/app-icon/hicolor/256x256/apps/ai.storyteller.wordcraft.png" "$OUT/res/mipmap-xxxhdpi/ic_launcher.png"
"$BUILD_TOOLS/aapt2" compile --dir "$OUT/res" -o "$OUT/res.zip"
"$BUILD_TOOLS/aapt2" link -o "$OUT/unaligned.apk" -I "$PLATFORM" \
    --manifest "$HERE/AndroidManifest.xml" \
    --min-sdk-version "$MIN_API" --target-sdk-version "$API" \
    --version-code "$VERSION_CODE" --version-name "$VERSION" \
    "$OUT/res.zip"

echo "== activity class"
# WordCraftActivity: NativeActivity plus onNewIntent (documents opened from other apps).
mkdir -p "$OUT/classes" "$OUT/dex"
javac --release 11 -classpath "$PLATFORM" -d "$OUT/classes" $(find "$HERE/java" -name '*.java')
"$BUILD_TOOLS/d8" --release --min-api "$MIN_API" --lib "$PLATFORM" --output "$OUT/dex" $(find "$OUT/classes" -name '*.class')
cp "$OUT/dex/classes.dex" "$OUT/apk/"

echo "== native library"
# Only WordCraft's library: cargo-ndk may also copy dependencies' own dylib builds, which
# libwordcraft_android.so doesn't load.
mkdir -p "$OUT/apk/lib/arm64-v8a"
cp "$OUT/jniLibs/arm64-v8a/libwordcraft_android.so" "$OUT/apk/lib/arm64-v8a/"
(cd "$OUT/apk" && zip -qr "$OUT/unaligned.apk" lib classes.dex)

echo "== align and sign"
"$BUILD_TOOLS/zipalign" -p -f 4 "$OUT/unaligned.apk" "$OUT/aligned.apk"
KS=${ANDROID_KEYSTORE:-$HERE/debug.keystore}
KS_PASS=${ANDROID_KEYSTORE_PASSWORD:-android}
ALIAS=${ANDROID_KEY_ALIAS:-androiddebugkey}
"$BUILD_TOOLS/apksigner" sign --ks "$KS" --ks-key-alias "$ALIAS" --ks-pass "pass:$KS_PASS" --key-pass "pass:$KS_PASS" \
    --out "$OUT/wordcraft.apk" "$OUT/aligned.apk"
"$BUILD_TOOLS/apksigner" verify "$OUT/wordcraft.apk"
ls -la "$OUT/wordcraft.apk"
