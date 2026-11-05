#!/bin/sh
# Cross-compile FFmpeg for Android (arm64-v8a) using the Android NDK.
#
# Runs from a POSIX shell (Git for Windows / MSYS2) on a plain Windows box --
# no WSL, no Docker, no MSYS2 required. Everything the build needs is already
# on the machine:
#
#   * POSIX shell + awk/sed/grep/perl/tar  -> Git for Windows (sh.exe)
#   * GNU make 4.3                          -> $NDK/prebuilt/windows-x86_64/bin/make.exe
#   * clang for aarch64-android             -> $LLVM/bin/aarch64-linux-android<API>-clang
#                                              (the extension-less bash wrapper -- the .cmd
#                                               variant cannot be exec'd from a POSIX shell)
#   * nasm/yasm                             -> only needed for x86 targets, not used here
#
# Usage (from PowerShell):
#   sh scripts/build_ffmpeg_android.sh
#
# Env overrides:
#   ANDROID_API      API level to compile against (default 26; AAudio needs 26+)
#   FFMPEG_SRC       FFmpeg source tree (default D:/Devs/tmp/ffmpeg-7.1.1)
#   FFMPEG_PREFIX    install prefix          (default <repo>/third_party/ffmpeg-android/arm64)
#   JOBS             parallel make jobs      (default: nproc)

set -e

API="${ANDROID_API:-26}"
ARCH=aarch64

NDK="${NDK_ROOT:-D:/Devs/android_sdk/ndk/28.2.13676358}"
LLVM="$NDK/toolchains/llvm/prebuilt/windows-x86_64"
SYSROOT="$LLVM/sysroot"
HOST_BIN="$NDK/prebuilt/windows-x86_64/bin"

SRC="${FFMPEG_SRC:-D:/Devs/tmp/ffmpeg-7.1.1}"
PREFIX="${FFMPEG_PREFIX:-D:/Devs/github/dyl-player/libdyl/third_party/ffmpeg-android/arm64}"

# Host compiler, used only by configure for its own C11 feature probes
# (--disable-programs means no host binaries actually get built).
# MSVC's cl.exe rejects -std=c11, so configure dies with "Host compiler lacks
# C11 support" unless we hand it a clang. Any clang works here -- libclang.dll
# is already a hard dependency for ffmpeg-sys-next's bindgen step.
#
# configure expands $hostcc unquoted (`test_cmd $hostcc $hostccflags ...`), so a
# path containing spaces comes out as `/c/Program: No such file or directory`.
# Wrap the real compiler in a space-free wrapper script instead.
HOSTCC_BIN="${HOSTCC_BIN:-/c/Program Files/LLVM/bin/clang.exe}"
HOSTCC_DIR="${HOSTCC_DIR:-D:/Devs/tmp/ffmpeg-hosttools}"
mkdir -p "$HOSTCC_DIR"
HOSTCC="$HOSTCC_DIR/hostcc"
cat > "$HOSTCC" <<EOF
#!/bin/sh
exec "$HOSTCC_BIN" "\$@"
EOF
chmod +x "$HOSTCC"

# NDK llvm tools (clang/llvm-ar/llvm-nm/llvm-ranlib/llvm-strip)
PATH="$LLVM/bin:$PATH"
export PATH

# GNU make 4.3 lives in the NDK's *host* prebuilt dir, not in toolchains/llvm.
# PATH alone is not enough: the NDK ships `make.exe` but no `make`, and MSYS
# resolves the bare name against /usr/bin first. Call it by absolute path.
MAKE="${MAKE:-$HOST_BIN/make.exe}"

# Archive step: make.exe runs every recipe line through cmd.exe, which caps a
# command line at 8191 chars. libavcodec's archive step needs ~20k of object
# paths, so `$^` arrives chopped mid-filename ("libavfilter/vf_qp." instead of
# "libavfilter/vf_qp.o" is the giveaway) and llvm-ar reports the truncated tail
# as missing files.
#
# Wrapping `ar` in a script does NOT help -- the wrapper's own argv is already
# truncated before it runs. The object list has to reach `ar` without passing
# through a command line at all, so library.mak's archive rule is patched below
# to route it through an @file response file (LLVM ar has supported @file since
# LLVM 12). `$(file >...)` is evaluated by make itself, so the list never
# reaches cmd.exe.
AR="$LLVM/bin/llvm-ar.exe"
RANLIB="$LLVM/bin/llvm-ranlib.exe"

JOBS="${JOBS:-$(nproc 2>/dev/null || echo 4)}"

echo "==> NDK      : $NDK"
echo "==> API      : $API"
echo "==> Source   : $SRC"
echo "==> Prefix   : $PREFIX"
echo "==> Host CC  : $HOSTCC (wrapping $HOSTCC_BIN)"
echo "==> Jobs     : $JOBS"

cd "$SRC"

# Static libs only: they get linked straight into libdyl.so, so the APK
# ships a single .so instead of six libav*.so files.
#
# --enable-pic is mandatory: the objects end up inside a shared object.
# Every tool is passed explicitly: FFmpeg 7.x's configure has no bare `clang`/`gcc`
# case in its --toolchain switch (only `clang-*` sanitizer variants, msvc, gcov, ...),
# `--toolchain=llvm` only exists from FFmpeg 8 on, and the NDK dropped GNU binutils
# back in r23 -- so the llvm binutils have to be named one by one.
# --disable-autodetect keeps the dependency set at "libc only" (no zlib, no iconv,
#   no TLS stack), which keeps the static link from dragging in more system libs.
# --disable-encoders: dyl only demuxes and decodes (no encoder is referenced
#   anywhere in crates/), and dropping the encoders removes a large chunk of
#   libavcodec -- including the native Opus encoder that Android cannot load.
./configure \
    --prefix="$PREFIX" \
    --target-os=android \
    --arch="$ARCH" \
    --enable-cross-compile \
    --sysroot="$SYSROOT" \
    --cc="$LLVM/bin/aarch64-linux-android$API-clang" \
    --ar="$AR" \
    --nm="$LLVM/bin/llvm-nm.exe" \
    --ranlib="$RANLIB" \
    --strip="$LLVM/bin/llvm-strip.exe" \
    --host-cc="$HOSTCC" \
    --enable-static \
    --disable-shared \
    --enable-pic \
    --disable-programs \
    --disable-doc \
    --disable-devices \
    --disable-autodetect \
    --disable-encoders \
    --disable-debug \
    --enable-stripping

# Rewrite the archive rule in ffbuild/library.mak so the object list travels via
# a response file instead of the command line. See the note next to AR above.
#
# `$(file >f,$^)` is expanded by make (never by a shell), and `$^` is make's own
# authoritative prerequisite list -- which is exactly what must not be guessed at
# by hand: FFmpeg 7.x spreads codec sources over subdirectories
# (libavcodec/aarch64/, libavcodec/opus/, libavcodec/hevc/, libavutil/aarch64/,
# ...), and a glob of the top level silently leaves ~10% of the objects out of
# the archive.
#
# Idempotent: re-running skips the patch if it is already applied.
patch_library_mak() {
    mak="$SRC/ffbuild/library.mak"
    [ -f "$mak" ] || { echo "error: $mak not found" >&2; exit 1; }

    if grep -q 'dyl-android-rsp' "$mak"; then
        echo "==> library.mak already patched"
        return 0
    fi

    # Replace the `$(AR) $(ARFLAGS) $(AR_O) $^` recipe line.
    #
    # The response file is keyed on `$@`, not `$(SUBDIR)$(LIBNAME)`: the latter are
    # plain globals that library.mak reassigns for every library while it is being
    # read, so by the time a recipe is expanded they hold the *last* library's
    # value. Every archive would then share one rsp path and the parallel jobs
    # would delete each other's file mid-flight.
    awk '
        /\$\(AR\) \$\(ARFLAGS\) \$\(AR_O\) \$\^/ && !done {
            print "\t$(file >$@.rsp,$^)"
            print "\t$(AR) $(ARFLAGS) $(AR_O) @$@.rsp"
            print "\t$(RM) $@.rsp  # dyl-android-rsp"
            done = 1
            next
        }
        { print }
    ' "$mak" > "$mak.new"

    if ! grep -q 'dyl-android-rsp' "$mak.new"; then
        echo "error: failed to patch the archive rule in $mak" >&2
        rm -f "$mak.new"
        exit 1
    fi

    mv "$mak.new" "$mak"
    echo "==> patched $mak (archive step now uses a response file)"
}

patch_library_mak

echo "==> Make     : $MAKE"
echo "==> configure OK, building"

# Drop stale archives before building. An incremental `make` only rebuilds an
# archive when one of its objects is newer, so a partially-written .a left by an
# earlier interrupted run survives and gets installed as-is.
find . -name '*.a' -delete

"$MAKE" -j"$JOBS"

echo "==> installing to $PREFIX"
"$MAKE" install

echo "==> done. Installed libraries:"
ls -la "$PREFIX/lib"