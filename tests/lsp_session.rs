//! `session.rs` 的集成测试 —— **真进程、真管道**。
//!
//! 这里测的是「客户端到底往管道里写了什么」。这件事**从外面看不见**：
//! 它不落盘、不打印，只在管道里过一下。所以假服务器会把收到的每一句
//! 报出来（stderr），我们再去翻它的账 —— 这是唯一能真正验证同步行为的路。
//!
//! 单元测试在这里帮不上忙：`didOpen` / `didClose` 的**顺序**、版本号、
//! 「一个字没动就别开口」，全都是**跨进程**才存在的性质。

use std::time::{Duration, Instant};

use stbd::lsp::diagnostics::PublishDiagnostics;
use stbd::lsp::session::{Outcome, Session};

mod common;

/// 假服务器的路径（cargo 会给集成测试设好这个环境变量）
const FAKE: &str = env!("CARGO_BIN_EXE_fake-lsp");

/// 测试里用的文件。**故意用大写盘符** —— 真实服务器会把 uri 里的盘符
/// 规范成小写（`D:` → `d:`），我们发出去的这条得能受得住那个变化。
const URI: &str = "file:///D:/fake-project/src/main.rs";

/// 会话 + 一个「已经取出来、还没被看」的缓冲区。
///
/// ⚠️ 为什么要缓冲：[`Outcome::Ready`] 是**一次性**的 —— 谁 `poll` 了它却没把
/// 结果处理掉，那个通知就永远没了。而测试里到处都需要「顺手推一下会话」，
/// 第一版就是这么写的（`count_in_log` 里偷偷 `poll` 了一下），
/// 结果 `Ready` 被它吃掉，后面的等待全部超时。
///
/// 所以「推」和「看」**分开**：`pump` 只管把结果搬进缓冲区，谁都不会漏。
struct Harness {
    session: Session,
    pending: Vec<Outcome>,
}

impl Harness {
    fn start(args: &[&str]) -> Self {
        Self {
            session: Session::start(FAKE, args, None, Some("file:///D:/fake-project"))
                .expect("起不来 fake-lsp"),
            pending: Vec::new(),
        }
    }

    /// 推一下会话，把新产出搬进缓冲区。
    fn pump(&mut self) {
        self.pending.extend(self.session.poll());
    }

    /// 把缓冲区里符合条件的第一个结果取走。
    fn take(&mut self, want: impl Fn(&Outcome) -> bool) -> Option<Outcome> {
        let index = self.pending.iter().position(want)?;
        Some(self.pending.remove(index))
    }

    /// 一直推到握手完成（或者超时炸掉）。
    fn wait_ready(&mut self) {
        let deadline = Instant::now() + common::FAKE_SERVER_WAIT;
        while Instant::now() < deadline {
            self.pump();
            if self.take(|outcome| *outcome == Outcome::Ready).is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("握手没走完；日志：{:#?}", self.session.log_tail());
    }

    /// 推到日志里出现某个片段为止，返回那一行。
    ///
    /// ⚠️ 必须一边推一边翻：stderr 是**另一个线程**在读，推进会话能让
    /// 「我们要等的那句话怎么还不来」这件事有个明确的截止时间，
    /// 而不是靠一个死等。
    fn wait_for_log(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + common::FAKE_SERVER_WAIT;
        while Instant::now() < deadline {
            if let Some(line) = self
                .session
                .log_tail()
                .into_iter()
                .find(|line| line.contains(needle))
            {
                return line;
            }
            self.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!(
            "日志里一直没出现 {needle:?}；日志：{:#?}",
            self.session.log_tail()
        );
    }

    /// 日志里现在有几行提到这个片段。
    ///
    /// 故意**不** `pump`：这条是给「我干完一件事，现在确认账单上没多出东西」用的，
    /// 不等待才是它要的语义。（要等就用 `wait_for_log`。）
    fn count_in_log(&self, needle: &str) -> usize {
        self.session
            .log_tail()
            .iter()
            .filter(|line| line.contains(needle))
            .count()
    }

    /// 推到收到一份诊断为止。
    fn wait_diagnostics(&mut self) -> PublishDiagnostics {
        let deadline = Instant::now() + common::FAKE_SERVER_WAIT;
        while Instant::now() < deadline {
            self.pump();
            if let Some(Outcome::Diagnostics(push)) =
                self.take(|outcome| matches!(outcome, Outcome::Diagnostics(_)))
            {
                return push;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("一直没等到诊断；日志：{:#?}", self.session.log_tail());
    }

    /// 报告事实：现在屏幕上是这个文件、这些文本。
    fn show(&mut self, uri: &str, text: &str) {
        self.session.show(uri, "rust", text).expect("show 不该失败");
    }
}

// ---------- 握手 ----------

/// 握手完成会**报一次** `Ready`，而且**只报一次**。
///
/// 「连上了」值得说一句，「还连着」不值得每轮都说 ——
/// 每轮都报的话，状态栏会被这句话永远占着。
#[test]
fn ready_is_announced_exactly_once() {
    let mut harness = Harness::start(&[]);
    harness.wait_ready();

    // 再推几轮，不该再冒出第二个 Ready
    for _ in 0..10 {
        harness.pump();
        assert!(
            harness.take(|outcome| *outcome == Outcome::Ready).is_none(),
            "`Ready` 报了两遍 —— 状态栏会被它占着不放"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

// ---------- 文档同步 ----------

/// ⚠️ 握手**没完成之前一个字都不能发**。
///
/// 规范说那段时间里服务器可以忽略一切（`initialize` / `exit` 除外），
/// 而 `rust-analyzer` 确实会 —— **静默**忽略，不报错。
/// 所以漏了这一条的表现是「打开文件后什么都没发生」，两头都看不出毛病。
///
/// 这条测试之所以**不会 flaky**：「状态」只在 `poll` 里变，而这里在第一次
/// `poll` 之前就调了 `show` —— 状态此刻**一定**还是 `Initializing`。
/// 也就是说这不是「赌它还没回话」，是确定的。
#[test]
fn nothing_is_sent_before_the_handshake_finishes() {
    let mut harness = Harness::start(&[]);

    harness.show(URI, "fn main() {}\n");

    // 给它一段时间去犯错（服务器那边回话很快，真要发早就发出去了）
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        harness.count_in_log("didOpen"),
        0,
        "握手还没完成就发了 didOpen —— 服务器会静默忽略它"
    );

    // 而握完手之后再报一遍事实，这次**必须**发出去。
    //
    // ⚠️ 这就是「每轮都要调 show」的理由：调用方没有「等握完手再报告」这个
    //    时机可用 —— 它手上只有「屏幕上是这样」这一个事实，
    //    什么时候能说出去是会话自己的事。
    harness.wait_ready();
    harness.show(URI, "fn main() {}\n");
    assert!(
        harness.wait_for_log("didOpen").contains("version=1"),
        "第一次同步该是 didOpen 且版本号为 1"
    );
}

/// 第一次报告一个文件 → `didOpen`，带着语言、版本 1、和**完整正文**。
#[test]
fn the_first_report_opens_the_document_with_its_full_text() {
    let mut harness = Harness::start(&[]);
    harness.wait_ready();

    harness.show(URI, "fn main() {\n    let x = 1;\n}\n");

    let line = harness.wait_for_log("didOpen");
    assert!(line.contains("language=rust"), "认错语言了：{line}");
    assert!(line.contains("version=1"), "版本号该从 1 开始：{line}");
    // ⚠️ 断言**整份正文**（转义过的一行），不只是长度：
    //    只比长度的话，内容错位、丢字符、多一个换行都发现不了。
    assert!(
        line.contains(r#"text="fn main() {\n    let x = 1;\n}\n""#),
        "正文不是整份原样：{line}"
    );
}

/// ⚠️ **一个字没动就别开口。**
///
/// 这不只是省事：一条内容相同的 `didChange` 也会让服务器**重算一遍整个文件**，
/// 而主循环每轮都会来报一次事实（每 50ms）—— 没有这条判断，
/// 一个空转的编辑器会让语言服务器 20 次/秒地做无用功。
#[test]
fn repeating_the_same_text_does_not_touch_the_wire() {
    let mut harness = Harness::start(&[]);
    harness.wait_ready();

    let text = "fn main() {}\n";
    for _ in 0..20 {
        harness.show(URI, text);
    }
    harness.wait_for_log("didOpen");

    assert_eq!(
        harness.count_in_log("didChange"),
        0,
        "报了一模一样的文本，却还是发了 didChange"
    );
    assert_eq!(
        harness.count_in_log("didOpen"),
        1,
        "同一个文件被 didOpen 了好几次"
    );
}

/// 改了字 → `didChange`（全量），版本号递增。
#[test]
fn a_change_sends_the_whole_text_with_a_rising_version() {
    let mut harness = Harness::start(&[]);
    harness.wait_ready();

    harness.show(URI, "fn main() {}\n");
    harness.wait_for_log("didOpen");

    harness.show(URI, "fn main() { let x = 1; }\n");
    let first = harness.wait_for_log("version=2");
    assert!(
        first.contains("didChange"),
        "第二次同步该是 didChange：{first}"
    );
    assert!(
        first.contains(r#"text="fn main() { let x = 1; }\n""#),
        "全量同步该把**整份新文本**发过去：{first}"
    );

    harness.show(URI, "fn main() { let x = 2; }\n");
    let second = harness.wait_for_log("version=3");
    assert!(second.contains("version=3"), "版本号没递增：{second}");
}

/// 换文件 → **先 `didClose` 旧的，再 `didOpen` 新的**。
///
/// ⚠️ 顺序是这件事的全部要点。不关的话服务器会**继续分析一个我们早就不看的
/// 文件**：白烧 CPU 和内存，还可能推来一份属于那个文件的诊断把我们搅乱。
#[test]
fn switching_files_closes_the_old_one_before_opening_the_new_one() {
    const OTHER: &str = "file:///D:/fake-project/src/lib.rs";

    let mut harness = Harness::start(&[]);
    harness.wait_ready();

    harness.show(URI, "fn main() {}\n");
    harness.wait_for_log("didOpen");

    harness.show(OTHER, "pub fn f() {}\n");
    harness.wait_for_log("didClose");

    let log = harness.session.log_tail();
    let closed = log
        .iter()
        .position(|line| line.contains("didClose") && line.contains(URI))
        .expect("没有关掉旧文件");
    let opened = log
        .iter()
        .position(|line| line.contains("didOpen") && line.contains(OTHER))
        .expect("没有打开新文件");

    assert!(
        closed < opened,
        "先开了新文件才关旧文件 —— 中间那段时间两个都开着；日志：{log:#?}"
    );
    assert!(
        log.iter().any(|line| line.contains(OTHER)),
        "新文件的 uri 没出现过；日志：{log:#?}"
    );
}

// ---------- 诊断 ----------

/// 服务器推来的诊断，能被翻译成 [`Outcome::Diagnostics`] 交出来。
///
/// 走的是完整的一条路：会话发 `didOpen` → 假服务器分析 → 推回来 →
/// 我们解析成 `Diagnostic`。中间任何一环断了，这里都拿不到东西。
#[test]
fn a_pushed_diagnostic_arrives_as_an_outcome() {
    let mut harness = Harness::start(&["--push-diagnostics"]);
    harness.wait_ready();

    // 正文里带 `BROKEN` → 假服务器会推一个第 5 行（0 基 4）的错误
    harness.show(URI, "// BROKEN\nfn main() {}\n");

    let push = harness.wait_diagnostics();
    assert_eq!(push.uri, URI, "推送说的不是我们打开的那个文件");
    assert_eq!(push.diagnostics.len(), 1, "该正好一条");
    assert_eq!(push.diagnostics[0].line, 4, "行号该是 0 基的 4");

    // ⚠️ 正文必须**原样**穿过来。我们一个词都不改 ——
    //    它是用户唯一能拿去搜的抓手（`cargo` 报的是同一句话）。
    assert_eq!(push.diagnostics[0].message, "fake: this file is broken");
}

/// ⚠️ **「改好了」必须能到达屏幕上。**
///
/// 服务器推一个**空数组**说的是「这个文件现在没毛病」。要是在哪一层把它
/// 当成「没有消息」吞掉，改好的错误就会**永远留在行号栏上** ——
/// 而且表现为「什么都没发生」，你根本不会怀疑到这里。
///
/// 所以这条测试的重点不是「收到了一份推送」，而是**它是一份空得可以交出去的推送**。
#[test]
fn an_empty_push_still_comes_through_as_a_clean_verdict() {
    let mut harness = Harness::start(&["--push-diagnostics"]);
    harness.wait_ready();

    // 先弄坏（会收到一份非空的）
    harness.show(URI, "// BROKEN\nfn main() {}\n");
    harness.wait_diagnostics();

    // 再改好（正文里没有 BROKEN 了 → 服务器推空数组）
    harness.show(URI, "// fixed\nfn main() {}\n");
    let push = harness.wait_diagnostics();

    assert!(
        push.diagnostics.is_empty(),
        "改好了却还收到 {} 条诊断",
        push.diagnostics.len()
    );
}

/// 服务器死了 → 会话报 `Broken`（**而不是一直等下去**）。
#[test]
fn a_server_that_dies_is_reported_as_broken() {
    let mut harness = Harness::start(&["--die-after-initialize"]);

    let deadline = Instant::now() + common::FAKE_SERVER_WAIT;
    while Instant::now() < deadline {
        harness.pump();
        if let Some(Outcome::Broken(why)) =
            harness.take(|outcome| matches!(outcome, Outcome::Broken(_)))
        {
            assert!(!why.is_empty(), "断了总得说一句为什么");
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "服务器已经死了，会话却没发现；日志：{:#?}",
        harness.session.log_tail()
    );
}

// ---------- 真服务器 ----------

/// **拿真的 `rust-analyzer` 走一遍完整的路。**
///
/// 标 `#[ignore]`：它依赖本机装了什么，别人机器上没有不该算测试失败。
/// 手动跑：
///
/// ```text
/// cargo test --test lsp_session -- --ignored --nocapture
/// ```
///
/// ## 它值这一趟
///
/// 上面所有测试用的都是**我们自己写的假服务器**。假服务器是按我们对协议的
/// 理解写的 —— 万一理解错了，它会跟着一起错：两边自洽、测试全绿，
/// 而真服务器根本不认。**只有它点头，才说明我们照着的那份规范没读错。**
///
/// 它要回答三个假服务器回答不了的问题：
///
/// 1. **`didChange` 只给整份文本（不带 `range`），它认不认？**
///    我们实现的是全量同步，而 RA 在 `initialize` 里通常宣称 `change: 2`
///    （增量）。规范说「不带 `range` 就是整份替换」，理论上是合法的 ——
///    但**理论**正是要被验证的那个东西。
/// 2. **改了字之后旧的那份诊断会不会被顶掉？**
/// 3. **把错误改好之后，它会不会推一份空的回来？**
///    这条最要紧：空的那份收不到，改好的错误就会永远留在屏幕上。
///
/// ## 它**不碰**磁盘上的任何文件
///
/// 用的是一个真实存在的路径（`src/check.rs`），但正文是 `didOpen` /
/// `didChange` 里带过去的 —— 服务器以内存里的为准，磁盘上的那份原封不动。
/// 所以跑完这个仓库里一个字节都没变。
#[test]
#[ignore = "需要本机装了 rust-analyzer，手动跑"]
fn the_whole_loop_with_the_real_rust_analyzer() {
    let root = std::env::current_dir().expect("拿不到当前目录");
    let root_uri = stbd::lsp::uri::path_to_uri(&root).expect("项目根该能转成 uri");

    let path = root.join("src").join("check.rs");
    let uri = stbd::lsp::uri::path_to_uri(&path).expect("文件路径该能转成 uri");
    let clean = std::fs::read_to_string(&path).expect("读不到 src/check.rs");

    let mut session = Session::start("rust-analyzer", &[], Some(root.as_path()), Some(&root_uri))
        .expect("起不来 rust-analyzer（本机装了吗？）");

    // ---- ① 握手 ----
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut ready = false;
    while Instant::now() < deadline && !ready {
        ready = session.poll().contains(&Outcome::Ready);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        ready,
        "和真的 rust-analyzer 握手没走完；日志：{:#?}",
        session.log_tail()
    );

    // ---- ② 打开一份**故意弄坏**的文本 ----
    //
    // ⚠️ 真服务器**开场会先推几份空的**（它还没分析完），所以这里等的不是
    //    「第一份推送」，而是「第一份**非空**的推送」—— 等错了的话，
    //    那份「还没分析完」的空推送会让下面那句断言假失败。
    let broken = format!("{}\nfn 这个函数根本写不对 {{", clean);
    session.show(&uri, "rust", &broken).expect("show 不该失败");

    let first = wait_for_push(&mut session, "弄坏之后", |push| {
        !push.diagnostics.is_empty()
    });
    eprintln!("① 弄坏之后收到 {} 条诊断", first.diagnostics.len());
    for diagnostic in &first.diagnostics {
        eprintln!("   {}", diagnostic.describe());
    }

    // ---- ③ 改好 → 必须来一份**空的**，而且**一直空着** ----
    //
    // ⚠️ 为什么还要「一直空着」这一步：
    //
    //     rust-analyzer 在**收到文档改动的当下**会先推一份空的（「我先把这个
    //     文件的旧诊断擦掉」），然后才重新分析。所以只等「一份空的」是不够的 ——
    //     假如那次 `didChange` **根本没生效**（比如它不认不带 `range` 的全量更新），
    //     我们照样会先收到那份「擦掉」的空推送，测试**假绿**，
    //     而屏幕上过几秒又会被错误重新填满。
    //
    //     所以真正的判据是：**一份空的之后，接下来这段时间里再也没有非空的**。
    //     这一步同时验证了 `didChange` 真的被认（不认的话错误会卷土重来），
    //     和「改好了」真的能到达我们手上。
    session.poll(); // 把改之前可能残留的清掉（那些是按旧文本算的）
    session.show(&uri, "rust", &clean).expect("show 不该失败");

    let cleared = wait_for_push(&mut session, "改好之后", |push| {
        push.diagnostics.is_empty()
    });
    eprintln!("② 改好之后收到一份空推送（{}）", cleared.uri);

    if let Some(again) = stays_clean_for(&mut session, Duration::from_secs(5)) {
        panic!(
            "擦掉之后错误又回来了 —— 那次 didChange 根本没生效（{} 条：{:?}）",
            again.diagnostics.len(),
            again
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.describe())
                .collect::<Vec<_>>()
        );
    }
    eprintln!("③ 之后 5 秒一直是干净的 —— 全量 didChange 它认");
}

/// 一直等到一份**满足条件**的诊断推送。
///
/// ⚠️ 条件是调用方给的，不是「第一份」—— 真服务器会先推几份中间状态的，
/// 把「等到了一份推送」当成「等到了那个结论」，是这类测试最常见的一种假绿。
fn wait_for_push(
    session: &mut Session,
    stage: &str,
    want: impl Fn(&PublishDiagnostics) -> bool,
) -> PublishDiagnostics {
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut seen = 0;
    while Instant::now() < deadline {
        for outcome in session.poll() {
            if let Outcome::Diagnostics(push) = outcome {
                seen += 1;
                if want(&push) {
                    return push;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "{stage}一直没等到符合条件的诊断（中间看到 {seen} 份）；日志：{:#?}",
        session.log_tail()
    );
}

/// 接下来这段时间里，如果收到**非空**的推送就把它交出来（没有就返回 `None`）。
fn stays_clean_for(session: &mut Session, window: Duration) -> Option<PublishDiagnostics> {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        for outcome in session.poll() {
            if let Outcome::Diagnostics(push) = outcome
                && !push.diagnostics.is_empty()
            {
                return Some(push);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

// ---------- 探针：换个项目的文件它认不认 ----------

/// 【探针】**根在 A 的服务器，看不看得见 B 项目的文件？**
///
/// 标 `#[ignore]`：依赖本机装了 rust-analyzer，还依赖**旁边正好有另一个
/// Cargo 项目**（这里找的是 `../minesweeper`）。两个条件都不该让别人跑测试失败。
///
/// 跑法：
///
/// ```text
/// cargo test --test lsp_session -- --ignored --nocapture probe_
/// ```
///
/// ## 它为什么在这里
///
/// 我原本只是**推断**「换一个项目就认不出来了」，没量过。推断和事实是两回事，
/// 所以留一个随时能重跑的探针 —— 免得把一句没验过的话当成设计依据。
///
/// ## ✅ 实测结论（2026-09-14）
///
/// ```text
/// 根在：file:///D:/MyProjects/command_code_editor
/// 报告的文件：file:///D:/MyProjects/minesweeper/src/main.rs（弄坏过）
/// == 45 秒里收到 0 份推送 ==
/// ```
///
/// **一份都没有** —— 连一份「空的」都没有。这比「报错了」更值得注意：
/// 说明它不是「分析完了发现没毛病」，而是**根本没把这份报告当回事**。
///
/// 对照组见下一条（同一个文件、只把根换成它自己那个项目）。
///
/// ## ✅ 实测结论（2026-09-14）
///
/// ```text
/// 根在：file:///D:/MyProjects/command_code_editor
/// 报告的文件：file:///D:/MyProjects/minesweeper/src/main.rs（弄坏过）
/// == 45 秒里收到 0 份推送 ==
/// ```
///
/// **一份都没有** —— 连一份「空的」都没有。这比“报错了”更值得注意：
/// 说明它不是「分析完了发现没毛病」，而是**根本没把这份报告当回事**。
///
/// 对照组见下一条（同一个文件、只把根换成它自己那个项目）。
///
/// ## 它**不碰**磁盘
///
/// 正文是 `didOpen` 里带过去的，服务器以内存里的为准。
#[test]
#[ignore = "探针；需要 rust-analyzer + 旁边另一个 Cargo 项目，手动跑"]
fn probe_a_file_that_belongs_to_another_project() {
    let root = std::env::current_dir().expect("拿不到当前目录");
    let other = root.parent().expect("没有上一级").join("minesweeper");
    let target = other.join("src").join("main.rs");
    assert!(
        target.exists(),
        "靶子不在：{}（换个项目改这里）",
        target.display()
    );

    let root_uri = stbd::lsp::uri::path_to_uri(&root).expect("根该能转成 uri");
    let target_uri = stbd::lsp::uri::path_to_uri(&target).expect("靶子该能转成 uri");

    // 故意弄坏：末尾少一个 `}`
    let clean = std::fs::read_to_string(&target).expect("读不到靶子");
    let broken = format!("{}\nfn 坏 {{", clean);

    let mut session = Session::start("rust-analyzer", &[], Some(root.as_path()), Some(&root_uri))
        .expect("起不来 rust-analyzer");

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut ready = false;
    while Instant::now() < deadline && !ready {
        ready = session.poll().contains(&Outcome::Ready);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(ready, "握手没走完；日志：{:#?}", session.log_tail());

    eprintln!("根在：{root_uri}");
    eprintln!("报告的文件：{target_uri}");

    session
        .show(&target_uri, "rust", &broken)
        .expect("show 失败");

    // 给它足够长的时间说话（真项目加载要几秒）
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut pushes = 0;
    while Instant::now() < deadline {
        for outcome in session.poll() {
            if let Outcome::Diagnostics(push) = outcome {
                pushes += 1;
                eprintln!(
                    "推送 {}：{} 条 ← {}",
                    pushes,
                    push.diagnostics.len(),
                    push.uri
                );
                for diagnostic in &push.diagnostics {
                    eprintln!("    {}", diagnostic.describe());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // 探针**不断言**结论 —— 它只把事情报出来。
    // 结论：45 秒 0 份推送（见文件头和这条测试的注释里的实测记录）。
    eprintln!("== 结论：45 秒里收到 {pushes} 份推送，见上面每一份的内容 ==");
}

/// 【探针】同一个文件，**把根换到它自己那个项目**，这回它说话吗？
///
/// 跟上面那条是**一组对照**：靶子、正文、弄坏的方式全都一样，
/// 只改了 `initialize` 里那个 `rootUri`。两条一起跑，才能说明
/// 「看不见」到底是文件的属性还是根的问题。
///
/// ```text
/// cargo test --test lsp_session -- --ignored --nocapture probe_
/// ```
///
/// ## ✅ 实测结论（2026-09-14）
///
/// ```text
/// 根在：file:///D:/MyProjects/minesweeper
/// 报告的文件：file:///D:/MyProjects/minesweeper/src/main.rs（同一个文件、同样弄坏）
/// 非空推送 1：2 条 ← file:///d:/MyProjects/minesweeper/src/main.rs
///     19: error: Syntax Error: expected function arguments
///     19: error: Syntax Error: expected R_CURLY
/// ```
///
/// 两条探针合起来就是一次**只改一个变量的对照实验**：一条 0 份、一条 2 条 ——
/// 所以「看不见」不是那个文件的属性，**是根决定的**。
#[test]
#[ignore = "探针；需要 rust-analyzer + 旁边另一个 Cargo 项目，手动跑"]
fn probe_the_same_file_with_its_own_project_as_the_root() {
    let here = std::env::current_dir().expect("拿不到当前目录");
    let other = here.parent().expect("没有上一级").join("minesweeper");
    let target = other.join("src").join("main.rs");
    assert!(target.exists(), "靶子不在：{}", target.display());

    // ⚠️ 这里就是**唯一**跟上面那条不一样的地方：根换成了它自己那个项目
    let root_uri = stbd::lsp::uri::path_to_uri(&other).expect("根该能转成 uri");
    let target_uri = stbd::lsp::uri::path_to_uri(&target).expect("靶子该能转成 uri");

    let clean = std::fs::read_to_string(&target).expect("读不到靶子");
    let broken = format!("{}\nfn 坏 {{", clean);

    let mut session = Session::start("rust-analyzer", &[], Some(other.as_path()), Some(&root_uri))
        .expect("起不来 rust-analyzer");

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut ready = false;
    while Instant::now() < deadline && !ready {
        ready = session.poll().contains(&Outcome::Ready);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(ready, "握手没走完；日志：{:#?}", session.log_tail());

    eprintln!("根在：{root_uri}");
    eprintln!("报告的文件：{target_uri}");

    session
        .show(&target_uri, "rust", &broken)
        .expect("show 失败");

    let deadline = Instant::now() + Duration::from_secs(45);
    let mut pushes = 0;
    let mut real = 0;
    while Instant::now() < deadline {
        for outcome in session.poll() {
            if let Outcome::Diagnostics(push) = outcome {
                pushes += 1;
                if !push.diagnostics.is_empty() {
                    real += 1;
                    eprintln!(
                        "非空推送 {}：{} 条 ← {}",
                        real,
                        push.diagnostics.len(),
                        push.uri
                    );
                    for diagnostic in &push.diagnostics {
                        eprintln!("    {}", diagnostic.describe());
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    eprintln!("== 结论：45 秒里收到 {pushes} 份推送，其中非空 {real} 份 ==");
}
