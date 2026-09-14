//! 用户配置 —— `config.rs` 的职责
//!
//! 这个文件回答两个问题：**配置文件长什么样**、**怎么把它读进来**。
//!
//! ## 它是什么
//!
//! 配置文件是「用户偏好」的存放处（行号开不开、Tab 几格、滚动边距多少……），
//! 和文档内容、撤销历史毫无关系。程序**启动时读一次**，把值填进 [`Config`]，
//! 之后运行期间就不再碰磁盘了（`:set` 只改内存里的这份，**不写回文件**）。
//!
//! ## 格式：TOML
//!
//! ```toml
//! # 井号开头是注释
//! show_line_numbers = true
//! tab_width = 4
//! scroll_margin = 3
//! side_scroll_margin = 5//!
//! # 分节要放在文件最后：TOML 里分节之后的键都属于那个分节
//! [colors]
//! text = "green"
//! line_number = "yellow"
//! ```
//!
//! 选 TOML 而不是自己发明语法的理由：
//! - 「文本 → 结构体」交给 `serde`，不用手写解析器和转义规则；
//! - 写错了会告诉你**第几行**错在哪，比自己写的 parse 友好得多；
//! - 加新分节（如 `[colors]`）不用改设计。
//!
//! ## 解析是怎么工作的
//!
//! 「解析」在这里是三层，我们只写了第三层：
//!
//! ```text
//! 文件文本 String
//!    │  ① toml 库：按 TOML 语法切成「键 → 值」的数据树
//!    ▼     （注释、引号、数字、分节这些语法细节都在这一步消化掉）
//! 通用数据树
//!    │  ② serde：靠 #[derive(Deserialize)] 生成的代码，
//!    ▼     按结构体的字段名去树上找键，逐个转成 Rust 类型
//! Config / Colors 结构体
//!    │  ③ 我们写的：validate() 检查取值范围
//!    ▼
//! 可用的 Config
//! ```
//!
//! ② 那层的「字段名 ↔ 键名」不是魔法：`#[derive(Deserialize)]` 会展开成一段
//! 类似 `match 键名 { "tab_width" => ..., 其它 => 报错 }` 的代码，字段名就写在
//! 字面量里。所以**改了结构体字段名就等于改了配置文件里的键名**。
//!
//! 我们额外要操心的只有两处：
//! - 字段缺省时该填什么 → 字段上的 `#[serde(default = "某个函数")]`；
//! - serde 不认识的类型怎么转换 → 自定义函数 [`de_color`]（先当字符串取出，
//!   再交给我们自己的 [`parse_color`]）。
//!
//! ## 怎么找到它
//!
//! 见 [`Config::candidate_paths`]：按顺序试，用**第一个存在**的文件。
//!
//! ## 出错怎么办
//!
//! 文件不存在 → 全用默认值，静默通过。
//! 文件存在但读不动 / 语法错 / 值越界 → **不崩溃**，退回默认值，
//! 并把原因塞进 [`LoadedConfig::warning`]，由 main.rs 显示到状态栏。
//! 配置文件是锦上添花的东西，绝不该让编辑器打不开。
//!
//! ## 依赖方向
//!
//! `Config` 只是**数据 + 加载逻辑**，它不知道 `App` 的存在。
//! 依赖方向是 `app.rs → config.rs`（单向），所以默认值常量定义在这里而不是 app.rs。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ratatui::style::Color;
use serde::Deserialize;

/// 默认是否显示行号
pub const DEFAULT_SHOW_LINE_NUMBERS: bool = true;

/// 默认的一次 Tab 缩进空格数
pub const DEFAULT_TAB_WIDTH: usize = 8;

/// 默认的滚动边距（scrolloff）：光标与视口上/下边缘至少保持的行数
pub const DEFAULT_SCROLL_MARGIN: usize = 3;

/// 默认的横向滚动边距（sidescrolloff）：光标与视口左/右边缘至少保持的列数
pub const DEFAULT_SIDE_SCROLL_MARGIN: usize = 5;

/// 默认最多同时保留几个语言服务器。
///
/// ## 为什么是 3
///
/// 实测（2026-09-14，本机，这个项目）：**一个加载完的 `rust-analyzer`
/// 工作集约 1.2 GB**（另外 VS Code 自己那个同项目的约 0.7 GB）。
/// 拿这个数去乘：3 最坏也就 ~3.6 GB，而且**只在真的打开了三个项目时**才会到。
///
/// ⚠️ **它数的是「进程」，不是「项目」。** 池子的钥匙是
/// 「命令 + 项目根」（见 `lsp::pool::key_of`），所以一个 Rust 和 C 混着的
/// 项目**自己就要两个进程**（`rust-analyzer` + `clangd`；clangd 小得多）。
///
/// 这就是从 2 改成 3 的原因：写 2 的时候，「一个混着 C 的 Rust 项目」
/// 已经把名额吃满了，再跳一次项目就开始来回颠 —— 而颠的代价是每次冷加载
/// 好几秒（rust-analyzer 尤其明显）。写 3 恰好盖住这个形态，
/// 也还盖得住最经典的那个：**库 + 用它的程序**。
///
/// ⚠️ 会话是**用到了才起**的，不是预先养几个 —— 所以你只在一个项目里干活时，
/// `1` 和 `3` 完全一样（都只有一个进程）。这个数字管的是「最多允许几个」。
pub const DEFAULT_LSP_MAX_SERVERS: usize = 3;

/// 默认的正文颜色
pub const DEFAULT_TEXT_COLOR: Color = Color::Green;

/// 默认的行号颜色。
///
/// ⚠️ **暗灰是故意的，不是随手选的。**
///
/// 行号是**背景设施** —— 它的本职是让你能报出「第几行」，而不是吸引眼球。
/// 而颜色在这个界面上是个**稀缺资源**：行号栏只有一个小格子，
/// 既要表示「这是行号」、又要表示「这一行有毛病」。
///
/// 以前行号是黄色的，结果**跟警告色抢**（警告的惯例色就是黄）—— 两者一个 SGR 33
/// 偏橄榄、一个 SGR 93 偏亮，理论上分得开，实际上看着像同一件事。
/// 把行号退成暗灰之后：
///
/// ```text
/// 暗灰 → 这一行没毛病
/// 黄色 → 警告
/// 红色 → 错误
/// ```
///
/// 三种状态一眼分得开，而且代码本身（默认绿色）更突出。
pub const DEFAULT_LINE_NUMBER_COLOR: Color = Color::DarkGray;

/// 默认的「错误」颜色（行号染成它、`:errors` 列表里那一行也是它）。
///
/// 用 `LightRed` 而不是 `Red`：ANSI 的普通红（SGR 31）在深色背景上经常发暗，
/// 而这是要**跳进眼睛里**的东西。而具体到你的终端长什么样，是你那个配色方案
/// 说了算 —— 我们只负责说「这是亮红」。
pub const DEFAULT_ERROR_COLOR: Color = Color::LightRed;

/// 默认的「警告」颜色。
///
/// 它现在能和行号色共存了 —— 因为行号退成了暗灰（见
/// [`DEFAULT_LINE_NUMBER_COLOR`]）。在那之前这两个是抢着用的。
pub const DEFAULT_WARNING_COLOR: Color = Color::LightYellow;

/// 默认的「当前行」背景色。0x303030 就是原来写死的 `Color::Indexed(236)`
/// （256 色盘的 236 号正好是 #303030），换成 RGB 写法是为了让用户能直接看懂和修改。
pub const DEFAULT_CURRENT_LINE_BG: Color = Color::Rgb(0x30, 0x30, 0x30);

/// 配置文件的名字（放在哪个目录都叫这个，用户只用记一个名字）
///
/// 为什么叫这个：加 `stbd-` 前缀是为了在「便携模式」下不跟别人撞名——
/// 那种用法里它就和 `stbd.exe` 挤在同一个目录；用复数 `settings` 是因为
/// 里面装的是一整组设置。同一个名字在三个位置都用，不搞两套。
pub const CONFIG_FILE_NAME: &str = "stbd-settings.toml";

/// 用环境变量直接指定配置文件路径（优先级最高，方便测试和多套配置切换）
pub const CONFIG_PATH_ENV: &str = "STBD_CONFIG";

/// 内置的配置模板 —— 就是项目根目录那份 `stbd-settings.example.toml`，
/// 编译时用 `include_str!` 嵌进二进制。
///
/// 有了它，`:settings` 能在没有配置文件时就地生成一份带注释的模板，
/// 用户不用先跑去仓库里拷。
///
/// ⚠️ 模板里的值必须与 [`Config::default()`] **完全一致**：生成出来的文件
/// 不该改变任何现有行为。测试 `settings_template_matches_the_defaults` 守着这一点。
pub const SETTINGS_TEMPLATE: &str = include_str!("../stbd-settings.example.toml");

/// `tab_width` 允许的最小值（与 `:set tabwidth` 的限制保持一致）
const MIN_TAB_WIDTH: usize = 1;
/// `tab_width` 允许的最大值
const MAX_TAB_WIDTH: usize = 16;

/// `lsp_max_servers` 的上限。
///
/// 纯属**防手滑**：这个值本身没什么道理，但 1.2 GB 一个的进程让你写
/// 一百个肯定不是你的本意，而那种笔误的后果是机器卡死。
const MAX_LSP_MAX_SERVERS: usize = 8;
/// `scroll_margin` / `side_scroll_margin` 允许的最大值
const MAX_MARGIN: usize = 100;

/// 用户偏好。字段全部来自配置文件，缺省时用默认值常量。
///
/// 字段是 public 的（`update.rs` 的 `:set` 会直接改），但**改完不写回磁盘**。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
// 写错 key 名（比如把 tab_width 写成 tabwidth）时直接报错，而不是静默忽略。
// 静默忽略最坑人：用户改了文件却发现「没生效」，还找不到原因。
#[serde(deny_unknown_fields)]
pub struct Config {
    /// 是否显示行号（`:set number` / `:set nonumber`）
    #[serde(default = "default_show_line_numbers")]
    pub show_line_numbers: bool,
    /// 一次 Tab 插入的空格数（`:set tabwidth N`）
    #[serde(default = "default_tab_width")]
    pub tab_width: usize,
    /// 光标与视口上下边缘保持的最小行距（`:set scrolloff N`）
    #[serde(default = "default_scroll_margin")]
    pub scroll_margin: usize,
    /// 光标与视口左右边缘保持的最小列距（`:set sidescrolloff N`）
    #[serde(default = "default_side_scroll_margin")]
    pub side_scroll_margin: usize,
    /// 最多同时保留几个语言服务器；**`0` = 完全不开**。
    ///
    /// 为什么不是一个布尔的「开/关」：这个数字天然把两头都盖住了 ——
    /// `0` 是关，`1` 是「只留当前项目」，`2` 是「跳回上一个项目不用等」。
    /// 而且它们走的是**同一份代码**（一个列表 + 一个上限），不是两条路。
    #[serde(default = "default_lsp_max_servers")]
    pub lsp_max_servers: usize,
    /// 各个语言服务器：`[lsp.<语言>]` 一节一个。
    ///
    /// ⚠️ 配置文件里写的那几节是**并进**内置默认，不是替换它 ——
    /// 见 [`Config::prepare_lsp`]。整个替换的话，一份没写 `[lsp]` 的配置
    /// （包括老版本生成的模板）会让「Rust 诊断」凭空消失，
    /// 而原因藏在「配置文件里没写」这种根本想不到的地方。
    ///
    /// ⚠️ 这里的 `default` 只说「这个字段可以不在配置文件里」。内置那三条
    /// **不是**从这儿来的 —— 它们由 [`Config::prepare_lsp`] 并进来，而
    /// `prepare_lsp` 是 [`Config::parse`] 的必经之路，所以那是**唯一**入口。
    ///
    /// 这儿原来写的是 `#[serde(default = "default_lsp")]`，看着像
    /// 「内置值从这儿来」。红检时把 `default_lsp` 摘掉、测试**照样绿**，
    /// 才发现它是句废话 —— 而且比废话更坏：它让「内置表打哪儿来」有了
    /// **两个**似是而非的答案，下一个读到这里的人只会挑一个信。
    /// （`default_lsp` 本身还有用：手写的 [`Config::default`] 要用它。）
    #[serde(default)]
    pub lsp: HashMap<String, LspServer>,
    /// 界面各部分用什么颜色
    #[serde(default)]
    pub colors: Colors,
}

/// 界面各部分的颜色。用户可以逐个指定，没写的用默认值。
///
/// 颜色写法（大小写不敏感，两端空格会被忽略）：
///
/// - **具名色**：`green`、`yellow`、`lightblue`、`darkgray`……（完整表见 [`NAMED_COLORS`]）
/// - **十六进制**：`#ffcc00`，也支持简写 `#fc0`（等于 `#ffcc00`）
/// - **reset**（也可写 `default` / `none`）：用终端默认色，也就是「不染色」
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
// 分区里的键名写错同样直接报错，理由同 Config
#[serde(deny_unknown_fields)]
pub struct Colors {
    /// 正文文字
    #[serde(default = "default_text_color", deserialize_with = "de_color")]
    pub text: Color,
    /// 行号
    #[serde(default = "default_line_number_color", deserialize_with = "de_color")]
    pub line_number: Color,
    /// 有错误的那一行的行号（`:errors` 列表里也用这个色）
    #[serde(default = "default_error_color", deserialize_with = "de_color")]
    pub error: Color,
    /// 有警告的那一行的行号
    #[serde(default = "default_warning_color", deserialize_with = "de_color")]
    pub warning: Color,
    /// 当前行的背景色（想关掉高亮就写 `reset`）
    #[serde(default = "default_current_line_bg", deserialize_with = "de_color")]
    pub current_line_bg: Color,
    /// 文本区的边框和标题
    #[serde(default = "default_border_color", deserialize_with = "de_color")]
    pub border: Color,
    /// `:` 命令输入
    #[serde(default = "default_command_color", deserialize_with = "de_color")]
    pub command: Color,
    /// 只读模式的模式标签（`-- READ-ONLY --`）
    #[serde(default = "default_mode_readonly_color", deserialize_with = "de_color")]
    pub mode_readonly: Color,
    /// 编辑模式的模式标签（`-- EDIT --`）
    #[serde(default = "default_mode_edit_color", deserialize_with = "de_color")]
    pub mode_edit: Color,
    /// 快捷键提示文字
    #[serde(default = "default_hint_color", deserialize_with = "de_color")]
    pub hint: Color,
    /// 底部状态信息
    #[serde(default = "default_status_color", deserialize_with = "de_color")]
    pub status: Color,
}

// ===== serde 的逐字段默认值 =====
//
// 注意：这里必须用「字段级」default，**不能**只在结构体上写 `#[serde(default)]`。
// 结构体级的 default 对缺失字段调用的是**该字段类型**的 Default（usize → 0、
// bool → false），而不是 `Config::default()` 里的值。那样用户只要写了
// show_line_numbers = false，没写的 tab_width 就会变成 0（按 Tab 什么都不插入），
// 是个很难发现的坑。测试 `missing_keys_fall_back_to_config_defaults` 守着这一点。

fn default_show_line_numbers() -> bool {
    DEFAULT_SHOW_LINE_NUMBERS
}

fn default_tab_width() -> usize {
    DEFAULT_TAB_WIDTH
}

fn default_scroll_margin() -> usize {
    DEFAULT_SCROLL_MARGIN
}

fn default_side_scroll_margin() -> usize {
    DEFAULT_SIDE_SCROLL_MARGIN
}

fn default_lsp_max_servers() -> usize {
    DEFAULT_LSP_MAX_SERVERS
}

// 颜色的默认值。每项单独写一个函数，理由同上：
// 字段级 default 才能保证「只写了 [colors] 里的一项」时，其余项仍然是好看的颜色，
// 而不是被重置成 `Color::Reset`。

fn default_text_color() -> Color {
    DEFAULT_TEXT_COLOR
}

fn default_line_number_color() -> Color {
    DEFAULT_LINE_NUMBER_COLOR
}

fn default_error_color() -> Color {
    DEFAULT_ERROR_COLOR
}

fn default_warning_color() -> Color {
    DEFAULT_WARNING_COLOR
}

fn default_current_line_bg() -> Color {
    DEFAULT_CURRENT_LINE_BG
}

fn default_border_color() -> Color {
    Color::Reset
}

fn default_command_color() -> Color {
    Color::Cyan
}

fn default_mode_readonly_color() -> Color {
    Color::DarkGray
}

fn default_mode_edit_color() -> Color {
    Color::Yellow
}

fn default_hint_color() -> Color {
    Color::DarkGray
}

fn default_status_color() -> Color {
    Color::Green
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            text: default_text_color(),
            line_number: default_line_number_color(),
            error: default_error_color(),
            warning: default_warning_color(),
            current_line_bg: default_current_line_bg(),
            border: default_border_color(),
            command: default_command_color(),
            mode_readonly: default_mode_readonly_color(),
            mode_edit: default_mode_edit_color(),
            hint: default_hint_color(),
            status: default_status_color(),
        }
    }
}

/// 认识哪些颜色名。键一律小写（用户写 `Yellow` 会先被转成小写再查表）。
const NAMED_COLORS: &[(&str, Color)] = &[
    ("black", Color::Black),
    ("red", Color::Red),
    ("green", Color::Green),
    ("yellow", Color::Yellow),
    ("blue", Color::Blue),
    ("magenta", Color::Magenta),
    ("cyan", Color::Cyan),
    ("gray", Color::Gray),
    ("grey", Color::Gray),
    ("white", Color::White),
    ("darkgray", Color::DarkGray),
    ("darkgrey", Color::DarkGray),
    ("lightred", Color::LightRed),
    ("lightgreen", Color::LightGreen),
    ("lightyellow", Color::LightYellow),
    ("lightblue", Color::LightBlue),
    ("lightmagenta", Color::LightMagenta),
    ("lightcyan", Color::LightCyan),
    // 常见叫法，顺便支持一下
    ("purple", Color::Magenta),
    ("orange", Color::LightRed),
];

/// 解析颜色字符串。`Err` 里是**给人看的提示**（会被塞进状态栏）。
pub fn parse_color(raw: &str) -> Result<Color, String> {
    let text = raw.trim().to_ascii_lowercase();

    // 「不染色」的几种写法都当作终端默认色
    if matches!(text.as_str(), "reset" | "default" | "none") {
        return Ok(Color::Reset);
    }

    if let Some(hex) = text.strip_prefix('#') {
        return parse_hex_color(hex)
            .ok_or_else(|| format!("invalid color `{raw}`: expected #RRGGBB or #RGB"));
    }

    NAMED_COLORS
        .iter()
        .find(|(name, _)| *name == text)
        .map(|(_, color)| *color)
        .ok_or_else(|| {
            format!("unknown color `{raw}` (use a name like yellow, a hex like #ffcc00, or reset)")
        })
}

/// 解析 `#` 后面的部分：`RRGGBB` 或简写 `RGB`。
fn parse_hex_color(hex: &str) -> Option<Color> {
    // 先确认全是十六进制字符。这步不只是校验：全 ASCII 之后，
    // 下面的字节下标才一定落在字符边界上（否则 `#你好` 这种会直接 panic）。
    if hex.is_empty() || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    // 已经校验过是十六进制 ASCII，取不到数字是不可能的，`unwrap_or(0)` 只是不给 panic 留门
    let digit =
        |index: usize| -> u8 { (hex.as_bytes()[index] as char).to_digit(16).unwrap_or(0) as u8 };

    match hex.len() {
        6 => Some(Color::Rgb(
            digit(0) * 16 + digit(1),
            digit(2) * 16 + digit(3),
            digit(4) * 16 + digit(5),
        )),
        // 简写 #abc 展开成 #aabbcc（d*17 就是 d*16+d）
        3 => Some(Color::Rgb(digit(0) * 17, digit(1) * 17, digit(2) * 17)),
        _ => None,
    }
}

/// 把 TOML 里的字符串反序列化成 [`Color`]。
///
/// serde 不懂 `Color`，所以自己接一下：先按 String 取出来，再走 [`parse_color`]。
fn de_color<'de, D>(deserializer: D) -> Result<Color, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    parse_color(&raw).map_err(<D::Error as serde::de::Error>::custom)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            show_line_numbers: DEFAULT_SHOW_LINE_NUMBERS,
            tab_width: DEFAULT_TAB_WIDTH,
            scroll_margin: DEFAULT_SCROLL_MARGIN,
            side_scroll_margin: DEFAULT_SIDE_SCROLL_MARGIN,
            lsp_max_servers: DEFAULT_LSP_MAX_SERVERS,
            lsp: default_lsp(),
            colors: Colors::default(),
        }
    }
}

/// 一个语言服务器 —— 配置里 `[lsp.<名字>]` 那一节。
///
/// ## 我们**不内置任何语言知识**
///
/// 「`.py` 是 Python」这种判断**不在代码里**，而在配置里（`extensions`）。
/// 理由跟「我们一行分析代码的逻辑都没有」是同一个：哪种后缀算哪种语言，
/// 是**你的工具链**说了算的事，不是编辑器该拍板的。
/// 而且内置了的话，你想给一个我们没听过的语言（Zig、Nim）配服务器就卡住了。
///
/// ## 节名就是语言名
///
/// `[lsp.rust]` 的节名 `rust` 会当作 `didOpen` 里报的 `languageId` ——
/// 除非你写 `language_id` 覆盖它。最常见的情况（节名就是语言名）不用写两遍。
///
/// ⚠️ `languageId` 不是装饰：`clangd` 靠它决定把 `.h` 当 C 还是 C++ 解析。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
// 表里键名写错（`comand`）会直接报错并指出第几行，跟别的配置一个待遇
#[serde(deny_unknown_fields)]
pub struct LspServer {
    /// 配置里那一节的节名（`language_id` 没写时用它兜底）。
    ///
    /// `skip` 是因为它来自表的**键**，不在那一节**里面** ——
    /// serde 反序列化一个 `HashMap<String, LspServer>` 时只看得到值。
    /// 填它的活由 [`Config::prepare_lsp`] 干。
    #[serde(skip)]
    pub name: String,
    /// 起什么命令。**可以是绝对路径**（那个程序不在 PATH 上时用）。
    ///
    /// ⚠️ 留空 = 这条不要了。用来关掉一条内置的（比如你不想要 clangd）。
    pub command: String,
    /// 命令行参数（一个词一个，不用自己加引号）
    #[serde(default)]
    pub args: Vec<String>,
    /// 管哪些扩展名。**小写、不带点**（写 `"rs"`，不是 `".rs"`）。
    ///
    /// 故意**不给默认值**：忘了写的话 serde 会直接报「缺 extensions」并指出行号，
    /// 而给个空默认只会让它安静地什么都匹配不到 —— 那种错最难查。
    pub extensions: Vec<String>,
    /// 往上找这个文件当项目根；**不写 = 不要求项目根**，用文件自己所在的目录。
    ///
    /// 这两种语义差别很大：`rust-analyzer` 必须给一个真项目根（拿一个不包含
    /// 这个文件的目录去糊它，实测 45 秒 0 份推送）；而 `clangd` 自己会往上找
    /// `compile_commands.json`，我们硬要求它反而会把本来能用的项目排除掉。
    #[serde(default)]
    pub root_marker: Option<String>,
    /// `didOpen` 里报的语言名；不写就用节名。
    #[serde(default)]
    pub language_id: Option<String>,
}

impl LspServer {
    /// `didOpen` 里报的语言名。
    pub fn language_id(&self) -> &str {
        self.language_id.as_deref().unwrap_or(&self.name)
    }

    /// 这条服务器管不管带这个后缀的文件。
    ///
    /// 后缀在两边都小写、不带点；空后缀（没有后缀的文件）一律不管 ——
    /// 那属于「按文件名而不是后缀认」的情况，现在不做。
    pub fn handles(&self, extension: &str) -> bool {
        !extension.is_empty()
            && self
                .extensions
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
    }
}

/// 内置的那几条服务器 —— 开箱就有。
///
/// 只放**各自生态里最标准的两个**：
///
/// - `rust-analyzer`：随 `rustup` 就有（`rustup component add rust-analyzer`）
/// - `clangd`：随 LLVM 发行版就有，正好也是「不用微软那套」的选择
///
/// 别的语言**不内置** —— 没装的东西内置进去，「开箱即用」就变成
/// 「开箱报一串找不到」，而那些提示会盖住真正有用的消息。想加就往配置里加一节。
///
/// ## 为什么 C 和 C++ 分两节
///
/// 因为 `languageId` 不一样，而 **`clangd` 靠它决定该怎么解析**。
/// 尤其 `.h`：它既可能是 C 也可能是 C++，我们**不猜** —— 按下面这么写就是当 C 看；
/// 想当 C++ 就把 `"h"` 从 `[lsp.c]` 那行挪到 `[lsp.cpp]`。
///
/// 两节的 `command` 相同，所以池子（按 **命令 + 根** 存会话）只会起**一个** clangd。
fn default_lsp() -> HashMap<String, LspServer> {
    let mut servers = HashMap::new();
    let mut add = |name: &str, command: &str, extensions: &[&str], root_marker: Option<&str>| {
        servers.insert(
            name.to_string(),
            LspServer {
                name: name.to_string(),
                command: command.to_string(),
                args: Vec::new(),
                extensions: extensions.iter().map(|ext| (*ext).to_string()).collect(),
                root_marker: root_marker.map(|marker| marker.to_string()),
                language_id: None,
            },
        );
    };
    add("rust", "rust-analyzer", &["rs"], Some("Cargo.toml"));
    add("c", "clangd", &["c", "h"], None);
    add(
        "cpp",
        "clangd",
        &["cpp", "cc", "cxx", "hpp", "hxx", "hh"],
        None,
    );
    servers
}

/// 文件后缀（小写、不带点）；没有后缀就空串。
///
/// 跟「有没有扩展名」的判定一致：`.gitignore` 这种点开头的**没有后缀**
/// （`Path::extension` 就是这个规矩），所以它不会被当成 `gitignore` 类型的文件。
fn extension_of(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// 加载配置文件的结果：配置本体 + 来源 + 需要提示给用户的问题。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    /// 实际生效的配置（读取或解析失败时是默认值）
    pub config: Config,
    /// 从哪个文件读到的；`None` = 所有候选位置都没有文件，全用默认值
    pub source: Option<PathBuf>,
    /// 需要显示给用户的提示（读不动 / 语法错 / 值越界）；`None` = 一切正常
    pub warning: Option<String>,
}

impl Config {
    /// 解析一段 TOML 文本。只做「文本 → Config + 合法性检查」，不碰磁盘。
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut config: Self = toml::from_str(text).map_err(|err| flatten_error(&err, text))?;
        config.prepare_lsp();
        config.validate()?;
        Ok(config)
    }

    /// 把 `[lsp]` 这张表整理成能用的样子。
    ///
    /// 两件事，顺序不能反：
    ///
    /// 1. **内置的并进来**（配置文件里同名的覆盖内置的）。
    ///    没这一步的话，一份只写了 `[lsp.python]` 的配置会把 Rust 那条踢掉 ——
    ///    现象是「Rust 诊断突然没了」，而矛头会指向完全无关的地方。
    /// 2. **把节名填进每一节**（`language_id` 没写时靠它兜底）。
    ///    节名是表的**键**，不在那一节里面，所以 serde 看不到它。
    fn prepare_lsp(&mut self) {
        for (name, spec) in default_lsp() {
            self.lsp.entry(name).or_insert(spec);
        }
        for (name, spec) in &mut self.lsp {
            spec.name = name.clone();
        }
    }

    /// 这个文件该用哪个服务器（按**扩展名**找）。
    ///
    /// 找不到就返回 `None` —— 那意味着**一个字节都不发**。这一点很要紧：
    /// 以前是「往上找到 `Cargo.toml` 就把文件丢给 `rust-analyzer`」，
    /// 于是在 Rust 项目里打开 `.py`，会拿 rust-analyzer 去解析 Python，
    /// 报出一堆**假的**语法错误（实测：4 条 `expected an item`）。
    /// 假错误比没有诊断糟得多 —— 你会去改本来没错的代码。
    ///
    /// ⚠️ **按名字排一遍再找**。`HashMap` 的遍历顺序是随机的，
    /// 要是两节都声称管 `.h`，「谁赢」会**每跑一次都可能不一样** ——
    /// 那是这类代码里最难查的一种不稳定。排完序之后规则是确定的：
    /// 节名（字典序）靠前的赢。
    ///
    /// `command` 留空的那条**直接跳过** —— 那是「这条我不要了」的写法，
    /// 用来关掉一条内置的（比如你不想要 `clangd`）。
    pub fn server_for(&self, path: &str) -> Option<&LspServer> {
        let extension = extension_of(path);
        let mut named: Vec<(&String, &LspServer)> = self.lsp.iter().collect();
        named.sort_by_key(|(name, _)| name.as_str());
        named
            .into_iter()
            .map(|(_, spec)| spec)
            .filter(|spec| !spec.command.is_empty())
            .find(|spec| spec.handles(&extension))
    }

    /// 按 [`Self::candidate_paths`] 的顺序加载配置。
    ///
    /// 任何一步失败都不会返回 `Err`：错误被翻译成 [`LoadedConfig::warning`]，
    /// 调用方只管把提示显示出来，不必处理失败分支。
    pub fn load() -> LoadedConfig {
        for path in Self::candidate_paths() {
            if !path.is_file() {
                continue;
            }
            return match Self::load_from_file(&path) {
                Ok(config) => LoadedConfig {
                    config,
                    source: Some(path),
                    warning: None,
                },
                // 文件是用户自己写的，错了就明确告诉他错在哪，
                // 不要偷偷跳到下一个候选文件（那会让人更糊涂）。
                Err(message) => LoadedConfig {
                    config: Self::default(),
                    source: Some(path.clone()),
                    warning: Some(format!("{message} (in {}, using defaults)", path.display())),
                },
            };
        }
        LoadedConfig {
            config: Self::default(),
            source: None,
            warning: None,
        }
    }

    /// 从指定文件读取并解析配置。
    pub fn load_from_file(path: &Path) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|err| format!("Cannot read config: {err}"))?;
        Self::parse(&text)
    }

    /// 候选配置文件路径，**按优先级从高到低**排列。
    ///
    /// 1. 环境变量 `STBD_CONFIG` 指定的路径（最灵活，测试与多套配置用）
    /// 2. 可执行文件同目录的 `stbd-settings.toml`（**首选**：配置跟着程序走，见 [`Self::preferred_path`]）
    /// 3. 用户配置目录的 `stbd-settings.toml`（兜底：exe 目录拿不到时用）：
    ///    - Windows：`%APPDATA%\stbd\stbd-settings.toml`
    ///    - 其它：`$XDG_CONFIG_HOME/stbd/stbd-settings.toml`，没有就用 `~/.config/stbd/stbd-settings.toml`
    ///
    /// 候选顺序里**先出现的优先**：所以便携文件会盖掉用户配置，环境变量又盖掉两者。
    pub fn candidate_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();

        if let Some(raw) = std::env::var_os(CONFIG_PATH_ENV)
            && !raw.is_empty()
        {
            paths.push(PathBuf::from(raw));
        }

        if let Some(exe_dir) = executable_dir() {
            paths.push(exe_dir.join(CONFIG_FILE_NAME));
        }

        if let Some(user_path) = Self::user_config_path() {
            paths.push(user_path);
        }

        paths.dedup();
        paths
    }

    /// 「配置文件该放哪」的**首选**位置：**可执行文件旁边**。
    ///
    /// 这是刻意选的：配置跟着程序走，不往系统盘塞东西 —— `stbd.exe` 拷到哪，
    /// 配置就跟到哪（便携）。`:settings` 也在这里生成新文件。
    ///
    /// 只有连可执行文件目录都拿不到时（极罕见的受限环境），才退回用户配置目录，
    /// 见 [`Self::user_config_path`]。
    pub fn preferred_path() -> Option<PathBuf> {
        executable_dir()
            .map(|dir| dir.join(CONFIG_FILE_NAME))
            .or_else(Self::user_config_path)
    }

    /// 确保配置文件存在，并把它的路径返回。`:settings` 靠这个实现。
    ///
    /// - 已经有文件 → 直接用；
    /// - 没有 → 按 [`SETTINGS_TEMPLATE`] 生成一份带注释的模板；
    /// - exe 目录写不进去（例如装在 `Program Files`）→ 退回用户配置目录，
    ///   而且**不静默**：调用方会把最终路径显示到状态栏。
    pub fn ensure_settings_file() -> Result<PathBuf, String> {
        let target =
            Self::preferred_path().ok_or_else(|| "cannot locate a settings path".to_string())?;
        if ensure_file_at(&target).is_ok() {
            return Ok(target);
        }
        let fallback = Self::user_config_path()
            .ok_or_else(|| format!("cannot create {}", target.display()))?;
        ensure_file_at(&fallback)?;
        Ok(fallback)
    }

    /// 兜底位置（用户配置目录）：只在可执行文件目录用不了时登场。
    ///
    /// 这个值**不参与加载**，只用于 `:config path` 提示用户「该把文件建在哪」。
    pub fn user_config_path() -> Option<PathBuf> {
        user_config_dir().map(|dir| dir.join(CONFIG_FILE_NAME))
    }

    /// 一行摘要，供 `:config` 命令展示当前生效的设置。
    pub fn describe(&self) -> String {
        format!(
            "number={} tabwidth={} scrolloff={} sidescrolloff={} lsp={}",
            if self.show_line_numbers { "on" } else { "off" },
            self.tab_width,
            self.scroll_margin,
            self.side_scroll_margin,
            // 0 说成 `off` 比说成 `0` 清楚 —— 它不是「零个」，它就是不开了
            if self.lsp_max_servers == 0 {
                "off".to_string()
            } else {
                self.lsp_max_servers.to_string()
            }
        )
    }

    /// 检查取值范围。越界就是**错误**（跟 `:set tabwidth 999` 一样会报错），
    /// 而不是悄悄夹到边界上——用户写错了应该知道。
    fn validate(&self) -> Result<(), String> {
        if !(MIN_TAB_WIDTH..=MAX_TAB_WIDTH).contains(&self.tab_width) {
            return Err(format!(
                "tab_width must be between {MIN_TAB_WIDTH} and {MAX_TAB_WIDTH}, got {}",
                self.tab_width
            ));
        }
        if self.scroll_margin > MAX_MARGIN {
            return Err(format!(
                "scroll_margin must be between 0 and {MAX_MARGIN}, got {}",
                self.scroll_margin
            ));
        }
        if self.side_scroll_margin > MAX_MARGIN {
            return Err(format!(
                "side_scroll_margin must be between 0 and {MAX_MARGIN}, got {}",
                self.side_scroll_margin
            ));
        }
        if self.lsp_max_servers > MAX_LSP_MAX_SERVERS {
            return Err(format!(
                "lsp_max_servers must be between 0 and {MAX_LSP_MAX_SERVERS}, got {}",
                self.lsp_max_servers
            ));
        }
        for (name, spec) in &self.lsp {
            for extension in &spec.extensions {
                // ⚠️ 犯这个错太容易了 —— 后缀看着就像要带个点。
                //    不报的话它只是安静地什么也匹配不到（`handles` 比的是不带点的），
                //    现象是「我配了 clangd 但 .c 文件没诊断」。
                if extension.starts_with('.') {
                    return Err(format!(
                        "lsp.{name}: extensions must not start with a dot, so write \"c\" rather than \".c\""
                    ));
                }
                if extension.is_empty() {
                    return Err(format!("lsp.{name}: extensions must not be empty"));
                }
            }
        }
        Ok(())
    }
}

/// 确保 `path` 处有一个配置文件：已存在就什么都不做，不存在就写入内置模板。
fn ensure_file_at(path: &Path) -> Result<(), String> {
    if path.is_file() {
        return Ok(());
    }
    std::fs::write(path, SETTINGS_TEMPLATE)
        .map_err(|err| format!("cannot create {}: {err}", path.display()))
}

/// 「程序自己的东西」该放在哪个目录。
///
/// 首选**可执行文件旁边**（便携，不往系统盘塞东西），拿不到就退回用户配置目录。
/// 配置文件和输出文件夹都从这儿长出来 —— 它们该待在一起。
pub fn config_directory() -> Option<PathBuf> {
    executable_dir().or_else(user_config_dir)
}

/// 可执行文件所在目录（`current_exe()` 失败时返回 `None`，比如某些受限环境）。
///
/// 公开是因为「程序自己的东西放哪儿」不止配置文件一处 —— 长输出的
/// 输出文件夹也按同一套走（见 `outbox.rs`）。**一处定义，两边用同一个判据**。
pub fn executable_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

/// 配置里那个 `command` 到底跑不跑得起来 —— 找得到就返回它的**完整路径**。
///
/// ## 它回答的是哪句话
///
/// 「我都没下载这个 LSP，它怎么跑起来的？」以及反过来的「我明明装了，怎么不动？」
///
/// ⚠️ **我们只认 `PATH` 上的命令。** VS Code 扩展**打包在里面**的那些服务器
/// （`pyright`、`jdtls`……）`PATH` 上没有，我们也看不见 —— 所以「装了扩展」
/// 和「我们找得到」是两件不相干的事。这条差别正是那句话的来源：
/// 用户以为「装了扩展就有了」，而我们的答案是「`PATH` 上没有」。
///
/// 判据和终端保持一致：
/// - 命令里**带路径分隔符** → 当路径用（相对路径按当前目录算），不再去 `PATH` 里找；
/// - 否则一项一项找 `PATH`，每一项里先试**原名**，再试 `.exe` / `.cmd` … 后缀。
///
/// 「先试原名」是照 Windows 自己的顺序来的（磁盘上那个文件叫 `clangd.exe`，
/// 但你敲的、以及配置里写的是 `clangd`）。
pub fn which(command: &str) -> Option<PathBuf> {
    let command = command.trim();
    if command.is_empty() {
        // `command = ""` 在配置里的意思是「这条关掉」。它不是「去找一个叫
        // 空字符串的程序」，所以这里不能装作找到什么。
        return None;
    }

    if command.contains(['/', '\\']) {
        // 写成了路径（绝对或相对）→ 就当路径看，不去打扰 PATH
        return candidates(command).into_iter().find(|p| p.is_file());
    }

    let dirs: Vec<PathBuf> = match std::env::var_os("PATH") {
        Some(raw) => std::env::split_paths(&raw).collect(),
        // 没有 PATH（几乎不可能）——那一个裸名字确实无处可找
        None => Vec::new(),
    };
    search_in_path(command, &dirs)
}

/// 在给定的几个目录里找一个命令（**这一半是纯的**，所以能直接测）。
///
/// `which` 只是把 `PATH` 拆开喂进来；「怎么判一个候选」这件事住在这儿，
/// 因为它的错法（把 `clangd.exe` 漏掉）在界面上只表现为「找不到」。
fn search_in_path(command: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    for dir in dirs {
        for candidate in candidates(command) {
            // `PATH` 里的空项在约定上就是「当前目录」（`.;C:\bin` 这种写法）
            let full = if dir.as_os_str().is_empty() {
                candidate
            } else {
                dir.join(candidate)
            };
            if full.is_file() {
                return Some(full);
            }
        }
    }
    None
}

/// 「这个命令在这个目录里可能叫什么」——原名优先，然后才是那几个后缀。
///
/// ⚠️ 顺序不是随便的：Windows 自己也是先试原名。磁盘上同时有 `clangd` 和
/// `clangd.exe` 时，该赢的是前者 —— 那是用户**明确**指的那一个。
fn candidates(command: &str) -> Vec<PathBuf> {
    const SUFFIXES: &[&str] = &["", ".exe", ".cmd", ".bat", ".com"];
    SUFFIXES
        .iter()
        .map(|suffix| PathBuf::from(format!("{command}{suffix}")))
        .collect()
}

/// 平台相关的「stbd 配置目录」。
fn user_config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);

    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));

    base.map(|dir| dir.join("stbd"))
}

/// 把 `toml` 的错误压成**一行**文本，并在前面补上行号。
///
/// toml 的 `Display` 会带一段多行的「错误位置示意图」（带 `|` 和 `^`），
/// 直接塞进状态栏会被截成一团乱码，所以这里用 `message()` 取纯消息，
/// 再自己从 `span()` 算出「第几行」补回去。
fn flatten_error(err: &toml::de::Error, text: &str) -> String {
    let message = err.message();
    let one_line: String = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let detail = if one_line.is_empty() {
        // 极少数没有 message 的情况（比如只带 span 的错误），退回整体压缩
        err.to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        one_line
    };

    // span 给的是「字节偏移」；数一下它前面有几个换行就是行号（1 基）。
    // 按字节切片是安全的（不需要是字符边界），只要不越界。
    match err.span().map(|span| span.start) {
        Some(offset) if offset <= text.len() => {
            let line = text.as_bytes()[..offset]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count()
                + 1;
            format!("line {line}: {detail}")
        }
        _ => detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在系统临时目录里造一个配置文件文件名（内容由调用方写）。
    fn temp_config_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("stbd_config_{}_{}.toml", name, std::process::id()))
    }

    #[test]
    fn parses_all_fields() {
        let config = Config::parse(
            r#"
# 井号开头是注释
show_line_numbers = false
tab_width = 4
scroll_margin = 1
side_scroll_margin = 2
"#,
        )
        .unwrap();

        assert!(!config.show_line_numbers);
        assert_eq!(config.tab_width, 4);
        assert_eq!(config.scroll_margin, 1);
        assert_eq!(config.side_scroll_margin, 2);
    }

    #[test]
    fn missing_keys_fall_back_to_config_defaults() {
        // 只写一个字段：其余必须保持「Config::default() 的值」，而不是 usize/bool 的零值
        let config = Config::parse("show_line_numbers = false").unwrap();
        assert!(!config.show_line_numbers);
        assert_eq!(config.tab_width, DEFAULT_TAB_WIDTH);
        assert_eq!(config.scroll_margin, DEFAULT_SCROLL_MARGIN);
        assert_eq!(config.side_scroll_margin, DEFAULT_SIDE_SCROLL_MARGIN);

        // 空文件 = 全默认值
        assert_eq!(Config::parse("").unwrap(), Config::default());
        // 只有注释的文件也一样
        assert_eq!(Config::parse("# 什么都没写\n").unwrap(), Config::default());
    }

    #[test]
    fn rejects_unknown_key_with_single_line_message() {
        // 刻意把 tab_width 写成 tabwidth：必须报错，而不是静默忽略
        let err = Config::parse("tabwidth = 4").unwrap_err();
        assert!(err.contains("tabwidth"), "错误信息应提到写错的键名：{err}");
        assert!(
            !err.contains('\n'),
            "状态栏只显示一行，错误信息不该含换行：{err}"
        );
    }

    #[test]
    fn rejects_broken_toml_with_single_line_message() {
        let err = Config::parse("tab_width = ").unwrap_err();
        assert!(!err.is_empty());
        assert!(!err.contains('\n'), "错误信息不该含换行：{err}");
    }

    #[test]
    fn rejects_out_of_range_values() {
        let too_wide = Config::parse("tab_width = 200").unwrap_err();
        assert!(too_wide.contains("tab_width"), "{too_wide}");

        let zero_tab = Config::parse("tab_width = 0").unwrap_err();
        assert!(zero_tab.contains("tab_width"), "{zero_tab}");

        let huge_margin = Config::parse("scroll_margin = 9999").unwrap_err();
        assert!(huge_margin.contains("scroll_margin"), "{huge_margin}");

        // 边界值本身合法
        assert!(Config::parse("tab_width = 16").is_ok());
        assert!(Config::parse("side_scroll_margin = 100").is_ok());
    }

    #[test]
    fn loads_config_from_file() {
        let path = temp_config_path("load");
        std::fs::write(&path, "tab_width = 2\n").unwrap();
        let config = Config::load_from_file(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(config.tab_width, 2);
    }

    #[test]
    fn reports_missing_file_instead_of_panicking() {
        let missing = std::env::temp_dir().join("stbd_config_definitely_missing_9527.toml");
        assert!(Config::load_from_file(&missing).is_err());
    }

    // ---------- 找命令（`which`）----------

    /// 造一个「PATH 里的一项」，里面放几个空文件冒充程序。
    fn fake_path_dir(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("stbd-which-{name}"));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("建临时目录");
        for file in files {
            std::fs::write(dir.join(file), "假装是个程序").expect("写临时文件");
        }
        dir
    }

    /// 裸名字要在 PATH 里找得到 —— 而且找的是 `.exe` 那个。
    ///
    /// 这是 `:lsp` 那张表里「找得到吗」那一列的**唯一**判据。它答错的后果是
    /// 用户拿着一张说「找不到」的表去改一份本来就对的配置。
    #[test]
    fn a_bare_name_is_found_inside_a_path_entry() {
        let dir = fake_path_dir("bare", &["findme.exe"]);
        assert_eq!(
            search_in_path("findme", std::slice::from_ref(&dir)),
            Some(dir.join("findme.exe"))
        );

        // 找不到就是找不到 —— 别把「没这个文件」说成「有」
        assert_eq!(
            search_in_path("nothing-here", std::slice::from_ref(&dir)),
            None
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// ⚠️ 磁盘上同时有 `findme` 和 `findme.exe` 时，赢的是**没有后缀**的那个。
    ///
    /// Windows 自己就是这个顺序（先试原名）。顺序反了的话，一个目录里
    /// 放着一个叫 `clangd` 的脚本和一个叫 `clangd.exe` 的真程序时，
    /// 我们报的路径会跟系统实际跑的不是同一个 —— 而两个都「找得到」，
    /// 屏幕上完全看不出区别。
    #[test]
    fn the_plain_name_wins_over_the_one_with_a_suffix() {
        let dir = fake_path_dir("order", &["findme", "findme.exe"]);
        assert_eq!(
            search_in_path("findme", std::slice::from_ref(&dir)),
            Some(dir.join("findme"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// PATH 的**第一项**先赢 —— 和系统一样，不挑「更晚的但更完整的」。
    #[test]
    fn the_first_path_entry_wins() {
        let first = fake_path_dir("first", &["findme.exe"]);
        let second = fake_path_dir("second", &["findme.exe"]);

        assert_eq!(
            search_in_path("findme", &[first.clone(), second]),
            Some(first.join("findme.exe"))
        );

        std::fs::remove_dir_all(&first).ok();
    }

    /// ⚠️ `command = ""` 的意思是「把这条关掉」，**不是**「找一个没名字的程序」。
    ///
    /// 这里要是返回点什么（比如当前目录），关掉一条内置的服务器就会变成
    /// 「起了一个不知道是什么的东西」。
    #[test]
    fn an_empty_command_finds_nothing() {
        assert_eq!(which(""), None);
        assert_eq!(which("   "), None);
        assert_eq!(search_in_path("", &[std::env::temp_dir()]), None);
    }

    /// 写成了路径（带 `/` 或 `\`）就**不去 PATH 里翻**。
    ///
    /// 不去翻是重点：`./my-server` 该按「当前目录下这个文件**在不在**」来判，
    /// 而不是「PATH 里有没有一个叫 `./my-server` 的」。后者永远找不到 ——
    /// 于是「用相对路径指定自己的服务器」会静默失效。
    #[test]
    fn a_command_written_as_a_path_is_not_looked_up_in_path() {
        assert_eq!(which("./definitely-not-here-9527"), None);
        assert_eq!(which(r".\definitely-not-here-9527"), None);
        assert_eq!(which(r"D:\definitely\not\here\9527.exe"), None);
    }

    // ---------- 颜色 ----------

    #[test]
    fn colour_defaults_keep_the_gutter_out_of_the_way() {
        let colors = Colors::default();
        assert_eq!(colors.text, Color::Green);
        // ⚠️ 行号是**背景设施** —— 它退成暗灰，黄色才能专门归警告用。
        //    这两条得一起看：单独看任何一个都看不出「为什么是这个色」。
        assert_eq!(colors.line_number, Color::DarkGray);
        assert_eq!(colors.warning, Color::LightYellow);
        assert_ne!(
            colors.line_number, colors.warning,
            "行号和警告抢同一个颜色的话，行号栏上就分不出「没毛病」和「有警告」"
        );
        assert_ne!(colors.warning, colors.error, "警告和错误不能同色");
        // 默认高亮 = 原来的 Indexed(236) = #303030
        assert_eq!(colors.current_line_bg, Color::Rgb(0x30, 0x30, 0x30));
        assert_eq!(Config::default().colors, Colors::default());
    }

    #[test]
    fn colors_section_overrides_only_the_keys_it_writes() {
        // 只改正文颜色：其余各项必须还是默认值，而不是被重置成 reset
        let config = Config::parse("[colors]\ntext = \"cyan\"\n").unwrap();
        assert_eq!(config.colors.text, Color::Cyan);
        assert_eq!(config.colors.line_number, DEFAULT_LINE_NUMBER_COLOR);
        assert_eq!(config.colors.current_line_bg, DEFAULT_CURRENT_LINE_BG);
        assert_eq!(config.colors.hint, Colors::default().hint);

        // 空的 [colors] 分节 = 全默认值
        assert_eq!(
            Config::parse("[colors]\n").unwrap().colors,
            Colors::default()
        );
    }

    #[test]
    fn parses_named_colors_case_insensitively() {
        let config = Config::parse("[colors]\ntext = \"LightGreen\"\n").unwrap();
        assert_eq!(config.colors.text, Color::LightGreen);

        let config = Config::parse("[colors]\nline_number = \"  Yellow  \"\n").unwrap();
        assert_eq!(config.colors.line_number, Color::Yellow);

        // 常见别名
        assert_eq!(parse_color("purple").unwrap(), Color::Magenta);
        assert_eq!(parse_color("grey").unwrap(), Color::Gray);
    }

    #[test]
    fn parses_hex_colors_with_short_and_long_form() {
        assert_eq!(parse_color("#ffcc00").unwrap(), Color::Rgb(255, 204, 0));
        // 简写 #fc0 等价于 #ffcc00
        assert_eq!(parse_color("#fc0").unwrap(), Color::Rgb(255, 204, 0));
        assert_eq!(parse_color("#000000").unwrap(), Color::Rgb(0, 0, 0));

        let config = Config::parse("[colors]\ncurrent_line_bg = \"#1e1e1e\"\n").unwrap();
        assert_eq!(config.colors.current_line_bg, Color::Rgb(0x1e, 0x1e, 0x1e));
    }

    #[test]
    fn reset_keywords_mean_no_color() {
        for keyword in ["reset", "default", "none", "RESET"] {
            assert_eq!(parse_color(keyword).unwrap(), Color::Reset, "{keyword}");
        }
        // 关掉当前行高亮
        let config = Config::parse("[colors]\ncurrent_line_bg = \"none\"\n").unwrap();
        assert_eq!(config.colors.current_line_bg, Color::Reset);
    }

    #[test]
    fn rejects_unknown_color_name_with_single_line_message() {
        let err = Config::parse("[colors]\ntext = \"yelow\"\n").unwrap_err();
        assert!(err.contains("unknown color"), "{err}");
        // 要把写错的值原样回显，用户才知道自己写了什么
        assert!(err.contains("yelow"), "{err}");
        assert!(err.contains("line 2"), "应指出第 2 行：{err}");
        assert!(!err.contains('\n'), "状态栏只显示一行：{err}");
    }

    #[test]
    fn rejects_malformed_hex_color() {
        for bad in ["#gg0000", "#12345", "#", "#12"] {
            let err = Config::parse(&format!("[colors]\ntext = \"{bad}\"\n")).unwrap_err();
            assert!(
                err.contains("invalid color") || err.contains("unknown color"),
                "{bad} → {err}"
            );
        }
    }

    #[test]
    fn rejects_unknown_key_inside_colors_section() {
        // 想写 current_line_bg 却拼错了
        let err = Config::parse("[colors]\ncurrent_bg = \"red\"\n").unwrap_err();
        assert!(err.contains("current_bg"), "{err}");
    }

    #[test]
    fn top_level_key_after_colors_section_is_reported() {
        // TOML 的经典坑：分节之后的键属于该分节，所以 tab_width 会被当成 colors.tab_width
        let err = Config::parse("[colors]\ntext = \"green\"\ntab_width = 4\n").unwrap_err();
        assert!(err.contains("tab_width"), "错误信息应提到这个键：{err}");
    }

    #[test]
    fn candidate_paths_all_point_at_the_config_file_name() {
        let paths = Config::candidate_paths();
        assert!(!paths.is_empty());
        for path in &paths {
            assert_eq!(path.file_name().unwrap(), CONFIG_FILE_NAME, "{path:?}");
        }
    }

    #[test]
    fn settings_template_matches_the_defaults() {
        // 内嵌的模板（`:settings` 用它生成新文件）必须：
        // 1. 能被解析 —— 否则生成出一个自己的程序都读不了的文件；
        // 2. 值等于当前默认值 —— 否则「生成配置文件」会静默改变用户的行为。
        let parsed = Config::parse(SETTINGS_TEMPLATE)
            .unwrap_or_else(|err| panic!("内置模板必须能解析：{err}"));
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn preferred_path_sits_next_to_the_executable() {
        let path = Config::preferred_path().expect("本机应该拿得到可执行文件目录");
        assert_eq!(path.file_name().unwrap(), CONFIG_FILE_NAME);
        // 首选位置是 exe 同目录（配置跟着程序走），不是系统盘的用户目录
        assert_eq!(path.parent().unwrap(), executable_dir().unwrap());
    }

    #[test]
    fn ensure_file_at_creates_the_file_with_the_template_once() {
        let path = temp_config_path("created");
        std::fs::remove_file(&path).ok();

        // 不存在 → 写出模板
        ensure_file_at(&path).expect("应该能写出模板");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SETTINGS_TEMPLATE);

        // 已存在 → 空操作，不报错也不覆盖
        std::fs::write(&path, "tab_width = 2\n").unwrap();
        ensure_file_at(&path).expect("已存在时应该什么都不做");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "tab_width = 2\n");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn ensure_file_at_reports_unwritable_path() {
        // 指向一个不存在的目录 → 写不进去，应该返回带路径的错误
        let path = std::env::temp_dir()
            .join("stbd_no_such_dir_9527")
            .join(CONFIG_FILE_NAME);
        let err = ensure_file_at(&path).unwrap_err();
        assert!(err.contains("cannot create"), "{err}");
    }

    #[test]
    fn error_message_points_at_the_line() {
        // 第二行写错键名 → 提示里应带上行号，用户不用自己数
        let err = Config::parse("tab_width = 4\ntabwidth = 2\n").unwrap_err();
        assert!(err.contains("line 2"), "应指出第 2 行：{err}");
        assert!(err.contains("tabwidth"), "应指出写错的键名：{err}");
    }

    #[test]
    fn describe_renders_current_settings() {
        assert_eq!(
            Config::default().describe(),
            "number=on tabwidth=8 scrolloff=3 sidescrolloff=5 lsp=3"
        );

        let config = Config {
            show_line_numbers: false,
            tab_width: 2,
            scroll_margin: 0,
            side_scroll_margin: 1,
            ..Config::default()
        };
        assert_eq!(
            config.describe(),
            "number=off tabwidth=2 scrolloff=0 sidescrolloff=1 lsp=3"
        );
    }

    /// 语言服务器那一项：`0` 说成 `off`，其余说成数字。
    ///
    /// `lsp=off` 比 `lsp=0` 清楚 —— 它不是「零个服务器」这种可有可无的状态，
    /// 它就是「不用语言服务器」。
    #[test]
    fn describe_says_off_when_language_servers_are_disabled() {
        let config = Config {
            lsp_max_servers: 0,
            ..Config::default()
        };
        assert!(
            config.describe().ends_with("lsp=off"),
            "{}",
            config.describe()
        );

        let config = Config {
            lsp_max_servers: 5,
            ..Config::default()
        };
        assert!(
            config.describe().ends_with("lsp=5"),
            "{}",
            config.describe()
        );
    }

    /// `lsp_max_servers` 超出范围要报错（跟别的设置一个待遇：写错就该知道）。
    #[test]
    fn an_out_of_range_server_count_is_rejected() {
        let config = Config {
            lsp_max_servers: 9,
            ..Config::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("lsp_max_servers"), "{err}");

        // 0 是**合法**的：它就是「不用语言服务器」，不是写错了
        let config = Config {
            lsp_max_servers: 0,
            ..Config::default()
        };
        assert!(config.validate().is_ok());
    }

    // ---------- 语言服务器表 ----------

    /// 内置的那几条：Rust 一条，clangd 两条（C 和 C++ 分开，因为 `languageId`
    /// 不一样，而 clangd 靠它决定怎么解析）。
    #[test]
    fn the_builtin_table_covers_rust_and_c_and_cpp() {
        let config = Config::default();

        let rust = config.server_for("src/main.rs").expect("Rust 该有服务器");
        assert_eq!(rust.command, "rust-analyzer");
        assert_eq!(rust.language_id(), "rust");
        assert_eq!(rust.root_marker.as_deref(), Some("Cargo.toml"));

        let c = config.server_for("a.c").expect("C 该有服务器");
        assert_eq!(c.command, "clangd");
        assert_eq!(
            c.language_id(),
            "c",
            "languageId 得是 c，不然 clangd 会按 C++ 解析"
        );
        assert_eq!(c.root_marker, None, "clangd 不要求项目根");

        let cpp = config.server_for("a.cpp").expect("C++ 该有服务器");
        assert_eq!(cpp.command, "clangd");
        assert_eq!(cpp.language_id(), "cpp");

        // 扩展名大小写不敏感（Windows 上 `.C` 和 `.c` 是同一个东西）
        assert_eq!(config.server_for("A.C").map(|s| s.language_id()), Some("c"));
    }

    /// ⚠️ **没有对应的服务器就返回 `None`，一个字节都不发。**
    ///
    /// 这条守着那个实测出来的坑：以前是「找到 `Cargo.toml` 就把文件丢给
    /// rust-analyzer」，于是在 Rust 项目里打开 `.py`，它会拿 **Rust 的语法**
    /// 去解析 Python，报出 4 条假的 `expected an item`。
    /// 假错误比没有诊断糟得多 —— 你会去改本来没错的代码。
    #[test]
    fn a_file_with_no_server_gets_nothing_at_all() {
        let config = Config::default();

        for path in [
            "script.py",
            "app.js",
            "notes.txt",
            "Makefile",   // 没有扩展名
            ".gitignore", // 点开头 —— 那不是「后缀是 gitignore 的文件」
        ] {
            assert!(
                config.server_for(path).is_none(),
                "{path} 不该匹配到任何服务器"
            );
        }
    }

    /// ⚠️ 配置文件里那张表是**并进**内置默认，**不是替换它**。
    ///
    /// 替换的话，一份只写了 `[lsp.python]` 的配置会把 Rust 那条踢掉 ——
    /// 现象是「Rust 诊断突然没了」，而矛头会指向完全无关的地方。
    #[test]
    fn a_config_section_merges_into_the_builtins_instead_of_replacing_them() {
        let config = Config::parse(
            r#"
[lsp.zig]
command = "zls"
extensions = ["zig"]
"#,
        )
        .expect("该能解析");

        assert_eq!(
            config.server_for("a.zig").map(|s| s.command.as_str()),
            Some("zls")
        );
        assert!(
            config.server_for("a.rs").is_some(),
            "只写了一节，Rust 那条就没了 —— 合并写成替换了"
        );
        assert_eq!(
            config.server_for("a.zig").map(|s| s.language_id()),
            Some("zig")
        );
    }

    /// **一节 `[lsp]` 都不写的配置**（也就是所有老配置）必须照旧拿到内置那三条。
    ///
    /// ## 它和已有测试重叠，这是故意的
    ///
    /// `Config::parse("") == Config::default()` 那条已经盖住了同样的路，
    /// 只是盖得很笼统。这条分开写是为了**把症状说出来**：真断了的时候，
    /// 用户看到的是「升级之后所有老配置的人突然全没了诊断」，
    /// 而第一反应是怀疑自己把配置改坏了。
    ///
    /// ## 它**不能**证明的事（红检发现的）
    ///
    /// 我原来以为它守着 `#[serde(default = "default_lsp")]`。红检时把那个
    /// 默认值摘掉，它**照样绿** —— 因为内置三条其实是
    /// [`Config::prepare_lsp`] 并进来的（`Config::parse` 的必经之路），
    /// serde 那层只是「字段可以不在」。所以那条属性已经改成光秃秃的
    /// `#[serde(default)]`，内置表只剩一个入口。
    ///
    /// 真正被这条测试守住的是 `prepare_lsp` 里那句 `entry().or_insert(...)` ——
    /// 把它删掉，这条和 `a_config_section_merges_into_the_builtins_...` 一起红。
    #[test]
    fn a_config_without_any_lsp_section_still_gets_the_builtins() {
        // 用真的老配置的样子：有个顶层键，但没有 [lsp]
        let config = Config::parse("tab_width = 8\n").expect("老配置该能解析");

        assert_eq!(
            config.server_for("a.rs").map(|s| s.command.as_str()),
            Some("rust-analyzer")
        );
        assert_eq!(
            config.server_for("a.c").map(|s| s.command.as_str()),
            Some("clangd")
        );
        assert_eq!(
            config.server_for("a.cpp").map(|s| s.command.as_str()),
            Some("clangd")
        );

        // 默认值本身也得有 —— `Config::default()` 是**手写的**（不是 derive），
        // 漏掉一行 `lsp: default_lsp()` 是这条路上最可能的错法
        assert_eq!(Config::default().lsp.len(), 3);
    }

    /// 同名的一节会**盖住**内置的那条（这是「改默认值」的正路）。
    #[test]
    fn a_section_with_a_builtin_name_overrides_it() {
        let config = Config::parse(
            r#"
[lsp.rust]
command = "my-analyzer"
args = ["--stdio"]
extensions = ["rs"]
"#,
        )
        .expect("该能解析");

        let rust = config.server_for("a.rs").expect("该有");
        assert_eq!(rust.command, "my-analyzer");
        assert_eq!(rust.args, vec!["--stdio"]);
        assert_eq!(rust.root_marker, None, "没写就是没写，不该继承旧的");
    }

    /// `command` 留空 = **这条不要了**（用来关掉一条内置的）。
    #[test]
    fn an_empty_command_turns_a_builtin_off() {
        let config = Config::parse(
            r#"
[lsp.c]
command = ""
extensions = ["c", "h"]
"#,
        )
        .expect("该能解析");

        assert!(
            config.server_for("a.c").is_none(),
            "command 留空该等于关掉那条"
        );
        assert!(config.server_for("a.cpp").is_some(), "不该殃及别的节");
    }

    /// 后缀写成 `.c`（带点）是最容易犯的错 —— 必须报错，而且说清怎么写。
    ///
    /// 不报的话它只是安静地什么都匹配不到，现象是「我配了 clangd 但没诊断」。
    #[test]
    fn an_extension_written_with_a_dot_is_rejected() {
        let err = Config::parse(
            r#"
[lsp.zig]
command = "zls"
extensions = [".zig"]
"#,
        )
        .unwrap_err();

        assert!(err.contains("lsp.zig"), "该说清是哪一节：{err}");
        assert!(err.contains("dot"), "该说清错在哪：{err}");
    }

    /// 两节都声称管同一个后缀时，**节名字典序靠前的赢** —— 而且是确定的。
    ///
    /// `HashMap` 的遍历顺序是随机的；不排序的话「谁赢」会每跑一次都可能不一样，
    /// 那是这类代码里最难查的一种不稳定。
    #[test]
    fn two_sections_claiming_one_extension_resolve_deterministically() {
        let text = r#"
[lsp.c]
command = "c-server"
extensions = ["h"]

[lsp.cpp]
command = "cpp-server"
extensions = ["h"]
"#;

        // 多解析几遍：真要是靠 HashMap 的随机顺序，这里迟早会翻。
        for _ in 0..20 {
            let config = Config::parse(text).expect("该能解析");
            assert_eq!(
                config.server_for("a.h").map(|s| s.command.as_str()),
                Some("c-server"),
                "节名靠前的该赢，而且每次都该是它"
            );
        }
    }

    /// 节名就是 `languageId` 的默认值 —— 最常见的情况（节名就是语言名）
    /// 不用写两遍。写了 `language_id` 则以它为准。
    #[test]
    fn the_section_name_becomes_the_language_id_unless_overridden() {
        let config = Config::parse(
            r#"
[lsp.zig]
command = "zls"
extensions = ["zig"]

[lsp.mylang]
command = "my-server"
extensions = ["mylang"]
language_id = "plaintext"
"#,
        )
        .expect("该能解析");

        assert_eq!(
            config.server_for("a.zig").map(|s| s.language_id()),
            Some("zig")
        );
        assert_eq!(
            config.server_for("a.mylang").map(|s| s.language_id()),
            Some("plaintext")
        );
    }

    /// 表里写错键名要报错，而且指出第几行 —— 跟别的配置一个待遇。
    #[test]
    fn a_typo_inside_a_server_section_is_reported_with_its_line() {
        let err = Config::parse(
            r#"
[lsp.zig]
comand = "zls"
extensions = ["zig"]
"#,
        )
        .unwrap_err();

        assert!(err.contains("line 3"), "该指出第 3 行：{err}");
        assert!(err.contains("comand"), "该指出写错的键名：{err}");
    }
}
