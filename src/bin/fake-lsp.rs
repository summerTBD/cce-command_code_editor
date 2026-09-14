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

fn run() -> Result<(), String> {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut writer = stdout.lock();

    loop {
        let body = match framing::read_message(&mut reader) {
            Ok(Some(body)) => body,
            // 客户端关了管道 → 我们收工。
            // （真实服务器还会通过 `initialize` 里的 `processId` 盯着客户端死没死，
            //   那是另一套互为保险的机制。这里靠管道就够。）
            Ok(None) => return Ok(()),
            Err(err) => return Err(format!("framing error: {err}")),
        };

        let message: Value =
            serde_json::from_str(&body).map_err(|err| format!("bad JSON from client: {err}"))?;

        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        // ⚠️ `"id": null` **不算**有 id —— 那也是一条通知。
        //    JSON-RPC 里 id 只能是字符串或数字，通知就是干脆没这个字段。
        let id = match message.get("id") {
            None | Some(Value::Null) => None,
            Some(other) => Some(other.clone()),
        };

        match (method, id) {
            // 请求：必须回一条带同样 id 的回应，否则对面会一直等
            ("initialize", Some(id)) => respond(&mut writer, id, initialize_result())?,
            ("shutdown", Some(id)) => respond(&mut writer, id, Value::Null)?,

            // 通知：不回
            ("exit", None) => return Ok(()),

            // 其余请求一律回 null。真实服务器会报「方法不存在」，
            // 但我们这一步只关心「有没有回」，回什么不重要。
            (_, Some(id)) => respond(&mut writer, id, Value::Null)?,

            // 其余通知：忽略（`initialized` / `didOpen` / `didChange` 都走这里）
            (_, None) => {}
        }
    }
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
