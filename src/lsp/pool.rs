//! 会话池 —— 按「项目根」存放会话，超过上限就淘汰最久没用过的 —— pool.rs 的职责
//!
//! ## 它解决的那个问题
//!
//! 服务器是**认根**的：一个实例的整个世界都从 `initialize` 里那个 `rootUri`
//! 长出来（它在那儿跑 `cargo metadata`，得到 crate 图）。拿这个根去问另一个
//! 项目的文件，它一个字都不说 —— 这是实测过的，见 `tests/lsp_session.rs`
//! 里那两条探针。
//!
//! 所以「打开另一个项目的文件」这件事，唯一的路是**为那个项目再起一个服务器**。
//! 而服务器很贵（实测一个加载完的约 1.2 GB），不能来一个开一个 —— 于是需要一个
//! 上限，和一个淘汰规则。
//!
//! ## 这个池子就是「上限 + 淘汰规则」
//!
//! ```text
//! 用到了某个根 → 有它的会话吗？
//!                 有 → 挪到「最近用过」那一端，用它
//!                 没有 → 起一个；起完如果超了上限，就从**另一头**踢掉一个
//! ```
//!
//! 「另一头」是全部的关键：列表按最近使用排序，淘汰永远从最久没用过的下手。
//! 这样**当前正在看的那个永远不会被踢掉** —— 不需要为它写任何特判，
//! 因为它必然是最后被挪到末尾的那一个。
//!
//! ## 上限的三种取值，同一份代码
//!
//! | 值 | 行为 |
//! |----|------|
//! | `0` | 一个都不留（不用语言服务器） |
//! | `1` | 只留当前项目 —— 跳项目=每次都要重新加载 |
//! | `N` | 留 N 个 —— 在 N 个项目之间跳是瞬时的 |
//!
//! ⚠️ 这三种**不是三条代码路径**，是同一个列表配不同的上限。这正是为什么
//! 配置项是一个**数字**而不是一个「多项目模式」的开关：两条独立的实现
//! 意味着两倍的 bug 面，而它们的差别其实只有一个比较。
//!
//! ## 它是懒的
//!
//! 会话不是预先起的 —— 用到哪个根才起哪个。所以你只在一个项目里干活时，
//! 上限写 1 和写 8 完全一样（都只有一个进程）。**上限管的是「最多允许几个」，
//! 不是「预先开几个」。**
//!
//! ## ⚠️ 踢人是有代价的，代价发生在主循环里
//!
//! `Session` 一被丢掉就会 `shutdown` 子进程（见 `client::Client::shutdown`），
//! 那里面有最多 200ms 的等待。所以淘汰、以及 `:set lspmaxservers 1` 那种
//! 大批收工，都会让界面顿一下。不常发生（只在真的来回跳项目时），
//! 但要心里有数：这不是「杀个线程」那么便宜的事。

use std::io;

use super::session::{Outcome, Session};

/// 一个会话起不来 —— 以及**要不要告诉用户**。
#[derive(Debug)]
pub struct Failure {
    /// 说给用户听的一句话。
    pub reason: String,
    /// 这个根上次也是这么失败的 → **别重复报**。
    ///
    /// 为什么要有这个：`acquire` 是每按一个键就可能调一次的。没有这个标记的话，
    /// 「rust-analyzer 没装」会在每次敲键之后把状态栏重新刷成同一句话 ——
    /// 于是 `:w` 那句「Saved foo.rs」一闪就没了，用户根本看不见。
    pub already_reported: bool,
}

/// 一个活着的会话，以及它是为哪个根起的。
struct Entry {
    /// `initialize` 里发出去的那个 `rootUri`。**用原样的字符串当钥匙** ——
    /// 它由 `uri::path_to_uri` 产出，同一个路径必然产出同一个串。
    root_uri: String,
    session: Session,
}

/// 会话池。
pub struct Pool {
    /// 最多留几个。
    limit: usize,
    /// 按「最近用过」排序：**末尾是最新的**，淘汰从头上下手。
    entries: Vec<Entry>,
    /// 上次报过失败的那个根（见 [`Failure::already_reported`]）。
    last_failure: Option<String>,
}

impl Pool {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            entries: Vec::new(),
            last_failure: None,
        }
    }

    /// 改上限。**多出来的当场收掉** —— 不改的话 `:set lspmaxservers 1` 会变成
    /// 一句空话，用户敲完看不到任何变化。
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
        self.trim();
    }

    /// 拿到这个根的会话；没有就开始一个新的。
    ///
    /// `start` 只在**真的需要**新会话时才会被调用 —— 所以「怎么起一个服务器」
    /// 这件事留在调用方（它知道用哪个命令、哪个工作目录），池子只管
    /// 「留几个、淘汰谁」。
    pub fn acquire(
        &mut self,
        root_uri: &str,
        start: impl FnOnce() -> io::Result<Session>,
    ) -> Result<&mut Session, Failure> {
        // ⚠️ 这个守卫**不只是**防呆。它担着两件事，实测（拿掉它跑
        //    `a_limit_of_zero_starts_nothing_at_all`）：
        //
        //    1. 不去起那个服务器 —— 用户明说了不用，你却花 1.2 GB 起一个，
        //       而且起完又马上收掉。
        //    2. `trim` 会把刚 push 进来的那个也收掉（列表变空），
        //       下面那句 `len() - 1` 就在 usize 上下溢 —— 报出来的是
        //       `attempt to subtract with overflow`，与真正的原因（上限 0）
        //       隔着十万八千里。
        if self.limit == 0 {
            return Err(self.failure(root_uri, "language servers are off".to_string()));
        }

        // ⚠️ 先把下标算出来，再 match。写成 `match self.entries.iter().position(..)`
        //    的话，那个临时迭代器的借用会活到**整个 match 结束**（Rust 里
        //    match 的临时值就是这么活着的），于是每个分支里对 `self.entries`
        //    的修改都会被借用检查拦住。
        let existing = self.entries.iter().position(|e| e.root_uri == root_uri);

        let index = match existing {
            Some(index) => {
                // 挪到末尾 = 标记「刚用过」。淘汰永远从另一头下手，
                // 所以这一步就是「别把我踢掉」的全部实现。
                let entry = self.entries.remove(index);
                self.entries.push(entry);
                self.entries.len() - 1
            }
            None => {
                let session = match start() {
                    Ok(session) => session,
                    Err(err) => return Err(self.failure(root_uri, err.to_string())),
                };
                self.entries.push(Entry {
                    root_uri: root_uri.to_string(),
                    session,
                });
                // 先放后收：`trim` 从头丢，所以刚 push 进来的这个**一定**活着
                // （上限 >= 1 已经由上面的守卫保证了）。
                self.trim();
                self.entries.len() - 1
            }
        };

        Ok(&mut self.entries[index].session)
    }

    /// 收所有人的话。
    ///
    /// 每一项都带着**是哪个根说的** —— 因为诊断要按根才能判断
    /// 「它说的是不是现在屏幕上这个文件」。
    ///
    /// 报过 [`Outcome::Broken`] 的会话会在这里被丢掉（`drop` 顺带收尸）：
    /// 那条线已经没了，之后再也不会产出任何东西，留着只会每轮白白问一次。
    pub fn poll_all(&mut self) -> Vec<(String, Outcome)> {
        let mut collected = Vec::new();
        let mut survivors = Vec::with_capacity(self.entries.len());

        // `drain` 把所有权拿出来再重建，省掉「一边遍历一边删」的下标错位问题
        for mut entry in self.entries.drain(..) {
            let mut broken = false;
            for outcome in entry.session.poll() {
                if matches!(outcome, Outcome::Broken(_)) {
                    broken = true;
                }
                collected.push((entry.root_uri.clone(), outcome));
            }
            if !broken {
                survivors.push(entry);
            }
        }

        self.entries = survivors;
        collected
    }

    /// 现在留了几个会话。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 活着的会话，按「最久没用过的」到「刚用过的」排列。
    ///
    /// 给测试用的 —— 「淘汰谁」这件事的正确性全靠这个顺序，而它是看不见的。
    pub fn roots(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.root_uri.as_str()).collect()
    }

    /// 把那几个多出来的收掉。
    ///
    /// ⚠️ 只有这一个地方丢会话 —— 「上限满了」和「上限调小了」走的都是它。
    /// 两件事共用一条实现，就不会出现「调小之后还留着一个多余的」这种漏网。
    fn trim(&mut self) {
        while self.entries.len() > self.limit {
            // 从头丢 = 丢最久没用过的。⚠️ `remove(0)` 会把 `Session` 移出来，
            //    它在这个语句结束时就 drop 了，`shutdown` 也随之发生。
            self.entries.remove(0);
        }
    }

    /// 拼一个失败出去，并记下「这个根刚报过」。
    fn failure(&mut self, root_uri: &str, reason: String) -> Failure {
        let already_reported = self.last_failure.as_deref() == Some(root_uri);
        self.last_failure = Some(root_uri.to_string());
        Failure {
            reason,
            already_reported,
        }
    }
}
