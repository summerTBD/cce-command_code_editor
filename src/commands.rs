//! 命令解析与执行 —— commands.rs 的职责
//!
//! 分两层，别混：
//!
//! - **解析**（[`parse`]）：一行文本 → [`Command`]（命令名 / 选项 / 位置参数）。
//!   纯字符串处理 —— 不碰 `App`、不读盘、不改状态，所以可以直接写测试。
//! - **执行**（[`run`] → [`execute`]）：改状态 + 产出 [`Action`] 让 main.rs 去做副作用。
//!   [`run`] 负责一行里的 `&&` 链（切开、依次跑、前一条失败就短路），[`execute`] 只管单条。
//!
//! 为什么要分开：**「`--force` 放前面放后面都一样」是解析层的性质，「走不走这个分支」
//! 是执行层的事。** 混在一起就会出现「为什么这个顺序不行」这类玄学 bug。
//!
//! ## 语法（完整设计见项目根 `COMMANDS.md`）
//!
//! ```text
//! :命令名 [位置参数...] [-选项...] [--选项...] [-- 后面全是位置参数]
//! :命令A && 命令B        # 前一条成功才做后一条（短路）
//! :open "my file.rs"     # 引号：把带空格 / `&&` 的东西收成一个词
//! ```
//!
//! - **命令名**必须在第一个词，而且**只能是第一个词**。别名会归一化成规范名
//!   （`d` / `del` → `delete`）。选项不能写到它前面：`-f back` 是「一条叫 `-f` 的命令」，
//!   跟 shell 一样（`-la ls` 就是命令 `-la`），报错时会给一句定向提示。
//! - **不分大小写**（跟 PowerShell 一个脾气）：命令名、别名、选项名、关键字参数都算 ——
//!   `:DELETE 1 2`、`:Q`、`:open a.rs --FORCE`、`:set NUMBER` 都认。
//!   边界是「**这个词是谁定的**」：我们定的词随便大小写，而**你写的字**
//!   （路径、行号、要复制的文本）一个字节都不动 ——
//!   `:open README.MD` 打开的就是那个大写名字的文件。
//!   三处用的是**同一条规矩**：把词**折成表里那个写法**，再查表 ——
//!   命令名 / 别名折在查表那一步（[`canonical`]，认出就返回表里那条命令的 `name`，
//!   一次分配都没有）；关键字参数折在执行前（[`KEYWORDS`] 表，同样给回表里的写法）；
//!   选项名折在解析那一步（[`parse_flag`]；选项还没有表 —— 今天只有一个 `--force`，
//!   所以它是真折了一个小写串）。折完之后**所有比较都是普通的 `==`**。
//!
//!   为什么不干脆把整行折成小写：`args` 里混着**你写的字**（路径）——
//!   `:open README.MD` 折了就变成另一个文件（Linux 上那真是两份东西）。
//!   只有**表上声明过的词**才折，别的原样传下去。
//! - **选项带名字**，所以在**命令名之后**放哪都一样；**位置参数没名字**，
//!   顺序就是它唯一的身份，必须保持。这两件事在 [`parse_command`] 里一次搞定 ——
//!   非选项的词按遇到的顺序 push 进 `args`，而**过滤本身就是保序操作**，不用额外写什么。
//! - `--key=value` 是**唯一**带值的写法。不支持 `--key value`：那要求解析器预知每个
//!   选项吃几个值，还要判断「下一个词是值还是参数」。
//! - 单独一个 `--` 之后，所有词都当位置参数（用来打开以 `-` 开头的文件）。
//! - `'...'` / `"..."` 把里面的东西收成一个词。**引号只改「边界」，不改「身份」**：
//!   `open "-x"` 里那个词仍然以 `-` 开头、仍然算选项（跟 shell 一样）。
//! - **不认 `!` 后缀**（vim 的 `:q!` 那套）。开关一律走选项：`:open --force a.rs`。
//!   但也不静默无视它 —— 名字以 `!` 结尾且去掉后是个已知命令时，给一句**定向提示**
//!   （见 [`unknown_command_message`]）。vim 的肌肉记忆值得被照顾一下。
//!
//! ## 切分（[`lex`]）是全层唯一有状态的地方
//!
//! 一行先被 [`lex`] 扫成「词 / `&&`」，然后才轮到分段（[`run`]）和分类（[`parse_command`]）。
//! 只有它需要知道「我是不是在引号里」—— 别处全是「看自己就够了」的局部判断。
//!
//! 支持 `'...'` / `"..."`，**没有转义**，**引号必须包住整个词**。
//! 这三条都是从 POSIX shell 的**本质**里挑出来的：它那些转义和拼接是为了伺候「展开」
//! （变量、通配、命令替换），而这里根本没有展开 —— 搬过来就只是白拿复杂度。
//! 详细理由写在 [`lex`] 的文档上。

use std::borrow::Cow;

use crate::app::{App, DocumentKind, EditorMode};
use crate::config::Config;
use crate::formatter;
use crate::outbox::OutFile;

/// **update**（按键或命令）要 main.rs 去做的「副作用」。
///
/// 注意：只有「碰外部世界」的事才在这里（退出进程、写磁盘、写剪贴板、让出终端）；
/// 切换模式、改设置这种纯状态变化由 [`execute`] 直接做掉，不产生 Action。
///
/// 产出方不只是命令 —— 按键也会产出（`y` 键产出 [`Action::Copy`]，
/// `!` 那一行产出 [`Action::RunExternal`]）。
///
/// 因为带上了 `Copy(String)`，这里**不能** derive `Copy`（String 不是 Copy）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 退出程序
    Quit,
    /// 把当前内容保存到文件
    Save,
    /// 另存为：写到这个路径，**并且把这个缓冲区改成这个名字**
    ///
    /// 跟 [`Action::Save`] 分开，是因为它多了一件事：写完之后缓冲区就不是
    /// 原来那个文件了。main 得把新路径记回去，否则第二次 `:w` 又写回旧地方。
    SaveAs(String),
    /// 保存并退出（`:wq`）
    SaveAndQuit,
    /// 把这段文本写入系统剪贴板
    Copy(String),
    /// 剪切：这段文本**已经**从文档里删掉了，现在把它写进剪贴板。
    ///
    /// 和 [`Action::Copy`] 分成两个变体，**只为了回执说得准**：main 是最后写状态栏的
    /// 那个人（剪贴板成没成只有它知道），可它从一段文本里看不出「这是一次剪切、
    /// 刚删了 3 行」。带上 `rows` 它才能说出 `Cut 3 lines to clipboard`，
    /// 而 `y` / `:copy` 那边照旧说 `Copied N chars to clipboard`。
    ///
    /// ⚠️ 顺序上注意：**删**发生在 [`crate::update`] 里（那一步要进撤销栈），
    /// 写剪贴板才是这个动作。所以剪贴板万一失败，那几行也已经不在文档里了 ——
    /// 回执里必须带上「用 `u` 能找回来」。
    Cut { text: String, rows: usize },
    /// 打开另一个路径（文件或目录），由 main 读取后交给 App
    ///
    /// 路径**可以是相对的**。产出方只管把「用户指的是哪个路径」说出来，
    /// **不要自己拼** —— 解析只发生在 `main::open_path` 一处
    /// （基准是 `App::current_directory()`）。同一个意思只有一份实现，
    /// 否则 `Enter` 和 `:open` 这种「同一件事」迟早会走歪。
    OpenPath(String),
    /// 打开配置文件来编辑（文件不存在时先按内置模板生成一份），由 main 去干 IO
    Settings,
    /// 重新从磁盘读配置文件（`:config reload`），由 main 去干 IO
    ReloadConfig,
    /// 把终端**让出去**，跑这一行外部命令。
    ///
    /// 载荷是**原样的一整行** —— 引号、`&&`、重定向全归 shell 管，
    /// 我们一个词都不分（这也是「两套语言在按键那一层分开」换来的好处）。
    /// main 负责：离开备用屏 → 跑 → 等回车 → 把界面收回来。
    RunExternal(String),
    /// 在后台跑一次 `cargo check`（`:check`）。
    ///
    /// ⚠️ 跟 [`Action::RunExternal`] 恰好**相反**：那个把终端交出去、
    /// 等你敲回车；这个一个字都不往终端写，你继续编辑，结果到了才报。
    /// main 负责：起线程 + 留一个收件通道（见 `check::spawn`）。
    RunCheck,
    /// 在后台跑一次格式化（`:fmt`）。
    ///
    /// ⚠️ 跟 [`Action::RunCheck`] 是**同一个形状**（后台线程 + 通道，结果到了才报），
    /// 但它排版的是**缓冲区里那份**，不是磁盘上那份 —— 所以它一个字都不许写盘。
    /// 理由见 `formatter` 模块顶上那段（盘上那份可能已经是旧的）。
    ///
    /// 挑哪个工具、怎么喂文本、结果怎么换回来，**全在 `formatter` 里**，
    /// 这里一个字的判断都没有 —— 那就是「提供者」那道门。
    Format,
    /// 从虚拟视图（`:errors`）退回原来那份文档。
    ///
    /// ⚠️ 它**不是** [`Action::OpenPath`] —— 那个会去磁盘上重新读一遍，
    /// 而重新读盘会丢掉没保存的改动（而且 `q` 那条路上还拦着「没保存不许走」，
    /// 于是你会被堵在清单里出不来）。清单只是换了个东西给你看，
    /// 退出时该把原来那份**原样放回来**。
    RestoreDocument,
    /// 把**现在屏幕上那份清单**抄进输出文件夹里的那个文件。
    ///
    /// 只带「往哪个文件写」—— **不带正文**。正文就是 `App` 里那个缓冲区，
    /// main 抄它就行。带上正文等于把同一份东西存两处，两边迟早对不上。
    WriteOutbox(OutFile),
    /// 铺一份「语言服务器现在什么状况」的清单（`:lsp`）。
    ///
    /// ⚠️ 它和上面那几种清单**不一样**：`:ls` / `:errors` 的内容全在 `App` 里
    /// （文档列表、诊断），所以命令层自己就能拼出正文；这一份要说出
    /// 「现在跑着哪几个」，而那只有**池子**知道。
    ///
    /// 池子是活着的东西（进程、线程、通道），按这个文件顶上那条纪律
    /// 不该让命令层碰 —— 所以这里只返回一个「要铺」，正文交给主循环拼。
    ShowLspStatus,
}

// ===== 命令表 =====

// 用法提示写成 const：表里引用它，具体命令的报错分支也引用它 ——
// 一处定义，两边用的是同一个字符串。
const QUIT_USAGE: &str = "Usage: quit";
const BACK_USAGE: &str = "Usage: back [--force]";
const NEXT_USAGE: &str = "Usage: next [--force]";
const LS_USAGE: &str = "Usage: ls";
const FORGET_USAGE: &str = "Usage: forget <n>  (n comes from :ls)";
const CHECK_USAGE: &str = "Usage: check";

const FORMAT_USAGE: &str = "Usage: format";
const ERRORS_USAGE: &str = "Usage: errors";
const LSP_USAGE: &str = "Usage: lsp";
const OPEN_USAGE: &str = "Usage: open <path> [--force]";
const WRITE_USAGE: &str = "Usage: write [<path>]";
const WQ_USAGE: &str = "Usage: wq";
const SETTINGS_USAGE: &str = "Usage: settings [--force]  (same as: config edit)";
const RELOAD_USAGE: &str = "Usage: reload  (same as: config reload)";
const CONFIG_USAGE: &str = "Usage: config | config path | config edit | config reload";
const DELETE_USAGE: &str = "Usage: delete <line> | delete <first> <last> | delete all";
const COPY_USAGE: &str =
    "Usage: copy <line> | copy <first> <last> | copy <r>:<c> <r>:<c> | copy all";
const SWAP_USAGE: &str = "Usage: swap <line x> <line y>  (both 1-based)";
const INSERT_USAGE: &str = "Usage: insert";
const UNDO_USAGE: &str = "Usage: undo";
const REDO_USAGE: &str = "Usage: redo";
const SET_USAGE: &str = "Usage: set number | set nonumber | set tabwidth <n> | set scrolloff <n> | set sidescrolloff <n> | set lspmaxservers <n>";

/// 一条命令的静态描述。
///
/// **一张表就是唯一的真相**：别名归一化、选项校验、用法提示全问它。
/// 要是拆成三个 `match` 各自维护，改了别名忘了改用法的提示是迟早的事。
struct Spec {
    /// 规范名（唯一真名，其余都是别名）
    name: &'static str,
    /// 别名（不含规范名本身）
    aliases: &'static [&'static str],
    /// 位置参数 / 选项写错时报给用户的用法提示
    usage: &'static str,
    /// 认不认 `--force` / `-f`
    force: bool,
}

/// 全部命令。分组顺序跟 `COMMANDS.md` 的表一致，方便对着看。
const COMMANDS: &[Spec] = &[
    // ---- 退出与导航 ----
    Spec {
        name: "quit",
        // ⚠️ 语义变更：`:q` 现在是「退出程序」，「返回上一级」是 `:back`。
        //    但 `q` **按键**仍然智能（有上一级就返回）—— 两套语言，见 COMMANDS.md
        aliases: &["q", "exit", "qa", "quitall"],
        usage: QUIT_USAGE,
        force: true, // 空操作：退出本来就不拦脏数据，但写了 `--force` 也不该报错
    },
    Spec {
        name: "back",
        aliases: &["prev", "bprev"],
        usage: BACK_USAGE,
        force: true,
    },
    Spec {
        name: "next",
        aliases: &["bnext", "n"],
        usage: NEXT_USAGE,
        force: true,
    },
    Spec {
        name: "ls",
        aliases: &["files"],
        usage: LS_USAGE,
        force: false,
    },
    Spec {
        name: "forget",
        aliases: &[],
        usage: FORGET_USAGE,
        force: false,
    },
    // ---- 文档与配置 ----
    Spec {
        name: "open",
        aliases: &["e", "edit", "stbd"],
        usage: OPEN_USAGE,
        force: true,
    },
    Spec {
        name: "write",
        aliases: &["w"],
        usage: WRITE_USAGE,
        force: false,
    },
    Spec {
        name: "wq",
        aliases: &[],
        usage: WQ_USAGE,
        force: false,
    },
    Spec {
        name: "settings",
        aliases: &[],
        usage: SETTINGS_USAGE,
        force: true,
    },
    Spec {
        name: "reload",
        aliases: &[],
        usage: RELOAD_USAGE,
        force: false,
    },
    Spec {
        name: "config",
        // 这一族只有 `config edit` 真的用得上 `--force`（它会顶掉当前文档）；
        // 其余子命令接受但忽略它 —— 跟 `quit --force` 一样的处理，不值得为它加特例。
        aliases: &[],
        usage: CONFIG_USAGE,
        force: true,
    },
    // ---- 后台任务 ----
    Spec {
        name: "check",
        // 故意**不给** `:c` 别名：vim 里 `:c` 是 quickfix 那一族，
        // 以后要加 `:errors` / 跳错误的命令时那个字母还会用到。
        //
        // （现在 `:errors` 加进来了，它也没有别名 —— vim 里 `:e` 是 `:edit`。）
        aliases: &[],
        usage: CHECK_USAGE,
        force: false,
    },
    Spec {
        name: "format",
        // `:fmt` 才是大家会敲的那个（`gofmt` / `rustfmt` / VS Code 的 Format 都这么叫），
        // 所以它必须有；规范名叫 `format` 是为了跟别的命令一样是完整的词
        aliases: &["fmt"],
        usage: FORMAT_USAGE,
        // 不需要 `--force`：它确实会**改正文**，但那个改动能一个 `u` 退掉，
        // 而且不碰磁盘。`--force` 是为了「会弄丢东西」那些命令准备的
        force: false,
    },
    // ---- 诊断 ----
    Spec {
        name: "errors",
        // ⚠️ `force: false` —— 它**能**顶掉当前屏幕上的东西，但不该要 `--force`：
        //    那份文档会被原样存下来，退出时一个字不差地放回去（见 `Action::RestoreDocument`）。
        //    「这个命令会弄丢什么吗」才是 `--force` 存在的理由，而它不丢东西。
        aliases: &[],
        usage: ERRORS_USAGE,
        force: false,
    },
    Spec {
        name: "lsp",
        // 同样不需要 `--force`：和 `:errors` 一样是把当前文档原样存下来再铺清单。
        aliases: &[],
        usage: LSP_USAGE,
        force: false,
    },
    // ---- 编辑 ----
    Spec {
        name: "delete",
        aliases: &["del", "d"],
        usage: DELETE_USAGE,
        force: false,
    },
    Spec {
        name: "copy",
        aliases: &["yank", "y"],
        usage: COPY_USAGE,
        force: false,
    },
    Spec {
        name: "swap",
        aliases: &["sw"],
        usage: SWAP_USAGE,
        force: false,
    },
    Spec {
        name: "insert",
        aliases: &["i"],
        usage: INSERT_USAGE,
        force: false,
    },
    Spec {
        name: "undo",
        aliases: &["u"],
        usage: UNDO_USAGE,
        force: false,
    },
    Spec {
        name: "redo",
        aliases: &[],
        usage: REDO_USAGE,
        force: false,
    },
    // ---- 设置 ----
    Spec {
        name: "set",
        aliases: &[],
        usage: SET_USAGE,
        force: false,
    },
];

impl Spec {
    /// 这个名字（规范名或别名）指的是不是这条命令 —— **不分大小写**。
    ///
    /// 用 `eq_ignore_ascii_case` 而不是 `to_lowercase()`，三个理由：
    /// ① 表里全是 ASCII，够用；② `to_lowercase` 会为 `İ` 变出两个字符、
    /// 又会把开尔文符号 `K` 折成 `k` —— 那种「聪明」在这里只会制造意外；
    /// ③ 它不分配内存，于是每次敲回车只是几次字节比较。
    ///
    /// 至于「你写的字」：这个函数只在查**表**的时候被调用，碰不到它们。
    fn answers_to(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
            || self
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(name))
    }

    /// 这条命令认不认这个选项（**比较是普通的 `==`**：名字在解析时已经折成小写）。
    ///
    /// v1 只有 `--force` 一个选项，所以判断这么简单就够了；
    /// 等真有第二个选项，这里就换成一张「选项名 → 短名」的表。
    ///
    /// ⚠️ 只管**选项名**：`--key=value` 里的 value 是用户的数据，原样不动。
    fn accepts(&self, flag: &Flag<'_>) -> bool {
        self.force && flag.is_named("force", "f")
    }
}

/// 把别名归一化成规范名；不认识的返回原样。
///
/// **大小写也算归一化的一部分**：`Q` / `q` / `quit` 都归到 `quit`。
///
/// 返回值的生命周期跟着**输入**，但认出来的名字其实借自 `COMMANDS` 那张恒表 ——
/// `&'static str` 可以当任意短的 `&'a str` 用。所以这里**一次分配都没有**，
/// 也不需要一个 `Cow` 去装「有时是表里的、有时是你自己写的」这两种来路。
fn canonical(name: &str) -> &str {
    COMMANDS
        .iter()
        .find(|spec| spec.answers_to(name))
        .map(|spec| spec.name)
        .unwrap_or(name)
}

/// 查表；别名也能直接查到（`d` / `D` → `delete`）。不认识的返回 `None`。
fn spec(name: &str) -> Option<&'static Spec> {
    COMMANDS.iter().find(|spec| spec.answers_to(name))
}

// ===== 解析 =====

/// 一个选项：`-f` / `--force` / `--key=value` 都解析成它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag<'a> {
    /// 名字：去掉 `-` / `--`，也去掉 `=value` 部分；**并且已经折成小写**。
    ///
    /// 为什么折在**解析**这一步：选项名**永远是我们定的**（不像位置参数里混着
    /// 用户的路径），所以这里折一次，后面所有地方比的都是普通的 `==` ——
    /// 大小写这件事不会散到每一个比较点上。
    ///
    /// 本来就全是小写时借原串（`Cow::Borrowed`），不分配。
    pub name: Cow<'a, str>,
    /// `--key=value` 的值（`-f` / `--force` 是 `None`）。
    ///
    /// ⚠️ 值**不折**：它是用户的数据（可能是个路径）。
    pub value: Option<&'a str>,
    /// 用户**原样**写的样子（`-f` / `--force`），只为了报错时能照原样还给他
    pub raw: &'a str,
}

/// 一条解析好的命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command<'a> {
    /// 规范命令名：**别名和大写都已经归一化**（`d` / `D` → `delete`）
    pub name: &'a str,
    /// 选项
    pub flags: Vec<Flag<'a>>,
    /// 位置参数，**保持书写顺序**（顺序就是它们的身份）
    pub args: Vec<&'a str>,
}

impl Command<'_> {
    /// 有没有这个选项（长名、短名任写一个都算；**比较是普通的 `==`**）。
    ///
    /// 用途：每个命令用它校验「认不认得这些选项」—— 见 [`reject_unknown_flags`]。
    pub fn has_flag(&self, long: &str, short: &str) -> bool {
        self.flags.iter().any(|flag| flag.is_named(long, short))
    }

    /// `--force` / `-f`：跳过「未保存改动」拦截
    pub fn force(&self) -> bool {
        self.has_flag("force", "f")
    }
}

impl<'a> Flag<'a> {
    /// 这个选项的名字是不是 `long` / `short` 之一。
    ///
    /// **整个仓库比选项名的地方就只有这里** —— 所以大小写的归一化只用做一次，
    /// 在这个方法里用普通的 `==` 就完了（名字在 [`parse_flag`] 里已经折成小写）。
    pub fn is_named(&self, long: &str, short: &str) -> bool {
        self.name == long || self.name == short
    }
}

/// 一行切开后的一块。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Piece<'a> {
    /// 一个词。**引号已经剥掉**（`"a b"` → `a b`）
    Word(&'a str),
    /// 连词 `&&`
    And,
}

/// 扫描一整行，切成一块一块（词 / `&&`）。
///
/// 这是命令层**唯一**需要状态的地方 —— 引号一开，空格和 `&&` 都不再是边界。
/// 别处全是「看自己就够了」的局部判断；状态机只住在这里，也只该住在这里。
///
/// ## 规则（跟 POSIX shell 的**本质**一致）
///
/// - `'...'` 和 `"..."` 都把里面的东西**原样**收成一个词。两种引号完全等价 ——
///   shell 里 `'...'` 还多一层「什么都不展开」的意思，而我们**根本没有展开**
///   （没有变量、通配、命令替换），所以那层区分不存在。两个都支持，只是让你
///   有个办法把引号本身写进词里：`'"'`、`"it's"`。
/// - **没有转义**：`\` 就是个普通字符。shell 需要 `\"` 是因为它要区分「展开出来的
///   引号」和「字面上的引号」，我们没有展开，那层区分照样不存在。
///   代价：同时含 `'` 和 `"` 的名字写不出来（Windows 上 `"` 本来就不许进文件名，
///   所以很罕见）。
/// - **引号必须包住整个词**，不能拼：`"my dir"/x.rs` 报错，写 `"my dir/x.rs"`。
///   shell 允许拼接是为了把「展开的结果」和字面量接起来（`"$dir"/x`），
///   我们没有展开可拼；而支持它就得让词变成拥有所有权的 `String`，
///   换来一个没人真需要的写法，不划算。报错是为了别让它静悄悄变成两个词。
/// - **引号没关上 = 错误**。shell 会换个提示符等你接着敲，命令模式一行一执行、
///   等不了，所以直接说清楚哪儿不对。
///
/// ## 引号只改「边界」，不改「身份」
///
/// `open "-weird.rs"` 里那个词**仍然**以 `-` 开头，所以**仍然被当成选项**。
/// 想表达「这是个路径」要用 `--`。这跟 shell 一模一样：
/// `ls "-la"` 和 `ls -la` 是一回事。
fn lex(line: &str) -> Result<Vec<Piece<'_>>, String> {
    let bytes = line.as_bytes();
    let mut pieces = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let byte = bytes[i];

        // 空白：词与词的分界，直接跳过
        if byte.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // `&&`：只有走到这里才说明它在引号外面
        if byte == b'&' && bytes.get(i + 1) == Some(&b'&') {
            pieces.push(Piece::And);
            i += 2;
            continue;
        }

        // 带引号的词：一直扫到收尾的那个引号
        if byte == b'\'' || byte == b'"' {
            let quote = byte;
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end] != quote {
                end += 1;
            }
            if end >= bytes.len() {
                return Err(format!("Unclosed {} quote", quote as char));
            }
            // 收尾引号后面必须是个边界，否则就是 `"a b"x` 这种想拼的写法
            let after = end + 1;
            let at_boundary = after >= bytes.len()
                || bytes[after].is_ascii_whitespace()
                || (bytes[after] == b'&' && bytes.get(after + 1) == Some(&b'&'));
            if !at_boundary {
                return Err("Quote must wrap the whole word".to_string());
            }
            pieces.push(Piece::Word(&line[start..end]));
            i = after;
            continue;
        }

        // 不带引号的词：扫到空白或 `&&` 为止。
        // ⚠️ 只有**词首**的引号才有特殊含义 —— 所以 `it's.txt` 里的撇号
        //    就只是个普通字符，不会触发「引号没关上」。
        let start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && !(bytes[i] == b'&' && bytes.get(i + 1) == Some(&b'&'))
        {
            i += 1;
        }
        pieces.push(Piece::Word(&line[start..i]));
    }

    Ok(pieces)
}

/// 把一行命令文本拆成 [`Command`]。
///
/// ⚠️ 这里**只看一条命令**，不管 `&&` —— 切链是 [`run`] 的活。
/// 两层分开的好处：解析器始终只需要处理「一个命令名 + 一串词」，不用知道链的存在。
///
/// 失败时返回一句**给人看的话**（会直接进状态栏），比如
/// `bad option: -force (short options are one letter; did you mean --force?)`。
pub fn parse(line: &str) -> Result<Command<'_>, String> {
    let words = lex(line)?
        .into_iter()
        .map(|piece| match piece {
            Piece::Word(word) => Ok(word),
            // 这一层不认 `&&`：链在 [`run`] 里就先切好了
            Piece::And => Err("`&&` can only appear between commands".to_string()),
        })
        .collect::<Result<Vec<&str>, String>>()?;
    parse_command(&words)
}

/// 把「一条命令的那几个词」分类成 [`Command`]。
///
/// 分词由 [`lex`] 做完了（只有它知道引号在哪），这里只做**分类**：
/// 第一个词是命令名，带 `-` 的是选项，其余按**书写顺序**是位置参数。
fn parse_command<'a>(words: &[&'a str]) -> Result<Command<'a>, String> {
    let Some((&first, rest)) = words.split_first() else {
        return Err("empty command".to_string());
    };

    let mut flags = Vec::new();
    let mut args = Vec::new();
    // 见过单独一个 `--` 之后，剩下的词全是位置参数
    let mut only_args = false;
    for &word in rest {
        if only_args {
            args.push(word);
        } else if word == "--" {
            only_args = true;
        } else if let Some(body) = word.strip_prefix('-').filter(|body| !body.is_empty()) {
            flags.push(parse_flag(word, body)?);
        } else {
            // 普通词；顺带把单独一个 `-` 也当位置参数（它不是合法选项）
            args.push(word);
        }
    }

    Ok(Command {
        name: canonical(first),
        flags,
        args,
    })
}

/// 解析一个选项词。`word` 是它原样，`body` 是去掉前导 `-` 之后的部分。
fn parse_flag<'a>(word: &'a str, body: &'a str) -> Result<Flag<'a>, String> {
    // ⚠️ 长选项的前导 `--` 有两个 `-`，调用方只剥掉了一个，这里得补上第二个。
    // 忘了这行就会得到名字为 `-force` 的选项 —— 它永远不会匹配 `force`，
    // 于是 `--force` 静默失效（比报错难查得多）。
    let long = word.starts_with("--");
    let body = if long { &body[1..] } else { body };
    let (name, value) = match body.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (body, None),
    };

    if name.is_empty() {
        return Err(format!("bad option: {word}"));
    }
    // 短选项只能一个字母 —— 我们不做 `-fx` 这种捆绑。
    // `-force` 要报错而不是悄悄当成 `--force`：让用户知道两种写法不能混。
    if !long && name.len() > 1 {
        return Err(format!(
            "bad option: {word} (short options are one letter; did you mean --{name}?)"
        ));
    }

    Ok(Flag {
        name: lowercase(name),
        value,
        raw: word,
    })
}

/// 折成 ASCII 小写；本来就全是小写时**借原串**（不分配）。
///
/// ⚠️ 用 `to_ascii_lowercase` 而不是 `to_lowercase`：后者的 Unicode 折叠会为
/// `İ` 变出两个字符、又会把开尔文符号 `K` 折成 `k` —— 那种「聪明」在命令语言里
/// 只会制造意外（我们表里全是 ASCII，本来也无需它）。
fn lowercase(name: &str) -> Cow<'_, str> {
    if name.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(name.to_ascii_lowercase())
    } else {
        Cow::Borrowed(name)
    }
}

// ===== 执行 =====

/// 一条命令执行完的结果。
///
/// - `Ok(Some(action))` —— 成功，而且要 main 去做点什么
/// - `Ok(None)` —— 成功，只是状态变化，main 不用管
/// - `Err(提示)` —— 失败
///
/// **失败走返回值，成功走状态栏。** 以前两者都塞在状态栏里，于是「成功但没副作用」
/// 和「失败」长得一模一样（都是 `None`）—— `&&` 正好卡在这个歧义上。分开之后，
/// 失败成了**控制流**（链要不要停），成功只是**汇报**（写一句就够）。
pub(crate) type Executed = Result<Option<Action>, String>;

/// 执行一整行命令（可能含 `&&` 链），返回 main 要**依次**执行的动作。
///
/// ## 短路：前一条失败就不做后面的
///
/// 这不是可选项，是安全要求。`copy 1 3 && delete 1 3`（复制完再删 = **移动**）
/// 这个惯用法里，要是复制失败了还接着删，用户那几行就**真没了**。
///
/// 想做「不管成败都接着做」，多敲一行命令就是了 —— **「都执行」本来就不需要语法**，
/// 它就是现状。真正需要专门语法的，是「有条件地执行」。
///
/// ## 什么算「成功」
///
/// **「该做的事没做成」就算失败。** 明显的错（`delete 99` 越界）算；
/// 「现状播报」也算 —— `undo` 撤到头了、`back` 已经在第一个，都是没做成。
///
/// 统一成一条规矩，比逐个争论「这算不算错」可靠；而且短路是**保守的方向**：
/// 它只会让链少做点，绝不会多做。
pub fn run(app: &mut App, line: &str) -> Vec<Action> {
    // 整行先扫一遍：**只有 [`lex`] 知道引号在哪**，所以 `&&` 的分割必须由它来做。
    // 「引号里的 `&&` 不是分隔符」就着落在这里。
    let pieces = match lex(line) {
        Ok(pieces) => pieces,
        Err(message) => {
            app.set_status_message(message);
            return Vec::new();
        }
    };

    // 按 `And` 分段：`&&` 把当前这段收走、重新开一段。
    // 用「当前段」这个局部变量，而不是 `commands.last_mut()`：
    // 后者要 `if let Some(...)`（空 Vec 没有最后一个）或 `unwrap`，
    // 而这里的「有没有当前段」本来就是个假问题 —— 一段必然在手上。
    let mut commands: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for piece in pieces {
        match piece {
            Piece::Word(word) => current.push(word),
            Piece::And => commands.push(std::mem::take(&mut current)),
        }
    }
    commands.push(current); // 最后一段收尾（空段也没事，下面会跳过）

    // 最后一段非空命令。报错时靠它判断「后面还有没有东西没跑」
    let last = commands.iter().rposition(|words| !words.is_empty());

    let mut actions = Vec::new();
    for (index, words) in commands.iter().enumerate() {
        // 空段直接跳过：敲个 `:` 又回车什么都不做；`&&` 挨在一起也不算错
        if words.is_empty() {
            continue;
        }
        match execute(app, words) {
            Ok(action) => actions.extend(action),
            Err(message) => {
                // 后面还有命令没跑，得说一声 —— 否则用户不知道链停在哪了
                let message = if last == Some(index) {
                    message
                } else {
                    format!("{message}  (stopped; nothing after && ran)")
                };
                app.set_status_message(message);
                break;
            }
        }
    }
    actions
}

/// 「位置参数里哪些词是**我们定的**」—— （命令名，关键字表）。
///
/// 位置参数里混着两种东西，而在切完之后它们长得一模一样：
///
/// - **我们定的词**：`all`、`number`、`path`… 该不挑大小写；
/// - **你写的字**：路径、行号。一个字节都不能碰 ——
///   `:open README.MD` 里的 `README.MD` 就是文件的真名
///   （Linux 上它和 `readme.md` 是两份东西）。
///
/// 区分它们不能靠「看这个词像不像路径」（那要靠猜，还猜不准），而是靠**声明**：
/// 表里写过它就归一化，没写过就原样传下去。于是「大小写不敏感」这件事
/// 只需要一个依据、一趟归一化（见 [`execute`]），而不是每个分支各自去比。
///
/// ⚠️ 这张小表以**命令名**为钥匙，命令改名时它会悄悄飘掉 ——
/// 测试 `every_declared_keyword_belongs_to_a_real_command` 守着。
const KEYWORDS: &[(&str, &[&str])] = &[
    // `all` = 整篇
    ("delete", &["all"]),
    ("copy", &["all"]),
    // `config` 的三个子命令
    ("config", &["path", "edit", "reload"]),
    // `set` 的旋钮名
    (
        "set",
        &[
            "number",
            "nonumber",
            "tabwidth",
            "scrolloff",
            "sidescrolloff",
            "lspmaxservers",
        ],
    ),
];

/// 这条命令声明了哪些关键字（没声明就是空）。
fn declared_keywords(command: &str) -> &'static [&'static str] {
    KEYWORDS
        .iter()
        .find(|(name, _)| *name == command)
        .map(|(_, keywords)| *keywords)
        .unwrap_or(&[])
}

/// 执行单条命令（[`lex`] 切出来的一段词）。
///
/// 失败一律走 `Err`：不认得的命令、写错的选项、不对的参数个数、干不成的事，
/// 都由调用方（[`run`]）决定怎么呈现。
fn execute(app: &mut App, words: &[&str]) -> Executed {
    let command = parse_command(words)?;

    // ① 认不认得这个名字
    let Some(spec) = spec(command.name) else {
        return Err(unknown_command_message(command.name));
    };

    // ② 选项校验。**选项只在声明的命令上有效** ——
    //    静默忽略的代价是「打错一个字母就没反应」，那种 bug 最难查。
    if let Some(flag) = command.flags.iter().find(|flag| !spec.accepts(flag)) {
        return Err(format!("Unknown option: {}  ({})", flag.raw, spec.usage));
    }

    // ③ 归一化：把**声明过的关键字**折成表里那个写法（小写）。
    //
    // 之后下面每一个分支都是普通的字面量模式（`("delete", ["all"])`）——
    // 大小写这件事在命令语言里只发生在**三个边界**上，各只做一次：
    //
    //   命令名 / 别名 → 查表时归一（[`canonical`]，认出就是表里那个名字）
    //   选项名         → 解析时归一（[`parse_flag`]）
    //   关键字参数     → 就是这里
    //
    // ⚠️ 折的**只有**表上声明过的词。别的词（路径、行号、正文）原样传下去 ——
    //    所以 `:open README.MD` 里的 `README.MD` 一个字节都不会变；
    //    而 `:open all` 里的 `all` 也不会被当成关键字（`open` 没声明它）。
    let keywords = declared_keywords(command.name);
    let args: Vec<&str> = command
        .args
        .iter()
        .map(|&word| {
            keywords
                .iter()
                .find(|keyword| keyword.eq_ignore_ascii_case(word))
                .copied()
                .unwrap_or(word)
        })
        .collect();
    let args = args.as_slice();
    let force = command.force();
    match (command.name, args) {
        // ---- 退出与导航 ----
        // `:q` 永远是「退出整个程序」；「返回上一级」是 `:back`。
        // 命令要的是**确定性**（写进 `&&` 里不该有歧义），
        // 而 `q` **按键**要的是**手感**（有上一级就返回，一个键走遍全树）。
        // 两者同名不同义是刻意的 —— 见 COMMANDS.md
        ("quit", []) => Ok(Some(Action::Quit)),
        ("back", []) => go_back(app, force),
        ("next", []) => go_next(app, force),
        ("ls", []) => {
            let shown = list_documents(app);
            Ok(shown.then_some(Action::WriteOutbox(OutFile::FileList)))
        }
        ("forget", [n]) => {
            forget_document(app, n)?;
            Ok(None)
        }

        // ---- 文档与配置 ----
        // 读盘 / 写盘都是副作用：这里只产出 Action，真正的活 main.rs 干
        ("open", [path]) => open_path(app, path, force),
        ("write", []) => Ok(Some(Action::Save)),
        // `:w <path>` —— 另存为。跟 `:open` 一样，相对路径相对的是
        // **当前文档所在的那个目录**（main 里用同一个 `current_directory()` 解）
        ("write", [path]) => Ok(Some(Action::SaveAs(path.to_string()))),
        ("wq", []) => Ok(Some(Action::SaveAndQuit)),
        ("settings", []) => open_settings(app, force),
        ("reload", []) => Ok(Some(Action::ReloadConfig)),
        ("config", []) => {
            app.set_status_message(format!("Config: {}", app.config.describe()));
            Ok(None)
        }
        ("config", ["path"]) => {
            report_config_path(app);
            Ok(None)
        }
        ("config", ["edit"]) => open_settings(app, force),
        ("config", ["reload"]) => Ok(Some(Action::ReloadConfig)),

        // ---- 后台任务 ----
        // **先把状态置上再返回 Action**：这样按下回车的那一帧就能看到
        // 「Checking…」—— 反馈必须在动作**开始那一刻**出现，不能等结果。
        // 两三秒的空白最让人怀疑「是不是没反应」，而这里本来就无从知道。
        ("check", []) => {
            if app.checking {
                return Err("Already checking".to_string());
            }
            app.checking = true;
            app.set_status_message("Checking…");
            Ok(Some(Action::RunCheck))
        }
        // 格式化：跟 `:check` 同一个形状 —— **先把状态置上再返回 Action**，
        // 让「Formatting…」在按下回车那一帧就出现，而不是等结果回来。
        //
        // ⚠️ 这里先挑一次提供者（纯查表，不碰任何活着的东西）：挑不到就**当场**说，
        //    不必先闪一下「Formatting…」再改口。main 那边还会再挑一次 —— 它自己
        //    本来就要那个提供者，多查一次纯函数比把 `&'static` 塞进 `Action` 便宜。
        ("format", []) => {
            if app.formatting {
                return Err("Already formatting".to_string());
            }
            if formatter::provider_for(app.file_path.as_deref()).is_none() {
                return Err("No formatter for this file type".to_string());
            }
            app.formatting = true;
            app.set_status_message("Formatting…");
            Ok(Some(Action::Format))
        }

        // ---- 编辑 ----
        // `delete` / `copy` 的第一个位置是**起点**、第二个是**终点**（都 1 基、含两端）。
        // `copy` 还多认 `行:列` 这种精确坐标 —— 位置参数只有「位置」一种概念，
        // 写 `行` 就是整行，写 `行:列` 就精确到列。一套语法，两种详略
        ("delete", ["all"]) => {
            // 不再绕「拼一个字符串再解析回来」那道弯 —— 全篇就是第 0 行到最后一行
            let last = app.buffer.get_line_count().saturating_sub(1);
            delete_rows(app, 0, last)?;
            Ok(None)
        }
        ("delete", [one]) => {
            delete_lines(app, one, one)?;
            Ok(None)
        }
        ("delete", [first, last]) => {
            delete_lines(app, first, last)?;
            Ok(None)
        }
        ("copy", ["all"]) => {
            let last = app.buffer.get_line_count().saturating_sub(1);
            copy_rows(app, 0, last)
        }
        ("copy", [one]) => copy_positions(app, one, one),
        ("copy", [from, to]) => copy_positions(app, from, to),
        ("swap", [x, y]) => {
            swap_lines(app, x, y)?;
            Ok(None)
        }
        ("insert", []) => {
            // 切模式是纯状态变化，不需要经过 Action / main
            app.set_mode(EditorMode::Edit);
            Ok(None)
        }
        ("undo", []) => {
            undo(app)?;
            Ok(None)
        }
        ("redo", []) => {
            redo(app)?;
            Ok(None)
        }

        // ---- 设置 ----
        ("set", ["number"]) => {
            app.config.show_line_numbers = true;
            Ok(None)
        }
        ("set", ["nonumber"]) => {
            app.config.show_line_numbers = false;
            Ok(None)
        }
        ("set", ["tabwidth", n]) => {
            set_tab_width(app, n)?;
            Ok(None)
        }
        ("set", ["scrolloff", n]) => {
            set_scroll_margin(app, n)?;
            Ok(None)
        }
        ("set", ["sidescrolloff", n]) => {
            set_side_scroll_margin(app, n)?;
            Ok(None)
        }
        ("set", ["lspmaxservers", n]) => {
            set_lsp_max_servers(app, n)?;
            Ok(None)
        }

        // ---- 诊断 ----
        ("errors", []) => {
            let shown = open_error_list(app);
            Ok(shown.then_some(Action::WriteOutbox(OutFile::ErrorLog)))
        }
        // ⚠️ 这里**不**拼正文，也不决定要不要铺 —— 正文需要池子（见
        //    [`Action::ShowLspStatus`]），而池子在主循环手上。
        //    正因如此这条命令**永远不会失败**（没有「没有东西可列」那种情况：
        //    内置那三条永远在表里）。
        ("lsp", []) => Ok(Some(Action::ShowLspStatus)),

        // 名字认得，但这组位置参数不是它接受的样子（少写了 / 多写了 / 子命令拼错了）
        _ => Err(spec.usage.to_string()),
    }
}

// ---------- 长输出：铺成一屏，还是写一行 ----------
//
// 有几条命令的输出**装不下一行**。它们和一个「一行状态栏」是两种东西：
//
//   一行状态栏   —— 一句话的回执（`:w` 说 Saved、`:check` 说跑完了）
//   一屏清单      —— 一份**内容**（有几个文档、这个文件有哪些毛病）
//
// 判断标准没法自动定，所以**在这里明说**：下面这几条走清单，其余全走状态栏。
//
//   `:ls`     → file_list.txt
//   `:errors` → error_log.txt
//
// 都从 [`open_list`] 出去 —— 一处定义，就不会出现「这条铺一屏、那条挤一行」
// 这种要靠记忆去维持的不一致。
//
// 清单除了铺到屏幕上，还会**落成文件**（见 `outbox.rs`）—— 长输出不该只活在
// 内存里，那样你想拿它去搜、去比对的时候就没辙了。
//
// ⚠️ 清单是**快照**：铺上去之后就不管了。诊断变了、文档列表变了，它自己不动 ——
//    想看新的就再敲一次那条命令。这样「屏幕上这份东西是什么时候的」永远是确定的。

/// 把一份清单铺到屏幕上，返回**是否真的铺了**。
///
/// ⚠️ 那个返回值不是装饰：调用方靠它决定要不要 `Action::WriteOutbox`。
/// 没铺却去写的话，写进去的是**上一份内容**（或者当前那份文档的正文）——
///  于是 `file_list.txt` 里躺着你正在编辑的代码，而下一次 `:ls` 之前它一直躺在那儿。
fn open_list(app: &mut App, kind: DocumentKind, content: String, empty_message: &str) -> bool {
    if content.is_empty() {
        app.set_status_message(empty_message.to_string());
        return false;
    }
    app.show_list(kind, content);
    true
}

/// `errors`：把当前文件现在的毛病列成一份只读清单。返回是否铺了。
fn open_error_list(app: &mut App) -> bool {
    open_list(
        app,
        DocumentKind::Errors,
        crate::diagnostic::list_text(&app.diagnostics),
        // 空清单不值得占一屏 —— 而且它会让人以为自己改好了
        "No problems in this file",
    )
}

/// `ls`：把打开过的文档列成一份只读清单。返回是否铺了。
fn list_documents(app: &mut App) -> bool {
    open_list(
        app,
        DocumentKind::DocumentList,
        app.documents.list_text(),
        "No documents opened yet",
    )
}

/// 「命令不认识」时给用户的话。
///
/// 单独拎出来是为了照顾两种容易踩的写法：
///
/// - `:q!` / `:open!` 这套以前是支持的，现在不认了；
/// - `-f back` 这种把选项写到命令名前面的 —— 第一个词永远是命令名，
///   所以这其实是「一条叫 `-f` 的命令」（shell 也一样：`-la ls` 是命令 `-la`）。
///
/// **不能只说一句「未知命令」**：用户会以为自己敲错了字，而真正的原因是写法变了。
///
/// ⚠️ `!` 现在另有工作（**按键**，进外部命令模式），所以顺带说一句
/// 「它不再是后缀」—— 这样用户才知道 `:q!` 不是「暂时没实现」。
fn unknown_command_message(name: &str) -> String {
    if name.starts_with('-') {
        return format!(
            "Unknown command: {name}  ——  options go after the command name, e.g. `:back -f`"
        );
    }

    let Some(bare) = name.strip_suffix('!') else {
        return format!("Unknown command: {name}");
    };
    // 去掉 `!` 之后得真是个已知命令，才给定向提示；否则还是普通报错
    let Some(spec) = spec(bare) else {
        return format!("Unknown command: {name}");
    };
    if spec.force {
        format!(
            "Unknown command: {name}  ——  `!` 不再是后缀（现在是个按键），要强制就用 `:{bare} --force`"
        )
    } else {
        format!("Unknown command: {name}  ——  `!` 不再是后缀（现在是个按键），直接用 `:{bare}`")
    }
}

// ===== 具体命令 =====

// ---------- 会顶掉当前文档的操作共用的守卫 ----------

/// 挡住时的提示。**必须写上 `--force` 这条出路**，否则用户就卡死了 ——
/// 他知道东西会丢，但没有办法表达「照做」。
const UNSAVED_CHANGES_HINT: &str =
    "Unsaved changes; `:w` to save, add `--force` to go anyway, or `:exit` to discard";

/// 脏检查：`--force` 放行、没脏放行，否则给一句（会说清楚出路）的话。
///
/// 挡住时返回 `Err` 而不是自己去写状态栏 —— 这样按键侧和命令侧各自决定怎么处理，
/// **提示文本却只有一份**。
fn ensure_no_unsaved_changes(app: &App, force: bool) -> Result<(), String> {
    if force || !app.dirty {
        return Ok(());
    }
    Err(UNSAVED_CHANGES_HINT.to_string())
}

/// 给**按键侧**用的版本：拦住了就自己把话写进状态栏，返回「是否被拦下」。
///
/// 按键没有「失败」这个通道（它返回的不是 `Result`），所以只能这么包一层。
pub(crate) fn blocked_by_unsaved_changes(app: &mut App) -> bool {
    match ensure_no_unsaved_changes(app, false) {
        Ok(()) => false,
        Err(reason) => {
            app.set_status_message(reason);
            true
        }
    }
}

// ---------- 退出与导航 ----------

/// `back`：回到上一个打开的文档。
///
/// 已经在第一个也算**失败** —— 「该做的事没做成」。所以 `back && ls` 在头一条文档上
/// 不会去执行 `ls`，`back && back` 也会在退无可退时自然停下。
fn go_back(app: &App, force: bool) -> Executed {
    // ⚠️ 虚拟视图（`:errors`）不在文档列表里 —— 它的「上一级」就是**进它之前
    //    那份文档**，而且是原样放回来，不是重新读盘。
    //    所以这条路上**不检查未保存改动**：什么都没丢，没什么要拦的。
    if app.kind.is_virtual() {
        return Ok(Some(Action::RestoreDocument));
    }
    let Some(previous) = app.documents.previous_path() else {
        return Err("Already at the first document".to_string());
    };
    ensure_no_unsaved_changes(app, force)?;
    Ok(Some(Action::OpenPath(previous)))
}

/// `next`：去下一个文档。已经在最后一个算失败（同 [`go_back`]）。
fn go_next(app: &App, force: bool) -> Executed {
    let Some(next) = app.documents.next_path() else {
        return Err("Already at the last document".to_string());
    };
    ensure_no_unsaved_changes(app, force)?;
    Ok(Some(Action::OpenPath(next)))
}

/// `forget <n>`：把第 n 个文档从列表里去掉（只影响列表，不动磁盘上的文件）。
fn forget_document(app: &mut App, number_text: &str) -> Result<(), String> {
    // 用户看到的是 1 基序号（和 `:ls` 一致），内部列表是 0 基
    match number_text.parse::<usize>() {
        Ok(number) if number >= 1 => match app.documents.forget(number - 1) {
            Some(path) => {
                app.set_status_message(format!("Forgot {path}"));
                Ok(())
            }
            None => Err(format!("No document {number} (see :ls)")),
        },
        _ => Err(FORGET_USAGE.to_string()),
    }
}

// ---------- 打开文档 / 配置 ----------

/// `open <path>`：产出「让 main.rs 去读盘并打开」的动作。
///
/// 打开别的文件会把当前 buffer 换掉，**未保存的改动会真的消失**
/// （`replace_document` 连撤销历史一起清空），所以默认要拦一下；
/// `--force` / `-f` 是用户明确表示「我知道会丢，照做」。
fn open_path(app: &App, path: &str, force: bool) -> Executed {
    ensure_no_unsaved_changes(app, force)?;
    Ok(Some(Action::OpenPath(path.to_string())))
}

/// `settings` / `config edit`：请 main.rs 打开配置文件来改
/// （文件不存在就先按内置模板生成一份）。
fn open_settings(app: &App, force: bool) -> Executed {
    // 打开设置文件同样会顶掉当前文档 → 脏了就拦下
    ensure_no_unsaved_changes(app, force)?;
    Ok(Some(Action::Settings))
}

/// `config path`：告诉用户配置是从哪个文件读来的。
///
/// 没找到文件时就把「该把文件建在哪」告诉他，并提一句有 `:settings` 这条捷径。
fn report_config_path(app: &mut App) {
    // 先 clone 出路径，否则借用 app.config_path 的同时没法再可变借用 app
    if let Some(path) = app.config_path.clone() {
        app.set_status_message(format!("Config file: {}", path.display()));
        return;
    }
    match Config::preferred_path() {
        Some(path) => app.set_status_message(format!(
            "No config file; `:settings` will create {}",
            path.display()
        )),
        None => app.set_status_message("No config file; set STBD_CONFIG to point at one"),
    }
}

// ---------- 编辑 ----------

/// `delete <first> <last>` 的**外壳**：把用户写的 1 基行号翻译成 0 基，
/// 再交给 [`delete_rows`]。
///
/// ⚠️ 「起 <= 止」的校验在这里，**不在 `delete_rows` 里** —— 因为按键那条路
/// 根本走不到这个错（选区的最左端天然就是起点，`App::selection` 已经排好序了）。
/// 与其在共用的实现里多一个永不触发的分支，不如把校验留在真正可能犯这个错的地方：
/// **人在键盘上敲的数字**。
fn delete_lines(app: &mut App, first_text: &str, last_text: &str) -> Result<(), String> {
    let (Some(first_row), Some(last_row)) = (
        parse_one_based_index(first_text),
        parse_one_based_index(last_text),
    ) else {
        return Err(format!("Invalid line number: {first_text} {last_text}"));
    };

    if first_row > last_row {
        return Err(format!("{DELETE_USAGE}  (first must be <= last)"));
    }

    delete_rows(app, first_row, last_row)
}

/// 删掉第 `first` 到第 `last` 行（0 基、含两端），并写上回执。
///
/// ## 它是**唯一**的那份「删一段行」的实现
///
/// 行选择模式（`V`）里的 `Delete` / `d`，和命令 `:delete <起> <终>`、`:delete all`，
/// 说的**是同一件事** —— 差别只在坐标是敲出来的还是从选区读出来的。
/// 所以两边都走这里，连状态栏那句话都只有一份（`:delete 1 2` 和选区的 `Delete`
/// 印出来的字一模一样）。
///
/// 真正动手的是 [`App::delete_row_range`]：记撤销步 + 删整段 + 同步 `dirty` 三合一。
pub(crate) fn delete_rows(app: &mut App, first: usize, last: usize) -> Result<(), String> {
    if !app.delete_row_range(first, last) {
        return Err(format!(
            "Line out of range: file has only {} lines",
            app.buffer.get_line_count()
        ));
    }

    if first == last {
        app.set_status_message(format!("Deleted line {}", first + 1));
    } else {
        app.set_status_message(format!("Deleted lines {} to {}", first + 1, last + 1));
    }
    Ok(())
}

/// `copy <位置> <位置>`：取一段文本包成 [`Action::Copy`] 交给 main 写剪贴板。
fn copy_positions(app: &App, from_text: &str, to_text: &str) -> Executed {
    let (Some(from), Some(to)) = (
        parse_position(from_text, false),
        parse_position(to_text, true),
    ) else {
        return Err(format!(
            "Invalid position: write `line` or `line:column`, both 1-based  ({COPY_USAGE})"
        ));
    };
    copy_range(app, from, to)
}

/// 解析一个「位置」（1 基）。
///
/// 只写行号时：当**起点**取行首（第 0 列），当**终点**取行尾（[`usize::MAX`]）。
/// 行尾不用去算那一行有多长 —— `get_text_in_range` 会把它夹到行末。
fn parse_position(text: &str, is_end: bool) -> Option<(usize, usize)> {
    match text.split_once(':') {
        // 写了列 = 精确坐标
        Some((row, col)) => Some((parse_one_based_index(row)?, parse_one_based_index(col)?)),
        // 只写行 = 整行
        None => Some((
            parse_one_based_index(text)?,
            if is_end { usize::MAX } else { 0 },
        )),
    }
}

/// 取「第 first 行到第 last 行」的**整行**文本（0 基、含两端）。
///
/// 行首传第 0 列、行尾传 `usize::MAX`（会被夹到行末），所以「整行」这件事
/// 不需要另写一套边界 —— 它和只写行号的 `:copy 1 3` 是同一条坐标。
///
/// 剪切那条路要的就是**文本本身**（得先拿出来才能删），所以它从这里取，
/// 而不是从 [`copy_rows`] 返回的动作里再择出来。
pub(crate) fn rows_text(app: &App, first: usize, last: usize) -> Result<String, String> {
    range_text(app, (first, 0), (last, usize::MAX))
}

/// `:copy all` / 选区的 `y` 走这条：把「这几行」包成 [`Action::Copy`]。
pub(crate) fn copy_rows(app: &App, first: usize, last: usize) -> Executed {
    rows_text(app, first, last).map(|text| Some(Action::Copy(text)))
}

/// 把第 `first` 到第 `last` 行整体上移 / 下移一行，并写上回执。
///
/// **唯一**的「移行」实现：行选择模式里的 `h` / `l` 走它（将来真加 `:move` 命令，
/// 也从这里出去 —— 一条逻辑一个入口）。
///
/// 真正动手的是 [`App::move_row_range`]：它是**用 swap 搭出来的**
/// （一段 N 行的块挪一格 == N 次相邻交换），所以这里和 `:swap` 之间也没有
/// 两套「换行」的实现。
pub(crate) fn move_rows(app: &mut App, first: usize, last: usize, up: bool) -> Result<(), String> {
    if !app.move_row_range(first, last, up) {
        return Err(format!(
            "Already at the {} of the file",
            if up { "top" } else { "bottom" }
        ));
    }
    // 回执只说**方向 + 行数**，不报行号：移完之后行号已经变了，
    // 报改动前那个数只会让人对不上屏幕上的行号栏
    let count = last - first + 1;
    let direction = if up { "up" } else { "down" };
    app.set_status_message(if count == 1 {
        format!("Moved 1 line {direction}")
    } else {
        format!("Moved {count} lines {direction}")
    });
    Ok(())
}

/// 取一段文本（坐标同上），拿不到就说清楚为什么。
///
/// **唯一**的「区间 → 文本 + 报错」实现：行号式的 [`rows_text`]、带列的
/// `:copy 2:3 5:7`，走的都是它。
fn range_text(app: &App, start: (usize, usize), end: (usize, usize)) -> Result<String, String> {
    app.get_text_in_range(start, end).ok_or_else(|| {
        format!(
            "Range out of bounds or reversed (file has {} lines)",
            app.buffer.get_line_count()
        )
    })
}

/// 取一段文本并包成 [`Action::Copy`]。
fn copy_range(app: &App, start: (usize, usize), end: (usize, usize)) -> Executed {
    range_text(app, start, end).map(|text| Some(Action::Copy(text)))
}

/// `swap <x> <y>`：交换两行。
fn swap_lines(app: &mut App, first_line_text: &str, second_line_text: &str) -> Result<(), String> {
    let (Some(first_row), Some(second_row)) = (
        parse_one_based_index(first_line_text),
        parse_one_based_index(second_line_text),
    ) else {
        return Err(format!(
            "Invalid line number: {first_line_text} {second_line_text}"
        ));
    };

    app.begin_undoable_command();
    if !app.buffer.swap_lines(first_row, second_row) {
        app.abort_undoable_command();
        return Err(format!(
            "Line out of range: file has only {} lines",
            app.buffer.get_line_count()
        ));
    }
    // ⚠️ 这里直接动了 buffer，所以必须自己说一声「文档变了」——
    //    `dirty` 是算出来的缓存，没人替我们刷新（同 `delete` 那个坑）
    app.sync_dirty();

    app.set_status_message(format!(
        "Swapped lines {} and {}",
        first_row + 1,
        second_row + 1
    ));
    Ok(())
}

/// 撤销一步。**没得撤销时返回 `Err`** ——「该做的事没做成」按失败算，
/// 于是 `undo && undo` 在撤到头时会自然停下。
fn undo(app: &mut App) -> Result<(), String> {
    if !app.undo() {
        return Err("Already at oldest change".to_string());
    }
    app.set_status_message("Undo");
    Ok(())
}

/// 重做一步。没得重做时返回 `Err`（同 [`undo`]）。
fn redo(app: &mut App) -> Result<(), String> {
    if !app.redo() {
        return Err("Already at newest change".to_string());
    }
    app.set_status_message("Redo");
    Ok(())
}

/// 给**按键**用的版本：失败时自己把话写进状态栏
/// （`u` 键按不出 `Result` —— 它没有可以把失败传出去的通道）。
pub fn undo_or_report(app: &mut App) {
    if let Err(message) = undo(app) {
        app.set_status_message(message);
    }
}

/// 给**按键**用的版本（`Ctrl+R` / `Ctrl+Y`）。
pub fn redo_or_report(app: &mut App) {
    if let Err(message) = redo(app) {
        app.set_status_message(message);
    }
}

// ---------- 设置 ----------

/// `set tabwidth N`：设置一次 Tab 插入的空格数
fn set_tab_width(app: &mut App, value_text: &str) -> Result<(), String> {
    match value_text.parse::<usize>() {
        Ok(new_width) if (1..=16).contains(&new_width) => {
            app.config.tab_width = new_width;
            app.set_status_message(format!("Tab width set to {new_width}"));
            Ok(())
        }
        _ => Err("Invalid tab width: use a number from 1 to 16".to_string()),
    }
}

/// `set scrolloff N`：设置光标与视口上下边缘保持的最小行距
fn set_scroll_margin(app: &mut App, value_text: &str) -> Result<(), String> {
    match value_text.parse::<usize>() {
        Ok(new_margin) if new_margin <= 100 => {
            app.config.scroll_margin = new_margin;
            app.set_status_message(format!("Scroll margin set to {new_margin}"));
            Ok(())
        }
        _ => Err("Invalid scroll margin: use a number from 0 to 100".to_string()),
    }
}

/// `set sidescrolloff N`：设置光标与视口左右边缘保持的最小列距
fn set_side_scroll_margin(app: &mut App, value_text: &str) -> Result<(), String> {
    match value_text.parse::<usize>() {
        Ok(new_margin) if new_margin <= 100 => {
            app.config.side_scroll_margin = new_margin;
            app.set_status_message(format!("Side scroll margin set to {new_margin}"));
            Ok(())
        }
        _ => Err("Invalid side scroll margin: use a number from 0 to 100".to_string()),
    }
}

/// `set lspmaxservers N`：设置最多同时保留几个语言服务器（`0` = 不开）。
///
/// ⚠️ 这里**只改那个数字**，不自己动手去踢服务器 —— 「多出来的那些当场收掉」
/// 是主循环的会话池在下一次同步时读到新数字之后干的。两边各管一件事：
/// 命令层只会改设置，进程的生死全归池子，就不会出现「两个地方都在杀进程」。
fn set_lsp_max_servers(app: &mut App, value_text: &str) -> Result<(), String> {
    match value_text.parse::<usize>() {
        // 上限跟 config.rs 里的 `MAX_LSP_MAX_SERVERS` 对齐；这儿再写一遍是因为
        // 这条命令面对的是「刚敲进去的一个数」，得能当场说清楚范围
        Ok(new_limit) if new_limit <= 8 => {
            app.config.lsp_max_servers = new_limit;
            let message = if new_limit == 0 {
                "Language servers off".to_string()
            } else {
                format!("Language servers limited to {new_limit}")
            };
            app.set_status_message(message);
            Ok(())
        }
        _ => Err("Invalid language server count: use a number from 0 to 8".to_string()),
    }
}

// ---------- 共用的小工具 ----------

/// 把「用户看到的 1 基行号」解析成内部 0 基（拒绝 0 / 非数字）。
fn parse_one_based_index(text: &str) -> Option<usize> {
    let number = text.parse::<usize>().ok()?;
    // ⚠️ 必须是 `then`（闭包，惰性），不能用 `then_some`（参数立刻求值）：
    // `then_some(number - 1)` 在 number == 0 时会先把 `0 - 1` 算出来，
    // 于是在 usize 上直接下溢 panic —— `:delete 0` 能当场把程序搞崩。
    (number >= 1).then(|| number - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;

    /// 跑一行命令，只要它产出的**第一个**动作。
    ///
    /// 这里**不模拟按键**：解析和执行本来就是纯函数式的两层，直接调 [`run`]
    /// 比走一遍 handle_key_event 更能定位问题（测试挂了就知道是解析还是执行）。
    /// 绝大多数测试只关心「这一行做了什么」，所以取第一个就够了 ——
    /// `&&` 链会产出多个，那种情况用 [`chain`]。
    fn run(app: &mut App, line: &str) -> Option<Action> {
        super::run(app, line).into_iter().next()
    }

    /// 跑一行命令，要它产出的**全部**动作（`&&` 链会有多个）。
    fn chain(app: &mut App, line: &str) -> Vec<Action> {
        super::run(app, line)
    }

    /// 造一个带若干行的 App 用来测 delete
    fn app_with(content: &str) -> App {
        App::from_content(None, content.to_string())
    }

    // ---------- 解析器：纯字符串，与 App 无关 ----------

    #[test]
    fn aliases_are_normalized_to_the_canonical_name() {
        for (input, expected) in [
            ("delete 1 2", "delete"),
            ("del 1 2", "delete"),
            ("d 1 2", "delete"),
            ("open x.rs", "open"),
            ("e x.rs", "open"),
            ("edit x.rs", "open"),
            ("stbd x.rs", "open"),
        ] {
            assert_eq!(parse(input).unwrap().name, expected, "{input}");
        }
    }

    #[test]
    fn command_names_and_aliases_ignore_case() {
        // PowerShell 的脾气：CapsLock 押没押上，命令都该照跑。
        // 归一化是在查表那一步做的，所以「别名」和「大写」在这里是同一件事 ——
        // `D` 既被认成别名 `d`，又不挑大小写。
        for (input, expected) in [
            ("DELETE 1 2", "delete"),
            ("Delete 1 2", "delete"),
            ("DEL 1 2", "delete"),
            ("D 1 2", "delete"),
            ("Q", "quit"),
            ("Quit", "quit"),
            ("OPEN x.rs", "open"),
            ("E x.rs", "open"),
            ("WQ", "wq"),
            ("SeT number", "set"),
        ] {
            assert_eq!(parse(input).unwrap().name, expected, "{input}");
        }
    }

    #[test]
    fn no_two_names_collide_once_case_is_ignored() {
        // 「不分大小写」的前提是**没有两个名字只差大小写** ——
        // 真有的话就得挑一个，而那是个谁也记不住的规则。
        // 这条守卫是给未来的人看的：哪天想给 `next` 再加个别名 `W`
        // （`w` 已经是 `write` 了），这里会先红。
        let mut seen: Vec<(String, &str)> = Vec::new();
        for spec in COMMANDS {
            for name in std::iter::once(spec.name).chain(spec.aliases.iter().copied()) {
                let key = name.to_ascii_lowercase();
                if let Some((_, owner)) = seen.iter().find(|(lower, _)| *lower == key) {
                    panic!(
                        "`{name}`（{}）和 `{owner}` 只差大小写 —— 那到底该认哪一个？",
                        spec.name
                    );
                }
                seen.push((key, spec.name));
            }
        }
    }

    #[test]
    fn the_same_command_in_caps_does_the_same_thing() {
        // 「查表认得」还不够 —— 认出来之后走的必须是**同一条分支**
        let mut app = app_with("a\nb\nc");
        run(&mut app, "DELETE All");
        assert_eq!(app.buffer.get_line_count(), 1, "`All` 也该算关键字");

        let mut app = app_with("a");
        assert_eq!(run(&mut app, "WRITE"), Some(Action::Save));
        assert_eq!(
            run(&mut app, "OPEN x.rs -F"),
            Some(Action::OpenPath("x.rs".to_string()))
        );
        assert_eq!(run(&mut app, "CONFIG RELOAD"), Some(Action::ReloadConfig));
        assert_eq!(run(&mut app, "CONFIG EDIT"), Some(Action::Settings));
        assert_eq!(
            chain(&mut app, "COPY ALL"),
            vec![Action::Copy("a".to_string())]
        );
        run(&mut app, "SET TABWIDTH 4");
        assert_eq!(app.config.tab_width, 4, "关键字也不该挑大小写");
        run(&mut app, "SET NONUMBER");
        assert!(!app.config.show_line_numbers);
    }

    #[test]
    fn your_own_words_are_never_case_folded() {
        // 「不分大小写」只针对**我们定的词**。位置参数里的路径是你写的字，
        // 把它折成小写就等于改了你的意思（Linux 上 `README.MD` 和 `readme.md` 是两份）。
        let mut app = app_with("a");
        assert_eq!(
            run(&mut app, "OPEN README.MD"),
            Some(Action::OpenPath("README.MD".to_string()))
        );
        assert_eq!(
            run(&mut app, "WRITE Notes.TXT"),
            Some(Action::SaveAs("Notes.TXT".to_string()))
        );
    }

    #[test]
    fn a_word_is_only_a_keyword_for_the_command_that_declared_it() {
        // 「我们定的词」和「你写的字」在参数里长得一模一样，唯一的区别是
        // **在不在那张表上**。`open` 没声明 `all`，所以那是个名字叫 ALL 的文件 ——
        // 这条守着那层边界（哪天有人图省事把 `all` 加成全局关键字，这里会红）。
        let mut app = app_with("a");
        assert_eq!(
            run(&mut app, "open ALL"),
            Some(Action::OpenPath("ALL".to_string())),
            "路径一个字节都不能被折掉"
        );
    }

    #[test]
    fn every_declared_keyword_belongs_to_a_real_command() {
        // `KEYWORDS` 是以**命令名**为钥匙的第二张小表 —— 命令改名时它会悄悄飘掉。
        // 顺手把「用法提示里得提到这个词」也一并盯上，表里打错字也会被抓住。
        for (command, keywords) in KEYWORDS {
            let Some(spec) = spec(command) else {
                panic!("`{command}` 不在命令表里 —— 关键字表飘了");
            };
            for keyword in *keywords {
                assert!(
                    spec.usage.contains(keyword),
                    "`{command}` 的用法提示里没有 `{keyword}`：要么表里打错了，要么提示忘了改"
                );
            }
        }
    }

    #[test]
    fn only_the_value_of_an_option_keeps_its_case() {
        // 选项名折成小写，**值一个字节都不碰** —— 值可能是路径
        let cmd = parse(r"set --Path=C:\Users\Me").unwrap();
        assert_eq!(cmd.flags[0].name, "path");
        assert_eq!(cmd.flags[0].value, Some(r"C:\Users\Me"));
        assert_eq!(cmd.flags[0].raw, r"--Path=C:\Users\Me");
    }

    #[test]
    fn option_order_does_not_matter() {
        let long = parse("open x.rs --force").unwrap();
        let short = parse("open -f x.rs").unwrap();
        // 位置参数（没名字，顺序才是身份）必须一样
        assert_eq!(long.args, short.args);
        // 选项（有名字）写在哪都一样
        assert!(long.force() && short.force());
    }

    #[test]
    fn positional_arguments_keep_the_order_they_were_written_in() {
        // 选项夹在位置参数中间，不能把参数挤乱
        let cmd = parse("open a -f b c").unwrap();
        assert_eq!(cmd.args, ["a", "b", "c"]);
    }

    #[test]
    fn long_option_value_is_written_with_an_equals_sign() {
        let cmd = parse("set --indent=4").unwrap();
        assert_eq!(cmd.flags.len(), 1);
        assert_eq!(cmd.flags[0].name, "indent");
        assert_eq!(cmd.flags[0].value, Some("4"));
        assert_eq!(cmd.flags[0].raw, "--indent=4");
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn double_dash_turns_everything_after_it_into_arguments() {
        // 用来打开以 `-` 开头的文件
        let cmd = parse("open -- -weird.rs").unwrap();
        assert!(cmd.flags.is_empty());
        assert_eq!(cmd.args, ["-weird.rs"]);
    }

    #[test]
    fn a_lone_dash_is_a_positional_argument_not_an_option() {
        let cmd = parse("open -").unwrap();
        assert!(cmd.flags.is_empty());
        assert_eq!(cmd.args, ["-"]);
    }

    #[test]
    fn multi_letter_short_option_is_rejected_with_a_hint() {
        // `-force` 不能悄悄当成 `--force`，得让用户知道两种写法不能混
        let err = parse("open x.rs -force").unwrap_err();
        assert!(err.contains("did you mean --force?"), "{err}");
    }

    #[test]
    fn a_bang_is_just_an_ordinary_character() {
        // `!` 不再有特殊含义 —— 它只是名字的一部分，于是名字查不到 → 未知命令。
        // （给 vim 肌肉记忆留的定向提示在执行层，见 `a_deprecated_bang_gets_a_pointer_to_force`）
        let cmd = parse("open! x.rs").unwrap();
        assert_eq!(cmd.name, "open!");
        assert!(cmd.flags.is_empty());
        assert_eq!(cmd.args, ["x.rs"]);
    }

    #[test]
    fn an_empty_line_cannot_be_parsed() {
        assert!(parse("").is_err());
        assert!(parse("   ").is_err());
    }

    #[test]
    fn an_option_cannot_come_before_the_command_name() {
        // 第一个词**永远**是命令名 —— 跟 shell 一样：`-la ls` 里 `-la` 就是命令名。
        // 所以 `-f back` 是「一条叫 `-f` 的命令」，不是「带 --force 的 back」。
        let mut app = app_with("a");
        run(&mut app, "-f back");
        assert!(
            app.status_message
                .contains("options go after the command name"),
            "{}",
            app.status_message
        );

        let mut app = app_with("a");
        run(&mut app, "--force back");
        assert!(
            app.status_message.starts_with("Unknown command: --force"),
            "{}",
            app.status_message
        );

        // 同样的两个词，顺序换一下就正常了 —— 报的是业务错误，不是语法错误
        let mut app = app_with("a");
        run(&mut app, "back -f");
        assert!(
            app.status_message.contains("first document"),
            "写对了就该被正常解析：{}",
            app.status_message
        );
    }

    // ---------- 引号：只改「边界」，不改「身份」 ----------

    #[test]
    fn quotes_group_a_word_with_spaces() {
        let cmd = parse(r#"open "my file.rs""#).unwrap();
        assert_eq!(cmd.args, ["my file.rs"]);
    }

    #[test]
    fn single_and_double_quotes_are_equivalent() {
        // 我们没有任何「展开」可抑制（无变量/通配/命令替换），
        // 所以 shell 里 `'...'` 和 `"..."` 的区别在这里不存在
        let double = parse(r#"open "a b""#).unwrap();
        let single = parse("open 'a b'").unwrap();
        assert_eq!(double.args, single.args);
    }

    #[test]
    fn either_quote_can_hold_the_other() {
        // 这就是「不支持转义」的补偿：要引号本身，换一种包
        let cmd = parse(r#"open 'it"s.txt'"#).unwrap();
        assert_eq!(cmd.args, [r#"it"s.txt"#]);

        let cmd = parse(r#"open "it's.txt""#).unwrap();
        assert_eq!(cmd.args, ["it's.txt"]);
    }

    #[test]
    fn an_apostrophe_mid_word_is_just_a_character() {
        // 只有**词首**的引号有特殊含义 —— 否则 `it's.txt` 这种名字就打不开了
        let cmd = parse("open it's.txt").unwrap();
        assert_eq!(cmd.args, ["it's.txt"]);
    }

    #[test]
    fn an_unclosed_quote_is_an_error() {
        // shell 会换个提示符等你接着敲；命令模式一行一执行，等不了
        let err = parse(r#"open "a b"#).unwrap_err();
        assert!(err.contains("Unclosed"), "{err}");
    }

    #[test]
    fn quotes_must_wrap_the_whole_word() {
        // 拼接（`"a b"x`）不做：shell 拼的是「展开的结果」，我们没有展开可拼。
        // 报错是为了别让它静悄悄变成两个词、然后在别处报个看不懂的错。
        let err = parse(r#"open "my dir"/x.rs"#).unwrap_err();
        assert!(err.contains("whole word"), "{err}");
    }

    #[test]
    fn quotes_hide_the_chain_separator() {
        // 这正是引号存在的理由：文件真叫 `a&&b.txt` 也打得开
        let mut app = app_with("x");
        assert_eq!(
            chain(&mut app, r#"open "a&&b.txt""#),
            vec![Action::OpenPath("a&&b.txt".to_string())]
        );
    }

    #[test]
    fn only_the_separators_outside_quotes_split_the_chain() {
        let mut app = app_with("x");
        assert_eq!(
            chain(&mut app, r#"open "a&&b.txt" && write"#),
            vec![Action::OpenPath("a&&b.txt".to_string()), Action::Save,]
        );
    }

    #[test]
    fn quotes_do_not_change_what_counts_as_an_option() {
        // 跟 shell 一致：`ls "-la"` 和 `ls -la` 是一回事。
        // 带引号和不带引号报的是**同一个错** —— 引号只改边界，不改身份。
        let mut quoted = app_with("x");
        run(&mut quoted, r#"open "-weird.rs""#);
        let mut bare = app_with("x");
        run(&mut bare, "open -weird.rs");
        assert_eq!(quoted.status_message, bare.status_message);
        assert!(quoted.status_message.contains("bad option"));

        // 想表达「这是个路径」，用 `--`（这也正是它存在的理由）
        let mut app = app_with("x");
        assert_eq!(
            run(&mut app, "open -- -weird.rs"),
            Some(Action::OpenPath("-weird.rs".to_string()))
        );
    }

    // ---------- 后台任务 ----------

    /// `:check` 只负责「起任务 + 立刻给反馈」，**不等结果**。
    ///
    /// 三条性质都在这儿：
    /// 1. 状态在按下回车那一刻就被置上 —— 不然你面对的是一两秒的空白，
    ///    完全没法区分「在跑」和「没反应」
    /// 2. 第二次会被挡下 —— 两个 `cargo check` 会去抢同一个 target 目录的锁
    /// 3. 被挡下时**不产出 Action** —— 否则 main 就会起第二个线程
    #[test]
    fn check_starts_a_background_task_and_refuses_a_second_one() {
        let mut app = App::new();
        assert!(!app.checking);

        assert_eq!(run(&mut app, "check"), Some(Action::RunCheck));
        assert!(app.checking, "得把状态置上，界面才知道该显示 Checking…");
        assert!(
            app.status_message.contains("Checking"),
            "{}",
            app.status_message
        );

        // 第二次：拒绝，并且不该再产出 Action
        assert_eq!(run(&mut app, "check"), None);
        assert!(
            app.status_message.contains("Already checking"),
            "{}",
            app.status_message
        );
    }

    /// `:fmt` 的三条性质跟 `:check` 一一对应（立刻给反馈 / 挡住第二次 / 挡下时不产 Action）。
    ///
    /// ⚠️ 但多一条 `:check` 没有的：**挑不到提供者就当场拒绝**。
    /// `cargo check` 对任何 Rust 项目都存在，而 `rustfmt` 只对 `.rs` 有意义 ——
    /// 对一个 `.txt` 先闪一下「Formatting…」再说「没这个格式的工具」，
    /// 读起来像是先答应了再反悔。
    #[test]
    fn format_starts_a_background_task_and_refuses_a_second_one() {
        let mut app = App::from_content(Some("a.rs".to_string()), "fn a(){}".to_string());
        assert!(!app.formatting);

        assert_eq!(run(&mut app, "format"), Some(Action::Format));
        assert!(app.formatting, "得把状态置上，界面才知道该显示 Formatting…");
        assert!(
            app.status_message.contains("Formatting"),
            "{}",
            app.status_message
        );

        // 第二次：拒绝，并且不该再产出 Action
        assert_eq!(run(&mut app, "fmt"), None, "`:fmt` 是别名，走的是同一条");
        assert!(
            app.status_message.contains("Already formatting"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn format_says_no_right_away_for_a_file_type_nobody_handles() {
        let mut app = App::from_content(Some("notes.txt".to_string()), "hello".to_string());

        assert_eq!(run(&mut app, "fmt"), None);
        assert!(
            app.status_message.contains("No formatter"),
            "{}",
            app.status_message
        );
        // ⚠️ 状态**不该**被置上：任务根本没起，界面不能显示「在跑」
        assert!(!app.formatting);
    }

    // ---------- 命令表本身 ----------

    #[test]
    fn every_command_in_the_table_is_reachable() {
        // 表里声明了却没实现，执行时会落到兜底分支只报一句用法 ——
        // 这个测试就是抓那个的。每个命令给一组「一定能走到行为分支」的参数。
        let invocations = [
            "quit",
            "back",
            "next",
            "ls",
            "forget 1",
            "open x.txt",
            "write",
            "wq",
            "settings",
            "reload",
            "config",
            "config path",
            "config edit",
            "config reload",
            "check",
            "format",
            "delete 1",
            "copy 1",
            "swap 1 2",
            "insert",
            "undo",
            "redo",
            "set number",
            "set nonumber",
            "set tabwidth 4",
            "set scrolloff 1",
            "set sidescrolloff 1",
            "set lspmaxservers 1",
            "errors",
            "lsp",
        ];

        for spec in COMMANDS {
            assert!(
                invocations
                    .iter()
                    .any(|line| line.split_whitespace().next() == Some(spec.name)),
                "`{}` 在表里，却没有测试真的调过它",
                spec.name
            );
        }

        for line in invocations {
            let mut app = app_with("a\nb\nc");
            app.documents.remember("a.txt");
            let action = run(&mut app, line);
            // 「有 Action」或「说了句别的」都算走到了行为分支；
            // 只剩一句 `Usage: ...` 就是没实现
            assert!(
                action.is_some() || !app.status_message.contains("Usage:"),
                "`{line}` 落到兜底分支了：{}",
                app.status_message
            );
        }
    }

    #[test]
    fn an_unknown_command_says_so() {
        let mut app = App::new();
        assert_eq!(run(&mut app, "frobnicate"), None);
        assert_eq!(app.status_message, "Unknown command: frobnicate");

        // 认不出来的时候**原样还给你**（连大小写一起）——
        // 报错里自作主张换个大小写，只会让人怀疑自己刚才敲的是什么
        let mut app = App::new();
        assert_eq!(run(&mut app, "FROBNICATE"), None);
        assert_eq!(app.status_message, "Unknown command: FROBNICATE");
    }

    #[test]
    fn a_deprecated_bang_gets_a_pointer_to_force() {
        let mut app = App::new();
        // `!` 不认了，但不能只说「未知命令」—— vim 的手会往这儿敲
        for (input, expected_hint) in [
            ("q!", ":q --force"),
            ("Q!", ":Q --force"),
            ("stbd! x.rs", ":stbd --force"),
            ("open! x.rs", ":open --force"),
            ("back!", ":back --force"),
        ] {
            run(&mut app, input);
            assert!(
                app.status_message.contains("`!` 不再是后缀")
                    && app.status_message.contains(expected_hint),
                "{input} → {}",
                app.status_message
            );
        }

        // 强制对这条命令没意义（`--force` 不会被接受）时，别瞎撺掇
        run(&mut app, "delete! 1");
        assert!(
            app.status_message.contains("直接用 `:delete`"),
            "{}",
            app.status_message
        );

        // `!` 后面不是已知命令就只是普通报错，不给定向提示
        run(&mut app, "nope!");
        assert_eq!(app.status_message, "Unknown command: nope!");
    }

    #[test]
    fn an_empty_command_does_nothing_quietly() {
        let mut app = app_with("a");
        app.set_status_message("before");
        assert_eq!(run(&mut app, "   "), None);
        assert_eq!(app.status_message, "before"); // 没被骂
    }

    // ---------- open ----------

    #[test]
    fn open_yields_an_action_without_touching_the_buffer() {
        let mut app = app_with("ab");
        assert_eq!(
            run(&mut app, "open src/main.rs"),
            Some(Action::OpenPath("src/main.rs".to_string()))
        );
        // 读盘是副作用：这里只能产出 Action，不能自己动内容
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("ab"));
    }

    #[test]
    fn open_accepts_its_aliases() {
        let mut app = App::new();
        for line in ["open README.md", "stbd README.md", "e README.md"] {
            assert_eq!(
                run(&mut app, line),
                Some(Action::OpenPath("README.md".to_string())),
                "{line}"
            );
        }
    }

    #[test]
    fn open_without_a_path_shows_the_usage() {
        let mut app = App::new();
        for line in ["open", "stbd"] {
            assert!(run(&mut app, line).is_none(), "{line}");
            assert!(
                app.status_message.contains("Usage: open"),
                "{line} → {}",
                app.status_message
            );
        }
    }

    #[test]
    fn open_is_blocked_by_unsaved_changes() {
        let mut app = app_with("a");
        app.insert_char_at_cursor('x');
        assert!(app.dirty);

        assert_eq!(run(&mut app, "open other.txt"), None);
        assert!(
            app.status_message.contains("Unsaved"),
            "{}",
            app.status_message
        );

        // 两条「我知道会丢，照做」的写法都得放行（长写、短写）
        for line in ["open --force other.txt", "open -f other.txt"] {
            assert_eq!(
                run(&mut app, line),
                Some(Action::OpenPath("other.txt".to_string())),
                "{line}"
            );
        }
    }

    #[test]
    fn open_rejects_an_unknown_option() {
        let mut app = App::new();
        assert!(run(&mut app, "open x.rs --frobnicate").is_none());
        assert!(
            app.status_message.contains("Unknown option: --frobnicate"),
            "{}",
            app.status_message
        );
    }

    // ---------- delete ----------

    #[test]
    fn delete_one_line_removes_exactly_one_row() {
        let mut app = app_with("a\nb\nc");
        assert_eq!(run(&mut app, "delete 2"), None);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("a"));
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("c"));
        assert!(app.status_message.contains("Deleted line 2"));
    }

    #[test]
    fn delete_a_range_removes_contiguous_rows_including_both_ends() {
        let mut app = app_with("a\nb\nc\nd\ne");
        run(&mut app, "delete 2 4"); // b、c、d
        assert_eq!(app.buffer.get_line_count(), 2);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("a"));
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("e"));
        assert!(app.status_message.contains("Deleted lines 2 to 4"));
    }

    #[test]
    fn delete_accepts_its_aliases() {
        let mut app = app_with("a\nb\nc");
        run(&mut app, "d 1");
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("b"));
        run(&mut app, "del 1");
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("c"));
    }

    #[test]
    fn delete_rejects_a_reversed_range_and_changes_nothing() {
        let mut app = app_with("a\nb\nc\nd\ne");
        run(&mut app, "delete 4 2");
        assert!(
            app.status_message.contains("first must be <= last"),
            "{}",
            app.status_message
        );
        assert_eq!(app.buffer.get_line_count(), 5);
    }

    #[test]
    fn delete_out_of_range_changes_nothing_and_leaves_no_undo_step() {
        let mut app = app_with("a\nb");
        run(&mut app, "delete 99");
        assert!(
            app.status_message.contains("out of range"),
            "{}",
            app.status_message
        );
        assert_eq!(app.buffer.get_line_count(), 2);
        // 失败的命令不该留下「撤销了却什么都没变」的空步
        assert!(!app.undo());
        assert!(!app.dirty);
    }

    #[test]
    fn delete_without_arguments_shows_the_usage() {
        let mut app = app_with("a\nb");
        assert!(run(&mut app, "delete").is_none());
        assert!(
            app.status_message.contains("Usage: delete"),
            "{}",
            app.status_message
        );
        assert_eq!(app.buffer.get_line_count(), 2);
    }

    #[test]
    fn delete_rejects_an_unknown_option() {
        // 选项只在声明它的命令上有效：`--force` 是 open 的，delete 不认
        let mut app = app_with("a\nb");
        assert!(run(&mut app, "delete 1 --force").is_none());
        assert!(
            app.status_message.contains("Unknown option: --force"),
            "{}",
            app.status_message
        );
        assert_eq!(app.buffer.get_line_count(), 2);
    }

    #[test]
    fn command_edits_mark_the_document_dirty() {
        // ⚠️ 这两个命令**曾经改了文档却不把 `dirty` 置起来**（`dirty` 是算出来的
        // 缓存，谁直接动 buffer 谁就得自己说一声）—— 后果不是界面不好看，
        // 而是 `:q` 不再拦你，几行改动直接就没了。这条守着这个不变量。
        for line in ["delete 2", "swap 1 2"] {
            let mut app = app_with("a\nb\nc");
            assert!(!app.dirty, "起点得是干净的");
            run(&mut app, line);
            assert!(
                app.dirty,
                "`{line}` 改了文档，dirty 必须跟着起来（否则 `:q` 会放走没保存的改动）"
            );
        }
    }

    #[test]
    fn delete_all_keeps_exactly_one_empty_line() {
        for content in ["a\nb", "only"] {
            let mut app = app_with(content);
            run(&mut app, "delete all");
            assert_eq!(app.buffer.get_line_count(), 1, "{content}");
            assert_eq!(app.buffer.get_line(0).as_deref(), Some(""), "{content}");
        }
    }

    #[test]
    fn delete_is_undoable() {
        let mut app = app_with("a\nb\nc");
        run(&mut app, "delete 2");
        assert_eq!(app.buffer.get_line_count(), 2);

        assert!(app.undo());
        assert_eq!(app.buffer.get_line_count(), 3);
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("b"));
    }

    #[test]
    fn the_old_delete_line_syntax_is_no_longer_recognised() {
        // `line` 是旧语法的残留，现在只是个普通位置参数 ——
        // 所以 `delete line 2` 不会删一行，而是抱怨 "line" 不是行号。
        // 留着这个测试是给文档一个可执行的注脚。
        let mut app = app_with("a\nb\nc");
        run(&mut app, "delete line 2");
        assert!(
            app.status_message.contains("Invalid line number"),
            "{}",
            app.status_message
        );
        assert_eq!(app.buffer.get_line_count(), 3);
    }

    // ---------- `&&` 链 ----------

    #[test]
    fn a_chain_runs_both_commands_in_order() {
        // `copy ... && delete ...` 就是「剪下」：先复制到剪贴板，再把原文删掉
        let mut app = app_with("a\nb\nc");
        assert_eq!(
            chain(&mut app, "copy 1 2 && delete 1 2"),
            vec![Action::Copy("a\nb".to_string())]
        );
        // 第二条真的执行了：前两行没了
        assert_eq!(app.buffer.get_line_count(), 1);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("c"));
    }

    #[test]
    fn a_chain_can_produce_several_actions() {
        let mut app = app_with("a");
        assert_eq!(
            chain(&mut app, "write && wq"),
            vec![Action::Save, Action::SaveAndQuit]
        );
    }

    #[test]
    fn a_failing_command_stops_the_chain() {
        // 这就是短路的**理由**：复制失败还接着删，用户那几行就真没了
        let mut app = app_with("a\nb\nc");
        assert!(chain(&mut app, "copy 1 99 && delete 1 99").is_empty());
        assert_eq!(app.buffer.get_line_count(), 3); // 删没跑
        assert!(
            app.status_message.contains("Range out of bounds"),
            "{}",
            app.status_message
        );
        // 而且得说一声后面的没跑，否则用户不知道链停在哪了
        assert!(
            app.status_message.contains("stopped; nothing after && ran"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn a_failure_at_the_very_end_of_a_chain_says_nothing_about_stopping() {
        // 后面本来就没东西，再提「后面的没跑」就是废话
        let mut app = app_with("a\nb");
        chain(&mut app, "ls && delete 99");
        assert!(
            app.status_message.contains("out of range"),
            "{}",
            app.status_message
        );
        assert!(
            !app.status_message.contains("stopped"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn an_unknown_command_stops_the_chain_too() {
        let mut app = app_with("a\nb");
        chain(&mut app, "nope && delete 1");
        assert!(
            app.status_message.starts_with("Unknown command: nope"),
            "{}",
            app.status_message
        );
        assert_eq!(app.buffer.get_line_count(), 2);
    }

    #[test]
    fn a_dangling_or_empty_ampersand_is_forgiven() {
        // 还没打完就回车（`w &&`）：做前半段就好，不该报错
        let mut app = app_with("a");
        assert_eq!(chain(&mut app, "w &&"), vec![Action::Save]);

        // 挨在一起的 `&&` 是空段，跳过就行
        let mut app = app_with("a");
        assert!(chain(&mut app, "&& ls &&").is_empty());
    }

    #[test]
    fn nothing_to_do_counts_as_a_failure() {
        // 「该做的事没做成」= 失败。这条规矩最容易起争议，所以钉住它：
        // 没有可撤销的东西时 `undo` 没做成事 → 链停在这里。
        let mut app = app_with("a");
        assert!(chain(&mut app, "undo && write").is_empty());
        assert!(
            app.status_message.contains("oldest"),
            "{}",
            app.status_message
        );

        // 已经在第一个文档时 `back` 同理
        let mut app = app_with("a");
        app.documents.remember("only.txt");
        assert!(chain(&mut app, "back && write").is_empty());
        assert!(
            app.status_message.contains("first document"),
            "{}",
            app.status_message
        );
    }

    // ---------- write / 另存为 ----------

    #[test]
    fn write_takes_an_optional_path() {
        let mut app = app_with("a");
        assert_eq!(run(&mut app, "write"), Some(Action::Save));
        // `:w <path>` = 另存为（写盘 + 改缓冲区名字都是 main 的事）
        assert_eq!(
            run(&mut app, "write notes.txt"),
            Some(Action::SaveAs("notes.txt".to_string()))
        );
    }

    #[test]
    fn every_command_falls_back_to_its_own_usage() {
        // 用户看到的「参数按什么顺序写」就在 usage 字符串里，而它和 `match` 分居两处，
        // 会走歪。这条钉住：每个命令乱给一堆参数，必须报**自己的**用法 ——
        // 哪天有人加了个会吞任意参数的分支，这里就红。
        for spec in COMMANDS {
            let mut app = app_with("a");
            run(&mut app, &format!("{} 1 2 3 4 5 6 7", spec.name));
            assert_eq!(
                app.status_message, spec.usage,
                "`{}` 没有兜底到自己的用法",
                spec.name
            );
        }
    }

    // ---------- errors ----------

    fn app_with_diagnostics() -> App {
        let mut app = app_with("one\ntwo\nthree");
        app.set_diagnostics(vec![
            crate::diagnostic::Diagnostic {
                line: 2,
                severity: crate::diagnostic::Severity::Error,
                message: "boom".to_string(),
            },
            crate::diagnostic::Diagnostic {
                line: 0,
                severity: crate::diagnostic::Severity::Warning,
                message: "meh".to_string(),
            },
        ]);
        app
    }

    #[test]
    fn errors_opens_a_read_only_list_of_the_problems() {
        let mut app = app_with_diagnostics();

        // 铺完还要**落成文件** —— 长输出不该只活在内存里
        assert_eq!(
            run(&mut app, "errors"),
            Some(Action::WriteOutbox(OutFile::ErrorLog))
        );

        assert_eq!(app.kind, crate::app::DocumentKind::Errors);
        // 按行号排好、一条一行
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("1: warning: meh"));
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("3: error: boom"));
    }

    /// `:lsp` 只产出一个动作，**不在命令层拼正文**。
    ///
    /// ⚠️ 这不是「实现细节」：正文里有一列是「现在跑着哪几个」，而那只有池子
    /// 知道 —— 池子是活的（进程、线程、通道），按这个文件顶上的纪律不该让
    /// 命令层碰。所以命令层只举手，正文和铺屏都归主循环。
    ///
    /// 这里同时也钉住了「它永远不会失败」：内置那三条永远在表里，
    /// 所以不存在 `:errors` 那种「没东西可列就别铺」的情况。
    #[test]
    fn lsp_asks_the_main_loop_to_show_its_list() {
        let mut app = app_with("hello");

        assert_eq!(run(&mut app, "lsp"), Some(Action::ShowLspStatus));

        // 命令层**没有**动屏幕 —— 铺屏是主循环的事
        assert_eq!(app.kind, crate::app::DocumentKind::File);
    }

    /// 它和 `:errors` / `:ls` 一样**不需要 `--force`** —— 而且给了还会被拒。
    ///
    /// 这不是形式主义：`force` 的唯一理由是「这个命令会弄丢什么吗」。
    /// `:lsp` 铺清单的时候当前那份文档会被原样存下来，`q` 时一个字不差地
    /// 放回去（和 `:errors` 同一条路），所以它不丢东西 —— 那就**不该**
    /// 让用户养成「凡是要顶掉屏幕就得加 --force」这种习惯。
    #[test]
    fn lsp_rejects_force_because_it_destroys_nothing() {
        let mut app = app_with("hello");
        assert!(run(&mut app, "lsp --force").is_none());
        assert!(
            app.status_message.contains("Unknown option: --force"),
            "{}",
            app.status_message
        );
        // 而且什么都没发生：屏幕还是原来那份文档
        assert_eq!(app.kind, crate::app::DocumentKind::File);
    }

    /// `:ls` 铺一屏之后**也要落成文件**。
    #[test]
    fn ls_puts_the_list_on_screen_and_writes_it_out() {
        let mut app = app_with("one");
        app.documents.remember("a.txt");

        assert_eq!(
            run(&mut app, "ls"),
            Some(Action::WriteOutbox(OutFile::FileList))
        );

        assert_eq!(app.kind, crate::app::DocumentKind::DocumentList);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("1 *a.txt"));
    }

    /// ⚠️ 没东西可列时**不能**产出写入动作。
    ///
    /// 产出了的话，main 会把 `app.buffer` 抄进那个文件 —— 而这时候缓冲区里
    /// 是**你正在编辑的那份文档**。于是 `file_list.txt` 里躺着你的代码，
    /// 而且在下一次 `:ls` 之前它一直躺在那儿。
    #[test]
    fn an_empty_list_writes_nothing() {
        let mut app = app_with("fn main() {}");

        assert_eq!(run(&mut app, "errors"), None);
        assert_eq!(run(&mut app, "ls"), None);
    }

    /// ⚠️ 从清单 `:back` 要**原样放回**那份文档，不是重新读盘。
    #[test]
    fn errors_with_nothing_to_report_says_so_instead_of_opening() {
        let mut app = app_with("one");

        run(&mut app, "errors");

        assert_eq!(app.kind, crate::app::DocumentKind::File);
        assert!(
            app.status_message.contains("No problems"),
            "{}",
            app.status_message
        );
    }

    /// ⚠️ 从清单 `:back` 要**原样放回**那份文档，不是重新读盘。
    #[test]
    fn back_from_the_list_restores_instead_of_reopening() {
        let mut app = app_with_diagnostics();
        app.file_path = Some("a.rs".to_string());
        app.documents.remember("a.rs");
        run(&mut app, "errors");

        let action = run(&mut app, "back");

        assert_eq!(action, Some(Action::RestoreDocument));
    }

    /// 从清单 `:back` **不做脏检查** —— 什么都没丢，没什么要拦的。
    ///
    /// 拦了的话你会被堵在清单里出不来（`:back` 要 `--force` 才动），
    /// 而那正是「刚改完代码看错误」的时刻。
    #[test]
    fn back_from_the_list_is_not_blocked_by_unsaved_changes() {
        let mut app = app_with_diagnostics();
        app.file_path = Some("a.rs".to_string());
        app.documents.remember("a.rs");
        run(&mut app, "errors");
        // 在清单里弄成「没保存」的样子（实际情况里它来自进清单之前那次编辑）
        app.dirty = true;

        let action = run(&mut app, "back");

        assert_eq!(action, Some(Action::RestoreDocument));
    }

    // ---------- 清单不会自己长出来 ----------

    /// ⚠️ 清单**不进**文档列表 —— 它不是一个「打开过的文档」。
    ///
    /// 进去的话有两处会立刻坏掉：
    /// - `:ls` 会把自己也列出来，而且列一次多一条；
    /// - `q` 会开始**在清单之间打转**（因为清单成了「上一级」）。
    #[test]
    fn running_ls_does_not_add_the_list_to_the_document_list() {
        let mut app = app_with("one");
        app.documents.remember("a.txt");
        app.documents.remember("b.txt");

        run(&mut app, "ls");
        run(&mut app, "back");
        run(&mut app, "ls");

        // 还是那两个 —— 清单自己没混进去
        assert_eq!(app.documents.list_text(), "1 a.txt\n2 *b.txt");
    }

    /// ⚠️ 两份清单之间来回切，**快照不跟着换**：`q` 一步就回到文档上。
    ///
    /// （这条只看命令层能看的那一半：`:back` 产出的是「原样放回」而不是
    ///  「重新打开」。真正的放回动作由 main 执行，那边另有一条测试。）
    #[test]
    fn switching_between_two_lists_still_asks_for_a_restore_not_a_reopen() {
        let mut app = app_with_diagnostics();
        app.file_path = Some("a.rs".to_string());
        app.documents.remember("a.rs");

        run(&mut app, "ls");
        assert_eq!(app.kind, crate::app::DocumentKind::DocumentList);
        run(&mut app, "errors");
        assert_eq!(app.kind, crate::app::DocumentKind::Errors);

        // ⚠️ 必须是 RestoreDocument，不能是 OpenPath —— 后者会重新读盘、
        //    丢掉没保存的改动，而且会被「未保存」拦下来堵在清单里
        assert_eq!(run(&mut app, "back"), Some(Action::RestoreDocument));
    }

    /// 清单里的内容是**铺上去那一刻的快照**，之后不刷新。
    ///
    /// 诊断变了它也照旧 —— 这样「屏幕上这份东西是什么时候的」永远是确定的。
    #[test]
    fn the_list_does_not_refresh_behind_your_back() {
        let mut app = app_with_diagnostics();
        app.file_path = Some("a.rs".to_string());
        run(&mut app, "errors");
        assert_eq!(app.buffer.get_line_count(), 2);

        // 服务器又推了一份（一条都没有了）
        app.set_diagnostics(Vec::new());

        assert_eq!(
            app.buffer.get_line_count(),
            2,
            "清单自己刷新了 —— 那屏幕上那份东西的「年龄」就说不清了"
        );
    }
}
