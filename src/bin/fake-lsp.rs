//! 一个「假的语言服务器」—— **只在开发和测试里用**
//!
//! ## 它为什么存在
//!
//! 真实服务器（rust-analyzer）**很难让它按剧本出错**：你想测「它在握手中途死掉」，
//! 总不能去改它的源码；你想测「它发来的消息被切成两半」，得碰运气。
//!
//! 而这些东西恰恰是最容易写错、又最难复现的地方。所以有一个**能听指挥**的假服务器：
//! 我们可以让它延迟、乱序、发垃圾、半路消失 —— 全部确定性地重放。
//!
//! 拿真的 rust-analyzer 去测这些，测出来的是运气，不是结论。
//!
//! ## 它不是产品的一部分
//!
//! 它是同一个 crate 里的第二个二进制，`cargo build --release` 会把它一起编出来。
//! 我们接受这点代价 —— 另一条路（用 `required-features` 把它圈起来）的代价是
//! **`cargo test` 会静默跳过所有 LSP 测试**，那是更糟的事。
//!
//! ## 两条铁律
//!
//! ⚠️ **stdout 只许写长度头 + 正文**，一个多余的 `println!` 都会污染协议流，
//! 让客户端读到一段看不懂的字节。要打日志请用 `eprintln!`（stderr 是自由的）。
//!
//! ⚠️ **收到的请求必须逐条回**。不回的话客户端会一直等 —— 而它等的时候
//! 看起来一切正常，只是什么都不发生。
//!
//! ## 剧本
//!
//! 默认只会规规矩矩握手。命令行参数能让它演别的：
//!
//! | 参数                     | 它干什么                                             |
//! | ------------------------ | ---------------------------------------------------- |
//! | `--ask`                  | 握完手**反过来问客户端一个问题**（两小节配置）       |
//! | `--die-after-initialize` | 回应完 `initialize` 就立刻退出——模拟握手中途死掉 |
//! | `--push-diagnostics`     | 每收到一次 `didOpen`/`didChange` 就**推一份诊断** |
//!
//! ## 它还会把「文档同步」记下来
//!
//! `didOpen` / `didChange` / `didClose` 这三条是**客户端自言自语** ——
//! 它们不落盘、不打印，只在管道里过一下。想测「改了字到底发没发 `didChange`」
//! 「换文件有没有先把旧的那个关掉」，就只能让服务器把它看见的写出来。
//! 所以这三条会被打成一行行格式固定的日志（stderr），供测试断言：
//!
//! ```text
//! textDocument/didOpen uri=file:///D:/a.rs language=rust version=1 text="fn main() {}\n"
//! textDocument/didChange uri=file:///D:/a.rs version=2 text="fn main() {①}\n"
//! textDocument/didClose uri=file:///D:/a.rs
//! ```

use std::io::{self, BufReader, Write};

use serde_json::{Value, json};
use stbd::lsp::framing;

fn main() {
    if let Err(err) = run() {
        // ⚠️ 这里只能走 stderr —— stdout 是协议通道
        eprintln!("fake-lsp: {err}");
        std::process::exit(2);
    }
}

/// 假服务器的「剧本」。
#[derive(Default)]
struct Script {
    /// 回应完 `initialize` 之后**反过来问客户端一个问题**。
    ///
    /// 真实服务器一定会这么干（`client/registerCapability`、
    /// `workspace/configuration`……），而这正是客户端最容易漏掉的一件事：
    /// 漏了不会报错，只会「什么都不发生」。所以必须能测。
    ask: bool,
    /// 回应完 `initialize` 就立刻退出 —— 模拟**握手中途死掉**。
    die_after_initialize: bool,
    /// 每收到一次文档同步就**推一份诊断**回去。
    ///
    /// 真实服务器就是这么干的：你改了字，它重新分析，然后把「这个文件现在
    /// 有什么毛病」推回来。
    ///
    /// 推的内容由**正文里有没有 `BROKEN`** 决定：有就是一个错误，
    /// 没有就是一个**空数组**。这两种都有用 —— 后者是「改好了」，
    /// 而它恰恰是最容易在客户端那边被当成「没有消息」丢掉的那一种。
    push_diagnostics: bool,
}

impl Script {
    fn from_args() -> Self {
        let mut script = Self::default();
        for arg in std::env::args().skip(1) {
            match arg.as_str() {
                "--ask" => script.ask = true,
                "--die-after-initialize" => script.die_after_initialize = true,
                "--push-diagnostics" => script.push_diagnostics = true,
                other => eprintln!("fake-lsp: 不认识的参数 {other:?}（忽略）"),
            }
        }
        script
    }
}

/// 我们反过来问客户端的那两小节配置的条数。
///
/// ⚠️ 写成「两」是故意的：客户端回数组时**必须回两条**，
/// 少一条就对不上了。日志里会把这个数字打出来，测试就断言它。
const ASK_ITEMS: usize = 2;

fn run() -> Result<(), String> {
    let script = Script::from_args();

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut writer = stdout.lock();

    // 我们自己问出去的那个问题的 id，和「问了没 / 答了没」
    let ask_id = json!(9001);
    let mut asked = false;
    let mut answered = false;

    loop {
        let body = match framing::read_message(&mut reader) {
            Ok(Some(body)) => body,
            // 客户端关了管道 → 我们收工。
            // （真实服务器还会通过 `initialize` 里的 `processId` 盯着客户端死没死，
            //   那是另一套互为保险的机制。这里靠管道就够。）
            Ok(None) => {
                if asked && !answered {
                    // 把这件事**喊出来**：测试靠这行字判定客户端漏了回话
                    eprintln!("NEVER ANSWERED");
                    return Err("客户端到死都没回答我们的问题".to_string());
                }
                return Ok(());
            }
            Err(err) => return Err(format!("framing error: {err}")),
        };

        let message: Value =
            serde_json::from_str(&body).map_err(|err| format!("bad JSON from client: {err}"))?;

        let method = message.get("method").and_then(Value::as_str);
        // ⚠️ `"id": null` **不算**有 id —— 那也是一条通知。
        //    JSON-RPC 里 id 只能是字符串或数字，通知就是干脆没这个字段。
        let id = match message.get("id") {
            None | Some(Value::Null) => None,
            Some(other) => Some(other.clone()),
        };

        // 先认领「这是不是我们那个问题的回答」—— 回应是没有 method 的。
        // 把条数也打出来，这样测试能一并钉住「回的数组长度对不对」。
        if method.is_none() && id.as_ref() == Some(&ask_id) {
            answered = true;
            let items = message
                .get("result")
                .and_then(Value::as_array)
                .map_or(usize::MAX, Vec::len);
            eprintln!("ANSWERED items={items}");
            continue;
        }

        match (method, id) {
            // 请求：必须回一条带同样 id 的回应，否则对面会一直等
            (Some("initialize"), Some(id)) => {
                respond(&mut writer, id, initialize_result())?;

                if script.die_after_initialize {
                    eprintln!("DIE AFTER INITIALIZE");
                    return Ok(());
                }
                if script.ask {
                    asked = true;
                    ask_client(&mut writer, &ask_id)?;
                }
            }
            (Some("shutdown"), Some(id)) => respond(&mut writer, id, Value::Null)?,

            // 通知：不回
            (Some("exit"), None) => return Ok(()),

            // 其余请求一律回 null。真实服务器会报「方法不存在」，
            // 但这里只关心「有没有回」，回什么不重要。
            (Some(_), Some(id)) => respond(&mut writer, id, Value::Null)?,

            // 其余通知：不回答，但把文档同步一类**记下来**（`initialized` 忽略）
            (Some(method), None) => {
                if let Some(sync) = read_document_message(method, &message) {
                    eprintln!("{}", sync.line);
                    if script.push_diagnostics
                        && let Some(text) = sync.text
                    {
                        push_diagnostics(&mut writer, &sync.uri, &text)?;
                    }
                }
            }

            // 别的东西的回应：不关我们的事
            (None, Some(_)) => {}

            (None, None) => return Err("既没有 method 也没有 id".to_string()),
        }
    }
}

/// 「文档同步」那三条通知里我们关心的事实。
struct DocumentSync {
    /// 打进日志的那一行（格式固定，供测试断言）。
    line: String,
    uri: String,
    /// 正文。`didClose` 没有正文。
    text: Option<String>,
}

/// 读一条「文档同步」通知（不是这三条就返回 `None`）。
///
/// ⚠️ 正文用 `json!` 转义着打进日志（变成 `"a\nb"`）：一行就是一条，测试能按行找。
///    直接打原文的话，一个换行就把日志切成两行，找起来全是坑。
///
/// 字段缺了就返回 `None`（**不猜**）：这份日志是拿来做断言的，
/// 一份猜出来的日志比没有日志更糟 —— 它会让测试以为对方发对了。
fn read_document_message(method: &str, message: &Value) -> Option<DocumentSync> {
    let params = message.get("params")?;
    let document = params.get("textDocument")?;
    let uri = document.get("uri")?.as_str()?.to_string();

    let (line, text) = match method {
        "textDocument/didOpen" => {
            let language = document.get("languageId")?.as_str()?;
            let version = document.get("version")?.as_i64()?;
            let text = document.get("text")?.as_str()?.to_string();
            (
                format!(
                    "{method} uri={uri} language={language} version={version} text={}",
                    json!(text)
                ),
                Some(text),
            )
        }
        "textDocument/didChange" => {
            let version = document.get("version")?.as_i64()?;
            // ⚠️ 只认「没有 `range` 的那种」——那才是全量同步。
            //    带 `range` 的增量更新我们**根本没实现**，要是客户端哪天发出来了，
            //    这份日志宁可报缺字段（测试立刻红），也不该默默当成全量放过去。
            let change = params.get("contentChanges")?.get(0)?;
            if change.get("range").is_some() {
                return None;
            }
            let text = change.get("text")?.as_str()?.to_string();
            (
                format!("{method} uri={uri} version={version} text={}", json!(text)),
                Some(text),
            )
        }
        "textDocument/didClose" => (format!("{method} uri={uri}"), None),
        _ => return None,
    };
    Some(DocumentSync { line, uri, text })
}

/// 这个文件现在有没有毛病 —— 由正文里有没有 `BROKEN` 决定。
///
/// 故意做得这么笨：它是**测试用的判据**，越简单越不容易自己出错。
/// 行号写死 4（0 基）：测试那边就断言第 5 行有错。
fn push_diagnostics<W: Write>(writer: &mut W, uri: &str, text: &str) -> Result<(), String> {
    let diagnostics = if text.contains("BROKEN") {
        json!([{
            "range": {
                "start": { "line": 4, "character": 0 },
                "end": { "line": 4, "character": 10 }
            },
            "severity": 1,
            "message": "fake: this file is broken"
        }])
    } else {
        // ⚠️ 空数组 = 「这个文件现在没毛病」。
        //    这不是「没有消息」，客户端要是把它当成后者，改好的错误就会永远
        //    留在屏幕上 —— 所以这条路径必须在测试里真的走一遍。
        json!([])
    };

    let body = json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": { "uri": uri, "diagnostics": diagnostics },
    })
    .to_string();
    framing::write_message(writer, &body).map_err(|err| format!("write failed: {err}"))
}

/// 反过来问客户端一个问题 —— 真实服务器天天这么干。
/// ⚠️ 这也正是要测的东西：客户端**必须回答**。
/// 它要是漏了，我们就会一直等下去，而两边看起来都很正常。
fn ask_client<W: Write>(writer: &mut W, id: &Value) -> Result<(), String> {
    let items: Vec<Value> = (0..ASK_ITEMS)
        .map(|n| json!({ "section": format!("fake.{n}") }))
        .collect();
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "workspace/configuration",
        "params": { "items": items },
    })
    .to_string();
    framing::write_message(writer, &body).map_err(|err| format!("write failed: {err}"))
}

/// `initialize` 的回应。故意非常小 —— 只承诺**一件事**：
/// 文档同步用全量（`textDocumentSync: 1` 就是 `Full`）。
///
/// ⚠️ 「服务器宣称支持什么」和「客户端打算实现什么」必须对得上。
/// 真实服务器在这里报的是一大坨，但报得越多、客户端欠的债就越多。
fn initialize_result() -> Value {
    json!({
        "capabilities": {
            "textDocumentSync": 1
        },
        "serverInfo": {
            "name": "fake-lsp",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

/// 回一条响应。`id` 必须**原样**带回去 —— 那是客户端认领这条回应的唯一凭据。
fn respond<W: Write>(writer: &mut W, id: Value, result: Value) -> Result<(), String> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
    .to_string();
    framing::write_message(writer, &body).map_err(|err| format!("write failed: {err}"))
}
