//! 程序入口 —— main.rs 的职责
//!
//! 串起所有模块：
//! 1. 解析命令行参数（可选的待打开文件路径）
//! 2. 加载用户配置（config.rs；失败也不阻止启动）
//! 3. 进入 raw mode + 备用屏（Alternate Screen）
//! 4. 主循环：画(ui) → 读事件(event) → 分发(update) → 执行副作用(Action)
//! 5. 无论如何退出都恢复终端

use std::io;
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
use stbd::update;
use stbd::{check, config, event, file_io, ui};

/// 我们用到的终端后端类型（Crossterm 输出到 stdout）
type Backend = CrosstermBackend<io::Stdout>;
type Term = Terminal<Backend>;

fn main() -> io::Result<()> {
    // 1. 读取命令行传入的文件（没传就开一个新文件）
    let (file_path, content) = load_file_from_args();

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
    let mut check: Option<mpsc::Receiver<check::CheckReport>> = None;

    // 先画一帧。新循环不再「每轮开头都画」，所以启动这一帧得自己补上，
    // 否则打开编辑器会看到一片空白 —— 直到你按第一个键。
    terminal.draw(|frame| ui::render_ui(frame, app))?;

    loop {
        let mut redraw = false;

        // 最多等 TICK。返回 `None` = 键盘没动静，但**不等于没事可做**：
        // 下面照样会去看后台消息。
        if let Some(ev) = event::poll_event(TICK)? {
            redraw = true;
            match ev {
                event::Event::Key(key) => {
                    // update 只处理按键；把文本区估算尺寸传进去用于自动滚动
                    let (view_h, view_w) = compute_view_size(app);
                    // 一个按键可能产出多个动作（命令模式的 `&&` 链），按顺序执行
                    for action in update::handle_key_event(app, key, view_h, view_w) {
                        match run_action(app, action) {
                            Step::Quit => return Ok(()),
                            // 让位：终端暂时交出去，回来之后界面还是原样
                            Step::HandOver(line) => run_external_command(terminal, app, &line),
                            Step::StartCheck => check = start_check(app),
                            Step::Continue => {}
                        }
                    }
                }
                event::Event::Mouse(mouse) => {
                    let (view_h, view_w) = compute_view_size(app);
                    update::handle_mouse_event(app, mouse, view_h, view_w);
                }
                // 尺寸变化无需特殊处理：下面那次 draw 会自己用新尺寸
                event::Event::Resize(..) => {}
                // 粘贴：bracketed paste 已把整段文本聚合成一个事件，交给 update 分发
                event::Event::Paste(text) => {
                    let (view_h, view_w) = compute_view_size(app);
                    update::handle_paste_event(app, &text, view_h, view_w);
                }
                event::Event::Ignored => {}
            }
        }

        // 把后台攒下的消息**一口气全取走**。
        //
        // ⚠️ 是「全取走」而不是「取一条」：任务慢了我们也不该越落越远，
        //    取干净之后手上的状态永远是最新那一份。
        if let Some(rx) = &check {
            // 一口气全取走（`while`）而不是取一条：后台攒了几条就消化几条，
            // 落后的永远只有「当前这一轮」。现在只会收到一条，但形状先立对 ——
            // LSP 那一步消息是连绵不断的。
            let mut finished = false;
            while let Ok(report) = rx.try_recv() {
                app.set_status_message(report.describe());
                finished = true;
            }

            // ⚠️ `Disconnected` 必须和 `Empty` 分开看：前者是「发送端没了」
            //    （正常发完就结束，或者线程 panic 了），后者是「暂时没消息」。
            //    混为一谈的话，「任务悄悄死了」就永远发现不了，
            //    状态栏会永远停在「Checking…」。
            //
            //    上面那个 `while` 已经排空了队列，所以这里返回 Disconnected
            //    就是真的结束了 —— 而 `finished` 为真时我们已经做过汇报，
            //    不能再补一句「线程死了」把好消息盖掉。
            if !finished && matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)) {
                app.set_status_message("Check: the worker thread died".to_string());
                finished = true;
            }

            if finished {
                app.checking = false;
                check = None;
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

/// 估算「文本区」能显示的行/列数，供 update 里的自动滚动使用。
///
/// 与 ui.rs 的布局保持一致：
/// - 高度 = 总行数 − 底部两行（命令/模式 + 状态）− 文本区上下边框 2 行
/// - 宽度 = 总列数 − 左右边框 2 格 − 行号栏宽度（若显示行号）
fn compute_view_size(app: &App) -> (usize, usize) {
    let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    view_size_for(columns as usize, rows as usize, app)
}

/// [`compute_view_size`] 的**纯计算**部分：不碰终端，所以边界情况可以直接写测试。
///
/// 全部用 `saturating_sub`：窗口被拖得极窄时（列数 < 边框 + 行号栏，
/// 比如 10 列宽的文件带 6 位行号），直接相减会**下溢 panic**（debug）
/// 或翻成一个天文数字（release），两种都不是我们想要的。
fn view_size_for(columns: usize, rows: usize, app: &App) -> (usize, usize) {
    let gutter_width = if app.config.show_line_numbers {
        app.buffer.get_line_count().to_string().len() + 1
    } else {
        0
    };
    let view_height = rows.saturating_sub(4);
    let view_width = columns.saturating_sub(2 + gutter_width).max(1);
    (view_height, view_width)
}

/// 执行复制动作（由 update 返回 `Action::Copy` 后触发）。
///
/// 用 `arboard` 直接调操作系统的剪贴板 API（Windows 上是 Win32 clipboard），
/// **不依赖终端**支持 OSC 52，所以哪个终端都能用。
fn copy_to_clipboard(app: &mut App, text: &str) {
    let result = arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text.to_string()));
    match result {
        Ok(()) => app.set_status_message(format!(
            "Copied {} chars to clipboard",
            text.chars().count()
        )),
        Err(err) => app.set_status_message(format!("Copy failed: {err}")),
    }
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
        Action::OpenPath(path) => open_path(app, &path),
        Action::Settings => open_settings(app),
        Action::ReloadConfig => reload_config(app),
        // 需要终端，交回主循环（见 [`Step`] 的说明）
        Action::RunExternal(line) => return Step::HandOver(line),
        // 需要起线程 / 留通道，同样交回主循环
        Action::RunCheck => return Step::StartCheck,
    }
    Step::Continue
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

    /// 尺寸换算要跟 ui.rs 的布局对齐：高 = 行数 − 4，宽 = 列数 − 2 − 行号栏
    #[test]
    fn view_size_follows_the_layout() {
        let app = App::from_content(None, "hello".to_string());
        // 1 行的文件 → 行号栏「1 」占 2 格
        assert_eq!(view_size_for(80, 24, &app), (20, 76));
        assert_eq!(view_size_for(12, 10, &app), (6, 8));
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
        assert_eq!(view_size_for(3, 4, &app), (0, 1));

        // 几万行的文件会把行号栏撑宽，窄窗口下依然不能下溢
        let long = (0..20_000)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let app = App::from_content(None, long);
        assert_eq!(view_size_for(10, 24, &app).1, 2, "10 列放不下 6 格行号栏");
        assert_eq!(view_size_for(20, 24, &app).1, 12);
    }

    /// 关掉行号后行号栏宽为 0，整行都归正文
    #[test]
    fn view_size_without_line_numbers_uses_the_whole_width() {
        let mut app = App::from_content(None, "hello".to_string());
        app.config.show_line_numbers = false;
        assert_eq!(view_size_for(80, 24, &app), (20, 78));
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
}
