//! 打印本机链接的 FFmpeg 实际编译进来的输入协议。
//!
//! 能力清单里的 `protocols` 就是这份列表与内核接受范围的交集，因此任何关于
//! "能不能播 http(s)" 的结论都必须先跑这个探针，而不是读构建脚本猜。

fn main() {
    let version = unsafe { std::ffi::CStr::from_ptr(ffmpeg_next::sys::av_version_info()) };
    println!("linked FFmpeg: {}", version.to_string_lossy());

    let mut inputs = Vec::new();
    let mut opaque: *mut std::os::raw::c_void = std::ptr::null_mut();
    unsafe {
        loop {
            let name = ffmpeg_next::ffi::avio_enum_protocols(&mut opaque, 0);
            if name.is_null() {
                break;
            }
            inputs.push(
                std::ffi::CStr::from_ptr(name)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    inputs.sort();

    println!("input protocols ({}):", inputs.len());
    for name in &inputs {
        println!("  {name}");
    }
    for probe in ["file", "http", "https"] {
        println!("{probe}: {}", inputs.iter().any(|n| n == probe));
    }
}
