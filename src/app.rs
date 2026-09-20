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
use crate::diagnostic::Diagnostic;
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
    /// **行选择模式**（只读模式下按 `V` 进入）：光标那一端可以上下拉，
    /// `y` / `d` / `Delete` 作用于圈住的那几行。
    ///
    /// ## 为什么是「行」选择，不是「字符」选择
    ///
    /// 因为它要伺候的三件事 —— 复制一段、剪切一段、删掉一段 —— 全是按行干的。
    /// 而字符选择得引入「列」：选区从哪一列起、跨过多少格、宽字符和 Tab 怎么算、
    /// 横向滚动时选区怎么跟着动 —— 全都要重新想一遍。
    /// 这和诊断那边定下的「契约是行，不存列」是同一个取舍。
    ///
    /// ## 和 [`EditorMode::Edit`] 的区别（别混）
    ///
    /// `Edit` 里敲什么就**改文档**；这里只改**选区** —— 除了 `d` / `Delete`
    /// 那两下（那是你明确要求改动），其余按键一个字都不会碰你的文件。
    Visual,
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
    /// `:errors` 的清单：**我们生成的**只读视图，磁盘上没这个东西
    ///
    /// `file_path` 留着它**来自的那个文件**（见 [`App::show_list`]），
    /// 所以标题和 `current_directory()` 都还对得上。
    Errors,
    /// `:ls` 的清单：打开过的文档，一行一个。同样是**我们生成的**只读视图。
    DocumentList,
    /// `:lsp` 的清单：配了哪些语言服务器、它们的命令在不在 `PATH` 上、
    /// 现在跑着几个。同样是**我们生成的**只读视图。
    ///
    /// 它和上面两种还不太一样：那两种的内容是**你本来就有的东西**
    /// （文档列表、某个文件的毛病），而这一份是**程序自己的配置**。
    /// 放进来是因为「一行装不下」这个理由一样 —— 而且它是你查
    /// 「我明明装了怎么不动」时唯一能看的地方。
    LspStatus,
}

impl DocumentKind {
    /// 它是**我们生成的**东西吗（`:errors` / `:ls` 那种清单）。
    ///
    /// 这个判断管的是**行为**，而且每一条都是必要的：
    ///
    /// - 不能 `:w` —— 把一份诊断清单写进你的源码文件？没有这个道理
    /// - 不参与语言服务器同步 —— 清单是**我们编的文本**，发过去服务器会
    ///   认认真真地报「这一堆字里有语法错误」，而那些错又会画到屏幕上
    /// - 行号栏**不**染诊断色 —— 清单那几行的行号是诊断的行号，
    ///   再按诊断染色就是拿自己的输出喂自己
    pub fn is_virtual(self) -> bool {
        matches!(self, Self::Errors | Self::DocumentList | Self::LspStatus)
    }

    /// `file_path` 指的是**一个文件**吗（而不是一个目录）。
    ///
    /// ⚠️ 它和 [`DocumentKind::is_virtual`] **不是一回事**，两者是交叉的：
    ///
    /// | 种类 | `is_virtual` | `wraps_a_file` |
    /// |------|--------------|----------------|
    /// | `File` | ✗ | ✓ |
    /// | `Errors` / `DocumentList` / `LspStatus` | ✓ 不能保存 | ✓ 盖在一个文件上 |
    /// | `DirectoryListing` | ✓ 不能保存 | ✗ `file_path` 是个**目录** |
    ///
    /// 分成两个方法的理由：它们回答的是两个不同的问题。
    /// 「能不能保存」「能不能发去语言服务器」问 `is_virtual`；
    /// 「这条推送说的是不是我们现在打开的那个文件」问这个。
    ///
    /// 混着用的代价实测过：拿 `is_virtual` 去当后者，会顺手把
    /// 「服务器在你看着清单时推来的诊断」一起丢掉，而且**永远丢**（见
    /// `main::is_current_file` 的注释）。
    pub fn wraps_a_file(self) -> bool {
        !matches!(self, Self::DirectoryListing)
    }
}

/// 行 `row` 在「`[first, last]` 整体挪一格」之后落到哪一行（不在区间里就不动）。
///
/// 单独拿出来是为了**两处用同一个算法**（光标一处、选区锚点一处）——
/// 那两处要是各写一遍，迟早会出现「光标跟上了、锚点没跟上」这种半截状态。
fn shifted_row(row: usize, first: usize, last: usize, up: bool) -> usize {
    if !(first..=last).contains(&row) {
        return row;
    }
    if up { row - 1 } else { row + 1 }
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
    /// 行选择模式的**锚点**（0 基行号）：钉在按下 `V` 的那一行上。
    ///
    /// 选区的另一端就是 `cursor.row` —— 它跟着 `j` / `k` 动。
    ///
    /// 它和模式是同一件事的两半：`mode == Visual` 时必然有值，反之必然没有。
    /// **别直接改它**，走 [`App::enter_visual`] / [`App::leave_visual`]，
    /// 否则就会造出「在 Visual 里却没有锚点」这种要么 panic 要么乱选的状态。
    selection_anchor: Option<usize>,
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
    /// 是否有 `:fmt` 正在后台跑。
    ///
    /// 跟 `checking` **一模一样**的立场：只记「有没有」，不持有那个通道。
    /// 为什么这条纪律要紧，见上面 `checking` 那段 —— 总之 `App` 一旦拿着
    /// 活着的东西（进程、线程、通道），`App::new()` 就不再是纯内存对象，
    /// 那一整套测试全得陪葬。
    pub formatting: bool,
    /// 服务器推来的、**当前这个文件**的诊断。
    ///
    /// 跟 `checking` 同一个立场：只存**数据**，不存客户端。
    /// UI 靠它把行号染成红/黄，`:errors` 靠它列清单。
    ///
    /// ⚠️ 这是「这个文件**现在**的全部问题」，不是「历次问题的累积」——
    /// 服务器每次推的是前者，所以换回来时是**整体替换**
    /// （见 [`App::set_diagnostics`]）。
    pub diagnostics: Vec<Diagnostic>,
    /// 撤销 / 重做栈（私有：外部只通过 `undo()` / `redo()` 使用）
    history: UndoStack,
    /// 为 true 时，内部编辑原语不再各自记录撤销步。
    /// 用于「粘贴」这类一次按键包含多次插入的整体编辑，保证整段粘贴只占一步。
    history_locked: bool,
    /// 进虚拟视图（`:errors`）之前的样子，退出来时原样放回去。
    ///
    /// **只在当前处在虚拟视图里时才有值** —— 真正的文档一换就被丢掉
    /// （见 [`App::replace_document`]），不然退出虚拟视图会把一个早就
    /// 不该回去的旧文档翻出来。
    saved: Option<Box<SavedDocument>>,
}

/// 一个文档的完整快照。
///
/// ## 为什么是快照，而不是「退出去时重新打开那个文件」
///
/// 因为**重新读盘会丢掉没保存的改动**。而且 `q` 那条路上还有一层拦截
/// （没保存就不许走），于是你会被堵在清单里出不来 —— 两头都是坏事。
///
/// `:errors` 根本没有理由碰你的文档：它只是换了个东西给你看。
///
/// ## 它真的不贵
///
/// 看上去是「复制一份文档」，其实不是：`Buffer` 里是 rope + `Arc`，
/// `clone()` 是 O(1)（见 `undo.rs` 里快照那段注释），撤销栈里每份快照也一样。
/// 所以这里存的是**几个指针**，不是几份文本。
#[derive(Debug, Clone)]
struct SavedDocument {
    file_path: Option<String>,
    kind: DocumentKind,
    buffer: Buffer,
    cursor: Cursor,
    viewport: Viewport,
    /// 诊断跟着文档一起存 —— 清单就是**从它生成**的，退回那个文件时标记还得在
    diagnostics: Vec<Diagnostic>,
    history: UndoStack,
    /// `dirty` 必须跟着存：它由撤销栈的修订号算出来，丢了它就会
    /// 「改过的文件看起来像没改过」，然后 `:q` 不再拦你 —— 直接丢数据。
    dirty: bool,
    mode: EditorMode,
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
            selection_anchor: None,
            file_path,
            config,
            config_path: None,
            documents: DocumentList::new(),
            kind: DocumentKind::default(),
            checking: false,
            formatting: false,
            diagnostics: Vec::new(),
            history: UndoStack::new(DEFAULT_UNDO_LIMIT),
            history_locked: false,
            saved: None,
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
        // ⚠️ 诊断描述的是**某一个文件**的文本，所以只在换成**另一个文件**时才清。
        //
        // 看上去「换文档就清掉」更保险，但那会捅出一个很阴的洞：从 `:errors`
        // 退回原文件时（那次也是走 `replace_document`）诊断会被清掉，
        // 而服务器**不会**再推一份 —— 它的文本一个字都没变，`Session::show`
        // 什么都不发。于是行号上的标记凭空消失，直到你下一次敲键才回来。
        // 那个 bug 的手感是：「我看了一眼 :errors，回来代码就没红点了，
        // 打一个字又有了。」
        if self.file_path.as_deref() != Some(file_path.as_str()) {
            self.diagnostics.clear();
        }
        // 真正的文档一换，虚拟视图那份快照就没意义了 —— 留着它的话，
        // 以后某次「退出虚拟视图」会把一个早就不该回去的旧文档翻出来
        self.saved = None;
        self.buffer = Buffer::from_str(&content);
        self.file_path = Some(file_path);
        self.mode = EditorMode::ReadOnly;
        self.selection_anchor = None;
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

    /// 用**同一份文档**的新文本整体换掉内容（`:fmt` 用）。
    ///
    /// ## ⚠️ 跟 [`App::replace_document`] 是两件事，别混
    ///
    /// | | 说的是什么 | 撤销历史 | 光标 |
    /// |---|---|---|---|
    /// | `replace_document` | 换成**另一份**文档了 | 清空 | 归零 |
    /// | `replace_all_text` | 还是**这一份**，只是被重排过 | **留着** | 尽量不动 |
    ///
    /// 格式化**必须**能按 `u` 撤回来。清掉撤销栈就等于「`:fmt` 一次，
    /// 之前所有的编辑全锁死」—— 而排版是**可以有主观看法的**（长表达式怎么折、
    /// 换行放哪），用户不满意时必须能一键退回去。
    ///
    /// 返回**是否真的改了**。一个字都没变时**不记撤销步**：否则连敲两次
    /// `:fmt` 会留下一个「撤了也没变化」的空步，用户得按两次 `u`
    /// 才能退掉一次真正的编辑。
    pub fn replace_all_text(&mut self, text: &str) -> bool {
        // 比的是**内容**，不是 `Buffer` 的指针 —— `Buffer::from_str` 每次都造新的
        if self.buffer.to_string() == text {
            return false;
        }
        self.record_standalone_edit();
        self.buffer = Buffer::from_str(text);
        // 排版会改行数（长表达式折开、空行合并），光标可能落到文件外面
        self.clamp_cursor_to_buffer();
        self.sync_dirty();
        true
    }

    // ---------- 虚拟视图（`:errors` / `:ls`） ----------

    /// 把一份清单铺到屏幕上（只读）。
    ///
    /// `kind` 必须是[虚拟种类](DocumentKind::is_virtual) —— 它决定标题怎么写，
    /// 也决定退出时能不能回到原来那份文档。
    ///
    /// ⚠️ 它**不动 `file_path`**，也**不动 `diagnostics`**：
    ///
    /// - `file_path` 留着，`current_directory()` 才继承得到那个文件的目录，
    ///   标题也才说得清「这是谁的问题」
    /// - `diagnostics` 留着 —— `:errors` 那份清单就是**从它生成**的，
    ///   而且退回那个文件时标记还得在（见 [`App::replace_document`]）
    ///
    /// 进它之前的那份文档会被整个存下来（见 [`SavedDocument`]），
    /// 退出时由 [`App::restore_document`] 原样放回去。
    ///
    /// ⚠️ 内容是**快照**：铺上之后就不管了。清单要在里面做的事（比如诊断又变了）
    /// 不会让它自己刷新 —— 想看新的就再敲一次那个命令。
    pub fn show_list(&mut self, kind: DocumentKind, content: String) {
        // 只接受虚拟种类。传 `File` 进来会让「这是个虚拟视图」这个前提悄悄失效，
        // 而上面那一大串基于它的判断（不保存、不同步、不染诊断）全都跟着错。
        debug_assert!(kind.is_virtual(), "show_list 只用来铺虚拟清单");

        // ⚠️ 已经在虚拟视图里就**别覆盖快照** —— 连着敲两次 `:errors`
        //    会把「清单自己」存成快照，于是退出去是退回清单上，
        //    再退一次才回得到文档，而用户按的明明是同一件事。
        if !self.kind.is_virtual() {
            self.saved = Some(Box::new(SavedDocument {
                file_path: self.file_path.clone(),
                kind: self.kind,
                buffer: self.buffer.clone(),
                cursor: self.cursor,
                viewport: self.viewport,
                diagnostics: self.diagnostics.clone(),
                history: self.history.clone(),
                dirty: self.dirty,
                mode: self.mode,
            }));
        }

        self.buffer = Buffer::from_str(&content);
        self.mode = EditorMode::ReadOnly;
        self.selection_anchor = None;
        self.cursor = Cursor::default();
        self.viewport = Viewport::default();
        self.command_input.clear();
        self.status_message.clear();
        self.kind = kind;
        // 撤销历史在这里也清掉：清单是只读的，而按 `u` 把**文档**的内容
        // 变回清单里来（kind 却还是清单）是个说不清的状态
        self.history.reset();
        self.history_locked = false;
        self.sync_dirty();
    }

    /// 从虚拟视图退回原来那份文档；没进过虚拟视图时返回 `false`。
    ///
    /// 退回去是**原样放回快照**，不是重新读盘 —— 理由见 [`SavedDocument`]。
    pub fn restore_document(&mut self) -> bool {
        let Some(saved) = self.saved.take() else {
            return false;
        };
        self.file_path = saved.file_path;
        self.kind = saved.kind;
        self.buffer = saved.buffer;
        self.cursor = saved.cursor;
        self.viewport = saved.viewport;
        self.diagnostics = saved.diagnostics;
        self.history = saved.history;
        self.dirty = saved.dirty;
        // 快照里的模式不该是「行选择」：进虚拟视图必经 `:` 或 Enter，
        // 而那两条路都已经把锚点清掉了。这里兜一道底 —— 万一将来多出一条路，
        // 也不会恢复出一个「在 Visual 里却没有锚点」的怪状态。
        self.mode = match saved.mode {
            EditorMode::Visual => EditorMode::ReadOnly,
            other => other,
        };
        self.selection_anchor = None;
        self.command_input.clear();
        self.status_message.clear();
        true
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

    /// 根据修订号刷新 `dirty` 字段（ui.rs 直接读这个字段）。
    ///
    /// ⚠️ `dirty` 是**算出来的缓存**，真相是撤销栈的修订号 —— 所以凡是绕过
    /// 下面那些编辑原语、自己直接动 `buffer` 的代码，都得自己调一下它。
    /// 忘了的代价不是「界面不好看」：文件改过却显示没改，`:q` 就不再拦你，
    /// 于是**改动直接没了**。（`:delete` / `:swap` 就这么错过一次。）
    pub(crate) fn sync_dirty(&mut self) {
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
        // 离开行选择模式就得丢掉锚点 —— 模式和锚点必须同生同死。
        // 留一个孤儿锚点的话，下次按 `V` 会长出一个「从前一次选区」开始的奇怪选区。
        if mode != EditorMode::Visual {
            self.selection_anchor = None;
        }
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

    // ---------- 行选择模式（`V`） ----------

    /// 进入行选择模式：锚点钉在**光标当前那一行**。
    ///
    /// `V` 再按一下是退出 —— 那不是这个方法的事，见 [`App::leave_visual`]。
    pub fn enter_visual(&mut self) {
        self.history.break_merge();
        self.selection_anchor = Some(self.cursor.row);
        self.mode = EditorMode::Visual;
    }

    /// 退出行选择模式（`Esc`、或者某个动作已经做完了）。
    ///
    /// 回到只读模式而不是「从哪来回哪去」：`V` 只有一个入口（只读模式），
    /// 所以出口也只该有一个。
    pub fn leave_visual(&mut self) {
        self.history.break_merge();
        self.selection_anchor = None;
        self.mode = EditorMode::ReadOnly;
    }

    /// 选区覆盖的行范围（0 基、含两端）；不在行选择模式时是 `None`。
    ///
    /// 两端都会**随用随夹** —— 文档变短（比如上一句刚删了几行）时锚点可能落到
    /// 文末之外，这里顺手夹住，不留一个「记得同步」的状态。
    pub fn selection(&self) -> Option<(usize, usize)> {
        if self.mode != EditorMode::Visual {
            return None;
        }
        let anchor = self.selection_anchor?;
        let last_row = self.buffer.get_line_count().saturating_sub(1);
        let a = anchor.min(last_row);
        let b = self.cursor.row.min(last_row);
        Some((a.min(b), a.max(b)))
    }

    /// 删掉 `[first, last]` 这几行（0 基、含两端），作为**一步可撤销的编辑**。
    ///
    /// ## 为什么它住在 app.rs，而不在 commands.rs
    ///
    /// 因为它必须一次做全三件事，少做哪件都是 bug：
    ///
    /// 1. **记撤销步**（[`App::begin_undoable_command`]）
    /// 2. 改 buffer（一次删整段，避开「删一行下标前移」那种错位）
    /// 3. **同步 `dirty`**（[`App::sync_dirty`]）
    ///
    /// ⚠️ 第 3 件曾经漏过：`:delete` / `:swap` 只做了前两件，于是**文件改过却
    /// 显示没改** —— 接着 `:q` 不拦你，改动直接没了。捏成一个方法之后，
    /// 就没有「第二个调用方忘了其中一件」的机会了。
    ///
    /// 返回 `false` = 越界**或区间倒着给**（这时不留空撤销步、光标也不动）。
    pub fn delete_row_range(&mut self, first: usize, last: usize) -> bool {
        // 倒着的区间不是「删一行」，是调用方弄错了。命令层那边会先报一句
        // 「起必须 <= 止」，但那是**给人看的**；真正拦下来的应该是这里。
        if first > last {
            return false;
        }
        let count = last - first + 1;
        self.begin_undoable_command();
        if !self.buffer.delete_lines(first, count) {
            // 没真删成 → 把刚才那一步回滚掉，不留「撤销了却什么都没变」的空步
            self.abort_undoable_command();
            return false;
        }
        self.buffer.ensure_at_least_one_line(); // 删光后保留一个空行
        // 停在**被删掉的那一段的原地**：这是唯一一个「接着再删一次」
        // 还说得通的位置（vim 也把光标撂在这儿）
        self.cursor.row = first;
        self.cursor.col = 0;
        self.clamp_cursor_to_buffer();
        self.sync_dirty();
        true
    }

    /// 把 `[first, last]` 这一段整体**上移一行**（`up`）或**下移一行**，一步可撤销。
    ///
    /// ## 它是「用 swap 搭出来的」
    ///
    /// 这里**没有**第二个「移动行」的原语：一段 N 行的块挪一格，就是把它逐行跟邻居
    /// 交换 N 次（冒泡排序里那一下）。所以用的就是 `:swap` 那个 [`Buffer::swap_lines`] ——
    /// 一行新机制都没加。
    ///
    /// ⚠️ 两个方向的**交换顺序不同**，都从「被跳过的那一端」开始：
    /// 上移时从 first 往 last 走，下移时反过来。顺序写反了会把块里的行推散。
    ///
    /// 返回 `false` = 区间不合法，或者**已经贴到文件头/尾**了（那时什么都不该动）。
    pub fn move_row_range(&mut self, first: usize, last: usize, up: bool) -> bool {
        let line_count = self.buffer.get_line_count();
        if first > last || last >= line_count {
            return false;
        }
        // 顶上那一段不能再上移，底下那一段不能再下移
        if (up && first == 0) || (!up && last + 1 >= line_count) {
            return false;
        }

        self.begin_undoable_command();
        if up {
            for row in first..=last {
                self.buffer.swap_lines(row - 1, row);
            }
        } else {
            for row in (first..=last).rev() {
                self.buffer.swap_lines(row, row + 1);
            }
        }

        // 光标（和选区锚点）跟着走 —— 否则连按两下 `l`，「这一段」就换人了。
        // 上面的边界检查保证了这里不会越界：上移时 first >= 1、下移时 last + 1 < 行数。
        self.cursor.row = shifted_row(self.cursor.row, first, last, up);
        self.selection_anchor = self
            .selection_anchor
            .map(|anchor| shifted_row(anchor, first, last, up));
        self.sync_dirty();
        true
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
            // 普通文件 → 它所在的目录。
            // `:errors` 的清单**继承**它来自的那个文件的目录 —— 这样你在清单里
            // 敲 `:open 文件名` 仍然找得到地方，而不是掉回进程的工作目录
            // （那个目录是隐形的，用户看不见它在哪）。
            //
            // `:lsp` 那份清单也一样：它是从「你刚才在看的那个文件」那儿开的，
            // 所以基准该是那个文件所在的目录。
            DocumentKind::File
            | DocumentKind::Errors
            | DocumentKind::DocumentList
            | DocumentKind::LspStatus => Path::new(path)
                .parent()
                // ⚠️ 空串要当成「没有」，不能当真目录交给调用方。
                //
                //    原因：`Path::new("x.c").parent()` 给的是 `Some("")` **不是**
                //    `None`。当「拼相对路径的基准」用时它恰好无害
                //    （`"".join("a.rs")` = `"a.rs"`，和退回进程 cwd 一个效果），
                //    但它作为一个**目录答案**是假的 —— 而一旦有人拿它去
                //    `Command::current_dir`，子进程根本起不来
                //    （Windows: os error 123「文件名、目录名或卷标语法不正确」）。
                //    这个 bug 是 `:fmt` 第一版炸出来的：拿相对名字打开文件时
                //    `Format: FAILED — cannot run clang-format: ... (os error 123)`。
                .filter(|dir| !dir.as_os_str().is_empty())
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

    /// Enter：在光标处断行，光标移到新行开头（第 0 列）。
    ///
    /// ## ⚠️ 这里**不做任何缩进预测**（2026-09-19 用户拍板）
    ///
    /// 新行就是空行，缩进全归用户自己打。
    ///
    /// 曾经按 VS Code 的档位做过「继承上一行缩进 / 开括号多进一级 / 闭括号退一级」，
    /// 试完被撤了。不是实现的问题，是**这个位置拿不到判断所需的信息**：
    /// 终端把手按的回车和粘贴内容里的换行送成**同一个** `KeyCode::Enter`
    /// （Windows 上 `Event::Paste` 根本不会到，详见 `event.rs`），
    /// 于是「这段文字是我打的还是粘的」永远只能猜。猜错的代价是
    /// 用户打的缩进和猜的缩进叠在一起 —— 比不猜糟得多。
    ///
    /// 现在只有**用户明确按下的键**会改变缩进：Tab 键插 `config.tab_width` 个空格
    /// （见 `update.rs`），其余时候我们一个空格都不加。
    pub fn split_line_at_cursor(&mut self) {
        self.record_standalone_edit();
        self.buffer.split_line_at(self.cursor.row, self.cursor.col);
        self.cursor.row += 1;
        self.cursor.col = 0;
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
    /// ✅ 复用 `split_line_at_cursor` 现在是**安全**的：它只断行、不猜缩进，
    /// 所以粘进来的每行的前导空白就是它自己的，不会被加上第二层。
    /// ⚠️ 这条前提是一道**暗门**：哪天又给 Enter 加回自动缩进，粘贴就会双缩进，
    /// 而在 Windows 上**没有一个测试拦得住** —— 测试能造出 `Event::Paste`，真终端不能。
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

    // ---------- 诊断 ----------

    /// 换上一批新诊断（服务器说「这个文件现在是这样」）。
    ///
    /// ⚠️ 是**整体替换**，不是追加 —— 服务器每次推的都是「这个文件现在的
    /// *全部*问题」，追加的话**改好的错误永远擦不掉**。而这类 bug 最阴的地方在于
    /// 它表现为「什么都没发生」：你只会觉得工具坏了，不会想到是这里多了一行 `extend`。
    ///
    /// ## ⚠️ 快照里那份要**一起换**
    ///
    /// 屏幕上是一份清单（虚拟视图）时，[`App::restore_document`] 会把
    /// 快照**整个**放回来，包括快照里的诊断。只换 `self.diagnostics` 的话，
    /// 刚才那条推送会在你按 `q` 的那一刻被旧的盖回去 —— 于是表现成
    /// 「诊断偶尔会消失」，而且只在「清单开着的时候服务器刚好推了东西」时出现。
    ///
    /// 只更新**同一个文件**的那份快照：从清单里 `:open` 去了别的文件再 `q` 回来时，
    /// 快照说的正是要回去的那个文件，两边必须一致。
    pub fn set_diagnostics(&mut self, diagnostics: Vec<Diagnostic>) {
        self.diagnostics = diagnostics;

        if let Some(saved) = self.saved.as_mut()
            && saved.file_path == self.file_path
        {
            saved.diagnostics = self.diagnostics.clone();
        }
    }

    /// 这一行上**最严重**的那条诊断（同一行有好几条时错误优先于警告）。
    ///
    /// 为什么返回最严重的一条而不是全部：行号栏只有一个格子，只能染一种颜色。
    /// 一个格子上同时有错误和警告时，**先看错误** —— 这就是选最严重的理由。
    /// 想看全部就上 `:errors`。
    pub fn diagnostic_at_row(&self, row: usize) -> Option<&Diagnostic> {
        self.diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.line == row)
            .max_by_key(|diagnostic| diagnostic.severity.weight())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::Severity;

    #[test]
    fn enter_never_guesses_the_indent() {
        let mut app = App::from_content(None, "    int a;".to_string());
        app.set_mode(EditorMode::Edit);
        app.cursor = Cursor { row: 0, col: 10 };
        app.split_line_at_cursor();
        assert_eq!(
            app.buffer.get_line(1).as_deref(),
            Some(""),
            "新行必须是空的"
        );
        assert_eq!(app.cursor, Cursor { row: 1, col: 0 });
        app.insert_char_at_cursor('x');
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("x"));
    }

    /// 光标落在缩进**里面**时回车 —— 这里正是「阶梯」那个 bug 的现场。
    ///
    /// 老实现的公式是「整行缩进 + 光标右边那段」，而光标右边那段的**开头就是缩进本身**，
    /// 同一段空白被算两遍（4 个空格变 8 个），于是一层层往右爬。
    /// 现在不猜了，那段空白原样退到下一行，一个字符都不多。
    #[test]
    fn enter_inside_the_indent_adds_nothing() {
        let mut app = App::from_content(None, "    int a;".to_string());
        app.cursor = Cursor { row: 0, col: 0 };
        app.split_line_at_cursor();
        assert_eq!(app.buffer.get_line(0).as_deref(), Some(""));
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("    int a;"));
        assert_eq!(app.cursor, Cursor { row: 1, col: 0 });
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

    /// 外部提供者送回来的整份新文本，走的是[`App::replace_all_text`]。
    ///
    /// ⚠️ 这一条盯的是它跟 `replace_document` 的**分界**：那个换的是**另一份文档**
    /// （清撤销栈、光标归零），这个换的是**同一份文档的内容**。
    /// 混成一个的话，`:fmt` 一次就会把你之前所有的编辑全锁死 ——
    /// 而排版是可以有主观看法的（长表达式怎么折、换行放哪），
    /// 用户不满意时**必须**能一键退回去。
    #[test]
    fn replace_all_text_is_one_undo_step() {
        let mut app = App::from_content(Some("a.rs".to_string()), "fn a(){let x=1;}".to_string());

        assert!(app.replace_all_text("fn a() {\n    let x = 1;\n}\n"));

        assert_eq!(app.buffer.get_line(0).as_deref(), Some("fn a() {"));
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("    let x = 1;"));
        assert_eq!(app.buffer.get_line(2).as_deref(), Some("}"));
        assert!(
            app.dirty,
            "内容变了就必须是脏的，否则 :q 不拦你，改动直接没"
        );

        assert!(app.undo(), "排版必须能一步撤回");
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("fn a(){let x=1;}"));
        assert!(!app.dirty);
    }

    #[test]
    fn replace_all_text_with_the_same_content_is_not_an_edit() {
        let mut app = App::from_content(None, "fn a() {}\n".to_string());
        assert!(!app.replace_all_text("fn a() {}\n"), "一个字没变就不算改过");

        // ⚠️ 没变的时候**不许**留下撤销步：留了的话，连敲两次 `:fmt` 之后
        //    要按两次 `u` 才能退掉一次真正的编辑（第一次按下去像卡住了）。
        //    这是 `replace_all_text` 自己返回值、而不是让调用方去比的原因。
        assert!(!app.undo(), "不该有可撤销的东西");
    }

    #[test]
    fn replace_all_text_never_leaves_the_cursor_outside_the_file() {
        // 排版会改行数（长表达式折开、空行合并）。光标要是留在第 5 行
        // 而新内容只剩 2 行，它就落到文件外面了 —— 下一次按键会写错地方。
        let mut app = App::from_content(None, "a\nb\nc\nd\ne".to_string());
        app.cursor = Cursor { row: 4, col: 1 };

        app.replace_all_text("a\nb");

        assert!(
            app.cursor.row < app.buffer.get_line_count(),
            "光标跑到文件外面了"
        );
        assert!(app.cursor.col <= app.buffer.get_char_count(app.cursor.row));
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

    // ---------- 行选择（`V`） ----------

    /// 造一个带内容的 App 并把光标放到第 `row` 行（0 基）
    fn app_with_cursor(content: &str, row: usize) -> App {
        let mut app = App::from_content(None, content.to_string());
        app.cursor = Cursor { row, col: 0 };
        app
    }

    #[test]
    fn the_selection_covers_every_row_between_the_anchor_and_the_cursor() {
        let mut app = app_with_cursor("a\nb\nc\nd", 1);
        app.enter_visual();
        assert_eq!(app.selection(), Some((1, 1)), "刚进来只有一行");

        app.move_cursor_by(1, 0);
        assert_eq!(app.selection(), Some((1, 2)), "往下拉就是扩");

        // 越过锚点就换了一端 —— 锚点没必要跟着倒手
        app.move_cursor_by(-2, 0);
        assert_eq!(app.selection(), Some((0, 1)));
    }

    #[test]
    fn the_selection_is_clamped_when_the_document_shrinks() {
        // 锚点就是「一个行号」，文档变短后可能指到文末之外。
        // 随用随夹，就不留一个「记得跟着改」的状态。
        let mut app = app_with_cursor("a\nb\nc\nd", 3);
        app.enter_visual();
        assert_eq!(app.selection(), Some((3, 3)));

        assert!(app.delete_row_range(0, 2)); // 后面只剩第 3 行（现在成了第 0 行）
        assert_eq!(app.selection(), Some((0, 0)));
    }

    #[test]
    fn leaving_visual_drops_the_selection() {
        let mut app = app_with_cursor("a\nb\nc", 0);
        app.enter_visual();
        app.move_cursor_by(1, 0);
        app.leave_visual();
        assert_eq!(app.mode, EditorMode::ReadOnly);
        assert_eq!(app.selection(), None);

        // 另一个出口（`set_mode`）也必须把锚点清掉 —— 否则下次 `V` 会长出
        // 一个「从前一次选区」开始的怪选区
        app.enter_visual();
        app.set_mode(EditorMode::Edit);
        assert!(app.selection_anchor.is_none());

        app.enter_visual();
        assert_eq!(app.selection(), Some((1, 1)), "重新进来应该只有光标那一行");
    }

    #[test]
    fn delete_row_range_is_one_undo_step_and_marks_dirty() {
        let mut app = app_with_cursor("a\nb\nc\nd", 0);
        assert!(app.delete_row_range(1, 2));
        assert_eq!(app.buffer.get_line_count(), 2);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("a"));
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("d"));
        assert!(app.dirty, "改过就得变脏，否则 `:q` 会放走改动");
        assert_eq!(app.cursor, Cursor { row: 1, col: 0 }, "停在被删那段的原地");

        assert!(app.undo(), "整段删是一步，一次撤销就该全回来");
        assert_eq!(app.buffer.get_line_count(), 4);
        assert!(!app.dirty, "撤销回原样 → 又干净了");
    }

    #[test]
    fn deleting_past_the_end_changes_nothing_and_leaves_no_undo_step() {
        let mut app = app_with_cursor("a\nb", 0);
        assert!(!app.delete_row_range(0, 5));
        assert_eq!(app.buffer.get_line_count(), 2);
        assert!(!app.dirty);
        assert!(
            !app.undo(),
            "失败的操作不该留下「撤销了却什么都没变」的空步"
        );

        // 区间倒着给也算「不行」—— 不能默默当成「删一行」
        assert!(!app.delete_row_range(1, 0));
        assert_eq!(app.buffer.get_line_count(), 2);
        assert!(!app.dirty);
    }

    #[test]
    fn moving_a_block_keeps_it_together() {
        let mut app = app_with_cursor("a\nb\nc\nd", 1);
        assert!(app.move_row_range(1, 2, true), "b、c 上移");
        assert_eq!(app.buffer.to_string(), "b\nc\na\nd");
        assert!(app.move_row_range(0, 1, false), "再下移回去");
        assert_eq!(app.buffer.to_string(), "a\nb\nc\nd");
    }

    #[test]
    fn a_move_is_one_undo_step_and_carries_the_cursor_along() {
        let mut app = app_with_cursor("a\nb\nc\nd", 2);
        app.enter_visual();
        app.move_cursor_by(1, 0); // 圈住 c、d（第 2-3 行）
        assert_eq!(app.selection(), Some((2, 3)));

        assert!(app.move_row_range(2, 3, true));
        assert_eq!(app.buffer.to_string(), "a\nc\nd\nb");
        assert_eq!(app.cursor.row, 2, "光标跟着块走");
        assert_eq!(app.selection(), Some((1, 2)), "选区也一起走");
        assert!(app.dirty);

        assert!(app.undo(), "整次移行是一步");
        assert_eq!(app.buffer.to_string(), "a\nb\nc\nd");
        assert_eq!(app.cursor.row, 3);
        assert!(!app.dirty);
    }

    #[test]
    fn moving_stops_at_the_file_edges() {
        let mut app = app_with_cursor("a\nb", 0);
        assert!(!app.move_row_range(0, 0, true), "最上面那行不能再上移");
        assert!(!app.move_row_range(1, 1, false), "最下面那行不能再下移");
        assert!(!app.move_row_range(1, 0, true), "区间倒着给也不行");
        assert_eq!(app.buffer.to_string(), "a\nb");
        assert!(!app.dirty);
        assert!(!app.undo(), "没动过就不该留下撤销步");
    }

    #[test]
    fn deleting_every_row_leaves_exactly_one_empty_line() {
        let mut app = app_with_cursor("a\nb\nc", 0);
        assert!(app.delete_row_range(0, 2));
        assert_eq!(app.buffer.get_line_count(), 1);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some(""));
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

    // ---------- 诊断 ----------

    fn error_on(line: usize) -> Diagnostic {
        Diagnostic {
            line,
            severity: Severity::Error,
            message: format!("broken on line {line}"),
        }
    }

    fn warning_on(line: usize) -> Diagnostic {
        Diagnostic {
            line,
            severity: Severity::Warning,
            message: format!("suspicious on line {line}"),
        }
    }

    #[test]
    fn a_new_file_has_no_diagnostics_at_all() {
        let app = App::from_content(None, "fn main() {}".to_string());
        assert!(app.diagnostics.is_empty());
        assert!(app.diagnostic_at_row(0).is_none());
    }

    /// ⚠️ 这条守着「整体替换」。要是哪天改成追加，改好的错误就会永远留在屏幕上
    /// —— 而且表现为「什么都没发生」，你根本不会怀疑到这里。
    #[test]
    fn a_new_push_replaces_the_old_diagnostics_instead_of_piling_up() {
        let mut app = App::from_content(None, "a\nb".to_string());
        app.set_diagnostics(vec![error_on(0), warning_on(1)]);
        assert_eq!(app.diagnostics.len(), 2);

        // 服务器说「现在只有第二行那个警告了」（第一行的错误改好了）
        app.set_diagnostics(vec![warning_on(1)]);

        assert_eq!(app.diagnostics.len(), 1);
        assert!(
            app.diagnostic_at_row(0).is_none(),
            "第一行的错误已经改好了，行号栏必须变回去"
        );
        assert!(app.diagnostic_at_row(1).is_some());
    }

    /// 空推送是「这个文件现在没问题」，行号栏必须整个变干净。
    #[test]
    fn an_empty_push_clears_every_mark() {
        let mut app = App::from_content(None, "a".to_string());
        app.set_diagnostics(vec![error_on(0)]);
        app.set_diagnostics(Vec::new());

        assert!(app.diagnostic_at_row(0).is_none());
    }

    /// 同一行上同时有错误和警告 → **错误说了算**（行号栏只有一个格子）。
    #[test]
    fn the_most_severe_diagnostic_on_a_line_wins() {
        let mut app = App::from_content(None, "a".to_string());
        // 故意把警告放在前面：靠顺序获胜的实现会在这里露馅
        app.set_diagnostics(vec![warning_on(0), error_on(0)]);

        let winner = app.diagnostic_at_row(0).expect("这一行有两条诊断");
        assert_eq!(winner.severity, Severity::Error);
    }

    #[test]
    fn a_diagnostic_is_only_found_on_its_own_line() {
        let mut app = App::from_content(None, "a\nb\nc".to_string());
        app.set_diagnostics(vec![error_on(1)]);

        assert!(app.diagnostic_at_row(0).is_none());
        assert!(app.diagnostic_at_row(1).is_some());
        assert!(app.diagnostic_at_row(2).is_none());
    }

    /// ⚠️ 换文档必须把诊断清掉。
    ///
    /// 不清的话，刚打开的新文件上会挂着一堆行号颜色，而且**找不到原因** ——
    /// 要等下一份推送来了才会被换掉，而那段空窗期里你可能已经在为一个
    /// 根本不存在的错误苦恼了。
    #[test]
    fn opening_another_document_clears_the_previous_files_diagnostics() {
        let mut app = App::from_content(Some("a.rs".to_string()), "a".to_string());
        app.set_diagnostics(vec![error_on(0)]);

        app.replace_document("b.rs".to_string(), "b".to_string());

        assert!(
            app.diagnostics.is_empty(),
            "上一个文件的诊断不能跟到新文件上"
        );
    }

    /// 改设置不是换文档 —— 诊断该留着（内容都没变，问题也还在）。
    #[test]
    fn reloading_the_config_keeps_the_diagnostics() {
        let mut app = App::from_content(None, "a".to_string());
        app.set_diagnostics(vec![error_on(0)]);

        app.apply_config(Config::default());

        assert_eq!(app.diagnostics.len(), 1);
    }

    /// ⚠️ **重新打开同一个文件不该把诊断清掉。**
    ///
    /// 这条守的是从 `:errors` 退回去那一步（那次也是走 `replace_document`）。
    /// 清掉的话，服务器**不会**再推一份 —— 它的文本一个字都没变，
    /// `Session::show` 什么都不发 —— 于是行号上的标记凭空消失，
    /// 直到你下次敲键才回来。
    #[test]
    fn reopening_the_same_file_keeps_its_diagnostics() {
        let mut app = App::from_content(Some("a.rs".to_string()), "a".to_string());
        app.set_diagnostics(vec![error_on(0)]);

        app.replace_document("a.rs".to_string(), "a".to_string());

        assert_eq!(app.diagnostics.len(), 1, "回到同一个文件，标记不该消失");
    }

    // ---------- 虚拟视图（`:errors`） ----------

    /// ⚠️ **清单在屏幕上时到达的诊断，退回文件时必须还在。**
    ///
    /// 这条是手动冒烟测试 A/B 对照抓出来的（2026-09-15）：
    ///
    /// ```text
    /// A：打开坏文件 → 等 → `:errors`          → 报出 1 条错误   ✅
    /// B：打开坏文件 → `:lsp` → `q` → `:errors` → 「No problems」  ❌
    /// ```
    ///
    /// 两处各错一半，合起来才凑成那个现象：
    ///
    /// 1. `main::is_current_file` 那时遇到虚拟视图一律不认 → 推送被**丢掉**；
    /// 2. 就算收了，`restore_document` 又会把快照里那份**旧的**盖回来。
    ///
    /// 而且丢了就**永远丢了** —— 服务器只在文本变了才推，而文本一个字没动。
    /// 于是症状是「诊断偶尔会消失」，只在「清单开着的时候服务器刚好推了东西」
    /// 这一种时序下出现 —— 手动复现要靠运气，所以只能靠这条测试钉住。
    #[test]
    fn diagnostics_that_arrive_while_a_list_is_up_survive_going_back() {
        let mut app = App::from_content(Some("a.rs".to_string()), "one\ntwo".to_string());
        assert!(app.diagnostics.is_empty(), "先得是干净的");

        app.show_list(DocumentKind::Errors, "no problems".to_string());
        // 清单在屏幕上的这段时间里，服务器推来了两条
        app.set_diagnostics(vec![error_on(0), error_on(1)]);

        assert!(app.restore_document());
        assert_eq!(
            app.diagnostics.len(),
            2,
            "退回去看到的是一个假的「干净」文件 —— 那份推送已经不会再来了"
        );
    }

    /// 上面那条的另一半：**快照说的是别的文件时，不能被顺手改掉**。
    ///
    /// 回到的是 a.rs，所以行的标记必须还是 a.rs 那一条。
    ///
    /// ⚠️ 这个状态**从命令层到不了**：换文件走 `replace_document`，它会把快照
    /// 整个清掉（那条路是刻意堵的 —— 见那里的注释）；而清单里 `:w` 又被拒。
    /// 但 `App` 的公开接口（[`App::rename_document`]，另存为那条路）允许它，
    /// 所以守卫得留着 —— 而这个守卫漏了的症状是「回到一个文件，看到的却是
    /// 另一个文件的标记」，屏幕上两份东西都对，只有行号栏在说假话。
    #[test]
    fn a_snapshot_about_another_file_is_left_alone() {
        let mut app = App::from_content(Some("a.rs".to_string()), "a".to_string());
        app.set_diagnostics(vec![error_on(0)]);
        app.show_list(DocumentKind::Errors, "1: error: x".to_string());
        // 快照是 a.rs 的，而「现在这个文件」改成了 b.rs
        app.rename_document("b.rs".to_string());
        // 这是 b.rs 的推送 —— 不该动「a.rs 那份快照」
        app.set_diagnostics(vec![error_on(0), error_on(1), error_on(2)]);

        assert!(app.restore_document());
        assert_eq!(app.file_path.as_deref(), Some("a.rs"));
        assert_eq!(
            app.diagnostics.len(),
            1,
            "回到 a.rs，看到的该是 a.rs 的诊断"
        );
    }

    /// 进去再退出来，文档必须**一个字都没变** —— 包括光标、滚动、
    /// 撤销栈、以及那个「改过没保存」的标记。
    #[test]
    fn leaving_the_error_list_puts_the_document_back_exactly_as_it_was() {
        let mut app = App::from_content(Some("a.rs".to_string()), "one\ntwo\nthree".to_string());
        app.set_diagnostics(vec![error_on(1)]);
        // 弄成「改过、没保存」的样子，再动一下光标和视口
        app.set_mode(EditorMode::Edit);
        app.cursor = Cursor { row: 1, col: 3 };
        app.insert_char_at_cursor('!');
        // 光标和视口放在插入**之后**设，这样它们就是我们要断言的那两个值
        app.cursor = Cursor { row: 2, col: 3 };
        app.viewport = Viewport { top: 1, left: 0 };
        let text_before = app.buffer.to_string();
        assert!(app.dirty, "先得真的改过");

        app.show_list(DocumentKind::Errors, "2: error: broken".to_string());
        assert!(app.restore_document());

        assert_eq!(app.file_path.as_deref(), Some("a.rs"));
        assert_eq!(app.kind, DocumentKind::File);
        assert_eq!(app.buffer.to_string(), text_before, "内容不该变");
        assert_eq!(app.cursor, Cursor { row: 2, col: 3 });
        assert_eq!(app.viewport, Viewport { top: 1, left: 0 });
        assert_eq!(app.diagnostics.len(), 1, "诊断得还在，行号上的标记才回得来");
        // ⚠️ 最要紧的一条：dirty 丢了的话，`:q` 不再拦你 —— 直接丢数据
        assert!(app.dirty, "「改过没保存」这个状态不能丢");
        assert!(app.undo(), "撤销栈也得还在");
    }

    /// 在清单里看到的应该是清单，不是那份文档。
    #[test]
    fn the_error_list_is_what_gets_shown() {
        let mut app = App::from_content(Some("a.rs".to_string()), "one\ntwo".to_string());
        app.set_diagnostics(vec![error_on(1)]);

        app.show_list(DocumentKind::Errors, "2: error: broken".to_string());

        assert_eq!(app.kind, DocumentKind::Errors);
        assert!(app.kind.is_virtual());
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("2: error: broken"));
        // `file_path` 留着 —— 标题和 `current_directory()` 都还得靠它
        assert_eq!(app.file_path.as_deref(), Some("a.rs"));
    }

    /// ⚠️ 连敲两次 `:errors`，退一次就该回到文档上。
    ///
    /// 覆盖快照的话会变成：退一次回到清单、再退一次才回文档 ——
    /// 而用户按的是**同一件事**，凭什么要走两步。
    #[test]
    fn asking_for_the_list_twice_still_leaves_in_one_step() {
        let mut app = App::from_content(Some("a.rs".to_string()), "one\ntwo".to_string());

        app.show_list(DocumentKind::Errors, "first".to_string());
        app.show_list(DocumentKind::Errors, "second".to_string());
        assert!(app.restore_document());

        assert_eq!(app.kind, DocumentKind::File);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("one"));
    }

    /// 从清单里打开了别的文件 → 那份快照就作废了。
    ///
    /// 不作废的话，之后某次「退出虚拟视图」会把一个早就不该回去的旧文档翻出来。
    #[test]
    fn opening_another_file_from_the_list_drops_the_snapshot() {
        let mut app = App::from_content(Some("a.rs".to_string()), "one".to_string());
        app.show_list(DocumentKind::Errors, "whatever".to_string());

        app.replace_document("b.rs".to_string(), "bee".to_string());

        assert!(!app.restore_document(), "换了文件之后不该还能退回旧文档");
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("bee"));
    }

    /// 没进过虚拟视图时退不回去。
    #[test]
    fn there_is_nothing_to_restore_when_no_list_was_opened() {
        let mut app = App::from_content(Some("a.rs".to_string()), "one".to_string());
        assert!(!app.restore_document());
    }

    /// 清单继承它来自的那个文件的目录 —— 在里面敲 `:open 名字` 才找得到地方。
    #[test]
    fn the_list_inherits_the_directory_of_the_file_it_came_from() {
        let mut app = App::from_content(Some("D:\\proj\\src\\a.rs".to_string()), "one".to_string());
        app.show_list(DocumentKind::Errors, "whatever".to_string());

        assert_eq!(app.current_directory().as_deref(), Some("D:\\proj\\src"));
    }

    /// 清单是**我们生成的**文本 —— 绝不能写回磁盘。
    #[test]
    fn the_error_list_is_marked_as_not_a_real_file() {
        let mut app = App::from_content(Some("a.rs".to_string()), "one".to_string());
        app.show_list(DocumentKind::Errors, "whatever".to_string());

        assert!(app.kind.is_virtual());
        assert!(!DocumentKind::File.is_virtual());
        assert!(!DocumentKind::DirectoryListing.is_virtual());
        assert!(DocumentKind::DocumentList.is_virtual());
    }

    /// ⚠️ 两份清单之间来回切，**快照不跟着换** —— `q` 一步就回到文档上。
    ///
    /// 每进一次清单都存一份快照的话：`:ls` → `:errors` → `q` 会退到 `:ls` 上，
    /// 再 `q` 才回文档。而用户按的明明是同一件事（我想回去），凭什么走两步。
    #[test]
    fn switching_between_two_lists_still_comes_back_in_one_step() {
        let mut app = App::from_content(Some("a.rs".to_string()), "one\ntwo".to_string());

        app.show_list(DocumentKind::DocumentList, "1 *a.rs".to_string());
        app.show_list(DocumentKind::Errors, "1: error: boom".to_string());
        assert!(app.restore_document());

        assert_eq!(app.kind, DocumentKind::File);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("one"));
    }
}
