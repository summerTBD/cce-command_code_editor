//! 后台跑 `cargo check` —— `tools/check.rs` 的职责
//!
//! 这是项目里**第一个后台任务**，所以这个文件的意义不止于「跑一次 cargo」——
//! 它把后面 LSP 要用的四件事全练了一遍：
//!
//! 1. **线程**：活儿在别的线程上干，主线程一秒都不等它
//! 2. **通道**：结果通过 `mpsc` 回到主循环，而不是共享内存
//! 3. **不阻塞**：主线程只在「轮到它的时候」用 `try_recv` 顺手取一下
//! 4. **子进程的输出不能继承终端**：`cargo` 要是往我们的备用屏上写字，
//!    画面立刻就花了
//!
//! ## 为什么不直接 `Command::status()`
//!
//! `status()` 会**一直阻塞到命令结束**。这个项目 `cargo check` 要两三秒 ——
//! 那两三秒里按键不响应、屏幕不刷新，等于卡死。
//!
//! ⚠️ 注意这跟 `!` 让位是**两件相反的事**：让位是主动把终端交出去、等你敲回车；
//! `:check` 是「你继续编辑，它在后台查」—— 所以它连一个字都不许往终端上写。
//!
//! ## v1 为什么**不数**「几条错误」
//!
//! 一开始想从人能看的 stderr 里数条数，试了才发现那是在**猜**：
//! `error: could not compile … due to 1 previous error` 这句总结也以 `error` 开头，
//! `warning: … generated 1 warning` 同理。数出来的条数会**永远虚高**，
//! 而一个虚高的错误数比没有错误数更糟 —— 它会让人不再信这个功能。
//!
//! 所以 v1 退一步，只报 **cargo 自己写的那句总结**（它就在输出末尾，
//! 是权威的、不会错的），加一个退出状态和耗时。
//! 真正要「哪一行、什么错」得解析 `--message-format=json`，
//! 而那件事该和 LSP 的诊断**共用同一套结构**，所以留到那一步一起做。

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Instant;

/// 末尾保留这么多行就够找总结了（cargo 的习惯是把总结放最后）。
const KEEP_LINES: usize = 50;

/// 状态栏上摘要最多显示这么多个字符。
///
/// 状态栏只有一行，摘要过长会被终端从右边截掉 —— 与其让它被随意切断
/// （可能正好断在关键处），不如我们自己**在词边界上**收一下。
const SUMMARY_CHARS: usize = 90;

/// 从 cargo 的输出里挑出「最值得报给用户的那一行」：末尾第一个非空行。
///
/// **纯函数**，所以能被测试。这个文件里线程、进程、计时都是编排，
/// **有判断的只有这里** —— 所以值得把它单独拎出来。
pub fn last_line(lines: &[&str]) -> String {
    lines
        .iter()
        .rev()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// 太长的摘要收一下，并在**词边界**上截断。
///
/// 为什么不按字符数硬切：cargo 的话里全是反引号和路径，
/// 切在词中间会读成一个不存在的名字（`` `stbd` `` → `` `stb ``），比截短更容易误事。
fn shorten(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    let mut kept = String::new();
    for word in text.split_whitespace() {
        // +1 是留出空格；末尾还要放一个省略号
        if kept.chars().count() + word.chars().count() + 1 > max_chars.saturating_sub(1) {
            break;
        }
        if !kept.is_empty() {
            kept.push(' ');
        }
        kept.push_str(word);
    }

    if kept.is_empty() {
        // 一个词就超长（比如一整个很长的路径）：只能硬切，
        // 但至少用 bytes 转 chars 而不是反过来，别切出半个字符
        return text
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>()
            + "…";
    }
    kept.push('…');
    kept
}

/// 一次检查的结果。
///
/// 拆成字段而不是让线程直接拼一句话，是为了让 [`CheckReport::describe`]
/// 能被单独测试 —— 以后想改措辞也不必动统计逻辑。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// `cargo` 自己说成功没有（退出码为 0）
    pub ok: bool,
    /// cargo 的总结那句话（成功时是 `Finished …`，失败时是
    /// `error: could not compile …`）。可能是空字符串（stderr 什么都没输出）。
    pub summary: String,
    /// 一共花了多少毫秒
    pub elapsed_ms: u128,
}

impl CheckReport {
    /// 给状态栏看的那一句话。
    ///
    /// ⚠️ 这里自己 `trim()`，**不指望调用方先收拾干净**：cargo 输出里
    /// `Finished …` 那行是带前导空格的，飘到状态栏上就是「Check: OK  —      Finished …」
    /// 这种莫名其妙的多余空白。判断留在一处，别让每个调用方各 trim 一遍。
    pub fn describe(&self) -> String {
        let verdict = if self.ok { "OK" } else { "FAILED" };
        let seconds = self.elapsed_ms as f64 / 1000.0;
        let summary = shorten(self.summary.trim(), SUMMARY_CHARS);
        if summary.is_empty() {
            format!("Check: {verdict}  ({seconds:.1}s)")
        } else {
            format!("Check: {verdict}  —  {summary}  ({seconds:.1}s)")
        }
    }
}

/// 在后台线程上跑 `cargo check`，结果通过通道送回来。
///
/// `dir` 是起点目录。给「当前文档所在的目录」就够了 —— `cargo` 会自己往上找
/// `Cargo.toml`（跟 `:open` 用的是同一个基准：`App::current_directory()`）。
///
/// 返回值是收结果的通道。**线程发完一条就结束，发送端随之被丢掉**，于是主循环
/// 下次 `try_recv` 会拿到 `Disconnected` —— 那就是「任务已结束」的通知，
/// 不需要另加一个状态位去记它。
pub fn spawn(dir: Option<&str>) -> io::Result<Receiver<CheckReport>> {
    let mut command = Command::new("cargo");
    command.arg("check");
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    // ⚠️ 关键：**子进程的输出绝不能继承终端**。
    //    stdout 直接丢掉（check 的人话版本都在 stderr），stderr 接管过来自己读。
    //    要是不接，cargo 就会往我们正在用的备用屏上写字，画面当场花掉。
    command.stdout(Stdio::null()).stderr(Stdio::piped());

    let mut child = command.spawn()?;
    // 上面刚 piped 过，一定在；真取不到也是「不可能发生」而不是运行期状况
    let stderr = child.stderr.take().expect("stderr was piped");

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let started = Instant::now();

        // 只留末尾几行 —— 总结在最后，前面几百行编译过程没有用
        let mut tail: VecDeque<String> = VecDeque::new();

        // 在**这个线程上**阻塞着逐行读 —— 这正是重点：阻塞只发生在这里，
        // 主线程完全不知道有人在外面等。
        //
        // ⚠️ 就算我们一个字都不要也必须读完：管道缓冲区一满，cargo 自己就会
        //    卡在写上面。读干净是「别把对面堵死」的义务，不只是为了拿数据。
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            tail.push_back(line);
            if tail.len() > KEEP_LINES {
                tail.pop_front();
            }
        }

        let ok = matches!(child.wait(), Ok(status) if status.success());
        let lines: Vec<&str> = tail.iter().map(String::as_str).collect();
        let report = CheckReport {
            ok,
            summary: last_line(&lines),
            elapsed_ms: started.elapsed().as_millis(),
        };

        // 主循环要是已经退出了（比如你在这几秒里按了 `:q`），send 会失败 ——
        // 那不是错误，线程平静结束就好。
        let _ = tx.send(report);
    });

    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_line_takes_the_final_non_empty_line() {
        assert_eq!(
            last_line(&["   Compiling stbd v0.1.0", "    Finished dev profile", ""]),
            "Finished dev profile"
        );
        // 末尾一堆空行（cargo 有时会多打一个换行）
        assert_eq!(
            last_line(&["error: could not compile `stbd`", "", "  "]),
            "error: could not compile `stbd`"
        );
        // 什么都没有：返回空串，让调用方**少说一句**而不是说错一句
        assert_eq!(last_line(&[]), "");
        assert_eq!(last_line(&["", "   "]), "");
    }

    #[test]
    fn describe_reports_ok_with_cargo_own_summary() {
        let report = CheckReport {
            ok: true,
            summary: "    Finished `dev` profile [unoptimized + debuginfo] target(s)".to_string(),
            elapsed_ms: 1340,
        };
        assert_eq!(
            report.describe(),
            "Check: OK  —  Finished `dev` profile [unoptimized + debuginfo] target(s)  (1.3s)"
        );
    }

    #[test]
    fn describe_reports_failure() {
        let report = CheckReport {
            ok: false,
            summary: "error: could not compile `stbd` (bin \"stbd\") due to 1 previous error"
                .to_string(),
            elapsed_ms: 2500,
        };
        let text = report.describe();
        assert!(text.starts_with("Check: FAILED"), "{text}");
        assert!(text.contains("due to 1 previous error"), "{text}");
        assert!(text.ends_with("(2.5s)"), "{text}");
    }

    /// 没有总结时**少说一句**，而不是补一句编出来的话。
    #[test]
    fn describe_omits_the_summary_when_there_is_none() {
        let report = CheckReport {
            ok: false,
            summary: String::new(),
            elapsed_ms: 800,
        };
        assert_eq!(report.describe(), "Check: FAILED  (0.8s)");
    }

    #[test]
    fn shorten_keeps_short_text_untouched() {
        assert_eq!(shorten("error: boom", 90), "error: boom");
    }

    /// ⚠️ 这条守着「在词边界上收」：硬切会把 `` `stbd` `` 切成 `` `stb ``，
    /// 读起来就像一个不存在的 crate 名。
    #[test]
    fn shorten_cuts_at_a_word_boundary() {
        let long = "error: could not compile `stbd` (bin \"stbd\") due to 12 previous errors; \
                    5 warnings emitted, and then some more words to push it over the limit";
        let short = shorten(long, 40);

        assert!(short.chars().count() <= 40, "{short}");
        assert!(short.ends_with('…'), "{short}");
        // 去掉省略号之后，剩下的必须是原句的一个**完整词前缀**
        let body = short.trim_end_matches('…');
        assert!(long.starts_with(body), "{short} 不是原句的前缀");
        assert!(!body.ends_with(' '), "不该留一个尾巴上的空格：{short:?}");
    }

    /// 一个词就超长（比如一整个很长的路径）时不能死循环，也不能 panic。
    #[test]
    fn shorten_survives_a_single_monster_word() {
        let monster = "a".repeat(500);
        let short = shorten(&monster, 20);
        assert!(short.chars().count() <= 20, "{short}");
        assert!(short.ends_with('…'), "{short}");
    }
}
