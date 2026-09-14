//! 把服务器推来的诊断翻译成我们自己的 —— diagnostics.rs 的职责
//!
//! 这一层是**两个世界的边界**：
//!
//! ```text
//! 服务器的世界                         我们的世界
//! textDocument/publishDiagnostics  →  PublishDiagnostics { uri, diagnostics }
//!   range{start,end}（行列 UTF-16）  →  line（0 基，**列丢掉**）
//!   severity 1/2/3/4                 →  Severity
//!   message / code / tags / data     →  message（**只要原文**）
//! ```
//!
//! 「丢掉」的东西都是**有意**丢的，理由写在 `diagnostic.rs` 的文件头。
//!
//! ## ⚠️ 空数组不是「没有消息」
//!
//! ```text
//! {"diagnostics": [], "uri": "file:///.../a.rs"}
//! ```
//!
//! 这是「**这个文件现在没问题**」。不处理这一条，改好的错误标记会永远
//! 留在屏幕上 —— 而且因为「什么都不发生」，你根本不会怀疑到这里。

use serde_json::Value;

use crate::diagnostic::{Diagnostic, Severity};

/// 一次诊断推送的内容。
#[derive(Debug, Clone, PartialEq)]
pub struct PublishDiagnostics {
    /// 原始 uri（**没有归一化**）。
    ///
    /// 留着原样的原因：它可能不是我们的当前文件（服务器也会报别的文件），
    /// 而「是不是同一个文件」这个判断要用 [`same_file`](super::uri::same_file)，
    /// 不能在解析这一步就自作主张地抹平。
    pub uri: String,
    /// 这个文件现在的全部诊断。**空 = 没毛病。**
    pub diagnostics: Vec<Diagnostic>,
}

/// 这条通知是诊断推送吗？是的话翻译过来。
///
/// 不是这个通知、或者参数畸形，返回 `None` —— **诊断是锦上添花的东西，
/// 不值得为它报错**（对面乱说一句，不该影响编辑器的其它部分）。
pub fn from_notification(method: &str, params: &Value) -> Option<PublishDiagnostics> {
    if method != "textDocument/publishDiagnostics" {
        return None;
    }

    let uri = params.get("uri")?.as_str()?.to_string();

    // `diagnostics` 缺席时按**空**处理：就这个通知的语义而言，
    // 「没有这个字段」和「空数组」是同一件事（都没毛病）。
    let diagnostics: Vec<Diagnostic> = params
        .get("diagnostics")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(read_one).collect())
        .unwrap_or_default();

    Some(PublishDiagnostics { uri, diagnostics })
}

/// 一条诊断。缺胳膊少腿（没 `range` / 没 `message`）的直接丢掉。
///
/// 丢掉而不是用默认值补：一条**位置不明**的诊断，补出来的行号是假的，
/// 而假的行号比没有行号更糟 —— 它会引你去一行无关的代码。
fn read_one(item: &Value) -> Option<Diagnostic> {
    let line = item
        .get("range")?
        .get("start")?
        .get("line")?
        .as_u64()
        .map(|n| n as usize)?;

    let message = item.get("message")?.as_str()?.to_string();

    Some(Diagnostic {
        line,
        severity: severity(item.get("severity")),
        message,
    })
}

/// LSP 的 `severity` 数字 → 我们的级别。
///
/// 规范：`1` 错误、`2` 警告、`3` 信息、`4` 提示。字段可以**缺席**。
///
/// ⚠️ 缺席（或者给了个看不懂的值）时按 **Error** 算：一条说不清级别的诊断，
/// 当成警告会把它藏起来，当成错误最多是多看一眼。**宁可吵，不可漏。**
///
/// 这个映射放在这里而不是 `diagnostic.rs`：那边是**纯数据**，不该认识 JSON。
fn severity(value: Option<&Value>) -> Severity {
    match value.and_then(Value::as_i64) {
        Some(2) => Severity::Warning,
        Some(3) => Severity::Information,
        Some(4) => Severity::Hint,
        _ => Severity::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 一条**真实抓到过**的 rust-analyzer 推送（2026-09-14 实测，字段裁剪过）。
    const REAL_PAYLOAD: &str = r#"{
        "uri": "file:///d:/MyProjects/command_code_editor/src/lsp/client.rs",
        "diagnostics": [
            {
                "code": "dead_code",
                "data": { "rendered": "warning: function `folder_name` is never used\n..." },
                "message": "function `folder_name` is never used\n`#[warn(dead_code)]` (part of `#[warn(unused)]`) on by default",
                "range": { "start": { "character": 3, "line": 463 }, "end": { "character": 14, "line": 463 } },
                "severity": 2,
                "source": "rustc",
                "tags": [1]
            }
        ]
    }"#;

    fn parse(params: &Value) -> PublishDiagnostics {
        from_notification("textDocument/publishDiagnostics", params).expect("该认出这是诊断推送")
    }

    #[test]
    fn severity_follows_the_spec_numbers() {
        assert_eq!(severity(Some(&json!(1))), Severity::Error);
        assert_eq!(severity(Some(&json!(2))), Severity::Warning);
        assert_eq!(severity(Some(&json!(3))), Severity::Information);
        assert_eq!(severity(Some(&json!(4))), Severity::Hint);
    }

    /// ⚠️ 守着「宁可吵，不可漏」。
    #[test]
    fn a_missing_or_unknown_severity_is_treated_as_an_error() {
        assert_eq!(severity(None), Severity::Error);
        assert_eq!(severity(Some(&json!(null))), Severity::Error);
        // 规范从没允许过字符串，但对面可能乱来
        assert_eq!(severity(Some(&json!("warning"))), Severity::Error);
        assert_eq!(severity(Some(&json!(99))), Severity::Error);
    }

    #[test]
    fn a_real_payload_comes_through_with_line_and_message_intact() {
        let params: Value = serde_json::from_str(REAL_PAYLOAD).unwrap();
        let push = parse(&params);

        assert_eq!(
            push.uri,
            "file:///d:/MyProjects/command_code_editor/src/lsp/client.rs"
        );
        assert_eq!(push.diagnostics.len(), 1);

        let d = &push.diagnostics[0];
        assert_eq!(d.line, 463, "行号按 LSP 原样的 0 基存着");
        assert_eq!(d.severity, Severity::Warning);
        assert!(
            d.message
                .starts_with("function `folder_name` is never used"),
            "原文要完整留着：{}",
            d.message
        );
    }

    /// 行号是 **0 基**，解析这一层**不许**替显示层 +1。
    ///
    /// 在两层之间来回加减，迟早会多算一次 —— 而且症状是「颜色染错了行」，
    /// 看着像渲染的 bug，其实根在这儿。
    #[test]
    fn the_line_is_kept_zero_based() {
        let push = parse(&json!({
            "uri": "file:///D:/a.rs",
            "diagnostics": [{ "range": { "start": { "line": 0 } }, "message": "第一行" }]
        }));
        assert_eq!(push.diagnostics[0].line, 0);
    }

    /// ⚠️ **空数组 = 「这个文件现在没毛病」。**
    ///
    /// 这条必须能穿过这一层，而且要**原样**穿过去（一个空 `Vec`），
    /// 不能变成 `None` —— 那两边表达的是完全相反的意思。
    #[test]
    fn an_empty_array_means_this_file_is_clean() {
        let push = parse(&json!({
            "uri": "file:///D:/a.rs",
            "diagnostics": []
        }));
        assert!(push.diagnostics.is_empty());
    }

    #[test]
    fn a_missing_diagnostics_field_is_also_clean() {
        let push = parse(&json!({ "uri": "file:///D:/a.rs" }));
        assert!(push.diagnostics.is_empty());
    }

    #[test]
    fn several_diagnostics_all_come_through_in_order() {
        let push = parse(&json!({
            "uri": "file:///D:/a.rs",
            "diagnostics": [
                { "range": { "start": { "line": 3 } }, "message": "后一个", "severity": 1 },
                { "range": { "start": { "line": 1 } }, "message": "前一个", "severity": 2 }
            ]
        }));
        // 解析**不排序**：顺序是服务器给的，要排也是显示那一层的事
        assert_eq!(push.diagnostics.len(), 2);
        assert_eq!(push.diagnostics[0].message, "后一个");
        assert_eq!(push.diagnostics[0].severity, Severity::Error);
        assert_eq!(push.diagnostics[1].message, "前一个");
        assert_eq!(push.diagnostics[1].severity, Severity::Warning);
    }

    /// 缺 `range` 的诊断**丢掉**，而不是拿默认值补一个行号 ——
    /// 假的行号会把你引到一行无关的代码上。
    #[test]
    fn a_diagnostic_without_a_position_is_dropped() {
        let push = parse(&json!({
            "uri": "file:///D:/a.rs",
            "diagnostics": [
                { "message": "我不知道自己在哪一行" },
                { "range": { "start": { "line": 5 } }, "message": "我知道" }
            ]
        }));
        assert_eq!(push.diagnostics.len(), 1);
        assert_eq!(push.diagnostics[0].message, "我知道");
    }

    #[test]
    fn a_diagnostic_without_a_message_is_dropped() {
        let push = parse(&json!({
            "uri": "file:///D:/a.rs",
            "diagnostics": [{ "range": { "start": { "line": 5 } } }]
        }));
        assert!(push.diagnostics.is_empty());
    }

    #[test]
    fn a_payload_without_a_uri_is_refused() {
        assert_eq!(
            from_notification(
                "textDocument/publishDiagnostics",
                &json!({ "diagnostics": [] })
            ),
            None
        );
    }

    #[test]
    fn other_notifications_are_not_ours() {
        assert_eq!(
            from_notification("window/logMessage", &json!({ "message": "hi" })),
            None
        );
        assert_eq!(from_notification("$/progress", &json!({})), None);
    }
}
