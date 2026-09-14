//! 文件系统读取 —— file_io.rs 的职责
//!
//! 唯一职责：**把「一个路径」变成「一段可以放进编辑器的文本」**。
//!
//! - 普通文件 → 文件内容
//! - 目录     → 直接子项列表（子目录带 `/` 后缀，按名称排序）
//!
//! 另外提供 [`full_path`]：把用户敲的相对路径定死成完整路径。
//! 它也得住在这里 —— `app.rs` 是纯状态，不碰磁盘也不问工作目录。
//!
//! 这里**不做**任何 UI、状态或命令判断，因此 main.rs / 未来的其它入口都能复用。
//! 把这段逻辑独立出来，是为了避免 `app.rs`（纯状态）依赖 `main.rs`（入口）——
//! 依赖方向必须始终是 main → app，而不是反过来。

use std::io;
use std::path::Path;

/// 读取路径内容：文件读内容，目录列出直接子项。
pub fn load_file_or_directory(path: &str) -> io::Result<String> {
    let metadata = std::fs::metadata(path)?;
    if metadata.is_dir() {
        list_directory_entries(path)
    } else {
        std::fs::read_to_string(path)
    }
}

/// 把路径定死成**完整路径**（相对路径按进程的工作目录展开）。
///
/// 这是 [`full_path_in`] 在不给基准时的样子（启动时就这样：还没有「当前文档」）。
pub fn full_path(path: &str) -> String {
    full_path_in(None, path)
}

/// 把路径定死成**完整路径**：绝对路径原样返回，相对路径相对 `base` 展开。
///
/// `base` 就是「用户此刻认为自己在哪」—— 见 [`crate::app::App::current_directory`]。
/// 它比进程的工作目录可靠得多：工作目录是**隐形**的（用户看不见它在哪、
/// 也没有命令能改它），而 `base` 恰恰是屏幕上那份文档所在的地方。
///
/// ## 为什么必须在「打开成功」那一刻定死
///
/// 只要还记着相对路径，「这份文档到底在哪」就成了一个跟着基准漂的东西：
///
/// - `:w` 可能写到别处（甚至失败）
/// - `:ls` 显示的不是真地址，用户没法判断自己在哪一层
/// - 更隐蔽的：`documents` 去重是**按字符串比**的，同一个文件写成两种样子
///   （`Cargo.toml` 和 `D:\proj\Cargo.toml`）会被当成两份文档
///
/// 打开成功的那一瞬间是我们**唯一有把握**的时刻（读到了内容 = 路径有效），
/// 就地钉住它，后面所有派生路径都从它长出来。
///
/// ## 为什么不用 `std::fs::canonicalize`
///
/// 它顺手多做两件我们不要的事：解开符号链接（于是 `:w` 写到了链接指向的真身，
/// 而用户以为自己开的是那个链接），以及在 Windows 上给出 `\\?\D:\...` 这种
/// verbatim 前缀 —— 那前缀会一路漏进状态栏和 `:ls`，人看着莫名其妙。
/// 我们要的只是「绝对」，不是「唯一」。
pub fn full_path_in(base: Option<&str>, path: &str) -> String {
    let path = Path::new(path);
    if path.is_absolute() {
        return path.to_string_lossy().into_owned();
    }
    if let Some(base) = base {
        return Path::new(base).join(path).to_string_lossy().into_owned();
    }
    // 还没打开任何东西，没基准可用：退回进程的工作目录
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path).to_string_lossy().into_owned(),
        // 拿不到工作目录（几乎不可能）：原样返回，至少不比改之前更差
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// 生成目录浏览内容。目录项按名称排序，子目录后附 `/` 便于区分。
fn list_directory_entries(path: &str) -> io::Result<String> {
    let mut entries = std::fs::read_dir(path)?
        .map(|entry| {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let suffix = if entry.file_type()?.is_dir() { "/" } else { "" };
            Ok(format!("{name}{suffix}"))
        })
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|name| name.to_lowercase());
    Ok(entries.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_path_reads_file_content() {
        let content = load_file_or_directory("Cargo.toml").expect("Cargo.toml 应该可读");
        assert!(content.contains("name = \"stbd\""));
    }

    #[test]
    fn load_path_lists_directory_sorted_with_dir_suffix() {
        // 用临时目录，避免依赖仓库里文件的具体内容
        let dir = std::env::temp_dir().join(format!("stbd_file_io_{}", std::process::id()));
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();

        let listing = load_file_or_directory(dir.to_str().unwrap()).unwrap();
        let lines: Vec<&str> = listing.lines().collect();
        assert_eq!(lines, vec!["a.txt", "b.txt", "sub/"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_path_reports_missing_path() {
        assert!(load_file_or_directory("stbd-definitely-missing-9527.txt").is_err());
    }

    #[test]
    fn full_path_makes_a_relative_path_absolute() {
        let full = full_path("Cargo.toml");
        assert!(Path::new(&full).is_absolute(), "{full}");
        assert!(full.ends_with("Cargo.toml"), "{full}");
        // `\\?\` 是给 API 看的，不该漏给用户
        assert!(!full.starts_with(r"\\?\"), "{full}");
    }

    #[test]
    fn full_path_is_idempotent() {
        // 启动路径和 open_path 都会调它，而目录列表里的路径本来就是完整的 ——
        // 「已经完整」的路径再展开一次必须还是它自己
        let once = full_path("Cargo.toml");
        assert_eq!(full_path(&once), once);
    }

    #[test]
    fn full_path_leaves_an_absolute_path_alone() {
        let absolute = if cfg!(windows) {
            r"D:\tmp\x.rs"
        } else {
            "/tmp/x.rs"
        };
        assert_eq!(full_path(absolute), absolute);
    }

    #[test]
    fn full_path_in_joins_a_relative_path_onto_the_base() {
        let base = if cfg!(windows) {
            r"D:\proj\src"
        } else {
            "/proj/src"
        };
        assert_eq!(
            Path::new(&full_path_in(Some(base), "main.rs")),
            Path::new(base).join("main.rs")
        );
        // `..` 交给操作系统去解，这里只负责拼
        assert_eq!(
            Path::new(&full_path_in(Some(base), "../README.md")),
            Path::new(base).join("../README.md")
        );
    }

    #[test]
    fn full_path_in_ignores_the_base_for_an_absolute_path() {
        let base = if cfg!(windows) {
            r"D:\proj\src"
        } else {
            "/proj/src"
        };
        let absolute = if cfg!(windows) {
            r"E:\other\x.rs"
        } else {
            "/other/x.rs"
        };
        assert_eq!(full_path_in(Some(base), absolute), absolute);
    }
}
