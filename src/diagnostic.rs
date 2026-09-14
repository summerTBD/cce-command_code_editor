//! 诊断：一行代码有什么毛病 —— diagnostic.rs 的职责
//!
//! ## ⚠️ 只有行，没有列
//!
//! LSP 给的是「区间」（起止行列），我们**只要行**。这不是省事，是刻意的：
//!
//! - 读代码的人**本来就是重读那一行**，不是去数第几列
//! - 那个区间的精度**时常兑现不了** —— 借用检查报在使用处，真正的修法
//!   在定义处或者更早那次借用。把红线画在某个字母底下，等于承诺了
//!   「问题就在这儿」，而它经常在骗人
//! - 一旦不要列，**UTF-16 坐标换算这件事就从项目里彻底消失了**
//!   （LSP 按 UTF-16 数，我们的 rope 按字符数 —— 唯一的差别是 emoji，
//!   而既然不画下划线，那个差别也就不存在了）
//!
//! ## 为什么单独一个文件
//!
//! 这个东西有三个使用者，而且它们**不该互相认识**：
//!
//! - `lsp/` 产出它（服务器推来的）
//! - `app.rs` 存着它
//! - `ui.rs` 拿它染色
//!
//! 放进任何一边都会让另外两条本不相干的依赖长出来（比如 `app → lsp`，
//! 核心状态依赖一个「功能模块」—— 方向是反的）。所以它自己占一个文件：
//! **谁都用，谁都不欠谁。**
//!
//! ⚠️ 所以这里**不认识 JSON**。「LSP 的数字 1/2/3/4 对应哪一档」是边界层的
//! 事（`lsp::diagnostics`）—— 数据就是数据，协议就是协议。

use std::cmp::Reverse;

/// 一条诊断。
///
/// 字段只有三样，这是**我们的全部理解** —— 服务器给的 `code`、`tags`、
/// `relatedInformation`、`data` 我们一概不要（见文件头）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// **0 基**行号（LSP 原样，不做 +1）。
    ///
    /// 0 基存着、显示时再变 1 基 —— 跟协议对齐的那一侧保持原样，
    /// 免得在两层之间来回加减、迟早多算一次。显示是 `ui.rs` 的事。
    pub line: usize,
    pub severity: Severity,
    /// 服务器给的**原文**，一个词都不改。
    ///
    /// 不改是有实际用处的：你可以把它整句丢进搜索框，而 `cargo build`
    /// 报的是同一句话。我们一「润色」，你就失去了唯一能搜的抓手。
    pub message: String,
}

impl Diagnostic {
    /// 给状态栏/列表看的一行：`行号（1 基）: 级别: 原文`。
    ///
    /// 前缀 `error:` / `warning:` 是我们**加的标签**，不是对原文的改动 ——
    /// 原文整句还在后面，照样能搜。
    pub fn describe(&self) -> String {
        format!(
            "{}: {}: {}",
            self.line + 1,
            self.severity.name(),
            self.message
        )
    }
}

/// 把一个文件现在所有的毛病排成一份能直接看的清单（`:errors` 用的就是它）。
///
/// 按行号排序 —— 服务器给的顺序不保证，而看清单的人是在心里**顺着文件往下走**的。
/// 同一行上有好几条时，最严重的排前面（跟行号栏上「一个格子只能染一种颜色」
/// 是同一条规矩）。
///
/// ⚠️ 每条**都带着行号**。行号在这里不是装饰：它是唯一能把这一行对回文件的线索。
/// 以后要加「回车跳过去」，也是拿这个行号去跳。
///
/// ⚠️ 级别名（`error:`）是我们**加**的标签，后面的原文一个字没改 ——
/// 原文是用户唯一能拿去搜的抓手（`cargo` 报的是同一句话）。
pub fn list_text(diagnostics: &[Diagnostic]) -> String {
    let mut sorted: Vec<&Diagnostic> = diagnostics.iter().collect();
    sorted.sort_by_key(|diagnostic| (diagnostic.line, Reverse(diagnostic.severity.weight())));
    sorted
        .iter()
        .map(|diagnostic| diagnostic.describe())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 严重程度。跟 LSP 的四档一一对应。
///
/// ⚠️ 我们只给 `Error` / `Warning` 染色 —— `Information` / `Hint` 太吵
/// （一个「可以加 `const` 哦」的提示不值得把行号染成第三种颜色）。
/// 它们在 `:errors` 列表里还是看得见的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

impl Severity {
    /// 写给用户看的名字。
    pub fn name(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "information",
            Self::Hint => "hint",
        }
    }

    /// 它配得上一个颜色标记吗（只有错误和警告配）。
    pub fn is_marked(self) -> bool {
        matches!(self, Self::Error | Self::Warning)
    }

    /// 有多严重 —— **数字越大越严重**。
    ///
    /// ⚠️ 写成一个显式的 `match` 而不是靠 `#[derive(PartialOrd)]`：
    /// 派生出来的顺序是**枚举的书写顺序**，而那个顺序是「按 LSP 的数字排的」——
    /// 一个改起来毫无理由去怀疑的小改动（比如把 Hint 挪到前面看起来更整齐），
    /// 会悄无声息地把「同一行上哪个诊断说了算」倒过来。
    pub fn weight(self) -> u8 {
        match self {
            Self::Hint => 0,
            Self::Information => 1,
            Self::Warning => 2,
            Self::Error => 3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_errors_and_warnings_get_a_colour_mark() {
        assert!(Severity::Error.is_marked());
        assert!(Severity::Warning.is_marked());
        assert!(!Severity::Information.is_marked());
        assert!(!Severity::Hint.is_marked());
    }

    /// 错误最重、提示最轻，四档严格排开。
    #[test]
    fn weight_ranks_errors_above_everything_else() {
        assert!(Severity::Error.weight() > Severity::Warning.weight());
        assert!(Severity::Warning.weight() > Severity::Information.weight());
        assert!(Severity::Information.weight() > Severity::Hint.weight());
    }

    /// 行号存的是 **0 基**，`describe` 显示时才 +1。
    #[test]
    fn describe_shows_a_one_based_line_and_the_level_as_a_label() {
        let diagnostic = Diagnostic {
            line: 36,
            severity: Severity::Error,
            message: "cannot find value `fo` in this scope".to_string(),
        };
        assert_eq!(
            diagnostic.describe(),
            "37: error: cannot find value `fo` in this scope"
        );
    }

    /// 预览一条真实的 rust-analyzer 诊断长什么样（`severity: 2` = 警告）。
    #[test]
    fn describe_labels_a_warning_too() {
        let diagnostic = Diagnostic {
            line: 462,
            severity: Severity::Warning,
            message: "function `folder_name` is never used".to_string(),
        };
        assert_eq!(
            diagnostic.describe(),
            "463: warning: function `folder_name` is never used"
        );
    }

    // ---------- :errors 的清单 ----------

    fn at(line: usize, severity: Severity, message: &str) -> Diagnostic {
        Diagnostic {
            line,
            severity,
            message: message.to_string(),
        }
    }

    #[test]
    fn the_list_shows_one_line_per_problem_with_its_line_number() {
        let text = list_text(&[
            at(4, Severity::Error, "boom"),
            at(0, Severity::Warning, "meh"),
        ]);
        assert_eq!(text, "1: warning: meh\n5: error: boom");
    }

    /// ⚠️ 按行号排序，不是照着服务器给的顺序。
    ///
    /// 服务器报的顺序不保证，而看清单的人是在心里**顺着文件往下走**的 ——
    /// 乱序的清单得来回找，比没有清单还烦。
    #[test]
    fn the_list_is_sorted_by_line() {
        let text = list_text(&[
            at(30, Severity::Error, "c"),
            at(2, Severity::Error, "a"),
            at(17, Severity::Error, "b"),
        ]);
        assert_eq!(text, "3: error: a\n18: error: b\n31: error: c");
    }

    /// 同一行有好几条时，最严重的排前面（跟行号栏「一个格子只染一种颜色」同规）。
    #[test]
    fn the_worst_problem_on_a_line_comes_first() {
        let text = list_text(&[
            at(7, Severity::Warning, "次要的"),
            at(7, Severity::Error, "主要的"),
        ]);
        assert_eq!(text, "8: error: 主要的\n8: warning: 次要的");
    }

    #[test]
    fn an_empty_list_is_empty_text() {
        assert_eq!(list_text(&[]), "");
    }
}
