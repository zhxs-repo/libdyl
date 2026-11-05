//! dyl-ffmpeg build script
//!
//! For PC platforms (Linux/macOS/Windows), FFmpeg is found via `ffmpeg-sys-next`'s
//! built-in pkg-config / vcpkg detection. No action needed here.
//!
//! For Android (`aarch64-linux-android`), FFmpeg must be cross-compiled separately
//! using the NDK toolchain. This build script sets up the search path based on
//! environment variables:
//!
//! - `FFMPEG_PKG_CONFIG_PATH_{arch}` — arch-specific pkg-config path
//!   (e.g. `FFMPEG_PKG_CONFIG_PATH_aarch64_linux_android=/path/to/ffmpeg/arm64/lib/pkgconfig`)
//! - `FFMPEG_DIR_{arch}` — arch-specific FFmpeg root directory (fallback)
//!   (e.g. `FFMPEG_DIR_aarch64_linux_android=/path/to/ffmpeg/arm64`)
//! - `FFMPEG_PKG_CONFIG_PATH` — generic pkg-config path
//! - `FFMPEG_DIR` — generic FFmpeg root directory (fallback)

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();

    // Only need special handling for Android
    if !target.contains("android") {
        return;
    }

    // Normalize target triple for env var lookup
    // (Rust converts target triple to uppercase + underscores for env vars)
    let target_env = target.replace('-', "_").to_uppercase();

    // Try arch-specific FFMPEG_PKG_CONFIG_PATH first, then generic
    let pkg_config_key = format!("FFMPEG_PKG_CONFIG_PATH_{target_env}");
    if let Ok(path) = std::env::var(&pkg_config_key) {
        std::env::set_var("FFMPEG_PKG_CONFIG_PATH", &path);
        println!("cargo:warning=dyl-ffmpeg: Android pkg-config path = {path}");
    } else if let Ok(path) = std::env::var("FFMPEG_PKG_CONFIG_PATH") {
        println!("cargo:warning=dyl-ffmpeg: pkg-config path = {path}");
    }

    // Try arch-specific FFMPEG_DIR first, then generic.
    //
    // ffmpeg-sys-next only looks at the un-suffixed FFMPEG_DIR, so the
    // per-ABI variable has to be forwarded to it under that exact name --
    // otherwise it silently falls through to pkg-config and the build dies
    // much later with confusing missing-libraries errors.
    let dir_key = format!("FFMPEG_DIR_{target_env}");
    if let Ok(dir) = std::env::var(&dir_key) {
        std::env::set_var("FFMPEG_DIR", &dir);
        println!("cargo:warning=dyl-ffmpeg: Android FFMPEG_DIR = {dir}");

        let lib_dir = format!("{dir}/lib");
        let include_dir = format!("{dir}/include");

        // Tell the linker where to find FFmpeg .so files
        println!("cargo:rustc-link-search=native={lib_dir}");

        // Link FFmpeg libraries.
        //
        // Android links against static archives only: the .a files are baked
        // straight into libdyl.so, so the APK ships one native library
        // instead of six libav*.so files that would also need matching
        // DT_NEEDED sonames at load time.
        //
        // Order matters for static archives: dependents come before
        // dependencies (avformat -> avcodec -> ... -> avutil).
        for lib in &[
            "avformat",
            "avcodec",
            "avfilter",
            "swscale",
            "swresample",
            "avutil",
        ] {
            println!("cargo:rustc-link-lib=static={lib}");
        }

        // Android system libraries FFmpeg pulls in even with
        // --disable-autodetect (libc math + the logcat logger). The NDK sysroot
        // provides all three.
        println!("cargo:rustc-link-lib=m");
        println!("cargo:rustc-link-lib=log");
        println!("cargo:rustc-link-lib=dl");

        // No -Wl,--exclude-libs here: `cargo:rustc-link-arg` from a dependency is not
        // forwarded to the cdylib link anyway, and it is unnecessary -- rustc
        // emits a version script for every cdylib that hides everything except
        // the crate's own exported items. The link-time symbol check that does
        // matter lives in dyl-ffi/build.rs, which is the crate being linked.

        // Pass include path to ffmpeg-sys-next (for bindgen)
        println!("cargo:include={include_dir}");

        // Re-run if env vars change
        println!("cargo:rerun-if-env-changed={pkg_config_key}");
        println!("cargo:rerun-if-env-changed={dir_key}");
    } else if let Ok(dir) = std::env::var("FFMPEG_DIR") {
        println!("cargo:warning=dyl-ffmpeg: FFMPEG_DIR = {dir}");
        println!("cargo:rustc-link-search=native={dir}/lib");
    } else {
        println!(
            "cargo:warning=dyl-ffmpeg: WARNING — No FFMPEG_DIR set for Android target. \
             Set FFMPEG_DIR_{target_env} or FFMPEG_DIR to the FFmpeg Android build root."
        );
    }

    println!("cargo:rerun-if-env-changed=FFMPEG_PKG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
}
