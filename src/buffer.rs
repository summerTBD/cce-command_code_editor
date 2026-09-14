//! 文本缓冲 —— 行模型，以及「字符 / 字节 / 显示列」三套坐标之间的换算。
//!
//! 这里**只**负责文本的存储与编辑原语（插入、删除、断行、换行合并……），
//! 不关心光标、视口、模式这些编辑器的上层概念。谁在用：
//!
//! - [`crate::app::App`] 在这些原语之上实现「按光标编辑」的语义；
//! - [`crate::ui`] 只读地取「某行从第几列开始可见」用于渲染。
//!
//! ## 存储：rope（平衡树），不是 `String`
//!
//! 正文存在 [`ropey::Rope`] 里。相比 `String` / `Vec<String>`：
//!
//! - 任意位置插入删除是 O(log n)，不需要把后面的字节整体后移；
//! - `Rope::clone()` 是 **O(1)**（写时复制，实测 3 ns，与文档大小无关），
//!   所以撤销快照几乎免费；
//! - 字符 / 字节 / 行号三种下标之间的换算由 ropey 直接提供（O(log n)），
//!   不再需要自己维护「每字符两个 usize」的边界表。
//!
//! ## 三套坐标
//!
//! | 坐标 | 单位 | 谁在用 |
//! |---|---|---|
//! | 字节 | UTF-8 字节 | rope 内部 |
//! | 字符 | `char` 个数 | 编辑器的行列坐标（中文不会错位） |
//! | 显示列 | 终端格数（中文/全角算 2） | 横向滚动、光标画在哪一格 |
//!
//! 前两者的换算 ropey 包了；**显示列 rope 不知道**（那是终端的事），
//! 所以按需扫描某一行来换算，见 [`Buffer::get_cell_at_char`]。
//!
//! ## 换行约定
//!
//! 行与行之间由 `\n` 分隔。为了让「行内容」和「保存」这两件事互不干扰：
//!
//! - [`Buffer::get_line`] 返回的文本**不含**行尾换行符；
//! - [`Buffer::to_string`] 会原样拼回（**字节级往返**，保存不丢字节）。
//!
//! 注意 ropey 默认还会把 `\r\n`、孤立 `\r`、以及 Unicode 行分隔符当作换行；
//! 我们**关掉了那些特性**（见 `Cargo.toml`），只认 `\n`，与旧实现语义一致。

use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use ropey::Rope;
use unicode_width::UnicodeWidthChar;

/// 单个字符占的终端显示列数。
///
/// - 中文 / 全角：2 列
/// - 控制字符（含 Tab）：`unicode-width` 返回 `None`，这里按 0 列算
///   （与旧实现一致；Tab 的真实宽度属于「渲染策略」，暂未纳入）
fn char_cells(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// 文本缓冲 —— 内部是 rope，对外仍然是「行」的视角。
#[derive(Debug, Clone, Default)]
pub struct Buffer {
    /// 正文（rope 的 `clone()` 是 O(1)，所以本结构体的 `Clone` 也很便宜）
    rope: Rope,
    /// 每行的显示列数，与 `rope` 的行一一对应。
    ///
    /// 只存「每行一个 usize」（8 字节/行），不像旧实现那样为每个字符存两张边界表
    /// （16 字节/字符，64 KB 的文件就要 ~1 MB）。
    /// 用途：横向滚动钳制需要知道「最宽的一行有多宽」。
    ///
    /// 用 `Arc` 是为了让 `Buffer::clone()`（撤销快照）保持 **O(1)**：
    /// 克隆只加一次引用计数，真要改的时候才 `Arc::make_mut` 复制一份（写时复制）。
    line_cells: Arc<Vec<usize>>,
    /// 全缓冲最大显示列数的缓存（= `line_cells` 的最大值），增量维护。
    ///
    /// 之所以要缓存：水平滚动钳制每敲一个键都会问一次「最宽的一行有多宽」，
    /// 若每次都扫全文档就是 O(行数)/键。这里改成编辑时顺手维护，绝大多数编辑是 O(1)。
    max_cells: usize,
}

/// 把整篇文本拼回一个 `String`（保存文件时用），行间以 `\n` 连接。
///
/// 走 `Display` 而不是写一个固有方法：`to_string()` 由标准库的 `ToString` 提供，
/// 调用方写法不变（`buffer.to_string()`），而且 `format!("{buffer}")` 也能用，
/// 也不会和 `ToString` 撞名（那样会被 clippy 的 `inherent_to_string` 盯上）。
impl fmt::Display for Buffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.rope)
    }
}

impl Buffer {
    /// 把一段文本切分成行。
    ///
    /// - `"abc\n"` → `["abc", ""]`（文件末尾换行时仍留一个可停留的空行）
    /// - `""`      → `[""]`（空文件也有一行，方便光标移动）
    ///
    // 名字刻意就叫 `from_str`，而不是去实现 `std::str::FromStr`：
    // 这个转换**不可能失败**，做成 `FromStr` 就得返回 `Result`，白白逼每个调用方 unwrap。
    // 而且它和 `to_string` 是一对，改成 `from_text` 反而丢了对称。
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(content: &str) -> Self {
        let mut buffer = Self {
            rope: Rope::from_str(content),
            line_cells: Arc::new(Vec::new()),
            max_cells: 0,
        };
        buffer.rebuild_all_line_cells();
        buffer
    }

    /// 重扫所有行、重建 `line_cells` 与 `max_cells`。
    ///
    /// 做法是**一次线性扫描整篇文本**，遇到 `\n` 就结算一行的宽度。
    /// 比「逐行取 `RopeSlice` 再迭代」少掉很多每行的固定开销（每行一次 B 树下行）。
    fn rebuild_all_line_cells(&mut self) {
        let mut cells = Vec::new();
        let mut current = 0usize;
        for ch in self.rope.chars() {
            if ch == '\n' {
                cells.push(current);
                current = 0;
            } else {
                current += char_cells(ch);
            }
        }
        cells.push(current); // 最后一行（可能为空）
        self.line_cells = Arc::new(cells);
        self.recompute_max_cells();
    }

    /// 取一份快照。
    ///
    /// 正文是 rope（写时复制）、`line_cells` 是 `Arc`，所以这个克隆是 **O(1)** 的
    /// ——存快照几乎不花时间和内存，撤销栈才可以放心地每步都存一份。
    pub fn snapshot(&self) -> Self {
        self.clone()
    }

    /// 量出第 row 行内容占多少显示列。
    fn measure_line_cells(&self, row: usize) -> usize {
        self.line_chars(row).map(char_cells).sum()
    }

    /// 第 row 行末尾的换行符占几个**字符**。
    ///
    /// 我们只把 `\n` 当换行，所以除了最后一行，每一行都恰好以 1 个 `\n` 结尾。
    fn line_break_chars(&self, row: usize) -> usize {
        if row + 1 == self.rope.len_lines() {
            0
        } else {
            1
        }
    }

    /// 第 row 行的**内容**（不含行尾换行符）。
    fn line_content(&self, row: usize) -> String {
        match self.rope.get_line(row) {
            Some(line) => {
                let content_chars = line.len_chars().saturating_sub(self.line_break_chars(row));
                line.slice(..content_chars).to_string()
            }
            None => String::new(),
        }
    }

    /// 第 row 行**内容**的字符迭代器（不含行尾换行符）。
    ///
    /// 直接用 `RopeSlice::chars()` 迭代，**不**分配中间 `String`。
    /// 这一点很重要：每次按键、每帧渲染都会调到这里。
    fn line_chars(&self, row: usize) -> impl Iterator<Item = char> + '_ {
        let content_len = self.get_char_count(row);
        self.rope
            .get_line(row)
            .map(move |line| line.chars().take(content_len))
            .into_iter()
            .flatten()
    }

    /// 第 row 行内容在 rope 里的起始字符下标。
    fn line_content_start(&self, row: usize) -> usize {
        self.rope.line_to_char(row)
    }

    /// 第 row 行**内容**的字符区间（不含行尾换行符）。
    fn line_content_range(&self, row: usize) -> Range<usize> {
        let start = self.line_content_start(row);
        start..start + self.get_char_count(row)
    }

    // ---------- 行内容 / 长度 ----------

    /// 共有多少行（rope 保证至少有一行）
    pub fn get_line_count(&self) -> usize {
        self.rope.len_lines()
    }

    /// 取第 row 行（不存在返回 None）；**不含**行尾换行符。
    ///
    /// 返回 owned `String`：ropey 的 `RopeSlice` 无法保证借出一段连续的 `&str`
    /// （`RopeSlice::as_str()` 只在切片恰好连续时才返回 `Some`），
    /// 所以没法维持原来的 `Option<&str>`。调用点主要是渲染（每帧几行）与少量编辑逻辑，
    /// 这点分配可以忽略。
    pub fn get_line(&self, row: usize) -> Option<String> {
        self.rope.get_line(row)?;
        Some(self.line_content(row))
    }

    /// 第 row 行有多少个「字符」（**不含**行尾换行符）。
    ///
    /// 注意是字符数而不是字节数：`"你好"` 是 2 个字符、6 个字节。
    /// 编辑器内部坐标一律用字符数，中文才不会错位。
    pub fn get_char_count(&self, row: usize) -> usize {
        match self.rope.get_line(row) {
            Some(line) => line.len_chars().saturating_sub(self.line_break_chars(row)),
            None => 0,
        }
    }

    /// 返回第 row 行开头的原始缩进文本，保留空格/Tab 形式。
    fn get_leading_indent(&self, row: usize) -> String {
        self.line_chars(row)
            .take_while(|ch| ch.is_whitespace())
            .collect()
    }

    /// 第 row 行开头有多少个缩进字符（供 Enter 断行后放光标用）
    pub(crate) fn get_leading_indent_char_count(&self, row: usize) -> usize {
        self.line_chars(row)
            .take_while(|ch| ch.is_whitespace())
            .count()
    }

    // ---------- 显示列换算（按需扫描：rope 不管终端宽度） ----------

    /// 第 row 行前 col 个字符对应的显示列。
    pub(crate) fn get_cell_at_char(&self, row: usize, col: usize) -> usize {
        self.line_chars(row).take(col).map(char_cells).sum()
    }

    /// 第 row 行中，给定显示列所在的字符下标；落在宽字符中间时返回该字符下标。
    pub(crate) fn get_char_at_cell(&self, row: usize, cell: usize) -> usize {
        let mut start_cell = 0;
        let mut index = 0;
        for ch in self.line_chars(row) {
            let next = start_cell + char_cells(ch);
            if next > cell {
                break; // 落在第 index 个字符内部
            }
            start_cell = next;
            index += 1;
        }
        index
    }

    /// 根据显示列生成当前行可渲染的内容；宽字符被左边界切开时补空格。
    pub(crate) fn get_visible_text_from_cell(&self, row: usize, left: usize) -> String {
        if left == 0 {
            return self.line_content(row); // 绝大多数情况：没有横向滚动
        }
        let char_count = self.get_char_count(row);
        let char_index = self.get_char_at_cell(row, left);
        if char_index >= char_count {
            return String::new();
        }
        let start_cell = self.get_cell_at_char(row, char_index);
        if start_cell == left {
            return self.line_chars(row).skip(char_index).collect();
        }

        // 左边界切在宽字符中间：这半个字符没法画，补空格，从**下一个**字符开始画。
        // （若连下一个字符都没有，就只剩这些空格。）
        let cut_width = self.line_chars(row).nth(char_index).map_or(0, char_cells);
        let pad = " ".repeat(start_cell + cut_width - left);
        let next = char_index + 1;
        if next >= char_count {
            pad
        } else {
            pad + &self.line_chars(row).skip(next).collect::<String>()
        }
    }

    /// 当前文本中最宽的一行，单位是终端显示列。
    ///
    /// O(1)：读的是增量维护好的缓存 `max_cells`，不再遍历所有行。
    pub(crate) fn get_max_cell_count(&self) -> usize {
        self.max_cells
    }

    /// 重扫所有行、重算最大显示列数（仅在「变窄」把最宽行弄没了时才需要）。
    fn recompute_max_cells(&mut self) {
        self.max_cells = self.line_cells.iter().copied().max().unwrap_or(0);
    }

    /// 第 row 行内容变了：重新量它的宽度并维护 `max_cells`。
    fn refresh_line_cells(&mut self, row: usize) {
        if row >= self.line_cells.len() {
            return;
        }
        let old_cells = self.line_cells[row];
        let new_cells = self.measure_line_cells(row);
        Arc::make_mut(&mut self.line_cells)[row] = new_cells;

        // 变宽：直接刷新缓存（O(1)）。
        // 变窄：只有「原本并列/独自最宽」的行才可能拉低最大值，此时重扫（O(行数)）。
        if new_cells >= self.max_cells {
            self.max_cells = new_cells;
        } else if old_cells == self.max_cells {
            self.recompute_max_cells();
        }
    }

    // ---------- 整行 / 多行操作 ----------

    /// 把第 row 行的内容整体换成 `text`（长度可以不同）。
    fn replace_line_content(&mut self, row: usize, text: &str) {
        let range = self.line_content_range(row);
        self.rope.remove(range.clone());
        self.rope.insert(range.start, text);
        self.refresh_line_cells(row);
    }

    /// 删掉第 index 个字符（ropey 只有按区间删的 `remove`，包一层更直观）。
    fn remove_one_char(&mut self, index: usize) {
        self.rope.remove(index..index + 1);
    }

    /// 交换第 x 行与第 y 行；下标非法或两行相同返回 false（供 `:swap` 命令报告错误）
    pub fn swap_lines(&mut self, x: usize, y: usize) -> bool {
        if x == y || x >= self.get_line_count() || y >= self.get_line_count() {
            return false;
        }
        let (a, b) = if x < y { (x, y) } else { (y, x) };
        let text_a = self.line_content(a);
        let text_b = self.line_content(b);

        // 先改靠后的行：这样再改前面那行时，后面那行的下标不会被位移影响。
        // （行尾换行符不在「内容区间」里，所以会稳稳留在原地。）
        self.replace_line_content(b, &text_a);
        self.replace_line_content(a, &text_b);
        true
    }

    /// 删除从第 start 行开始的连续 count 行。
    ///
    /// 一次 `remove` 掉整段（含行尾换行符），下标不会因为「删一行而前移」导致错位。
    pub fn delete_lines(&mut self, start: usize, count: usize) -> bool {
        let line_count = self.get_line_count();
        let end = start.saturating_add(count);
        if start >= line_count || end > line_count {
            return false;
        }
        // 先记下「被删的行里是否包含最宽行」，删完再决定要不要重算缓存
        let removed_max = self.line_cells[start..end].contains(&self.max_cells);

        // 删到文档末尾时最后一行后面没有换行符，终点要取到 rope 结尾；
        // 同时要把**上一行行尾**那个换行符一起删掉，否则剩下的最后一行会多出一个空行。
        // （例：`"a\nb\nc"` 删掉 1、2 行，应剩 `"a"` 而不是 `"a\n"`。）
        let to = if end < line_count {
            self.rope.line_to_char(end)
        } else {
            self.rope.len_chars()
        };
        let from = if end == line_count && start > 0 {
            self.rope.line_to_char(start) - 1
        } else {
            self.rope.line_to_char(start)
        };
        self.rope.remove(from..to);

        Arc::make_mut(&mut self.line_cells).drain(start..end);
        if removed_max {
            self.recompute_max_cells();
        }
        true
    }

    /// 保证至少有一行（某些操作后行可能被删光）。
    ///
    /// rope 本身保证「空文档也有 1 行」，所以这里只需要让 `line_cells` 跟上。
    pub fn ensure_at_least_one_line(&mut self) {
        Arc::make_mut(&mut self.line_cells).resize(self.rope.len_lines(), 0);
        self.recompute_max_cells();
    }

    // ---------- 编辑原语（坐标都是「字符下标」） ----------

    /// 在第 row 行的第 col 个字符处插入 ch（col 可等于行字符数 = 行尾）
    pub fn insert_char(&mut self, row: usize, col: usize, ch: char) {
        if row >= self.get_line_count() {
            return;
        }
        let col = col.min(self.get_char_count(row));
        let index = self.line_content_start(row) + col;
        self.rope.insert_char(index, ch);
        self.refresh_line_cells(row);
    }

    /// 删除 (row, col) 之前的字符（Backspace）；当 col == 0 时合并到上一行
    pub fn delete_char_before(&mut self, row: usize, col: usize) {
        if row >= self.get_line_count() {
            return;
        }
        if col > 0 {
            let col = col.min(self.get_char_count(row));
            // 防御：行是空的时（col 被夹成 0）没有「前一个字符」可删。
            // 正常调用路径不会走到这里（App 会先 clamp 光标），
            // 但 Buffer 是公开 API，越界不该变成 `0 - 1` 的 panic。
            if col == 0 {
                return;
            }
            let index = self.line_content_start(row) + col - 1;
            self.remove_one_char(index);
            self.refresh_line_cells(row);
        } else if row > 0 {
            // col == 0：删掉上一行末尾的换行符，两行合并
            let newline_index = self.line_content_start(row) - 1;
            self.remove_one_char(newline_index);

            let removed_cells = self.line_cells[row];
            Arc::make_mut(&mut self.line_cells).remove(row);
            self.refresh_line_cells(row - 1);
            // 被并掉的那行若正好是最宽的，最宽值可能变小
            if removed_cells == self.max_cells {
                self.recompute_max_cells();
            }
        }
    }

    /// 删除 (row, col) 处的字符（Delete）；在行尾时与下一行合并
    pub fn delete_char_at(&mut self, row: usize, col: usize) {
        if row >= self.get_line_count() {
            return;
        }
        let line_len = self.get_char_count(row);
        let index = self.line_content_start(row) + col.min(line_len);
        if col < line_len {
            self.remove_one_char(index);
            self.refresh_line_cells(row);
        } else if row + 1 < self.get_line_count() {
            // 已在行尾：删掉行尾换行符，与下一行合并
            self.remove_one_char(index);

            let removed_cells = self.line_cells[row + 1];
            Arc::make_mut(&mut self.line_cells).remove(row + 1);
            self.refresh_line_cells(row);
            // 被并掉的那行若正好是最宽的，最宽值可能变小
            if removed_cells == self.max_cells {
                self.recompute_max_cells();
            }
        }
    }

    /// 在第 row 行的第 col 个字符处断行（Enter）：右侧内容成为新的一行，
    /// 并继承当前行的前导缩进。
    pub fn split_line_at(&mut self, row: usize, col: usize) {
        if row >= self.get_line_count() {
            return;
        }
        let col = col.min(self.get_char_count(row));
        let start = self.line_content_start(row);
        let content_len = self.get_char_count(row);
        let indent = self.get_leading_indent(row);
        let rest: String = self.line_chars(row).skip(col).collect();

        // 把行内容里 [col, 末尾) 这一段换成「换行 + 缩进 + 右半段」。
        // 行尾原有的换行符不在这个区间里，所以会留在新行的末尾，位置正确。
        self.rope.remove(start + col..start + content_len);
        self.rope.insert(start + col, &format!("\n{indent}{rest}"));

        // 先把新行的格子数插进列表，再重算 row：
        // 万一 row 变窄触发了重扫，此刻新行已经在列表里，不会漏算。
        Arc::make_mut(&mut self.line_cells).insert(row + 1, 0);
        self.refresh_line_cells(row + 1);
        self.refresh_line_cells(row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内部不变量：`line_cells` 必须与 rope 的行数对齐，`max_cells` 必须等于最大值。
    ///
    /// 这类「跟着正文维护的缓存」最容易在**行数变化**的操作里失配，
    /// 所以每个涉及增删行的测试都顺手验一次。
    fn assert_consistent(b: &Buffer) {
        assert_eq!(
            b.line_cells.len(),
            b.rope.len_lines(),
            "line_cells 与行数失配"
        );
        assert_eq!(
            b.max_cells,
            b.line_cells.iter().copied().max().unwrap_or(0),
            "max_cells 缓存失准"
        );
    }

    #[test]
    fn from_str_keeps_trailing_empty_line() {
        let b = Buffer::from_str("a\nbb\n");
        assert_eq!(b.get_line_count(), 3);
        assert_eq!(b.get_line(2).as_deref(), Some(""));
        assert_consistent(&b);
    }

    #[test]
    fn empty_content_is_one_empty_line() {
        let b = Buffer::from_str("");
        assert_eq!(b.get_line_count(), 1);
        assert_eq!(b.get_line(0).as_deref(), Some(""));
        assert_consistent(&b);
    }

    #[test]
    fn char_len_counts_chars_not_bytes() {
        let b = Buffer::from_str("你好世界");
        assert_eq!(b.get_char_count(0), 4);
    }

    #[test]
    fn insert_char_handles_multibyte() {
        let mut b = Buffer::from_str("你好世界");
        b.insert_char(0, 2, 'X');
        assert_eq!(b.get_line(0).as_deref(), Some("你好X世界"));
        assert_consistent(&b);
    }

    #[test]
    fn insert_char_appends_at_end_of_line() {
        let mut b = Buffer::from_str("你好");
        b.insert_char(0, 2, '!'); // col == 行字符数 → 追加到末尾
        assert_eq!(b.get_line(0).as_deref(), Some("你好!"));
    }

    #[test]
    fn layout_cache_maps_chars_and_cells() {
        let b = Buffer::from_str("a你好b");
        assert_eq!(b.get_char_count(0), 4);
        assert_eq!(b.get_cell_at_char(0, 0), 0);
        assert_eq!(b.get_cell_at_char(0, 1), 1);
        assert_eq!(b.get_cell_at_char(0, 2), 3);
        assert_eq!(b.get_cell_at_char(0, 4), 6);
        assert_eq!(b.get_char_at_cell(0, 0), 0);
        assert_eq!(b.get_char_at_cell(0, 2), 1); // 落在“你”内部，归到“你”
        assert_eq!(b.get_char_at_cell(0, 3), 2);
        assert_eq!(b.get_max_cell_count(), 6);
    }

    #[test]
    fn layout_cache_updates_after_editing() {
        let mut b = Buffer::from_str("ab");
        b.insert_char(0, 1, '你');
        assert_eq!(b.get_cell_at_char(0, 2), 3);
        b.delete_char_before(0, 2);
        assert_eq!(b.get_line(0).as_deref(), Some("ab"));
        assert_eq!(b.get_cell_at_char(0, 2), 2);
        b.split_line_at(0, 1);
        assert_eq!(b.get_char_count(0), 1);
        assert_eq!(b.get_char_count(1), 1);
        assert_eq!(b.get_cell_at_char(1, 1), 1);
        assert_consistent(&b);
    }

    #[test]
    fn max_cell_count_cache_tracks_growth_and_shrink() {
        let mut b = Buffer::from_str("ab\ncdef");
        assert_eq!(b.get_max_cell_count(), 4);

        // 变宽：缓存直接跟上
        b.insert_char(0, 2, '你'); // "ab你" = 4 列
        assert_eq!(b.get_max_cell_count(), 4);
        b.insert_char(0, 3, '好'); // "ab你好" = 6 列，成为新的最宽行
        assert_eq!(b.get_max_cell_count(), 6);

        // 变窄：删掉最宽行里的字符，缓存必须跟着变小
        b.delete_char_before(0, 4);
        assert_eq!(b.get_max_cell_count(), 4);

        // 变窄到比另一行还短
        b.delete_char_before(0, 3);
        assert_eq!(b.get_max_cell_count(), 4); // 仍是 "cdef" 的 4 列
        b.delete_char_before(0, 2);
        assert_eq!(b.get_max_cell_count(), 4);
        assert_eq!(b.get_line(0).as_deref(), Some("a"));
    }

    #[test]
    fn max_cell_count_cache_tracks_line_removal_and_merge() {
        // delete_lines 删掉最宽行
        let mut b = Buffer::from_str("a\nbbbbbb\ncc");
        assert_eq!(b.get_max_cell_count(), 6);
        b.delete_lines(1, 1);
        assert_eq!(b.get_max_cell_count(), 2);
        assert_consistent(&b);
        // Backspace 行首合并：被并掉的行是最宽行
        let mut b = Buffer::from_str("a\nbbbbbb");
        b.delete_char_before(1, 0); // 合并为 "abbbbbb"
        assert_eq!(b.get_line_count(), 1);
        assert_eq!(b.get_max_cell_count(), 7);
        assert_consistent(&b);

        // Delete 行尾合并
        let mut b = Buffer::from_str("aa\nbbbbbb");
        b.delete_char_at(0, 2); // 合并为 "aabbbbbb"
        assert_eq!(b.get_line_count(), 1);
        assert_eq!(b.get_max_cell_count(), 8);
        assert_consistent(&b);
    }

    #[test]
    fn max_cell_count_cache_tracks_split() {
        // 在最宽行中间断行：左边变短、右边带缩进，最大值要重新算准
        let mut b = Buffer::from_str("aa\nbbbbbb");
        b.split_line_at(1, 3);
        assert_eq!(b.get_line(1).as_deref(), Some("bbb"));
        assert_eq!(b.get_line(2).as_deref(), Some("bbb"));
        assert_eq!(b.get_max_cell_count(), 3);
        assert_consistent(&b);

        // 在行首断行时新行会继承缩进，长度可能超过原行
        let mut b = Buffer::from_str("    abc");
        b.split_line_at(0, 0);
        assert_eq!(b.get_line(0).as_deref(), Some(""));
        assert_eq!(b.get_line(1).as_deref(), Some("        abc"));
        assert_eq!(b.get_max_cell_count(), 11);
    }

    #[test]
    fn break_line_splits_row() {
        let mut b = Buffer::from_str("abcd");
        b.split_line_at(0, 2);
        assert_eq!(b.get_line_count(), 2);
        assert_eq!(b.get_line(0).as_deref(), Some("ab"));
        assert_eq!(b.get_line(1).as_deref(), Some("cd"));
    }

    #[test]
    fn delete_char_before_merges_lines_at_col_zero() {
        let mut b = Buffer::from_str("hello\nworld");
        b.delete_char_before(1, 0);
        assert_eq!(b.get_line_count(), 1);
        assert_eq!(b.get_line(0).as_deref(), Some("helloworld"));
        assert_consistent(&b);
    }

    #[test]
    fn delete_char_at_removes_one_char() {
        let mut b = Buffer::from_str("abcd");
        b.delete_char_at(0, 1);
        assert_eq!(b.get_line(0).as_deref(), Some("acd"));
    }

    #[test]
    fn delete_char_at_end_of_line_joins_next() {
        let mut b = Buffer::from_str("ab\ncd");
        b.delete_char_at(0, 2); // 行尾，应与下一行合并
        assert_eq!(b.get_line_count(), 1);
        assert_eq!(b.get_line(0).as_deref(), Some("abcd"));
    }

    #[test]
    fn swap_lines_works_and_reports_invalid() {
        let mut b = Buffer::from_str("a\nb\nc");
        assert!(b.swap_lines(0, 2));
        assert_eq!(b.get_line(0).as_deref(), Some("c"));
        assert_eq!(b.get_line(2).as_deref(), Some("a"));
        assert!(!b.swap_lines(0, 99));
        assert_consistent(&b);
    }

    #[test]
    fn swap_lines_handles_different_lengths() {
        // 行长不同时，行尾的换行符必须留在原地，不能跟着内容一起被搬走
        let mut b = Buffer::from_str("short\n\nlonger");
        assert!(b.swap_lines(0, 2));
        assert_eq!(b.get_line(0).as_deref(), Some("longer"));
        assert_eq!(b.get_line(1).as_deref(), Some(""));
        assert_eq!(b.get_line(2).as_deref(), Some("short"));
        assert_eq!(b.to_string(), "longer\n\nshort");
        assert_consistent(&b);
    }

    #[test]
    fn delete_lines_removes_trailing_newline_too() {
        let mut b = Buffer::from_str("a\nb\nc\nd");
        assert!(b.delete_lines(1, 2)); // 删掉 b、c
        assert_eq!(b.to_string(), "a\nd");
        assert_eq!(b.get_line_count(), 2);
        assert_consistent(&b);

        // 删到末尾：最后一行后面没有换行符，不能多删
        let mut b = Buffer::from_str("a\nb\nc");
        assert!(b.delete_lines(1, 2));
        assert_eq!(b.to_string(), "a");
        assert_eq!(b.get_line_count(), 1);
        assert_consistent(&b);
    }

    #[test]
    fn delete_all_lines_leaves_one_empty_line() {
        let mut b = Buffer::from_str("a\nb");
        assert!(b.delete_lines(0, 2));
        b.ensure_at_least_one_line();
        assert_eq!(b.get_line_count(), 1);
        assert_eq!(b.get_line(0).as_deref(), Some(""));
        assert_eq!(b.to_string(), "");
        assert_consistent(&b);
    }

    #[test]
    fn roundtrip_buffer_to_string() {
        // 末尾换行会被保留（对应缓冲里最后那个空行），保存不丢字节
        let b = Buffer::from_str("你好\nworld\n");
        assert_eq!(b.to_string(), "你好\nworld\n");
        // 无末尾换行的文件同样能原样往返
        let b = Buffer::from_str("你好\nworld");
        assert_eq!(b.to_string(), "你好\nworld");
        // 空文件
        assert_eq!(Buffer::from_str("").to_string(), "");
    }

    #[test]
    fn line_splitting_matches_split_on_newline_only() {
        // 我们关掉了 ropey 的 unicode_lines / cr_lines：只把 '\n' 当换行，
        // 所以 "\r\n" 里的 '\r' 会留在行内容里（与旧实现 split('\n') 完全一致）。
        let b = Buffer::from_str("a\r\nb");
        assert_eq!(b.get_line_count(), 2);
        assert_eq!(b.get_line(0).as_deref(), Some("a\r"));
        assert_eq!(b.get_line(1).as_deref(), Some("b"));
        assert_eq!(b.to_string(), "a\r\nb", "字节仍然原样往返");
    }

    #[test]
    fn horizontal_scroll_slicing_matches_wide_chars() {
        let b = Buffer::from_str("a你好b");
        assert_eq!(b.get_visible_text_from_cell(0, 0), "a你好b");
        assert_eq!(b.get_visible_text_from_cell(0, 1), "你好b");
        // 左边界切在“你”中间（列 2）：补 1 个空格
        assert_eq!(b.get_visible_text_from_cell(0, 2), " 好b");
        assert_eq!(b.get_visible_text_from_cell(0, 3), "好b");
        // 超出行尾 → 空
        assert_eq!(b.get_visible_text_from_cell(0, 6), "");
    }

    /// 回归：`delete_char_before` 拿到越界的 (row, col) 时不该 panic。
    ///
    /// 以前是 `line_content_start(row) + col - 1`：空行上 col 被夹成 0，
    /// 于是算出 `0 - 1` —— debug 下直接 underflow panic。
    /// （正常调用路径不会这样传，但 Buffer 是公开 API，越界输入不该炸。）
    #[test]
    fn delete_char_before_out_of_range_is_a_noop() {
        let mut b = Buffer::from_str("");
        b.delete_char_before(0, 5); // 空行 + col 越界
        assert_eq!(b.to_string(), "");
        assert_consistent(&b);

        b.delete_char_before(9, 1); // row 越界
        assert_eq!(b.to_string(), "");
        assert_consistent(&b);

        // 相邻的兄弟操作也顺手确认一下不炸
        let mut b = Buffer::from_str("ab");
        b.delete_char_at(9, 9);
        b.insert_char(9, 9, 'x');
        assert_eq!(b.to_string(), "ab");
        assert_consistent(&b);
    }
}
