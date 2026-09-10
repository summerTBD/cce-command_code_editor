//! 事件读取 —— event.rs 的职责
//!
//! 真正的“输入通道”：读终端，把 crossterm 的原始事件翻译成编辑器关心的
//! `Event`，供 update.rs 处理。
//!
//! 本文件**不做**任何“这个键该干什么”的判断（那是 update.rs 的事），
//! 也不改状态、不渲染。
//!
//! 说明：MVP 采用“按键驱动”——`read()` 阻塞等一个有意义的事件，
//! 事件一到 main.rs 就处理并重绘，因此天然“实时”。
//! 将来若要做光标闪烁动画或后台任务，再在 event.rs 里加“定时 Tick + 通道”即可。

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event as CrosstermEvent, KeyEvent, KeyEventKind, MouseEvent};

/// 编辑器关心的终端事件
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// 键盘按键（已过滤掉 Windows 上 Press/Release 里的 Release 重复事件）
    Key(KeyEvent),
    /// 鼠标事件
    Mouse(MouseEvent),
    /// 终端尺寸变化
    Resize(u16, u16),
    /// 粘贴内容（需要启用 bracketed paste，Windows 终端支持有限，MVP 可忽略）
    Paste(String),
    /// 鼠标等暂不关心的事件。内部使用：`read()` 会自动跳过它继续等待
    Ignored,
}

/// 阻塞等待并返回下一个“关心的事件”。
///
/// 会一直读到出现有意义的事件为止（自动跳过 `Event::Ignored`）。
pub fn read() -> io::Result<Event> {
    loop {
        let ev = event::read()?;
        match translate(ev) {
            Event::Ignored => continue,
            ours => return Ok(ours),
        }
    }
}

/// 非阻塞探测：在 timeout 内是否有事件到达
pub fn poll(timeout: Duration) -> io::Result<bool> {
    event::poll(timeout)
}

/// 把 crossterm 的原始事件翻译成编辑器自己的 `Event`
fn translate(ev: CrosstermEvent) -> Event {
    match ev {
        CrosstermEvent::Key(key) => {
            // 在 Windows 上，一次按键会同时产生 Press 和 Release 两个事件，
            // 若都放行会让同一个键触发两次；这里只保留 Press / Repeat。
            match key.kind {
                KeyEventKind::Press | KeyEventKind::Repeat => Event::Key(key),
                KeyEventKind::Release => Event::Ignored,
            }
        }
        CrosstermEvent::Mouse(mouse) => Event::Mouse(mouse),
        CrosstermEvent::Resize(cols, rows) => Event::Resize(cols, rows),
        CrosstermEvent::Paste(text) => Event::Paste(text),
        _ => Event::Ignored,
    }
}
