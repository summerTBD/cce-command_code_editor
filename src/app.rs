//! 编辑器状态 —— `App` 及其坐标类型。
//!
//! 这个文件负责两件事：
//! 1. 保存编辑器的全部状态（模式、文本缓冲、光标、视口、命令输入……）
//! 2. 提供「修改自己状态」的纯方法（编辑文本、移动光标、钳制越界等）
//!
//! 它**绝不**碰：终端绘制（ui.rs）、键盘事件读取（event.rs）、
//! 按键→动作的分发与副作用（update.rs，如读写文件、退出）。
//!
//! 好处：状态逻辑独立、无副作用，可直接用 `cargo test` 验证。
//!
//! 周边模块：文本存储与编辑原语在 [`crate::buffer`]，撤销栈在 [`crate::undo`]，
//! 用户偏好（行号 / Tab 宽度 / 滚动边距）在 [`crate::config`]。
//! 这个文件只保留「编辑器本身」的状态与语义。

use std::path::{Path, PathBuf};

use crate::buffer::Buffer;
use crate::config::Config;
use crate::documents::DocumentList;
use crate::undo::{DEFAULT_UNDO_LIMIT, EditKind, Snapshot, UndoStack};

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
    /// 外部命令模式：输入**原样**交给系统的 shell，编辑器的语法在那里停下
    ///
    /// 它跟 [`EditorMode::Command`] 长得很像（都是底部收集一行），差别**只有回车**：
    /// 命令模式把这一行当**编辑器的话**解析；外部命令模式把终端**让出去**、
    /// 整行原样交给 shell。
    ///
    /// 为什么在**按键**这一层就分开，而不是做成 `:!cmd` 那种命令前缀：
    /// 那样是「一个入口两种含义」（命令名的位置又挂了个「这行归 shell」）。
    /// 分开之后，分类发生在**按哪个键**那一刻 —— `commands.rs` 一行都不用改。
    External,
}

// 用户偏好的默认值（DEFAULT_TAB_WIDTH 等）住在 config.rs —— 那里才是它们的家。
// 依赖方向是 app.rs → config.rs（单向），所以常量不能定义在这个文件里。

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
    /// 最左边显示的是「从行首往右数第几列」（显示格数，中文算 2 格）
    pub left: usize,
}

/// 当前这块 buffer 装的是什么东西。
///
/// 为什么不用一堆 bool（`is_directory_listing`、`is_special`……）：种类是**互斥**的，
/// 用枚举能让「一个文档只能是其中一种」由类型系统保证；以后要加新种类，
/// 所有 `match` 都会被编译器逼着处理到（bool 做不到这一点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DocumentKind {
    /// 普通文件，也可能是还没命名的新文件：`file_path` 是它的路径，`:w` 可以写回
    #[default]
    File,
    /// 目录的子项列表：只读的浏览视图，**不能保存**；Enter 可以「进入」光标下的条目
    DirectoryListing,
}

/// 编辑器整体状态。字段对外公开（ui.rs / update.rs 需要读它们来渲染、分发），
/// 但**修改状态请走下面这些方法**，以保证光标等内部不变量不被破坏。
pub struct App {
    pub mode: EditorMode,
    pub buffer: Buffer,
    pub cursor: Cursor,
    pub viewport: Viewport,
    /// 命令模式 / 外部命令模式时，底部那一行收集到的输入
    pub command_input: String,
    /// 底部提示条信息（如 `-- 只读模式 --`、`已保存`）
    pub status_message: String,
    /// 是否有未保存的修改（`:q` 前提示、标题栏显示 `*`）
    pub dirty: bool,
    /// 当前打开的文件路径（None = 新建文件）
    pub file_path: Option<String>,
    /// 用户偏好（行号 / Tab 宽度 / 滚动边距），访问方式如 `app.config.tab_width`。
    ///
    /// 启动时由 main.rs 从配置文件读好后传进来；`:set` 只改内存里的这份，
    /// **不写回文件**（想永久生效请直接改配置文件）。
    pub config: Config,
    /// 配置文件是从哪个路径读来的（`None` = 没找到，全用默认值）。
    /// 纯展示用（`:config path`），不参与任何逻辑。
    pub config_path: Option<PathBuf>,
    /// 打开过的文档列表 + 当前位置（`q` 返回上一级、`:back`/`:next`、`:ls`）。
    ///
    /// 由 main.rs 在**打开成功之后**调 `documents.remember(path)` 维护 ——
    /// 读盘失败时不能记，否则 `q` 会「返回」到一个从没打开过的路径。
    pub documents: DocumentList,
    /// 当前 buffer 是什么东西（普通文件 / 目录列表）。
    ///
    /// 它决定的是「行为」：只有目录列表才能按 Enter 进入光标下的条目，
    /// 也只有 [`DocumentKind::File`] 才允许 `:w`。
    pub kind: DocumentKind,
    /// 是否有 `:check` 正在后台跑。
    ///
    /// ⚠️ 只记「有没有」，**不持有那个通道** —— 通道是 IO 的东西，归主循环管。
    /// 理由跟 `run_action` 不拿 `&mut Terminal` 一样：`App` 一旦持有活着的东西，
    /// `App::new()` 就不再是纯内存对象，那一整套测试全得陪葬。
    pub checking: bool,
    /// 撤销 / 重做栈（私有：外部只通过 `undo()` / `redo()` 使用）
    history: UndoStack,
    /// 为 true 时，内部编辑原语不再各自记录撤销步。
    /// 用于「粘贴」这类一次按键包含多次插入的整体编辑，保证整段粘贴只占一步。
    history_locked: bool,
}

impl Default for App {
    /// `App::default()` 就是 [`App::new`]：一个空文档、全用默认设置的编辑器。
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    /// 新建一个空文件编辑器（设置全用默认值）
    pub fn new() -> Self {
        Self::from_content(None, String::new())
    }

    /// 打开文件内容时调用；file_path 传 None 表示新建。设置用默认值。
    pub fn from_content(file_path: Option<String>, content: String) -> Self {
        Self::with_config(file_path, content, Config::default())
    }

    /// 带上用户配置构造。main.rs 读好配置文件后走这个入口。
    pub fn with_config(file_path: Option<String>, content: String, config: Config) -> Self {
        Self {
            mode: EditorMode::ReadOnly,
            buffer: Buffer::from_str(&content),
            cursor: Cursor::default(),
            viewport: Viewport::default(),
            command_input: String::new(),
            status_message: String::new(),
            dirty: false,
            file_path,
            config,
            config_path: None,
            documents: DocumentList::new(),
            kind: DocumentKind::default(),
            checking: false,
            history: UndoStack::new(DEFAULT_UNDO_LIMIT),
            history_locked: false,
        }
    }

    /// 记录配置文件的来源，供 `:config path` 展示（main.rs 加载完配置后调用）。
    pub fn set_config_source(&mut self, path: Option<PathBuf>) {
        self.config_path = path;
    }

    /// 整体换掉用户偏好（`:config reload` 用）。
    ///
    /// **只动 `config`**：文档、光标、撤销历史、dirty 全都不会被碰。
    /// 改设置不是编辑文档，不该把文件标记成「已修改」，也不该弄丢撤销链。
    pub fn apply_config(&mut self, config: Config) {
        self.config = config;
    }

    /// 用一个新文档替换当前内容（由 main.rs 在读好文件/目录后调用）。
    ///
    /// app.rs **不负责读文件**（那是 file_io.rs 的事），这里只接收结果并重置视图。
    /// `self.config` 属于用户偏好，换文档时原样保留。
    pub fn replace_document(&mut self, file_path: String, content: String) {
        self.buffer = Buffer::from_str(&content);
        self.file_path = Some(file_path);
        self.mode = EditorMode::ReadOnly;
        self.cursor = Cursor::default();
        self.viewport = Viewport::default();
        self.command_input.clear();
        self.status_message.clear();
        // 默认当普通文件；main.rs 紧接着会按实际情况改它
        self.kind = DocumentKind::default();
        // 撤销历史不能跨文件：换了文档就整体清空
        self.history.reset();
        self.history_locked = false;
        self.sync_dirty();
    }

    /// 另存为之后改个名字：**只改名字**。
    ///
    /// 跟 [`App::replace_document`] 是两件事，别混：那个是「换成另一份文档」，
    /// 于是光标、视口、撤销历史全重置。这里是「还是这份内容，只是它现在住到
    /// 另一个文件去了」—— 内容和撤销历史都得**原样留着**，
    /// 否则你辛苦撤销的链条会因为一次另存为而断掉。
    pub fn rename_document(&mut self, file_path: String) {
        self.file_path = Some(file_path);
    }

    // ---------- 撤销 / 重做 ----------

    /// 撤销一步；没有可撤销的内容时返回 `false`。
    ///
    /// 调用方（commands.rs 的 `undo_or_report`）负责给用户反馈（如状态栏 "Undo"）。
    pub fn undo(&mut self) -> bool {
        if !self.history.can_undo() {
            return false;
        }
        let buffer = self.buffer.snapshot();
        match self.history.undo(buffer, self.cursor) {
            Some(snapshot) => {
                self.restore_snapshot(snapshot);
                true
            }
            None => false,
        }
    }

    /// 重做一步；没有可重做的内容时返回 `false`。
    pub fn redo(&mut self) -> bool {
        if !self.history.can_redo() {
            return false;
        }
        let buffer = self.buffer.snapshot();
        match self.history.redo(buffer, self.cursor) {
            Some(snapshot) => {
                self.restore_snapshot(snapshot);
                true
            }
            None => false,
        }
    }

    /// 保存成功后由 main.rs 调用：把「当前修订」记为已保存，`dirty` 随之清除。
    ///
    /// 用修订号而不是直接把 `dirty` 置 false，是为了支持
    /// 「改 → 存 → 再改 → 撤销」能正确回到「未修改」状态。
    pub fn mark_saved(&mut self) {
        self.history.mark_saved();
        // 保存是一次「编辑中断」：之后继续输入应开启新的撤销步，
        // 否则「改 → 存 → 再改」会被合并成一步，撤销时连保存前的内容一起退掉。
        self.history.break_merge();
        self.sync_dirty();
    }

    /// 断开撤销步合并：用于「与文本无关的用户动作」（鼠标点击等），
    /// 保证之后输入的内容能被单独撤销。
    pub fn break_undo_group(&mut self) {
        self.history.break_merge();
    }

    /// 执行「命令式编辑」（`:delete` / `:swap` 等直接改缓冲的命令）**之前**调用，
    /// 把当前状态存成独立的一步，让该命令也能被 `u` 撤销。
    ///
    /// 若命令最终没有真正改动缓冲，请调用 [`abort_undoable_command`](Self::abort_undoable_command)
    /// 回滚这一步，避免留下「撤销了却什么都没变」的空步。
    pub fn begin_undoable_command(&mut self) {
        self.record_standalone_edit();
    }

    /// 命令式编辑没有真正发生改动时，回滚 [`begin_undoable_command`](Self::begin_undoable_command) 存下的快照。
    pub fn abort_undoable_command(&mut self) {
        self.history.discard_last_step();
        self.sync_dirty();
    }

    /// 把快照写回状态（文本 + 光标），并同步 `dirty`
    fn restore_snapshot(&mut self, snapshot: Snapshot) {
        // 直接搬运快照里的 Buffer（move，连 O(1) 的克隆都省了）
        self.buffer = snapshot.buffer;
        self.cursor = snapshot.cursor;
        self.clamp_cursor_to_buffer();
        self.sync_dirty();
    }

    /// 根据修订号刷新 `dirty` 字段（ui.rs 直接读这个字段）
    fn sync_dirty(&mut self) {
        self.dirty = self.history.is_dirty();
    }

    /// 记录一个「可合并」的编辑：同类同行的连续编辑只占一个撤销步。
    ///
    /// 合并时直接返回，连快照都不存。
    fn record_coalescing_edit(&mut self, kind: EditKind) {
        if self.history_locked || !self.history.needs_step(kind, self.cursor.row) {
            return;
        }
        // 快照是 O(1) 的（rope + Arc 的写时复制），不必吝惜
        let buffer = self.buffer.snapshot();
        let cursor = self.cursor;
        self.history
            .push_step(buffer, cursor, Some((kind, self.cursor.row)));
    }

    /// 记录一个「独占一步」的编辑（Enter 断行、粘贴）。
    fn record_standalone_edit(&mut self) {
        if self.history_locked {
            return;
        }
        let buffer = self.buffer.snapshot();
        let cursor = self.cursor;
        self.history.push_step(buffer, cursor, None);
    }

    // ---------- 模式切换（供 update.rs 调用） ----------

    pub fn set_mode(&mut self, mode: EditorMode) {
        // 模式切换视为一次「编辑中断」：之后的输入应开启新的撤销步
        self.history.break_merge();
        self.mode = mode;
    }

    /// 进入命令模式（按下 `:`）：先清掉旧的命令输入
    ///
    /// 注意：这个函数不做模式合法性检查。按键分发（update.rs）负责保证
    /// 「只有只读模式下按 `:` 才调用它」，编辑模式下 `:` 会走 `insert_char_at_cursor` 当作普通字符。
    pub fn enter_command_mode(&mut self) {
        self.history.break_merge();
        self.mode = EditorMode::Command;
        self.command_input.clear();
    }

    /// 进入外部命令模式（按下 `!`）：先清掉旧输入
    ///
    /// 跟 [`App::enter_command_mode`] 一样不做模式合法性检查 ——
    /// 「只有只读模式下按 `!` 才调用它」由按键分发保证。
    pub fn enter_external_mode(&mut self) {
        self.history.break_merge();
        self.mode = EditorMode::External;
        self.command_input.clear();
    }

    /// 取消底部这一行（Esc）：回到只读模式
    ///
    /// 命令模式和外部命令模式共用 —— 两者都是「底部在收集一行」，取消的方式一样。
    pub fn cancel_command_mode(&mut self) {
        self.history.break_merge();
        self.mode = EditorMode::ReadOnly;
        self.command_input.clear();
    }

    // ---------- 光标移动与滚动 ----------

    /// 以 (row_delta, col_delta) 增量移动光标。
    ///
    /// - 上/下（row_delta≠0）：只改行号，越界钳制到首/末行；col 截断到目标行的长度。
    /// - 左/右（col_delta≠0）：逐格移动；在行尾继续按 → 会换到下一行行首，
    ///   在行首继续按 ← 会回到上一行行尾；首行行首 / 末行行尾则停在原地。
    ///
    /// 说明：「记住上一行的目标列」属于锦上添花，留作以后优化。
    pub fn move_cursor_by(&mut self, row_delta: isize, col_delta: isize) {
        // 光标移动视为一次「编辑中断」：在同一行里挪动后再输入，应该能单独撤销
        self.history.break_merge();
        self.clamp_cursor_to_buffer();
        let row_count = self.buffer.get_line_count() as isize;
        let mut row = self.cursor.row as isize;
        let mut col = self.cursor.col as isize;

        // 上 / 下：行号越界就钳制到首行 / 末行
        if row_delta != 0 {
            row = (row + row_delta).clamp(0, row_count - 1);
        }

        // 左 / 右：按步长逐格移动，跨过行边界就换到相邻行
        for _ in 0..col_delta.abs() {
            if col_delta > 0 {
                let line_len = self.buffer.get_char_count(row as usize) as isize;
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
                    col = self.buffer.get_char_count(row as usize) as isize;
                }
                // 首行行首：原地不动
            }
        }

        // 垂直移动后，把 col 截断到目标行长度（目标行更短时）
        if row_delta != 0 {
            let line_len = self.buffer.get_char_count(row as usize) as isize;
            col = col.min(line_len);
        }

        self.cursor.row = row as usize;
        self.cursor.col = col as usize;
    }

    /// 把越界的光标钳制回合法范围（任何编辑操作后都应调用一次做保险）
    pub fn clamp_cursor_to_buffer(&mut self) {
        self.buffer.ensure_at_least_one_line();
        let max_row = self.buffer.get_line_count() - 1;
        self.cursor.row = self.cursor.row.min(max_row);
        let max_col = self.buffer.get_char_count(self.cursor.row);
        self.cursor.col = self.cursor.col.min(max_col);
    }

    /// 保证光标落在可视区内，并尽量与边缘保持边距：
    /// - 上/下：`config.scroll_margin` 行（类似 vim 的 scrolloff）；
    /// - 左/右：`config.side_scroll_margin` 列（类似 vim 的 sidescrolloff）。
    ///
    /// 视口滚不动时（文件首/尾、行首/行尾）允许光标贴边。
    ///
    /// `view_height` / `view_width` 是「文本区」能显示的行/列数（不含底部命令栏），
    /// 由调用方（ui/update）从终端布局尺寸算出后传入，因此 app.rs 不用碰终端。
    pub fn scroll_viewport_to_keep_cursor_visible(
        &mut self,
        view_height: usize,
        view_width: usize,
    ) {
        if view_height == 0 || view_width == 0 {
            return;
        }

        // 边距最多取到半个视口，避免小窗口里约束自相矛盾
        let margin = self
            .config
            .scroll_margin
            .min(view_height.saturating_sub(1) / 2);

        // 垂直：光标贴近上/下边缘时，把视口一起带走
        if self.cursor.row < self.viewport.top + margin {
            self.viewport.top = self.cursor.row.saturating_sub(margin);
        } else if self.cursor.row + margin >= self.viewport.top + view_height {
            self.viewport.top = (self.cursor.row + margin + 1).saturating_sub(view_height);
        }

        // 兜底：无论 margin 多大，光标本身必须可见
        if self.cursor.row < self.viewport.top {
            self.viewport.top = self.cursor.row;
        } else if self.cursor.row >= self.viewport.top + view_height {
            self.viewport.top = self.cursor.row + 1 - view_height;
        }

        // 水平：光标贴近左/右边缘时，把视口一起带走（sidescrolloff，单位是「列」）
        let cursor_cells = self
            .buffer
            .get_cell_at_char(self.cursor.row, self.cursor.col);
        let h_margin = self
            .config
            .side_scroll_margin
            .min(view_width.saturating_sub(1) / 2);
        if cursor_cells < self.viewport.left + h_margin {
            self.viewport.left = cursor_cells.saturating_sub(h_margin);
        } else if cursor_cells + h_margin >= self.viewport.left + view_width {
            self.viewport.left = (cursor_cells + h_margin + 1).saturating_sub(view_width);
        }

        // 兜底：光标本身必须可见
        if cursor_cells < self.viewport.left {
            self.viewport.left = cursor_cells;
        } else if cursor_cells >= self.viewport.left + view_width {
            self.viewport.left = cursor_cells + 1 - view_width;
        }

        // 视口不越过内容边界（滚不动就停在边界，光标自然贴边）
        self.clamp_viewport_to_content(view_height, view_width);
    }

    /// 只移动视口 (row_delta 行, col_delta 列)，**不动光标**（滚轮等“纯浏览”操作）。
    /// `col_delta` 的单位是**显示列**（格），不是字符数。
    ///
    /// 越界会被 [`clamp_viewport_to_content`](Self::clamp_viewport_to_content) 钳回内容边界。
    pub fn move_viewport_by(
        &mut self,
        row_delta: isize,
        col_delta: isize,
        view_height: usize,
        view_width: usize,
    ) {
        if view_height == 0 || view_width == 0 {
            return;
        }
        self.viewport.top = (self.viewport.top as isize + row_delta).max(0) as usize;
        self.viewport.left = (self.viewport.left as isize + col_delta).max(0) as usize;
        self.clamp_viewport_to_content(view_height, view_width);
    }

    /// 把视口钳制回合法范围，保证不越出内容边界。
    ///
    /// - 垂直：最多滚到「最后一行贴住视口底边」；
    /// - 水平：最多滚到「最长一行的末尾贴住视口右边」（单位是显示列）。
    pub fn clamp_viewport_to_content(&mut self, view_height: usize, view_width: usize) {
        if view_height == 0 || view_width == 0 {
            return;
        }

        let max_top = self.buffer.get_line_count().saturating_sub(view_height);
        self.viewport.top = self.viewport.top.min(max_top);

        let max_cells = self.buffer.get_max_cell_count();
        let max_left = max_cells.saturating_sub(view_width.saturating_sub(1));
        self.viewport.left = self.viewport.left.min(max_left);
    }

    // ---------- 查询（不改变状态） ----------

    /// 「我现在在哪个目录」—— 命令里**相对路径的基准**。
    ///
    /// - 当前是**目录列表** → 就是它列的那个目录
    /// - 当前是**普通文件** → 它所在的目录
    /// - 还没打开任何东西 → `None`（只能退回进程的工作目录）
    ///
    /// ⚠️ 这**不是**进程的工作目录。工作目录是隐形的：用户看不见它在哪、
    /// 也没有命令能改它，于是「`:open main.rs` 到底上哪儿找」变成一个谜。
    /// 而这里给的答案就写在屏幕上 —— 你正在看的那份文档所在的地方。
    ///
    /// 有了它，目录列表里按 `Enter` 和 `:open <名字>` 就是**同一件事**了
    /// （它们本来就该是，以前却一个能用一个不能用）。
    pub fn current_directory(&self) -> Option<String> {
        let path = self.file_path.as_deref()?;
        match self.kind {
            // 列表的 `file_path` 就是被列的那个目录本身
            DocumentKind::DirectoryListing => Some(path.to_string()),
            DocumentKind::File => Path::new(path)
                .parent()
                .map(|dir| dir.display().to_string()),
        }
    }

    /// 当前光标所在行的文本（只读模式下 `y` 复制用）。
    ///
    /// 返回 owned String：调用方要把它交给 `Action::Copy` 带出 app，
    /// 不能留着对 `self.buffer` 的借用。
    pub fn get_current_line_text(&self) -> String {
        self.buffer.get_line(self.cursor.row).unwrap_or_default()
    }

    /// 取出一段文本（行、列均 **0 基**，且**含两端**）。
    ///
    /// 供 `:copy` 命令使用。坐标越界或起点在终点之后时返回 `None`，
    /// 由调用方（commands.rs）负责报错。
    ///
    /// `col` 会被自动夹到该行末尾，所以想取整行可以传 `usize::MAX`。
    pub fn get_text_in_range(&self, start: (usize, usize), end: (usize, usize)) -> Option<String> {
        let (sr, sc) = start;
        let (er, ec) = end;
        // 起点不得在终点之后（按行优先比较）
        if sr > er || (sr == er && sc > ec) {
            return None;
        }
        if er >= self.buffer.get_line_count() {
            return None;
        }

        let chars_at = |row: usize| -> Vec<char> {
            self.buffer
                .get_line(row)
                .unwrap_or_default()
                .chars()
                .collect()
        };

        // 同一行：直接取 chars[sc..=ec]
        if sr == er {
            let chars = chars_at(sr);
            if chars.is_empty() {
                return Some(String::new());
            }
            let sc = sc.min(chars.len() - 1);
            let ec = ec.min(chars.len() - 1);
            return Some(chars[sc..=ec].iter().collect());
        }

        // 跨行：首行取 sc 到行尾 + 中间整行 + 末行行首到 ec
        let mut out = String::new();
        let first = chars_at(sr);
        let from = sc.min(first.len());
        out.extend(first[from..].iter());

        for row in (sr + 1)..er {
            out.push('\n');
            out.push_str(&self.buffer.get_line(row).unwrap_or_default());
        }

        out.push('\n');
        let last = chars_at(er);
        if !last.is_empty() {
            let to = ec.min(last.len() - 1);
            out.extend(last[..=to].iter());
        }

        Some(out)
    }

    // ---------- 文本编辑（update.rs 在 Edit 模式下调用） ----------

    /// 在光标处插入一个普通字符，光标右移一格
    pub fn insert_char_at_cursor(&mut self, ch: char) {
        self.record_coalescing_edit(EditKind::Insert);
        self.buffer
            .insert_char(self.cursor.row, self.cursor.col, ch);
        self.cursor.col += 1;
        self.sync_dirty();
    }

    /// Backspace：删掉光标前一个字符；在行首则把当前行并入上一行
    pub fn delete_char_before_cursor(&mut self) {
        if self.cursor.col > 0 {
            self.record_coalescing_edit(EditKind::Delete);
            self.buffer
                .delete_char_before(self.cursor.row, self.cursor.col);
            self.cursor.col -= 1;
        } else if self.cursor.row > 0 {
            self.record_coalescing_edit(EditKind::Delete);
            let prev_len = self.buffer.get_char_count(self.cursor.row - 1);
            self.buffer.delete_char_before(self.cursor.row, 0);
            self.cursor.row -= 1;
            self.cursor.col = prev_len;
        } else {
            return; // 首行行首：什么都没发生，不该记撤销步
        }
        self.sync_dirty();
    }

    /// Delete：删掉光标「右边」紧邻的字符（即光标块盖住的那个字符），光标本身不动；
    /// 若光标已在行尾（右边没有字符），则把下一行拼到本行行尾
    pub fn delete_char_at_cursor(&mut self) {
        let line_len = self.buffer.get_char_count(self.cursor.row);
        if self.cursor.col < line_len || self.cursor.row + 1 < self.buffer.get_line_count() {
            self.record_coalescing_edit(EditKind::Delete);
            self.buffer.delete_char_at(self.cursor.row, self.cursor.col);
            self.sync_dirty();
        }
    }

    /// Enter：在光标处断行，光标移到新行开头
    pub fn split_line_at_cursor(&mut self) {
        self.record_standalone_edit();
        self.buffer.split_line_at(self.cursor.row, self.cursor.col);
        self.cursor.row += 1;
        // 新行已经带有继承来的缩进，光标应放在缩进之后。
        self.cursor.col = self.buffer.get_leading_indent_char_count(self.cursor.row);
        self.sync_dirty();
    }

    /// 粘贴一段文本（来自终端的 bracketed paste，整段一次性到达）。
    ///
    /// - 先把 Windows 常见的 `\r\n`（以及孤立的 `\r`）归一化成 `\n`，
    ///   否则缓冲里会混进 `\r`，显示和保存都会出问题；
    /// - 再按 `\n` 切段逐行插入，光标最终落在粘贴内容的末尾。
    ///
    /// 这里刻意复用 `insert_char_at_cursor` / `split_line_at_cursor`，让粘贴走和手输完全相同的
    /// 代码路径（多字节字符、dirty 标记、光标推进都自动一致）。
    ///
    /// 撤销方面：**整段粘贴只占一个撤销步**。做法是先存一份「粘贴前」的快照，
    /// 再临时锁住历史记录（`history_locked`），让内部逐字插入不再各自记步。
    pub fn paste_text_at_cursor(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.record_standalone_edit();
        self.history_locked = true;

        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");

        let mut segments = normalized.split('\n');
        // 第一段接在光标当前位置（不换行）
        if let Some(first) = segments.next() {
            for ch in first.chars() {
                self.insert_char_at_cursor(ch);
            }
        }
        // 其余每段：先断行到新的一行，再写入
        for segment in segments {
            self.split_line_at_cursor();
            for ch in segment.chars() {
                self.insert_char_at_cursor(ch);
            }
        }

        self.history_locked = false;
        self.sync_dirty();
    }

    // ---------- 状态栏 ----------

    /// 设置底部提示信息
    pub fn set_status_message(&mut self, msg: impl Into<String>) {
        self.status_message = msg.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_inherits_indent_and_places_cursor_after_it() {
        let mut app = App::from_content(None, "    int a;".to_string());
        app.set_mode(EditorMode::Edit);
        app.cursor = Cursor { row: 0, col: 10 };
        app.split_line_at_cursor();
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("    "));
        assert_eq!(app.cursor, Cursor { row: 1, col: 4 });
        app.insert_char_at_cursor('x');
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("    x"));
    }

    #[test]
    fn enter_preserves_tab_indent() {
        let mut app = App::from_content(None, "\tvalue".to_string());
        app.cursor = Cursor { row: 0, col: 6 };
        app.split_line_at_cursor();
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("\t"));
        assert_eq!(app.cursor.col, 1);
    }

    #[test]
    fn move_cursor_clamps_col_to_short_line() {
        let mut app = App::from_content(None, "hello\nhi".to_string());
        app.cursor = Cursor { row: 0, col: 5 };
        app.move_cursor_by(1, 0); // 从 5 字符长的行下移到 2 字符长的行
        assert_eq!((app.cursor.row, app.cursor.col), (1, 2));
    }

    #[test]
    fn move_cursor_stops_at_file_edges() {
        let mut app = App::from_content(None, "a\nb".to_string());
        app.move_cursor_by(-1, -1); // 已在左上角，应原地不动
        assert_eq!((app.cursor.row, app.cursor.col), (0, 0));
        app.move_cursor_by(100, 100); // 越界应钳制到右下角
        assert_eq!((app.cursor.row, app.cursor.col), (1, 1));
    }

    #[test]
    fn move_cursor_right_wraps_to_next_line() {
        let mut app = App::from_content(None, "ab\ncd".to_string());
        app.cursor = Cursor { row: 0, col: 2 }; // 第 1 行行尾
        app.move_cursor_by(0, 1); // → 应换到第 2 行行首
        assert_eq!((app.cursor.row, app.cursor.col), (1, 0));
    }

    #[test]
    fn move_cursor_left_wraps_to_prev_line() {
        let mut app = App::from_content(None, "ab\ncd".to_string());
        app.cursor = Cursor { row: 1, col: 0 }; // 第 2 行行首
        app.move_cursor_by(0, -1); // ← 应回到第 1 行行尾
        assert_eq!((app.cursor.row, app.cursor.col), (0, 2));
    }

    #[test]
    fn move_cursor_right_stays_at_last_line_end() {
        let mut app = App::from_content(None, "ab\ncd".to_string());
        app.cursor = Cursor { row: 1, col: 2 }; // 末行行尾，再 → 应原地不动
        app.move_cursor_by(0, 1);
        assert_eq!((app.cursor.row, app.cursor.col), (1, 2));
    }

    #[test]
    fn ensure_cursor_visible_scrolls_viewport() {
        let mut app = App::from_content(None, "a\nb\nc\nd".to_string());
        app.cursor = Cursor { row: 3, col: 0 };
        app.scroll_viewport_to_keep_cursor_visible(2, 80); // 可视区只有 2 行
        assert_eq!(app.viewport.top, 2); // 应向下滚到能看见第 3 行
    }

    #[test]
    fn ensure_cursor_visible_keeps_scroll_margin() {
        let text = (0..30)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let mut app = App::from_content(None, text);
        app.cursor = Cursor { row: 7, col: 0 };
        app.scroll_viewport_to_keep_cursor_visible(10, 80);
        assert_eq!(app.viewport.top, 1); // 光标下方仍保留 3 行边距
    }

    #[test]
    fn ensure_cursor_visible_releases_margin_at_content_end() {
        let mut app = App::from_content(None, "a\nb\nc\nd\ne".to_string());
        app.cursor = Cursor { row: 4, col: 0 };
        app.scroll_viewport_to_keep_cursor_visible(5, 80);
        assert_eq!(app.viewport.top, 0); // 视口已到边界，光标只能贴底边
    }

    #[test]
    fn viewport_move_scrolls_and_clamps() {
        let text = (0..6).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let mut app = App::from_content(None, text);
        app.move_viewport_by(2, 0, 4, 80);
        assert_eq!(app.viewport.top, 2);
        app.move_viewport_by(100, 0, 4, 80); // 最多滚到 max_top = 6 - 4 = 2
        assert_eq!(app.viewport.top, 2);
        app.move_viewport_by(-100, 0, 4, 80);
        assert_eq!(app.viewport.top, 0);
    }

    #[test]
    fn ensure_cursor_visible_keeps_side_margin() {
        let mut app = App::from_content(None, "a".repeat(40));
        app.config.side_scroll_margin = 3;
        app.cursor = Cursor { row: 0, col: 8 };
        app.scroll_viewport_to_keep_cursor_visible(10, 10);
        assert_eq!(app.viewport.left, 2); // 光标右侧保留 3 列
    }

    #[test]
    fn horizontal_scroll_is_cell_based_for_wide_chars() {
        // 8 个汉字 = 16 列；窗口只有 8 列，光标在行尾时要能滚到看见末尾
        let mut app = App::from_content(None, "你好世界你好世界".to_string());
        app.config.side_scroll_margin = 0;
        app.cursor = Cursor { row: 0, col: 8 };
        app.scroll_viewport_to_keep_cursor_visible(10, 8);
        assert_eq!(app.viewport.left, 9);
    }

    #[test]
    fn viewport_move_horizontal_is_clamped() {
        let mut app = App::from_content(None, "a".repeat(20));
        app.move_viewport_by(0, 5, 4, 8);
        assert_eq!(app.viewport.left, 5);
        app.move_viewport_by(0, 100, 4, 8); // max_left = 20 - 7 = 13
        assert_eq!(app.viewport.left, 13);
        app.move_viewport_by(0, -100, 4, 8);
        assert_eq!(app.viewport.left, 0);
    }

    #[test]
    fn typing_marks_dirty_and_advances_cursor() {
        let mut app = App::new();
        app.insert_char_at_cursor('a');
        assert!(app.dirty);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("a"));
        assert_eq!(app.cursor.col, 1);
    }

    #[test]
    fn backspace_joins_rows_via_app() {
        let mut app = App::from_content(None, "ab\ncd".to_string());
        app.cursor = Cursor { row: 1, col: 0 };
        app.delete_char_before_cursor(); // 行首 Backspace → 并入上一行末尾
        assert_eq!(app.buffer.get_line_count(), 1);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("abcd"));
        assert_eq!(app.cursor.row, 0);
        assert_eq!(app.cursor.col, 2);
    }

    #[test]
    fn paste_single_line_inserts_at_cursor() {
        let mut app = App::from_content(None, "ac".to_string());
        app.cursor = Cursor { row: 0, col: 1 };
        app.paste_text_at_cursor("b");
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("abc"));
        assert_eq!(app.cursor.col, 2);
        assert!(app.dirty);
    }

    #[test]
    fn paste_multiline_splits_into_rows() {
        let mut app = App::from_content(None, "ad".to_string());
        app.cursor = Cursor { row: 0, col: 1 };
        app.paste_text_at_cursor("b\nc");
        assert_eq!(app.buffer.get_line_count(), 2);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("ab"));
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("cd"));
        assert_eq!((app.cursor.row, app.cursor.col), (1, 1));
    }

    #[test]
    fn paste_normalizes_crlf_and_lone_cr() {
        // Windows 剪贴板常见的 \r\n，以及旧 Mac 风格的孤立 \r
        let mut app = App::from_content(None, String::new());
        app.paste_text_at_cursor("a\r\nb\rc");
        assert_eq!(app.buffer.get_line_count(), 3);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("a"));
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("b"));
        assert_eq!(app.buffer.get_line(2).as_deref(), Some("c"));
    }

    #[test]
    fn paste_empty_is_noop() {
        let mut app = App::from_content(None, "ab".to_string());
        app.paste_text_at_cursor("");
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("ab"));
        assert!(!app.dirty);
    }

    // ---------- 撤销 / 重做 ----------

    #[test]
    fn undo_restores_previous_text_and_cursor() {
        let mut app = App::from_content(None, "ab".to_string());
        app.cursor = Cursor { row: 0, col: 2 };
        app.insert_char_at_cursor('c');
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("abc"));

        assert!(app.undo());
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("ab"));
        assert_eq!(app.cursor, Cursor { row: 0, col: 2 });
        assert!(!app.undo(), "已经回到最初状态，没有更多可撤销的");
    }

    #[test]
    fn consecutive_typing_merges_into_one_undo_step() {
        let mut app = App::new();
        for ch in "abc".chars() {
            app.insert_char_at_cursor(ch);
        }
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("abc"));

        assert!(app.undo());
        assert_eq!(
            app.buffer.get_line(0).as_deref(),
            Some(""),
            "连续输入应合并成一步"
        );
        assert!(!app.undo());
    }

    #[test]
    fn cursor_move_ends_the_merge_group() {
        let mut app = App::from_content(None, "ac".to_string());
        app.cursor = Cursor { row: 0, col: 1 };
        app.insert_char_at_cursor('b'); // -> "abc"
        app.move_cursor_by(0, -1); // 光标移动：断开合并
        app.insert_char_at_cursor('x'); // -> "axbc"

        assert_eq!(app.buffer.get_line(0).as_deref(), Some("axbc"));
        assert!(app.undo());
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("abc"));
        assert!(app.undo());
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("ac"));
    }

    #[test]
    fn consecutive_backspaces_merge_into_one_step() {
        let mut app = App::from_content(None, "abcd".to_string());
        app.cursor = Cursor { row: 0, col: 4 };
        app.delete_char_before_cursor();
        app.delete_char_before_cursor();
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("ab"));

        assert!(app.undo());
        assert_eq!(
            app.buffer.get_line(0).as_deref(),
            Some("abcd"),
            "连续删除应合并成一步"
        );
    }

    #[test]
    fn paste_is_a_single_undo_step() {
        let mut app = App::from_content(None, String::new());
        app.paste_text_at_cursor("a\nb\nc");
        assert_eq!(app.buffer.get_line_count(), 3);

        assert!(app.undo(), "整段粘贴应只需撤销一次");
        assert_eq!(app.buffer.get_line_count(), 1);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some(""));
        assert!(!app.undo());
    }

    #[test]
    fn enter_split_is_its_own_undo_step() {
        let mut app = App::from_content(None, "ab".to_string());
        app.cursor = Cursor { row: 0, col: 1 };
        app.split_line_at_cursor();
        assert_eq!(app.buffer.get_line_count(), 2);

        assert!(app.undo());
        assert_eq!(app.buffer.get_line_count(), 1);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("ab"));
    }

    #[test]
    fn new_edit_after_undo_clears_redo() {
        let mut app = App::from_content(None, "a".to_string());
        app.cursor = Cursor { row: 0, col: 1 };
        app.insert_char_at_cursor('b'); // -> "ab"
        assert!(app.undo());
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("a"));

        app.insert_char_at_cursor('z'); // 撤销后又改了 → 重做链失效
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("az"));
        assert!(!app.redo());
    }

    #[test]
    fn redo_reapplies_undone_edit() {
        let mut app = App::from_content(None, "a".to_string());
        app.cursor = Cursor { row: 0, col: 1 };
        app.insert_char_at_cursor('b');
        assert!(app.undo());
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("a"));

        assert!(app.redo());
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("ab"));
        assert!(!app.redo());
    }

    #[test]
    fn dirty_follows_revision_and_save() {
        let mut app = App::from_content(None, "a".to_string());
        assert!(!app.dirty);

        app.cursor = Cursor { row: 0, col: 1 };
        app.insert_char_at_cursor('b');
        assert!(app.dirty);

        app.mark_saved();
        assert!(!app.dirty, "保存后应回到干净状态");

        app.insert_char_at_cursor('c');
        assert!(app.dirty, "保存后再改应重新变脏");

        assert!(app.undo());
        assert!(!app.dirty, "撤销回到上次保存的状态 → 不该再显示为已修改");
    }

    #[test]
    fn noop_edits_do_not_create_undo_steps() {
        let mut app = App::from_content(None, "ab".to_string());
        app.cursor = Cursor { row: 0, col: 0 };
        app.delete_char_before_cursor(); // 首行行首：什么都没发生
        assert!(!app.dirty);
        assert!(!app.undo());
    }

    #[test]
    fn replace_document_clears_undo_history() {
        let mut app = App::from_content(None, "a".to_string());
        app.insert_char_at_cursor('b');
        app.replace_document("new.txt".to_string(), "xyz".to_string());

        assert!(!app.undo(), "换文档后不该还能撤销回旧文件的内容");
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("xyz"));
        assert!(!app.dirty);
    }

    #[test]
    fn undo_limit_drops_oldest_steps() {
        let mut app = App::new();
        app.history = UndoStack::new(2); // 用很小的上限验证「超限丢最旧」
        for _ in 0..5 {
            app.cursor = Cursor { row: 0, col: 0 };
            app.break_undo_group(); // 每轮都独立成步
            app.insert_char_at_cursor('x');
        }
        assert!(app.undo());
        assert!(app.undo());
        assert!(!app.undo(), "上限 2 → 最多只能撤销 2 次");
    }

    #[test]
    fn current_line_text_returns_cursor_row() {
        let mut app = App::from_content(None, "ab\ncd".to_string());
        app.cursor = Cursor { row: 1, col: 1 };
        assert_eq!(app.get_current_line_text(), "cd");
    }

    #[test]
    fn text_range_same_row_is_inclusive() {
        let app = App::from_content(None, "abcdef".to_string());
        assert_eq!(
            app.get_text_in_range((0, 0), (0, 2)).as_deref(),
            Some("abc")
        );
        assert_eq!(
            app.get_text_in_range((0, 3), (0, 5)).as_deref(),
            Some("def")
        );
    }

    #[test]
    fn text_range_usize_max_means_to_end_of_line() {
        let app = App::from_content(None, "abc".to_string());
        assert_eq!(
            app.get_text_in_range((0, 0), (0, usize::MAX)).as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn text_range_across_rows_keeps_newlines() {
        let app = App::from_content(None, "abc\ndef\nghi".to_string());
        assert_eq!(
            app.get_text_in_range((0, 1), (2, 1)).as_deref(),
            Some("bc\ndef\ngh")
        );
    }

    #[test]
    fn text_range_rejects_reversed_and_out_of_bounds() {
        let app = App::from_content(None, "ab\ncd".to_string());
        assert_eq!(app.get_text_in_range((1, 0), (0, 0)), None); // 起点在终点之后
        assert_eq!(app.get_text_in_range((0, 2), (0, 1)), None); // 同行但列倒序
        assert_eq!(app.get_text_in_range((0, 0), (5, 0)), None); // 行越界
    }

    #[test]
    fn text_range_on_empty_line_is_empty_string() {
        let app = App::from_content(None, String::new());
        assert_eq!(
            app.get_text_in_range((0, 0), (0, usize::MAX)).as_deref(),
            Some("")
        );
    }

    #[test]
    fn replace_document_swaps_content_and_resets_view() {
        let mut app = App::from_content(Some("old.txt".to_string()), "old".to_string());
        app.cursor = Cursor { row: 0, col: 3 };
        app.viewport.top = 2;
        app.dirty = true;
        app.config.show_line_numbers = true;
        app.config.tab_width = 2;
        app.kind = DocumentKind::DirectoryListing;

        app.replace_document("new.txt".to_string(), "x\ny".to_string());

        assert_eq!(app.file_path.as_deref(), Some("new.txt"));
        assert_eq!(app.buffer.get_line_count(), 2);
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("y"));
        assert_eq!((app.cursor.row, app.cursor.col), (0, 0));
        assert_eq!(app.viewport.top, 0);
        assert_eq!(app.mode, EditorMode::ReadOnly);
        assert!(!app.dirty);
        assert!(app.status_message.is_empty());
        // 用户偏好类设置应保留
        assert!(app.config.show_line_numbers);
        assert_eq!(app.config.tab_width, 2);
        // 文档种类要回到默认（main.rs 会紧接着按实际情况重设）
        assert_eq!(app.kind, DocumentKind::File);
    }

    // ---------- 用户配置（:set / config.rs） ----------

    #[test]
    fn with_config_applies_user_settings() {
        let config = Config {
            show_line_numbers: false,
            tab_width: 2,
            scroll_margin: 1,
            side_scroll_margin: 1,
            // 其余字段（如颜色）跟默认值走，以后加字段不用改这个测试
            ..Config::default()
        };
        let app = App::with_config(None, "a".to_string(), config.clone());
        assert_eq!(app.config, config);
        // 还没告诉它配置从哪来
        assert!(app.config_path.is_none());
    }

    #[test]
    fn new_and_from_content_use_default_config() {
        assert_eq!(App::new().config, Config::default());
        assert_eq!(
            App::from_content(None, "a".to_string()).config,
            Config::default()
        );
    }

    #[test]
    fn config_source_is_kept_for_display() {
        let mut app = App::new();
        app.set_config_source(Some(PathBuf::from("stbd-settings.toml")));
        assert_eq!(app.config_path, Some(PathBuf::from("stbd-settings.toml")));

        app.set_config_source(None);
        assert!(app.config_path.is_none());
    }

    #[test]
    fn apply_config_swaps_settings_without_touching_the_document() {
        let mut app = App::from_content(Some("a.txt".to_string()), "line1\nline2".to_string());
        app.cursor = Cursor { row: 1, col: 3 };
        app.insert_char_at_cursor('X'); // 把文档弄脏
        assert!(app.dirty);

        let reloaded = Config {
            tab_width: 2,
            show_line_numbers: false,
            ..Config::default()
        };
        app.apply_config(reloaded.clone());

        assert_eq!(app.config, reloaded);
        // 重载设置不是编辑文档：内容、光标、dirty 都不该被动
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("linXe2"));
        assert_eq!(app.cursor, Cursor { row: 1, col: 4 });
        assert!(app.dirty, "改设置不该把文档标记成已保存");
    }
}
