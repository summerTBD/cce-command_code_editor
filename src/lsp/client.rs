//! 语言服务器客户端：进程、管道、线程、记账 —— client.rs 的职责
//!
//! 这是整个 LSP 里唯一「有活物」的地方：一个真进程、两根管道、两个线程。
//! 所以它**故意放在 `lsp/` 里最靠外的一层** —— 上面那两层
//! （[`framing`](super::framing) / [`message`](super::message)）都是纯的，
//! 只有这里不纯。
//!
//! ## 两个线程各自在干什么
//!
//! ```text
//! 主线程 ──write──▶ 服务器的 stdin
//!    ▲
//!    └── 通道 ◀── 读线程 ◀── 服务器的 stdout   （消息）
//!             ◀── 读线程 ◀── 服务器的 stderr   （日志，进环形缓冲）
//! ```
//!
//! ⚠️ **stderr 也必须有人读。** 不读的话它会堵：rust-analyzer 日志写满了
//! 管道缓冲区就会卡在写上，然后**整个服务器都不动了** —— 表现出来是
//! 「它启动了但什么都不干」，而且一点线索都没有。
//!
//! ## 为什么主线程写不会（太）阻塞
//!
//! 严格说「对面不读我们的 stdin」时 `write` 是会阻塞的，而我们是在主线程上写。
//! v1 接受这个风险：rust-analyzer 读得很勤，而且我们发的东西很少。
//! 真碰上了再加一个写线程 —— 那时候会知道是为什么。
//!
//! ## 这个文件里最容易写错的一条
//!
//! **服务器发来的每一条请求都必须回答。** 见 [`Client::answer`]。
//! 少回一条，它会一直等 —— 不报错、不退出、什么都不干。

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, BufWriter};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::framing;
use super::message::{self, Message, RequestId};

/// 服务器那边发生的事。
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// 它说了一句话（该做的账我们已经做完了，见 [`Client::absorb`]）。
    Message(Message),
    /// 这条线没了：分帧错、JSON 错，或者进程退出了。
    ///
    /// **之后不会再有事件** —— 调用方该收摊了。
    Broken(String),
}

/// 握手走到哪一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// 起来了，`initialize` 已经发出去但还没回应。
    ///
    /// ⚠️ 这段时间里**除了 `initialize` 和 `exit` 什么都不能发** —— 规范明文。
    /// 违反的话服务器**静默忽略**，不报错，于是你会看到「发了消息但它没反应」。
    Initializing,
    /// 握手完成，可以干活了。
    Ready,
    /// 已经发过 `shutdown`。
    ShuttingDown,
    /// 收工了，或者线断了。
    Dead,
}

/// 日志缓冲留这么多行。它只是排查线索，不是账本。
const LOG_LINES: usize = 200;

/// 收工时最多等它自己退多久。
///
/// **必须有上限**：`Drop` 里等待就是卡界面，而这个等待发生在每一次退出编辑器时。
/// 正常情况 `exit` 是毫秒级的，这个上限只在服务器已经卡死时才用得上。
const GRACEFUL_EXIT: Duration = Duration::from_millis(200);

/// 一个语言服务器。
pub struct Client {
    /// 我们自己发出去的 `initialize` 的 id —— 回应靠它认领。
    initialize_id: Option<RequestId>,
    /// 那个子进程。收工的时候靠它 `try_wait` / `kill`。
    child: Child,
    /// 我们写给它的话。包一层 `BufWriter` 是为了少几次系统调用 ——
    /// `framing::write_message` 结尾会 flush，所以不会攒着不发。
    stdin: BufWriter<ChildStdin>,
    events: Receiver<Event>,
    /// 它的 stderr + 我们自己记的账。合在一起是因为**对排查来说它们是同一件事**：
    /// 「刚才到底发生了什么」。
    log: Arc<Mutex<VecDeque<String>>>,
    next_id: i64,
    state: State,
}

impl Client {
    /// 起一个服务器。
    ///
    /// `args` 是给它的命令行参数 —— 很多语言服务器需要 `--stdio` 之类才能用管道说话，
    /// 所以这里不能省。
    ///
    /// ⚠️ **stderr 不接给终端**（跟 `:check` 里那条规矩一样）：它要是往我们的
    /// 备用屏上写字，画面立刻就花了。我们接管过来自己读。
    pub fn spawn(command: &str, args: &[&str], cwd: Option<&Path>) -> io::Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        let (tx, events) = mpsc::channel();
        let log: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));

        // ---- 读线程：它说的话 ----
        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    let event = match framing::read_message(&mut reader) {
                        Ok(Some(body)) => match message::parse(&body) {
                            Ok(msg) => Event::Message(msg),
                            Err(err) => {
                                Event::Broken(format!("服务器发来的不是合法的 JSON-RPC：{err}"))
                            }
                        },
                        // 干净收工：它自己关了管道（也就是退出了）
                        Ok(None) => Event::Broken("服务器关闭了管道（它退出了）".to_string()),
                        Err(err) => Event::Broken(format!("读服务器输出时出错：{err}")),
                    };
                    let broken = matches!(event, Event::Broken(_));
                    if tx.send(event).is_err() || broken {
                        // 发送失败 = 主循环已经走了；broken = 这条线没得救了
                        break;
                    }
                }
            });
        }

        // ---- 读线程：它自己的日志 ----
        //
        // ⚠️ **就算我们一个字都不要也必须读完。** 管道缓冲区一满，
        //    rust-analyzer 就会卡在写 stderr 上 —— 然后整个服务器都不动了。
        //    这是我们这边**必须**尽的义务，不是「顺便收个日志」。
        {
            let log = Arc::clone(&log);
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    let Ok(line) = line else { break };
                    push_log(&log, line);
                }
            });
        }

        Ok(Self {
            initialize_id: None,
            child,
            stdin: BufWriter::new(stdin),
            events,
            log,
            next_id: 1,
            state: State::Initializing,
        })
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// 日志尾巴（它自己的 stderr + 我们记的账）。
    pub fn log_tail(&self) -> Vec<String> {
        self.log
            .lock()
            .map(|log| log.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// 发 `initialize`。**不等回应** —— 回应会从 [`Client::next_event`] 出来。
    ///
    /// 为什么不在这儿等：这是在主循环里调用的，等它就是把界面冻住。
    /// 冷启动时 rust-analyzer 要加载整个 crate 图，可能好几秒。
    pub fn initialize(&mut self, root_uri: Option<&str>) -> io::Result<()> {
        let id = self.take_id();
        self.initialize_id = Some(id.clone());
        let text = message::request(
            &id,
            "initialize",
            initialize_params(root_uri, std::process::id()),
        );
        self.write(&text)
    }

    /// 非阻塞地取一条事件。没有就返回 `None`。
    ///
    /// 取到消息时，**该做的账在这一步就做完了**（见 [`Client::absorb`]）——
    /// 调用方拿到的已经是「处理过」的消息，不用自己去维护状态机。
    pub fn next_event(&mut self) -> Option<Event> {
        let event = match self.events.try_recv() {
            Ok(event) => event,
            // 暂时没有：正常，下一轮再看
            Err(TryRecvError::Empty) => return None,
            // 通道断了 = 读线程结束了。⚠️ 这跟 Empty 是两回事，不能混。
            Err(TryRecvError::Disconnected) => {
                Event::Broken("读线程结束了（这条线不会再有任何消息）".to_string())
            }
        };

        match &event {
            Event::Message(msg) => self.absorb(msg),
            Event::Broken(_) => self.state = State::Dead,
        }
        Some(event)
    }

    /// 发一条通知（不等回答）。`didOpen` / `didChange` 都是。
    ///
    /// ⚠️ 握手完成**之前**会被拒：规范说那段时间里服务器可以忽略一切
    /// （`initialize` 和 `exit` 除外），而 rust-analyzer 确实会。
    /// 与其「发了但没用」，不如在这里就拦住。
    pub fn notify(&mut self, method: &str, params: Value) -> io::Result<()> {
        if self.state == State::Initializing {
            return Err(io::Error::other(format!(
                "握手还没完成，现在不能发 `{method}`（服务器会忽略它）"
            )));
        }
        let text = message::notification(method, params);
        self.write(&text)
    }

    /// 优雅收工：`shutdown` → `exit` → 有上限地等 → 还没死就 `kill`。
    ///
    /// 可以重复调用（[`Drop`] 也会调它）。
    pub fn shutdown(&mut self) {
        if self.state == State::Dead {
            return;
        }
        self.state = State::ShuttingDown;

        // 规范规定的收尾顺序，不能反。
        // **不等 `shutdown` 的回应**：`Drop` 里不能有「可能永远不返回」的操作。
        let id = self.take_id();
        let _ = self.write(&message::request(&id, "shutdown", Value::Null));
        let _ = self.write(&message::notification("exit", Value::Null));

        // ⚠️ 有上限地等 —— 绝不能用 `child.wait()`，那可能永远挂住。
        if self.wait_bounded(GRACEFUL_EXIT).is_none() {
            // 它不肯走。那就别客气了 —— 留着一个语言服务器当孤儿，
            // 它会一直占着内存和文件锁。
            let _ = self.child.kill();
            // `kill` 之后 `wait` 会立刻返回；这一步是**收尸**，
            // 不做的话进程会变成僵尸（Unix 上）留在进程表里。
            let _ = self.child.wait();
        }
        self.state = State::Dead;
    }

    /// 非阻塞地问一句「它死了没」。
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    // ---------- 内部 ----------

    fn write(&mut self, text: &str) -> io::Result<()> {
        framing::write_message(&mut self.stdin, text)
    }

    fn take_id(&mut self) -> RequestId {
        let id = RequestId::number(self.next_id);
        self.next_id += 1;
        id
    }

    /// 有上限地等它退。返回 `None` = 这段时间里它没退。
    fn wait_bounded(&mut self, limit: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                // 还没退。这里的 sleep 是为了**别把 CPU 烧在轮询上** ——
                // 我们在等一个真实进程的调度，没有别的方式「稍后再问一次」。
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => return None,
            }
        }
        None
    }

    /// 把一条收到的消息该做的账做完。
    fn absorb(&mut self, msg: &Message) {
        match msg {
            Message::Request { id, method, params } => {
                // ⚠️ 这条分支是整个文件里最重要的：**必须回答**。
                // 记一笔是为了排查 —— 「服务器问了什么」是这条线上最常见的追问。
                push_log(&self.log, format!("← {method}（它在问我们）"));
                if let Err(err) = self.answer(id, method, params) {
                    push_log(&self.log, format!("回答 `{method}` 失败：{err}"));
                }
            }

            Message::Response { id, result } => {
                if Some(id) == self.initialize_id.as_ref() {
                    self.initialize_id = None;
                    match result {
                        Ok(_) => {
                            // 规范要求：收到 `initialize` 的回应之后，
                            // 立刻发 `initialized` 通知 —— 那之后才算「可以干活」。
                            if let Err(err) =
                                self.write(&message::notification("initialized", json!({})))
                            {
                                push_log(&self.log, format!("发 initialized 失败：{err}"));
                                self.state = State::Dead;
                                return;
                            }
                            self.state = State::Ready;
                        }
                        Err(err) => {
                            push_log(
                                &self.log,
                                format!(
                                    "服务器拒绝了 initialize：{} (code {})",
                                    err.message, err.code
                                ),
                            );
                            self.state = State::Dead;
                        }
                    }
                }
            }

            // 通知：调用方自己去关心（诊断就走这条）。
            //
            // 这里也记一笔账：**「它说了什么」和「它问了什么」一样重要**，
            // 而且这是排查时唯一能看得见的东西（它默认不往 stderr 写日志）。
            // ⚠️ `$/progress` 除外 —— 它加载项目时会把进度刷屏，
            //    记下来只会把真正有用的那几行挤出去。
            Message::Notification { method, .. } => {
                if !method.starts_with("$/progress") {
                    push_log(&self.log, format!("← {method}"));
                }
            }
        }
    }

    /// 服务器问我们问题 —— **每一条都必须回。**
    ///
    /// ⚠️ 这是接 LSP 最容易踩、又最难查的坑：少回一条，它会一直等。
    /// 不报错、不退出、什么都不干 —— 你会以为「服务器是不是没启动」，
    /// 然后去查进程、查配置、查路径，全都正常。
    ///
    /// 所以这里的原则是：**认识的好好答，不认识的也要答**（回一条
    /// 「方法不存在」的错误）。「我不认识」和「我不理你」对服务器来说是天壤之别。
    fn answer(&mut self, id: &RequestId, method: &str, params: &Value) -> io::Result<()> {
        let text = match method {
            // 「我要注册一个能力」（比如它想监听文件变更）。
            // 我们没有任何动态能力，回 null 就是「知道了」。
            "client/registerCapability" | "client/unregisterCapability" => {
                message::response(id, Value::Null)
            }

            // 「这几个文件/这个项目的配置是什么？」我们没有 LSP 配置 —— 全是 null。
            // ⚠️ 必须回**和它问的条数一样长**的数组：少一条它就对不上了。
            "workspace/configuration" => {
                let count = params
                    .get("items")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                message::response(id, Value::Array(vec![Value::Null; count]))
            }

            // 「能不能让我报个进度？」答应它 —— 我们还不画进度条，
            // 但不答应的话有些服务器会不高兴。
            "window/workDoneProgress/create" => message::response(id, Value::Null),

            // 「我要弹个提示让你选」—— 回 null = 用户把它关掉了。
            "window/showMessageRequest" => message::response(id, Value::Null),

            // 「诊断可能变了，你来问一遍」—— 这是「拉」诊断那一套的提示。
            // 我们用的是「推」，所以收到它什么也不用做，但**必须回 null**。
            // （实测：真的 rust-analyzer 会发这条。）
            "workspace/diagnostic/refresh" => message::response(id, Value::Null),

            // ⚠️ 其余的一律回「方法不存在」。**回一条错误也远远好过不回。**
            other => message::error_response(
                id,
                message::METHOD_NOT_FOUND,
                &format!("stbd does not handle `{other}`"),
            ),
        };
        self.write(&text)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // 守卫：**不管怎么出去**（正常退出、提前 return、panic），
        // 进程都会被收干净。这跟 `TerminalHandover` 是同一个手法。
        self.shutdown();
    }
}

/// 往环形缓冲里放一行日志，满了就丢最老的。
fn push_log(log: &Arc<Mutex<VecDeque<String>>>, line: String) {
    // 锁中毒（别的线程 panic 时）就放弃写日志 —— **记日志失败不该让程序崩**。
    let Ok(mut log) = log.lock() else { return };
    log.push_back(line);
    if log.len() > LOG_LINES {
        log.pop_front();
    }
}

/// `initialize` 的参数。
///
/// 抽成自由函数是为了能单独测它的**形状** —— 方法名多一个字母、少一个字段，
/// 服务器都不会报错，只会「什么都不发生」。
///
/// ## ⚠️ 实测：`window.workDoneProgress` 不能省
///
/// 这不是「顺手声明一下」，是**能不能拿到诊断的前提**。一个个变量拆过的结果：
///
/// | 声明了 `workDoneProgress` | 结果（真的 rust-analyzer，30 秒窗口） |
/// | ------------------------- | ------------------------------------- |
/// | 否                        | 推来的永远是**空数组**（项目根本没被加载） |
/// | 是                        | **3 秒内就有诊断**                      |
///
/// 它会把加载进度通过 `$/progress` 报出来，而声明之前**它连项目都不碰**。
/// 我们暂时不画进度条，但**为了让它真的干活，这条必须留着**。
///
/// 内部的因果关系没深挖，但相关性是可复现的（把这一行删掉就会退回去）。
pub fn initialize_params(root_uri: Option<&str>, process_id: u32) -> Value {
    // `rootUri` 规范里已标「弃用」，取而代之的就是 `workspaceFolders`。
    // 拆变量时确认过：光有它不够（真正的前提是下面那条能力声明），
    // 但给了是对的 —— 服务器靠它认出「这是哪个项目」。
    let folders = root_uri.map(|uri| json!([{ "uri": uri, "name": folder_name(uri) }]));

    json!({
        // 很多服务器**用不上**这个字段，但填对了有实际好处：
        // 我们崩了的时候，它可以靠这个 pid 发现自己成了孤儿，然后自己退。
        "processId": process_id,
        "clientInfo": { "name": "stbd", "version": env!("CARGO_PKG_VERSION") },
        "rootUri": root_uri,
        "workspaceFolders": folders,
        "capabilities": {
            // 只声明**我们真的会处理**的。声明得越多，服务器照着你发的越多。
            "workspace": {
                // 声明了它，服务器才**会**来问配置（我们答「没有配置」）
                "configuration": true,
            },
            // ⚠️ 这一条是**拿到诊断的前提**，不是为了好看 —— 见上面的实测表。
            "window": { "workDoneProgress": true },
            "textDocument": {
                "publishDiagnostics": {}
            }
        },
    })
}

/// 从一个 `file://` uri 里猜出目录名。
///
/// 它只是给人看的**显示名** —— rust-analyzer 认的是 `uri`。所以这里
/// 不追求解百分号编码：中文目录名会显示成 `%E9%A1%B9...`，难看但无害。
fn folder_name(uri: &str) -> String {
    uri.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(uri)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_params_declare_only_what_we_handle() {
        let params = initialize_params(Some("file:///D:/proj"), 4242);
        assert_eq!(params["processId"], 4242);
        assert_eq!(params["rootUri"], "file:///D:/proj");
        assert_eq!(params["clientInfo"]["name"], "stbd");
        assert!(params["capabilities"]["textDocument"]["publishDiagnostics"].is_object());
    }

    /// ⚠️ 这条守着一个**实测出来的前提**：少了 `window.workDoneProgress`，
    /// 真的 rust-analyzer 就不加载项目，诊断永远是空的（拆过变量确认过）。
    #[test]
    fn initialize_params_declare_work_done_progress() {
        let params = initialize_params(Some("file:///D:/proj/stbd"), 1);
        assert_eq!(
            params["capabilities"]["window"]["workDoneProgress"], true,
            "少了这条，服务器不会加载项目 —— 见 initialize_params 的注释"
        );

        let folders = params["workspaceFolders"]
            .as_array()
            .expect("得给 workspaceFolders");
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0]["uri"], "file:///D:/proj/stbd");
        assert_eq!(folders[0]["name"], "stbd", "显示名取自 uri 最后一段");
    }

    /// 没有项目根的时候要**明确**给 `null`，而不是干脆不写这个字段。
    #[test]
    fn initialize_params_can_say_there_is_no_root() {
        let params = initialize_params(None, 1);
        assert!(params["rootUri"].is_null());
        assert!(params["workspaceFolders"].is_null());
        assert!(params.as_object().unwrap().contains_key("rootUri"));
        assert!(params.as_object().unwrap().contains_key("workspaceFolders"));
    }
}
