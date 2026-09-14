//! 事件读取 —— event.rs 的职责
//!
//! 真正的“输入通道”：读终端，把 crossterm 的原始事件翻译成编辑器关心的
//! `Event`，供 update.rs 处理。
//!
//! 本文件**不做**任何“这个键该干什么”的判断（那是 update.rs 的事），
//! 也不改状态、不渲染。
//!
//! 说明：主循环用 [`poll_event`] **带超时**地等事件。超时（返回 `None`）
//! 不是「什么都没发生」，而是「轮到我们看一眼后台任务了」。
//!
//! 这就是上面那段预告的「定时 Tick + 通道」，落地形状比预想的简单：
//! **Tick 就是 `None`** —— 不需要单独造一个 `Event::Tick` 变体；
//! 而通道归调用方管，因为 `event.rs` 不该知道谁会往里面发消息。

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

/// 最多等 `timeout`，然后返回一个「关心的事件」或 `None`。
///
/// ⚠️ **`None` 不是「什么都没有」** —— 它是「这段时间里键盘没动静」。
/// 对主循环来说那是一次**宝贵的机会**：正好用来看看后台任务有没有消息、
/// 有没有到期的定时工作。所以这个超时是**特性**，不是不得已的妥协。
///
/// 为什么不继续用「一直阻塞到有事件」：那样主循环就只有一个耳朵，
/// 后台任务说的话得等到你下次按键才被看见。完整的理由见 `main::run_event_loop`。
pub fn poll_event(timeout: Duration) -> io::Result<Option<Event>> {
    if !event::poll(timeout)? {
        return Ok(None);
    }
    // `poll` 说有事件了，这次 `read` 就不会再阻塞
    match translate_crossterm_event(event::read()?) {
        // 罕见的 `Release` 之类：当成一次空转返回。
        // ⚠️ **不能 loop 回去重读** —— 那就又变回「可能无限期阻塞」了，
        //    而消灭这个可能性正是本函数存在的全部理由。
        Event::Ignored => Ok(None),
        ours => Ok(Some(ours)),
    }
}

/// 把 crossterm 的原始事件翻译成编辑器自己的 `Event`
fn translate_crossterm_event(ev: CrosstermEvent) -> Event {
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
