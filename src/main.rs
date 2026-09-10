//! 程序入口 —— main.rs 的职责
//!
//! 串起所有模块：
//! 1. 解析命令行参数（可选的待打开文件路径）
//! 2. 进入 raw mode + 备用屏（Alternate Screen）
//! 3. 主循环：画(ui) → 读事件(event) → 分发(update) → 执行副作用(Action)
//! 4. 无论如何退出都恢复终端

use std::io;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use cce::app::App;
use cce::update::{self, Action};
use cce::{event, ui};

/// 我们用到的终端后端类型（Crossterm 输出到 stdout）
type Backend = CrosstermBackend<io::Stdout>;
type Term = Terminal<Backend>;

fn main() -> io::Result<()> {
    // 1. 读取命令行传入的文件（没传就开一个新文件）
    let (file_path, content) = load_file_arg();

    // 2. 初始化终端：进入 raw mode + 备用屏
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Term::new(CrosstermBackend::new(stdout))?;

    // 3. 初始状态（若 dirty 有提示需求，可在这里 set_status）
    let mut app = App::from_content(file_path, content);

    // 4. 主循环；结束后恢复终端
    let result = run(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen)?;
    result
}

/// 主循环：画 → 读事件 → 分发 → 执行副作用，循环往复直到退出
fn run(terminal: &mut Term, app: &mut App) -> io::Result<()> {
    loop {
        // 先画当前状态（每轮都会用最新的终端尺寸）
        terminal.draw(|frame| ui::draw(frame, app))?;

        match event::read()? {
            event::Event::Key(key) => {
                // update 只处理按键；把文本区估算尺寸传进去用于自动滚动
                let (view_h, view_w) = view_size(app);
                if let Some(action) = update::handle(app, key, view_h, view_w) {
                    match action {
                        Action::Quit => return Ok(()),
                        Action::Save => save_file(app)?,
                        Action::SaveAndQuit => {
                            save_file(app)?;
                            return Ok(());
                        }
                    }
                }
            }
            event::Event::Mouse(mouse) => {
                let (view_h, view_w) = view_size(app);
                update::handle_mouse(app, mouse, view_h, view_w);
            }
            // 尺寸变化无需特殊处理：下一轮 draw 会自动使用新尺寸
            event::Event::Resize(..) => {}
            // 粘贴 / 忽略事件：MVP 先不管
            event::Event::Paste(_) | event::Event::Ignored => {}
        }
    }
}

/// 估算「文本区」能显示的行/列数，供 update 里的自动滚动使用。
///
/// 与 ui.rs 的布局保持一致：
/// - 高度 = 总行数 − 底部两行（命令/模式 + 状态）− 文本区上下边框 2 行
/// - 宽度 = 总列数 − 左右边框 2 格 − 行号栏宽度（若显示行号）
fn view_size(app: &App) -> (usize, usize) {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let gutter_w = if app.show_line_numbers {
        app.buffer.line_count().to_string().len() + 1
    } else {
        0
    };
    let view_h = (rows as usize).saturating_sub(4);
    let view_w = (cols as usize - 2 - gutter_w).max(1);
    (view_h, view_w)
}

/// 执行保存动作（由 update 返回 `Action::Save` 后触发）
fn save_file(app: &mut App) -> io::Result<()> {
    match app.file_path.clone() {
        Some(path) => {
            std::fs::write(&path, app.buffer.to_string())?;
            app.dirty = false;
            app.set_status(format!("Saved {path}"));
        }
        None => {
            app.set_status("No file name; cannot save (open with `cce <filename>`)");
        }
    }
    Ok(())
}

/// 读取命令行参数里的文件路径；读不到文件内容时按「新文件」处理（保留路径，
/// 这样之后用 `:w` 能把文件建出来）
fn load_file_arg() -> (Option<String>, String) {
    match std::env::args().nth(1) {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(content) => (Some(path), content),
            Err(err) => {
                eprintln!("Note: cannot read {path} ({err}); opening as a new file");
                (Some(path), String::new())
            }
        },
        None => (None, String::new()),
    }
}
