//! 界面渲染 —— ui.rs 的职责
//!
//! 只做一件事：**读 `App` 状态 → 在终端上画出来**。
//! 它不读按键、不改状态、不做文件 I/O。
//!
//! 布局（自上而下）：
//!   ┌────────────────────────────────┐  ← 文本区（Rounded 圆角边框）
//!   │ 第1行内容                       │
//!   │ 第2行内容                       │
//!   └────────────────────────────────┘
//!   [ 底部栏 ]：命令模式下显示 `:xxx` 输入框，其它模式显示模式提示/状态信息

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, EditorMode};

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

/// 画中间的文本区（含顶部标题、可选行号、当前行高亮、光标）
fn render_text_area(frame: &mut Frame, app: &App, area: Rect) {
    let colors = &app.config.colors;

    // 行号栏宽度：若开启，取「总行数的位数」，统一右对齐
    let gutter_width = if app.config.show_line_numbers {
        app.buffer.get_line_count().to_string().len()
    } else {
        0
    };

    // 顶部标题：STBD · 文件名，未保存时末尾带 *
    let file_name = app.file_path.as_deref().unwrap_or("untitled");
    let dirty_mark = if app.dirty { "*" } else { "" };
    let title = format!(" STBD · {file_name}{dirty_mark} ");

    let border_style = Style::default().fg(colors.border);
    let outer_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(border_style)
        .title_style(border_style);
    let inner_area = outer_block.inner(area);
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
                Style::default().fg(colors.line_number),
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

    frame.render_widget(Paragraph::new(rendered_lines).block(outer_block), area);

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

/// 画底部倒数第 2 行：命令模式 = 输入框；其它模式 = 模式提示
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

/// 非命令模式：模式名 + 快捷键提示，占一整行
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
            Span::styled(format!(" {label} "), label_style),
            Span::styled(hint, Style::default().fg(colors.hint)),
        ])),
        area,
    );
}

/// 最底下的状态信息行：独占一整行，长消息不容易被挤到截断
fn render_status_line(frame: &mut Frame, app: &App, area: Rect) {
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

    /// 默认配色：正文绿、行号黄（用户点名要的效果）
    #[test]
    fn default_colors_reach_the_screen() {
        let app = App::from_content(None, "hello".to_string());
        let buffer = render_frame(&app, 20, 6);

        // 第 1 个内容行在第 1 行（第 0 行是上边框）；
        // 左边框占第 0 列，行号 "1 " 占第 1-2 列，正文从第 3 列开始
        assert_eq!(buffer[(1, 1)].symbol(), "1");
        assert_eq!(buffer[(1, 1)].fg, Color::Yellow, "行号应该是黄色");

        assert_eq!(buffer[(3, 1)].symbol(), "h");
        assert_eq!(buffer[(3, 1)].fg, Color::Green, "正文应该是绿色");

        // 光标当前行会叠一层背景色
        assert_eq!(buffer[(3, 1)].bg, Colors::default().current_line_bg);
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

        assert_eq!(buffer[(1, 1)].fg, Color::Magenta);
        assert_eq!(buffer[(3, 1)].fg, Color::Cyan);
        assert_eq!(buffer[(3, 1)].bg, Color::Reset, "reset 应关掉当前行高亮");

        // 最底下一行是状态信息行
        let status_row = 5;
        assert_eq!(buffer[(0, status_row)].symbol(), "s");
        assert_eq!(buffer[(0, status_row)].fg, Color::LightBlue);
    }

    /// 关掉行号后，正文应该贴着左边框，不再给行号留位置
    #[test]
    fn hiding_line_numbers_shifts_text_left() {
        let mut app = App::from_content(None, "hello".to_string());
        app.config.show_line_numbers = false;

        let buffer = render_frame(&app, 20, 6);

        assert_eq!(buffer[(1, 1)].symbol(), "h");
        assert_eq!(buffer[(1, 1)].fg, Color::Green);
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
}
