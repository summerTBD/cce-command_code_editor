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
pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_text_area(frame, app, chunks[0]);
    // 底部占两行、竖着排：第 1 行 = 命令输入 / 模式提示，第 2 行 = 状态信息
    draw_bottom_bar(frame, app, chunks[1]);
    draw_status_line(frame, app, chunks[2]);
}

/// 画中间的文本区（含顶部标题、可选行号、当前行高亮、光标）
fn draw_text_area(frame: &mut Frame, app: &App, area: Rect) {
    // 行号栏的宽度：若开启，取「总行数的位数」，统一右对齐
    let gutter_w = if app.show_line_numbers {
        app.buffer.line_count().to_string().len()
    } else {
        0
    };

    // 顶部标题：CCE · 文件名，未保存时末尾带 *
    let name = app.file_path.as_deref().unwrap_or("untitled");
    let dirty_mark = if app.dirty { "*" } else { "" };
    let title = format!(" CCE · {name}{dirty_mark} ");

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title);
    let inner = block.inner(area);
    let visible_height = inner.height as usize;

    // 逐行构建内容：只取「视口内可见的行」，并裁掉左侧被横向滚动走的部分
    let mut rows: Vec<Line> = Vec::with_capacity(visible_height);
    for i in 0..visible_height {
        let file_row = app.viewport.top + i;
        let text = app.buffer.line(file_row).unwrap_or("");
        let shown = skip_chars(text, app.viewport.left);

        let mut spans: Vec<Span> = Vec::with_capacity(2);
        if app.show_line_numbers {
            let gutter = if file_row < app.buffer.line_count() {
                format!("{:>w$} ", file_row + 1, w = gutter_w)
            } else {
                " ".repeat(gutter_w + 1)
            };
            spans.push(Span::styled(gutter, Style::default().fg(Color::DarkGray)));
        }

        //当前行加一层很淡的背景色，方便定位
        let text_style = if file_row == app.cursor.row {
            Style::default().bg(Color::Indexed(236))
        } else {
            Style::default()
        };
        spans.push(Span::styled(shown.to_string(), text_style));
        rows.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(rows).block(block), area);

    // 光标（命令模式下光标移到底部命令栏，这里不画）
    if app.mode != EditorMode::Command {
        let row_in_view = app.cursor.row.saturating_sub(app.viewport.top);
        if row_in_view < visible_height {
            let line_text = app.buffer.line(app.cursor.row).unwrap_or("");
            // 行号栏占掉的格子数（显示行号时文本整体右移 gutter_disp 格）
            let gutter_disp = if app.show_line_numbers {
                gutter_w + 1
            } else {
                0
            };
            // 光标在「可见文本」内的横坐标（不含行号栏）
            let text_w = inner.width.saturating_sub(gutter_disp as u16) as usize;
            let x_in_text = cursor_cell_x(line_text, app.cursor.col, app.viewport.left, text_w);
            // 加上行号栏偏移才是真实屏幕坐标，再钳制在框内
            let col = (gutter_disp + x_in_text).min(inner.width.saturating_sub(1) as usize);
            frame.set_cursor_position((inner.x + col as u16, inner.y + row_in_view as u16));
        }
    }
}

/// 画底部倒数第 2 行：命令模式 = 输入框；其它模式 = 模式提示
fn draw_bottom_bar(frame: &mut Frame, app: &App, area: Rect) {
    match app.mode {
        EditorMode::Command => draw_command_prompt(frame, app, area),
        _ => draw_mode_hint(frame, app, area),
    }
}

/// 命令模式：显示 `:` 开头的光标输入框
fn draw_command_prompt(frame: &mut Frame, app: &App, area: Rect) {
    let prompt = format!(":{}", app.command_input);
    frame.render_widget(
        Paragraph::new(prompt.as_str()).style(Style::default().fg(Color::Cyan)),
        area,
    );
    let x = 1 + UnicodeWidthStr::width(app.command_input.as_str());
    let x = (area.x + x as u16).min(area.right().saturating_sub(1));
    frame.set_cursor_position((x, area.y));
}

/// 非命令模式：模式名 + 快捷键提示，占一整行
fn draw_mode_hint(frame: &mut Frame, app: &App, area: Rect) {
    let (label, label_style, hint) = match app.mode {
        EditorMode::ReadOnly => (
            "-- READ-ONLY --",
            Style::default().fg(Color::DarkGray),
            "q quit | : command | i edit",
        ),
        EditorMode::Edit => (
            "-- EDIT --",
            Style::default().fg(Color::Yellow),
            "Esc read-only | type to edit",
        ),
        EditorMode::Command => unreachable!("Command mode should not be reached here"),
    };

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {label} "), label_style),
            Span::styled(hint, Style::default().fg(Color::DarkGray)),
        ])),
        area,
    );
}

/// 最底下的状态信息行：独占一整行，长消息不容易被挤到截断
fn draw_status_line(frame: &mut Frame, app: &App, area: Rect) {
    let status = app.status_message.trim();
    if status.is_empty() {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            status,
            Style::default().fg(Color::Green),
        ))),
        area,
    );
}

/// 裁掉字符串开头的 n 个「字符」（n 是字符数，不是字节数），返回剩下的子串
fn skip_chars(s: &str, n: usize) -> &str {
    s.char_indices()
        .nth(n)
        .map(|(byte, _)| &s[byte..])
        .unwrap_or("")
}

/// 光标所在列的显示宽度：
/// 把 `(viewport.left .. cursor.col)` 之间那段可见字符按终端显示宽度求和
/// （中文算 2 格），结果不会超过可视宽度。
fn cursor_cell_x(line_text: &str, col: usize, viewport_left: usize, width: usize) -> usize {
    if col <= viewport_left {
        return 0;
    }
    let visible = skip_chars(line_text, viewport_left);
    let prefix = visible
        .chars()
        .take(col - viewport_left)
        .collect::<String>();
    UnicodeWidthStr::width(prefix.as_str()).min(width)
}
