#!/usr/bin/env bash
# Inject libjudgehook.so into the AstroDX APK and re-sign it.
# Run inside `nix develop .#android`.
#
#   tools/patch_apk.sh <input.apk> [output.apk]
#
# Signing key, either via env:
#   KEYSTORE=path/to.jks KS_PASS=... KEY_ALIAS=... tools/patch_apk.sh in.apk
# or signing/astrodx-mod.jks + signing/KEYSTORE.md (STORE_PASSWORD=..., KEY_ALIAS=...),
# which is git-ignored. Key and store password must be the same.
#
# Steps:
#   1. cargo ndk build of the crate at the repo root (arm64-v8a)
#   2. libmain.so += DT_NEEDED libjudgehook.so   (patchelf, keeps 16 KiB alignment)
#   3. replace/add both .so in a copy of the APK (arm64-v8a only)
#   4. zipalign (16 KiB page alignment) + apksigner
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
IN=${1:?usage: tools/patch_apk.sh <input.apk> [output.apk]}
OUT=${2:-$ROOT/out/AstroDX-judgehook.apk}
ABI=arm64-v8a
WORK=$ROOT/work/patch
KS=${KEYSTORE:-$ROOT/signing/astrodx-mod.jks}
KS_MD=$ROOT/signing/KEYSTORE.md

: "${BUILD_TOOLS:?run inside: nix develop .#android}"
[ -f "$IN" ] || { echo "input APK not found: $IN"; exit 1; }
[ -f "$KS" ] || { echo "keystore not found: $KS (set KEYSTORE=...)"; exit 1; }
PASS=${KS_PASS:-$( [ -f "$KS_MD" ] && sed -n 's/^STORE_PASSWORD=//p' "$KS_MD" )}
ALIAS=${KEY_ALIAS:-$( [ -f "$KS_MD" ] && sed -n 's/^KEY_ALIAS=//p' "$KS_MD" )}
[ -n "$PASS" ] && [ -n "$ALIAS" ] || { echo "set KS_PASS and KEY_ALIAS (or provide $KS_MD)"; exit 1; }

echo "== 1/4 build libjudgehook.so"
[ -f "$ROOT/third_party/And64InlineHook/And64InlineHook.cpp" ] || git -C "$ROOT" submodule update --init
( cd "$ROOT" && cargo ndk -t "$ABI" -P 25 build --release 2>&1 | grep -E "^(warning|error)|Finished" )
SO=$ROOT/target/aarch64-linux-android/release/libjudgehook.so

# Every undefined (non-weak) dynamic symbol must be exported by the NDK stub of
# a lib we DT_NEEDED, at our minSdk. bionic does NOT export compiler-rt builtins
# (__clear_cache, __aarch64_*, ...); if one leaks through, dlopen fails at
# runtime and takes libmain.so (and the whole game) down with it.
STUBS=$(ls -d "$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/*/sysroot/usr/lib/aarch64-linux-android/25)
NEEDED=$(readelf -d "$SO" | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p')
EXPORTED=$(for lib in $NEEDED; do readelf --dyn-syms -W "$STUBS/$lib" | awk '$7!="UND" && $8!="" {sub(/@.*/,"",$8); print $8}'; done | sort -u)
MISSING=$(comm -23 <(readelf --dyn-syms -W "$SO" | awk '$7=="UND" && $5!="WEAK" && $8!="" {sub(/@.*/,"",$8); print $8}' | sort -u) \
                   <(printf '%s\n' "$EXPORTED"))
if [ -n "$MISSING" ]; then
  echo "ERROR: undefined (non-weak) symbols not provided by $(echo $NEEDED) at API 25:"
  printf '   %s\n' $MISSING
  exit 1
fi
echo "   all resolve against: $(echo $NEEDED)"

echo "== 2/4 patch libmain.so"
rm -rf "$WORK" && mkdir -p "$WORK/stage/lib/$ABI"
unzip -q -o "$IN" "lib/$ABI/libmain.so" -d "$WORK/stage"
MAIN=$WORK/stage/lib/$ABI/libmain.so
if readelf -d "$MAIN" | grep -q 'libjudgehook.so'; then
  echo "   libmain.so already depends on libjudgehook.so (input already patched?)"
else
  patchelf --page-size 16384 --add-needed libjudgehook.so "$MAIN"
fi
readelf -d "$MAIN" | grep NEEDED | sed 's/^/   /'
cp "$SO" "$WORK/stage/lib/$ABI/libjudgehook.so"

echo "== 3/4 repack"
cp "$IN" "$WORK/unsigned.apk"
chmod u+w "$WORK/unsigned.apk"
# old v2/v3 signature block is invalid after this; apksigner replaces it
( cd "$WORK/stage" && zip -q -X "$WORK/unsigned.apk" "lib/$ABI/libmain.so" "lib/$ABI/libjudgehook.so" )

echo "== 4/4 align + sign"
"$BUILD_TOOLS/zipalign" -P 16 -f 4 "$WORK/unsigned.apk" "$WORK/aligned.apk"
mkdir -p "$(dirname "$OUT")"
"$BUILD_TOOLS/apksigner" sign --ks "$KS" --ks-key-alias "$ALIAS" \
  --ks-pass "pass:$PASS" --key-pass "pass:$PASS" \
  --out "$OUT" "$WORK/aligned.apk"
"$BUILD_TOOLS/apksigner" verify --verbose "$OUT" | grep -E "Verifies|v[0-9] scheme" | sed 's/^/   /'
rm -f "$OUT.idsig"

echo
echo "done: $OUT"
echo "install (uninstall the original first, signature differs):"
echo "  adb uninstall com.Reflektone.AstroDX && adb install \"$OUT\""
