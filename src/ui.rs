//! 界面渲染 —— ui.rs 的职责
//!
//! 只做一件事：**读 `App` 状态 → 在终端上画出来**。
//! 它不读按键、不改状态、不做文件 I/O。
//!
//! 布局（自上而下）：
//! ```text
//! 1 int main(void) { return 0; }     ← 文本区：**没有边框**，占满宽度
//!  2
//!  3
//!  D:\tmp\probe\broken.c  -- READ-ONLY --  q back/quit | : command …
//!  Saved broken.c
//! ```
//!
//! ## ⚠️ 为什么没有边框
//!
//! 原来是 Rounded 圆角框，标题画在上边框上。去掉的理由是**和 nvim 一比就出来了**：
//! 框的四条线是纯粹的装饰，而它占掉的正是最值钱的地方 —— 上下各一行，左右各一格。
//! 终端的行数本来就稀缺，拿两行去画两条横线很亏。
//!
//! 代价是文件名没了住处（它原先是标题）。新的家是**底栏**，跟 nvim 的 statusline
//! 一个位置：那里本来就有一行，只是原来只放模式提示。最左是文件名，因为
//! 窄终端截断时**右边先没** —— 而「我在看哪个文件」是这一行里最不能丢的。
//!
//! 行号栏现在直接从**屏幕第 0 列**开始（原来被左边框挤到第 1 列）。

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, DocumentKind, EditorMode};
use crate::diagnostic::{Diagnostic, Severity};
use crate::outbox::OutFile;

/// 主入口：把整个终端纵向切成「文本区 + 底部栏」
pub fn render_ui(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_text_area(frame, app, chunks[0]);
    // 底部占两行、竖着排：第 1 行 = 命令输入 / 模式提示，第 2 行 = 状态信息
    render_bottom_bar(frame, app, chunks[1]);
    render_status_line(frame, app, chunks[2]);
}

/// 中间文本区的那一行**左侧标签** —— 也就是 nvim statusline 里放文件名的那个位置。
///
/// 它原来是画在顶边框上的「标题」，边框去掉之后就搬到这儿了。
///
/// 三种情况：
/// - 普通文件：文件名，改过没保存时末尾带 `*`
/// - 虚拟清单：**屏幕上那份文本不是这个文件名所指的东西** —— 说清「这是谁的什么」，
///   以及「它落在哪个输出文件里」（清单会同时写进输出文件夹，见 `outbox.rs`）
/// - 还没打开文件：`untitled`
///
/// ⚠️ **不要加 `STBD · ` 这类前缀**。这个是 nvim 的 statusline，不是标题栏；
/// 占掉的每一格都是窄终端里最先被截掉的东西，而「我在看哪个文件」比「这是谁的程序」
/// 要紧得多。程序名在 `:lsp` 那些地方出现已经够了。
fn document_label(app: &App) -> String {
    let file_name = app.file_path.as_deref().unwrap_or("untitled");
    let dirty_mark = if app.dirty { "*" } else { "" };
    match app.kind {
        DocumentKind::Errors => format!(
            "problems in {file_name} ({})  →  {}",
            app.diagnostics.len(),
            OutFile::ErrorLog.name()
        ),
        DocumentKind::DocumentList => format!(
            "documents ({})  →  {}",
            app.buffer.get_line_count(),
            OutFile::FileList.name()
        ),
        // `:lsp` 那份不是「关于某个文件」的，所以标签里不带文件名 ——
        // 它讲的是**整个程序**的语言服务器配置，跟你在看哪个文件无关。
        DocumentKind::LspStatus => format!(
            "language servers ({})  →  {}",
            app.buffer.get_line_count(),
            OutFile::LspStatus.name()
        ),
        _ => format!("{file_name}{dirty_mark}"),
    }
}

/// 画中间的文本区（可选行号、当前行高亮、光标）。**没有边框** —— 整个 area 都是正文。
fn render_text_area(frame: &mut Frame, app: &App, area: Rect) {
    let colors = &app.config.colors;

    // 行号栏宽度：若开启，取「总行数的位数」，统一右对齐
    let gutter_width = if app.config.show_line_numbers {
        app.buffer.get_line_count().to_string().len()
    } else {
        0
    };

    // ⚠️ 没有边框了，所以 `area` **就是**文本区：行号从第 0 列开始，
    //    第 0 行就是文件的第一行。把这个事实写成一个名字，
    //    免得下面每一处坐标都要想「要不要减 1」。
    let inner_area = area;
    let visible_height = inner_area.height as usize;

    // 逐行构建内容：只取「视口内可见的行」，并裁掉左侧被横向滚动走的部分
    let mut rendered_lines: Vec<Line> = Vec::with_capacity(visible_height);
    for line_index in 0..visible_height {
        let file_row = app.viewport.top + line_index;
        let visible_text = app
            .buffer
            .get_visible_text_from_cell(file_row, app.viewport.left);

        let mut spans: Vec<Span> = Vec::with_capacity(2);
        if app.config.show_line_numbers {
            let line_number_text = if file_row < app.buffer.get_line_count() {
                format!("{:>w$} ", file_row + 1, w = gutter_width)
            } else {
                " ".repeat(gutter_width + 1)
            };
            spans.push(Span::styled(
                line_number_text,
                Style::default().fg(line_number_colour(app, file_row, colors.line_number)),
            ));
        }

        // 正文用配置里的颜色；当前行再叠一层很淡的背景色，方便定位
        let mut line_style = Style::default().fg(colors.text);
        if file_row == app.cursor.row {
            line_style = line_style.bg(colors.current_line_bg);
        }
        spans.push(Span::styled(visible_text, line_style));
        rendered_lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(rendered_lines), area);

    // 光标（命令模式下光标移到底部命令栏，这里不画）
    if app.mode != EditorMode::Command {
        let row_in_viewport = app.cursor.row.saturating_sub(app.viewport.top);
        if row_in_viewport < visible_height {
            // 行号栏占掉的格子数（显示行号时文本整体右移）
            let gutter_display_width = if app.config.show_line_numbers {
                gutter_width + 1
            } else {
                0
            };
            // 光标在「可见文本」内的横坐标（不含行号栏）
            let text_width = inner_area.width.saturating_sub(gutter_display_width as u16) as usize;
            let x_in_text_area = app
                .buffer
                .get_cell_at_char(app.cursor.row, app.cursor.col)
                .saturating_sub(app.viewport.left)
                .min(text_width);
            // 加上行号栏偏移才是真实屏幕坐标，再钳制在框内
            let cursor_screen_col = (gutter_display_width + x_in_text_area)
                .min(inner_area.width.saturating_sub(1) as usize);
            frame.set_cursor_position((
                inner_area.x + cursor_screen_col as u16,
                inner_area.y + row_in_viewport as u16,
            ));
        }
    }
}

/// 这一行的行号该用哪个颜色。
///
/// 没有诊断（或者这是个虚拟视图）就是普通行号色。
///
/// ## ⚠️ 为什么只染**行号**，不染正文
///
/// 因为「问题在哪一列」这件事**我们根本没存** —— 诊断是按行记的
/// （理由见 `diagnostic.rs` 的文件头）。既然不知道是哪几个字有问题，
/// 能给的最精确的标记就是「这一行」。
///
/// 一个更花哨的做法（整行背景色）被否了：彩色背景在终端里很容易变成一个
/// 色块，反倒把代码本身盖住；而下划线需要列，我们又没有。
///
/// ⚠️ **虚拟视图（`:errors`）永远用普通行号色**：那份清单的行号**就是**
/// 诊断的行号，再按诊断给它们染色，等于拿自己的输出喂自己。
fn line_number_colour(app: &App, file_row: usize, plain: Color) -> Color {
    if app.kind.is_virtual() {
        return plain;
    }
    match app.diagnostic_at_row(file_row) {
        Some(diagnostic) if diagnostic.severity.is_marked() => match diagnostic.severity {
            Severity::Error => app.config.colors.error,
            _ => app.config.colors.warning,
        },
        _ => plain,
    }
}

/// 画底部倒数第 2 行：命令模式 = 输入框；其它模式 = 「文件名 + 模式提示」
///
/// ⚠️ 命令模式那一支**整行都归输入框**，文件名会暂时看不见 —— 这是故意的，
/// 和 nvim 一样（`:cmd` 那一行会盖掉 statusline）。输入框只剩一行，
/// 再切一块给文件名，能看见的命令就更短了。
fn render_bottom_bar(frame: &mut Frame, app: &App, area: Rect) {
    match app.mode {
        EditorMode::Command => render_input_prompt(frame, app, area, ':'),
        EditorMode::External => render_input_prompt(frame, app, area, '!'),
        _ => render_mode_hint(frame, app, area),
    }
}

/// 底部输入行：`:` 命令 / `!` 外部命令。只差一个提示符，光标都在末尾。
fn render_input_prompt(frame: &mut Frame, app: &App, area: Rect, prompt: char) {
    let text = format!("{prompt}{}", app.command_input);
    frame.render_widget(
        Paragraph::new(text.as_str()).style(Style::default().fg(app.config.colors.command)),
        area,
    );

    // 光标停在输入内容的末尾。命令可能很长（粘一大段进来），
    // 所以宽度先夹到 u16 以内、相加用 saturating_add —— 直接相加会溢出 panic。
    let prompt_width = 1 + UnicodeWidthStr::width(app.command_input.as_str());
    let x = area
        .x
        .saturating_add(u16::try_from(prompt_width).unwrap_or(u16::MAX))
        .min(area.right().saturating_sub(1));
    frame.set_cursor_position((x, area.y));
}

/// 非命令模式：**文件名 + 模式名 + 快捷键提示**，占一整行
///
/// ## ⚠️ 三段的顺序就是它们的优先级
///
/// 这一行太长时，ratatui 从**右边**截掉。所以最左是最不能丢的：
///
/// | 位置 | 内容 | 丢了会怎样 |
/// |------|------|------------|
/// | 1 | 文件名（+ `*`、清单那几行的说明） | 不知道自己在看哪个文件 |
/// | 2 | `-- READ-ONLY --` / `-- EDIT --` | 不知道现在能不能打字 |
/// | 3 | 快捷键提示 | 少个提醒，翻文档能补回来 |
///
/// 实测过 40 列的终端：这个顺序下前两段都完整，只有提示被切。
fn render_mode_hint(frame: &mut Frame, app: &App, area: Rect) {
    let colors = &app.config.colors;
    let (label, label_style, hint) = match app.mode {
        EditorMode::ReadOnly => (
            "-- READ-ONLY --",
            Style::default().fg(colors.mode_readonly),
            "q back/quit | : command | ! shell | i edit | u undo | y copy line",
        ),
        EditorMode::Edit => (
            "-- EDIT --",
            Style::default().fg(colors.mode_edit),
            "Esc read-only | type to edit | ^Z undo | ^Y redo",
        ),
        // 底部在收集输入的那两种模式，这一行由 [`render_input_prompt`] 接管。
        // 这里直接返回而不是 `unreachable!()`：渲染路径上不该有任何 panic ——
        // 万一将来调用关系变了，少画一行也远比整程序崩掉好。
        EditorMode::Command | EditorMode::External => return,
    };

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            // 文件名用**正文色**：这一行里它是主角，而 `-- READ-ONLY --` 和提示
            // 都是暗灰的配角。想换成别的颜色只改这一处。
            Span::styled(
                format!(" {}  ", document_label(app)),
                Style::default().fg(colors.text),
            ),
            Span::styled(format!("{label}  "), label_style),
            Span::styled(hint, Style::default().fg(colors.hint)),
        ])),
        area,
    );
}

/// 最底下的状态信息行：独占一整行，长消息不容易被挤到截断
///
/// ## 优先级：光标那行的诊断 > status_message
///
/// 两样东西想用同一行。选诊断优先的理由：它是**关于你现在在哪**的，
/// 而 status_message 是一句「刚才那件事办好了」的回执 —— 你已经在往下看了，
/// 回执的价值就过去了。
///
/// ⚠️ 代价得说清楚：光标停在一行有错的地方时，`:w` 那句「Saved xxx」会被盖住。
/// 想反过来（永远先显示回执）只需掉个顺序 —— 它是这一处的一个决定，不是散开的。
fn render_status_line(frame: &mut Frame, app: &App, area: Rect) {
    if let Some(diagnostic) = cursor_line_diagnostic(app) {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                diagnostic.describe(),
                Style::default().fg(severity_colour(app, diagnostic.severity)),
            ))),
            area,
        );
        return;
    }

    let status = app.status_message.trim();
    if status.is_empty() {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            status,
            Style::default().fg(app.config.colors.status),
        ))),
        area,
    );
}

/// 光标所在那一行上的诊断（只认值得染色的那两档）。
///
/// 虚拟视图里不显示 —— 那份清单的「光标那一行」是清单自己的一行，
/// 跟哪一行代码有毛病没关系。
fn cursor_line_diagnostic(app: &App) -> Option<&Diagnostic> {
    if app.kind.is_virtual() {
        return None;
    }
    app.diagnostic_at_row(app.cursor.row)
        .filter(|diagnostic| diagnostic.severity.is_marked())
}

/// 某一档严重度用哪个颜色。
fn severity_colour(app: &App, severity: Severity) -> Color {
    match severity {
        Severity::Error => app.config.colors.error,
        _ => app.config.colors.warning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::config::Colors;

    /// 把一帧画进内存里的终端，返回整个屏幕的缓冲区。
    ///
    /// 这样能验证「配置里的颜色真的到了屏幕上」，而不只是结构体里赋了值。
    fn render_frame(app: &App, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("建终端");
        terminal
            .draw(|frame| render_ui(frame, app))
            .expect("渲染一帧");
        terminal.backend().buffer().clone()
    }

    /// 默认配色：正文绿、行号暗灰（用户点名要的效果）
    #[test]
    fn default_colors_reach_the_screen() {
        let app = App::from_content(None, "hello".to_string());
        let buffer = render_frame(&app, 20, 6);

        // **没有边框了**：第 0 行就是文件第一行，行号 "1 " 占第 0-1 列，
        // 正文从第 2 列开始。
        //
        // ⚠️ 行号是**暗灰**而不是黄的：它得给警告色让位（见 `DEFAULT_LINE_NUMBER_COLOR`）。
        assert_eq!(buffer[(0, 0)].symbol(), "1");
        assert_eq!(buffer[(0, 0)].fg, Color::DarkGray, "行号应该是暗灰");

        assert_eq!(buffer[(2, 0)].symbol(), "h");
        assert_eq!(buffer[(2, 0)].fg, Color::Green, "正文应该是绿色");

        // 光标当前行会叠一层背景色
        assert_eq!(buffer[(2, 0)].bg, Colors::default().current_line_bg);
    }

    /// 改了配置之后，屏幕上应该跟着变
    #[test]
    fn configured_colors_override_the_defaults() {
        let mut app = App::from_content(None, "hello".to_string());
        app.config.colors.text = Color::Cyan;
        app.config.colors.line_number = Color::Magenta;
        app.config.colors.current_line_bg = Color::Reset;
        app.config.colors.status = Color::LightBlue;
        app.set_status_message("saved");

        let buffer = render_frame(&app, 20, 6);

        assert_eq!(buffer[(0, 0)].fg, Color::Magenta);
        assert_eq!(buffer[(2, 0)].fg, Color::Cyan);
        assert_eq!(buffer[(2, 0)].bg, Color::Reset, "reset 应关掉当前行高亮");

        // 最底下一行是状态信息行
        let status_row = 5;
        assert_eq!(buffer[(0, status_row)].symbol(), "s");
        assert_eq!(buffer[(0, status_row)].fg, Color::LightBlue);
    }

    /// 关掉行号后，正文应该贴着屏幕最左边，不再给行号留位置
    #[test]
    fn hiding_line_numbers_shifts_text_left() {
        let mut app = App::from_content(None, "hello".to_string());
        app.config.show_line_numbers = false;

        let buffer = render_frame(&app, 20, 6);

        assert_eq!(buffer[(0, 0)].symbol(), "h");
        assert_eq!(buffer[(0, 0)].fg, Color::Green);
    }

    /// 回归：命令输入得很长（粘一大段）时光标定位不能溢出。
    ///
    /// 以前是 `area.x + width as u16`，宽度超过 u16 就会在加法上 panic。
    #[test]
    fn a_very_long_command_does_not_blow_up_the_cursor_position() {
        let mut app = App::new();
        app.set_mode(EditorMode::Command);
        app.command_input = "x".repeat(70_000);

        // 能画完一帧就算过
        let buffer = render_frame(&app, 80, 6);
        assert_eq!(buffer[(0, 4)].symbol(), ":");
    }

    // ---------- 诊断（行号染色 + 状态栏） ----------

    use crate::app::Cursor;
    use crate::diagnostic::Diagnostic;

    fn with_diagnostics(diagnostics: Vec<Diagnostic>) -> App {
        let mut app = App::from_content(Some("a.rs".to_string()), "one\ntwo\nthree".to_string());
        app.set_diagnostics(diagnostics);
        app
    }

    fn on(line: usize, severity: Severity, message: &str) -> Diagnostic {
        Diagnostic {
            line,
            severity,
            message: message.to_string(),
        }
    }

    /// 出错那一行的**行号**变色，其它行不变 —— 正文一个字都不动。
    #[test]
    fn only_the_line_number_of_a_broken_line_changes_colour() {
        let app = with_diagnostics(vec![on(1, Severity::Error, "boom")]);
        // 高度 7：文本区 5 行（7 − 底部两行），三行正文都放得下
        let buffer = render_frame(&app, 30, 7);

        // 第 2 行（屏幕上第 2 个内容行）的行号是红/亮红
        assert_eq!(buffer[(0, 1)].symbol(), "2");
        assert_eq!(buffer[(0, 1)].fg, Colors::default().error);
        // 正文还是原来的颜色 —— 我们**只知道是哪一行**，不知道是哪几个字
        assert_eq!(buffer[(2, 1)].symbol(), "t");
        assert_eq!(buffer[(2, 1)].fg, Color::Green);

        // 没问题的那两行还是普通行号色
        assert_eq!(buffer[(0, 0)].fg, Colors::default().line_number);
        assert_eq!(buffer[(0, 2)].fg, Colors::default().line_number);
    }

    #[test]
    fn a_warning_gets_its_own_colour() {
        let app = with_diagnostics(vec![on(0, Severity::Warning, "meh")]);
        let buffer = render_frame(&app, 30, 7);

        assert_eq!(buffer[(0, 0)].fg, Colors::default().warning);
    }

    /// `information` / `hint` **不染色** —— 太吵了。
    ///
    /// 行号栏只有一个格子，而「可以加个 `const` 哦」这种提示不值得占用它。
    /// （它们在 `:errors` 清单里还是看得见的。）
    #[test]
    fn hints_and_information_do_not_colour_the_gutter() {
        for severity in [Severity::Information, Severity::Hint] {
            let app = with_diagnostics(vec![on(0, severity, "轻轻提一句")]);
            let buffer = render_frame(&app, 30, 7);

            assert_eq!(
                buffer[(0, 0)].fg,
                Colors::default().line_number,
                "{severity:?} 不该染色"
            );
        }
    }

    /// 同一行上错误和警告都有时，**错误说了算**（行号栏只有一个格子）。
    #[test]
    fn an_error_wins_over_a_warning_on_the_same_line() {
        let app = with_diagnostics(vec![
            on(0, Severity::Warning, "次要的"),
            on(0, Severity::Error, "主要的"),
        ]);
        let buffer = render_frame(&app, 30, 7);

        assert_eq!(buffer[(0, 0)].fg, Colors::default().error);
    }

    /// 状态栏显示光标那行的诊断，**原文一字不改**。
    ///
    /// ⚠️ 原文是我们唯一不能动的东西：用户要把它整句丢进搜索框，
    /// 而 `cargo` 报的是同一句话。我们只在前面加了个「第几行、什么级别」的标签。
    #[test]
    fn the_status_line_shows_the_diagnostic_under_the_cursor() {
        let mut app = with_diagnostics(vec![on(1, Severity::Error, "cannot find value `fo`")]);
        app.cursor = Cursor { row: 1, col: 0 };

        let buffer = render_frame(&app, 60, 6);

        let status: String = (0..60).map(|x| buffer[(x, 5)].symbol()).collect();
        assert!(
            status.starts_with("2: error: cannot find value `fo`"),
            "状态栏该说清楚是第几行、什么级别、原文是什么：{status:?}"
        );
        assert_eq!(buffer[(0, 5)].fg, Colors::default().error);
    }

    /// 光标不在出错的那一行时，状态栏照旧显示普通消息。
    #[test]
    fn a_diagnostic_on_another_line_does_not_hijack_the_status_line() {
        let mut app = with_diagnostics(vec![on(2, Severity::Error, "boom")]);
        app.cursor = Cursor { row: 0, col: 0 };
        app.set_status_message("Saved a.rs");

        let buffer = render_frame(&app, 60, 6);

        let status: String = (0..60).map(|x| buffer[(x, 5)].symbol()).collect();
        assert!(status.starts_with("Saved a.rs"), "{status:?}");
    }

    /// 虚拟视图（`:errors`）里行号**不染色**。
    ///
    /// ⚠️ 那份清单的行号**就是**诊断的行号。再按诊断给它们染色，等于
    /// 拿自己的输出喂自己 —— 一行 `463: warning: ...` 会被染成
    /// 「第 463 行有毛病」的颜色，而它只是在说别处的第 463 行。
    #[test]
    fn the_error_list_does_not_colour_its_own_line_numbers() {
        let mut app = with_diagnostics(vec![on(0, Severity::Error, "boom")]);
        app.show_list(DocumentKind::Errors, "1: error: boom".to_string());

        let buffer = render_frame(&app, 40, 7);

        assert_eq!(buffer[(0, 0)].fg, Colors::default().line_number);
    }

    /// 虚拟视图的标签说得清「这是谁的问题」。
    ///
    /// ⚠️ 它现在在**底栏**（屏幕倒数第 2 行）—— 以前在上边框上，
    /// 而边框已经去掉了。那种「东西换了个位置」的改动，测试是唯一会响的哨兵。
    #[test]
    fn the_error_list_label_names_the_file_it_came_from() {
        let mut app = with_diagnostics(vec![on(0, Severity::Error, "boom")]);
        app.show_list(DocumentKind::Errors, "1: error: boom".to_string());

        // 高度 6 → 底部两行是第 4、5 行，标签在第 4 行
        let buffer = render_frame(&app, 60, 6);

        let label: String = (0..60).map(|x| buffer[(x, 4)].symbol()).collect();
        assert!(label.contains("problems in a.rs"), "{label:?}");
        assert!(label.contains("(1)"), "该说一条：{label:?}");
        // 模式提示还在同一行，没被挤掉
        assert!(label.contains("READ-ONLY"), "{label:?}");
    }

    /// ⚠️ **这一行最左边是文件名，而且不能被截掉。**
    ///
    /// 窄终端里 ratatui 从右边截，所以顺序 = 优先级：文件名 → 模式 → 提示。
    /// 实测 40 列的终端：前两段完整，只有提示被切。
    #[test]
    fn the_label_comes_first_so_a_narrow_terminal_keeps_it() {
        let app = App::from_content(Some("a.rs".to_string()), "hello".to_string());

        // 40 列：完整提示（60 多个字符）放不下
        let buffer = render_frame(&app, 40, 6);
        let bar: String = (0..40).map(|x| buffer[(x, 4)].symbol()).collect();

        assert!(
            bar.trim_start().starts_with("a.rs"),
            "文件名得在最左：{bar:?}"
        );
        assert!(bar.contains("READ-ONLY"), "模式也不能丢：{bar:?}");
        assert!(!bar.contains("copy line"), "提示被切掉是预期内的：{bar:?}");
    }

    /// 改过没保存时，底栏的文件名要带 `*` —— 它是**唯一**能看出这一点的全局标记。
    #[test]
    fn the_label_carries_the_unsaved_mark() {
        let mut app = App::from_content(Some("a.rs".to_string()), "hello".to_string());
        app.config.show_line_numbers = false;
        app.set_mode(EditorMode::Edit);
        app.insert_char_at_cursor('!');
        assert!(app.dirty, "先得真的改过");

        let buffer = render_frame(&app, 40, 6);
        let bar: String = (0..40).map(|x| buffer[(x, 4)].symbol()).collect();

        assert!(bar.trim_start().starts_with("a.rs*"), "{bar:?}");
    }

    /// 命令模式下这**整行**归输入框 —— 文件名暂时让位（和 nvim 一样）。
    #[test]
    fn the_command_line_takes_over_the_whole_bar() {
        let mut app = App::from_content(Some("a.rs".to_string()), "hello".to_string());
        app.set_mode(EditorMode::Command);
        app.command_input = "w".to_string();

        let buffer = render_frame(&app, 40, 6);
        let bar: String = (0..40).map(|x| buffer[(x, 4)].symbol()).collect();

        assert!(bar.starts_with(":w"), "{bar:?}");
        assert!(!bar.contains("a.rs"), "输入时文件名让位：{bar:?}");
    }
}
