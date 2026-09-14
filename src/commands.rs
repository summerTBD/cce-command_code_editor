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

use crate::app::{App, EditorMode};
use crate::config::Config;

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
}

// ===== 命令表 =====

// 用法提示写成 const：表里引用它，具体命令的报错分支也引用它 ——
// 一处定义，两边用的是同一个字符串。
const QUIT_USAGE: &str = "Usage: quit";
const BACK_USAGE: &str = "Usage: back [--force]";
const NEXT_USAGE: &str = "Usage: next [--force]";
const LS_USAGE: &str = "Usage: ls";
const FORGET_USAGE: &str = "Usage: forget <n>  (n comes from :ls)";
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
const SET_USAGE: &str = "Usage: set number | set nonumber | set tabwidth <n> | set scrolloff <n> | set sidescrolloff <n>";

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
    /// 这条命令认不认这个选项。
    ///
    /// v1 只有 `--force` 一个选项，所以判断这么简单就够了；
    /// 等真有第二个选项，这里就换成一张「选项名 → 短名」的表。
    fn accepts(&self, flag: &Flag<'_>) -> bool {
        self.force && (flag.name == "force" || flag.name == "f")
    }
}

/// 把别名归一化成规范名；不认识的返回原样。
fn canonical(name: &str) -> &str {
    COMMANDS
        .iter()
        .find(|spec| spec.name == name || spec.aliases.contains(&name))
        .map(|spec| spec.name)
        .unwrap_or(name)
}

/// 查表；别名也能直接查到（`d` → `delete`）。不认识的返回 `None`。
fn spec(name: &str) -> Option<&'static Spec> {
    let name = canonical(name);
    COMMANDS.iter().find(|spec| spec.name == name)
}

// ===== 解析 =====

/// 一个选项：`-f` / `--force` / `--key=value` 都解析成它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag<'a> {
    /// 名字：去掉 `-` / `--`，也去掉 `=value` 部分
    pub name: &'a str,
    /// `--key=value` 的值（`-f` / `--force` 是 `None`）
    pub value: Option<&'a str>,
    /// 用户**原样**写的样子（`-f` / `--force`），只为了报错时能照原样还给他
    pub raw: &'a str,
}

/// 一条解析好的命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command<'a> {
    /// 规范命令名（别名已归一化：`d` → `delete`）
    pub name: &'a str,
    /// 选项
    pub flags: Vec<Flag<'a>>,
    /// 位置参数，**保持书写顺序**（顺序就是它们的身份）
    pub args: Vec<&'a str>,
}

impl Command<'_> {
    /// 有没有这个选项（长名、短名任写一个都算）。
    ///
    /// 用途：每个命令用它校验「认不认得这些选项」—— 见 [`reject_unknown_flags`]。
    pub fn has_flag(&self, long: &str, short: &str) -> bool {
        self.flags
            .iter()
            .any(|flag| flag.name == long || flag.name == short)
    }

    /// `--force` / `-f`：跳过「未保存改动」拦截
    pub fn force(&self) -> bool {
        self.has_flag("force", "f")
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
        name,
        value,
        raw: word,
    })
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
type Executed = Result<Option<Action>, String>;

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

    // ③ 按「命令名 + 位置参数形状」匹配。选项在 ② 里已经验完，
    //    所以这里只看位置参数 —— 它们是有形状的（几个、什么顺序）。
    let args = command.args.as_slice();
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
            app.set_status_message(format!("Documents: {}", app.documents.describe()));
            Ok(None)
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

        // ---- 编辑 ----
        // `delete` / `copy` 的第一个位置是**起点**、第二个是**终点**（都 1 基、含两端）。
        // `copy` 还多认 `行:列` 这种精确坐标 —— 位置参数只有「位置」一种概念，
        // 写 `行` 就是整行，写 `行:列` 就精确到列。一套语法，两种详略
        ("delete", ["all"]) => {
            let last = app.buffer.get_line_count().to_string();
            delete_lines(app, "1", &last)?;
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
            copy_range(app, (0, 0), (last, usize::MAX))
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

        // 名字认得，但这组位置参数不是它接受的样子（少写了 / 多写了 / 子命令拼错了）
        _ => Err(spec.usage.to_string()),
    }
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

/// `delete <first> <last>`：删掉这些行（1 基，含两端；单行时 first == last）。
///
/// 内部用 `Buffer::delete_lines(start, count)` 一次删整段，
/// 避免「删一行后下标前移」导致删错行。
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

    let delete_count = last_row - first_row + 1; // 含两端
    app.begin_undoable_command();
    if !app.buffer.delete_lines(first_row, delete_count) {
        // 越界：把 begin_undoable_command 存下的空步回滚掉，别留下「撤销了却没变」的坑
        app.abort_undoable_command();
        return Err(format!(
            "Line out of range: file has only {} lines",
            app.buffer.get_line_count()
        ));
    }

    app.buffer.ensure_at_least_one_line(); // 删光后保留一个空行
    if first_row == last_row {
        app.set_status_message(format!("Deleted line {}", first_row + 1));
    } else {
        app.set_status_message(format!(
            "Deleted lines {} to {}",
            first_row + 1,
            last_row + 1
        ));
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

/// 取一段文本并包成 `Action::Copy`；坐标非法就说清楚为什么不行。
fn copy_range(app: &App, start: (usize, usize), end: (usize, usize)) -> Executed {
    match app.get_text_in_range(start, end) {
        Some(text) => Ok(Some(Action::Copy(text))),
        None => Err(format!(
            "Range out of bounds or reversed (file has {} lines)",
            app.buffer.get_line_count()
        )),
    }
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
    }

    #[test]
    fn a_deprecated_bang_gets_a_pointer_to_force() {
        let mut app = App::new();
        // `!` 不认了，但不能只说「未知命令」—— vim 的手会往这儿敲
        for (input, expected_hint) in [
            ("q!", ":q --force"),
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
}
