//! 程序入口 —— main.rs 的职责
//!
//! 串起所有模块：
//! 1. 解析命令行参数（可选的待打开文件路径）
//! 2. 加载用户配置（config.rs；失败也不阻止启动）
//! 3. 进入 raw mode + 备用屏（Alternate Screen）
//! 4. 把语言服务器接上（lsp/；起不来也不阻止启动）
//! 5. 主循环：画(ui) → 读事件(event) → 分发(update) → 执行副作用(Action)
//! 6. 无论如何退出都恢复终端
//!
//! ## 为什么这儿的每个自选功能都「坏了也不拦」
//!
//! 配置读不到、`cargo check` 起不来、`rust-analyzer` 找不到 —— 全都只写一句状态栏。
//! 因为它们的共同点是：**它们都不是编辑器**。编辑器就是那个把文件和按键连起来的
//! 东西，它必须永远能用。把「锦上添花」和「基本盘」的失败分开处理，
//! 是 `run_action` 不抛错误、`start_check` 不抛错误、`start_lsp` 也不抛错误的同一个理由。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use stbd::app::{App, DocumentKind};
use stbd::commands::Action;
use stbd::lsp;
use stbd::lsp::pool::Pool;
use stbd::lsp::session::{Outcome, Session};
use stbd::outbox::{OutFile, Outbox};
use stbd::tools::{check, formatter};
use stbd::update;
use stbd::{background, config, event, file_io, ui};

/// 我们用到的终端后端类型（Crossterm 输出到 stdout）
type Backend = CrosstermBackend<io::Stdout>;
type Term = Terminal<Backend>;

fn main() -> io::Result<()> {
    // 1. 读取命令行传入的文件（没传就开一个新文件）
    let (file_path, content) = load_file_from_args();

    // 1.5 把输出文件夹清一遍。
    //
    // **进入时清一次就够了**（退出时不清，理由见 `main` 末尾）。
    // 这一次清掉的是上一次留下的东西 —— 不管上次是正常退出还是崩掉的：
    // 上一次的内容看起来跟这一次的一模一样，你会拿着上次的列表当这次的用。
    Outbox::locate().clean();

    // 2. 读用户配置：找不到文件就用默认值；文件写错了也不致命，只记下提示
    //    （配置文件是锦上添花的东西，绝不该让编辑器打不开）
    let loaded = config::Config::load();

    // 3. 初始化终端：进入 raw mode + 备用屏
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // EnableBracketedPaste：告知终端“粘贴时请把内容用标记包起来”，
    // 这样粘贴会作为**一个** Event::Paste 到达，而不是被拆成一堆按键。
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        SetCursorStyle::SteadyBlock
    )?;
    let mut terminal = Term::new(CrosstermBackend::new(stdout))?;

    // 4. 初始状态：带上用户配置
    let mut app = App::with_config(file_path, content, loaded.config);
    app.set_config_source(loaded.source);
    // 记住启动时打开的那个文档，这样 `q` 才知道有没有「上一级」可回。
    // 路径要先定死成完整路径（理由见 `file_io::full_path`）。
    if let Some(path) = app.file_path.clone() {
        let full = file_io::full_path(&path);
        app.kind = document_kind(&full);
        app.file_path = Some(full.clone());
        app.documents.remember(&full);
    }
    if let Some(warning) = loaded.warning {
        // 配置写错必须明确提示，否则「改了设置却不生效」会变成玄学问题
        app.set_status_message(format!("Config: {warning}"));
    }

    // 5. 主循环；结束后把终端还给用户
    let result = run_event_loop(&mut terminal, &mut app);
    release_terminal()?;

    // ⚠️ 退出时**不**清输出文件夹。
    //
    // 这里曾经也清一次，理由是「进入时那次不够，上次可能是崩掉的」——
    // 但仔细读那句话会发现它在说**进入**那次有必要，没说退出那次有什么用。
    // 而退出那次的代价是实测出来的：`:errors` 之后一 `:q`，`error_log.txt`
    // 就空了 —— 于是「把清单写成文件」这件事只剩下「编辑器开着时另一个窗口
    // 去读它」，退出之后就拿不到了。
    //
    // 进入时那次已经足够：不管上次是正常退出还是崩掉，你打开时看到的
    // 永远是干净的。下次打开时它自然会被清掉。
    result
}

/// 把终端还原成**普通终端**：程序退出时和「让位」之前都用它。
///
/// 「编辑器的终端长什么样」和「普通终端长什么样」各只有一处定义 ——
/// 让位 / 收回 / 退出三条路共用同一份，不会走歪。
fn release_terminal() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        SetCursorStyle::DefaultUserShape
    )
}

/// 主循环一轮最多睡这么久。
///
/// **这个数字就是「后台任务说的话最多晚多少被看见」。** 50ms = 20 次/秒 ——
/// 对人眼无感，对 CPU 也基本无感，同时远低于能察觉到的延迟阈值。
///
/// 为什么必须有它：主循环只有一个线程，而「终端事件」和「后台消息」是两个
/// **都会阻塞**的源（键盘等控制台句柄，通道等发送方）。一个线程一次只能在一个
/// 地方睡着，所以只能「睡一小会儿，醒了两个都看一眼」。
/// 完整的推演见 `COMMANDS.md` 里没有的那一段 —— 它只存在于这里：
///
/// - 等键盘 → 后台的活全堆着，你按下一个键才被看见
/// - 等后台 → 服务器/任务不说话的那几秒，按键全不响应（最坏的那种 bug：
///   间歇、依赖负载、难复现）
///
/// 所以答案不是「选一边」，而是「哪边都不睡死」。
const TICK: Duration = Duration::from_millis(50);

/// 主循环：**等**（终端事件或超时）→ 分发 → 收**后台消息** → 有变化才重画。
fn run_event_loop(terminal: &mut Term, app: &mut App) -> io::Result<()> {
    // 正在跑的后台检查。放在主循环的局部变量里而不是 `App` 里 ——
    // 理由见 `App::checking` 的注释：App 只记「有没有」，不持有通道。
    let mut check: Option<background::Worker<check::CheckReport>> = None;

    // 正在跑的格式化。
    //
    // 跟 `check` **一模一样**的立场：通道是「活着的东西」，所以住在主循环里；
    // `App` 只记「有没有」（`app.formatting`）。理由见 `App::checking` 的注释。
    let mut format: Option<background::Worker<formatter::Report>> = None;

    // 语言服务器。跟 `check` 完全同一个立场：它是**活的东西**（一个进程、
    // 两根管道、两个线程），所以住在主循环里，不进 `App`。
    // `App` 只拿它的**产出**（`app.diagnostics`），那才是纯数据。
    //
    // ⚠️ 池子一开始是**空的**：会话用到了才起。所以你只在一个项目里干活时，
    //    上限写 1 和写 8 完全一样（都只有一个进程）。
    let mut lsp = Pool::new(app.config.lsp_max_servers);

    // 先画一帧。新循环不再「每轮开头都画」，所以启动这一帧得自己补上，
    // 否则打开编辑器会看到一片空白 —— 直到你按第一个键。
    terminal.draw(|frame| ui::render_ui(frame, app))?;

    // 开机先报一次事实。
    //
    // ⚠️ 不能省：下面那个同步是挂在「这一轮手上有动作」上的，不补这一次的话，
    //    打开一个文件、什么都不按，语言服务器就永远不会起来 ——
    //    看起来就像「它只在我开始打字之后才睡醒」。
    sync_document(&mut lsp, app);

    loop {
        let mut redraw = false;
        // 这一轮手上动了东西吗（键盘 / 鼠标 / 粘贴）。
        //
        // ⚠️ 拿它当「要不要去同步文档」的开关，是为了**别在闲着的时候干活**：
        //    比文本要先把它整份拷出来（O(文件大小)），而屏幕上的字只可能
        //    因为这几类事件改变 —— 没人按键的 50ms 里，它不可能变。
        //    没有这个开关的话，一个空转的编辑器也会每秒白白拷 20 次文件。
        let mut touched = false;

        // 最多等 TICK。返回 `None` = 键盘没动静，但**不等于没事可做**：
        // 下面照样会去看后台消息。
        if let Some(ev) = event::poll_event(TICK)? {
            redraw = true;
            match ev {
                event::Event::Key(key) => {
                    touched = true;
                    // update 只处理按键；把文本区估算尺寸传进去用于自动滚动
                    let (view_h, view_w) = compute_view_size(app);
                    // 一个按键可能产出多个动作（命令模式的 `&&` 链），按顺序执行
                    for action in update::handle_key_event(app, key, view_h, view_w) {
                        match run_action(app, action) {
                            Step::Quit => {
                                // 提前把所有服务器收掉：`shutdown` 可能要等每个最多
                                // 200ms，放在这里等于**在备用屏里等** —— 用户看到的是
                                // 「按 q 到回 shell」之间那点空白，而不是回到 shell 后
                                // 干等一个提示符。
                                //
                                // ⚠️ 写成显式 `drop` 而不是让它自然离开作用域：
                                //    这一步是**有代价**的（真的在杀进程、等它走），
                                //    值得让读者看见它在这里发生。
                                drop(lsp);
                                return Ok(());
                            }
                            // 让位：终端暂时交出去，回来之后界面还是原样
                            Step::HandOver(line) => run_external_command(terminal, app, &line),
                            Step::StartCheck => {
                                check = start_check(app).map(background::Worker::new);
                            }
                            // 格式化是**同一个形状**，所以直接套同一个收件端
                            Step::StartFormat => {
                                format = start_format(app).map(background::Worker::new);
                            }
                            // 正文在这里拼（要池子），拼完铺上去、再拄进输出文件夹 ——
                            // 和 `:ls` / `:errors` 一样：**屏幕上显示的就是拄出去的那份**
                            Step::ShowLspStatus => {
                                let text = app.config.lsp_status(&lsp.running());
                                app.show_list(DocumentKind::LspStatus, text);
                                write_outbox(app, OutFile::LspStatus);
                            }
                            Step::Continue => {}
                        }
                    }
                }
                event::Event::Mouse(mouse) => {
                    touched = true;
                    let (view_h, view_w) = compute_view_size(app);
                    update::handle_mouse_event(app, mouse, view_h, view_w);
                }
                // 尺寸变化无需特殊处理：下面那次 draw 会自己用新尺寸
                event::Event::Resize(..) => {}
                // 粘贴：bracketed paste 已把整段文本聚合成一个事件，交给 update 分发
                event::Event::Paste(text) => {
                    touched = true;
                    let (view_h, view_w) = compute_view_size(app);
                    update::handle_paste_event(app, &text, view_h, view_w);
                }
                event::Event::Ignored => {}
            }
        }

        // 屏幕上的东西可能变了 → 告诉服务器现在是什么样。
        // 要不要真的开口由会话自己判断（一个字没动就什么都不发）。
        if touched {
            sync_document(&mut lsp, app);
        }

        // 收服务器这一轮说的话（非阻塞，没话说就立刻回来）
        drain_lsp(&mut lsp, app, &mut redraw);

        // 收两个后台任务这一轮攒下的消息。
        //
        // ⚠️ 那段容易拿捏错的判断（「一口气全取走」和「`Disconnected` 不等于
        //    `Empty`」）在 `background::Worker` 里，**只有一份**，而且有测试盯着。
        //    这里只管「拿到的消息怎么处理」。
        //
        //    以前这段在主循环里被写了两遍（第二份是照着第一份抄的），
        //    而主循环测不了（要一个真终端）—— 所以那两条判断**一直没被任何测试
        //    盯过**。现在它们有了。
        //
        // ⚠️ 先 `map` 出结果再 `match`：借用在那一步就结束了，
        //    因此下面收工时可以放心把 `check` / `format` 整个丢掉。
        match check.as_mut().map(background::Worker::poll) {
            None | Some(background::Progress::Quiet) => {}
            Some(background::Progress::Reports { reports, over }) => {
                for report in &reports {
                    app.set_status_message(report.describe());
                }
                redraw = true;
                // ⚠️ 只有 `over` 才能清标记、丢通道。**半路的消息不该顺手把任务
                //    标记成结束** —— 那会让后面还会说话的任务提前失去听众，
                //    而且它是静默的（消息发过来没人接，`send` 报错但没人看）。
                if over {
                    app.checking = false;
                    check = None;
                }
            }
            Some(background::Progress::DiedBeforeSpeaking) => {
                // 必须说出来，否则状态栏永远停在「Checking…」
                app.set_status_message("Check: the worker thread died".to_string());
                app.checking = false;
                check = None;
                redraw = true;
            }
        }

        match format.as_mut().map(background::Worker::poll) {
            None | Some(background::Progress::Quiet) => {}
            Some(background::Progress::Reports { reports, over }) => {
                for report in &reports {
                    app.set_status_message(report.describe());

                    // ⚠️ `changed` 是**在后台线程上**算的（拿它手上那份原文比），
                    //    不能在这儿比：用户可能在你排版这两秒里又敲了几个字，
                    //    拿现在的缓冲区去比，比出来的是错的答案。
                    if let Ok(text) = &report.outcome
                        && report.changed
                    {
                        app.replace_all_text(text);
                        // 排版会改行数，视口可能落到内容外面 ——
                        // 那种画面看上去像「文件被清空了」，而它其实好好的
                        let (view_h, view_w) = compute_view_size(app);
                        app.clamp_viewport_to_content(view_h, view_w);
                    }
                }
                redraw = true;
                if over {
                    app.formatting = false;
                    format = None;
                }
            }
            Some(background::Progress::DiedBeforeSpeaking) => {
                app.set_status_message("Format: the worker thread died".to_string());
                app.formatting = false;
                format = None;
                redraw = true;
            }
        }

        if redraw {
            terminal.draw(|frame| ui::render_ui(frame, app))?;
        }
    }
}

/// 起一次后台检查，返回新的收件通道。
///
/// 起不来（`cargo` 不在 PATH 之类）就只写一句状态栏，**不往上抛** ——
/// 跟 `run_action` 里那批 IO 一个立场：锦上添花的功能不该让编辑器挂掉。
fn start_check(app: &mut App) -> Option<mpsc::Receiver<check::CheckReport>> {
    let dir = app.current_directory();
    match check::spawn(dir.as_deref()) {
        Ok(receiver) => Some(receiver),
        Err(err) => {
            app.set_status_message(format!("Cannot run cargo check: {err}"));
            app.checking = false;
            None
        }
    }
}

/// 起一次后台格式化，返回新的收件通道。
///
/// ⚠️ 送到提供者手上的正文是 `app.buffer.to_string()` —— **缓冲区里那份**，
/// 不是磁盘上那份。这是整个功能最重要的一条：盘上那份可能改过但没存，
/// 拿它去排版再换回来，用户刚敲的东西就没了。
///
/// （命令层已经挑过一次提供者，这里是第二道；真到了这里还没有，
/// 说明缓冲区状态变了 —— 比如你自己刚把文件另存成了 `.txt`。
/// 跟 `start_check` 一个立场：报一句，**不往上抛**。）
fn start_format(app: &mut App) -> Option<mpsc::Receiver<formatter::Report>> {
    let Some(provider) = formatter::provider_for(app.file_path.as_deref()) else {
        app.set_status_message("No formatter for this file type".to_string());
        app.formatting = false;
        return None;
    };

    let text = app.buffer.to_string();
    let context = formatter::Context {
        dir: app.current_directory(),
        // ⚠️ 路径也得给它：clang-format 靠它认语言（`.c` / `.cpp`）
        //    和找项目里的 `.clang-format`。走 stdin 就把这个名字抹掉了，
        //    所以要由我们**还回去** —— 跟 rustfmt 要 `--edition` 同一个道理。
        path: app.file_path.clone(),
    };
    Some(formatter::spawn(provider, text, context))
}

// ---------- 语言服务器 ----------

/// 拿这个文件去找它的**项目根**。
///
/// 服务器要的是根，不是当前这个文件 —— 我们打开的是 `src/main.rs`，
/// 根还在上面两层。用 `Cargo.toml` 当标记：`cargo` 自己就是靠它往上找的，
/// 我们跟它用同一套判据，才不会出现「我们的根和 cargo 的根不是同一个」
/// 这种事后极难查的怪事。
///
/// ⚠️ **每次都要重新算，不能只在启动时算一次。**
///
/// 这是这个功能里最容易做错、又最难发现的一处。第一版就是启动时算了一次，
/// 于是 `:open ..\另一个项目\src\main.rs` 之后：那个文件不属于启动时那个根，
/// 服务器对它一个字都不说（实测 45 秒 0 份推送），而屏幕上什么都不显示 ——
/// 看起来就像「这个文件恰好没问题」。
///
/// `marker` 从哪来：配置里那一节的 `root_marker`。**不写标记**（`None`）时
/// 返回文件自己所在的目录 —— 有些服务器不要求项目根（`clangd` 自己会往上找
/// `compile_commands.json`），硬要求它反而会把本来能用的项目排除掉。
///
/// 找不到标记时返回 `None`，调用方**不起服务器**。这是**正常**情况：
/// 拿它编辑一个单独的 `.rs`，凭什么要求它是个 Cargo 项目。所以不报错、不提示。
fn project_root_of(path: &str, marker: Option<&str>) -> Option<PathBuf> {
    // ⚠️ 相对路径**直接不给根**。两个反直觉的地方叠在一起：
    //
    //   1. `Path::new("x.c").parent()` 是 `Some("")`，不是 `None` ——
    //      照直用会拿着一个空路径去求 rootUri；
    //   2. 相对路径喂给 `find_upwards`，它是**按进程当前目录**去找标记的 ——
    //      于是「这个文件属于哪个项目」取决于你在哪个目录敲的
    //      `cargo run` / `cargo test`。同一个文件两次跑给出两个不同的根，
    //      而错的那个表现是「一个字都不说的服务器」，屏幕上毫无提示。
    //
    // 正常流程里进不来（`file_io::full_path_in` 在打开那一刻就钉成绝对的了），
    // 所以这是个零成本的防守 —— 而要查的毛病很难查。
    let path = Path::new(path);
    if !path.is_absolute() {
        return None;
    }

    let Some(marker) = marker else {
        // 不要求项目根 → 就用文件自己所在的目录。
        // 绝对路径的父目录一定拿得到，所以这里不会空手而归。
        return path.parent().map(Path::to_path_buf);
    };
    file_io::find_upwards(path, marker)
}

/// 报告事实：**现在屏幕上是这个文件、这些文本**。
///
/// 这里干四件事，顺序不能乱：
/// 1. 把上限从配置同步给池子（`:set` / `:config reload` 改的都算数）
/// 2. 查表：这个后缀归哪个服务器；**查不到就什么都不做**
/// 3. 算出这个文件属于哪个项目根，拿到「那个服务器的那个根」的会话
/// 4. 把「现在是什么」告诉它
///
/// 要不要真的发消息，由 [`Session::show`] 自己判断（一个字没变就什么都不发）。
fn sync_document(pool: &mut Pool, app: &mut App) {
    // 上限**每轮都同步一次**，而不是启动时读一次。
    //
    // 这样 `:set lspmaxservers 1` 和 `:config reload` 都自动生效，不需要
    // 任何一处额外的接线 —— 也就是说，不可能出现「改了设置但那条路忘了通知池子」。
    pool.set_limit(app.config.lsp_max_servers);

    // 0 = 用户把语言服务器整个关掉了。**在这里就停住**，不去打扰池子 ——
    // 池子的「上限 0」守卫是防死循环用的（见 `Pool::acquire`），
    // 不是拿来说给用户听的。
    if app.config.lsp_max_servers == 0 {
        return;
    }

    // ⚠️ **只有真文件才同步。**
    //
    // 目录列表不是源代码。更要紧的是 `:errors` 那个清单 —— 它是一份
    // **我们生成的**文本，要是也发过去，服务器会认认真真地给你报
    // 「这一堆字里有语法错误」，然后那些错又会显示在屏幕上。
    if app.kind != DocumentKind::File {
        return;
    }
    let Some(path) = app.file_path.clone() else {
        return;
    };

    // ⚠️ 查不到就**一个字节都不发**。以前这里是「找到 Cargo.toml 就丢给
    //    rust-analyzer」，于是在 Rust 项目里打开 `.py` 会让 rust-analyzer
    //    拿 Rust 语法去解析 Python，报出一堆**假的**语法错误
    //    （实测：4 条 `expected an item`）。假错误比没有诊断糟得多 ——
    //    你会去改本来没错的代码。
    //
    //    `command` 留空（= 关掉一条内置的）也会在这里变成 `None` ——
    //    那条规则住在 `server_for` 里，不在这儿。
    let Some(server) = app.config.server_for(&path) else {
        return;
    };

    let Some(root) = project_root_of(&path, server.root_marker.as_deref()) else {
        return;
    };
    let Some(uri) = lsp::uri::path_to_uri(Path::new(&path)) else {
        return;
    };
    let Some(root_uri) = lsp::uri::path_to_uri(&root) else {
        return;
    };

    let text = app.buffer.to_string();
    let language = server.language_id();
    let command = server.command.clone();
    let args: Vec<&str> = server.args.iter().map(String::as_str).collect();

    // 先拿到会话，再说话 —— 分两步是因为 `acquire` 要可变借池子，
    // 而 `show` 要可变借那个会话，同一个表达式里做不到
    let outcome = match pool.acquire(&command, &root_uri, || {
        Session::start(&command, &args, Some(root.as_path()), Some(&root_uri))
    }) {
        Ok(session) => session.show(&uri, language, &text).err(),
        // 起不来（最常见的就是那个命令没装）。⚠️ 同一把钥匙**只报一次** ——
        // 这句话每按一个键就刷一遍的话，会把 `:w` 那句「Saved foo.rs」冲掉，
        // 用户就看不见自己保存成功了
        Err(failure) if !failure.already_reported => {
            app.set_status_message(format!("LSP: {}", failure.reason));
            None
        }
        Err(_) => None,
    };

    if let Some(err) = outcome {
        // 写不进去基本就是「它已经死了」。真正的收摊在 `drain_lsp` 里 ——
        // 那边的 `Broken` 才是准信，这里只把这一轮的写失败说出来。
        app.set_status_message(format!("LSP: {err}"));
    }
}

/// 收所有服务器这一轮说的话。
///
/// `redraw` 会被改成 `true`：诊断来了行号栏的颜色就变了，必须重画 ——
/// 否则新颜色要等你下次按键才出现，看起来就像「它反应很慢」。
fn drain_lsp(pool: &mut Pool, app: &mut App, redraw: &mut bool) {
    for spoken in pool.poll_all() {
        match spoken.outcome {
            // 握上手了。说一句，**只一次**（每个会话各说一次）——
            // 不然「这个文件没问题」和「服务器根本没连上」在屏幕上长得一模一样。
            //
            // ⚠️ 名字**必须**用 `spoken.command`，不能写死。这里原来是一句
            //    `"LSP: rust-analyzer is ready"` 的字面量 —— 只有 rust-analyzer
            //    的年代看不出来，配了 clangd 之后就成了**假消息**：起了 clangd，
            //    屏幕上却说 rust-analyzer。而这句话是用户唯一能确认
            //    「服务器到底起没起」的地方（手动冒烟测试时就是这么被骗了一次）。
            Outcome::Ready => {
                app.set_status_message(format!("LSP: {} is ready", spoken.command));
            }
            Outcome::Diagnostics(push) => {
                // 它也会报**别的文件**（我们刚关掉的那个、`build.rs`……），
                // 而且不同的根还会各报各的。只认现在屏幕上这个 ——
                // 不然你会看到一堆不属于这份代码的红线。
                if !is_current_file(app, &push.uri) {
                    continue;
                }
                // 空的那份也照样换上去：那是「这个文件现在没毛病」，
                // 不换的话改好的错误会永远留在行号栏上。
                app.set_diagnostics(push.diagnostics);
                *redraw = true;
            }
            // 线断了。不需要在这里做什么收摊 —— `Pool::poll_all` 已经把它
            // 从池子里摘掉了（`drop` 顺带收尸），我们只负责说一句。
            Outcome::Broken(why) => {
                app.set_status_message(format!("LSP: {why}"));
            }
        }
    }
}

/// 这条诊断推送说的是**现在我们打开的那个文件**吗。
///
/// ⚠️ 用 [`lsp::uri::same_file`] 比，**不能比字符串**：我们发出去的是
/// `file:///D:/...`，它回来的是 `file:///d:/...`（实测），字符串相等永远是假
/// —— 于是表现成「诊断一条都不显示」，而服务器那头一切正常，两头都看不出毛病。
///
/// 单独拆成函数是为了能直接测：它判错的后果（诊断全丢）在界面上看不出来。
///
/// ## ⚠️ 它只管「**存不存**」，不管「**画不画**」
///
/// 这两件事原来是混在一起的：这里曾经写着「不是普通文件就一律不认」，
/// 理由是「清单是我们生成的文本，认了就会把诊断画到清单本身上」。
/// 但那个理由管的是**画**，而这一层管的是**存** —— 结果多了一条没人想要的
/// 副作用，实测出来的（2026-09-15，手动冒烟测试 A/B 对照）：
///
/// ```text
/// A：打开坏文件 → 等 → `:errors`          → 报出 1 条错误   ✅
/// B：打开坏文件 → `:lsp` → `q` → `:errors` → 「No problems」  ❌
/// ```
///
/// 因为清单在屏幕上那段时间里到达的推送被**丢掉**了，而丢掉之后它
/// **不会再回来** —— 服务器只在文本变了的时候才推，而文本一个字没动。
/// 于是你从清单退回来，看到的是一个「干净」的文件。
///
/// `:lsp` 只是让这个坑更容易撞上（服务器还在启动时你就可能已经在看清单了）。
/// 真正修的是把两个判断分开：
///
/// - **画**：`ui::line_number_colour` 遇到虚拟视图直接返回 `plain`
///   （测试 `the_error_list_does_not_colour_its_own_line_numbers`）
/// - **存**：就是这里 —— 只要说的是我们打开的那个文件，就先收下来
///
/// 于是从清单退回去时，行号栏上的标记是活的，而不是进清单那一刻的。
///
/// 目录列表那一支不受影响：它的 `file_path` 是个**目录**，
/// 拿它去跟任何一个文件的 uri 比都是「不是」。
fn is_current_file(app: &App, uri: &str) -> bool {
    // 目录列表那个 `file_path` 是个**目录**，不是文件 —— 而诊断是长在文本上的。
    //
    // ⚠️ 判据是 [`DocumentKind::wraps_a_file`]，**不是** `is_virtual`。
    // 用后者就顺手把「清单盖在屏幕上时推来的诊断」一起丢了（见上面那段实测）。
    if !app.kind.wraps_a_file() {
        return false;
    }
    let Some(path) = app.file_path.as_deref() else {
        return false;
    };
    let Some(current) = lsp::uri::path_to_uri(Path::new(path)) else {
        return false;
    };
    lsp::uri::same_file(&current, uri)
}

/// 估算「文本区」能显示的行/列数，供 update 里的自动滚动使用。
///
/// 与 ui.rs 的布局保持一致：
/// - 高度 = 总行数 − 底部两行（`文件名+模式提示` 那一行 + 状态行）
/// - 宽度 = 总列数 − 行号栏宽度（若显示行号）
///
/// ⚠️ **没有边框了，所以不见 `− 2` / `− 4`。** 以前这里的两个减号是「上下各一条框线」
/// 和「左右各一格框线」—— 文本区去掉圆角框之后，第 0 行就是文件第一行、第 0 列就是
/// 行号栏第一格。ui.rs 那边是同一套坐标（见 `render_text_area` 里的 `inner_area`）。
fn compute_view_size(app: &App) -> (usize, usize) {
    let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    view_size_for(columns as usize, rows as usize, app)
}

/// [`compute_view_size`] 的**纯计算**部分：不碰终端，所以边界情况可以直接写测试。
///
/// 全部用 `saturating_sub`：窗口被拖得极窄时（列数 < 行号栏，
/// 比如 10 列宽的文件带 6 位行号），直接相减会**下溢 panic**（debug）
/// 或翻成一个天文数字（release），两种都不是我们想要的。
fn view_size_for(columns: usize, rows: usize, app: &App) -> (usize, usize) {
    let gutter_width = if app.config.show_line_numbers {
        app.buffer.get_line_count().to_string().len() + 1
    } else {
        0
    };
    // 底部两行：`文件名 + 模式提示` 一行，状态行一行
    let view_height = rows.saturating_sub(2);
    let view_width = columns.saturating_sub(gutter_width).max(1);
    (view_height, view_width)
}

/// 执行复制动作（由 update 返回 `Action::Copy` 后触发）。
///
/// 用 `arboard` 直接调操作系统的剪贴板 API（Windows 上是 Win32 clipboard），
/// **不依赖终端**支持 OSC 52，所以哪个终端都能用。
fn copy_to_clipboard(app: &mut App, text: &str) {
    match write_clipboard(text) {
        Ok(()) => app.set_status_message(format!(
            "Copied {} chars to clipboard",
            text.chars().count()
        )),
        Err(err) => app.set_status_message(format!("Copy failed: {err}")),
    }
}

/// 执行剪切动作（`Action::Cut`）：那几行**已经**从文档里删掉了，这里只负责写剪贴板。
///
/// ⚠️ 失败时的措辞要多说一句：内容已经从文档里没了，只报一句 `Copy failed`
/// 会让人以为「什么都没发生」，而实际上那几行真没了 ——
/// 得告诉他 `u` 能把它们找回来。
fn cut_to_clipboard(app: &mut App, text: &str, rows: usize) {
    let what = if rows == 1 {
        "1 line".to_string()
    } else {
        format!("{rows} lines")
    };
    match write_clipboard(text) {
        Ok(()) => app.set_status_message(format!("Cut {what} to clipboard")),
        Err(err) => app.set_status_message(format!(
            "Clipboard failed: {err}  (the cut still removed {what}; `u` to undo)"
        )),
    }
}

/// 真正碰剪贴板的那一下。`Copy` 和 `Cut` 共用，只有回执不一样。
fn write_clipboard(text: &str) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut cb| cb.set_text(text.to_string()))
        .map_err(|err| err.to_string())
}

/// 一个动作执行完之后，主循环该干什么。
///
/// 为什么要把「让位」单独分出来：**只有它需要终端**，而终端只有主循环手上有。
/// 要是把它塞进 [`run_action`]，那个函数就得要一个 `&mut Terminal` ——
/// 而它在测试里根本造不出来（`Terminal::new` 要真终端），于是
/// 「存不下来就不许退」那类性质就再也测不了了。
enum Step {
    /// 接着跑
    Continue,
    /// 退出主循环
    Quit,
    /// 把终端让出去跑这一行
    HandOver(String),
    /// 起一个后台任务（`:check`）。
    ///
    /// 同样得回主循环才能做：起线程、拿收件通道、以后每轮去 `try_recv` ——
    /// 这些都是「活着的东西」，不该让 [`run_action`] 碰。
    StartCheck,
    /// 起一次后台格式化（`:fmt`）。
    ///
    /// 跟 [`Step::StartCheck`] 是同一个理由：线程 + 通道是活着的东西。
    StartFormat,
    /// 铺一份「语言服务器现在什么状况」的清单（`:lsp`）。
    ///
    /// 同样得回主循环：那份清单要说「现在跑着哪几个」，只有池子知道。
    ShowLspStatus,
}

/// 执行一个动作，告诉主循环下一步干什么。
///
/// ## 为什么把它从事件循环里拎出来
///
/// **它绝不能往外抛错误。** 以前这里是 `Action::Save => save_file(app)?` ——
/// `?` 会把 io 错误一路冒到 `main`，于是「目标目录不存在 / 文件只读 / 磁盘满」
/// 会让**整个编辑器当场退出**，而用户刚敲的内容还在内存里、一起没了。
/// 打开失败只是一行状态栏，保存失败没理由更暴力。
///
/// 另一层原因很实际：**事件循环没法测**（要一个真终端），
/// 拎出来之后「存不下来就不许退」这条性质才能被测试钉住。
fn run_action(app: &mut App, action: Action) -> Step {
    match action {
        Action::Quit => return Step::Quit,
        Action::Save => {
            save_file(app);
        }
        Action::SaveAs(path) => {
            save_file_as(app, &path);
        }
        // ⚠️ 存不下来就**别退** —— 退了的话用户刚敲的东西就没了
        Action::SaveAndQuit => {
            if save_file(app) {
                return Step::Quit;
            }
        }
        Action::Copy(text) => copy_to_clipboard(app, &text),
        Action::Cut { text, rows } => cut_to_clipboard(app, &text, rows),
        Action::OpenPath(path) => open_path(app, &path),
        Action::Settings => open_settings(app),
        Action::ReloadConfig => reload_config(app),
        // 需要终端，交回主循环（见 [`Step`] 的说明）
        Action::RunExternal(line) => return Step::HandOver(line),
        // 需要起线程 / 留通道，同样交回主循环
        Action::RunCheck => return Step::StartCheck,
        // 同上 —— 格式化不碰终端，但它要起线程、留通道
        Action::Format => return Step::StartFormat,
        // 正文要池子（「现在跑着哪几个」），交回主循环
        Action::ShowLspStatus => return Step::ShowLspStatus,
        // 纯状态：把虚拟视图退掉，原来那份文档原样放回来（不需要读盘）
        Action::RestoreDocument => {
            if !app.restore_document() {
                // 几乎不可能：按 `q` 的前提就是「正处在虚拟视图里」。
                // 真到了这儿也只能说一句，不该抛错更不该退出。
                app.set_status_message("Nothing to go back to");
            }
        }
        // 把屏幕上那份清单抄进输出文件夹
        Action::WriteOutbox(file) => write_outbox(app, file),
    }
    Step::Continue
}

/// 把**屏幕上那份清单**抄进输出文件夹。
///
/// ⚠️ 抄的是 `app.buffer`，不是重新算一遍 —— 屏幕上显示的就是 `app.buffer`，
/// 两个来源的话迟早会出现「文件里有、屏幕上没有」这种对不上的情况。
///
/// 失败只说一句：屏幕上那份清单照样在，而输出文件夹只是一个方便。
/// （跟「配置文件读不到就全用默认值」一个立场：锦上添花的东西不该拦住基本盘。）
fn write_outbox(app: &mut App, file: OutFile) {
    let text = app.buffer.to_string();
    if let Err(err) = Outbox::locate().write(file, &text) {
        app.set_status_message(format!("Cannot write {} ({err})", file.name()));
    }
}

/// 把编辑器要的终端状态装回去（[`release_terminal`] 的逆操作），
/// 并让下一帧全量重画。
fn restore_editor_terminal(terminal: &mut Term) -> io::Result<()> {
    enable_raw_mode()?;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        SetCursorStyle::SteadyBlock
    )?;
    // 备用屏被外部命令写过，可能是脏的 —— 逼下一轮 draw 全量重画
    terminal.clear()
}

/// 让位期间的守卫：**它一离开作用域，终端就一定被装回来**。
///
/// 为什么非要这样：raw 模式 / 备用屏 / 鼠标捕获是**终端自己的标志位** ——
/// 进程外的全局状态，不是我们的变量。忘了还回去，用户就留在一个
/// 「敲字不回显、Ctrl+C 失灵、得敲 reset 才能救」的终端里。
///
/// 所以不能指望「每条路径都记得恢复」，得让**作用域**来保证：正常返回、
/// 提前 return、panic（默认 unwind 会跑 drop）全都经过同一个出口。
/// ⚠️ 前提是 panic 用 unwind —— `Cargo.toml` 里没有 `panic = "abort"`。
struct TerminalHandover<'a> {
    terminal: &'a mut Term,
}

impl Drop for TerminalHandover<'_> {
    fn drop(&mut self) {
        // 恢复失败**不能 panic**：在 drop 里再炸一次只会让情况更糟，尽力就好
        let _ = restore_editor_terminal(self.terminal);
    }
}

/// 把一行原样交给系统的 shell。
///
/// 「原样」是重点：我们不分词、不管引号 —— 那是 shell 的语法。
/// （Windows 有个细节正好对上：`cmd /C` 要求把 `a && b` 那种整条包在引号里，
/// 而 Rust 传参时遇到空格会自动加引号。）
fn shell_command(line: &str) -> std::process::Command {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut cmd = std::process::Command::new(shell);
    cmd.arg(flag).arg(line);
    cmd
}

/// 让位时先重现的那一行：`!` + 用户敲的命令。
///
/// **为什么要重现**：离开备用屏之后终端是「上一个 shell 提示符」那个状态，
/// 直接蹦出一堆输出会很突兀 —— 用户一瞬间不知道刚才跑了什么、是不是自己敲的那条。
/// 先把命令行打出来，就跟在 shell 里敲一样有来龙去脉（Vim 的 `:!ls` 也这样）。
///
/// 格式跟底部输入行**逐字一致**（同一个 `!`、同样不加空格），
/// 这样画面就是连续的：屏幕上接着写出来的，就是你刚才在底栏看到的那一行。
fn echo_command_line(line: &str) -> String {
    format!("!{line}")
}

/// 执行一条外部命令：**把终端让出去**，让命令自己画、自己读按键。
///
/// 这是 Vim `:!cmd` 的做法：离开备用屏 → 把命令写出来 → 命令在真终端上跑
/// → 提示按回车 → 回来重画。
///
/// 好处是编码（GBK！）、颜色、交互式程序、超大输出**全都不用我们管** ——
/// 因为终端真的交给它了，我们一个字节都不碰。代价是**输出留不住**：
/// 想留住就得另做「捕获」那条路（Vim 也是两个都有：`:!` 和 `:r !`）。
///
/// ⚠️ **子进程自己的报错不用我们操心**：stdin/stdout/stderr 都继承给它，
/// 所以 `cmd` 的「不是内部或外部命令」、`git` 的 `fatal:`、编译器的诊断
/// 全都**原样打在终端上**，我们既不拦也改不了。我们只负责自己那两行：
/// 开跑前重现命令行，跑完说一句怎么回去。
fn run_external_command(terminal: &mut Term, app: &mut App, line: &str) {
    // 守卫**先建**：从这一行起，不管怎么出去（提前 return / panic），终端都会被装回来
    let _guard = TerminalHandover { terminal };

    if let Err(err) = release_terminal() {
        app.set_status_message(format!("Cannot hand the terminal over: {err}"));
        return; // drop 会把（可能只拆了一半的）状态装回去
    }

    // ⚠️ 必须印在 `release_terminal()` **之后**：之前印的话落在备用屏上，
    // 一离开备用屏就跟着一起消失，等于没印。
    println!("{}", echo_command_line(line));

    // stdin / stdout / stderr 都继承给子进程 —— 它就是在真终端上跑
    match shell_command(line).status() {
        Ok(status) if status.success() => {}
        Ok(status) => println!("\n[stbd] command exited with {status}"),
        Err(err) => println!("\n[stbd] cannot run: {err}"),
    }

    // 等一次回车再回去，否则输出会被立刻擦掉（Vim 也这样）
    println!("\n[stbd] press ENTER to return");
    let mut ignored = String::new();
    let _ = io::stdin().read_line(&mut ignored);
    // 函数结束 → `_guard` 被 drop → 界面回来
}

/// 把缓冲区写到 `path`。成功返回 `true`。
///
/// ⚠️ **失败只写状态栏，绝不往上抛**。以前这里是 `io::Result`，调用方写
/// `save_file(app)?` —— 于是「目标目录不存在 / 文件只读 / 磁盘满」会把错误一路冒到
/// `main`，**整个编辑器当场退出**，而用户刚敲的内容还在内存里、一起没了。
/// 打开失败是温和的（一行状态栏），保存失败没有理由更暴力。
fn write_buffer_to(app: &mut App, path: &str) -> bool {
    match std::fs::write(path, app.buffer.to_string()) {
        Ok(()) => true,
        Err(err) => {
            app.set_status_message(format!("Save failed: {err}  ({path})"));
            false
        }
    }
}

/// 执行保存动作（无参数 `:w`：写回**原来那个文件**）。返回是否成功。
fn save_file(app: &mut App) -> bool {
    let Some(path) = app.file_path.clone() else {
        // 新开的缓冲区还没有名字 —— 告诉用户出路，而不是只说「不行」
        app.set_status_message("No file name; use `:w <path>` to save it somewhere");
        return false;
    };
    // 目录列表不是可编辑的文档 —— 种类在打开时就定好了，这里不用再 stat 一次。
    // （但 `:w <path>` 另存为是允许的：那是把列表导出去，不是写回目录。）
    if app.kind != DocumentKind::File {
        app.set_status_message("Cannot save: this buffer is a directory listing (use `:w <path>`)");
        return false;
    }
    if !write_buffer_to(app, &path) {
        return false;
    }
    app.mark_saved();
    app.set_status_message(format!("Saved {path}"));
    true
}

/// 执行 `:w <path>` —— **另存为**：写出去，并且这个缓冲区从此就叫那个名字。
///
/// 路径按 `current_directory()` 解析，跟 `:open` 用的是同一个基准 ——
/// 否则又是「同一件事两个锚点」那个 bug。
fn save_file_as(app: &mut App, path: &str) -> bool {
    let target = file_io::full_path_in(app.current_directory().as_deref(), path);
    if !write_buffer_to(app, &target) {
        return false;
    }
    // 写成功了才算另存为：失败时名字不能改，否则第二次 `:w` 会写到那个不存在的地方
    app.rename_document(target.clone());
    app.kind = DocumentKind::File;
    app.mark_saved();
    // 记进导航列表，`:ls` 上的 `*` 才指得对 —— 缓冲区换名字了，「我在哪」也变了
    app.documents.remember(&target);
    app.set_status_message(format!("Saved as {target}"));
    true
}

/// 打开一个路径后，判定它属于哪种文档（读不到元数据就当普通文件）。
///
/// 判定只做一次，结果存在 `App::kind` 里 —— 之后要判断「能不能保存」
/// 「Enter 能不能进入条目」都读那个字段，不再重复 stat 磁盘。
fn document_kind(path: &str) -> DocumentKind {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => DocumentKind::DirectoryListing,
        _ => DocumentKind::File,
    }
}

/// 读取命令行参数里的路径。
///
/// - 普通文件：读取文件内容；
/// - 目录：列出直接子项；
/// - 不存在或无法读取的路径：按新文件处理，保留路径让 `:w` 可以创建它。
///
/// 读取逻辑统一放在 `file_io`，这里只负责命令行这一层的错误提示。
fn load_file_from_args() -> (Option<String>, String) {
    match std::env::args().nth(1) {
        Some(path) => match file_io::load_file_or_directory(&path) {
            Ok(content) => (Some(path), content),
            Err(err) => {
                eprintln!("Note: cannot open {path} ({err}); opening as a new file");
                (Some(path), String::new())
            }
        },
        None => (None, String::new()),
    }
}

/// 执行打开动作（由 update 返回 `Action::OpenPath` 后触发）。
///
/// 路径可以是文件也可以是目录。读取失败时只改状态栏，
/// 不破坏当前已经打开的内容（这比先清空再报错安全）。
fn open_path(app: &mut App, path: &str) {
    // 相对路径相对的是**当前文档所在的那个目录**，不是进程的工作目录。
    // 这样「在列表里对 main.rs 按 Enter」和「敲 `:open main.rs`」才是同一件事 ——
    // 以前前者能开、后者报「系统找不到指定的文件」，就因为它们用了两个不同的基准：
    // Enter 那条会拼上目录名，:open 那条直接去问操作系统（cwd 是启动时那个目录）。
    let target = file_io::full_path_in(app.current_directory().as_deref(), path);

    match file_io::load_file_or_directory(&target) {
        Ok(content) => {
            // 读盘成功了才动状态：先判定文档种类（它决定 Enter 能不能「进入」条目、能不能保存）
            let kind = document_kind(&target);
            app.replace_document(target.clone(), content);
            app.kind = kind;
            // 同样只有成功才记进导航列表 —— 失败时当前文档没变，
            // 记进去会让 `q` 「返回」到一个从没打开过的路径
            app.documents.remember(&target);
            app.set_status_message(format!("Opened {target}"));
        }
        // 报错要带上**实际找的那个完整路径** —— 否则「相对谁」又变成一个谜
        Err(err) => app.set_status_message(format!("Open failed: {err}  (looked for {target})")),
    }
}

/// 执行 `:settings`：打开配置文件让用户改（文件还不存在就先按内置模板生成一份）。
///
/// 这是「配置就在程序旁边」这条设计的具体体现：用户不用自己去 %APPDATA%
/// 或仓库里找文件，敲一句命令就能打开来改。
fn open_settings(app: &mut App) {
    match config::Config::ensure_settings_file() {
        // 复用打开文件那条路：读盘 → replace_document，行为与 `:stbd <path>` 完全一致
        Ok(path) => {
            let text = path.display().to_string();
            open_path(app, &text);
        }
        Err(message) => app.set_status_message(format!("Settings: {message}")),
    }
}

/// 执行 `:config reload`：重读磁盘上的配置文件，把新设置换上。
///
/// 典型用法：`:settings` 打开配置 → 改 → `:w` 保存 → `:config reload`，不用重启。
/// 读盘失败不致命：退回默认值并说明原因（与启动时的处理一致）。
fn reload_config(app: &mut App) {
    let loaded = config::Config::load();
    app.apply_config(loaded.config);
    app.set_config_source(loaded.source);
    match loaded.warning {
        Some(warning) => app.set_status_message(format!("Config: {warning}")),
        // 把新的摘要一并报出来，用户能立刻确认「改的东西生效了没」
        None => app.set_status_message(format!("Settings reloaded: {}", app.config.describe())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 让位时先印出来的那一行，必须跟底栏显示的一模一样。
    ///
    /// `run_external_command` 自己要真终端，测不了 —— 所以把「我们说的那句话」
    /// 拆成这个纯函数，让格式能被钉住。
    ///
    /// 看着像废话，但它守着一个真实的坑：如果哪天有人在这里「顺手」加个
    /// `$ ` 前缀、加个空格、或者把引号重新转义一遍，用户看到的就不再是自己敲的东西
    /// —— 而**这条命令已经真跑出去了**，对不上会让人怀疑跑的是不是另一条。
    #[test]
    fn the_command_is_echoed_exactly_as_the_bottom_bar_showed_it() {
        assert_eq!(echo_command_line("git status"), "!git status");
        // 引号、`&&`、连续空格：一个字符都不许动
        assert_eq!(
            echo_command_line(r#"git  commit -m "a  b" && echo done"#),
            r#"!git  commit -m "a  b" && echo done"#
        );
    }

    /// 不要求项目根的语言（`clangd`）拿到的是**文件自己所在的目录**。
    ///
    /// 这是 clangd 能不能用的**唯一**开关：`[lsp.c]` 那节没写 `root_marker`，
    /// 于是走这条分支。要是这里返回 `None`，`.c` 文件就永远不起服务器 ——
    /// 而屏幕上什么都不显示，看起来就像「这个文件恰好没问题」。
    #[test]
    fn a_language_that_needs_no_project_root_uses_the_files_own_directory() {
        assert_eq!(
            project_root_of("D:/proj/src/x.c", None),
            Some(PathBuf::from("D:/proj/src"))
        );
        // 根目录下的文件：父目录是 `D:/`，不是空的
        assert_eq!(project_root_of("D:/x.c", None), Some(PathBuf::from("D:/")));
    }

    /// ⚠️ **相对路径绝不能变成一个「根」。**
    ///
    /// 反直觉的一条：`Path::new("x.c").parent()` 是 `Some("")` 而**不是**
    /// `None`（空路径确实是个「父目录」，只是没意义）。照直用就会拿着 `""`
    /// 去求 rootUri，那是一条连不上任何项目的路。
    ///
    /// 正常流程里进不来 —— `file_io::full_path_in` 在打开那一刻就把路径钉成
    /// 绝对的了。但这条分支值一毛钱的防守：代价为零，而错了以后的表现
    /// 是「这个文件永远没诊断」，很难查。
    #[test]
    fn a_relative_path_never_becomes_a_project_root() {
        assert_eq!(project_root_of("x.c", None), None);
        assert_eq!(project_root_of("x.c", Some("Cargo.toml")), None);
    }

    /// 要求项目根的语言（`rust-analyzer`）真的**往上**找。
    ///
    /// 实测过一次错得很难看的样子：根算错时 rust-analyzer 对那个文件
    /// 一个字都不说（45 秒 0 份推送），屏幕上同样毫无提示。
    #[test]
    fn a_marker_is_found_by_walking_upwards() {
        let root = std::env::temp_dir().join("stbd-root-probe");
        let nested = root.join("crates").join("inner").join("src");
        std::fs::create_dir_all(&nested).expect("建临时目录");
        std::fs::write(root.join("Cargo.toml"), "[package]\n").expect("写标记文件");

        let file = nested.join("main.rs");
        let file = file.to_string_lossy().into_owned();
        std::fs::write(&file, "fn main() {}\n").expect("写源文件");

        assert_eq!(
            project_root_of(&file, Some("Cargo.toml")),
            Some(root.clone())
        );
        // 标记在 `/crates` 里 —— 那它就该停在那儿，不能一路窜到顶
        assert_ne!(
            project_root_of(&file, Some("Cargo.toml")),
            Some(nested.clone())
        );

        // 找一个不存在的标记 → `None`（调用方据此**不起服务器**，不报错）
        assert_eq!(project_root_of(&file, Some("build.gradle")), None);

        std::fs::remove_dir_all(&root).ok();
    }

    /// 尺寸换算要跟 ui.rs 的布局对齐：高 = 行数 − 2，宽 = 列数 − 行号栏
    ///
    /// ⚠️ **没有边框，所以没有那两个减号。** 以前是「高 − 4、宽 − 2 − 行号栏」：
    /// 4 = 底部两行 + 上下两条框线，2 = 左右两条框线（见 `view_size_for`）。
    #[test]
    fn view_size_follows_the_layout() {
        let app = App::from_content(None, "hello".to_string());
        // 1 行的文件 → 行号栏「1 」占 2 格
        assert_eq!(view_size_for(80, 24, &app), (22, 78));
        assert_eq!(view_size_for(12, 10, &app), (8, 10));
    }

    /// 回归：窗口被拖得极窄时**不能 panic**。
    ///
    /// 以前这里写的是 `columns - 2 - gutter_width`：列数不够时 debug 下溢 panic、
    /// release 下翻成一个天文数字（视口计算全乱）。现在全走 saturating_sub。
    #[test]
    fn view_size_survives_a_tiny_terminal() {
        let app = App::from_content(None, "hello".to_string());
        assert_eq!(view_size_for(0, 0, &app), (0, 1), "宽度至少留 1");
        assert_eq!(view_size_for(1, 1, &app), (0, 1));
        assert_eq!(view_size_for(3, 4, &app), (2, 1));

        // 几万行的文件会把行号栏撑宽（「20000」5 位 + 1 个空格 = 6 格）
        let long = (0..20_000)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let app = App::from_content(None, long);
        assert_eq!(view_size_for(10, 24, &app).1, 4, "10 格减去 6 格行号栏");
        // ⚠️ **窗口比行号栏还窄**才是原来那个下溢的场景：6 > 3，
        //    相减会掉到 0 以下 —— 所以这里只能靠 saturating_sub + max(1)
        assert_eq!(view_size_for(3, 24, &app).1, 1, "再窄也得留下 1 格正文");
        assert_eq!(view_size_for(0, 24, &app).1, 1);
        assert_eq!(view_size_for(20, 24, &app).1, 14);
    }

    /// 关掉行号后行号栏宽为 0，整行都归正文（而且**真的**是整行 —— 没有边框了）
    #[test]
    fn view_size_without_line_numbers_uses_the_whole_width() {
        let mut app = App::from_content(None, "hello".to_string());
        app.config.show_line_numbers = false;
        assert_eq!(view_size_for(80, 24, &app), (22, 80));
    }

    /// 回归：打开成功后必须记**完整路径**。
    ///
    /// 以前记的是用户敲的原样（`Cargo.toml`），于是「这份文档在哪」跟着进程的
    /// 工作目录漂：`:w` 会写到别处，`:ls` 也不是真地址。
    #[test]
    fn open_path_remembers_the_full_path() {
        let mut app = App::new();
        open_path(&mut app, "Cargo.toml");

        let path = app.file_path.clone().expect("打开成功后该记住路径");
        assert!(std::path::Path::new(&path).is_absolute(), "{path}");
        assert!(path.ends_with("Cargo.toml"), "{path}");
        // 导航列表里记的必须是**同一个**字符串，否则 `q` 会「返回」到另一种写法
        assert_eq!(app.documents.current().as_deref(), Some(path.as_str()));
    }

    /// 同一个文件用两种写法打开，只该在导航列表里留一条。
    ///
    /// ⚠️ 第二次必须用一个**真正不同的写法**（绝对路径）。第一版这里写的是
    /// 「读回 `app.file_path` 再打开一次」—— 那等于用同一个字符串开两次，
    /// 修复前也会通过，是个空测试。
    #[test]
    fn the_same_file_opened_twice_stays_one_entry() {
        let mut app = App::new();
        // 第一次：相对写法（用户敲的）
        open_path(&mut app, "Cargo.toml");
        // 第二次：同一个文件的**绝对**写法（用户从 `:ls` 里复制粘贴的那种）
        let absolute = std::env::current_dir()
            .unwrap()
            .join("Cargo.toml")
            .to_string_lossy()
            .into_owned();
        open_path(&mut app, &absolute);

        // 两种写法必须归成一条记录 ⇒ 没有「上一个」（`DocumentList` 没有 len，就用这个判）
        assert!(app.documents.previous_path().is_none());
    }

    /// 带目录的相对路径（用户实际的场景：文件在子目录里）。
    ///
    /// 关键断言是**「指向同一个文件」** —— `:w` 写回哪里全靠它。
    /// 光有 `is_absolute` / `ends_with` 不够：join 错了也能满足那两条。
    #[test]
    fn open_path_resolves_a_relative_path_that_has_directories() {
        let mut app = App::new();
        open_path(&mut app, "src/main.rs");

        let stored = app.file_path.clone().expect("打开成功后该记住路径");
        assert!(std::path::Path::new(&stored).is_absolute(), "{stored}");
        assert!(stored.ends_with("main.rs"), "{stored}");
        assert_eq!(
            std::fs::canonicalize(&stored).unwrap(),
            std::fs::canonicalize("src/main.rs").unwrap(),
            "存的路径必须指向打开的那个文件"
        );
    }

    /// 回归：`:open` 的相对路径相对的是**当前文档所在的那个目录**。
    ///
    /// 以前它相对进程的工作目录，于是「打开 `src/main.rs` 后敲 `:open commands.rs`」
    /// 必然失败（`commands.rs` 不在仓库根）。而同一个意思，在目录列表里按 Enter 却行。
    #[test]
    fn open_resolves_relative_paths_against_the_current_directory() {
        let mut app = App::new();
        open_path(&mut app, "src/main.rs"); // 基准变成 ...\src
        open_path(&mut app, "commands.rs"); // 相对它 → ...\src\commands.rs

        let stored = app.file_path.clone().expect("应该打开成功");
        assert!(stored.ends_with("commands.rs"), "{stored}");
        assert!(std::path::Path::new(&stored).exists(), "{stored}");
    }

    /// 用户实际踩的那条路：打开 `src`（看到一份列表），敲 `:open main.rs`。
    ///
    /// 列表的 `file_path` **就是**它列的那个目录，所以基准天然成立。
    #[test]
    fn a_relative_open_works_from_a_directory_listing() {
        let mut app = App::new();
        open_path(&mut app, "src");
        assert_eq!(app.kind, DocumentKind::DirectoryListing);

        open_path(&mut app, "main.rs");
        assert_eq!(app.kind, DocumentKind::File);
        let stored = app.file_path.clone().unwrap();
        assert!(stored.ends_with("main.rs"), "{stored}");
        assert!(std::path::Path::new(&stored).exists(), "{stored}");
    }

    /// 找不到时要说清楚**去哪儿找过** —— 否则「相对谁」又变成一个谜。
    #[test]
    fn a_failed_open_says_where_it_looked() {
        let mut app = App::new();
        open_path(&mut app, "stbd-definitely-missing-9527.rs");
        assert!(
            app.status_message.contains("looked for"),
            "{}",
            app.status_message
        );
        assert!(
            app.status_message
                .contains("stbd-definitely-missing-9527.rs"),
            "{}",
            app.status_message
        );
    }

    /// `:w <path>` —— 另存为：写出去，并且缓冲区从此叫这个名字。
    #[test]
    fn save_as_writes_the_file_and_renames_the_buffer() {
        let dir = std::env::temp_dir().join(format!("stbd_save_as_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.txt").to_string_lossy().into_owned();

        // 新开的缓冲区：没有名字，所以以前根本存不了
        let mut app = App::from_content(None, "hello".to_string());
        assert!(app.file_path.is_none());

        assert!(save_file_as(&mut app, &target));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
        // 从此这个缓冲区就叫那个名字了 —— 第二次 `:w` 会写回这里
        assert_eq!(app.file_path.as_deref(), Some(target.as_str()));
        assert!(!app.dirty);
        assert!(
            app.status_message.contains("Saved as"),
            "{}",
            app.status_message
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 回归：保存失败**只报错，不退出**。
    ///
    /// 以前 `main` 里写的是 `save_file(app)?`，写失败就把错误冒到 `main` ——
    /// 整个编辑器当场退出，而用户刚敲的内容还在内存里。
    #[test]
    fn a_failed_save_reports_instead_of_killing_the_editor() {
        let mut app = App::from_content(None, "hello".to_string());
        // 目标目录不存在 → 写失败
        assert!(!save_file_as(&mut app, "stbd-no-such-dir-9527/out.txt"));
        assert!(
            app.status_message.contains("Save failed"),
            "{}",
            app.status_message
        );
        // 没写成功就不算另存为：名字不能改，否则第二次 `:w` 会写到那个不存在的地方
        assert!(app.file_path.is_none());
    }

    /// 把动作跑了，只看「它有没有要求退出」——`run_action` 返回的是 `Step`，
    /// 测试里关心退不退，这么写比满地 `matches!(.., Step::Quit)` 顺眼。
    fn asks_to_quit(app: &mut App, action: Action) -> bool {
        matches!(run_action(app, action), Step::Quit)
    }

    /// 回归：`:wq` 存不下来时**不许退**（退了内容就没了）；存得下来才退。
    #[test]
    fn a_failed_save_must_not_ask_to_quit() {
        let mut app = App::from_content(None, "hello".to_string());

        // 没有名字 ⇒ 存不了 ⇒ 绝不能退
        assert!(!asks_to_quit(&mut app, Action::SaveAndQuit));
        assert!(
            app.status_message.contains("No file name"),
            "{}",
            app.status_message
        );

        // 先给它一个名字（另存为）—— 这一步**不该**退出
        let dir = std::env::temp_dir().join(format!("stbd_wq_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.txt").to_string_lossy().into_owned();
        assert!(!asks_to_quit(&mut app, Action::SaveAs(target.clone())));
        assert_eq!(app.file_path.as_deref(), Some(target.as_str()));

        // 现在存得下来了 → `:wq` 才该退
        assert!(asks_to_quit(&mut app, Action::SaveAndQuit), "这次该退了");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 无参 `:w` 且没有名字时，要说清楚出路是 `:w <path>`。
    #[test]
    fn saving_without_a_name_points_at_save_as() {
        let mut app = App::from_content(None, "hello".to_string());
        assert!(!save_file(&mut app));
        assert!(
            app.status_message.contains(":w <path>"),
            "{}",
            app.status_message
        );
    }

    /// 目录列表不能写回原地（那是个目录），但**可以另存为** —— 那是把列表导出去。
    #[test]
    fn a_listing_cannot_be_saved_in_place_but_can_be_saved_as() {
        let mut app = App::new();
        open_path(&mut app, "src");
        assert_eq!(app.kind, DocumentKind::DirectoryListing);

        assert!(!save_file(&mut app));
        assert!(
            app.status_message.contains("directory listing"),
            "{}",
            app.status_message
        );

        let dir = std::env::temp_dir().join(format!("stbd_listing_out_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("listing.txt").to_string_lossy().into_owned();
        assert!(save_file_as(&mut app, &target));
        assert!(
            std::fs::read_to_string(&target)
                .unwrap()
                .contains("main.rs")
        );
        // 导出去之后它就是一份普通文案了
        assert_eq!(app.kind, DocumentKind::File);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// **Enter 和 `:open` 是同一条路** —— 这条把两半接起来。
    ///
    /// 半条在 `update.rs`（Enter 只交出条目的**名字**，见同名测试），
    /// 半条在这儿（`open_path` 按当前目录解析）。两边各自都测过了，
    /// 这条测的是**接起来之后**确实落在同一个文件上。
    #[test]
    fn enter_and_open_land_on_the_same_file() {
        let mut app = App::new();
        open_path(&mut app, "src"); // 基准 = ...\src
        assert_eq!(app.kind, DocumentKind::DirectoryListing);

        // Enter 在列表里交出来的就是一个名字
        run_action(&mut app, Action::OpenPath("main.rs".to_string()));

        let stored = app.file_path.clone().expect("应该打开成功");
        assert!(stored.ends_with("main.rs"), "{stored}");
        assert!(stored.contains("src"), "应该落在 src 里：{stored}");
    }

    // ---------- 语言服务器的判断 ----------

    /// ⚠️ **大小写不同的盘符是同一个文件。**
    ///
    /// 实测：我们发出去的是 `file:///D:/...`，服务器回来的是 `file:///d:/...`。
    /// 拿字符串相等去比，这条判断**永远是假** —— 表现成「诊断一条都不显示」，
    /// 而服务器那头一切正常。两头都看不出毛病，只能靠这条测试钉住。
    /// ⚠️ 同一个根反复失败只报一次 —— 不然每按一个键就把「Saved x.rs」盖掉
    ///
    /// （下面这几条测的是 `is_current_file`，它守着「这条推送是不是说现在
    ///   屏幕上这个文件的」。虚拟视图那一条跟它同时管着「清单不会被当成源码发出去」。）
    #[test]
    fn a_push_about_the_same_file_in_a_different_case_still_matches() {
        let mut app = App::from_content(Some("D:\\proj\\src\\main.rs".to_string()), String::new());
        app.kind = DocumentKind::File;

        assert!(
            is_current_file(&app, "file:///d:/proj/src/main.rs"),
            "服务器把盘符规范化成小写了，我们却认不出来 —— 诊断会全丢"
        );
        assert!(
            is_current_file(&app, "file:///D:/proj/src/main.rs"),
            "原样的盘符当然也得认"
        );
    }

    /// 别的文件的诊断**不能**显示在当前文件上。
    #[test]
    fn a_push_about_another_file_is_refused() {
        let mut app = App::from_content(Some("D:\\proj\\src\\main.rs".to_string()), String::new());
        app.kind = DocumentKind::File;

        assert!(!is_current_file(&app, "file:///D:/proj/src/lib.rs"));
        assert!(!is_current_file(&app, "file:///D:/proj/src/main.rs.bak"));
    }

    /// ⚠️ **不是普通文件（目录列表、`:errors` 清单）一律不认。**
    ///
    /// 认了的后果很具体：那是一份**我们生成的**文本，服务器会认认真真地
    /// 给它报「这一堆字里有语法错误」，而那些错又会显示在屏幕上 ——
    /// 一个自己造出来的错误。
    #[test]
    fn a_listing_is_never_the_file_a_diagnostic_is_about() {
        let mut app = App::from_content(Some("D:\\proj".to_string()), String::new());
        app.kind = DocumentKind::DirectoryListing;

        assert!(!is_current_file(&app, "file:///D:/proj"));
        assert!(!is_current_file(&app, "file:///D:/proj/main.rs"));
    }

    /// 还没打开任何文件 → 谁的诊断都不是。
    #[test]
    fn with_no_file_open_nothing_matches() {
        let app = App::new();
        assert!(!is_current_file(&app, "file:///D:/proj/src/main.rs"));
    }

    /// 虚拟视图（`:errors` / `:lsp` 清单）**照样认**那个文件 —— 但认的是
    /// 「存下来」，不是「画出来」。
    ///
    /// ## 这条原来是反的，实测改的
    ///
    /// 旧版这里写的是「虚拟视图一律不认」，理由是「认了诊断就会画到清单上」。
    /// 那个理由管的是**画**，而这一层管的是**存** —— 混在一起的代价是
    /// 清单在屏幕上那段时间里到达的推送被**丢掉**，而且**不会再回来**
    /// （服务器只在文本变了才推）。手动冒烟 A/B 对照实测：
    ///
    /// ```text
    /// A：打开坏文件 → 等 → `:errors`          → 报出 1 条错误   ✅
    /// B：打开坏文件 → `:lsp` → `q` → `:errors` → 「No problems」  ❌
    /// ```
    ///
    /// 现在「画」由 `ui::line_number_colour` 单独守着（它遇到虚拟视图直接返回
    /// 原色，测试 `the_error_list_does_not_colour_its_own_line_numbers`），
    /// 所以这里收下来是安全的，而且从清单退回去时标记是**活的**。
    #[test]
    fn a_virtual_view_still_keeps_the_file_behind_it() {
        let mut app = App::from_content(Some("D:\\proj\\src\\main.rs".to_string()), String::new());
        app.kind = DocumentKind::File;
        assert!(is_current_file(&app, "file:///D:/proj/src/main.rs"));

        app.show_list(DocumentKind::Errors, "1: error: x".to_string());

        assert!(
            is_current_file(&app, "file:///D:/proj/src/main.rs"),
            "清单盖在上面时到达的推送必须**收下来** —— 丢掉就永远丢了，\
             退回去会看到一个假的「干净」文件"
        );
        // 但别的文件仍然不认（那才是「一堆不属于这份代码的红线」的来源）
        assert!(!is_current_file(&app, "file:///D:/proj/src/lib.rs"));
    }

    /// ⚠️ **清单绝不能写回你的源码文件。**
    ///
    /// `:w` 落地成 [`Action::Save`] 之后由 `run_action` 执行，所以这一条
    /// 只能在 main 这一层测 —— 命令层只管产出动作。
    #[test]
    fn saving_is_refused_while_the_error_list_is_on_screen() {
        let mut app = App::from_content(Some("target.rs".to_string()), "code".to_string());
        app.show_list(DocumentKind::Errors, "1: error: boom".to_string());

        run_action(&mut app, Action::Save);

        assert!(
            app.status_message.starts_with("Cannot save"),
            "该明确拦下来：{}",
            app.status_message
        );
    }

    /// ⚠️ 退出虚拟视图走的是「**原样放回**」，不是「重新打开」。
    ///
    /// 这条只能在 main 这一层测：命令层和按键层都只**产出**动作，
    /// 真正把它执行掉的是这里。
    ///
    /// 两件事都不能错：
    /// - 走 `OpenPath` 的话会重新读盘 —— **丢掉没保存的改动**；
    /// - 而且那条路上还拦着「没保存不许走」，于是你会被堵在清单里出不来 ——
    ///   而那恰好是「刚改完代码、想看还剩什么错」的时刻。
    #[test]
    fn leaving_a_list_puts_the_document_back_untouched() {
        let mut app = App::from_content(Some("a.rs".to_string()), "one\ntwo".to_string());
        // 改一下，把「未保存」这个状态摆上
        app.set_mode(stbd::app::EditorMode::Edit);
        app.cursor = stbd::app::Cursor { row: 0, col: 3 };
        app.insert_char_at_cursor('!');
        assert!(app.dirty);
        app.show_list(DocumentKind::Errors, "1: error: boom".to_string());

        run_action(&mut app, Action::RestoreDocument);

        assert_eq!(app.kind, DocumentKind::File);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("one!"));
        assert!(app.dirty, "「改过没保存」不能丢 —— 丢了 `:q` 就不再拦你");
    }
}
