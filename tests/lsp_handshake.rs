//! 用**真的子进程 + 真的管道**跑一遍 LSP 握手。
//!
//! `framing.rs` 的单元测试证明的是「写出来的字节符合规范」（金标准）；
//! 这个文件证明的是另一件事：**在真管道上也能用**。
//!
//! 两者不一样：`Cursor` 永远一次给全，而管道会切碎、会阻塞、会突然消失。
//! 更重要的是，只有这里能测到一件单测根本碰不到的事 ——
//! **进程会不会干净地退出**。

use std::io::{BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use stbd::lsp::framing;

/// 起一个假服务器，把两条管道接好。
fn start_fake_server() -> (Child, ChildStdin, BufReader<ChildStdout>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fake-lsp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // stderr 接过来：假服务器只在出错时写它，让报错能在测试输出里看见。
        // ⚠️ 别用 `Stdio::null()`，那样协议坏了会一点线索都不剩。
        .stderr(Stdio::inherit())
        .spawn()
        .expect("起不来 fake-lsp");

    let stdin = child.stdin.take().expect("stdin was piped");
    let stdout = BufReader::new(child.stdout.take().expect("stdout was piped"));
    (child, stdin, stdout)
}

/// **有上限地**等进程结束。
///
/// ⚠️ 为什么不用 `child.wait()`：它会无限期阻塞。假服务器哪天卡住了，
/// 整个测试套件就挂在那儿不返回 —— 而且报出来的是最没信息量的那种失败。
/// 有上限地等，才能明确说出「它没退出」。
fn wait_bounded(child: &mut Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("try_wait failed") {
            return Some(status);
        }
        // 测试代码里 sleep 是合适的：我们在等一个**真实进程**的调度，
        // 没有别的办法「稍后再问一次」。
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

/// 读一条回应并解析成 JSON。读不到就直接失败（附上原因）。
fn read_response(reader: &mut BufReader<ChildStdout>) -> serde_json::Value {
    let body = framing::read_message(reader)
        .expect("读回应时出错")
        .expect("服务器提前关掉了管道");
    serde_json::from_str(&body).expect("服务器回的不是 JSON")
}

/// 规范的握手顺序：`initialize` → `initialized` → …… → `shutdown` → `exit`。
#[test]
fn handshake_completes_in_order() {
    let (mut child, mut stdin, mut stdout) = start_fake_server();

    // → initialize（请求）
    framing::write_message(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    )
    .unwrap();

    let response = read_response(&mut stdout);
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1, "id 必须原样带回来");
    assert!(
        response["result"]["capabilities"].is_object(),
        "initialize 要给一份 capabilities：{response}"
    );

    // → initialized（**通知**：没有 id 字段）
    //
    // 注意下面那条断言的巧妙之处：通知不该有回应，所以下一次读到的必须是
    // **shutdown 的回应（id = 2）**。要是服务器给通知也回了一条，id 就对不上。
    framing::write_message(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
    )
    .unwrap();

    // → shutdown（请求）
    framing::write_message(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"shutdown"}"#,
    )
    .unwrap();
    let response = read_response(&mut stdout);
    assert_eq!(response["id"], 2, "通知不该有回应，这条必须是 shutdown 的");
    assert!(response["result"].is_null(), "{response}");

    // → exit（通知）→ 进程该自己退了
    framing::write_message(&mut stdin, r#"{"jsonrpc":"2.0","method":"exit"}"#).unwrap();

    let status =
        wait_bounded(&mut child, Duration::from_secs(5)).expect("收到 exit 之后进程没有退出");
    assert!(status.success(), "正常退出应该是 0：{status}");
}

/// ⚠️ **这条守着一整类最难查的 bug。**
///
/// 一个陌生方法的请求也必须被回应。真实的 rust-analyzer 会主动发
/// `client/registerCapability` 之类的请求过来 —— 少回一条，它就一直在那儿等，
/// 表现出来是「服务器启动了、但什么都不干」，**而且没有任何报错**。
#[test]
fn an_unknown_request_still_gets_an_answer() {
    let (mut child, mut stdin, mut stdout) = start_fake_server();

    framing::write_message(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":7,"method":"something/unknown","params":{}}"#,
    )
    .unwrap();

    let response = read_response(&mut stdout);
    assert_eq!(response["id"], 7, "每个请求都必须有回应：{response}");

    drop(stdin);
    let _ = wait_bounded(&mut child, Duration::from_secs(5));
}

/// 客户端死了（管道关了）→ 服务器也该自己退，不能留在那儿当孤儿。
#[test]
fn closing_the_client_side_makes_the_server_quit() {
    let (mut child, stdin, _stdout) = start_fake_server();

    // 关掉我们的写端 → 服务器读到 EOF
    drop(stdin);

    let status =
        wait_bounded(&mut child, Duration::from_secs(5)).expect("客户端关了管道，服务器却没退");
    assert!(status.success(), "{status}");
}

/// 协议流坏掉之后**不能装作没事继续读** —— 因为已经没有任何办法知道
/// 下一条消息从哪个字节开始。这种时候唯一正确的动作是退出。
#[test]
fn a_corrupt_frame_makes_the_server_give_up() {
    let (mut child, mut stdin, _stdout) = start_fake_server();

    // 头说 7 个字节，正文只给 3 个，然后关掉写端 → 必然读到一半就断
    stdin.write_all(b"Content-Length: 7\r\n\r\n{\"a").unwrap();
    stdin.flush().unwrap();
    drop(stdin);

    let status =
        wait_bounded(&mut child, Duration::from_secs(5)).expect("协议坏了，服务器却没退出");
    assert!(!status.success(), "协议坏了就该非 0 退出：{status}");
}
