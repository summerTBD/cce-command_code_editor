//! ropey 验证性试验（spike）—— 迁移前先证实前提，别凭信仰动手。
//!
//! 运行：
//! ```text
//! cargo test --release --test rope_spike -- --ignored --nocapture --test-threads=1
//! cargo test --test rope_spike            # 只跑语义断言
//! ```
//!
//! 待验证的三件事：
//! 1. `Rope::clone()` 是否真的 O(1)（这是「撤销快照几乎免费」的全部依据）；
//! 2. `Rope::insert` 在中部插入是否与文档大小无关；
//! 3. ropey 的行语义与当前 `Buffer` 有哪些**不一样**的地方（迁移时必须知道）。

use std::time::{Duration, Instant};

use ropey::Rope;

fn ns(d: Duration) -> String {
    format!("{:>12} ns", d.as_nanos())
}

fn make_doc(lines: usize) -> String {
    (0..lines)
        .map(|i| format!("line {i}: some problem statement text here"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 前提 1：`Rope::clone()` 应该是 O(1)，与文档大小无关。
#[test]
#[ignore]
fn spike_rope_clone_is_o1() {
    println!("\nSPIKE1 Rope::clone vs String::clone (同规模对照)");
    for lines in [1_000usize, 10_000, 100_000] {
        let text = make_doc(lines);
        let rope = Rope::from_str(&text);
        let iters = if lines >= 100_000 { 200usize } else { 2_000 };

        let t = Instant::now();
        for _ in 0..iters {
            std::hint::black_box(rope.clone());
        }
        let rope_clone = t.elapsed() / iters as u32;

        let t = Instant::now();
        for _ in 0..iters {
            std::hint::black_box(text.clone());
        }
        let string_clone = t.elapsed() / iters as u32;

        println!(
            "SPIKE1 bytes={:>9} lines={:>7} rope_clone={} string_clone={}",
            text.len(),
            lines,
            ns(rope_clone),
            ns(string_clone)
        );
    }
}

/// 前提 2：中部插入应与文档大小无关；顺带测一下字符/行/字节换算的开销。
#[test]
#[ignore]
fn spike_rope_insert_and_index_cost() {
    println!("\nSPIKE2 中部插入 + 索引换算（10 万行 / 4.5 MB）");
    let text = make_doc(100_000);
    let mut rope = Rope::from_str(&text);
    let mid = rope.len_chars() / 2;
    let iters = 2_000usize;

    let t = Instant::now();
    for i in 0..iters {
        rope.insert(mid + i, "x");
    }
    let insert = t.elapsed() / iters as u32;

    let t = Instant::now();
    for i in 0..iters {
        let c = mid + i;
        std::hint::black_box(rope.char_to_line(c));
        std::hint::black_box(rope.char_to_byte(c));
        std::hint::black_box(rope.line_to_char(i % 1_000));
        std::hint::black_box(rope.line(i % 1_000).len_chars());
    }
    let index = t.elapsed() / (iters as u32 * 4);

    println!(
        "SPIKE2 insert_at_mid={}  per_index_op={}  len_chars={} len_lines={}",
        ns(insert),
        ns(index),
        rope.len_chars(),
        rope.len_lines()
    );
}

/// 前提 3：语义差异（这些断言不带 #[ignore]，`cargo test` 会跑）。
///
/// 这些差异直接决定迁移的工作量，必须先摸清。
#[test]
fn spike_rope_semantics() {
    let r = Rope::from_str("a\nbb\n");

    // (1) 行数口径：与 Buffer（split('\n')）一致
    assert_eq!(r.len_lines(), 3, "a\\nbb\\n 应有 3 行（含末尾空行）");

    // (2) ⚠️ 关键差异：ropey 的 line(i) **包含行尾换行符**，
    //     而当前 Buffer::get_line(i) 不含。迁移时每个调用点都要处理。
    assert_eq!(r.line(0).to_string(), "a\n");
    assert_eq!(r.line(1).to_string(), "bb\n");
    assert_eq!(r.line(2).to_string(), "");

    // (3) RopeSlice::as_str() 返回 Option<&str>，只在「切片恰好在一段连续内存里」时才是 Some，
    //     否则必须走 to_string()。不能靠它维持现有的 `get_line -> Option<&str>` API。
    let line0 = r.line(0);
    assert!(
        line0.as_str().is_some(),
        "短行通常能借出 &str，但这是实现细节，不能依赖"
    );

    // (4) 字节级往返是保真的（保存文件不会丢字节）
    assert_eq!(r.to_string(), "a\nbb\n");
    assert_eq!(
        Rope::from_str("a\r\nb").to_string(),
        "a\r\nb",
        "换行符原样保留"
    );

    // (5) 我们**关掉了** ropey 的 `unicode_lines` / `cr_lines` 特性（见 Cargo.toml），
    //     于是只有 '\n' 算换行 —— 与旧实现 `split('\n')` 完全一致，迁移期间行为不变。
    assert_eq!(
        Rope::from_str("a\rb").len_lines(),
        1,
        "孤立 \\r 不算换行（旧 Buffer::from_str 也是 1 行）"
    );
    let crlf = Rope::from_str("a\r\nb");
    assert_eq!(crlf.len_lines(), 2, "\\n 算换行，所以是 2 行");
    assert_eq!(
        crlf.line(0).to_string(),
        "a\r\n",
        "line() 会把行尾整个 \\r\\n 都包含进去"
    );
}

/// 前提 4：克隆出来的 Rope 是否与原 Rope **共享**底层数据（真 O(1) 而非"很快的深拷贝"）。
///
/// 做法：克隆后修改副本，原副本内容必须不变——证明是写时复制（COW），
/// 而不是"每次 insert 都整体复制一份"。
#[test]
fn spike_rope_clone_is_copy_on_write() {
    let mut a = Rope::from_str("hello world");
    let mut b = a.clone();

    b.insert(5, ",");
    assert_eq!(b.to_string(), "hello, world");
    assert_eq!(a.to_string(), "hello world", "改副本不该影响原 rope");

    a.insert(0, ">> ");
    assert_eq!(a.to_string(), ">> hello world");
    assert_eq!(b.to_string(), "hello, world", "反向也一样");
}

/// 前提 5：确认 rope 的基础操作**不随文档变大而变慢**。
///
/// 这很重要：`len_lines()` / `line_to_char()` 这类调用在每次按键、每帧渲染里都会走到，
/// 万一哪个是 O(n)，就会在后台悄悄退化成"大文件卡死"。
#[test]
#[ignore]
fn spike_rope_primitive_ops_scale() {
    println!("\nSPIKE3 rope 基础操作 vs 文档大小（应为常数）");
    for lines in [1_000usize, 10_000, 100_000] {
        let text = make_doc(lines);
        let mut rope = Rope::from_str(&text);
        let mid_line = lines / 2;
        let iters = 20_000usize;

        let t = Instant::now();
        for _ in 0..iters {
            std::hint::black_box(rope.len_lines());
        }
        let len_lines = t.elapsed() / iters as u32;

        let t = Instant::now();
        for i in 0..iters {
            std::hint::black_box(rope.line_to_char(i % lines));
        }
        let line_to_char = t.elapsed() / iters as u32;

        let t = Instant::now();
        for _ in 0..iters {
            std::hint::black_box(rope.get_line(mid_line).map(|l| l.len_chars()));
        }
        let get_line = t.elapsed() / iters as u32;

        let base = rope.line_to_char(mid_line);
        let t = Instant::now();
        for i in 0..iters {
            let idx = base + (i % 30);
            rope.insert_char(idx, 'x');
            rope.remove(idx..idx + 1);
        }
        let insert_remove = t.elapsed() / iters as u32;

        println!(
            "SPIKE3 lines={:>7} len_lines={} line_to_char={} get_line={} insert+remove={}",
            lines,
            ns(len_lines),
            ns(line_to_char),
            ns(get_line),
            ns(insert_remove)
        );
    }
}
