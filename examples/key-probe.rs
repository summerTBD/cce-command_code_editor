//! 按键探针 · 「我这台终端到底把哪些键送进来了？」
//!
//! 用来回答一个**猜不出来**的问题：`Shift+↑` / `Alt+↑` / `Ctrl+Shift+↑` 这些组合，
//! 究竟是我们收不到（终端自己吃掉了），还是收到了但读错了？
//! 同一个键在不同终端（Windows Terminal / VS Code / conhost / iTerm）发出的东西
//! 并不一样，所以只能实测 —— 这跟量字体、量宽度是同一类活。
//!
//! ## 两条纪律
//!
//! 1. **读键路径必须和编辑器完全一致**：`crossterm::event::read` + raw mode。
//!    这里要是换成「开了键盘增强标志」的读法，测出来的就是另一个世界 ——
//!    而编辑器没开，那份结论对编辑器不成立。（终端**支不支持**增强协议是另一码事，
//!    开场会单独报一句，但**不启用**它。）
//! 2. **一个字都不解释**：只把 crossterm 给的 `code / modifiers / kind` 原样打出来。
//!    「这个键该干什么」是 `update.rs` 的事；探针一旦开始解释，就不能当证据了。
//!
//! 唯一**故意**和编辑器不一样的地方：这里**不启用鼠标捕获**。
//! 鼠标捕获会把终端自己的「拖选复制」也一起关掉，而你可能正想把屏幕上的输出
//! 拖选出来贴给我。键的解析不受它影响，所以这一处不同不影响结论。
//!
//! ## 怎么用
//!
//! ```text
//! cargo run --example key-probe
//! ```
//!
//! 然后照着屏幕上那几行提示按。**某一组按键一行都没打出来 = 终端把它吃掉了**，
//! 这正是我们要的答案。Ctrl+Q（或 F10）退出。

use std::io::{self, Write};

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal;

/// 屏幕上那几行提示。写成常量，省得改了代码忘了改提示。
const INSTRUCTIONS: &[&str] = &[
    "按下面每一组各试两三下（每按一下这里就会打一行）：",
    "",
    "    ↑  ↓                 基线，这两个应该一定收得到",
    "    Shift+↑  Shift+↓     「扩选」想用的键",
    "    Alt+↑   Alt+↓        「把这一段上下移动」想用的键",
    "    Ctrl+↑  Ctrl+Shift+↑",
    "    Shift+←  Shift+→  Shift+Home  Shift+End",
    "    Ctrl+C  Ctrl+V       顺便看看它们在终端里是什么",
    "    Tab  Enter  Esc  Backspace  Delete",
    "",
    "哪一组「一行都没打出来」，就是终端自己吃掉了 —— 那组键不能拿来做功能。",
    "注意看倒数第二列：`modifiers=NONE` 的意思是「键到了，但修饰键丢了」，",
    "那和「一行都没有」是两种不同的坏法，修法也不一样。",
    "",
    "Ctrl+Q 或 F10 退出（F10 是保险：万一 Ctrl+Q 在你这台终端里送不进来）。",
];

fn main() -> io::Result<()> {
    let mut out = io::stdout();

    // ---------- 开场：先说清楚「这份结论属于哪台终端」 ----------
    // 换个终端结论就会变。没有这一行，三个月后没人知道当初量的是谁。
    println!(
        "TERM_PROGRAM={}  TERM={}  WT_SESSION={}",
        env_or_dash("TERM_PROGRAM"),
        env_or_dash("TERM"),
        if std::env::var_os("WT_SESSION").is_some() {
            "yes"
        } else {
            "no"
        },
    );
    // 只是**问一句**支不支持增强键盘协议（kitty 那套 `CSI u`），不启用它 ——
    // 见文件头上那条纪律：启用了，下面测的就不是编辑器看到的世界了。
    match terminal::supports_keyboard_enhancement() {
        Ok(true) => println!("keyboard enhancement: 支持"),
        Ok(false) => println!("keyboard enhancement: 不支持"),
        Err(err) => println!("keyboard enhancement: 问不出来（{err}）"),
    }
    println!();
    for line in INSTRUCTIONS {
        println!("{line}");
    }
    println!();
    out.flush()?;

    // ---------- 开测 ----------
    // 横幅在**普通模式**下打完再进 raw：raw 模式下 `\n` 只往下不回车，
    // 那会打成楼梯形。进 raw 之后的每一行都自己带 `\r\n`。
    terminal::enable_raw_mode()?;
    let result = watch_keys(&mut out);
    terminal::disable_raw_mode()?;
    writeln!(out, "\r\n-- 探针结束 --")?;
    result
}

/// 把每一个事件打成一行，直到 Ctrl+Q。
fn watch_keys(out: &mut impl Write) -> io::Result<()> {
    let mut count = 0usize;
    loop {
        let event = match event::read() {
            Ok(event) => event,
            Err(err) => {
                write!(out, "读键出错（{err}）—— 到此为止\r\n")?;
                out.flush()?;
                return Ok(());
            }
        };

        let line = match event {
            Event::Key(key) => {
                // 两个退出键：Ctrl+Q 是常规的，F10 是保险 ——
                // 万一某个终端的 Ctrl+Q 被流量控制吃掉，人不应该被困在 raw 模式里
                // （raw 模式下 Ctrl+C 在 Windows 上不再杀进程，那就是真困住了）
                let quit = key.code == KeyCode::F(10)
                    || (key.code == KeyCode::Char('q')
                        && key.modifiers.contains(KeyModifiers::CONTROL));
                if quit {
                    return Ok(());
                }
                // `kind` 也要打：Windows 上一次按键会送 Press + Release 两个，
                // 编辑器那边靠这个字段过滤重复（见 `event.rs`）
                format!(
                    "code={:<16?} modifiers={:<16} kind={:?}",
                    key.code,
                    describe(key.modifiers),
                    key.kind
                )
            }
            Event::Mouse(mouse) => format!("mouse={:?}", mouse.kind),
            Event::Resize(cols, rows) => format!("resize={cols}x{rows}"),
            Event::Paste(text) => format!("paste={} chars", text.chars().count()),
            // 焦点变化之类：静默跳过。
            // ⚠️ 必须是 `continue` 不能是 `break` —— 那样探针会莫名其妙自己结束
            _ => continue,
        };

        count += 1;
        write!(out, "[{count:>3}] {line}\r\n")?;
        out.flush()?;
    }
}

/// 把修饰键说成人话（`SHIFT|ALT` / `NONE`）。
///
/// 不用 crossterm 的 `Debug` 是因为它把 flags 包成 `KeyModifiers(SHIFT)`，
/// 一列对不齐、扫起来累 —— 这个工具唯一的读者是人眼。
fn describe(modifiers: KeyModifiers) -> String {
    if modifiers.is_empty() {
        return "NONE".to_string();
    }
    let mut parts = Vec::new();
    for (flag, name) in [
        (KeyModifiers::SHIFT, "SHIFT"),
        (KeyModifiers::CONTROL, "CTRL"),
        (KeyModifiers::ALT, "ALT"),
        (KeyModifiers::SUPER, "SUPER"),
        (KeyModifiers::META, "META"),
        (KeyModifiers::HYPER, "HYPER"),
    ] {
        if modifiers.contains(flag) {
            parts.push(name);
        }
    }
    parts.join("|")
}

/// 环境变量的值；没设过就给一个 `-`（比空着好读）。
fn env_or_dash(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| "-".to_string())
}
