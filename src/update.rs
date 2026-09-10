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
/// 注意：只有「碰外部世界」的事才放这里（退出进程、写磁盘）；
/// 切换模式是纯状态变化，update 里直接调 `app.set_mode(...)` 即可，不需要进 Action。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 退出程序
    Quit,
    /// 把当前内容保存到文件
    Save,
    /// 保存并退出（:wq）
    SaveAndQuit,
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

/// 处理鼠标左键点击，把屏幕坐标换算成缓冲区里的行列坐标。
pub fn handle_mouse(app: &mut App, mouse: MouseEvent, view_h: usize, view_w: usize) {
    if app.mode == EditorMode::Command || mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return;
    }

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
        ["delete", "single", "line", x] => {
            delete_single_line(app, *x);
            None
        }
        ["delete", "multiline", start, last] => {
            delete_multiline(app, *start, *last);
            None
        }
        // delete 前缀写对了但参数个数不对 → 提示用法
        ["delete", ..] => {
            app.set_status("Usage: delete single line <x> | delete multiline <start> <last>");
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

/// 处理 `:delete single line x`：删除第 x 行（1 基）。
fn delete_single_line(app: &mut App, x: &str) {
    let Some(line) = parse_1based(x) else {
        app.set_status(format!("Invalid line number: {x}"));
        return;
    };

    if app.buffer.delete_line(line) {
        app.buffer.ensure_nonempty(); // 删光后保留一个空行
        app.set_status(format!("Deleted line {}", line + 1));
    } else {
        app.set_status(format!(
            "Line out of range: file has only {} lines",
            app.buffer.line_count()
        ));
    }
}

/// 处理 `:delete multiline start last`：删除 start..=last 这些行（1 基，含两端）。
///
/// 内部用 `Buffer::delete_lines(start, count)` 一次删整段，
/// 避免「删一行后下标前移」导致删错行。
fn delete_multiline(app: &mut App, start: &str, last: &str) {
    let (Some(start), Some(last)) = (parse_1based(start), parse_1based(last)) else {
        app.set_status(format!("Invalid line number: {start} {last}"));
        return;
    };

    if start > last {
        app.set_status("Usage: delete multiline <start> <last>, start must be <= last");
        return;
    }

    let count = last - start + 1; // [start, last] 含两端
    if app.buffer.delete_lines(start, count) {
        app.buffer.ensure_nonempty(); // 删光后保留一个空行
        app.set_status(format!("Deleted lines {} to {}", start + 1, last + 1));
    } else {
        app.set_status(format!(
            "Line out of range: file has only {} lines",
            app.buffer.line_count()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, handle};
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
        app.command_input = "delete single line 2".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, None);
        assert_eq!(app.buffer.line(0), Some("a"));
        assert_eq!(app.buffer.line(1), Some("c"));
        assert!(app.status_message.contains("Deleted line 2"));
    }

    #[test]
    fn command_delete_multiline_removes_contiguous_rows() {
        let mut app = App::from_content(None, "a\nb\nc\nd\ne".to_string());
        app.set_mode(EditorMode::Command);
        // 删第 2..4 行（1 基），即 b、c、d
        app.command_input = "delete multiline 2 4".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, None);
        assert_eq!(app.buffer.line_count(), 2);
        assert_eq!(app.buffer.line(0), Some("a"));
        assert_eq!(app.buffer.line(1), Some("e"));
        assert!(app.status_message.contains("Deleted lines 2 to 4"));
    }

    #[test]
    fn command_delete_multiline_rejects_bad_args() {
        let mut app = App::from_content(None, "a\nb\nc\nd\ne".to_string());
        // start > last
        app.set_mode(EditorMode::Command);
        app.command_input = "delete multiline 4 2".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert!(app.status_message.contains("start must be <= last"));
        // 越界
        app.set_mode(EditorMode::Command);
        app.command_input = "delete multiline 1 99".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert!(app.status_message.contains("out of range"));
    }

    #[test]
    fn command_delete_all_lines_keeps_one_empty_line() {
        let mut app = App::from_content(None, "a\nb".to_string());
        app.set_mode(EditorMode::Command);
        app.command_input = "delete multiline 1 2".to_string();
        run(&mut app, press(KeyCode::Enter));
        // 删光后应保留一个空行（ensure_nonempty）
        assert_eq!(app.buffer.line_count(), 1);
        assert_eq!(app.buffer.line(0), Some(""));
    }
}
