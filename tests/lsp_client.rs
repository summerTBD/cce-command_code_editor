//! `client.rs` 的集成测试 —— **真进程、真管道**。
//!
//! 这里测的全是单元测试碰不到的东西：握手走到哪一步、**服务器反过来问我们时
//! 我们有没有回**、它中途死掉我们会怎样、收工时进程有没有真的被收干净。

use std::time::{Duration, Instant};

use serde_json::json;
use stbd::lsp::message::Message;

use stbd::lsp::client::{Client, Event, State};

mod common;

/// 假服务器的路径（cargo 会给集成测试设好这个环境变量）
const FAKE: &str = env!("CARGO_BIN_EXE_fake-lsp");

/// 有上限地推进客户端，直到状态满足条件。
///
/// ⚠️ 时间上限不是「怕慢」，是为了**别让测试挂死**：真挂了的话报出来的是
/// 「卡住不动」，而我们要的是「条件没满足」这种查得下去的失败。
fn drive_until(client: &mut Client, limit: Duration, mut done: impl FnMut(State) -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done(client.state()) {
            return true;
        }
        if let Some(Event::Broken(why)) = client.next_event() {
            panic!("连接断了：{why}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// 一边排空事件、一边等日志里出现某句话。
fn wait_for_log(client: &mut Client, needle: &str, limit: Duration) -> Vec<String> {
    let deadline = Instant::now() + limit;
    loop {
        let log = client.log_tail();
        if log.iter().any(|line| line.contains(needle)) || Instant::now() >= deadline {
            return log;
        }
        // 必须继续取事件：不然通道里的东西一直没人看
        client.next_event();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 握手能走完，**而且服务器反问我们的时候我们答了**。
///
/// ⚠️ 后半句才是重点。真实服务器一定会主动问（`workspace/configuration`
/// 之类），漏答不会报错、不会退出 —— 只会「什么都不发生」。
#[test]
fn handshake_reaches_ready_and_we_answer_the_servers_question() {
    let mut client = Client::spawn(FAKE, &["--ask"], None).expect("起不来 fake-lsp");
    assert_eq!(client.state(), State::Initializing);

    client.initialize(Some("file:///D:/fake/project")).unwrap();

    assert!(
        drive_until(&mut client, common::FAKE_SERVER_WAIT, |state| {
            state == State::Ready
        }),
        "握手没走到 Ready；日志：{:#?}",
        client.log_tail()
    );

    // 它问了两小节配置，我们回的数组就必须是**两条** ——
    // 假的服务器会把条数打进日志，所以这条断言连「形状」一起钉住了
    let log = wait_for_log(&mut client, "ANSWERED", common::FAKE_SERVER_WAIT);
    assert!(
        log.iter().any(|line| line.contains("ANSWERED items=2")),
        "没有按正确的条数回答服务器的问题；日志：{log:#?}"
    );
}

/// 服务器握手中途死掉 → **我们会知道**，而不是一直等下去。
///
/// 这是「不阻塞」在 LSP 这一侧的对应物：等一个不会来的回应，
/// 表现是编辑器永远停在「正在启动语言服务器」。
#[test]
fn a_server_that_dies_mid_handshake_is_reported_instead_of_hanging() {
    let mut client = Client::spawn(FAKE, &["--die-after-initialize"], None).expect("起不来");

    client.initialize(None).unwrap();

    let mut broken = None;
    let deadline = Instant::now() + common::FAKE_SERVER_WAIT;
    while Instant::now() < deadline {
        match client.next_event() {
            Some(Event::Broken(why)) => {
                broken = Some(why);
                break;
            }
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }

    let why = broken.expect("服务器已经死了，客户端却没发现 —— 它会一直等下去");
    assert_eq!(client.state(), State::Dead, "线断了状态就该是 Dead");
    assert!(why.contains("管道"), "这句话该说清楚是管道没了：{why}");
}

/// **拿真的 rust-analyzer 跑一遍握手。**
///
/// 标 `#[ignore]` 是因为它**依赖本机装了什么** —— 别人机器上没有
/// rust-analyzer，那不应该是测试失败。
///
/// 手动跑：
///
/// ```text
/// cargo test --test lsp_client -- --ignored
/// ```
///
/// 它值这一趟：上面所有测试用的都是**我们自己写的假服务器**。
/// 万一我们把协议理解错了，假服务器会跟着一起错 —— 两边自洽，测试全绿，
/// 而真服务器根本不认。**只有它点头，才说明我们照着的那份规范没读错。**
#[test]
#[ignore = "需要本机装了 rust-analyzer，手动跑"]
fn handshake_with_the_real_rust_analyzer() {
    let root = std::env::current_dir().expect("拿不到当前目录");
    // 测试里这个路径没有空格和中文字符，所以够用了。
    // （正经的 `file://` 转换 —— 大小写、编码、非 ASCII —— 是第 3 步的事。）
    let root_uri = format!("file:///{}", root.to_string_lossy().replace('\\', "/"));

    let mut client = Client::spawn("rust-analyzer", &[], Some(root.as_path()))
        .expect("起不来 rust-analyzer（本机装了吗？）");
    client.initialize(Some(root_uri.as_str())).unwrap();

    assert!(
        drive_until(&mut client, Duration::from_secs(60), |state| {
            state == State::Ready
        }),
        "和真的 rust-analyzer 握手没走完；日志：{:#?}",
        client.log_tail()
    );

    // ---- ① 它会不会**反过来问我们**？ ----
    //
    // 这是整条线上最容易漏、又最难查的一件事，所以要拿真服务器验一次。
    // （它加载项目要几秒，所以这里得等。）
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut asked = Vec::new();
    while Instant::now() < deadline && asked.is_empty() {
        match client.next_event() {
            Some(Event::Message(Message::Request { method, .. })) => asked.push(method),
            Some(Event::Broken(why)) => panic!("线断了：{why}"),
            // 别的消息：让它过去，我们要等的是「它问我们」
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    assert!(
        !asked.is_empty(),
        "rust-analyzer 一直没问我们 —— 要么它变了，要么我们哪里配得不对"
    );
    eprintln!("它问了：{asked:?}");

    // ---- ② 它推不推诊断？ ----
    //
    // 它刚才发了 `workspace/diagnostic/refresh` —— 那是「拉」那一套的信号。
    // 要是它其实用「拉」，第 3 步的设计就得改（得我们主动去问，而不是等它推）。
    // 所以这里主动开一个**故意弄坏**的文档，看到底会不会收到推送。
    //
    // ## ✅ 已经查清了（2026-09-14）
    //
    // 根因不在这一层，而在 `initialize` 的能力声明里：加上了
    // `window.workDoneProgress` 之后，3 秒内就有诊断（之前等 30 秒都是空数组）。
    // 拆过变量的结论见 `client::initialize_params` 的注释。
    //
    // 顺带两个发现：
    // - `publishDiagnostics` 推过来的 uri 会被**规范化成小写盘符**
    //   （我们发 `D:`，它回 `d:`）—— 第 3 步比对时绝不能直接字符串相等
    // - 它 stderr 里那句 `WARN notify error: Input watch path ...`
    //   **跟这件事无关**，是个红鲑鱼（声明之后它还在）
    //
    // 所以下面现在**真的断言**「被弄坏的那个文件得有诊断」。
    let path = root.join("src").join("check.rs");
    let uri = format!("file:///{}", path.to_string_lossy().replace('\\', "/"));
    let mut text = std::fs::read_to_string(&path).expect("读不到 src/check.rs");
    // 删掉最后一个 `}` —— 括号不配，语法错，一定会报
    let cut = text.rfind('}').expect("找不到 `}`");
    text.remove(cut);

    client
        .notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "rust",
                    "version": 1,
                    "text": text,
                }
            }),
        )
        .expect("didOpen 发不出去");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = Vec::new();
    let mut got_push = false;
    while Instant::now() < deadline {
        match client.next_event() {
            Some(Event::Message(msg)) => {
                let name = match &msg {
                    Message::Notification { method, params } => {
                        if method == "textDocument/publishDiagnostics" {
                            // ⚠️ 第一条推送往往是**空的**（它还没分析完）。
                            //    所以「收到过推送」和「收到过结果」是两回事。
                            let n = params
                                .get("diagnostics")
                                .and_then(serde_json::Value::as_array)
                                .map_or(0, Vec::len);
                            let uri = params["uri"].as_str().unwrap_or("").to_string();
                            eprintln!("push：{n} 条 ← {uri}");

                            // ⚠️ 还必须是**我们自己那个文件**的推送。
                            //    第一版没加这个条件，结果被另一个文件里的一个告警
                            //    提前满足了 —— 测试绿了，结论是错的。
                            if n > 0 && uri.ends_with("check.rs") {
                                got_push = true;
                            }
                        }
                        format!("通知 {method}")
                    }
                    Message::Request { method, .. } => format!("请求 {method}"),
                    Message::Response { .. } => "回应".to_string(),
                };
                seen.push(name);
                if got_push {
                    break;
                }
            }
            Some(Event::Broken(why)) => {
                eprintln!("线断了：{why}");
                break;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    eprintln!("didOpen 之后收到：{seen:#?}");
    assert!(
        got_push,
        "把 src/check.rs 的收尾那个右大括号删掉之后，没等到它的诊断；\n收到过的消息：{seen:#?}\n它自己的日志：{:#?}",
        client.log_tail()
    );
    eprintln!("它推诊断吗：{got_push}");
    eprintln!("它自己的日志：{:#?}", client.log_tail());
}

/// 收工要**真的把进程收干净**，不能留孤儿。
///
/// 留一个语言服务器当孤儿，它会一直占着内存和文件锁 ——
/// 而且你下次开编辑器时，它会变成第二个。
#[test]
fn shutdown_actually_takes_the_process_down() {
    let mut client = Client::spawn(FAKE, &[], None).expect("起不来");
    client.initialize(None).unwrap();
    assert!(
        drive_until(&mut client, common::FAKE_SERVER_WAIT, |state| {
            state == State::Ready
        }),
        "握手没走到 Ready；日志：{:#?}",
        client.log_tail()
    );

    client.shutdown();

    assert_eq!(client.state(), State::Dead);
    let status = client.try_wait().expect("try_wait 失败");
    assert!(status.is_some(), "shutdown 之后进程还在 —— 它会变成孤儿");
}
