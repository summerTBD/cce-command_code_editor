//! 性能基准（默认跳过，不会拖慢 `cargo test`）。
//!
//! 用途：回答「要不要换数据结构（Rope / GapBuffer）？」这类问题——
//! **先用数据说话，不要凭感觉优化**。
//!
//! 运行方式（必须 release，debug 数字无意义）：
//!
//! ```text
//! cargo test --release --test perf_scaling -- --ignored --nocapture --test-threads=1
//! ```
//!
//! 三条曲线分别回答三个问题：
//! - `CASE1`  / `CASE1b`：在行首插入 1 个字符，代价随**行长度**怎样变化？
//!   （当前是 O(行长)，因为 `String::insert` 要整体后移，且要重建整行的列布局缓存）
//! - `CASE2`：敲一个键（插入 + 光标钳制 + 滚动），代价随**总行数**怎样变化？
//!   （优化后应为常数：最大列宽缓存不再每次重扫全文档）

use std::time::{Duration, Instant};

use stbd::app::{App, Cursor, EditorMode};
use stbd::buffer::Buffer;

fn ns(d: Duration) -> String {
    format!("{:>12} ns", d.as_nanos())
}

/// 行内插入的代价 vs 行长度（ASCII）。
#[test]
#[ignore]
fn bench_insert_vs_line_length() {
    println!("\nCASE1 insert char at col 0, ASCII line");
    for n in [100usize, 1_000, 10_000, 100_000, 1_000_000] {
        let content = "a".repeat(n);
        let mut b = Buffer::from_str(&content);
        let iters = 2_000usize;
        let t = Instant::now();
        for _ in 0..iters {
            b.insert_char(0, 0, 'x');
        }
        println!(
            "CASE1  line_chars={:>9}  per_insert={}",
            n,
            ns(t.elapsed() / iters as u32)
        );
    }
}

/// 行内插入的代价 vs 行长度（中文：3 字节 + 显示宽 2，最坏情况）。
#[test]
#[ignore]
fn bench_insert_vs_line_length_cjk() {
    println!("\nCASE1b insert char at col 0, CJK line (3 bytes, width 2)");
    for n in [100usize, 1_000, 10_000, 100_000] {
        let content = "\u{597d}".repeat(n);
        let mut b = Buffer::from_str(&content);
        let iters = 1_000usize;
        let t = Instant::now();
        for _ in 0..iters {
            b.insert_char(0, 0, 'x');
        }
        println!(
            "CASE1b line_chars={:>9}  per_insert={}",
            n,
            ns(t.elapsed() / iters as u32)
        );
    }
}

/// 完整一次按键的代价 vs 总行数（应接近常数；若随行数线性增长说明有全文档扫描）。
#[test]
#[ignore]
fn bench_keystroke_vs_line_count() {
    println!("\nCASE2 full keystroke (insert + clamp + scroll) vs total lines");
    for m in [100usize, 1_000, 10_000, 100_000] {
        let content = (0..m)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut app = App::from_content(None, content);
        app.set_mode(EditorMode::Edit);
        let iters = 2_000usize;
        let t = Instant::now();
        for _ in 0..iters {
            app.insert_char_at_cursor('x');
            app.clamp_cursor_to_buffer();
            app.scroll_viewport_to_keep_cursor_visible(30, 80);
        }
        println!(
            "CASE2  total_lines={:>9}  per_keystroke={}",
            m,
            ns(t.elapsed() / iters as u32)
        );
    }
}

/// CASE4：真实负载 —— 用 stbd 打开 stbd 自己的源码。
///
/// 分别测量三件事：
/// - `open`      = `Buffer::from_str()`，即打开文件时的一次性代价；
/// - `keystroke` = `Buffer` 层一次字符插入（含整行列布局重建）；
/// - `insert+undo` = 走一次「按键 → 存快照 → 撤销」。
///
/// `est_mem` 是估算的常驻内存：文本字节 + 布局缓存。
/// 布局缓存是 `byte_starts`/`cell_starts` 两个 `Vec<usize>`，每个字符各占 8 字节，
/// 所以约等于 `(字符数 + 行数) × 16` 字节。
#[test]
#[ignore]
fn bench_open_stbd_source() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let files = [
        "src/app.rs",
        "src/update.rs",
        "src/main.rs",
        "src/ui.rs",
        "src/event.rs",
        "src/file_io.rs",
    ];

    println!("\nCASE4 real workload: open stbd's own source");
    for rel in files {
        let path = root.join(rel);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let bytes = content.len();
        let line_count = content.split('\n').count();
        let chars: usize = content.chars().count();
        let iters = 200usize;
        let mid = line_count / 2;

        // (a) 打开：from_str（会为每一行建 LineLayout）
        let t = Instant::now();
        for _ in 0..iters {
            std::hint::black_box(Buffer::from_str(&content));
        }
        let open = t.elapsed() / iters as u32;

        // (b) 每键：Buffer 层一次插入 + 一次删除（取平均）
        let mut b = Buffer::from_str(&content);
        let t = Instant::now();
        for _ in 0..iters {
            b.insert_char(mid, 0, 'x');
            b.delete_char_before(mid, 1);
        }
        let keystroke = t.elapsed() / (iters as u32 * 2);

        // (c) 按键 + 撤销
        let mut app = App::from_content(None, content.clone());
        let t = Instant::now();
        for _ in 0..iters {
            app.cursor = Cursor { row: mid, col: 0 };
            app.break_undo_group();
            app.insert_char_at_cursor('x');
            app.undo();
        }
        let insert_undo = t.elapsed() / iters as u32;

        let est_mem = bytes + (chars + line_count) * 16;
        println!(
            "CASE4 {:<16} {:>6} B {:>5} lines | open={} | keystroke={} | insert+undo={} | est_mem={:>9} B",
            rel,
            bytes,
            line_count,
            ns(open),
            ns(keystroke),
            ns(insert_undo),
            est_mem
        );
    }
}
/// 撤销一步的代价 vs 文档大小。
///
/// 正文换成 rope 之后，存快照 = `Buffer::clone()`（rope + `Arc` 的写时复制），
/// 应当是 **O(1)**。这里同时测一个 `to_string()` 作对照——
/// 后者就是旧实现每存一步快照要付的代价（O(文档长度)，而且每步占一份全文内存）。
#[test]
#[ignore]
fn bench_undo_step_vs_document_size() {
    println!("\nCASE3 undo: cost per undo step (full-text snapshot) vs document size");
    for lines in [100usize, 1_000, 10_000, 100_000] {
        let content = (0..lines)
            .map(|i| format!("line {i}: some problem statement text here"))
            .collect::<Vec<_>>()
            .join("\n");
        let doc_bytes = content.len();
        let iters = if lines >= 10_000 { 200usize } else { 2_000 };

        // (a) 存一份快照：现在是 Buffer::clone()（rope + Arc 写时复制），应当 O(1)
        let buffer = Buffer::from_str(&content);
        let t = Instant::now();
        for _ in 0..iters {
            std::hint::black_box(buffer.snapshot());
        }
        let snapshot = t.elapsed() / iters as u32;

        // (a2) 对照：to_string() —— 旧实现每存一步快照要付的代价
        let t = Instant::now();
        for _ in 0..iters {
            std::hint::black_box(buffer.to_string());
        }
        let text_copy = t.elapsed() / iters as u32;

        // (b) 完整走一次「按键 → 存快照 → 撤销」
        let mut app = App::from_content(None, content.clone());
        let t = Instant::now();
        for i in 0..iters {
            app.cursor = Cursor {
                row: i % 10,
                col: 0,
            };
            app.break_undo_group();
            app.insert_char_at_cursor('x');
            app.undo();
        }
        let full_undo = t.elapsed() / iters as u32;

        println!(
            "CASE3  lines={:>7}  bytes={:>9}  snapshot={}  old_to_string={}  full_undo={}",
            lines,
            doc_bytes,
            ns(snapshot),
            ns(text_copy),
            ns(full_undo)
        );
    }
}
