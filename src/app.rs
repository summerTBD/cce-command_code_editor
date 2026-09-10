//! 编辑器「纯状态」模块 —— app.rs 的职责边界
//!
//! 这个文件**只**负责两件事：
//! 1. 保存编辑器的全部状态（模式、文本缓冲、光标、视口、命令输入……）
//! 2. 提供「修改自己状态」的纯方法（编辑文本、移动光标、钳制越界等）
//!
//! 它**绝不**碰：终端绘制（ui.rs）、键盘事件读取（event.rs）、
//! 按键→动作的分发与副作用（update.rs，如读写文件、退出）。
//!
//! 好处：状态逻辑独立、无副作用，可直接用 `cargo test` 验证。

/// 编辑器模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditorMode {
    /// 只读浏览（默认），类似 Vim 的 normal 模式
    #[default]
    ReadOnly,
    /// 编辑模式，按键直接写入文本
    Edit,
    /// 命令模式，输入将被收集为命令字符串（如 `:w`、`:swap`）
    Command,
}

/// 默认的一次 Tab 缩进空格数（可用 `:set tabwidth N` 修改）
const DEFAULT_TAB_WIDTH: usize = 4;

/// 文本缓冲 —— 采用「行模型」：每一行是一个 String。
///
/// 行号、`:swap line x y`、按行滚动这些需求在行模型下都很容易实现。
/// 将来若打开超大文件卡顿，可换成 GapBuffer / Rope，但**只需改这个结构体内部**
/// 并保持对外方法不变，其它文件无需改动。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Buffer {
    /// 所有文本行，不含行尾换行符；始终保证至少有一行
    lines: Vec<String>,
}

impl Buffer {
    /// 把一段文本切分成行。
    ///
    /// - `"abc\n"` → `["abc", ""]`（文件末尾换行时仍留一个可停留的空行）
    /// - `""`      → `[""]`（空文件也有一行，方便光标移动）
    pub fn from_str(content: &str) -> Self {
        Self {
            lines: content.split('\n').map(String::from).collect(),
        }
    }

    /// 共有多少行
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// 取第 row 行（不存在返回 None）
    pub fn line(&self, row: usize) -> Option<&str> {
        self.lines.get(row).map(String::as_str)
    }

    /// 第 row 行有多少个「字符」。
    ///
    /// 注意是字符数而不是字节数：`"你好"` 是 2 个字符、6 个字节。
    /// 编辑器内部坐标一律用字符数，中文才不会错位。
    pub fn char_len(&self, row: usize) -> usize {
        self.line(row).map_or(0, |l| l.chars().count())
    }

    /// 交换第 x 行与第 y 行；下标非法返回 false（供 `:swap` 命令报告错误）
    pub fn swap_lines(&mut self, x: usize, y: usize) -> bool {
        if x == y || x >= self.lines.len() || y >= self.lines.len() {
            return false;
        }
        self.lines.swap(x, y);
        true
    }

    /// 删除第 x 行；下标非法返回 false（供命令层报告错误）。
    ///
    /// 注意：`remove(x)` 之后，原来 x 后面的所有行会整体前移一格。
    /// 所以如果要连续删多行，**不能**删完再按“原来的 start+1”去删——
    /// 那样会删到原本的第 2 行。删整段请用下面的 `delete_lines`。
    pub fn delete_line(&mut self, x: usize) -> bool {
        if x >= self.lines.len() {
            return false;
        }
        self.lines.remove(x);
        true
    }

    /// 删除从第 start 行开始的连续 count 行。
    ///
    /// 用 `drain(start..end)` 一次删掉一整段，下标不会因为“删一行而前移”导致错位。
    pub fn delete_lines(&mut self, start: usize, count: usize) -> bool {
        let end = start.saturating_add(count);
        if start >= self.lines.len() || end > self.lines.len() {
            return false;
        }
        self.lines.drain(start..end);
        true
    }

    /// 把缓冲拼回整段文本（保存文件时用），行间以 `\n` 连接
    pub fn to_string(&self) -> String {
        self.lines.join("\n")
    }

    /// 在第 row 行的第 col 个字符处插入 ch（col 是字符下标，可等于行字符数=末尾）
    pub fn insert_char(&mut self, row: usize, col: usize, ch: char) {
        if row >= self.lines.len() {
            return;
        }
        let byte = byte_idx_for_char(&self.lines[row], col);
        self.lines[row].insert(byte, ch);
    }

    /// 删除 (row, col) 之前的字符（Backspace）；当 col == 0 时合并到上一行
    pub fn delete_char_before(&mut self, row: usize, col: usize) {
        if row >= self.lines.len() {
            return;
        }
        if col > 0 {
            let byte = byte_idx_for_char(&self.lines[row], col - 1);
            self.lines[row].remove(byte);
        } else if row > 0 {
            let removed = self.lines.remove(row);
            self.lines[row - 1].push_str(&removed);
        }
    }

    /// 删除 (row, col) 处的字符（Delete）；在行尾时与下一行合并
    pub fn delete_char_at(&mut self, row: usize, col: usize) {
        if row >= self.lines.len() {
            return;
        }
        let line_len = self.lines[row].len();
        let byte = byte_idx_for_char(&self.lines[row], col);
        if byte < line_len {
            self.lines[row].remove(byte);
        } else if row + 1 < self.lines.len() {
            let next = self.lines.remove(row + 1);
            self.lines[row].push_str(&next);
        }
    }

    /// 在第 row 行的第 col 个字符处断行（Enter）：右侧内容成为新的一行
    pub fn break_line(&mut self, row: usize, col: usize) {
        if row >= self.lines.len() {
            return;
        }
        let byte = byte_idx_for_char(&self.lines[row], col);
        let right = self.lines[row].split_off(byte);
        self.lines.insert(row + 1, right);
    }

    /// 保证至少有一行（某些操作后行可能被删光）
    pub fn ensure_nonempty(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
    }
}

/// 光标位置；(row, col) 都从 0 开始，col 是「字符数」而不是字节数
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
}

/// 视口 —— 可视区左上角所在的 (行, 列)，用来做滚动
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Viewport {
    /// 最上面显示的是第几行（向下滚动的基础）
    pub top: usize,
    /// 最左边显示的是第几个字符（长行横向滚动的基础）
    pub left: usize,
}

/// 编辑器整体状态。字段对外公开（ui.rs / update.rs 需要读它们来渲染、分发），
/// 但**修改状态请走下面这些方法**，以保证光标等内部不变量不被破坏。
pub struct App {
    pub mode: EditorMode,
    pub buffer: Buffer,
    pub cursor: Cursor,
    pub viewport: Viewport,
    /// 命令模式时 `:` 之后的输入内容
    pub command_input: String,
    /// 底部提示条信息（如 `-- 只读模式 --`、`已保存`）
    pub status_message: String,
    /// 是否有未保存的修改（`:q` 前提示、标题栏显示 `*`）
    pub dirty: bool,
    /// 当前打开的文件路径（None = 新建文件）
    pub file_path: Option<String>,
    /// 是否显示行号（`:set number` 开关）
    pub show_line_numbers: bool,
    /// 一次 Tab 插入的空格数（`:set tabwidth N` 可改）
    pub tab_width: usize,
}

impl App {
    /// 新建一个空文件编辑器
    pub fn new() -> Self {
        Self::from_content(None, String::new())
    }

    /// 打开文件内容时调用；file_path 传 None 表示新建
    pub fn from_content(file_path: Option<String>, content: String) -> Self {
        Self {
            mode: EditorMode::ReadOnly,
            buffer: Buffer::from_str(&content),
            cursor: Cursor::default(),
            viewport: Viewport::default(),
            command_input: String::new(),
            status_message: String::new(),
            dirty: false,
            file_path,
            show_line_numbers: false,
            tab_width: DEFAULT_TAB_WIDTH,
        }
    }

    // ---------- 模式切换（供 update.rs 调用） ----------

    pub fn set_mode(&mut self, mode: EditorMode) {
        self.mode = mode;
    }

    /// 进入命令模式（按下 `:`）：先清掉旧的命令输入
    ///
    /// 注意：这个函数不做模式合法性检查。按键分发（update.rs）负责保证
    /// 「只有只读模式下按 `:` 才调用它」，编辑模式下 `:` 会走 `type_char` 当作普通字符。
    pub fn enter_command(&mut self) {
        self.mode = EditorMode::Command;
        self.command_input.clear();
    }

    /// 取消命令（Esc）：回到只读模式
    pub fn cancel_command(&mut self) {
        self.mode = EditorMode::ReadOnly;
        self.command_input.clear();
    }

    // ---------- 光标移动与滚动 ----------

    /// 以 (dr, dc) 增量移动光标。
    ///
    /// - 上/下（dr≠0）：只改行号，越界钳制到首/末行；col 截断到目标行的长度。
    /// - 左/右（dc≠0）：逐格移动；在行尾继续按 → 会换到下一行行首，
    ///   在行首继续按 ← 会回到上一行行尾；首行行首 / 末行行尾则停在原地。
    ///
    /// 说明：「记住上一行的目标列」属于锦上添花，留作以后优化。
    pub fn move_cursor(&mut self, dr: isize, dc: isize) {
        self.clamp_cursor();
        let row_count = self.buffer.line_count() as isize;
        let mut row = self.cursor.row as isize;
        let mut col = self.cursor.col as isize;

        // 上 / 下：行号越界就钳制到首行 / 末行
        if dr != 0 {
            row = (row + dr).clamp(0, row_count - 1);
        }

        // 左 / 右：按步长逐格移动，跨过行边界就换到相邻行
        for _ in 0..dc.abs() {
            if dc > 0 {
                let line_len = self.buffer.char_len(row as usize) as isize;
                if col < line_len {
                    col += 1; // 行内右移一格
                } else if row < row_count - 1 {
                    row += 1; // 行尾 → 下一行行首
                    col = 0;
                }
                // 末行行尾：原地不动
            } else {
                if col > 0 {
                    col -= 1; // 行内左移一格
                } else if row > 0 {
                    row -= 1; // 行首 → 上一行行尾
                    col = self.buffer.char_len(row as usize) as isize;
                }
                // 首行行首：原地不动
            }
        }

        // 垂直移动后，把 col 截断到目标行长度（目标行更短时）
        if dr != 0 {
            let line_len = self.buffer.char_len(row as usize) as isize;
            col = col.min(line_len);
        }

        self.cursor.row = row as usize;
        self.cursor.col = col as usize;
    }

    /// 把越界的光标钳制回合法范围（任何编辑操作后都应调用一次做保险）
    pub fn clamp_cursor(&mut self) {
        self.buffer.ensure_nonempty();
        let max_row = self.buffer.line_count() - 1;
        self.cursor.row = self.cursor.row.min(max_row);
        let max_col = self.buffer.char_len(self.cursor.row);
        self.cursor.col = self.cursor.col.min(max_col);
    }

    /// 保证光标落在可视区内，必要时滚动视口。
    ///
    /// `view_height` / `view_width` 是「文本区」能显示的行/列数（不含底部命令栏），
    /// 由调用方（ui/update）从终端布局尺寸算出后传入，因此 app.rs 不用碰终端。
    pub fn ensure_cursor_visible(&mut self, view_height: usize, view_width: usize) {
        if view_height == 0 || view_width == 0 {
            return;
        }
        if self.cursor.row < self.viewport.top {
            self.viewport.top = self.cursor.row;
        } else if self.cursor.row >= self.viewport.top + view_height {
            self.viewport.top = self.cursor.row + 1 - view_height;
        }
        if self.cursor.col < self.viewport.left {
            self.viewport.left = self.cursor.col;
        } else if self.cursor.col >= self.viewport.left + view_width {
            self.viewport.left = self.cursor.col + 1 - view_width;
        }
    }

    // ---------- 文本编辑（update.rs 在 Edit 模式下调用） ----------

    /// 在光标处插入一个普通字符，光标右移一格
    pub fn type_char(&mut self, ch: char) {
        self.buffer
            .insert_char(self.cursor.row, self.cursor.col, ch);
        self.cursor.col += 1;
        self.dirty = true;
    }

    /// Backspace：删掉光标前一个字符；在行首则把当前行并入上一行
    pub fn backspace(&mut self) {
        if self.cursor.col > 0 {
            self.buffer
                .delete_char_before(self.cursor.row, self.cursor.col);
            self.cursor.col -= 1;
        } else if self.cursor.row > 0 {
            let prev_len = self.buffer.char_len(self.cursor.row - 1);
            self.buffer.delete_char_before(self.cursor.row, 0);
            self.cursor.row -= 1;
            self.cursor.col = prev_len;
        } else {
            return;
        }
        self.dirty = true;
    }

    /// Delete：删掉光标「右边」紧邻的字符（即光标块盖住的那个字符），光标本身不动；
    /// 若光标已在行尾（右边没有字符），则把下一行拼到本行行尾
    pub fn delete_forward(&mut self) {
        let line_len = self.buffer.char_len(self.cursor.row);
        if self.cursor.col < line_len || self.cursor.row + 1 < self.buffer.line_count() {
            self.buffer.delete_char_at(self.cursor.row, self.cursor.col);
            self.dirty = true;
        }
    }

    /// Enter：在光标处断行，光标移到新行开头
    pub fn insert_newline(&mut self) {
        self.buffer.break_line(self.cursor.row, self.cursor.col);
        self.cursor.row += 1;
        self.cursor.col = 0;
        self.dirty = true;
    }

    // ---------- 状态栏 ----------

    /// 设置底部提示信息
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = msg.into();
    }
}

/// 把「字符下标」换算成「字节下标」。
///
/// 因为 String 是 UTF-8，`String::insert/remove` 等 API 只接受字节下标；
/// 编辑器坐标却是字符下标，所以必须在这两者之间转换。
/// 当 col 超过字符串字符数时返回末尾字节下标（即追加位置）。
fn byte_idx_for_char(s: &str, col: usize) -> usize {
    s.char_indices()
        .nth(col)
        .map(|(byte, _)| byte)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_keeps_trailing_empty_line() {
        let b = Buffer::from_str("a\nbb\n");
        assert_eq!(b.line_count(), 3);
        assert_eq!(b.line(2), Some(""));
    }

    #[test]
    fn empty_content_is_one_empty_line() {
        let b = Buffer::from_str("");
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.line(0), Some(""));
    }

    #[test]
    fn char_len_counts_chars_not_bytes() {
        let b = Buffer::from_str("你好世界");
        assert_eq!(b.char_len(0), 4);
    }

    #[test]
    fn insert_char_handles_multibyte() {
        let mut b = Buffer::from_str("你好世界");
        b.insert_char(0, 2, 'X');
        assert_eq!(b.line(0), Some("你好X世界"));
    }

    #[test]
    fn insert_char_appends_at_end_of_line() {
        let mut b = Buffer::from_str("你好");
        b.insert_char(0, 2, '!'); // col == 行字符数 → 追加到末尾
        assert_eq!(b.line(0), Some("你好!"));
    }

    #[test]
    fn break_line_splits_row() {
        let mut b = Buffer::from_str("abcd");
        b.break_line(0, 2);
        assert_eq!(b.line_count(), 2);
        assert_eq!(b.line(0), Some("ab"));
        assert_eq!(b.line(1), Some("cd"));
    }

    #[test]
    fn delete_char_before_merges_lines_at_col_zero() {
        let mut b = Buffer::from_str("hello\nworld");
        b.delete_char_before(1, 0);
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.line(0), Some("helloworld"));
    }

    #[test]
    fn delete_char_at_removes_one_char() {
        let mut b = Buffer::from_str("abcd");
        b.delete_char_at(0, 1);
        assert_eq!(b.line(0), Some("acd"));
    }

    #[test]
    fn delete_char_at_end_of_line_joins_next() {
        let mut b = Buffer::from_str("ab\ncd");
        b.delete_char_at(0, 2); // 行尾，应与下一行合并
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.line(0), Some("abcd"));
    }

    #[test]
    fn swap_lines_works_and_reports_invalid() {
        let mut b = Buffer::from_str("a\nb\nc");
        assert!(b.swap_lines(0, 2));
        assert_eq!(b.line(0), Some("c"));
        assert_eq!(b.line(2), Some("a"));
        assert!(!b.swap_lines(0, 99));
    }

    #[test]
    fn move_cursor_clamps_col_to_short_line() {
        let mut app = App::from_content(None, "hello\nhi".to_string());
        app.cursor = Cursor { row: 0, col: 5 };
        app.move_cursor(1, 0); // 从 5 字符长的行下移到 2 字符长的行
        assert_eq!((app.cursor.row, app.cursor.col), (1, 2));
    }

    #[test]
    fn move_cursor_stops_at_file_edges() {
        let mut app = App::from_content(None, "a\nb".to_string());
        app.move_cursor(-1, -1); // 已在左上角，应原地不动
        assert_eq!((app.cursor.row, app.cursor.col), (0, 0));
        app.move_cursor(100, 100); // 越界应钳制到右下角
        assert_eq!((app.cursor.row, app.cursor.col), (1, 1));
    }

    #[test]
    fn move_cursor_right_wraps_to_next_line() {
        let mut app = App::from_content(None, "ab\ncd".to_string());
        app.cursor = Cursor { row: 0, col: 2 }; // 第 1 行行尾
        app.move_cursor(0, 1); // → 应换到第 2 行行首
        assert_eq!((app.cursor.row, app.cursor.col), (1, 0));
    }

    #[test]
    fn move_cursor_left_wraps_to_prev_line() {
        let mut app = App::from_content(None, "ab\ncd".to_string());
        app.cursor = Cursor { row: 1, col: 0 }; // 第 2 行行首
        app.move_cursor(0, -1); // ← 应回到第 1 行行尾
        assert_eq!((app.cursor.row, app.cursor.col), (0, 2));
    }

    #[test]
    fn move_cursor_right_stays_at_last_line_end() {
        let mut app = App::from_content(None, "ab\ncd".to_string());
        app.cursor = Cursor { row: 1, col: 2 }; // 末行行尾，再 → 应原地不动
        app.move_cursor(0, 1);
        assert_eq!((app.cursor.row, app.cursor.col), (1, 2));
    }

    #[test]
    fn ensure_cursor_visible_scrolls_viewport() {
        let mut app = App::from_content(None, "a\nb\nc\nd".to_string());
        app.cursor = Cursor { row: 3, col: 0 };
        app.ensure_cursor_visible(2, 80); // 可视区只有 2 行
        assert_eq!(app.viewport.top, 2); // 应向下滚到能看见第 3 行
    }

    #[test]
    fn typing_marks_dirty_and_advances_cursor() {
        let mut app = App::new();
        app.type_char('a');
        assert!(app.dirty);
        assert_eq!(app.buffer.line(0), Some("a"));
        assert_eq!(app.cursor.col, 1);
    }

    #[test]
    fn backspace_joins_rows_via_app() {
        let mut app = App::from_content(None, "ab\ncd".to_string());
        app.cursor = Cursor { row: 1, col: 0 };
        app.backspace(); // 行首 Backspace → 并入上一行末尾
        assert_eq!(app.buffer.line_count(), 1);
        assert_eq!(app.buffer.line(0), Some("abcd"));
        assert_eq!(app.cursor.row, 0);
        assert_eq!(app.cursor.col, 2);
    }

    #[test]
    fn roundtrip_buffer_to_string() {
        // 末尾换行会被保留（对应缓冲里最后那个空行），保存不丢字节
        let app = App::from_content(None, "你好\nworld\n".to_string());
        assert_eq!(app.buffer.to_string(), "你好\nworld\n");
        // 无末尾换行的文件同样能原样往返
        let app2 = App::from_content(None, "你好\nworld".to_string());
        assert_eq!(app2.buffer.to_string(), "你好\nworld");
    }
}
