//! 撤销 / 重做 —— 纯状态，不碰终端与文件。
//!
//! ## 为什么用「快照」
//!
//! 撤销有两种主流做法：
//!
//! 1. **快照**（本模块）：每步保存「编辑前的缓冲 + 光标」，撤销 = 恢复它。
//! 2. **逆操作日志**（vim 做法）：每步保存「怎么撤销这一步」，撤销 = 执行逆操作。
//!
//! 选 1 是因为它**实现极简且天然正确**——不必为每种编辑再实现一个逆操作，
//! 也就不会有「逆操作写错导致撤销后内容不对」这类经典 bug。
//!
//! 而这套做法原本的软肋——「存快照要拷一份全文」——已经消失：
//! 正文是 rope（见 [`crate::buffer`]），`Buffer::clone()` 是 **O(1)** 的写时复制。
//! 实测 4.5 MB 文档下，一份快照从 622 µs / 4.5 MB 降到 **3 ns / 几乎不占内存**。
//! 于是「实现简单」的优点留下，代价没了。
//!
//! ## 合并（coalescing）
//!
//! 一次「撤销步」**不等于**一次按键：连续的同类型编辑（同一行的连续输入 /
//! 连续删除）会被合并成一步，否则打一行字就要撤销几十次。
//! 光标移动、模式切换、保存、粘贴等会**断开**合并。
//!
//! ## 修订号
//!
//! 用单调递增的修订号而不是布尔值记录「有没有改过」，这样撤销回到上次保存的
//! 状态时，`dirty` 会自动变回 false。

use std::collections::VecDeque;

use crate::app::Cursor;
use crate::buffer::Buffer;

/// 默认的撤销步数上限（超出后丢弃最旧的一步），避免快照无限堆积。
///
/// 注意「一次撤销步」**不等于**一次按键：连续的同类输入会被合并成一步，
/// 所以 200 步对应的实际编辑量通常远不止 200 个按键。
pub const DEFAULT_UNDO_LIMIT: usize = 200;

/// 可合并的编辑类型：连续的同类型编辑会被并进同一个撤销步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditKind {
    /// 输入字符（含 Tab 展开出来的空格）
    Insert,
    /// 删除字符（Backspace / Delete）
    Delete,
}

/// 一次撤销快照：整份缓冲 + 光标位置 + 修订号。
///
/// `Buffer` 的 `clone()` 是 O(1)，所以存快照几乎不花时间和内存。
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    /// 编辑**之前**的整份缓冲
    pub(crate) buffer: Buffer,
    /// 执行这次编辑**之前**的光标位置
    pub(crate) cursor: Cursor,
    /// 这份快照对应的修订号
    revision: u64,
}

/// 撤销 / 重做栈。
///
/// `Clone` 不是随手加的：`App` 进虚拟视图（`:errors`）时要把整个文档状态
/// 存一份快照、退出来再放回去。里面的快照本身都是 O(1) 的（rope + `Arc`），
/// 所以这一份克隆是**几个指针**，不是几份文本。
#[derive(Debug, Clone)]
pub(crate) struct UndoStack {
    /// 已发生的编辑（每步保存「执行前」的状态）
    undo: VecDeque<Snapshot>,
    /// 被撤销掉的编辑（按撤销顺序倒着放，便于重做）
    redo: Vec<Snapshot>,
    /// 当前允许合并的「类型 + 行号」；与之不符就开启新的撤销步
    merge_key: Option<(EditKind, usize)>,
    /// 步数上限
    limit: usize,
    /// 当前状态的修订号（每次真正开启新的一步就 +1）
    revision: u64,
    /// 最近一次保存时的修订号
    saved_revision: u64,
}

impl UndoStack {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            undo: VecDeque::new(),
            redo: Vec::new(),
            merge_key: None,
            limit,
            revision: 0,
            saved_revision: 0,
        }
    }

    /// 这次编辑是否需要单独存一份快照？`false` = 可以并入上一步（省掉一次全文档拷贝）。
    pub(crate) fn needs_step(&self, kind: EditKind, row: usize) -> bool {
        self.merge_key != Some((kind, row))
    }

    /// 压入一份「编辑前」的快照，开启一个新的撤销步。
    ///
    /// `merge_key` 传 `Some(..)` 表示这一步允许后续同类编辑并入；传 `None` 表示独占一步。
    pub(crate) fn push_step(
        &mut self,
        buffer: Buffer,
        cursor: Cursor,
        merge_key: Option<(EditKind, usize)>,
    ) {
        self.redo.clear(); // 产生新分支后，旧的重做链失效
        self.undo.push_back(Snapshot {
            buffer,
            cursor,
            revision: self.revision,
        });
        self.revision += 1;
        self.merge_key = merge_key;
        while self.undo.len() > self.limit {
            self.undo.pop_front(); // 丢最旧的一步
        }
    }

    /// 断开合并：下一次编辑必须重新开一步。
    pub(crate) fn break_merge(&mut self) {
        self.merge_key = None;
    }

    /// 回滚刚刚 `push_step` 的那一步（那次编辑最终没有真正发生）。
    pub(crate) fn discard_last_step(&mut self) {
        if self.undo.pop_back().is_some() {
            self.revision = self.revision.saturating_sub(1);
        }
        self.merge_key = None;
    }

    /// 撤销一步：把当前状态存进 `redo`，返回要恢复的快照。
    pub(crate) fn undo(&mut self, buffer: Buffer, cursor: Cursor) -> Option<Snapshot> {
        let prev = self.undo.pop_back()?;
        self.redo.push(Snapshot {
            buffer,
            cursor,
            revision: self.revision,
        });
        self.revision = prev.revision;
        self.merge_key = None;
        Some(prev)
    }

    /// 重做一步：把当前状态存回 `undo`，返回要恢复的快照。
    pub(crate) fn redo(&mut self, buffer: Buffer, cursor: Cursor) -> Option<Snapshot> {
        let next = self.redo.pop()?;
        self.undo.push_back(Snapshot {
            buffer,
            cursor,
            revision: self.revision,
        });
        self.revision = next.revision;
        self.merge_key = None;
        Some(next)
    }

    pub(crate) fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub(crate) fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// 当前内容是否与「上次保存」不同
    pub(crate) fn is_dirty(&self) -> bool {
        self.revision != self.saved_revision
    }

    pub(crate) fn mark_saved(&mut self) {
        self.saved_revision = self.revision;
    }

    /// 换文档时整体清空（撤销历史不能跨文件）
    pub(crate) fn reset(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.merge_key = None;
        self.revision = 0;
        self.saved_revision = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(row: usize, col: usize) -> Cursor {
        Cursor { row, col }
    }

    /// 造一个只有一行文本的缓冲，当作快照内容用。
    fn buf(text: &str) -> Buffer {
        Buffer::from_str(text)
    }

    #[test]
    fn undo_then_redo_round_trips() {
        let mut s = UndoStack::new(10);
        assert!(!s.can_undo());
        assert!(!s.can_redo());

        s.push_step(buf("a"), at(0, 0), None);
        assert!(s.can_undo());

        // 撤销：拿「当前状态 b」换回「快照 a」
        let prev = s.undo(buf("b"), at(0, 1)).expect("应能撤销");
        assert_eq!(prev.buffer.to_string(), "a");
        assert_eq!(prev.cursor, at(0, 0));
        assert!(s.can_redo());

        // 重做：拿「当前状态 a」换回「b」
        let next = s.redo(buf("a"), at(0, 0)).expect("应能重做");
        assert_eq!(next.buffer.to_string(), "b");
        assert!(s.can_undo(), "重做后应能再撤销回去");
        assert!(!s.can_redo(), "重做链已被取空");
    }

    #[test]
    fn push_step_clears_redo() {
        let mut s = UndoStack::new(10);
        s.push_step(buf("a"), at(0, 0), None);
        s.undo(buf("b"), at(0, 1));
        assert!(s.can_redo());

        s.push_step(buf("c"), at(0, 0), None);
        assert!(!s.can_redo(), "产生新分支后重做链应失效");
    }

    #[test]
    fn is_dirty_follows_revision_and_save() {
        let mut s = UndoStack::new(10);
        assert!(!s.is_dirty(), "刚打开时是干净的");

        s.push_step(buf("a"), at(0, 0), None);
        assert!(s.is_dirty());

        s.mark_saved();
        assert!(!s.is_dirty(), "保存后应回到干净状态");

        s.push_step(buf("b"), at(0, 0), None);
        assert!(s.is_dirty());

        s.undo(buf("c"), at(0, 0));
        assert!(!s.is_dirty(), "撤销回到已保存的修订 → 不该算作已修改");
    }

    #[test]
    fn discard_last_step_rolls_back_revision() {
        let mut s = UndoStack::new(10);
        s.push_step(buf("a"), at(0, 0), None);
        assert!(s.is_dirty());

        s.discard_last_step();
        assert!(!s.can_undo());
        assert!(!s.is_dirty(), "回滚一个空步后不该算作已修改");
    }

    #[test]
    fn limit_drops_oldest_steps() {
        let mut s = UndoStack::new(2);
        for text in ["a", "b", "c"] {
            s.push_step(buf(text), at(0, 0), None);
        }
        assert_eq!(s.undo.len(), 2, "超出上限应丢弃最旧的一步");
    }

    #[test]
    fn needs_step_breaks_on_kind_or_row_change() {
        let mut s = UndoStack::new(10);
        assert!(s.needs_step(EditKind::Insert, 0), "还没有任何合并组");

        s.push_step(buf("a"), at(0, 0), Some((EditKind::Insert, 0)));
        assert!(!s.needs_step(EditKind::Insert, 0), "同类同行 → 并入上一步");
        assert!(s.needs_step(EditKind::Insert, 1), "换行 → 断开合并");
        assert!(s.needs_step(EditKind::Delete, 0), "换类型 → 断开合并");

        s.break_merge();
        assert!(s.needs_step(EditKind::Insert, 0), "显式断开后要重新开步");
    }
}
