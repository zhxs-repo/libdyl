//! `load_file` 的来源判定（规范 2.1 的 HTTP(S) 直链）。
//!
//! 内核只接受两类来源：本地路径，以及 `http://` / `https://` 的 URL。
//! 本地路径由调用方先做存在性检查；URL 交给 FFmpeg 的对应协议打开。
//! 不认识的 scheme 在这里就返回 `not_supported`，而不是让 `Path::exists()`
//! 把它误报成 `file_not_found`。`file://` 也走这条拒绝分支：本地文件请直接
//! 传路径，省掉一层 URL 解码后的一致性问题。

use dyl_core::{DylError, Result};

use crate::capabilities::protocol_available;

/// 内核放行的来源类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// 没有 `scheme://` 前缀，按本地文件处理
    LocalPath,
    Http,
    Https,
}

impl SourceKind {
    /// 是否需要走网络（决定读超时与看门狗宽限）。
    pub fn is_network(self) -> bool {
        matches!(self, SourceKind::Http | SourceKind::Https)
    }
}

/// 取出 URL 的 scheme（RFC 3986：字母开头，后接字母/数字/`+`/`-`/`.`），
/// 统一转小写。只有跟着 `://` 才算 scheme —— 否则 `C:\x.mp4` 这类
/// Windows 路径里的冒号会被误认。
pub fn scheme_of(url: &str) -> Option<String> {
    let (scheme, _) = url.split_once("://")?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    Some(scheme.to_ascii_lowercase())
}

/// 判定来源；不支持的 scheme 返回 `NotSupported`。
///
/// `http` / `https` 还要求本构建的 FFmpeg 真的编译进了对应协议：Android 的
/// 静态 FFmpeg 没有 TLS 后端，`https` 在那里会以 `not_supported` 被拒绝，
/// 而不是等到打开时抛一个含糊的 demux 错误。
pub fn classify(url: &str) -> Result<SourceKind> {
    let Some(scheme) = scheme_of(url) else {
        return Ok(SourceKind::LocalPath);
    };
    match scheme.as_str() {
        "http" | "https" if protocol_available(&scheme) => Ok(if scheme == "http" {
            SourceKind::Http
        } else {
            SourceKind::Https
        }),
        "http" => Err(DylError::NotSupported(
            "this build has no http support: FFmpeg was compiled without the http protocol".into(),
        )),
        "https" => Err(DylError::NotSupported(
            "this build has no https support: FFmpeg was compiled without a TLS backend".into(),
        )),
        other => Err(DylError::NotSupported(format!(
            "unsupported source scheme '{other}': only http:// and https:// URLs are accepted, \
             everything else must be a local path"
        ))),
    }
}

/// 只看 URL 语法，不查 FFmpeg 编译了哪些协议（给看门狗挑宽限期用）。
pub fn is_network_url(url: &str) -> bool {
    matches!(scheme_of(url).as_deref(), Some("http") | Some("https"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paths_are_local() {
        for path in [
            "test_video.mp4",
            "videos/test.mp4",
            "D:\\media\\test.mkv",
            "D:/media/test.mkv",
            "/data/data/app/files/test.mp4",
            "./relative.mp4",
        ] {
            assert_eq!(classify(path).unwrap(), SourceKind::LocalPath, "{path}");
            assert!(!is_network_url(path), "{path}");
        }
    }

    #[test]
    fn http_and_https_are_urls() {
        assert_eq!(
            classify("http://example.com/a.mp4").unwrap(),
            SourceKind::Http
        );
        // scheme 大小写不敏感
        assert_eq!(
            classify("HTTP://example.com/a.mp4").unwrap(),
            SourceKind::Http
        );
        assert!(is_network_url("https://example.com/a.mp4"));
    }

    #[test]
    fn https_is_accepted_exactly_when_the_build_has_tls() {
        match classify("https://example.com/a.mp4") {
            Ok(kind) => {
                assert_eq!(kind, SourceKind::Https);
                assert!(protocol_available("https"));
            }
            Err(DylError::NotSupported(message)) => {
                assert!(!protocol_available("https"), "{message}");
                assert!(message.contains("https"), "{message}");
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn unsupported_schemes_are_rejected_as_not_supported() {
        for url in [
            "rtmp://example.com/live",
            "ftp://example.com/a.mp4",
            "foo://bar",
        ] {
            match classify(url) {
                Err(DylError::NotSupported(message)) => {
                    assert!(message.contains("scheme"), "{url}: {message}");
                }
                other => panic!("{url}: expected NotSupported, got {other:?}"),
            }
        }
    }

    /// `file://` 不是本地路径的另一种写法：它在这里被明确拒绝
    #[test]
    fn file_urls_are_rejected_with_a_pointer_to_plain_paths() {
        match classify("file:///C:/media/test.mp4") {
            Err(DylError::NotSupported(message)) => {
                assert!(message.contains("local path"), "{message}");
            }
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }

    /// 不是合法 scheme 的 `://` 不能被当成 URL
    #[test]
    fn invalid_scheme_shapes_fall_back_to_local_paths() {
        assert_eq!(scheme_of("://example.com"), None);
        assert_eq!(scheme_of("1http://example.com"), None);
        assert_eq!(scheme_of("ht tp://example.com"), None);
        assert_eq!(scheme_of("http:/example.com"), None);
        assert_eq!(classify("://example.com").unwrap(), SourceKind::LocalPath);
    }
}
