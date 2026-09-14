//! `pool.rs` 的集成测试 —— **真进程**。
//!
//! 这里测的全是「留几个、淘汰谁」：那些东西只有真的起过进程才说得清，
//! 而它们的错法都很安静 —— 淘汰错了最多是多等几秒，没有任何东西会报错。
//!
//! ## 这套测试里最有力的一招
//!
//! `Pool::acquire` 的第二个参数是「怎么起一个新会话」。于是**该复用的时候**，
//! 传一个直接 `panic!` 的闭包进去 —— 只要池子敢重新起一个，测试当场炸。
//! 比断言「进程数没变」这种间接信号硬得多。

use std::io;
use std::time::{Duration, Instant};

use stbd::lsp::pool::{Failure, Pool};
use stbd::lsp::session::{Outcome, Session};

mod common;

/// 假服务器的路径（cargo 会给集成测试设好这个环境变量）
const FAKE: &str = env!("CARGO_BIN_EXE_fake-lsp");

// 三个「项目根」。用 uri 原样当钥匙 —— 池子不解析它。
const ROOT_A: &str = "file:///D:/project-a";
const ROOT_B: &str = "file:///D:/project-b";
const ROOT_C: &str = "file:///D:/project-c";

const URI_A: &str = "file:///D:/project-a/src/main.rs";
const URI_B: &str = "file:///D:/project-b/src/main.rs";

/// 大多数测试里所有条目都属于**同一个命令** —— 那些测试关心的是「哪个根活着」，
/// 不是「哪个命令」。真正验「两个命令同一个根」的只有一条，它自己显式传命令。
const COMMAND: &str = "fake-lsp";

/// 起一个真的假服务器。
fn start() -> io::Result<Session> {
    Session::start(FAKE, &[], None, None)
}

/// 拿某个根的会话（省得每个调用点都写一遍命令）。
///
/// 池子的钥匙是 `命令@根`（见 `pool::key_of`），但测试关心的是根那一半，
/// 所以命令在这里固定住。
fn acquire<'a>(
    pool: &'a mut Pool,
    root: &str,
    start: impl FnOnce() -> io::Result<Session>,
) -> Result<&'a mut Session, Failure> {
    pool.acquire(COMMAND, root, start)
}

/// 池子里「根」那一列，按「最久没用过的」到「刚用过的」。
///
/// 钥匙是 `命令@根`，这里只取根那一半 —— 「哪个根活着、谁被淘汰了」才是
/// 这些测试要问的。
fn order(pool: &Pool) -> Vec<String> {
    pool.keys()
        .into_iter()
        .map(|key| {
            key.split_once('@')
                .map_or(key.clone(), |(_, root)| root.to_string())
        })
        .collect()
}

/// 等所有活着的会话都握完手。
///
/// ⚠️ 必须在**全部 `acquire` 之后**调一次，不能每 acquire 一个就调一次：
/// `Outcome::Ready` 是**一次性**的（见它的注释），先来的那个一旦被消费掉，
/// 第二次就再也等不到了。
fn settle(pool: &mut Pool) {
    let want = pool.len();
    let mut ready: Vec<String> = Vec::new();
    let deadline = Instant::now() + common::FAKE_SERVER_WAIT;
    while Instant::now() < deadline {
        for spoken in pool.poll_all() {
            if spoken.outcome == Outcome::Ready && !ready.contains(&spoken.root_uri) {
                ready.push(spoken.root_uri);
            }
        }
        if ready.len() >= want {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "握手没走完：{ready:?} 中只到了 {} 个，要 {want} 个",
        ready.len()
    );
}

/// 把「本该失败」的结果拆出来（`unwrap_err` 用不了：`&mut Session` 不是 `Debug`）。
fn expect_failure(result: Result<&mut Session, Failure>) -> Failure {
    match result {
        Ok(_) => panic!("这一步本该失败"),
        Err(failure) => failure,
    }
}

/// 借一下某个根那个会话的账本（拿不到就给个空的）。
///
/// 这里传一个「返回错误」的闭包而不是 `panic!`：正常情况下会话已经在了，
/// 永远走不到它；万一不在，我们只想拿到个空账本，不想把测试炸掉。
fn session_log(pool: &mut Pool, root: &str) -> Vec<String> {
    // ⚠️ 传 `pool` 而不是 `&mut pool`：这里 `pool` 本身**已经**是 `&mut Pool`，
    //    再取一次引用就成了 `&mut &mut Pool`，编译不过。
    match acquire(pool, root, || Err(io::Error::other("只是想看看账本"))) {
        Ok(session) => session.log_tail(),
        Err(_) => Vec::new(),
    }
}

/// 等某个根的账本里出现某句话，然后返回整本账。
///
/// ⚠️ **必须等，不能读完就走**：服务器的 stderr 是**另一个线程**在读的，
/// 我们 `show` 完的那一刻，那句话很可能还在路上。第一版就是因为这个假失败了两条。
fn wait_for_log(pool: &mut Pool, root: &str, needle: &str) -> Vec<String> {
    let deadline = Instant::now() + common::FAKE_SERVER_WAIT;
    loop {
        pool.poll_all();
        let log = session_log(pool, root);
        if log.iter().any(|line| line.contains(needle)) || Instant::now() >= deadline {
            return log;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ---------- 复用 ----------

/// ⚠️ **同一个根、两个命令 → 两个会话。**
///
/// 这是把钥匙从「根」改成「**命令 + 根**」的全部理由。
///
/// 只用「根」当钥匙的话，你打开 `.c` 时池子会说「这个根已经有会话了」，
/// 然后把 **rust-analyzer** 交出去 —— 而它会拿 Rust 的语法去解析你的 C 代码，
/// 报出一堆**假的**错误（实测过：拿 Rust 解析 Python 会报 4 条
/// `expected an item`）。假错误比没有诊断糟得多：你会去改本来没错的代码。
#[test]
fn one_root_two_commands_gets_two_separate_servers() {
    let mut pool = Pool::new(2);

    pool.acquire("rust-analyzer", ROOT_A, start).unwrap();
    pool.acquire("clangd", ROOT_A, start).unwrap();

    assert_eq!(pool.len(), 2, "同一个根下两个服务器该各有一个会话");
    assert_eq!(
        pool.keys(),
        vec![
            "rust-analyzer@file:///D:/project-a",
            "clangd@file:///D:/project-a",
        ],
        "钥匙该是「命令@根」"
    );

    // 再各要一次：还是原来那两个，都没重起（重起了这个闭包就炸）
    pool.acquire("rust-analyzer", ROOT_A, || panic!("rust-analyzer 还活着"))
        .unwrap();
    pool.acquire("clangd", ROOT_A, || panic!("clangd 还活着"))
        .unwrap();
    assert_eq!(pool.len(), 2);
}

/// ⚠️ **池子说出来的每句话都必须带对「是哪个服务器说的」。**
///
/// 这条守着一个真发生过的**假消息**：状态栏那句「握手成功」以前写死成
/// `"LSP: rust-analyzer is ready"`，配了 clangd 之后就变成「起了 clangd，
/// 屏幕上却说 rust-analyzer」。
///
/// 为什么这不只是「字写错了」：那句话是用户**唯一**能看出「服务器到底起没起、
/// 起的是哪个」的地方（`-- READ-ONLY --` 那行和它旁边的提示都不说这事）。
/// 说错名字比不说还糟 —— 你会拿它去确认一个根本没发生的事。
/// 写这条测试之前，手动冒烟测试就是这么被骗过去的（见 `drain_lsp` 的注释）。
#[test]
fn every_message_names_the_server_it_came_from() {
    let mut pool = Pool::new(2);

    // ⚠️ 命令**只是身份标签**，和 `start` 真的起什么无关 —— 所以这里可以
    //    拿同一个假服务器冒充两个不同的服务器。
    acquire(&mut pool, ROOT_A, start).unwrap();
    pool.acquire("clangd", ROOT_B, start).unwrap();

    let mut named: Vec<String> = Vec::new();
    let deadline = Instant::now() + common::FAKE_SERVER_WAIT;
    while named.len() < 2 && Instant::now() < deadline {
        for spoken in pool.poll_all() {
            if spoken.outcome == Outcome::Ready {
                named.push(spoken.command);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    named.sort();
    assert_eq!(
        named,
        vec!["clangd".to_string(), COMMAND.to_string()],
        "每个会话报的名字必须是**它自己的**命令，不是写死的某一个"
    );

    // 名字和钥匙那一半也得对得上 —— 两处各自拼一个名字的话，
    // 「谁在说话」和「谁在池子里」会悄悄错开，而两边都看不出毛病
    let mut from_keys: Vec<String> = pool
        .keys()
        .into_iter()
        .filter_map(|key| key.split_once('@').map(|(command, _)| command.to_string()))
        .collect();
    from_keys.sort();
    assert_eq!(named, from_keys, "报出来的名字和钥匙里的命令对不上");
}

/// 反过来：**同一个命令 + 同一个根**还是只有一个会话。
///
/// 我们内置的 `[lsp.c]` 和 `[lsp.cpp]` 都是 `clangd` —— 靠这条，
/// 它们自然会共用一个进程，不用特意去合。
#[test]
fn one_command_two_sections_shares_one_server() {
    let mut pool = Pool::new(2);

    pool.acquire("clangd", ROOT_A, start).unwrap();
    // 第二次不给 start 的机会：共用才不重起
    pool.acquire("clangd", ROOT_A, || panic!("clangd 该是同一个进程"))
        .unwrap();

    assert_eq!(pool.len(), 1);
}

/// ⚠️ **`running()` 和 `keys()` 必须是同一份数据的两种说法。**
///
/// `:lsp` 那一屏靠 `running()` 说「现在跑着哪几个」，而「淘汰谁」的正确性
/// 靠 `keys()` 的顺序。两个要是各自遍历一遍、哪天改了排序规则就会出现
/// 「表里说在跑、池子里没有」这种对不上 —— 而那一屏正是用户拿来看
/// 「到底有没有在工作」的地方，它说的假话没人能发现。
#[test]
fn the_running_list_and_the_keys_agree() {
    let mut pool = Pool::new(3);

    pool.acquire("rust-analyzer", ROOT_A, start).unwrap();
    pool.acquire("clangd", ROOT_A, start).unwrap();
    pool.acquire("clangd", ROOT_B, start).unwrap();

    // 顺序：最久没用过的在前 —— 和 `keys()` 同一条规矩
    assert_eq!(
        pool.running(),
        vec![
            ("rust-analyzer".to_string(), ROOT_A.to_string()),
            ("clangd".to_string(), ROOT_A.to_string()),
            ("clangd".to_string(), ROOT_B.to_string()),
        ]
    );

    assert_eq!(
        pool.running().len(),
        pool.keys().len(),
        "两种说法数出来的会话数不一样"
    );
    assert_eq!(pool.running().len(), pool.len());
}

/// 同一个根要第二次，拿到的必须是**原来那个会话**（没有重起进程）。
///
/// ⚠️ 复用这件事错了不会报错，只会**慢** —— 每次按键都重起一个服务器，
/// 屏幕上表现成「诊断老是闪一下再出来」，很容易被当成「它本来就慢」。
#[test]
fn asking_for_the_same_root_twice_reuses_the_very_same_session() {
    let mut pool = Pool::new(2);
    acquire(&mut pool, ROOT_A, start).unwrap();
    settle(&mut pool);

    // 第二次：`start` 换成一个直接 panic 的闭包 —— 它敢重起，这条测试就炸
    acquire(&mut pool, ROOT_A, || panic!("同一个根不该重新起服务器"))
        .unwrap()
        .show(URI_A, "rust", "fn main() {}\n")
        .unwrap();

    let log = wait_for_log(&mut pool, ROOT_A, "didOpen");
    assert!(
        log.iter().any(|line| line.contains("didOpen")),
        "拿到的不是原来那个会话（它的账本里没有我们刚发的那条 didOpen）：{log:#?}"
    );
    assert_eq!(pool.len(), 1, "同一个根不该多出一个会话");
}

/// 两个根 → 两个独立的服务器，各管各的文件。
#[test]
fn two_roots_get_two_independent_servers() {
    let mut pool = Pool::new(2);
    acquire(&mut pool, ROOT_A, start).unwrap();
    acquire(&mut pool, ROOT_B, start).unwrap();
    settle(&mut pool);

    acquire(&mut pool, ROOT_A, || panic!("A 还活着"))
        .unwrap()
        .show(URI_A, "rust", "fn a() {}\n")
        .unwrap();
    acquire(&mut pool, ROOT_B, || panic!("B 还活着"))
        .unwrap()
        .show(URI_B, "rust", "fn b() {}\n")
        .unwrap();

    assert_eq!(pool.len(), 2);

    let log_a = wait_for_log(&mut pool, ROOT_A, "didOpen");
    assert!(log_a.iter().any(|line| line.contains("didOpen")));
    assert!(
        !log_a.iter().any(|line| line.contains("project-b")),
        "A 那个服务器收到了 B 的文件 —— 两个会话串了：{log_a:#?}"
    );
}

// ---------- 淘汰 ----------

/// 超上限时，走的是**最久没用过**的那个。
#[test]
fn going_over_the_limit_evicts_the_least_recently_used() {
    let mut pool = Pool::new(2);
    acquire(&mut pool, ROOT_A, start).unwrap();
    acquire(&mut pool, ROOT_B, start).unwrap();
    assert_eq!(order(&pool), vec![ROOT_A, ROOT_B]);

    // 第三个进来。A 在最前面 = 最久没用过 → 该它走
    acquire(&mut pool, ROOT_C, start).unwrap();

    assert_eq!(pool.len(), 2);
    assert_eq!(
        order(&pool),
        vec![ROOT_B, ROOT_C],
        "淘汰的不是最久没用过的那个"
    );
}

/// ⚠️ **正在用的那个永远不该被踢掉。**
///
/// 这是整个池子最要紧的一条性质，而且它是「淘汰从头下手」自然得到的 ——
/// 没有为「当前文件」写任何特判。这条测试守的就是那个「自然」。
///
/// 它错了的后果很具体：你正看着的文件突然没有诊断了，而且**要等下一次推送
/// 才可能回来** —— 而那个服务器已经被我们杀掉了，所以永远回不来。
#[test]
fn shrinking_the_limit_keeps_the_one_you_are_actually_looking_at() {
    let mut pool = Pool::new(3);
    for root in [ROOT_A, ROOT_B, ROOT_C] {
        acquire(&mut pool, root, start).unwrap();
    }

    // 又回去看了一眼 A —— 它现在是「刚用过」的那个（排在最后）
    acquire(&mut pool, ROOT_A, || panic!("A 还活着")).unwrap();
    assert_eq!(order(&pool), vec![ROOT_B, ROOT_C, ROOT_A]);

    // 上限调到 1。留下的必须是 A —— 不是「最先建的那个」，
    // 也不是「随便留一个」
    pool.set_limit(1);
    assert_eq!(
        order(&pool),
        vec![ROOT_A],
        "调小上限时把**正在看的那个**也收掉了"
    );
}

/// 上限调到 0 = 全收掉（用户敲了 `:set lspmaxservers 0`）。
#[test]
fn a_limit_of_zero_clears_everything() {
    let mut pool = Pool::new(2);
    acquire(&mut pool, ROOT_A, start).unwrap();
    acquire(&mut pool, ROOT_B, start).unwrap();

    pool.set_limit(0);

    assert!(
        pool.is_empty(),
        "调成 0 之后还留着服务器 —— 那 `:set` 就是句空话"
    );
}

// ---------- 起不来 ----------

/// 上限 0：**根本不该去起服务器**。
///
/// ⚠️ 这条守着一个死循环：`trim` 要是写成 `while len >= limit`，
/// 上限 0 时 `len >= 0` 永远成立，循环会一直 `remove(0)` 到把列表掏空、
/// 然后 panic。所以上限 0 必须在最前面就拦掉。
#[test]
fn a_limit_of_zero_starts_nothing_at_all() {
    let mut pool = Pool::new(0);

    let failure = expect_failure(acquire(&mut pool, ROOT_A, || {
        panic!("0 的时候不该去起服务器")
    }));

    assert!(pool.is_empty());
    assert!(!failure.already_reported, "第一次得说一句");
}

/// ⚠️ 同一个根起不来，**只报一次**。
///
/// 这条不是省事：`acquire` 每按一个键就可能调一次，同一句话反复刷的话，
/// `:w` 那句「Saved foo.rs」一闪就被冲掉 —— 用户根本看不见自己保存成功了。
#[test]
fn a_failure_is_reported_once_per_root_not_once_per_keystroke() {
    let mut pool = Pool::new(2);

    let first = expect_failure(acquire(&mut pool, ROOT_A, || {
        Err(io::Error::other("no server"))
    }));
    assert!(!first.already_reported);
    assert!(first.reason.contains("no server"));

    let second = expect_failure(acquire(&mut pool, ROOT_A, || {
        Err(io::Error::other("no server"))
    }));
    assert!(
        second.already_reported,
        "同一个根又失败一次，不该再报一遍 —— 那会把别的消息冲掉"
    );

    // 换一个根 = 那是**新**问题，该说
    let other = expect_failure(acquire(&mut pool, ROOT_B, || {
        Err(io::Error::other("no server"))
    }));
    assert!(!other.already_reported, "换了个根，这是个新消息");
}

// ---------- 断线 ----------

/// 服务器死了 → 报 `Broken`，而且**从池子里摘掉**。
///
/// 摘掉的理由：那条线已经没了，`poll` 之后再也不会吐出任何东西。
/// 留着它只会每轮白白问一次，而且会一直占着上限的一个名额 ——
/// 于是「上限 2」实际上变成「1 个活的 + 1 个死的」。
#[test]
fn a_server_that_died_is_taken_out_of_the_pool() {
    let mut pool = Pool::new(2);
    acquire(&mut pool, ROOT_A, || {
        Session::start(FAKE, &["--die-after-initialize"], None, None)
    })
    .unwrap();

    let mut broken = None;
    let deadline = Instant::now() + common::FAKE_SERVER_WAIT;
    while broken.is_none() && Instant::now() < deadline {
        for spoken in pool.poll_all() {
            if let Outcome::Broken(why) = spoken.outcome {
                broken = Some(why);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(broken.is_some(), "服务器已经死了，池子却没发现");
    assert!(
        pool.is_empty(),
        "断线的会话还占着池子里的一个名额 —— 上限会被它白吃一个"
    );
}

// ---------- 真服务器 ----------

/// **两个真项目，各起一个服务器，各拿各的答案。**
///
/// 标 `#[ignore]`：依赖本机装了 rust-analyzer，还依赖旁边有另一个 Cargo 项目。
/// 手动跑：
///
/// ```text
/// cargo test --test lsp_pool -- --ignored --nocapture two_real
/// ```
///
/// ## 它值这一趟
///
/// 上面所有测试用的都是假服务器 —— 假服务器能验「池子的逻辑对不对」，
/// 但验不了**这一整步想解决的问题**：一个根的服务器真的看不见另一个项目的
/// 文件（这是实测过的，见 `tests/lsp_session.rs` 里那两条探针），
/// 而多起一个就真的看得见。
///
/// 也就是说：**只有这条测试能证明「一个根一个会话」真的是那个解。**
///
/// 它**不碰磁盘**：正文是 `didOpen` 里带过去的，服务器以内存为准。
#[test]
#[ignore = "需要 rust-analyzer + 旁边另一个 Cargo 项目，手动跑"]
fn two_real_projects_both_get_their_own_answers() {
    let here = std::env::current_dir().expect("拿不到当前目录");
    let other = here.parent().expect("没有上一级").join("minesweeper");

    let target_here = here.join("src").join("check.rs");
    let target_other = other.join("src").join("main.rs");
    assert!(
        target_other.exists(),
        "靶子不在：{}",
        target_other.display()
    );

    let root_here = stbd::lsp::uri::path_to_uri(&here).expect("根该能转成 uri");
    let root_other = stbd::lsp::uri::path_to_uri(&other).expect("根该能转成 uri");
    let uri_here = stbd::lsp::uri::path_to_uri(&target_here).expect("文件该能转成 uri");
    let uri_other = stbd::lsp::uri::path_to_uri(&target_other).expect("文件该能转成 uri");

    // 末尾少一个 `}`：括号不配，一定会报
    let broken = |path: &std::path::Path| {
        let text = std::fs::read_to_string(path).expect("读不到靶子");
        format!("{text}\nfn 坏 {{")
    };

    let spawn = |root: &std::path::Path, root_uri: &str| {
        let root = root.to_path_buf();
        let root_uri = root_uri.to_string();
        move || {
            Session::start(
                "rust-analyzer",
                &[],
                Some(root.as_path()),
                Some(root_uri.as_str()),
            )
        }
    };

    let mut pool = Pool::new(2);
    acquire(&mut pool, &root_here, spawn(&here, &root_here))
        .expect("起不来 rust-analyzer（本机装了吗？）");
    acquire(&mut pool, &root_other, spawn(&other, &root_other))
        .expect("起不来第二个 rust-analyzer");
    assert_eq!(pool.len(), 2, "两个根该有两个会话");

    // 两个项目都要加载 crate 图，给足时间
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut ready = 0;
    while ready < 2 && Instant::now() < deadline {
        ready += pool
            .poll_all()
            .iter()
            .filter(|spoken| spoken.outcome == Outcome::Ready)
            .count();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(ready, 2, "有两个没握完手");

    acquire(&mut pool, &root_here, || panic!("还活着"))
        .unwrap()
        .show(&uri_here, "rust", &broken(&target_here))
        .unwrap();
    acquire(&mut pool, &root_other, || panic!("还活着"))
        .unwrap()
        .show(&uri_other, "rust", &broken(&target_other))
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(90);
    let mut got_here = 0;
    let mut got_other = 0;
    while (got_here == 0 || got_other == 0) && Instant::now() < deadline {
        for spoken in pool.poll_all() {
            if let Outcome::Diagnostics(push) = spoken.outcome {
                if push.diagnostics.is_empty() {
                    continue; // 「还没分析完」的空推送
                }
                // ⚠️ 这里同时顺手验了那个盘符大小写的坑：我们发出去的是
                //    `file:///D:/...`，服务器回来的是 `file:///d:/...`
                if stbd::lsp::uri::same_file(&push.uri, &uri_here) {
                    got_here = push.diagnostics.len();
                }
                if stbd::lsp::uri::same_file(&push.uri, &uri_other) {
                    got_other = push.diagnostics.len();
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    eprintln!("第一个项目：{got_here} 条诊断");
    eprintln!("第二个项目：{got_other} 条诊断");
    assert!(got_here > 0, "第一个项目弄坏了它却没说话");
    assert!(
        got_other > 0,
        "第二个项目弄坏了它却没说话 —— 「一个根一个会话」这个解没成立"
    );
}
