//! 按键分发 —— update.rs 的职责
//!
//! 把 event.rs 给的一个按键（KeyEvent）+ 当前 App 状态，
//! 翻译成「状态变化」，必要时返回一个 `Action` 让 main.rs 去执行副作用
//! （保存、退出这类碰文件 / 碰进程的事）。
//!
//! 原则：
//! - 只做「判断和调度」，改动状态一律调 app.rs 已有的方法
//! - 不亲自碰终端、不读键盘、不写文件
//!
//! ## 命令不在这里
//!
//! `:` 后面敲的那一行归 `commands.rs` 管（「命令说什么」），
//! 这里只管「按了什么键」。命令模式本身只是一条输入管道：
//! 收集字符，回车交给 [`commands::execute`]，仅此而已。

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::app::{App, DocumentKind, EditorMode};
use crate::commands::{self, Action, blocked_by_unsaved_changes, redo_or_report, undo_or_report};

/// 滚轮一格滚动的行数
const WHEEL_LINES: isize = 3;

/// 主入口：根据「当前模式」分发这个按键该干什么。
///
/// - `key`：用户按下的键（来自 `event::Event::Key`）
/// - `view_height` / `view_width`：文本区可见的高/宽，滚动时需要，由 main.rs 传入
///
/// 返回 main 需要执行的副作用，**按顺序**执行。绝大多数按键只产出 0 或 1 个；
/// 只有命令模式的 `&&` 链会产出多个。
pub fn handle_key_event(
    app: &mut App,
    key: KeyEvent,
    view_height: usize,
    view_width: usize,
) -> Vec<Action> {
    // 是否带了 Ctrl / Alt 修饰（Ctrl+q、Alt+i 这类组合，MVP 先一律忽略）
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl_alt = ctrl || alt;
    // 只认「Ctrl + 单键」、不认 Alt 的撤销/重做快捷键
    let is_ctrl = |c: char| ctrl && !alt && key.code == KeyCode::Char(c);

    let mut actions: Vec<Action> = Vec::new();

    match app.mode {
        // ---------- 只读模式：浏览 + 模式的起点 ----------
        EditorMode::ReadOnly => {
            if is_ctrl('r') {
                redo_or_report(app);
            } else if !ctrl_alt {
                match key.code {
                    // q 键：有上一级就返回，没有才退出（`:q` 命令则是直接退出）
                    KeyCode::Char('q') => actions.extend(back_or_quit(app)),
                    KeyCode::Char(':') => app.enter_command_mode(), // 进命令模式
                    // !：进外部命令模式（跟 `:` 平行 —— 两套语言在**按键**这层分开）
                    KeyCode::Char('!') => app.enter_external_mode(),
                    KeyCode::Char('i') => app.set_mode(EditorMode::Edit), // 进编辑模式
                    // Enter：在目录列表里「进入」光标下的条目（普通文件上不做事）
                    KeyCode::Enter => actions.extend(open_entry_under_cursor(app)),
                    // u：撤销一步（类似 vim 的 normal 模式 u）
                    KeyCode::Char('u') => undo_or_report(app),
                    // y：复制当前行到系统剪贴板（暂无选区模型，先做「整行复制」）
                    KeyCode::Char('y') => {
                        actions.push(Action::Copy(app.get_current_line_text()));
                    }
                    // 移动：hjkl 或方向键
                    KeyCode::Char('h') | KeyCode::Left => app.move_cursor_by(0, -1),
                    KeyCode::Char('l') | KeyCode::Right => app.move_cursor_by(0, 1),
                    KeyCode::Char('j') | KeyCode::Down => app.move_cursor_by(1, 0),
                    KeyCode::Char('k') | KeyCode::Up => app.move_cursor_by(-1, 0),
                    _ => {}
                }
            }
        }

        // ---------- 编辑模式：自由输入 ----------
        EditorMode::Edit => {
            if is_ctrl('z') {
                // Ctrl+Z 撤销（编辑模式下唯一被接受的 Ctrl 组合）
                undo_or_report(app);
            } else if is_ctrl('y') {
                redo_or_report(app);
            } else if !ctrl_alt {
                match key.code {
                    // 普通字符直接插入（包括 ':'！这里它就是文本）
                    KeyCode::Char(c) => app.insert_char_at_cursor(c),
                    KeyCode::Backspace => app.delete_char_before_cursor(),
                    KeyCode::Delete => app.delete_char_at_cursor(),
                    KeyCode::Enter => app.split_line_at_cursor(),
                    // 编辑模式里移动只认方向键（hjkl 是字母，会打进文本）
                    KeyCode::Left => app.move_cursor_by(0, -1),
                    KeyCode::Right => app.move_cursor_by(0, 1),
                    KeyCode::Down => app.move_cursor_by(1, 0),
                    KeyCode::Up => app.move_cursor_by(-1, 0),
                    // Tab：插入缩进（空格数来自配置，可用 :set tabwidth 修改）
                    KeyCode::Tab => {
                        for _ in 0..app.config.tab_width {
                            app.insert_char_at_cursor(' ');
                        }
                    }
                    KeyCode::Esc => app.set_mode(EditorMode::ReadOnly), // 回只读
                    _ => {}
                }
            }
        }

        // ---------- 底部输入行：`:` 命令 / `!` 外部命令 ----------
        // 两者**共用一套收集**，差别只有回车那一步：
        // 命令模式把这一行当编辑器的话解析；外部命令模式把终端让出去、整行交给 shell。
        EditorMode::Command | EditorMode::External => match key.code {
            KeyCode::Char(c) if !ctrl_alt => app.command_input.push(c),
            KeyCode::Backspace => {
                app.command_input.pop();
            }
            KeyCode::Esc => app.cancel_command_mode(), // 取消，回只读
            KeyCode::Enter => {
                actions = if app.mode == EditorMode::External {
                    external_command(app)
                } else {
                    execute_command(app)
                };
            }
            _ => {}
        },
    }

    // 收尾：无论刚做了什么，都保证光标合法且在可视区内
    app.clamp_cursor_to_buffer();
    app.scroll_viewport_to_keep_cursor_visible(view_height, view_width);

    actions
}

/// 处理终端粘贴：bracketed paste 会把整段剪贴板文本聚合成**一个**
/// `Event::Paste(String)`，内容就是这个 `text`。
///
/// 分发规则：
/// - 编辑模式：调用 `App::paste` 一次写入整段（换行会被正确地变成多行）；
/// - 只读模式：不写入，只提示——避免“以为在浏览却改了内容”；
/// - 命令模式：忽略（粘贴内容进命令输入意义不大）。
pub fn handle_paste_event(app: &mut App, text: &str, view_height: usize, view_width: usize) {
    match app.mode {
        EditorMode::Edit => {
            let char_count = text.chars().count();
            app.paste_text_at_cursor(text);
            app.set_status_message(format!("Pasted {char_count} chars"));
            app.clamp_cursor_to_buffer();
            app.scroll_viewport_to_keep_cursor_visible(view_height, view_width);
        }
        EditorMode::ReadOnly => app.set_status_message("Read-only: press i to edit, then paste"),
        // 底部那一行的粘贴暂时不做（两边一致）
        EditorMode::Command | EditorMode::External => {}
    }
}

/// 处理鼠标左键点击，把屏幕坐标换算成缓冲区里的行列坐标。
pub fn handle_mouse_event(app: &mut App, mouse: MouseEvent, view_height: usize, view_width: usize) {
    // 底部在收集输入时不接受鼠标 —— 点一下跑光标会让人莫名其妙
    if matches!(app.mode, EditorMode::Command | EditorMode::External) {
        return;
    }

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // 文本区顶部和左侧各有 1 格边框；底部两行不属于文本区。
            let text_row = mouse.row as usize;
            // 第 0 行是上边框，正文是 1..=view_height
            if text_row == 0 || text_row > view_height {
                return;
            }
            let gutter_width = if app.config.show_line_numbers {
                app.buffer.get_line_count().to_string().len() + 1
            } else {
                0
            };
            let text_col = mouse.column as usize;
            if text_col < gutter_width + 1 {
                return;
            }

            let row = (text_row - 1 + app.viewport.top).min(app.buffer.get_line_count() - 1);
            let cell = text_col - 1 - gutter_width;
            let abs_cell = cell + app.viewport.left;
            let col = app.buffer.get_char_at_cell(row, abs_cell);
            // 点击 = 光标跳转，属于「编辑中断」：断开撤销步合并
            app.break_undo_group();
            app.cursor.row = row;
            app.cursor.col = col;
            app.clamp_cursor_to_buffer();
            app.scroll_viewport_to_keep_cursor_visible(view_height, view_width);
        }
        MouseEventKind::ScrollDown => {
            app.move_viewport_by(WHEEL_LINES, 0, view_height, view_width);
        }
        MouseEventKind::ScrollUp => {
            app.move_viewport_by(-WHEEL_LINES, 0, view_height, view_width);
        }
        MouseEventKind::ScrollRight => {
            app.move_viewport_by(0, WHEEL_LINES, view_height, view_width);
        }
        MouseEventKind::ScrollLeft => {
            app.move_viewport_by(0, -WHEEL_LINES, view_height, view_width);
        }
        _ => {}
    }
}

/// 底部那一行的收尾：清空输入并回只读。
///
/// **除非命令自己把模式切走了**（`:insert` 已经进了编辑模式），那就不动 ——
/// 否则会把它的切换又掰回去。
fn finish_input_line(app: &mut App) {
    app.command_input.clear();
    if matches!(app.mode, EditorMode::Command | EditorMode::External) {
        app.set_mode(EditorMode::ReadOnly);
    }
}

/// 执行命令模式里收集到的命令字符串，返回 main 要依次执行的动作。
///
/// 自己只剩「搬输入 + 收尾」这点活 —— 解析和执行全在 `commands.rs`，
/// `&&` 链也是那边拆的。之所以要先把输入拷成独立的 `String`：
/// 后面要 `&mut` 借用 `app`，若让 [`commands::run`] 的结果蹭着
/// `app.command_input` 活就会借用冲突。
fn execute_command(app: &mut App) -> Vec<Action> {
    let cmd = app.command_input.trim().to_string();
    let actions = commands::run(app, &cmd);
    finish_input_line(app);
    actions
}

/// 外部命令模式回车：把这一行原样交给 main 去让位。
///
/// ⚠️ **一个词都不分。** 我们不 `lex`、不查表、不管引号 —— 那都是 shell 的事。
/// 这正是「两套语言在按键那一层分开」换来的好处：这里没有「这行归谁」的判断，
/// 因为**归谁在按 `:` 还是按 `!` 的那一刻就已经定了**。
fn external_command(app: &mut App) -> Vec<Action> {
    let line = app.command_input.trim().to_string();
    finish_input_line(app);
    if line.is_empty() {
        return Vec::new(); // 敲了个 `!` 又直接回车：什么都不做，也不骂人
    }
    vec![Action::RunExternal(line)]
}

// ---------- 按键侧的导航（命令侧的导航在 commands.rs） ----------

/// 只读模式按 `q`：**优先回上一个文档，没有上一个才退出程序**（ranger 式）。
///
/// ⚠️ 这跟 `:q` **命令**不一样 —— `:q` 永远退出，`q` 键则智能返回。
/// 两套语言是刻意的：按键要手感（一个键走遍全树），命令要确定性
/// （写进 `&&` 里不该有歧义）。详见 `COMMANDS.md`。
///
/// 「退出」那一支故意**不做**脏检查 —— 那是用户主动放弃，而且他已经在最早打开的
/// 那一层了，按下去就应该能走（这是当初定下的行为，保留）。
fn back_or_quit(app: &mut App) -> Option<Action> {
    let Some(previous) = app.documents.previous_path() else {
        return Some(Action::Quit); // 没有上一级了 → 退出程序
    };
    if blocked_by_unsaved_changes(app) {
        return None;
    }
    Some(Action::OpenPath(previous))
}

/// 只读模式按 Enter：当前若是目录列表，就打开光标所在行的那个条目。
///
/// ⚠️ **这里故意不解析路径**，只把「列表里那一行的名字」交出去。
/// 解析统一在 `main::open_path` 里做一次（按 `App::current_directory()` 那个基准）——
/// 跟 `:open <名字>` 走的是完全同一条路。
/// 以前这里自己拼了个 `Path::new(base).join(entry)`，于是「同一件事」又变回两套实现：
/// 将来 `file_io::full_path_in` 要是加了什么（比如 `.`/`..` 的规范化），这条不会跟着变。
fn open_entry_under_cursor(app: &mut App) -> Option<Action> {
    if app.kind != DocumentKind::DirectoryListing {
        return None;
    }
    let entry = app.get_current_line_text().trim().to_string();
    if entry.is_empty() {
        return None; // 空行（列表末尾常有一个）
    }
    if blocked_by_unsaved_changes(app) {
        return None;
    }
    // 子目录在列表里带 `/` 尾巴（见 file_io），摘掉再交出去
    Some(Action::OpenPath(entry.trim_end_matches('/').to_string()))
}

#[cfg(test)]
mod tests {
    use super::{Action, handle_key_event, handle_mouse_event, handle_paste_event};
    use crate::app::{App, Cursor, DocumentKind, EditorMode};
    use crate::config::DEFAULT_TAB_WIDTH;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };

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

    /// 简化调用 handle（给一个固定的可视区尺寸）。
    ///
    /// 取**第一个**动作就够了：绝大多数测试只关心「这一下按键产出了什么」，
    /// 而单个按键最多产出一个（`&&` 链是命令模式的事，用 [`run_all`]）。
    fn run(app: &mut App, key: KeyEvent) -> Option<Action> {
        run_all(app, key).into_iter().next()
    }

    /// 取按键产出的**全部**动作（命令模式的 `&&` 链会有多个）。
    fn run_all(app: &mut App, key: KeyEvent) -> Vec<Action> {
        handle_key_event(app, key, 10, 80)
    }

    /// 构造一个鼠标事件（行列坐标在滚轮测试里用不到）
    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    // ---------- 只读模式 ----------

    #[test]
    fn readonly_q_requests_quit() {
        // 列表里只有它自己（甚至还没 remember 过）→ 没有上一级，q 就是退出
        let mut app = App::new();
        assert_eq!(run(&mut app, press(KeyCode::Char('q'))), Some(Action::Quit));
    }

    #[test]
    fn readonly_q_goes_back_when_there_is_a_previous_document() {
        let mut app = App::from_content(None, "a".to_string());
        app.documents.remember("first.txt");
        app.documents.remember("second.txt"); // 当前在 second

        assert_eq!(
            run(&mut app, press(KeyCode::Char('q'))),
            Some(Action::OpenPath("first.txt".to_string()))
        );
    }

    #[test]
    fn readonly_enter_hands_out_the_entry_name_without_resolving_it() {
        let mut app = App::from_content(Some("D:\\proj".to_string()), "a.txt\nsrc/\n".to_string());
        app.kind = DocumentKind::DirectoryListing;

        let open = |app: &mut App, row: usize| {
            app.cursor = Cursor { row, col: 0 };
            run(app, press(KeyCode::Enter))
        };

        // ⚠️ 交出去的是**名字**，不是拼好的路径 —— 解析统一在 `main::open_path` 做，
        // 跟 `:open a.txt` 走同一条路。这里要是又拼一遍，就变回两套实现了。
        assert_eq!(
            open(&mut app, 0),
            Some(Action::OpenPath("a.txt".to_string()))
        );
        // 子目录在列表里带 `/` 后缀，交出去之前要先摘掉
        assert_eq!(open(&mut app, 1), Some(Action::OpenPath("src".to_string())));
        // 末尾那个空行（"a.txt\nsrc/\n" 的第三行）不该打开任何东西
        assert_eq!(open(&mut app, 2), None);
    }

    #[test]
    fn readonly_enter_does_nothing_in_a_normal_file() {
        let mut app = App::from_content(Some("a.txt".to_string()), "hello".to_string());
        assert_eq!(app.kind, DocumentKind::File);
        assert_eq!(run(&mut app, press(KeyCode::Enter)), None);
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
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("a"));
        assert_eq!(app.cursor.col, 1);
        assert!(app.dirty);
    }

    #[test]
    fn edit_types_multibyte_char() {
        let mut app = App::new();
        app.set_mode(EditorMode::Edit);
        run(&mut app, press(KeyCode::Char('你')));
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("你"));
    }

    #[test]
    fn edit_colon_is_plain_text() {
        // 关键：编辑模式下按 : 是插入文本，而不是进命令模式
        let mut app = App::new();
        app.set_mode(EditorMode::Edit);
        run(&mut app, press(KeyCode::Char(':')));
        assert_eq!(app.mode, EditorMode::Edit);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some(":"));
    }

    #[test]
    fn edit_letters_are_typed_not_movement() {
        let mut app = App::from_content(None, "ab".to_string());
        app.set_mode(EditorMode::Edit);
        run(&mut app, press(KeyCode::Char('h')));
        // h 作为字母被插入
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("hab"));
        assert_eq!(app.cursor.col, 1);
    }

    #[test]
    fn edit_arrow_keys_move_without_typing() {
        let mut app = App::from_content(None, "ab\ncd".to_string());
        app.set_mode(EditorMode::Edit);
        run(&mut app, press(KeyCode::Right));
        assert_eq!(app.cursor.col, 1);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("ab"));
        run(&mut app, press(KeyCode::Down));
        assert_eq!(app.cursor.row, 1);
    }

    #[test]
    fn edit_tab_inserts_indent_spaces() {
        let mut app = App::new();
        app.set_mode(EditorMode::Edit);
        run(&mut app, press(KeyCode::Tab));
        // 期望值跟着 app 的默认值走，避免以后改默认值又漏改测试
        assert_eq!(
            app.buffer.get_line(0).as_deref(),
            Some(" ".repeat(DEFAULT_TAB_WIDTH).as_str())
        );
        assert_eq!(app.cursor.col, DEFAULT_TAB_WIDTH);
        assert!(app.dirty);
    }

    #[test]
    fn tab_uses_configured_width() {
        let mut app = App::new();
        app.set_mode(EditorMode::Edit);
        app.config.tab_width = 2;
        run(&mut app, press(KeyCode::Tab));
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("  "));
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
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("a"));
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("b"));
        assert_eq!(app.cursor.row, 1);
        assert_eq!(app.cursor.col, 0);
    }

    // ---------- 外部命令模式（`!` 让位） ----------

    #[test]
    fn bang_key_enters_external_mode() {
        let mut app = App::new();
        assert_eq!(app.mode, EditorMode::ReadOnly);

        run(&mut app, press(KeyCode::Char('!')));

        assert_eq!(app.mode, EditorMode::External);
        assert!(app.command_input.is_empty(), "进来时不该有残留输入");
    }

    #[test]
    fn external_esc_cancels_and_returns_readonly() {
        let mut app = App::new();
        app.set_mode(EditorMode::External);
        app.command_input = "ls".to_string();

        run(&mut app, press(KeyCode::Esc));

        assert_eq!(app.mode, EditorMode::ReadOnly);
        assert!(app.command_input.is_empty());
    }

    #[test]
    fn external_typing_and_backspace_edit_the_line() {
        let mut app = App::new();
        app.set_mode(EditorMode::External);

        for c in "git status".chars() {
            run(&mut app, press(KeyCode::Char(c)));
        }
        assert_eq!(app.command_input, "git status");
        assert_eq!(app.mode, EditorMode::External, "打字期间不该跑掉");

        run(&mut app, press(KeyCode::Backspace));
        assert_eq!(app.command_input, "git statu");
    }

    /// **这个功能的核心测试**：这一行必须**原封不动**交给 shell。
    ///
    /// 引号、`&&`、`>`、`$VAR` 全是 shell 的语法，我们一个都不许碰。
    /// 要是哪天有人「顺手」把 `commands::lex` 挂上来分词（哪怕只是
    /// `split_whitespace().join(" ")` 这种看起来无害的「规整一下」），
    /// 这条就会红 —— 那正是它存在的意义。
    ///
    /// ⚠️ 载荷里**故意留了连续空格和制表符**：規整的空格串连分词都测不出来
    /// （分词再拼回来是一样的），只有不规则空白才能把「原样」钉死。
    #[test]
    fn external_enter_hands_the_line_over_verbatim() {
        let mut app = App::new();
        app.set_mode(EditorMode::External);
        let line = "git  commit -m \"a  b\"\t&&   echo done > out.txt";
        app.command_input = line.to_string();

        let actions = run_all(&mut app, press(KeyCode::Enter));

        assert_eq!(actions, vec![Action::RunExternal(line.to_string())]);
        assert_eq!(app.mode, EditorMode::ReadOnly, "交出去之后先回只读");
        assert!(app.command_input.is_empty());
    }

    /// 敲了 `!` 又直接回车：什么都不做，**也不该骂人**（没有「未知命令」那种提示）。
    #[test]
    fn external_enter_with_an_empty_line_does_nothing() {
        let mut app = App::new();
        app.set_mode(EditorMode::External);
        app.command_input = "   ".to_string(); // 光敲空格也算空

        let actions = run_all(&mut app, press(KeyCode::Enter));

        assert!(actions.is_empty());
        assert_eq!(app.mode, EditorMode::ReadOnly);
        assert!(app.status_message.is_empty(), "{}", app.status_message);
    }

    /// 让位模式里 `!` 只是一个普通字符 —— **别套娃**。
    #[test]
    fn bang_inside_external_mode_is_just_a_character() {
        let mut app = App::new();
        app.set_mode(EditorMode::External);

        run(&mut app, press(KeyCode::Char('!')));
        run(&mut app, press(KeyCode::Char('!')));

        assert_eq!(app.mode, EditorMode::External);
        assert_eq!(app.command_input, "!!");
    }

    /// 两套语言在**按键**那一层就分开了：`:` 进命令模式，`!` 进外部模式。
    /// 所以同一个字符串在两边的下场完全不同 —— 这条把差别钉住。
    #[test]
    fn the_leading_key_decides_which_language_the_line_belongs_to() {
        // 故意选一行**两个世界都有意义、但含义无关**的命令：
        // `echo` / `ls` 在 shell 里天经地义，在我们这儿是两个不存在的命令。
        let line = "echo && ls";

        // `!` 那边：整行原样出去，一个词都没动
        let mut ext = App::new();
        ext.set_mode(EditorMode::External);
        ext.command_input = line.to_string();
        assert_eq!(
            run_all(&mut ext, press(KeyCode::Enter)),
            vec![Action::RunExternal(line.to_string())]
        );

        // `:` 那边：被拆成两条**编辑器命令**，于是 `echo` 先不认识 → 什么都不做
        let mut cmd = App::new();
        cmd.set_mode(EditorMode::Command);
        cmd.command_input = line.to_string();
        assert_eq!(run_all(&mut cmd, press(KeyCode::Enter)), Vec::new());
        assert!(
            cmd.status_message.contains("Unknown command: echo"),
            "该按编辑器命令报错：{}",
            cmd.status_message
        );
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
    fn command_enter_q_always_quits() {
        // `:q` 是**命令**，要的是确定性：不管有没有上一级，它都退出。
        // 「返回上一级」是 `:back`，智能返回只留给 `q` **按键**。
        let mut app = App::from_content(None, "a".to_string());
        app.documents.remember("first.txt");
        app.documents.remember("second.txt");
        app.set_mode(EditorMode::Command);
        app.command_input = "q".to_string();
        assert_eq!(run(&mut app, press(KeyCode::Enter)), Some(Action::Quit));
    }

    #[test]
    fn command_exit_quits_no_matter_where_you_are() {
        let mut app = App::from_content(None, "a".to_string());
        app.documents.remember("first.txt");
        app.documents.remember("second.txt");
        app.insert_char_at_cursor('x'); // 脏了也一样能走：这是明确放弃

        for input in ["quit", "exit", "q", "qa", "quitall"] {
            app.set_mode(EditorMode::Command);
            app.command_input = input.to_string();
            assert_eq!(
                run(&mut app, press(KeyCode::Enter)),
                Some(Action::Quit),
                "{input}"
            );
        }
    }

    #[test]
    fn force_option_skips_the_unsaved_changes_check() {
        let mut app = App::from_content(None, "a".to_string());
        app.documents.remember("first.txt");
        app.documents.remember("second.txt");
        app.insert_char_at_cursor('x');
        assert!(app.dirty);

        // 用户明确表示「我知道会丢，照做」：长写短写、放前放后都得放行
        for (input, expected) in [
            (
                "open --force other.txt",
                Action::OpenPath("other.txt".to_string()),
            ),
            (
                "open -f other.txt",
                Action::OpenPath("other.txt".to_string()),
            ),
            (
                "open other.txt --force",
                Action::OpenPath("other.txt".to_string()),
            ),
            ("back --force", Action::OpenPath("first.txt".to_string())),
            ("settings --force", Action::Settings),
        ] {
            app.set_mode(EditorMode::Command);
            app.command_input = input.to_string();
            assert_eq!(
                run(&mut app, press(KeyCode::Enter)),
                Some(expected),
                "{input}"
            );
        }
    }

    #[test]
    fn navigation_is_blocked_by_unsaved_changes() {
        let mut app = App::from_content(None, "a".to_string());
        app.documents.remember("first.txt");
        app.documents.remember("second.txt");
        app.insert_char_at_cursor('x');
        assert!(app.dirty);

        // 按键 q
        assert_eq!(run(&mut app, press(KeyCode::Char('q'))), None);
        assert!(
            app.status_message.contains("Unsaved"),
            "{}",
            app.status_message
        );
        // 提示里得告诉用户「有强制这条路」，否则他就卡住了
        assert!(
            app.status_message.contains("--force"),
            "提示该提到 `--force` 强制：{}",
            app.status_message
        );

        // :back / :stbd / :settings 同样该被拦下
        for input in ["back", "stbd other.txt", "settings"] {
            app.set_mode(EditorMode::Command);
            app.command_input = input.to_string();
            assert_eq!(run(&mut app, press(KeyCode::Enter)), None, "{input}");
            assert!(
                app.status_message.contains("Unsaved"),
                "{input} → {}",
                app.status_message
            );
        }
    }

    #[test]
    fn back_and_next_commands_walk_the_document_list() {
        // 列表里没有上一级时：提示，不退程序
        let mut app = App::from_content(None, "a".to_string());
        app.documents.remember("a.txt");
        app.set_mode(EditorMode::Command);
        app.command_input = "back".to_string();
        assert_eq!(run(&mut app, press(KeyCode::Enter)), None);
        assert!(
            app.status_message.contains("first document"),
            "{}",
            app.status_message
        );

        // 站回第一个之后 :next 应该给下一个
        app.documents.remember("b.txt");
        app.documents.remember("a.txt");
        app.set_mode(EditorMode::Command);
        app.command_input = "next".to_string();
        assert_eq!(
            run(&mut app, press(KeyCode::Enter)),
            Some(Action::OpenPath("b.txt".to_string()))
        );

        // 已经在最后一个
        app.documents.remember("b.txt");
        app.set_mode(EditorMode::Command);
        app.command_input = "next".to_string();
        assert_eq!(run(&mut app, press(KeyCode::Enter)), None);
        assert!(
            app.status_message.contains("last document"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn command_ls_shows_the_document_list() {
        let mut app = App::new();
        app.documents.remember("a.txt");
        app.documents.remember("b.txt");
        app.set_mode(EditorMode::Command);
        app.command_input = "ls".to_string();
        run(&mut app, press(KeyCode::Enter));

        assert!(
            app.status_message.contains("1 a.txt"),
            "{}",
            app.status_message
        );
        assert!(
            app.status_message.contains("2 *b.txt"),
            "当前那个应该带 *：{}",
            app.status_message
        );
    }

    #[test]
    fn command_forget_drops_an_entry_from_the_list() {
        let mut app = App::new();
        app.documents.remember("a.txt");
        app.documents.remember("b.txt");
        app.set_mode(EditorMode::Command);
        app.command_input = "forget 1".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert!(
            app.status_message.contains("Forgot a.txt"),
            "{}",
            app.status_message
        );
        assert_eq!(app.documents.len(), 1);

        // 越界 → 提示看 :ls
        app.set_mode(EditorMode::Command);
        app.command_input = "forget 9".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert!(
            app.status_message.contains("No document 9"),
            "{}",
            app.status_message
        );

        // 参数不合法
        app.set_mode(EditorMode::Command);
        app.command_input = "forget x".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert!(
            app.status_message.contains("Usage: forget"),
            "{}",
            app.status_message
        );
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
        assert!(app.config.show_line_numbers);
    }

    #[test]
    fn command_set_tabwidth_changes_indent() {
        let mut app = App::from_content(None, "a\nb".to_string());
        app.set_mode(EditorMode::Command);
        app.command_input = "set tabwidth 2".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, None);
        assert_eq!(app.config.tab_width, 2);
        assert!(app.status_message.contains("Tab width set to 2"));
    }

    #[test]
    fn command_set_tabwidth_rejects_bad_value() {
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        app.command_input = "set tabwidth 0".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert!(app.status_message.contains("Invalid tab width"));
        assert_eq!(app.config.tab_width, DEFAULT_TAB_WIDTH); // 非法输入不改动原值
    }

    #[test]
    fn command_set_scrolloff_changes_margin() {
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        app.command_input = "set scrolloff 5".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, None);
        assert_eq!(app.config.scroll_margin, 5);
        assert!(app.status_message.contains("Scroll margin set to 5"));
    }

    #[test]
    fn command_set_sidescrolloff_changes_margin() {
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        app.command_input = "set sidescrolloff 4".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, None);
        assert_eq!(app.config.side_scroll_margin, 4);
        assert!(app.status_message.contains("Side scroll margin set to 4"));
    }

    #[test]
    fn command_config_lists_current_settings() {
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        app.command_input = "config".to_string();
        let action = run(&mut app, press(KeyCode::Enter));

        assert_eq!(action, None);
        // 摘要来自 Config::describe()
        assert!(
            app.status_message.contains("number=on"),
            "{}",
            app.status_message
        );
        assert!(
            app.status_message.contains("tabwidth=8"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn command_config_path_reports_source() {
        let mut app = App::new();
        app.set_config_source(Some(std::path::PathBuf::from(
            "C:\\tmp\\stbd-settings.toml",
        )));
        app.set_mode(EditorMode::Command);
        app.command_input = "config path".to_string();
        run(&mut app, press(KeyCode::Enter));

        assert!(
            app.status_message.contains("stbd-settings.toml"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn command_config_path_suggests_a_location_when_missing() {
        // 没读过配置文件时，不能只说「没有」，要告诉用户该把文件建在哪
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        app.command_input = "config path".to_string();
        run(&mut app, press(KeyCode::Enter));

        assert!(app.config_path.is_none());
        assert!(
            app.status_message.contains("No config file"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn command_config_misuse_shows_usage() {
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        app.command_input = "config whatever".to_string();
        run(&mut app, press(KeyCode::Enter));

        assert!(
            app.status_message.contains("Usage: config"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn command_settings_asks_main_to_open_the_config_file() {
        // `:settings` 和 `:config edit` 是同一个东西
        for input in ["settings", "config edit"] {
            let mut app = App::new();
            app.set_mode(EditorMode::Command);
            app.command_input = input.to_string();
            let action = run(&mut app, press(KeyCode::Enter));
            assert_eq!(action, Some(Action::Settings), "{input}");
        }
    }

    #[test]
    fn command_settings_with_extra_args_shows_usage() {
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        app.command_input = "settings now".to_string();
        run(&mut app, press(KeyCode::Enter));

        assert!(
            app.status_message.contains("Usage: settings"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn command_config_reload_asks_main_to_reread_the_file() {
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        app.command_input = "config reload".to_string();

        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, Some(Action::ReloadConfig));
    }

    #[test]
    fn click_maps_wide_chars_by_cells() {
        let mut app = App::from_content(None, "你好世界".to_string());
        app.config.show_line_numbers = false;
        // 屏幕 (row=1, col=3)：上边框下第一行、左边框右第 3 格 → 绝对第 2 列
        let mut ev = mouse(MouseEventKind::Down(MouseButton::Left));
        ev.row = 1;
        ev.column = 3;
        handle_mouse_event(&mut app, ev, 10, 80);
        assert_eq!(app.cursor.row, 0);
        assert_eq!(app.cursor.col, 1); // “好”是第 2 个字符（0 基为 1）
    }

    #[test]
    fn wheel_scrolls_viewport_without_moving_cursor() {
        let text = (0..30)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let mut app = App::from_content(None, text);
        app.cursor = Cursor { row: 0, col: 0 };
        handle_mouse_event(&mut app, mouse(MouseEventKind::ScrollDown), 10, 80);
        assert_eq!(app.cursor.row, 0); // 滚轮不动光标
        assert_eq!(app.viewport.top, 3); // 视口向下滚 3 行
        handle_mouse_event(&mut app, mouse(MouseEventKind::ScrollUp), 10, 80);
        assert_eq!(app.viewport.top, 0);
        assert_eq!(app.cursor.row, 0);
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
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("  "));
    }

    #[test]
    fn command_swap_swaps_1based_rows() {
        let mut app = App::from_content(None, "a\nb\nc".to_string());
        app.set_mode(EditorMode::Command);
        // 用户看到的是 1 基：交换“第 1 行”和“第 3 行”
        app.command_input = "swap 1 3".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, None);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("c"));
        assert_eq!(app.buffer.get_line(2).as_deref(), Some("a"));
        assert!(app.status_message.contains("Swapped"));
        // 结束后回到只读并清空命令输入
        assert_eq!(app.mode, EditorMode::ReadOnly);
        assert!(app.command_input.is_empty());
    }

    #[test]
    fn command_swap_rejects_bad_args() {
        let mut app = App::from_content(None, "a\nb\nc".to_string());

        // 参数数量不对
        app.set_mode(EditorMode::Command);
        app.command_input = "swap 1".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert!(app.status_message.contains("Usage"));

        // 行号超范围（execute_command 结束后会回到只读，需再次进入命令模式）
        app.set_mode(EditorMode::Command);
        app.command_input = "swap 1 99".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert!(app.status_message.contains("out of range"));
    }

    #[test]
    fn command_tolerates_extra_spaces() {
        // 分词后，中间多余空格不影响识别
        let mut app = App::from_content(None, "a\nb\nc".to_string());

        app.set_mode(EditorMode::Command);
        app.command_input = "swap   1   3".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("c"));
        assert!(app.status_message.contains("Swapped"));

        app.set_mode(EditorMode::Command);
        app.command_input = "set  number".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert!(app.config.show_line_numbers);
    }

    // ---------- 撤销 / 重做（键位与命令） ----------

    #[test]
    fn readonly_u_undoes_last_edit() {
        let mut app = App::from_content(None, "ab".to_string());
        app.set_mode(EditorMode::Edit);
        app.cursor = Cursor { row: 0, col: 2 };
        run(&mut app, press(KeyCode::Char('c')));
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("abc"));

        app.set_mode(EditorMode::ReadOnly);
        run(&mut app, press(KeyCode::Char('u')));
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("ab"));
    }

    #[test]
    fn edit_mode_ctrl_z_undoes_and_ctrl_y_redoes() {
        let mut app = App::from_content(None, "ab".to_string());
        app.set_mode(EditorMode::Edit);
        app.cursor = Cursor { row: 0, col: 2 };
        run(&mut app, press(KeyCode::Char('c')));
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("abc"));

        run(&mut app, ctrl(KeyCode::Char('z')));
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("ab"));

        run(&mut app, ctrl(KeyCode::Char('y')));
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("abc"));
    }

    #[test]
    fn readonly_ctrl_r_redoes() {
        let mut app = App::from_content(None, "ab".to_string());
        app.set_mode(EditorMode::Edit);
        app.cursor = Cursor { row: 0, col: 2 };
        run(&mut app, press(KeyCode::Char('c')));
        app.set_mode(EditorMode::ReadOnly);
        run(&mut app, press(KeyCode::Char('u')));
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("ab"));

        run(&mut app, ctrl(KeyCode::Char('r')));
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("abc"));
    }

    #[test]
    fn command_undo_and_redo() {
        let mut app = App::from_content(None, "ab".to_string());
        app.set_mode(EditorMode::Edit);
        app.cursor = Cursor { row: 0, col: 2 };
        run(&mut app, press(KeyCode::Char('c')));

        app.set_mode(EditorMode::Command);
        app.command_input = "undo".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("ab"));

        app.set_mode(EditorMode::Command);
        app.command_input = "redo".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("abc"));
    }

    #[test]
    fn command_u_alias_undoes() {
        let mut app = App::from_content(None, "ab".to_string());
        app.set_mode(EditorMode::Edit);
        app.cursor = Cursor { row: 0, col: 2 };
        run(&mut app, press(KeyCode::Char('c')));

        app.set_mode(EditorMode::Command);
        app.command_input = "u".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("ab"));
    }

    #[test]
    fn undo_with_nothing_to_undo_reports_it() {
        let mut app = App::from_content(None, "ab".to_string());
        let action = run(&mut app, press(KeyCode::Char('u')));
        assert_eq!(action, None);
        assert!(app.status_message.contains("oldest"));
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("ab"));
    }

    #[test]
    fn command_swap_lines_is_undoable() {
        let mut app = App::from_content(None, "a\nb".to_string());
        app.set_mode(EditorMode::Command);
        app.command_input = "swap 1 2".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("b"));

        app.set_mode(EditorMode::Command);
        app.command_input = "undo".to_string();
        run(&mut app, press(KeyCode::Enter));
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("a"));
    }

    // ---------- 粘贴（bracketed paste） ----------

    #[test]
    fn paste_in_edit_mode_inserts_text() {
        let mut app = App::from_content(None, "ab".to_string());
        app.set_mode(EditorMode::Edit);
        app.cursor = Cursor { row: 0, col: 1 };
        handle_paste_event(&mut app, "XY", 10, 80);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("aXYb"));
        assert!(app.dirty);
    }

    #[test]
    fn paste_multiline_in_edit_mode_creates_rows() {
        let mut app = App::from_content(None, String::new());
        app.set_mode(EditorMode::Edit);
        handle_paste_event(&mut app, "one\r\ntwo", 10, 80);
        assert_eq!(app.buffer.get_line_count(), 2);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("one"));
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("two"));
    }

    #[test]
    fn paste_in_readonly_is_ignored() {
        let mut app = App::from_content(None, "ab".to_string());
        handle_paste_event(&mut app, "XY", 10, 80);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("ab"));
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
        assert_eq!(app.buffer.get_line(1).as_deref(), Some("cd"));
    }

    #[test]
    fn edit_y_is_typed_not_copied() {
        let mut app = App::from_content(None, String::new());
        app.set_mode(EditorMode::Edit);
        let action = run(&mut app, press(KeyCode::Char('y')));
        assert_eq!(action, None);
        assert_eq!(app.buffer.get_line(0).as_deref(), Some("y"));
    }

    // ---------- :copy 命令 ----------

    #[test]
    fn command_copy_single_line() {
        let mut app = App::from_content(None, "ab\ncd\nef".to_string());
        app.set_mode(EditorMode::Command);
        app.command_input = "copy 2".to_string();
        let action = run(&mut app, press(KeyCode::Enter));
        assert_eq!(action, Some(Action::Copy("cd".to_string())));
        // 复制不应改内容
        assert!(!app.dirty);
        assert_eq!(app.buffer.get_line_count(), 3);
    }

    #[test]
    fn command_copy_line_range() {
        let mut app = App::from_content(None, "ab\ncd\nef".to_string());
        app.set_mode(EditorMode::Command);
        app.command_input = "copy 1 2".to_string();
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
        // 起点在终点之后
        app.set_mode(EditorMode::Command);
        app.command_input = "copy 2 1".to_string();
        assert_eq!(run(&mut app, press(KeyCode::Enter)), None);
        assert!(
            app.status_message.contains("out of bounds or reversed"),
            "{}",
            app.status_message
        );
        // 行越界
        app.set_mode(EditorMode::Command);
        app.command_input = "copy 1 99".to_string();
        assert_eq!(run(&mut app, press(KeyCode::Enter)), None);
        assert!(app.status_message.contains("out of bounds"));
    }

    #[test]
    fn command_copy_rejects_malformed_positions() {
        let mut app = App::from_content(None, "ab".to_string());
        // `行:` 少了列号
        app.set_mode(EditorMode::Command);
        app.command_input = "copy 1: 2".to_string();
        assert_eq!(run(&mut app, press(KeyCode::Enter)), None);
        assert!(
            app.status_message.contains("Invalid position"),
            "{}",
            app.status_message
        );
        // 行号是 1 基，0 不合法
        app.set_mode(EditorMode::Command);
        app.command_input = "copy 0 1".to_string();
        assert_eq!(run(&mut app, press(KeyCode::Enter)), None);
        assert!(app.status_message.contains("Invalid position"));
    }

    // ---------- `&&` 链（穿过按键这一层） ----------

    #[test]
    fn a_chain_typed_in_command_mode_yields_every_action_in_order() {
        let mut app = App::from_content(None, "a\nb".to_string());
        app.set_mode(EditorMode::Command);
        app.command_input = "write && wq".to_string();

        // 一个回车产出两个动作，main 会按顺序执行
        assert_eq!(
            run_all(&mut app, press(KeyCode::Enter)),
            vec![Action::Save, Action::SaveAndQuit]
        );
        // 收尾照旧：回只读、清空输入
        assert_eq!(app.mode, EditorMode::ReadOnly);
        assert!(app.command_input.is_empty());
    }

    #[test]
    fn a_chain_that_fails_early_yields_nothing() {
        let mut app = App::from_content(None, "a\nb".to_string());
        app.set_mode(EditorMode::Command);
        app.command_input = "copy 1 99 && delete 1 99".to_string();

        assert!(run_all(&mut app, press(KeyCode::Enter)).is_empty());
        assert_eq!(app.buffer.get_line_count(), 2); // 删没跑
    }
}
