//! JSON-RPC 2.0 —— message.rs 的职责
//!
//! [`framing`](crate::lsp::framing) 负责「把一串字节读出来」，这一层负责
//! 「那串字节到底在说什么」。
//!
//! ## 三种消息，而我们**最该小心的是分清楚它们**
//!
//! ```text
//! {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}   ← 请求（带 id，要回应）
//! {"jsonrpc":"2.0","method":"initialized","params":{}}         ← 通知（没 id，不回）
//! {"jsonrpc":"2.0","id":1,"result":{...}}                      ← 回应（没 method）
//! {"jsonrpc":"2.0","id":9,"method":"workspace/configuration"}   ← ⚠️ 服务器在问**我们**
//! ```
//!
//! 最后那条是最容易分错的一类：**它既有 `id` 又有 `method`**。看到 `id` 就当
//! 「这是谁给我的回应」是错的 —— 那是它在提问，而且**必须回答**，不然它会一直等。
//!
//! 所以分类的依据不是「有没有 id」，而是**「有没有 method」**：
//!
//! | 有 `method` | 有 `id` | 是什么            | 该怎么办     |
//! | ----------- | ------- | ----------------- | ------------ |
//! | ✅          | ✅      | 它在问我们        | **必须回**   |
//! | ✅          | ❌      | 它在通知我们      | 收下就行     |
//! | ❌          | ✅      | 它在回答我们      | 认领，交出去 |
//!
//! ## 为什么这一层能又纯又全地测
//!
//! 它不碰进程、不碰线程、不碰终端 —— 就是「一段文本进，一个枚举出」。
//! 所以可以拿各种畸形输入往死里喂。

use serde_json::{Value, json};

// JSON-RPC 规定好的几个错误码。我们用得到的只有这几个。
/// 收到的请求我们根本不认识这个方法
pub const METHOD_NOT_FOUND: i64 = -32601;
/// 方法认识，但参数不对
pub const INVALID_PARAMS: i64 = -32602;

/// 请求 / 回应里那个 `id`。
///
/// 规范说它可以是**数字或字符串**。我们自己发出去的永远用数字，
/// 但**回给对方的必须原样带回去** —— 所以两种都得装得下，
/// 不能假定「id 一定是个数字」。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    Number(i64),
    String(String),
}

impl RequestId {
    pub fn number(n: i64) -> Self {
        Self::Number(n)
    }

    /// 从 JSON 里取一个 id。浮点数、布尔、null 都不是合法的 id。
    fn from_json(value: &Value) -> Option<Self> {
        match value {
            Value::Number(n) => n.as_i64().map(Self::Number),
            Value::String(s) => Some(Self::String(s.clone())),
            // 浮点（`1.5` 也是 `Number`，但 `as_i64` 给它 `None` 了）、
            // null、布尔、数组、对象 —— 都不是合法的 id
            _ => None,
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Self::Number(n) => json!(n),
            Self::String(s) => json!(s),
        }
    }
}

/// 对方回过来的错误。
#[derive(Debug, Clone, PartialEq)]
pub struct ResponseError {
    pub code: i64,
    pub message: String,
}

/// 一条消息，按**我们要怎么处理它**分类（见文件头那张表）。
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// 我们问的，它答了
    Response {
        id: RequestId,
        result: Result<Value, ResponseError>,
    },
    /// 它反过来问我们 —— **必须回**
    Request {
        id: RequestId,
        method: String,
        params: Value,
    },
    /// 它主动通知我们（诊断就走这条）
    Notification { method: String, params: Value },
}

/// 把一段正文解析成消息。
///
/// 失败返回一句**能直接给人看**的话 —— 这条线一旦坏掉，那是唯一的线索。
pub fn parse(text: &str) -> Result<Message, String> {
    let value: Value = serde_json::from_str(text).map_err(|err| format!("not JSON: {err}"))?;

    // 规范要求这个字段，而且必须是 "2.0"。这里**严格**：
    // 宽容地接受别的东西，等于把「对面在乱说」这件事藏起来。
    match value.get("jsonrpc").and_then(Value::as_str) {
        Some("2.0") => {}
        other => return Err(format!("jsonrpc must be \"2.0\", got {other:?}")),
    }

    let method = value.get("method").and_then(Value::as_str);

    // ⚠️ `"id": null` **不算**有 id —— 那是一条通知。
    //    规范要求通知干脆不写这个字段，但很多实现会显式写 null。
    let id = match value.get("id") {
        None | Some(Value::Null) => None,
        Some(raw) => Some(RequestId::from_json(raw).ok_or_else(|| format!("invalid id: {raw}"))?),
    };

    let params = value.get("params").cloned().unwrap_or(Value::Null);

    match (method, id) {
        // 有 method 又有 id → **它在问我们**。这不是回应。
        (Some(method), Some(id)) => Ok(Message::Request {
            id,
            method: method.to_string(),
            params,
        }),

        (Some(method), None) => Ok(Message::Notification {
            method: method.to_string(),
            params,
        }),

        (None, Some(id)) => {
            let has_error = value.get("error").is_some();
            let has_result = value.get("result").is_some();
            let result = match (has_error, has_result) {
                (true, false) => Err(read_error(&value["error"])),
                (false, true) => Ok(value["result"].clone()),
                // 两个都有或者两个都没有 —— 都不是一条合法的回应
                _ => {
                    return Err("response must carry exactly one of `result` / `error`".to_string());
                }
            };
            Ok(Message::Response { id, result })
        }

        // 既没有 method 也没有 id：不是 JSON-RPC 消息
        (None, None) => Err("neither `method` nor `id` — not a JSON-RPC message".to_string()),
    }
}

fn read_error(value: &Value) -> ResponseError {
    ResponseError {
        // `code` 只能是整数；对面给了别的就当 0（并且 message 里也看得出不对）
        code: value.get("code").and_then(Value::as_i64).unwrap_or(0),
        message: value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("<no message>")
            .to_string(),
    }
}

// ===== 造消息 =====
//
// 返回值是**已经序列化好的 JSON 文本**，不是 `Value` —— 因为下一步就是
// 塞进 `framing::write_message`，中间再转一道没有意义。

/// 请求：我们要问一个问题，等它用同一个 `id` 回答。
pub fn request(id: &RequestId, method: &str, params: Value) -> String {
    with_params(
        json!({
            "jsonrpc": "2.0",
            "id": id.to_json(),
            "method": method,
        }),
        params,
    )
}

/// 通知：只告诉它一声，不等回答（`initialized` / `didOpen` / `exit` 都是）。
pub fn notification(method: &str, params: Value) -> String {
    with_params(
        json!({
            "jsonrpc": "2.0",
            "method": method,
        }),
        params,
    )
}

/// 回应：它问我们，我们答。
pub fn response(id: &RequestId, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id.to_json(), "result": result }).to_string()
}

/// 回一条错误。用它来回答「这个方法我不认识」——
/// **回一条错误也远远好过不回**：不回的话对面会一直等下去。
pub fn error_response(id: &RequestId, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id.to_json(),
        "error": { "code": code, "message": message },
    })
    .to_string()
}

/// 把 `params` 挂上去；值为 `null` 时干脆不写这个字段。
///
/// 写 `"params": null` 也是合法的，但**少一种形状就少一处要照顾的地方** ——
/// `exit` 之类本来就没有参数。
fn with_params(mut base: Value, params: Value) -> String {
    if !params.is_null()
        && let Some(object) = base.as_object_mut()
    {
        object.insert("params".to_string(), params);
    }
    base.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Message {
        parse(text).unwrap_or_else(|err| panic!("解析 {text:?} 失败：{err}"))
    }

    // ---------- 分类 ----------

    #[test]
    fn a_message_with_method_and_id_is_a_request_not_a_response() {
        // ⚠️ 这是本文件里最该守的一条。看到 id 就当「这是给我的回应」是错的 ——
        //    服务器也会反过来问我们，而且我们必须回答。
        let message = parsed(r#"{"jsonrpc":"2.0","id":9,"method":"workspace/configuration"}"#);
        match message {
            Message::Request { id, method, .. } => {
                assert_eq!(id, RequestId::Number(9));
                assert_eq!(method, "workspace/configuration");
            }
            other => panic!("应该是 Request，实际是 {other:?}"),
        }
    }

    #[test]
    fn a_message_with_only_a_method_is_a_notification() {
        let message = parsed(r#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics"}"#);
        assert_eq!(
            message,
            Message::Notification {
                method: "textDocument/publishDiagnostics".to_string(),
                params: Value::Null,
            }
        );
    }

    /// 通知显式写了 `"id": null` 也还是通知 —— 很多实现会这么写。
    #[test]
    fn an_explicit_null_id_still_means_notification() {
        let message = parsed(r#"{"jsonrpc":"2.0","id":null,"method":"initialized"}"#);
        assert!(
            matches!(message, Message::Notification { .. }),
            "{message:?}"
        );
    }

    #[test]
    fn a_successful_response_carries_the_result() {
        let message = parsed(r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#);
        match message {
            Message::Response { id, result } => {
                assert_eq!(id, RequestId::Number(1));
                assert!(result.unwrap()["capabilities"].is_object());
            }
            other => panic!("应该是 Response，实际是 {other:?}"),
        }
    }

    /// `shutdown` 的 `result` 就是个 `null` —— 这仍然是一条**有** result 的回应。
    #[test]
    fn a_null_result_is_still_a_successful_response() {
        let message = parsed(r#"{"jsonrpc":"2.0","id":2,"result":null}"#);
        assert_eq!(
            message,
            Message::Response {
                id: RequestId::Number(2),
                result: Ok(Value::Null),
            }
        );
    }

    #[test]
    fn an_error_response_carries_the_code_and_message() {
        let message = parsed(
            r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"method not found"}}"#,
        );
        assert_eq!(
            message,
            Message::Response {
                id: RequestId::Number(3),
                result: Err(ResponseError {
                    code: -32601,
                    message: "method not found".to_string(),
                }),
            }
        );
    }

    /// id 可以是字符串，而且**必须原样带回去**。
    #[test]
    fn a_string_id_round_trips() {
        let message = parsed(r#"{"jsonrpc":"2.0","id":"abc-1","method":"x"}"#);
        match &message {
            Message::Request { id, .. } => {
                assert_eq!(id, &RequestId::String("abc-1".to_string()))
            }
            other => panic!("{other:?}"),
        }

        let Message::Request { id, .. } = message else {
            unreachable!()
        };
        let text = response(&id, Value::Null);
        assert!(
            text.contains(r#""id":"abc-1""#),
            "id 必须原样带回去：{text}"
        );
    }

    // ---------- 畸形输入 ----------

    #[test]
    fn a_non_json_body_is_rejected() {
        let err = parse("这不是 JSON").unwrap_err();
        assert!(err.starts_with("not JSON"), "{err}");
    }

    #[test]
    fn a_missing_or_wrong_jsonrpc_field_is_rejected() {
        assert!(parse(r#"{"id":1,"result":null}"#).is_err(), "缺 jsonrpc");
        let err = parse(r#"{"jsonrpc":"1.0","id":1,"result":null}"#).unwrap_err();
        assert!(err.contains("must be \"2.0\""), "{err}");
    }

    #[test]
    fn a_message_with_neither_method_nor_id_is_rejected() {
        let err = parse(r#"{"jsonrpc":"2.0"}"#).unwrap_err();
        assert!(err.contains("neither"), "{err}");
    }

    #[test]
    fn a_response_with_neither_result_nor_error_is_rejected() {
        let err = parse(r#"{"jsonrpc":"2.0","id":1}"#).unwrap_err();
        assert!(err.contains("exactly one"), "{err}");
    }

    /// 浮点数不是合法的 id —— 这是规范里一个真实存在的边界。
    #[test]
    fn a_float_id_is_rejected() {
        let err = parse(r#"{"jsonrpc":"2.0","id":1.5,"method":"x"}"#).unwrap_err();
        assert!(err.contains("invalid id"), "{err}");
    }

    // ---------- 造消息 ----------

    #[test]
    fn a_request_has_id_and_method_and_params() {
        let text = request(
            &RequestId::number(1),
            "initialize",
            json!({"processId": 42}),
        );
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 1);
        assert_eq!(value["method"], "initialize");
        assert_eq!(value["params"]["processId"], 42);
    }

    /// 没有参数就不写 `params` 字段 —— 少一种形状就少一处要照顾的地方。
    #[test]
    fn a_notification_without_params_omits_the_field() {
        let text = notification("exit", Value::Null);
        let value: Value = serde_json::from_str(&text).unwrap();
        assert!(value.get("params").is_none(), "{text}");
        assert_eq!(value["method"], "exit");
        assert!(value.get("id").is_none(), "通知不能有 id：{text}");
    }

    #[test]
    fn an_error_response_is_parseable_as_an_error() {
        let text = error_response(&RequestId::number(5), METHOD_NOT_FOUND, "no such method");
        // 造出来的东西应该能被自己读回去 —— 这是这一层最省事的正确性检查
        match parse(&text).unwrap() {
            Message::Response { id, result } => {
                assert_eq!(id, RequestId::Number(5));
                let err = result.unwrap_err();
                assert_eq!(err.code, METHOD_NOT_FOUND);
                assert_eq!(err.message, "no such method");
            }
            other => panic!("{other:?}"),
        }
    }

    /// 造出来的每一条消息，都必须能被自己读回去。
    #[test]
    fn everything_we_build_can_be_parsed_back() {
        let cases = [
            request(&RequestId::number(1), "initialize", json!({})),
            notification("initialized", json!({})),
            notification("exit", Value::Null),
            response(&RequestId::number(2), json!({"ok": true})),
            error_response(&RequestId::Number(-1), INVALID_PARAMS, "bad"),
        ];
        for text in cases {
            assert!(parse(&text).is_ok(), "自己造的自己读不回来：{text}");
        }
    }
}
