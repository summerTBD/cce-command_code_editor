//! 文件系统读取 —— file_io.rs 的职责
//!
//! 唯一职责：**把「一个路径」变成「一段可以放进编辑器的文本」**。
//!
//! - 普通文件 → 文件内容
//! - 目录     → 直接子项列表（子目录带 `/` 后缀，按名称排序）
//!
//! 这里**不做**任何 UI、状态或命令判断，因此 main.rs / 未来的其它入口都能复用。
//! 把这段逻辑独立出来，是为了避免 `app.rs`（纯状态）依赖 `main.rs`（入口）——
//! 依赖方向必须始终是 main → app，而不是反过来。

use std::io;

/// 读取路径内容：文件读内容，目录列出直接子项。
pub fn load_path(path: &str) -> io::Result<String> {
    let metadata = std::fs::metadata(path)?;
    if metadata.is_dir() {
        directory_listing(path)
    } else {
        std::fs::read_to_string(path)
    }
}

/// 生成目录浏览内容。目录项按名称排序，子目录后附 `/` 便于区分。
fn directory_listing(path: &str) -> io::Result<String> {
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
        let content = load_path("Cargo.toml").expect("Cargo.toml 应该可读");
        assert!(content.contains("name = \"cce\""));
    }

    #[test]
    fn load_path_lists_directory_sorted_with_dir_suffix() {
        // 用临时目录，避免依赖仓库里文件的具体内容
        let dir = std::env::temp_dir().join(format!("cce_file_io_{}", std::process::id()));
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();

        let listing = load_path(dir.to_str().unwrap()).unwrap();
        let lines: Vec<&str> = listing.lines().collect();
        assert_eq!(lines, vec!["a.txt", "b.txt", "sub/"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_path_reports_missing_path() {
        assert!(load_path("cce-definitely-missing-9527.txt").is_err());
    }
}
