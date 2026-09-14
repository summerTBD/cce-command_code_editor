//! 路径 ↔ `file://` uri —— uri.rs 的职责
//!
//! LSP 里说话**从不提路径**，只提 uri：`file:///D:/proj/main.rs`。
//! 所以每次对话都要在两个世界之间来回翻译，而翻译错了症状很轻、代价很大。
//!
//! ## ⚠️ 这一层存在的真正理由：**两个 uri 是同一个文件**
//!
//! 一开始的想法是「字符串相等不就行了」。实测打脸：
//!
//! ```text
//! 我们发过去：file:///D:/MyProjects/.../check.rs      ← 盘符大写
//! 它推回来：  file:///d:/MyProjects/.../check.rs      ← 盘符小写
//! ```
//!
//! 直接比字符串 → 永远不相等 → **诊断一条都不显示**，而且不报错、不崩，
//! 就是安静地什么都不发生。所以必须有 [`same_file`] 这一层：
//! **先各自解析成路径、按平台规矩归一化，再比。**
//!
//! ## 这里全是纯函数
//!
//! 不碰文件系统、不碰终端、不碰进程 —— 一串字符进，一串字符出。
//! 所以可以拿各种畸形输入往死里喂（中文、空格、百分号、大小写、UNC……）。

use std::path::{Path, PathBuf};

/// 这个字节在 uri 的路径部分里**不用转义**吗？
///
/// 保留 `A-Za-z0-9-._~`（RFC 3986 的 unreserved），再加上路径里必须保留的
/// `/` 和 `:`（Windows 的 `D:` 靠它）。
///
/// ⚠️ 判据故意**收窄**：多转义几个字符永远是合法的（对面会解回来），
/// 少转义一个就可能让 uri 变歧义（`%`、`#`、`?` 尤其）。宁可多转。
fn is_plain(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':')
}

/// 绝对路径 → `file://` uri。**相对路径返回 `None`。**
///
/// 为什么拒绝相对路径：相对谁？基准是调用方的事，而这一层不该猜。
/// （跟 `file_io::full_path` 一个立场 —— 解析只发生在一处。）
pub fn path_to_uri(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return None;
    }

    let mut text = path.to_string_lossy().replace('\\', "/");

    // ⚠️ Windows 的长路径前缀（`\\?\D:\...`）**不是 uri 的一部分**。
    //    不摘掉的话会变成一个带奇怪前半段的 uri，对面认不出来。
    if let Some(stripped) = text.strip_prefix("//?/") {
        text = stripped.to_string();
    }

    // uri 的路径部分必须以 `/` 开头：Windows 的 `D:/x` 要补一个，
    // Unix 的 `/home/x` 本来就有 —— 所以**先判再补**，不能无脑加。
    let path_part = if text.starts_with('/') {
        text
    } else {
        format!("/{text}")
    };

    let mut encoded = String::with_capacity(path_part.len() + 16);
    for byte in path_part.bytes() {
        if is_plain(byte) {
            // `is_plain` 保证了是 ASCII，所以这一步不会切坏任何字符
            encoded.push(byte as char);
        } else {
            // 非 ASCII 在这里是**逐字节**转义的，拼起来正好是 UTF-8 的百分号编码
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }

    Some(format!("file://{encoded}"))
}

/// `file://` uri → 路径。不是 `file:` 或者解不开的，返回 `None`。
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;

    // 有些工具会写成 `file://localhost/D:/x`。别的 host（真正的 UNC 共享）
    // 我们不支持 —— 认不出来就老实说不知道，比猜一个错的强。
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);

    if !rest.starts_with('/') {
        return None;
    }

    let decoded = percent_decode(rest)?;

    // Windows 上解出来是 `/D:/proj/x.rs`，开头那个斜杠得去掉才是合法路径。
    // （Unix 上 `/home/u` 的斜杠是路径本身的一部分，不能动。）
    let decoded = if cfg!(windows) && is_drive_prefixed(&decoded) {
        decoded[1..].to_string()
    } else {
        decoded
    };

    Some(PathBuf::from(
        decoded.replace('/', std::path::MAIN_SEPARATOR_STR),
    ))
}

/// 解百分号编码。**只认合法的 `%XX`**，别处的 `%` 当普通字符留下。
///
/// 为什么不严格报错：文件名里真出现一个裸 `%` 是常见的事
/// （对面没转义、或者那本来就是个百分号）。把它当成转义会解出乱七八糟的字节，
/// 而**原样留下**最坏也只是匹配不上，不会匹配错。
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &text[i + 1..i + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // 解完还得是合法的 UTF-8，否则这个 uri 是坏的
    String::from_utf8(out).ok()
}

/// `/D:` 开头吗（Windows 的盘符路径）。
fn is_drive_prefixed(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':'
}

/// 这两个 uri 指的是**同一个文件**吗？
///
/// ⚠️ 这是本文件存在的理由（见文件头）。判据是：各自解析成路径，按平台规矩
/// 归一化之后再比 —— 所以**大小写、百分号编码、斜杠方向**的差异都能抹平。
///
/// 有一个不是 `file:` uri（或者解不开）时，退回到原样字符串比较 ——
/// 那种情况下我们确实没有更好的依据。
pub fn same_file(a: &str, b: &str) -> bool {
    match (uri_to_path(a), uri_to_path(b)) {
        (Some(a), Some(b)) => path_key(&a) == path_key(&b),
        _ => a == b,
    }
}

/// 用来比对的「路径身份」。
///
/// ⚠️ **Windows 上整个转小写**：那边的路径本来就不分大小写，
/// 而服务器会自作主张规范盘符（实测把 `D:` 写成 `d:`）。
/// Unix 上不能这么做 —— 那边的 `/Home` 和 `/home` 是两个目录。
fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本平台上的一个「绝对路径 → 它该变成的 uri」样例。
    ///
    /// 两个平台各写一份，因为**同一段路径字符串在两边不是同一个意思**
    /// （`D:\x` 在 Linux 上根本不算绝对路径）。
    fn sample() -> (&'static str, &'static str) {
        if cfg!(windows) {
            // 故意同时带上中文和空格 —— 这两样最容易编码错
            (
                r"D:\proj\项目 x.rs",
                "file:///D:/proj/%E9%A1%B9%E7%9B%AE%20x.rs",
            )
        } else {
            (
                "/home/u/项目 x.rs",
                "file:///home/u/%E9%A1%B9%E7%9B%AE%20x.rs",
            )
        }
    }

    #[test]
    fn path_to_uri_encodes_the_way_the_spec_says() {
        let (path, expected) = sample();
        assert_eq!(path_to_uri(Path::new(path)).as_deref(), Some(expected));
    }

    /// 转义是**逐字节**做的，所以解回来必须一模一样 —— 中文不能变成乱码。
    #[test]
    fn uri_round_trips_including_chinese_and_spaces() {
        let (path, _) = sample();
        let uri = path_to_uri(Path::new(path)).unwrap();
        assert_eq!(uri_to_path(&uri).unwrap(), PathBuf::from(path));
    }

    /// 相对路径得先有基准，而基准不是这一层该猜的。
    #[test]
    fn a_relative_path_is_refused() {
        assert_eq!(path_to_uri(Path::new("src/main.rs")), None);
    }

    /// Windows 的长路径前缀不是 uri 的一部分。
    #[test]
    fn the_windows_long_path_prefix_is_stripped() {
        if !cfg!(windows) {
            return;
        }
        let uri = path_to_uri(Path::new(r"\\?\D:\proj\x.rs")).unwrap();
        assert_eq!(uri, "file:///D:/proj/x.rs");
    }

    // ---------- 解 uri ----------

    #[test]
    fn uri_to_path_decodes_percent_escapes() {
        let (path, uri) = sample();
        assert_eq!(uri_to_path(uri).unwrap(), PathBuf::from(path));
    }

    #[test]
    fn a_localhost_authority_is_tolerated() {
        if !cfg!(windows) {
            return;
        }
        // 有些工具会写成 `file://localhost/...`，得跟 `file:///...` 一样能解
        assert_eq!(decoded_text("file://localhost/D:/x.rs"), "D:/x.rs");
    }

    /// 真正的 UNC（`file://server/share`）我们**认不出来就说认不出来**。
    /// 猜一个错的比说不知道危险得多。
    #[test]
    fn a_remote_host_is_refused() {
        assert_eq!(uri_to_path("file://server/share/x.rs"), None);
    }

    #[test]
    fn a_non_file_uri_is_refused() {
        assert_eq!(uri_to_path("https://example.com/x.rs"), None);
        assert_eq!(uri_to_path("untitled:Untitled-1"), None);
    }

    /// 裸的 `%`（不是合法转义）要**原样留下**，不能吃掉后面的字符。
    #[test]
    fn a_lone_percent_sign_is_kept_literally() {
        if !cfg!(windows) {
            return;
        }
        // 严格按转义解的话，`%zz` 里那两个 z 会被吃掉
        assert_eq!(decoded_text("file:///D:/a%zz.rs"), "D:/a%zz.rs");
    }

    /// 解完不是合法 UTF-8 的 uri 是坏的。
    #[test]
    fn invalid_utf8_after_decoding_is_refused() {
        // `%FF` 单独一个字节不是合法 UTF-8
        assert_eq!(uri_to_path("file:///D:/%FF.rs"), None);
    }

    // ---------- 同一个文件 ----------

    /// ⚠️ **这条是整个文件的理由。**
    ///
    /// 实测 rust-analyzer 会把我们发过去的 `file:///D:/...` 规范成
    /// `file:///d:/...`（盘符变小写）。判不出来 = 诊断一条都不显示，且不报错。
    #[test]
    fn the_drive_letter_case_does_not_matter() {
        if !cfg!(windows) {
            return;
        }
        assert!(same_file(
            "file:///D:/MyProjects/x.rs",
            "file:///d:/MyProjects/x.rs"
        ));
        // 整个路径的大小写也不该有影响（Windows 上本来就不分）
        assert!(same_file("file:///D:/Proj/X.RS", "file:///d:/proj/x.rs"));
    }

    /// 一边转义一边不转义，也得认出来是同一个。
    #[test]
    fn percent_encoding_does_not_matter() {
        assert!(same_file(
            "file:///D:/proj/%E9%A1%B9%E7%9B%AE.rs",
            "file:///D:/proj/项目.rs"
        ));
    }

    #[test]
    fn different_files_are_different() {
        assert!(!same_file("file:///D:/a.rs", "file:///D:/b.rs"));
        // 长一点的路径不能因为前缀相同就算同一个
        assert!(!same_file("file:///D:/a.rs", "file:///D:/a.rs.bak"));
    }

    /// 认不出来的 uri 退回到原样比较 —— 至少不会把两个不同的东西判成同一个。
    #[test]
    fn unknown_uris_fall_back_to_plain_comparison() {
        assert!(same_file("untitled:Untitled-1", "untitled:Untitled-1"));
        assert!(!same_file("untitled:Untitled-1", "untitled:Untitled-2"));
    }

    /// 把 uri 解出来的路径写成「正斜杠文本」，方便断言。
    ///
    /// 不直接比 `PathBuf`：那个比较在跨平台时不直观（分隔符、盘符、前缀都掺在里面），
    /// 而测试要的是「解出来的那串字符对不对」。
    fn decoded_text(uri: &str) -> String {
        uri_to_path(uri)
            .unwrap_or_else(|| panic!("{uri} 解不开"))
            .to_string_lossy()
            .replace('\\', "/")
    }
}
