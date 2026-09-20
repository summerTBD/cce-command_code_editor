//! 格式化 —— 项目里**第一个「提供者」**
//!
//! ## 为什么缩进自己做，格式化却要叫别人来
//!
//! 因为这两件事的**性质**不一样：
//!
//! - 「回车之后该缩进多少」**没有确定答案** —— 它取决于你脑子里接下来想写什么。
//!   只能猜，而猜错的代价（你打的缩进和它猜的叠在一起）比不猜高得多。
//! - 「这段 Rust 排版成什么样」**有确定答案**，而且是别人已经写好、
//!   被无数项目验证过的答案。
//!
//! 所以这个文件里**不写一行排版逻辑**。它只做三件事：挑提供者、
//! 把文本喂进去、把结果拿出来。
//!
//! ## 协议只有一种：stdin 进、stdout 出
//!
//! ⚠️ **绝不让它去读磁盘上的那个文件**。这不是洁癖：
//!
//! - 缓冲区里可能有**没保存的改动**。让程序去读盘 = 它排版的是**旧内容**，
//!   我们再拿结果往回一换，用户刚敲的东西就**一声不响地没了**。
//! - 格式化不该产生「用户没要求的写盘」。
//!
//! ## ⚠️ `main.rs` 里不许出现 `if 语言 == "rust" { 跑 cargo fmt }`
//!
//! 这是**这个模块存在的唯一理由**。现在只有一个提供者，但就算如此也得走这道门：
//! 将来能不能接第二个，只由这一个决定决定。
//!
//! （VS Code 的做法也一样 —— 它自己的内置扩展走跟第三方**完全同一套**机制。
//! 那才是「可扩展」的实质，不是多建一个文件夹。）

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Instant;

/// 一个格式化提供者：**给它一段文本，它还一段文本**。
///
/// ## 「等第二个出现，共同点自己会浮出来」—— 它真的发生了
///
/// 只有一个提供者的时候，这里是一张**纯静态**的表：程序名 + 一组固定参数 +
/// 一个 `wants_edition` 开关。加第二个（clang-format）的时候它当场变形：
///
/// - rustfmt 要的是 `--edition <版本>`（从 `Cargo.toml` 里找）
/// - clang-format 要的是 `-assume-filename=<真路径>`（从文档路径来）
///
/// 于是「一组固定参数 + 几个开关」变成了**「每个工具自己从上下文算 argv」**。
/// 这不是提前设计出来的，是被第二个实例逼出来的 ——
/// 也正因为如此，它比当初硬猜一个 trait 要准。
pub struct Provider {
    /// 显示给用户的名字（状态栏）
    name: &'static str,
    /// 程序名（走 PATH 查，跟 `:lsp` 那套一样）
    program: &'static str,
    /// 认哪些后缀（**小写、不带点**）
    extensions: &'static [&'static str],
    /// 从上下文算出**完整的** argv。
    ///
    /// 为什么必须是函数而不是「固定参数 + 几个布尔开关」：见上面那段。
    argv: fn(&Context) -> Vec<String>,
}

impl Provider {
    /// 状态栏上怎么称呼它
    pub fn name(&self) -> &'static str {
        self.name
    }
}

/// **唯一**的提供者名单。
///
/// 加一个提供者 = 往这张表加一条 + 写一个 `xxx_argv`。除此之外
/// **没有任何别的地方**要知道「有哪些工具、认哪些后缀、吃什么参数」。
pub const PROVIDERS: &[Provider] = &[
    Provider {
        name: "rustfmt",
        program: "rustfmt",
        extensions: &["rs"],
        argv: rustfmt_argv,
    },
    Provider {
        name: "clang-format",
        program: "clang-format",
        // 跟配置里 `[lsp.c]` + `[lsp.cpp]` 认的那批后缀**对齐**：
        // 语言服务器认的文件，格式化器也该认 —— 两处不一致只会让人困惑
        // （「为什么这里有诊断却没有格式化」）。
        extensions: &["c", "h", "cpp", "cc", "cxx", "hpp", "hxx", "hh"],
        argv: clang_format_argv,
    },
];

/// rustfmt 的 argv。
fn rustfmt_argv(context: &Context) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--emit".to_string(),
        "stdout".to_string(),
        // ⚠️ 这里**没有**要求换行的参数（曾经有过一个 `--config newline_style=Unix`）——
        //    现在换行统一在 [`run`] 的**出口**归一化，理由写在那儿。
        //    按工具各配一个参数的做法已经错过一次：rustfmt 要它、clang-format 看起来
        //    不要它（只量了 LF 输入），而后者其实**也要**。
    ];

    // ⚠️ `--edition` 不是可选项。rustfmt 不指定 edition 就按 **2015** 解析，
    //    而 `if let Some(x) = y && cond`（let-chain）是 **2024** 的语法 ——
    //    这个仓库自己在用，所以不传的话 `:fmt` 会在**它自己的源码上**报语法错。
    //    实测：`error: let chains are only allowed in Rust 2024 or later`。
    if let Some(edition) = context.dir.as_deref().map(Path::new).and_then(edition_of) {
        args.push("--edition".to_string());
        args.push(edition);
    }
    args
}

/// clang-format 的 argv。
fn clang_format_argv(context: &Context) -> Vec<String> {
    let mut args: Vec<String> = vec![
        // ⚠️ 不给 `-style=file` 的话，clang-format 用的是**它内置的 LLVM style**，
        //    项目里的 `.clang-format` 一个字都不看。实测（cwd 在项目外）：
        //    同一份输入 → 2 空格；补上 `-style=file` → 4 空格（`.clang-format` 里写的）。
        //
        //    这很可能就是「clang-format 好像没格式化」的头号原因：它拿一套
        //    你并不认同的默认值把你的代码排成了**另一个样子**，而那些本来就
        //    接近 LLVM style 的代码，输出会和输入**一模一样**。
        "-style=file".to_string(),
    ];

    // ⚠️ `-assume-filename` 同样是必须的：走 stdin 就把**文件名**这个信息抹掉了，
    //    而 clang-format 靠后缀判断语言（`.c` 还是 `.cpp`），
    //    **也靠它去找项目里的 `.clang-format`**。
    //
    //    实测：只给 `-style=file`、cwd 在项目外 → 仍然 2 空格；
    //    补上 `-assume-filename=<真路径>`（cwd 还在项目外）→ 4 空格。
    //    也就是说这一个参数就顶替了 cwd，比「靠 cwd 碰运气」稳得多。
    //
    //    它跟 rustfmt 要 `--edition` 是**同一个道理**：
    //    **喂 stdin 就要把被抹掉的上下文还给它**，否则它只能猜。
    if let Some(path) = context.path.as_deref() {
        args.push(format!("-assume-filename={path}"));
    }

    // 这里也**没有**强制换行的参数，跟 rustfmt 那边一样 —— 换行统一在**出口**
    // 归一化（见函数末尾）。
    //
    // ⚠️ 我一开始在这里写了「clang-format 跟着输入推导，我们缓冲区本来就是 LF，
    //    所以不用管」—— **那条结论是错的**。只量了 LF 输入就下了结论，
    //    而实测 CRLF 进去会**原样 CRLF 出来**。这就是后来把归一化挪到出口的原因：
    //    **两个工具在这件事上行为不同**，靠「每个工具各配一个参数」就得各量一次，
    //    而它们的行为还可能随版本改。
    args
}

/// 按文件后缀挑提供者；挑不到返回 `None`（调用方负责说人话）。
///
/// `path` 是 `Option`，因为**新文件还没有名字** —— 那种情况下没有后缀可看，
/// 也就不该瞎猜一个格式化器出来。
pub fn provider_for(path: Option<&str>) -> Option<&'static Provider> {
    let extension = Path::new(path?).extension()?.to_str()?.to_lowercase();
    PROVIDERS
        .iter()
        .find(|provider| provider.extensions.contains(&extension.as_str()))
}

/// 跑一次格式化需要的上下文。
///
/// 它存在的理由就是上面那两条 ⚠️：**喂 stdin 就必须把被抹掉的上下文还回去**。
/// 而不同的工具要的是**不同的那块** —— rustfmt 要目录（去找 `Cargo.toml`），
/// clang-format 要路径（去认语言和 `.clang-format`）—— 所以两块都带上。
pub struct Context {
    /// 当前文档所在的目录。
    ///
    /// 两个用途：子进程的 **cwd**，以及往上找 `Cargo.toml` 的**起点**。
    pub dir: Option<String>,
    /// 当前文档的**完整路径**。
    ///
    /// clang-format 的 `-assume-filename` 要它。没有名字的缓冲区（新文件）
    /// 这里是 `None` —— 不过那种情况本来也挑不出提供者（没有后缀可看）。
    pub path: Option<String>,
}

/// 跑一次格式化。**同步、阻塞** —— 调用方负责把它丢到后台线程上。
///
/// 成功时返回**整份新文本**（不是 diff）：提供者给的就是一整份，
/// 硬要拆成编辑区间就得做 diff，而那是另一件事（而且做错了会改坏文件）。
pub fn run(provider: &Provider, text: &str, context: &Context) -> Result<String, String> {
    let args = (provider.argv)(context);

    let mut command = Command::new(provider.program);
    command.args(&args);
    if let Some(dir) = context.dir.as_deref() {
        // cwd 不只是「在哪儿跑」：有些工具靠它找到自己的配置文件。
        // ⚠️ 但它**不该是唯一**的线索 —— clang-format 就实测过「cwd 在项目外
        //    就认不到 `.clang-format`」，所以那个信息是走 `-assume-filename` 给的。
        command.current_dir(dir);
    }
    // ⚠️ 三根管子**一根都不许继承终端**：它要是往我们正用着的备用屏上写字，
    //    画面当场花掉。这条教训在 `check.rs` 顶上已经写过一次了。
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|err| format!("cannot run {}: {err}", provider.program))?;

    // 把正文喂进去，然后**把 stdin 关掉**（离开作用域即 drop）——
    // 这类工具都是「读完整个输入才开始干活」，不关它就一直等 EOF。
    //
    // ⚠️ 顺序是「先写完，再读输出」。对现在这两个工具都安全 ——
    //    **两个都实测过**：都是读完整个输入才开始写输出。
    //    但这不是通用的：换成「边读边写」的工具，只要它先吐的东西超过管道缓冲区，
    //    我们卡在写、它卡在写，两边**互等到死**。
    //
    //    真要接第三个时，**先确认它是「读完才写」**；不是的话就得把写 stdin
    //    挪到另一个线程上去（这样读和写就能同时进行）。今天不加那个线程，
    //    因为没有哪个已知工具需要它 —— 为一个测不了的场景写代码是白搭。
    {
        let mut stdin = child.stdin.take().expect("stdin was piped");
        stdin
            .write_all(text.as_bytes())
            .map_err(|err| format!("cannot send the text to {}: {err}", provider.program))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|err| format!("{} did not finish: {err}", provider.program))?;

    if !output.status.success() {
        return Err(first_line(&String::from_utf8_lossy(&output.stderr)));
    }

    let formatted = String::from_utf8(output.stdout)
        .map_err(|_| format!("{} returned text that is not UTF-8", provider.program))?;

    // ⚠️ **出口一律归一化成 LF** —— 这是 [`run`] 契约的一部分，
    //    不是哪个提供者的私事。
    //
    //    因为我们的缓冲区**只认 `\n`**（ropey 关掉了 `unicode_lines` / `cr_lines`）。
    //    一份 CRLF 进来，缓冲区里会变成「若干行、每行尾多一个 `\r`」——
    //    而那个 `\r` 是个真字符：它参与列号计算（虽然 `char_cells` 算它 0 列），
    //    也会在存盘时原样写出去。在这里一次性归一化，**所有**提供者
    //    （现在两个，将来更多）就都不可能把 `\r` 塞进缓冲区了。
    //
    //    ⚠️ 两个工具在这件事上**行为不一样**，这正是必须由我们兜住的原因：
    //
    //    - `rustfmt`：`newline_style` 默认 `Auto` 是「跟着输入走」，而输入里
    //      **一个换行都没有**时它探测不出来，退回 Windows 原生 CRLF。
    //    - `clang-format`：它**真的**跟着输入走 —— CRLF 进 → CRLF 出。
    //      （一开始只量了 LF 输入就以为它「不用管」，那是错的。）
    //
    //    靠「给每个工具加一个参数」来堵，就得**每个工具各量一次、各配一次**，
    //    而且它们的行为还会随版本改。在出口兜住只需要对一次。
    //
    //    ⚠️ 副作用要说清：**`:fmt` 会顺手把 CRLF 文件的整份行尾改成 LF。**
    //    在这个强制 LF 的仓库里那是**修正**而不是破坏（`.gitattributes` 就是
    //    `eol=lf`）；而我们本来也忠实地表示不了 CRLF 文件。
    //
    //    只归一 `\r\n`，不动孤立的 `\r` —— 那是另一个东西（老 Mac 的行尾，
    //    或者正文里真有一个回车字符），擅自改它反而更像自作主张。
    //
    // ⚠️ **这里还欠一条守卫**（2026-09-20 定：先不做，留个标记）——
    //    「扫一遍 `src/**/*.rs` 看有没有 `\r`」。
    //
    //    起因：新写的 `background.rs` 整个是 CRLF，而**三样东西都看不出来**：
    //    `cargo fmt --check` 抓不到（rustfmt 检查时不管行尾）、`git status`
    //    看不出来（`.gitattributes` 是 `eol=lf`，提交那一刻才归一化）、
    //    屏幕上也没区别 —— 只有真跑一次 `:fmt`，它才会报「有改动」露出来。
    //
    //    真要加的话加在 `tests/` 里就成：一条扫全仓库的测试，读字节看有没有 `13`。
    Ok(formatted.replace("\r\n", "\n"))
}

/// 工具报错时，只要它说的**第一句**。
///
/// 后面跟着的源码片段和箭头（`--> <stdin>:1:19`、一行代码、一个 `^`）
/// 塞不进一行状态栏，硬塞就是把有用的那半挤掉。
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("failed")
        .to_string()
}

/// 从 `dir` **往上**找 `Cargo.toml`，读出 `[package] edition`。
///
/// 为什么是往上找而不是只看 `dir`：我们手上的目录常常是 `src/`，
/// 而 `Cargo.toml` 在上一层。
pub fn edition_of(dir: &Path) -> Option<String> {
    let mut current = Some(dir);
    while let Some(dir) = current {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file()
            && let Ok(text) = std::fs::read_to_string(&manifest)
            // ⚠️ 找到了但**没写** edition 时要继续往上找，不能就此放弃：
            //    工作区根目录的 `Cargo.toml` 只有 `[workspace]` 没有 `[package]`，
            //    而从成员目录起步时先撞上的正是它。
            && let Some(edition) = edition_in(&text)
        {
            return Some(edition);
        }
        current = dir.parent();
    }
    None
}

/// **纯函数**：从 manifest 的正文里取 `[package] edition`。
///
/// 单独拆出来是为了能测 —— 上面那个要真文件系统，测起来是另一件事。
/// 这个文件里**有判断**的就是它，所以值得单独拎出来。
fn edition_in(text: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Manifest {
        package: Option<Package>,
    }
    #[derive(serde::Deserialize)]
    struct Package {
        edition: Option<String>,
    }

    // 解析不了（不是 TOML、或者 `edition` 写成了数字）就当没有 ——
    // 那只会退化成「不传 --edition」，不该让 `:fmt` 整个失败
    toml::from_str::<Manifest>(text).ok()?.package?.edition
}

/// 一次格式化的结果。
///
/// 拆成字段而不是让线程直接拼一句话，是为了让 [`Report::describe`] 能被单独测试
/// —— 跟 `CheckReport` 一个路数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// 提供者的名字（状态栏要说清「是谁干的」）
    pub name: &'static str,
    /// 成功时是**排好版的整份新文本**；失败时是那句人话
    pub outcome: Result<String, String>,
    /// 结果跟送进去的那份是不是**逐字相同**
    ///
    /// ⚠️ 在这里比，不让调用方比：调用方手上那份缓冲区可能已经变了
    /// （用户在你排版的那两秒里又敲了几个字），那时候比出来的是错的答案。
    pub changed: bool,
    /// 一共花了多少毫秒
    pub elapsed_ms: u128,
}

impl Report {
    /// 给状态栏看的那一句话。
    pub fn describe(&self) -> String {
        let seconds = self.elapsed_ms as f64 / 1000.0;
        match &self.outcome {
            Err(message) => format!("Format: FAILED  —  {message}"),
            // 「排版过但一个字没动」值得单独说 —— 否则用户会怀疑是不是没跑
            Ok(_) if !self.changed => format!("Format: already clean  ({seconds:.1}s)"),
            Ok(_) => format!("Format: OK  —  {}  ({seconds:.1}s)", self.name),
        }
    }
}

/// 在后台线程上跑一次格式化，结果通过通道送回来。
///
/// **没有 `Result`**：起线程不成会 panic，不会返回错误 —— 这点跟
/// `check::spawn` 不一样（那个要 spawn 子进程，会失败）。子进程起不来
/// 是**任务内部**的失败，走 [`Report::outcome`] 回来，不该在这里抛。
pub fn spawn(provider: &'static Provider, text: String, context: Context) -> Receiver<Report> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let started = Instant::now();
        let outcome = run(provider, &text, &context);
        let changed = matches!(&outcome, Ok(formatted) if *formatted != text);

        // 主循环要是已经退出了（比如你在排版这两秒里按了 `:q`），send 会失败 ——
        // 那不是错误，线程平静结束就好。
        let _ = tx.send(Report {
            name: provider.name(),
            outcome,
            changed,
            elapsed_ms: started.elapsed().as_millis(),
        });
    });

    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- 挑提供者 ----------

    #[test]
    fn the_extension_picks_the_provider() {
        let provider = provider_for(Some("src/main.rs")).expect(".rs 该有提供者");
        assert_eq!(provider.name(), "rustfmt");
    }

    #[test]
    fn the_extension_is_matched_without_caring_about_case() {
        // 大写后缀在 Windows 上很常见（`MAIN.RS`），而用户不该因为
        // 文件名大小写就白敲一次 `:fmt`
        let provider = provider_for(Some("D:\\proj\\MAIN.RS")).expect(".RS 也该认");
        assert_eq!(provider.name(), "rustfmt");
    }

    #[test]
    fn a_format_we_do_not_know_gets_nobody() {
        // ⚠️ 这条是**故意的**：挑不到就老老实实说「没有」，
        //    而不是随便抓一个格式化器往不认识的文本上套
        assert!(provider_for(Some("notes.txt")).is_none());
        assert!(provider_for(Some("Makefile")).is_none());
        assert!(provider_for(None).is_none(), "新文件还没有名字，不该瞎猜");
    }

    #[test]
    fn only_the_last_dot_counts_as_the_extension() {
        // `a.rs.bak` 的后缀是 `bak` 不是 `rs` —— `Path::extension` 天然如此，
        // 这条钉住它，免得哪天手写成「看包含哪个后缀」
        assert!(provider_for(Some("a.rs.bak")).is_none());
    }

    // ---------- 找 edition ----------

    #[test]
    fn the_edition_is_read_out_of_the_manifest() {
        let manifest = "[package]\nname = \"x\"\nedition = \"2024\"\n";
        assert_eq!(edition_in(manifest).as_deref(), Some("2024"));
    }

    #[test]
    fn a_manifest_without_an_edition_gives_nothing() {
        // 老 crate 不写 edition（= 2015）。这里是 None，上层于是**不传**
        // `--edition`，让 rustfmt 用自己的默认 —— 而不是我们替它编一个
        assert_eq!(edition_in("[package]\nname = \"x\"\n"), None);
        // 工作区根的 manifest 只有 `[workspace]`，连 `[package]` 都没有
        assert_eq!(edition_in("[workspace]\nmembers = [\"a\"]\n"), None);
    }

    #[test]
    fn a_broken_manifest_does_not_blow_up() {
        // 解析不了就当作没有 —— 那只会退化成「不传 --edition」，
        // 不该让 `:fmt` 整个失败
        assert_eq!(edition_in("this is not toml {{{"), None);
    }

    #[test]
    fn the_walk_up_finds_the_manifest_from_a_subdirectory() {
        // 起点故意用 `src/` —— 真实调用就是这个形状（打开的文档在 `src/` 里），
        // 而 `Cargo.toml` 在**上一层**。
        //
        // 期望值从 manifest 正文现算，不写死版本号：这一条盯的是「往上找」
        // 那一段，不是「这个项目的 edition 是几」。
        let manifest = std::fs::read_to_string("Cargo.toml").expect("测试的 cwd 就是 crate 根");
        let expected = edition_in(&manifest);
        assert!(expected.is_some(), "本仓库的 Cargo.toml 里写着 edition");
        assert_eq!(edition_of(Path::new("src")), expected);
    }

    // ---------- 报错只取一句 ----------

    #[test]
    fn only_the_first_line_of_a_complaint_is_kept() {
        // rustfmt 报错的真实形状：第一句有用，后面全是给人看的源码片段
        let stderr = "\nerror: expected expression, found `;`\n --> <stdin>:1:19\n  |\n1 | fn x() { let a = ; }\n  |                   ^\n";
        assert_eq!(first_line(stderr), "error: expected expression, found `;`");
    }

    #[test]
    fn silence_still_gives_something_to_show() {
        assert_eq!(first_line(""), "failed");
        assert_eq!(first_line("\n  \n"), "failed");
    }

    // ---------- 状态栏那句话 ----------

    #[test]
    fn the_report_says_which_of_the_three_things_happened() {
        let failed = Report {
            name: "rustfmt",
            outcome: Err("error: expected expression".to_string()),
            changed: false,
            elapsed_ms: 12,
        };
        assert_eq!(
            failed.describe(),
            "Format: FAILED  —  error: expected expression"
        );

        // ⚠️ 「跑过但一个字没动」必须和「改了」分开说：
        //    混成一句「OK」的话，文件本来就没问题、和 `:fmt` 悄悄什么也没干，
        //    在用户眼里是**同一个样子**
        let clean = Report {
            name: "rustfmt",
            outcome: Ok("fn a() {}\n".to_string()),
            changed: false,
            elapsed_ms: 120,
        };
        assert_eq!(clean.describe(), "Format: already clean  (0.1s)");

        let changed = Report {
            name: "rustfmt",
            outcome: Ok("fn a() {\n}\n".to_string()),
            changed: true,
            elapsed_ms: 120,
        };
        assert_eq!(changed.describe(), "Format: OK  —  rustfmt  (0.1s)");
    }

    // ---------- 跟真工具的契约 ----------

    /// rustfmt 的 argv 里那行 `--edition` **必须**在。
    ///
    /// ⚠️ 看着像「测常量」，其实它拦的是一个**实测抓到的真 bug**：
    /// 少了 `--edition`，rustfmt 就按 **2015** 解析，而 let-chain
    /// （`if let Some(x) = y && cond`）是 2024 的语法 —— 这个仓库自己在用，
    /// 于是 `:fmt` 在**它自己的源码上**报语法错。
    ///
    /// （换行那条**不再**是对着某个参数测的：现在统一在 `run` 的出口归一化，
    /// 由 `no_provider_ever_hands_back_carriage_returns` 盯着。）
    #[test]
    fn rustfmt_has_to_be_told_the_things_stdin_takes_away() {
        let provider = provider_for(Some("x.rs")).expect("rustfmt");
        // dir 用 `src` —— 真实调用就是这个形状（文档在 src/ 里、Cargo.toml 在上一层），
        // 于是 `--edition` 那条才真的测得到
        let context = Context {
            dir: Some("src".to_string()),
            path: Some("src/x.rs".to_string()),
        };

        let joined = (provider.argv)(&context).join(" ");

        assert!(joined.contains("--edition"), "{joined}");
    }

    /// clang-format 也有两条「喂了 stdin 就得还回去」的参数。
    ///
    /// ⚠️ 两条都是**实测**出来的，别删：
    ///
    /// - 少了 `-style=file`：它用**内置的 LLVM style**，项目里的 `.clang-format`
    ///   一个字都不看。实测同一份输入 → 2 空格 vs 4 空格。
    ///   这大概就是「clang-format 好像没格式化」的头号原因。
    /// - 少了 `-assume-filename`：**认不到项目里的 `.clang-format`**
    ///   （实测：只给 `-style=file`、cwd 在项目外 → 仍然 2 空格），
    ///   而且它也是靠后缀判断 `.c` / `.cpp` 的。
    #[test]
    fn clang_format_has_to_be_told_the_things_stdin_takes_away() {
        let provider = provider_for(Some("D:\\proj\\x.cpp")).expect("clang-format");
        let context = Context {
            dir: Some("D:\\proj".to_string()),
            path: Some("D:\\proj\\x.cpp".to_string()),
        };

        let joined = (provider.argv)(&context).join(" ");

        assert!(joined.contains("-style=file"), "{joined}");
        assert!(
            joined.contains("-assume-filename=D:\\proj\\x.cpp"),
            "得把文件名还给它，否则 `.clang-format` 认不到：{joined}"
        );
    }

    #[test]
    fn the_c_family_goes_to_clang_format_and_rust_goes_to_rustfmt() {
        for path in ["a.c", "a.h", "a.cpp", "a.CC", "a.hpp", "a.hh"] {
            let provider = provider_for(Some(path)).unwrap_or_else(|| panic!("{path} 该有人管"));
            assert_eq!(provider.name(), "clang-format", "{path}");
        }
        assert_eq!(
            provider_for(Some("a.RS")).expect("rustfmt").name(),
            "rustfmt"
        );

        // 两批后缀**不许重叠**。重叠了的话命中谁就全看表的顺序 ——
        // 「改一行顺序就换了个工具」是最难查的那种 bug，
        // 而且它一句话都不会报错。
        let mut seen = std::collections::HashSet::new();
        for provider in PROVIDERS {
            for extension in provider.extensions {
                assert!(
                    seen.insert(*extension),
                    "后缀 `{extension}` 被两个提供者同时认领了"
                );
            }
        }
    }

    /// 工具在不在 PATH 上。不在就让下面几条**平静跳过** ——
    /// 它们测的是「跟真工具的契约」，装不上工具的环境不该因此变红。
    ///
    /// ⚠️ 这是**有意的静默跳过**。同一个仓库里真语言服务器那几条用的是 `#[ignore]`
    /// （要人手动开），这里能自己判断就顺手判断了。代价是「跳过了你也不会知道」，
    /// 所以 CI 上真跑一次仍然有价值。
    fn available(program: &str) -> bool {
        Command::new(program).arg("--version").output().is_ok()
    }

    /// 给这些测试拼一个上下文：目录和路径都给（两个工具各要一块）。
    ///
    /// ⚠️ 目录要处理成「没有」而**不是空串**：`Path::new("x.c").parent()` 给的是
    /// `Some("")`。而空串当目录传给子进程会**当场起不来**
    /// （Windows：os error 123）。
    ///
    /// 这真的不是测试的小题大做 —— 它跟 `App::current_directory` 里那段注释
    /// 说的是同一个 bug，那个是 `:fmt` 第一版炸出来的。
    fn context_for(path: &str) -> Context {
        Context {
            dir: std::path::Path::new(path)
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(|parent| parent.to_string_lossy().into_owned()),
            path: Some(path.to_string()),
        }
    }

    /// **每个**提供者、每种输入，都不许把 `\r` 还回来。
    ///
    /// ⚠️ 两种载荷都要测，因为它们触发的是**两个不同**的问题：
    ///
    /// 1. **一个换行都没有** —— rustfmt 的 `newline_style = Auto` 探测不出来，
    ///    退回 Windows 原生 CRLF。
    /// 2. **CRLF 输入** —— 这一种是**真的撞上过**的：文件本身就是 CRLF
    ///    （Windows 上太常见 —— 写这个模块时 `src/background.rs` 自己就中了一次）。
    ///    如果不管，`:fmt` 会把它**整份行尾**换掉：用户只按了一次 `:fmt`，
    ///    在 git 里却是「每一行都改了」。
    ///
    /// 这条同时也钉住了我们这半边的前提：**缓冲区只认 `\n`**
    /// （ropey 关掉了 `unicode_lines` / `cr_lines`），所以提供者交回来的东西
    /// 也必须是纯 LF —— 不然它和我们的模型对不上。
    #[test]
    fn no_provider_ever_hands_back_carriage_returns() {
        let cases = [
            ("x.rs", "fn main(){}"),
            ("x.rs", "fn main() {\r\n}\r\n"),
            ("x.c", "int main(){return 0;}"),
            ("x.c", "int main()\r\n{\r\n    return 0;\r\n}\r\n"),
        ];

        for (path, payload) in cases {
            let provider = provider_for(Some(path)).expect("这个后缀该有提供者");
            if !available(provider.program) {
                continue;
            }

            let formatted = run(provider, payload, &context_for(path))
                .unwrap_or_else(|err| panic!("{path}: {err}"));

            assert!(
                !formatted.contains('\r'),
                "`{}` 还回了 CR —— 存盘会让整个文件在 git 里显示成全改过：{formatted:?}",
                provider.name()
            );
        }
    }

    /// **clang-format 得认出项目自己的 `.clang-format`**。
    ///
    /// ⚠️ 这条测的是**结果**，不是「参数在不在」。上面那条已经把参数钉住了，
    /// 但「参数在」不等于「行为对」—— 万一哪天 clang-format 改了找配置的规则，
    /// 只有这条会红。
    #[test]
    fn a_real_clang_format_finds_the_projects_own_style_file() {
        if !available("clang-format") {
            return;
        }
        let dir = std::env::temp_dir().join("stbd-clang-format-probe");
        std::fs::create_dir_all(&dir).expect("造临时目录");
        std::fs::write(
            dir.join(".clang-format"),
            "{BasedOnStyle: LLVM, IndentWidth: 4}\n",
        )
        .expect("写 .clang-format");
        let file = dir.join("x.c");
        std::fs::write(&file, "int main(){if(1){return 0;}}\n").expect("写源文件");
        let full = file.to_string_lossy().into_owned();

        let provider = provider_for(Some(&full)).expect("clang-format");
        // ⚠️ cwd 故意指到**上一层**：证明「认哪个 `.clang-format`」靠的是
        //    `-assume-filename` 而不是 cwd。实测过：只给 `-style=file`
        //    而 cwd 在项目外时，它照样按 LLVM 排成 2 空格。
        let context = Context {
            dir: Some(std::env::temp_dir().to_string_lossy().into_owned()),
            path: Some(full),
        };

        let formatted =
            run(provider, "int main(){if(1){return 0;}}\n", &context).expect("clang-format 该成功");

        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            formatted.contains("\n    if (1)"),
            "没认项目里的 .clang-format（拿到的是 2 空格？）：{formatted:?}"
        );
    }

    /// rustfmt 报错时只取**第一句** —— 后面全是给人看的源码片段，
    /// 塞进一行状态栏就是把有用的那半挤掉。
    #[test]
    fn a_real_rustfmt_complaint_comes_back_as_exactly_one_line() {
        if !available("rustfmt") {
            return;
        }
        let provider = provider_for(Some("x.rs")).expect("rustfmt");

        let message = run(provider, "fn main(){let x = ;}", &context_for("x.rs")).unwrap_err();

        assert!(message.starts_with("error:"), "{message}");
        assert!(
            !message.contains('\n'),
            "状态栏只有一行，多行的话后面全被截掉：{message:?}"
        );
    }

    /// **clang-format 对「编译不过的代码」不会失败** —— 它照样排。
    ///
    /// ⚠️ 这条是**行为契约**，不是巧合：实测 `int main(){int x = ;}` 进去，
    /// 它 exit 0、照样把空格补齐（`int main() { int x = ; }`）。
    ///
    /// 两个后果要记住：
    /// 1. C/C++ 文件上 `Format: FAILED` 会**很罕见** —— 那是正常的，不是坏了；
    /// 2. 反过来，**编译不过的代码照样会被排版**，别指望它是语法检查器。
    ///
    /// 钉住它是因为「哪天它开始报错了」是一个值得有人知道的行为变化。
    #[test]
    fn clang_format_formats_even_what_does_not_compile() {
        if !available("clang-format") {
            return;
        }
        let provider = provider_for(Some("x.c")).expect("clang-format");

        let formatted = run(provider, "int main(){int x = ;}", &context_for("x.c"))
            .expect("clang-format 不该在语法错误上失败");

        assert!(formatted.contains("int x = ;"), "{formatted:?}");
    }
}
