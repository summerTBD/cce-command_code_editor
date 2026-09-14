//! 会话 —— 把「一个客户端」和「屏幕上现在是什么」绑在一起 —— session.rs 的职责
//!
//! ## 为什么还要多一层
//!
//! [`Client`] 只懂「收发消息」，它**不知道我们在编辑哪个文件**。这一层补上：
//!
//! ```text
//! main.rs 每次输入之后都说一句：        Session 自己决定要不要开口：
//!   "现在屏幕上是这个文件、这些文本"  →   换文件了   → didClose(旧的) + didOpen(新的)
//!                                        只是改了字 → didChange（全量）
//!                                        一个字没动 → 什么都不发
//! ```
//!
//! 这一层存在的**全部价值**就是那个「自己决定」：调用方不用记得「我刚才是
//! 不是换过文件」「上次发的是什么」，它只管把**事实**报上来。
//!
//! ## ⚠️ 为什么是「整份文本比一比」而不是「编辑时告诉我改了哪一行」
//!
//! 因为**改文本的入口太多**（打字、回车、退格、删除、粘贴、撤销、重做、
//! 命令式编辑……），散在 `app.rs` / `update.rs` / `commands.rs` 里，
//! 没有哪个单一位置能保证「所有编辑都经过我」。
//!
//! 漏一个入口的后果很具体：**屏幕上改了，服务器那份没改** ——
//! 于是它按旧代码给你报错，你在新代码上找不到那个错。这种 bug 看起来像
//! 「rust-analyzer 有时候抽风」，实际是我们这边漏了一处。
//!
//! 比整份文本就不一样了：**状态是最新的那一份说了算**，漏不掉。
//! 代价是每次要比一遍字符串（不做的时候是零成本），以及全量同步更费带宽 ——
//! 但我们面对的是本地进程和管道，这个代价我们付得起。
//!
//! ## ⚠️ 谁来 `drop` 它
//!
//! 会话里的 [`Client`] 一被 drop 就会 `shutdown` 那个子进程。
//! 所以「什么时候丢会话」等于「什么时候杀服务器」—— 由 `main.rs` 决定。

use std::io;
use std::path::Path;

use serde_json::json;

use super::client::{Client, Event, State};
use super::diagnostics::{self, PublishDiagnostics};
use super::message::Message;

/// 服务器那边发生的一件事，**翻译成我们这边关心的样子**。
///
/// 为什么不直接把 [`Event`] 抛出去：那样 `main.rs` 就得认识
/// `Message::Notification { method, params }` 那一套，还得自己判断
/// 「这条通知是不是诊断」—— 那是协议知识，属于这一层。
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// 握手完成 —— 从此它认得我们这个项目了。
    ///
    /// **只报一次**。「连上了」值得说一句，「还连着」不值得每轮都说。
    ///
    /// ⚠️ 正因为只报一次，它是**一次性**的：[`Session::poll`] 交出来的东西
    /// 调用方必须处理掉 —— **丢掉那一次，就永远收不到了**。
    /// （这不是理论上的担心：写测试时就因为一个「顺手 poll 一下、把结果扔了」
    /// 的辅助函数踩过一次。）只需要一处消费者就没问题，`main.rs` 里正是如此。
    Ready,
    /// 它说某个文件现在有哪些毛病。**空 = 那个文件没毛病**（不是「没有消息」）。
    Diagnostics(PublishDiagnostics),
    /// 这条线没了：进程退了，或者协议错了。
    ///
    /// **之后不会再有事件** —— 调用方该把这个会话丢掉（`drop` 会顺带收尸）。
    Broken(String),
}

/// 我们已经告诉服务器的那个文档。
struct Opened {
    uri: String,
    /// 上次发过去的那份文本 —— 用来判断「要不要开口」。
    text: String,
    /// 规范要求的版本号，每次 `didChange` 递增。
    version: i64,
}

/// 一个会话。
pub struct Session {
    client: Client,
    /// `None` = 还没给它看过任何文件。
    opened: Option<Opened>,
    /// `Ready` 只报一次（见 [`Outcome::Ready`]）。
    announced_ready: bool,
}

impl Session {
    /// 起一个服务器并发出 `initialize`。
    ///
    /// **不等握手完成** —— 那可能是好几秒（冷启动要加载整个 crate 图），
    /// 而这是在主循环里调用的，等它就是冻住界面。握完手会从
    /// [`Session::poll`] 里报一个 [`Outcome::Ready`] 出来。
    ///
    /// `root_uri` 是**项目根**（`Cargo.toml` 所在的那个目录），不是当前文件 ——
    /// 服务器要它才知道去哪儿找整个项目。
    pub fn start(
        command: &str,
        args: &[&str],
        cwd: Option<&Path>,
        root_uri: Option<&str>,
    ) -> io::Result<Self> {
        let mut client = Client::spawn(command, args, cwd)?;
        client.initialize(root_uri)?;
        Ok(Self {
            client,
            opened: None,
            announced_ready: false,
        })
    }

    /// 报告事实：**现在屏幕上是这个文件、这些文本**。
    ///
    /// 调用方只管说事实，要不要告诉服务器由这里决定（见文件头）。
    /// 一个字都没变时它什么都不做 —— 所以尽管每轮都调。
    ///
    /// `language_id` 用 LSP 规定的那一套名字（`rust` / `toml` / …），
    /// 由 [`language_id`] 从路径后缀推出来。
    pub fn show(&mut self, uri: &str, language_id: &str, text: &str) -> io::Result<()> {
        // ⚠️ 握手没完成之前**什么都不能发**（`notify` 会拒），那不是错误、
        //    是「还不到时候」。这里直接跳过 —— 下一次 `show` 还会来，
        //    那时候就发出去了。（所以「打开文件后立刻有诊断」这件事，
        //    靠的就是后面每轮都调一遍。）
        if self.client.state() != State::Ready {
            return Ok(());
        }

        match &self.opened {
            // 还是同一个文件
            Some(opened) if opened.uri == uri => {
                // ⚠️ 一个字都没变就**别开口**。哪怕是「内容相同的一次 didChange」，
                //    也会让服务器重算一遍 —— 而这件事每轮都在发生。
                if opened.text == text {
                    return Ok(());
                }
                let version = opened.version + 1;
                self.client.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": version },
                        // ⚠️ 全量同步：`contentChanges` 里**没有 `range`** 就是
                        //    「整份文本换成这个」。服务器在 `initialize` 的回应里
                        //    承诺过 `textDocumentSync: Full`，两边对得上。
                        "contentChanges": [ { "text": text } ]
                    }),
                )?;
                self.opened = Some(Opened {
                    uri: uri.to_string(),
                    text: text.to_string(),
                    version,
                });
            }
            // 换文件了，或者第一次
            _ => {
                // 先把旧的关掉。不关的话它会**继续分析一个我们早就不看的文件**：
                // 白烧 CPU 和内存，还可能推来一份诊断把我们搅乱。
                if let Some(old) = self.opened.take() {
                    self.client.notify(
                        "textDocument/didClose",
                        json!({ "textDocument": { "uri": old.uri } }),
                    )?;
                }
                self.client.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id,
                            "version": 1,
                            "text": text,
                        }
                    }),
                )?;
                self.opened = Some(Opened {
                    uri: uri.to_string(),
                    text: text.to_string(),
                    version: 1,
                });
            }
        }
        Ok(())
    }

    /// 非阻塞地收一轮消息（把攒下的一口气全取走）。没有就返回空 `Vec`。
    pub fn poll(&mut self) -> Vec<Outcome> {
        let mut outcomes = Vec::new();

        // 一口气全取走而不是取一条：服务器说的话是连绵不断的（每次输入都可能
        // 推一份诊断），落后了就该尽快追上，而不是每轮只消化一条。
        while let Some(event) = self.client.next_event() {
            match event {
                Event::Message(Message::Notification { method, params }) => {
                    // 现在只认诊断推送。
                    //
                    // 其它的（`window/logMessage`、`window/showMessage`、
                    // `$/progress`……）暂时不往界面上抛：它已经记在客户端的
                    // 日志里了，需要时翻得到，而界面上多一行噪音的代价
                    // 比少一行线索大。
                    if let Some(push) = diagnostics::from_notification(&method, &params) {
                        outcomes.push(Outcome::Diagnostics(push));
                    }
                }
                // 请求已经被客户端自己答完了（见 `Client::absorb`），
                // 回应我们也都在那儿对过账了 —— 到这儿已经没什么要做的。
                Event::Message(_) => {}
                Event::Broken(why) => {
                    outcomes.push(Outcome::Broken(why));
                    // ⚠️ 断了就**真的没有以后了**，接着取只会拿到重复的 Broken
                    break;
                }
            }
        }

        // ⚠️ 放在循环**外面**：状态只可能在 `next_event` 里变，所以出来看一眼
        //    就够，而且能保证「一条消息都没有的那几轮」不会漏报。
        if self.client.state() == State::Ready && !self.announced_ready {
            self.announced_ready = true;
            outcomes.push(Outcome::Ready);
        }

        outcomes
    }

    /// 客户端记的账（它的 stderr + 我们自己的记账）。
    ///
    /// 排查用的：服务器为什么不理我们、它到底在抱怨什么，都在这儿。
    pub fn log_tail(&self) -> Vec<String> {
        self.client.log_tail()
    }
}

/// 文件路径 → LSP 里的 `languageId`。
///
/// 认不出来的就当 `plaintext`：这个字段只影响服务器**把文件当什么语言看**，
/// 猜错不会报错，顶多是那个文件没有诊断 —— 而猜错的另一种写法
/// （根据内容猜）要复杂得多，收益也看不出在哪。
///
/// ⚠️ 大小写不敏感：Windows 上 `MAIN.RS` 和 `main.rs` 是同一个东西。
pub fn language_id(path: &str) -> &'static str {
    let extension = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "rs" => "rust",
        "toml" => "toml",
        "json" => "json",
        "md" => "markdown",
        "py" => "python",
        "js" => "javascript",
        "ts" => "typescript",
        "c" | "h" => "c",
        "cpp" | "cc" | "hpp" => "cpp",
        "sh" => "shellscript",
        _ => "plaintext",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extension_decides_the_language() {
        assert_eq!(language_id("D:\\p\\src\\main.rs"), "rust");
        assert_eq!(language_id("Cargo.toml"), "toml");
        assert_eq!(language_id("README.md"), "markdown");
    }

    /// ⚠️ Windows 上 `MAIN.RS` 和 `main.rs` 是同一个文件 —— 认成 `plaintext`
    /// 的话，那个文件就永远不会有诊断，而且看起来像「服务器不管它」。
    #[test]
    fn the_extension_is_matched_case_insensitively() {
        assert_eq!(language_id("MAIN.RS"), "rust");
        assert_eq!(language_id("Cargo.TOML"), "toml");
    }

    #[test]
    fn an_unknown_or_missing_extension_is_plaintext() {
        assert_eq!(language_id("Makefile"), "plaintext");
        assert_eq!(language_id("D:\\p\\weird.zzz"), "plaintext");
        // 别把 `.gitignore` 这种「点开头没后缀」当成后缀为 `gitignore` 的文件 ——
        // 结果其实一样（都是 plaintext），但至少不会因为路径里有个点就乱认
        assert_eq!(language_id(".gitignore"), "plaintext");
    }
}
