//! 文档列表 —— 「打开过哪些文档」以及「上一个 / 下一个」。
//!
//! 这里**不读文件**，只是一串路径加一个下标，纯粹的导航记忆。
//! 真正的加载在 main.rs（IO 只能在入口层做）。
//!
//! ## 为什么是列表，不是栈
//!
//! 栈只能 push/pop，退到哪就只看得见哪。而我们要的是像 vim 的 arglist 那样：
//!
//! - 能**前后走**：[`DocumentList::previous_path`] / [`DocumentList::next_path`]
//!   （`q` 的「返回上一级」和 `:next` 都靠它）；
//! - 能**看到全貌**：`:ls` 列出所有打开过的文档；
//! - 能**删掉用不到的**：`:forget <n>`，删完下标仍指着同一个文档。
//!
//! 内部是 `Vec<String>` + `index`，不涉及任何 IO —— 真正去读盘的是调用方。
//!
//! ## 三个语义细节
//!
//! - [`DocumentList::remember`] 在**打开成功之后**调用。已在列表里的路径不会
//!   产生重复项，而是直接跳过去（否则 `:stbd a` `:stbd b` `:stbd a` 会攒出三份 a）。
//! - `previous_path()` / `next_path()` 只是**窥探，不移动位置**。位置只在
//!   `remember()` 里移动 —— 也就是「成功打开了才认为你走过去了」。
//!   否则 `:back` 之后读盘失败，列表说你在 A、屏幕上却是 B。
//! - 没有上一个时 `previous_path()` 返回 `None`；「那就退出程序」是调用方
//!   （update.rs）的决定，不在这里做。

/// 打开过的文档列表 + 当前位置。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocumentList {
    /// 按打开顺序排列的路径
    paths: Vec<String>,
    /// 当前在 `paths` 中的下标（0 基）；列表为空时无意义
    index: usize,
}

impl DocumentList {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记住一个刚打开的文档，并把当前位置移到它上面。
    ///
    /// 路径已在列表里就跳过去（不追加），这样来回打开同一个文件不会把列表撑爆。
    pub fn remember(&mut self, path: &str) {
        match self.paths.iter().position(|existing| existing == path) {
            Some(existing) => self.index = existing,
            None => {
                self.paths.push(path.to_string());
                self.index = self.paths.len() - 1;
            }
        }
    }

    /// 当前文档路径
    pub fn current(&self) -> Option<String> {
        self.paths.get(self.index).cloned()
    }

    /// 上一个文档的路径 —— **只是看一眼，不移动位置**。
    ///
    /// 位置由 [`DocumentList::remember`] 在打开成功后移动，所以这里
    /// 不会出现「列表说在前面、屏幕上却是原来那个」的错位。已经在第一个返回 `None`。
    pub fn previous_path(&self) -> Option<String> {
        self.index
            .checked_sub(1)
            .and_then(|at| self.paths.get(at))
            .cloned()
    }

    /// 下一个文档的路径（同样不移动位置）。已经在最后一个返回 `None`。
    pub fn next_path(&self) -> Option<String> {
        self.paths.get(self.index + 1).cloned()
    }

    /// 从列表里去掉第 `at` 个（0 基；`:ls` 给用户看的是 1 基），返回被去掉的路径。
    ///
    /// 删完会让下标继续指向**同一个文档**（而不是同一个下标）：
    /// 删的是当前项之前的 → 下标前移；删的是当前项本身 → 尽量指向原来后面那个。
    /// 越界返回 `None`。
    pub fn forget(&mut self, at: usize) -> Option<String> {
        if at >= self.paths.len() {
            return None;
        }
        let removed = self.paths.remove(at);

        if self.paths.is_empty() {
            self.index = 0;
        } else if at < self.index {
            self.index -= 1;
        } else if self.index >= self.paths.len() {
            self.index = self.paths.len() - 1;
        }
        Some(removed)
    }

    /// 列表长度
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// 列表是否为空
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// 当前下标（0 基）
    pub fn index(&self) -> usize {
        self.index
    }

    /// 一行摘要，给 `:ls` 用。当前文档前面带 `*`，序号从 1 开始。
    pub fn describe(&self) -> String {
        if self.paths.is_empty() {
            return "no documents".to_string();
        }
        self.paths
            .iter()
            .enumerate()
            .map(|(position, path)| {
                if position == self.index {
                    format!("{} *{path}", position + 1)
                } else {
                    format!("{} {path}", position + 1)
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list_of(paths: &[&str]) -> DocumentList {
        let mut list = DocumentList::new();
        for path in paths {
            list.remember(path);
        }
        list
    }

    #[test]
    fn empty_list_has_no_current_and_nowhere_to_go() {
        let list = DocumentList::new();
        assert!(list.is_empty());
        assert_eq!(list.current(), None);
        assert_eq!(list.previous_path(), None);
        assert_eq!(list.next_path(), None);
    }

    #[test]
    fn remember_appends_in_order_and_lands_on_the_new_entry() {
        let mut list = DocumentList::new();
        list.remember("a.txt");
        assert_eq!(list.current().as_deref(), Some("a.txt"));
        assert_eq!(list.index(), 0);

        list.remember("b.txt");
        assert_eq!(list.current().as_deref(), Some("b.txt"));
        assert_eq!(list.index(), 1);
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn remember_reuses_an_existing_entry_instead_of_duplicating() {
        let list = list_of(&["a.txt", "b.txt", "a.txt"]);
        assert_eq!(list.len(), 2, "重复路径不该产生第二个条目");
        assert_eq!(list.current().as_deref(), Some("a.txt"));
        assert_eq!(list.index(), 0, "应该直接跳回已有那条");
        assert_eq!(list.next_path().as_deref(), Some("b.txt"));
    }

    #[test]
    fn previous_and_next_paths_peek_without_moving_the_position() {
        let mut list = list_of(&["a", "b", "c"]); // 当前 = c

        assert_eq!(list.current().as_deref(), Some("c"));
        assert_eq!(list.previous_path().as_deref(), Some("b"));
        assert_eq!(list.next_path(), None, "已经在最后一个");
        assert_eq!(list.current().as_deref(), Some("c"), "窥探不该挪位置");

        // 真正「走过去」靠 remember（打开成功后才调）
        list.remember("b");
        assert_eq!(list.current().as_deref(), Some("b"));
        assert_eq!(list.previous_path().as_deref(), Some("a"));
        assert_eq!(list.next_path().as_deref(), Some("c"));

        list.remember("a");
        assert_eq!(list.previous_path(), None, "已经在第一个");
        assert_eq!(list.next_path().as_deref(), Some("b"));
    }

    #[test]
    fn forget_before_current_keeps_pointing_at_the_same_document() {
        let mut list = list_of(&["a", "b", "c"]); // 当前 = c（下标 2）
        assert_eq!(list.forget(0).as_deref(), Some("a"));

        assert_eq!(list.len(), 2);
        assert_eq!(
            list.current().as_deref(),
            Some("c"),
            "删别人不该改变当前文档"
        );
        assert_eq!(list.index(), 1);
    }

    #[test]
    fn forget_the_current_entry_moves_to_the_next_one() {
        let mut list = list_of(&["a", "b", "c"]);
        list.remember("b"); // 当前 = b（下标 1）
        assert_eq!(list.forget(1).as_deref(), Some("b"));

        assert_eq!(list.current().as_deref(), Some("c"), "应落到原来后面那个");
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn forget_the_last_entry_falls_back_and_empty_list_is_safe() {
        let mut list = list_of(&["a", "b"]);
        assert_eq!(list.forget(1).as_deref(), Some("b"));
        assert_eq!(list.current().as_deref(), Some("a"));

        assert_eq!(list.forget(0).as_deref(), Some("a"));
        assert!(list.is_empty());
        assert_eq!(list.current(), None);
        assert_eq!(list.previous_path(), None);
        assert_eq!(list.next_path(), None);
    }

    #[test]
    fn forget_out_of_range_is_none() {
        let mut list = list_of(&["a"]);
        assert_eq!(list.forget(1), None);
        assert_eq!(list.forget(99), None);
        assert_eq!(list.len(), 1, "越界不该改动列表");
    }

    #[test]
    fn describe_marks_the_current_entry_with_a_star() {
        let mut list = list_of(&["a.txt", "b.txt"]);
        // remember 后停在最后一项
        assert_eq!(list.describe(), "1 a.txt  2 *b.txt");

        list.remember("a.txt");
        assert_eq!(list.describe(), "1 *a.txt  2 b.txt");

        assert_eq!(DocumentList::new().describe(), "no documents");
    }
}
