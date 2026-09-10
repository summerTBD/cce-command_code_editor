//! 按键分发 —— update.rs 的职责
//!
//! 把 event.rs 给的一个按键（KeyEvent）+ 当前 App 状态，
//! 翻译成「状态变化」，必要时返回一个 `Action` 让 main.rs 去执行副作用
//! （保存、退出这类碰文件 / 碰进程的事）。
//!
//! 原则：
//! - 只做「判断和调度」，改动状态一律调 app.rs 已有的方法
//! - 不亲自碰终端、不读键盘、不写文件

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use unicode_width::UnicodeWidthChar;

use crate::app::{App, EditorMode};

/// update 处理完后，需要 main.rs 去执行的「有副作用」动作。
///
/// 注意：只有「碰外部世界」的事才放这里（退出进程、写磁盘、写剪贴板）；
/// 切换模式是纯状态变化，update 里直接调 `app.set_mode(...)` 即可，不需要进 Action。
///
/// 因为带上了 `Copy(String)`，这里**不能**再 derive `Copy`（String 不是 Copy）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 退出程序
    Quit,
    /// 把当前内容保存到文件
    Save,
    /// 保存并退出（:wq）
    SaveAndQuit,
    /// 把这段文本写入系统剪贴板
    Copy(String),
}

/// 主入口：根据「当前模式」分发这个按键该干什么。
///
/// - `key`：用户按下的键（来自 `event::Event::Key`）
/// - `view_h` / `view_w`：文本区可见的高/宽，滚动时需要，由 main.rs 传入
///
/// 返回 `Some(Action)` 表示需要 main 去执行副作用；返回 `None` 表示只是状态变化。
pub fn handle(app: &mut App, key: KeyEvent, view_h: usize, view_w: usize) -> Option<Action> {
    // 是否带了 Ctrl / Alt 修饰（Ctrl+q、Alt+i 这类组合，MVP 先一律忽略）
    let ctrl_alt =
        key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::ALT);

    let mut action = None;

    match app.mode {
        // ---------- 只读模式：浏览 + 模式的起点 ----------
        EditorMode::ReadOnly => {
            if !ctrl_alt {
                match key.code {
                    KeyCode::Char('q') => action = Some(Action::Quit),
                    KeyCode::Char(':') => app.enter_command(), // 进命令模式
                    KeyCode::Char('i') => app.set_mode(EditorMode::Edit), // 进编辑模式
                    // y：复制当前行到系统剪贴板（暂无选区模型，先做「整行复制」）
                    KeyCode::Char('y') => {
                        action = Some(Action::Copy(app.current_line_text()));
                    }
                    // 移动：hjkl 或方向键
                    KeyCode::Char('h') | KeyCode::Left => app.move_cursor(0, -1),
                    KeyCode::Char('l') | KeyCode::Right => app.move_cursor(0, 1),
                    KeyCode::Char('j') | KeyCode::Down => app.move_cursor(1, 0),
                    KeyCode::Char('k') | KeyCode::Up => app.move_cursor(-1, 0),
                    _ => {}
                }
            }
        }

        // ---------- 编辑模式：自由输入 ----------
        EditorMode::Edit => {
            if !ctrl_alt {
                match key.code {
                    // 普通字符直接插入（包括 ':'！这里它就是文本）
                    KeyCode::Char(c) => app.type_char(c),
                    KeyCode::Backspace => app.backspace(),
                    KeyCode::Delete => app.delete_forward(),
                    KeyCode::Enter => app.insert_newline(),
                    // 编辑模式里移动只认方向键（hjkl 是字母，会打进文本）
                    KeyCode::Left => app.move_cursor(0, -1),
                    KeyCode::Right => app.move_cursor(0, 1),
                    KeyCode::Down => app.move_cursor(1, 0),
                    KeyCode::Up => app.move_cursor(-1, 0),
                    // Tab：插入缩进（空格数来自 app.tab_width，可用 :set tabwidth 修改）
                    KeyCode::Tab => {
                        for _ in 0..app.tab_width {
                            app.type_char(' ');
                        }
                    }
                    KeyCode::Esc => app.set_mode(EditorMode::ReadOnly), // 回只读
                    _ => {}
                }
            }
        }

        // ---------- 命令模式：收集输入，回车执行 ----------
        EditorMode::Command => match key.code {
            KeyCode::Char(c) if !ctrl_alt => app.command_input.push(c),
            KeyCode::Backspace => {
                app.command_input.pop();
            }
            KeyCode::Esc => app.cancel_command(), // 取消命令，回只读
            KeyCode::Enter => action = execute_command(app),
            _ => {}
        },
    }

    // 收尾：无论刚做了什么，都保证光标合法且在可视区内
    app.clamp_cursor();
    app.ensure_cursor_visible(view_h, view_w);

    action
}

/// 处理终端粘贴：bracketed paste 会把整段剪贴板文本聚合成**一个**
/// `Event::Paste(String)`，内容就是这个 `text`。
///
/// 分发规则：
/// - 编辑模式：调用 `App::paste` 一次写入整段（换行会被正确地变成多行）；
/// - 只读模式：不写入，只提示——避免“以为在浏览却改了内容”；
/// - 命令模式：忽略（粘贴内容进命令输入意义不大）。
pub fn handle_paste(app: &mut App, text: &str, view_h: usize, view_w: usize) {
    match app.mode {
        EditorMode::Edit => {
            let char_count = text.chars().count();
            app.paste(text);
            app.set_status(format!("Pasted {char_count} chars"));
            app.clamp_cursor();
            app.ensure_cursor_visible(view_h, view_w);
        }
        EditorMode::ReadOnly => app.set_status("Read-only: press i to edit, then paste"),
        EditorMode::Command => {}
    }
}

/// 处理鼠标左键点击，把屏幕坐标换算成缓冲区里的行列坐标。
pub fn handle_mouse(app: &mut App, mouse: MouseEvent, view_h: usize, view_w: usize) {
    if app.mode == EditorMode::Command {
        return;
    }

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // 文本区顶部和左侧各有 1 格边框；底部两行不属于文本区。
            let text_row = mouse.row as usize;
            if text_row == 0 || text_row >= view_h + 1 {
                return;
            }
            let gutter_width = if app.show_line_numbers {
                app.buffer.line_count().to_string().len() + 1
            } else {
                0
            };
            let text_col = mouse.column as usize;
            if text_col < gutter_width + 1 {
                return;
            }

            let row = (text_row - 1 + app.viewport.top).min(app.buffer.line_count() - 1);
            let cell = text_col - 1 - gutter_width;
            let line = app.buffer.line(row).unwrap_or("");
            let col = char_col_at_cell(line, cell, app.viewport.left);
            app.cursor.row = row;
            app.cursor.col = col;
            app.clamp_cursor();
            app.ensure_cursor_visible(view_h, view_w);
        }
        MouseEventKind::ScrollDown => {
            app.move_cursor(1, 0);
            app.ensure_cursor_visible(view_h, view_w);
        }
        MouseEventKind::ScrollUp => {
            app.move_cursor(-1, 0);
            app.ensure_cursor_visible(view_h, view_w);
        }
        _ => {}
    }
}

/// 把终端显示列换算成一行中的字符下标。
fn char_col_at_cell(line: &str, cell: usize, viewport_left: usize) -> usize {
    let visible = line.chars().skip(viewport_left);
    let mut used = 0;
    for (index, ch) in visible.enumerate() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        if cell < used + width {
            return viewport_left + index;
        }
        used += width;
    }
    line.chars().count()
}

/// 执行命令模式里收集到的命令字符串，返回需要 main 去做的动作（如果有）。
///
/// 这是 MVP 的极简实现。
/// 命令先按空白「分词」再匹配，因此多余/缺失的空格都不影响识别，
/// 例如 `set  number`、`swap single   line 1 3` 都能正常解析。
/// 将来命令多了可把「解析」拆成独立函数/独立模块（commands）。
fn execute_command(app: &mut App) -> Option<Action> {
    // 先拷成独立的 String：后面要 &mut 借用 app（切模式、set_status、swap…），
    // 若还让分词结果借用 app.command_input（&str）就会发生借用冲突。
    let cmd = app.command_input.trim().to_string();
    let parts: Vec<&str> = cmd.split_whitespace().collect();

    let action = match parts.as_slice() {
        // ---- 无参数命令 ----
        ["q"] | ["quit"] => Some(Action::Quit),
        ["w"] | ["write"] => Some(Action::Save),
        ["wq"] => Some(Action::SaveAndQuit),
        ["i"] | ["insert"] => {
            // 切模式是纯状态变化，update 直接做，不需要经过 Action/main
            app.set_mode(EditorMode::Edit);
            None
        }
        ["set", "number"] => {
            app.show_line_numbers = true;
            None
        }
        ["set", "nonumber"] => {
            app.show_line_numbers = false;
            None
        }
        ["set", "tabwidth", n] => {
            set_tab_width(app, *n);
            None
        }
        // set 前缀写对了但参数不对 → 提示用法
        ["set", ..] => {
            app.set_status("Usage: set number | set nonumber | set tabwidth <n>");
            None
        }
        // ---- 带参数命令：参数已在模式里逐个取好 ----
        // x/y 在 slice 模式里被绑定成 &&str，这里显式 *x/*y 解一层成 &str
        ["swap", "single", "line", x, y] => {
            swap_single_line(app, *x, *y);
            None
        }
        // swap 前缀写对了但参数个数不对 → 提示用法
        ["swap", "single", "line", ..] => {
            app.set_status("Usage: swap single line <line x> <line y> (1-based)");
            None
        }
        // ---- 删除命令 ----
        // `:delete line <x>` 删一行；`:delete line <start> <last>` 删一段（1 基，含两端）
        ["delete", "line", start] => {
            delete_lines(app, *start, *start);
            None
        }
        ["delete", "line", start, last] => {
            delete_lines(app, *start, *last);
            None
        }

        ["delete", "all"] => {
            delete_all_lines(app);
            None
        }
        // delete 前缀写对了但参数个数不对 → 提示用法
        ["delete", ..] => {
            app.set_status("Usage: delete line <x> | delete line <start> <last> | delete all");
            None
        }
        // ---- 复制命令 ----
        // `:copy line <x>` 复制一行；`:copy line <start> <last>` 复制连续多行（1 基，含两端）
        ["copy", "line", start] => copy_line_range(app, *start, *start),
        ["copy", "line", start, last] => copy_line_range(app, *start, *last),
        // 整份文件
        ["copy", "all"] => {
            let last = app.buffer.line_count().saturating_sub(1);
            copy_range(app, (0, 0), (last, usize::MAX))
        }
        // 精确到坐标：`行:列`，行/列均 1 基，含两端
        ["copy", from, to] => match (parse_row_col(from), parse_row_col(to)) {
            (Some(from), Some(to)) => copy_range(app, from, to),
            _ => {
                app.set_status("Usage: copy <r1>:<c1> <r2>:<c2> (1-based, e.g. copy 2:3 5:7)");
                None
            }
        },
        // copy 前缀写对了但格式不对 → 提示用法
        ["copy", ..] => {
            app.set_status(
                "Usage: copy line <x> | copy line <start> <last> | copy <r1>:<c1> <r2>:<c2> | copy all",
            );
            None
        }

        _ => {
            app.set_status(format!("Unknown command: {cmd}"));
            None
        }
    };

    // 清空命令输入；只有命令没有把模式切走时（例如 :i 已进编辑），才回到只读
    app.command_input.clear();
    if app.mode == EditorMode::Command {
        app.set_mode(EditorMode::ReadOnly);
    }
    action
}

/// 把「用户看到的 1 基行号」解析成内部 0 基（拒绝 0 / 非数字）。
fn parse_1based(s: &str) -> Option<usize> {
    let n = s.parse::<usize>().ok()?;
    (n >= 1).then_some(n - 1)
}

/// 把 `行:列`（均 1 基）解析成内部 0 基坐标。
fn parse_row_col(s: &str) -> Option<(usize, usize)> {
    let (row, col) = s.split_once(':')?;
    Some((parse_1based(row)?, parse_1based(col)?))
}

/// 处理 `:copy line <x>` / `:copy line <start> <last>`：按行复制（1 基，含两端）。
fn copy_line_range(app: &mut App, start: &str, last: &str) -> Option<Action> {
    let (Some(start), Some(last)) = (parse_1based(start), parse_1based(last)) else {
        app.set_status(format!("Invalid line number: {start} {last}"));
        return None;
    };

    if start > last {
        app.set_status("Usage: copy line <x> | copy line <start> <last>, start must be <= last");
        return None;
    }
    // 列传 usize::MAX：text_range 会把它夹到行末，即「整行」
    copy_range(app, (start, 0), (last, usize::MAX))
}

/// 取一段文本并包成 `Action::Copy`；坐标非法时设置错误提示并返回 None。
fn copy_range(app: &mut App, start: (usize, usize), end: (usize, usize)) -> Option<Action> {
    match app.text_range(start, end) {
        Some(text) => Some(Action::Copy(text)),
        None => {
            app.set_status(format!(
                "Range out of bounds or reversed (file has {} lines)",
                app.buffer.line_count()
            ));
            None
        }
    }
}

/// 处理 `:set tabwidth n`：设置一次 Tab 插入的空格数
fn set_tab_width(app: &mut App, n: &str) {
    match n.parse::<usize>() {
        Ok(w) if (1..=16).contains(&w) => {
            app.tab_width = w;
            app.set_status(format!("Tab width set to {w}"));
        }
        _ => app.set_status("Invalid tab width: use a number from 1 to 16"),
    }
}

/// 交换第 x 行与第 y 行（x/y 已是去掉多余空白后的独立参数，1 基）。
fn swap_single_line(app: &mut App, x: &str, y: &str) {
    let (Some(x), Some(y)) = (parse_1based(x), parse_1based(y)) else {
        app.set_status(format!("Invalid line number: {x} {y}"));
        return;
    };

    if app.buffer.swap_lines(x, y) {
        app.set_status(format!("Swapped lines {} and {}", x + 1, y + 1));
    } else {
        app.set_status(format!(
            "Line out of range: file has only {} lines",
            app.buffer.line_count()
        ));
    }
}

/// 处理 `:delete line <x>` 与 `:delete line <start> <last>`：
/// 删除 start..=last 这些行（1 基，含两端；单行时 start == last）。
///
/// 内部用 `Buffer::delete_lines(start, count)` 一次删整段，
/// 避免「删一行后下标前移」导致删错行。
fn delete_lines(app: &mut App, start: &str, last: &str) {
    let (Some(start), Some(last)) = (parse_1based(start), parse_1based(last)) else {
        app.set_status(format!("Invalid line number: {start} {last}"));
        return;
    };

    if start > last {
        app.set_status(
            "Usage: delete line <x> | delete line <start> <last>, start must be <= last",
        );
        return;
    }

    let count = last - start + 1; // [start, last] 含两端
    if app.buffer.delete_lines(start, count) {
        app.buffer.ensure_nonempty(); // 删光后保留一个空行
        if start == last {
            app.set_status(format!("Deleted line {}", start + 1));
        } else {
            app.set_status(format!("Deleted lines {} to {}", start + 1, last + 1));
        }
    } else {
        app.set_status(format!(
            "Line out of range: file has only {} lines",
            app.buffer.line_count()
        ));
    }
}

/// 处理 `:delete all`：删除全部内容，但保留一个空行。
fn delete_all_lines(app: &mut App) {
    let last = app.buffer.line_count().to_string();
    delete_lines(app, "1", &last);
}

#[cfg(test)]
mod tests {
    use super::{Action, handle, handle_paste};
    use crate::app::{App, Cursor, EditorMode};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    /// 模拟一次普通按键（无修饰、Press）
    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// 模拟带 Ctrl 的按键
    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// 简化调用 handle（给一个固定的可视区尺寸）
    fn run(app: &mut App, key: KeyEvent) -> Option<Action> {
        handle(app, key, 10, 80)
    }

    // ---------- 只读模式 ----------

    #[test]
    fn readonly_q_requests_quit() {
        let mut app = App::new();
        assert_eq!(run(&mut app, press(KeyCode::Char('q'))), Some(Action::Quit));
    }

    #[test]
    fn readonly_ctrl_q_is_ignored() {
        let mut app = App::new();
        assert_eq!(run(&mut app, ctrl(KeyCode::Char('q'))), None);
        assert_eq!(app.mode, EditorMode::ReadOnly);
    }

    #[test]
    fn readonly_colon_enters_command_mode() {
        let mut app = App::new();
        assert!(run(&mut app, press(KeyCode::Char(':'))).is_none());
        assert_eq!(app.mode, EditorMode::Command);
        assert!(app.command_input.is_empty());
    }

    #[test]
    fn readonly_i_enters_edit_mode() {
        let mut app = App::new();
        assert!(run(&mut app, press(KeyCode::Char('i'))).is_none());
        assert_eq!(app.mode, EditorMode::Edit);
    }

    #[test]
    fn readonly_hjkl_move_cursor() {
        let mut app = App::from_content(None, "ab\ncd".to_string());
        app.cursor = Cursor { row: 0, col: 1 };
        run(&mut app, press(KeyCode::Char('h')));
        assert_eq!(app.cursor.col, 0);
        run(&mut app, press(KeyCode::Char('j')));
        assert_eq!(app.cursor.row, 1);
        run(&mut app, press(KeyCode::Char('l')));
        assert_eq!(app.cursor.col, 1);
        run(&mut app, press(KeyCode::Char('k')));
        assert_eq!(app.cursor.row, 0);
    }

    // ---------- 编辑模式 ----------

    #[test]
    fn edit_types_normal_char() {
        let mut app = App::new();
        app.set_mode(EditorMode::Edit);
        run(&mut app, press(KeyCode::Char('a')));
        assert_eq!(app.buffer.line(0), Some("a"));
        assert_eq!(app.cursor.col, 1);
        assert!(app.dirty);
    }

    #[test]
    fn edit_types_multibyte_char() {
        let mut app = App::new();
        app.set_mode(EditorMode::Edit);
        run(&mut app, press(KeyCode::Char('你')));
        assert_eq!(app.buffer.line(0), Some("你"));
    }

    #[test]
    fn edit_colon_is_plain_text() {
        // 关键：编辑模式下按 : 是插入文本，而不是进命令模式
        let mut app = App::new();
        app.set_mode(EditorMode::Edit);
        run(&mut app, press(KeyCode::Char(':')));
        assert_eq!(app.mode, EditorMode::Edit);
        assert_eq!(app.buffer.line(0), Some(":"));
    }

    #[test]
    fn edit_letters_are_typed_not_movement() {
        let mut app = App::from_content(None, "ab".to_string());
        app.set_mode(EditorMode::Edit);
        run(&mut app, press(KeyCode::Char('h')));
        // h 作为字母被插入
        assert_eq!(app.buffer.line(0), Some("hab"));
        assert_eq!(app.cursor.col, 1);
    }

    #[test]
    fn edit_arrow_keys_move_without_typing() {
        let mut app = App::from_content(None, "ab\ncd".to_string());
        app.set_mode(EditorMode::Edit);
        run(&mut app, press(KeyCode::Right));
        assert_eq!(app.cursor.col, 1);
        assert_eq!(app.buffer.line(0), Some("ab"));
        run(&mut app, press(KeyCode::Down));
        assert_eq!(app.cursor.row, 1);
    }

    #[test]
    fn edit_tab_inserts_indent_spaces() {
        let mut app = App::new();
        app.set_mode(EditorMode::Edit);
        run(&mut app, press(KeyCode::Tab));
        assert_eq!(app.buffer.line(0), Some("    "));
        assert_eq!(app.cursor.col, 4);
        assert!(app.dirty);
    }

    #[test]
    fn tab_uses_configured_width() {
        let mut app = App::new();
        app.set_mode(EditorMode::Edit);
        app.tab_width = 2;
        run(&mut app, press(KeyCode::Tab));
        assert_eq!(app.buffer.line(0), Some("  "));
        assert_eq!(app.cursor.col, 2);
    }

    #[test]
    fn edit_esc_returns_to_readonly() {
        let mut app = App::new();
        app.set_mode(EditorMode::Edit);
        run(&mut app, press(KeyCode::Esc));
        assert_eq!(app.mode, EditorMode::ReadOnly);
    }

    #[test]
    fn edit_enter_splits_line() {
        let mut app = App::from_content(None, "ab".to_string());
        app.set_mode(EditorMode::Edit);
        app.cursor = Cursor { row: 0, col: 1 };
        run(&mut app, press(KeyCode::Enter));
        assert_eq!(app.buffer.line(0), Some("a"));
        assert_eq!(app.buffer.line(1), Some("b"));
        assert_eq!(app.cursor.row, 1);
        assert_eq!(app.cursor.col, 0);
    }

    // ---------- 命令模式 ----------

    #[test]
    fn command_typing_collects_input() {
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        run(&mut app, press(KeyCode::Char('w')));
        assert_eq!(app.command_input, "w");
        assert_eq!(app.mode, EditorMode::Command);
    }

    #[test]
    fn command_backspace_removes_last_char() {
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        app.command_input = "wq".to_string();
        run(&mut app, press(KeyCode::Backspace));
        assert_eq!(app.command_input, "w");
    }

    #[test]
    fn command_esc_cancels_and_returns_readonly() {
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        app.command_input = "w".to_string();
        run(&mut app, press(KeyCode::Esc));
        assert_eq!(app.mode, EditorMode::ReadOnly);
        assert!(app.command_input.is_empty());
    }

    #[test]
    fn command_enter_w_requests_save() {
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        app.command_input = "w".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, Some(Action::Save));
        assert_eq!(app.mode, EditorMode::ReadOnly);
        assert!(app.command_input.is_empty());
    }

    #[test]
    fn command_enter_q_requests_quit() {
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        app.command_input = "quit".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, Some(Action::Quit));
    }

    #[test]
    fn command_enter_wq_requests_save_and_quit() {
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        app.command_input = "wq".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, Some(Action::SaveAndQuit));
        // 依然会退出命令模式、清空输入
        assert_eq!(app.mode, EditorMode::ReadOnly);
        assert!(app.command_input.is_empty());
    }

    #[test]
    fn command_unknown_shows_status() {
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        app.command_input = "hahaha".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, None);
        assert!(app.status_message.contains("Unknown command"));
    }

    #[test]
    fn command_set_number_toggles_line_numbers() {
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        app.command_input = "set number".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, None);
        assert!(app.show_line_numbers);
    }

    #[test]
    fn command_set_tabwidth_changes_indent() {
        let mut app = App::from_content(None, "a\nb".to_string());
        app.set_mode(EditorMode::Command);
        app.command_input = "set tabwidth 2".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, None);
        assert_eq!(app.tab_width, 2);
        assert!(app.status_message.contains("Tab width set to 2"));
    }

    #[test]
    fn command_set_tabwidth_rejects_bad_value() {
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        app.command_input = "set tabwidth 0".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert!(app.status_message.contains("Invalid tab width"));
        assert_eq!(app.tab_width, 4); // 非法输入不改动原值
    }

    #[test]
    fn command_tabwidth_then_tab_uses_new_width() {
        let mut app = App::new();
        // 先用命令改成 2
        app.set_mode(EditorMode::Command);
        app.command_input = "set tabwidth 2".to_string();
        run(&mut app, press(KeyCode::Enter));
        // 再进编辑模式按 Tab
        app.set_mode(EditorMode::Edit);
        run(&mut app, press(KeyCode::Tab));
        assert_eq!(app.buffer.line(0), Some("  "));
    }

    #[test]
    fn command_swap_single_line_swaps_1based_rows() {
        let mut app = App::from_content(None, "a\nb\nc".to_string());
        app.set_mode(EditorMode::Command);
        // 用户看到的是 1 基：交换“第 1 行”和“第 3 行”
        app.command_input = "swap single line 1 3".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, None);
        assert_eq!(app.buffer.line(0), Some("c"));
        assert_eq!(app.buffer.line(2), Some("a"));
        assert!(app.status_message.contains("Swapped"));
        // 结束后回到只读并清空命令输入
        assert_eq!(app.mode, EditorMode::ReadOnly);
        assert!(app.command_input.is_empty());
    }

    #[test]
    fn command_swap_single_line_rejects_bad_args() {
        let mut app = App::from_content(None, "a\nb\nc".to_string());

        // 参数数量不对
        app.set_mode(EditorMode::Command);
        app.command_input = "swap single line 1".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert!(app.status_message.contains("Usage"));

        // 行号超范围（execute_command 结束后会回到只读，需再次进入命令模式）
        app.set_mode(EditorMode::Command);
        app.command_input = "swap single line 1 99".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert!(app.status_message.contains("out of range"));
    }

    #[test]
    fn command_tolerates_extra_spaces() {
        // 分词后，中间多余空格不影响识别
        let mut app = App::from_content(None, "a\nb\nc".to_string());

        app.set_mode(EditorMode::Command);
        app.command_input = "swap   single  line   1   3".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert_eq!(app.buffer.line(0), Some("c"));
        assert!(app.status_message.contains("Swapped"));

        app.set_mode(EditorMode::Command);
        app.command_input = "set  number".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert!(app.show_line_numbers);
    }

    #[test]
    fn command_delete_single_line_removes_one_row() {
        let mut app = App::from_content(None, "a\nb\nc".to_string());
        app.set_mode(EditorMode::Command);
        app.command_input = "delete line 2".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, None);
        assert_eq!(app.buffer.line(0), Some("a"));
        assert_eq!(app.buffer.line(1), Some("c"));
        assert!(app.status_message.contains("Deleted line 2"));
    }

    #[test]
    fn command_delete_lines_removes_contiguous_rows() {
        let mut app = App::from_content(None, "a\nb\nc\nd\ne".to_string());
        app.set_mode(EditorMode::Command);
        // 删第 2..4 行（1 基），即 b、c、d
        app.command_input = "delete line 2 4".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, None);
        assert_eq!(app.buffer.line_count(), 2);
        assert_eq!(app.buffer.line(0), Some("a"));
        assert_eq!(app.buffer.line(1), Some("e"));
        assert!(app.status_message.contains("Deleted lines 2 to 4"));
    }

    #[test]
    fn command_delete_lines_rejects_bad_args() {
        let mut app = App::from_content(None, "a\nb\nc\nd\ne".to_string());
        // start > last
        app.set_mode(EditorMode::Command);
        app.command_input = "delete line 4 2".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert!(app.status_message.contains("start must be <= last"));
        // 越界
        app.set_mode(EditorMode::Command);
        app.command_input = "delete line 1 99".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert!(app.status_message.contains("out of range"));
    }

    #[test]
    fn command_delete_all_lines_keeps_one_empty_line() {
        let mut app = App::from_content(None, "a\nb".to_string());
        app.set_mode(EditorMode::Command);
        app.command_input = "delete all".to_string();
        run(&mut app, press(KeyCode::Enter));
        // 删光后应保留一个空行（ensure_nonempty）
        assert_eq!(app.buffer.line_count(), 1);
        assert_eq!(app.buffer.line(0), Some(""));
    }

    #[test]
    fn command_delete_all_single_line_keeps_one_empty_line() {
        let mut app = App::from_content(None, "only".to_string());
        app.set_mode(EditorMode::Command);
        app.command_input = "delete all".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert_eq!(app.buffer.line_count(), 1);
        assert_eq!(app.buffer.line(0), Some(""));
    }

    // ---------- 粘贴（bracketed paste） ----------

    #[test]
    fn paste_in_edit_mode_inserts_text() {
        let mut app = App::from_content(None, "ab".to_string());
        app.set_mode(EditorMode::Edit);
        app.cursor = Cursor { row: 0, col: 1 };
        handle_paste(&mut app, "XY", 10, 80);
        assert_eq!(app.buffer.line(0), Some("aXYb"));
        assert!(app.dirty);
    }

    #[test]
    fn paste_multiline_in_edit_mode_creates_rows() {
        let mut app = App::from_content(None, String::new());
        app.set_mode(EditorMode::Edit);
        handle_paste(&mut app, "one\r\ntwo", 10, 80);
        assert_eq!(app.buffer.line_count(), 2);
        assert_eq!(app.buffer.line(0), Some("one"));
        assert_eq!(app.buffer.line(1), Some("two"));
    }

    #[test]
    fn paste_in_readonly_is_ignored() {
        let mut app = App::from_content(None, "ab".to_string());
        handle_paste(&mut app, "XY", 10, 80);
        assert_eq!(app.buffer.line(0), Some("ab"));
        assert!(!app.dirty);
        assert!(app.status_message.contains("Read-only"));
    }

    // ---------- 复制（只读模式 y） ----------

    #[test]
    fn readonly_y_requests_copy_of_current_line() {
        let mut app = App::from_content(None, "ab\ncd".to_string());
        app.cursor = Cursor { row: 1, col: 0 };
        let action = run(&mut app, press(KeyCode::Char('y')));
        assert_eq!(action, Some(Action::Copy("cd".to_string())));
        // 复制是只读操作，不该改内容
        assert!(!app.dirty);
        assert_eq!(app.buffer.line(1), Some("cd"));
    }

    #[test]
    fn edit_y_is_typed_not_copied() {
        let mut app = App::from_content(None, String::new());
        app.set_mode(EditorMode::Edit);
        let action = run(&mut app, press(KeyCode::Char('y')));
        assert_eq!(action, None);
        assert_eq!(app.buffer.line(0), Some("y"));
    }

    // ---------- :copy 命令 ----------

    #[test]
    fn command_copy_single_line() {
        let mut app = App::from_content(None, "ab\ncd\nef".to_string());
        app.set_mode(EditorMode::Command);
        app.command_input = "copy line 2".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, Some(Action::Copy("cd".to_string())));
        // 复制不应改内容
        assert!(!app.dirty);
        assert_eq!(app.buffer.line_count(), 3);
    }

    #[test]
    fn command_copy_line_range() {
        let mut app = App::from_content(None, "ab\ncd\nef".to_string());
        app.set_mode(EditorMode::Command);
        app.command_input = "copy line 1 2".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, Some(Action::Copy("ab\ncd".to_string())));
    }

    #[test]
    fn command_copy_exact_coordinates() {
        let mut app = App::from_content(None, "abcd\nefgh".to_string());
        app.set_mode(EditorMode::Command);
        app.command_input = "copy 1:2 2:2".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        // 第1行第2列起 → "bcd"，接第2行到第2列 → "ef"
        assert_eq!(action, Some(Action::Copy("bcd\nef".to_string())));
    }

    #[test]
    fn command_copy_all() {
        let mut app = App::from_content(None, "ab\ncd".to_string());
        app.set_mode(EditorMode::Command);
        app.command_input = "copy all".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, Some(Action::Copy("ab\ncd".to_string())));
    }

    #[test]
    fn command_copy_rejects_bad_range() {
        let mut app = App::from_content(None, "ab\ncd".to_string());
        // start > last
        app.set_mode(EditorMode::Command);
        app.command_input = "copy line 2 1".to_string();
        assert_eq!(run(&mut app, press(KeyCode::Enter)), None);
        assert!(app.status_message.contains("start must be <= last"));
        // 行越界
        app.set_mode(EditorMode::Command);
        app.command_input = "copy line 1 99".to_string();
        assert_eq!(run(&mut app, press(KeyCode::Enter)), None);
        assert!(app.status_message.contains("out of bounds"));
    }

    #[test]
    fn command_copy_rejects_malformed_coordinates() {
        let mut app = App::from_content(None, "ab".to_string());
        app.set_mode(EditorMode::Command);
        app.command_input = "copy 1 2".to_string(); // 缺少 `行:列`
        assert_eq!(run(&mut app, press(KeyCode::Enter)), None);
        assert!(app.status_message.contains("Usage: copy"));
    }
}
