//! dyl-ffi build script
//!
//! Only Android needs anything here, and only one thing: refuse to emit a
//! cdylib that still has unresolved symbols.
//!
//! # Why this has to live here
//!
//! A cdylib is *supposed* to leave symbols undefined for the loader to resolve
//! at dlopen time. The flip side is that a mistyped FFI declaration links
//! cleanly and only fails on the device:
//!
//! ```text
//! dlopen failed: libdyl.so: undefined symbol: AAudioStreamBuilder_setSampleFormat
//! ```
//!
//! (The real NDK entry point is `AAudioStreamBuilder_setFormat`.)
//!
//! `-Wl,--no-undefined` turns that into a link error. Every symbol we
//! legitimately leave open -- libc, libm, libdl, liblog, libandroid, libaaudio --
//! resolves against the NDK sysroot stubs at link time, so nothing legitimate is
//! rejected.
//!
//! `cargo:rustc-link-arg` is **not** inherited from dependencies: cargo only
//! forwards it for the package's own artifact kinds. An earlier attempt put it in
//! dyl-ffmpeg/build.rs (where the other native link flags live) and it was
//! silently dropped -- the bad symbol shipped all the way into the APK.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // cdylib only: the rlib is just an intermediate and has no link step.
    // Android/ELF only: MSVC's link.exe has no such option (it warns LNK4044),
    // and on Windows the import libraries already fail the link when a symbol
    // is missing.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rustc-link-arg-cdylib=-Wl,--no-undefined");
    }

    // Note: FFmpeg's ~2000 symbols need no explicit hiding. rustc emits a
    // version script for every cdylib that makes everything except the
    // crate's own `#[no_mangle] pub extern` items local, so `-Wl,--exclude-libs,ALL`
    // would be redundant here.
}
