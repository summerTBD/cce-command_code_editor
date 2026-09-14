//! LSP 的「Base Protocol」：给消息套一个长度头 —— framing.rs 的职责
//!
//! ## 线上长什么样
//!
//! ```text
//! Content-Length: 42\r\n
//! \r\n
//! {"jsonrpc":"2.0", ... }      ← 正好 42 个**字节**
//! ```
//!
//! 就这些。头部沿用 HTTP 的形式（`名字: 值`），用一个空行结束，后面跟正文。
//! 规范里还可能带 `Content-Type`，我们不需要它 —— **不认识的头直接忽略**。
//!
//! ## 三个必须记住的坑
//!
//! **① 长度数的是字节，不是字符。**
//! 一条含中文的消息（诊断消息里全是中文是常事），按字符数报长度，
//! 对面会少读几个字节，然后把下一条消息的头当成正文的一部分 —— 之后全乱。
//! 而且乱得**很隐蔽**：前几条还正常，一直到那条中文消息出现才崩。
//!
//! **② 长度不能信。**
//! 头里写个天文数字就能让我们按那个数字去分配内存。那不是「对面说多少我们
//! 收多少」，那是**用对方的输入决定我们的分配**。所以要有个上限。
//!
//! **③ 干净的结束和断在半路不是一回事。**
//! 对面收工关掉管道（正常）和一条消息读到一半就没了（协议已经坏了）
//! 必须分得开 —— 前者要安静退出，后者要报错。这就是返回 `Option` 的原因。
//!
//! ## 为什么这一层能「金标准」测
//!
//! 它不解析 JSON、不懂 `method` 是什么、不知道 `id` 有什么用。它只有
//! 「几个字节的近去出」。所以可以拿一段**手写的字节**当标准答案逐字节对，
//! 而不用启动任何进程。

use std::io::{self, BufRead, Write};

/// 超过这个长度的消息直接拒收。
///
/// 64 MiB 对 LSP 来说绰绰有余（真正常见的也就几十 KB），
/// 但它挡住了「对面一个数字让我们分配 2 GiB」这种事。
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// 读一条消息的正文。**头部被完整吃掉，返回的只有正文。**
///
/// - `Ok(Some(body))`：收到一条
/// - `Ok(None)`：对面**干净地**收工了（管道关闭，而当时我们正要读一条新消息的开头）
/// - `Err(..)`：中间断了，或者发来的字节不符合规范
pub fn read_message<R: BufRead>(reader: &mut R) -> io::Result<Option<String>> {
    let mut length: Option<usize> = None;
    let mut header_lines = 0usize;

    // ---------- 头 ----------
    loop {
        let mut raw = String::new();
        let read = reader.read_line(&mut raw)?;

        if read == 0 {
            return if header_lines == 0 {
                // 一个字都还没读到 → 对面正常收工，不是错误
                Ok(None)
            } else {
                Err(invalid(
                    "stream ended in the middle of the header".to_string(),
                ))
            };
        }
        header_lines += 1;

        // ⚠️ 严格按规范要求 CRLF。宽容处理（也接受裸 LF）会把真正的格式错误
        //    藏起来 —— 而这一层最大的价值就是「第一时间发现对面在乱说」。
        let Some(line) = raw.strip_suffix("\r\n") else {
            return Err(invalid(format!(
                "header line must end with CRLF, got {raw:?}"
            )));
        };

        // 空行 = 头结束
        if line.is_empty() {
            break;
        }

        let Some((name, value)) = line.split_once(':') else {
            return Err(invalid(format!("malformed header line: {line:?}")));
        };

        // HTTP 的头名是大小写不敏感的，规范说头部分沿用 HTTP 的格式，
        // 所以 `content-length` 也得认
        if name.trim().eq_ignore_ascii_case("Content-Length") {
            let value = value.trim();
            let parsed: usize = value
                .parse()
                .map_err(|_| invalid(format!("bad Content-Length: {value:?}")))?;
            if parsed > MAX_MESSAGE_BYTES {
                return Err(invalid(format!(
                    "Content-Length {parsed} exceeds the {MAX_MESSAGE_BYTES} byte cap"
                )));
            }
            length = Some(parsed);
        }
        // 其余的头（Content-Type 等）一律忽略：我们用不到，也不该因此报错
    }

    let Some(length) = length else {
        return Err(invalid("missing Content-Length header".to_string()));
    };

    // 0 字节的正文不是一条消息。在这里报，比让 JSON 层回一句
    // 「EOF while parsing a value」清楚得多。
    if length == 0 {
        return Err(invalid("empty message body".to_string()));
    }

    // ---------- 正文 ----------
    // 先按长度开一块字节，再一次性读满：`read_exact` 会把「不够长」
    // 报成 `UnexpectedEof`，正好是我们要的语义。
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).map_err(|err| {
        // 保留原来的 ErrorKind（`UnexpectedEof` 有讲究），只补一句上下文
        io::Error::new(
            err.kind(),
            format!("stream ended in the middle of a {length}-byte body"),
        )
    })?;

    // LSP 规定正文是 UTF-8。在这里就验掉，别让一串乱字节流到 JSON 层去。
    String::from_utf8(body)
        .map(Some)
        .map_err(|err| invalid(format!("body is not valid UTF-8: {err}")))
}

/// 写一条消息。**会 flush。**
pub fn write_message<W: Write>(writer: &mut W, body: &str) -> io::Result<()> {
    // ⚠️ `String::len()` 是**字节数**，正是这里要的。
    //    写成 `.chars().count()` 就会在含中文的消息上出错（见文件头 ①）。
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body.as_bytes())?;
    // ⚠️ 必须 flush：不刷的话消息会躺在缓冲区里，对面永远等不到，
    //    表现出来是「两边都在等对方」，而且两边看起来都很正常。
    writer.flush()
}

/// 造一个「对面发来的字节不符合规范」的错误。
///
/// 统一用 `InvalidData`：调用方（读线程）**不需要区分细节，它只需要知道
/// 「这条线不能用了」**。但消息必须写具体 —— 排查的时候那是唯一线索。
fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    /// 一条「每次只给一个字节」的 reader。
    ///
    /// 真管道不会规规矩矩一次给一整条消息：头可能被切成两半，正文可能分几次到，
    /// 甚至两个消息可能挤在一次 read 里。这个 reader 把「零散到达」放大到极限 ——
    /// 只要分帧逻辑有一点「假设一次能读全」的地方，它在这里就活不下去。
    struct Drip {
        data: Vec<u8>,
        at: usize,
    }

    impl io::Read for Drip {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.at >= self.data.len() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.data[self.at];
            self.at += 1;
            Ok(1)
        }
    }

    fn drip(text: &str) -> BufReader<Drip> {
        BufReader::new(Drip {
            data: text.as_bytes().to_vec(),
            at: 0,
        })
    }

    fn read(text: &str) -> io::Result<Option<String>> {
        read_message(&mut BufReader::new(Cursor::new(text.as_bytes().to_vec())))
    }

    // ---------- 写 ----------

    /// **金标准**：手写一段正确的字节，逐字节对。
    #[test]
    fn write_produces_exactly_the_spec_bytes() {
        let mut out = Vec::new();
        write_message(&mut out, r#"{"a":1}"#).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Content-Length: 7\r\n\r\n{\"a\":1}"
        );
    }

    /// ⚠️ 这条是本文件里最该存在的一条。
    ///
    /// `{"m":"你好"}` 是 10 个字符 / 14 个字节。报成 10 的话，对面会少读
    /// 4 个字节 —— 然后把下一条消息的头吃进正文里。**前几条消息全都正常**，
    /// 一直到这条中文的出现才开始崩，所以查起来会怀疑人生。
    #[test]
    fn the_length_counts_bytes_not_characters() {
        let body = r#"{"m":"你好"}"#;
        assert_eq!(body.chars().count(), 10, "字符数");
        assert_eq!(body.len(), 14, "字节数");

        let mut out = Vec::new();
        write_message(&mut out, body).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with("Content-Length: 14\r\n\r\n"),
            "必须报字节数，实际是 {text:?}"
        );
    }

    // ---------- 读 ----------

    #[test]
    fn read_round_trips_what_write_produced() {
        let mut out = Vec::new();
        write_message(&mut out, r#"{"hello":"世界"}"#).unwrap();
        let back = read(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(back.as_deref(), Some(r#"{"hello":"世界"}"#));
    }

    /// ⚠️ **粘包**：一次 read 拿到两条消息的字节。必须一条一条地吐出来。
    #[test]
    fn two_messages_stuck_together_are_read_separately() {
        let mut out = Vec::new();
        write_message(&mut out, r#"{"n":1}"#).unwrap();
        write_message(&mut out, r#"{"n":2}"#).unwrap();

        let mut reader = BufReader::new(Cursor::new(out));
        assert_eq!(
            read_message(&mut reader).unwrap().as_deref(),
            Some(r#"{"n":1}"#)
        );
        assert_eq!(
            read_message(&mut reader).unwrap().as_deref(),
            Some(r#"{"n":2}"#)
        );
        // 第三条：干净收工
        assert_eq!(read_message(&mut reader).unwrap(), None);
    }

    /// ⚠️ **分片**：每个字节单独到达。见 [`Drip`] 的说明。
    #[test]
    fn a_message_split_across_many_reads_still_works() {
        let mut out = Vec::new();
        write_message(&mut out, r#"{"你好":"世界"}"#).unwrap();
        let text = String::from_utf8(out).unwrap();

        let mut reader = drip(&text);
        assert_eq!(
            read_message(&mut reader).unwrap().as_deref(),
            Some(r#"{"你好":"世界"}"#)
        );
        assert_eq!(read_message(&mut reader).unwrap(), None);
    }

    #[test]
    fn unknown_headers_are_ignored() {
        let text = "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n\
                    Content-Length: 7\r\n\
                    \r\n\
                    {\"a\":1}";
        assert_eq!(read(text).unwrap().as_deref(), Some(r#"{"a":1}"#));
    }

    #[test]
    fn the_header_name_is_case_insensitive() {
        // HTTP 的头名不分大小写，规范说头部分沿用 HTTP 的格式
        let text = "content-length: 7\r\n\r\n{\"a\":1}";
        assert_eq!(read(text).unwrap().as_deref(), Some(r#"{"a":1}"#));
    }

    #[test]
    fn the_header_value_may_have_spaces_around_it() {
        let text = "Content-Length:   7  \r\n\r\n{\"a\":1}";
        assert_eq!(read(text).unwrap().as_deref(), Some(r#"{"a":1}"#));
    }

    /// 一条正文里含中文的消息，字节数别处算错时这里就会露馅。
    #[test]
    fn a_body_with_multibyte_characters_reads_back_intact() {
        let body = r#"{"message":"找不到值 `foo`"}"#;
        let mut out = Vec::new();
        write_message(&mut out, body).unwrap();
        assert_eq!(
            read(&String::from_utf8(out).unwrap()).unwrap().as_deref(),
            Some(body)
        );
    }

    // ---------- 干净收工 vs 断在半路 ----------

    #[test]
    fn a_closed_stream_is_a_clean_end_not_an_error() {
        assert_eq!(read("").unwrap(), None);
    }

    #[test]
    fn a_stream_that_dies_in_the_header_is_an_error() {
        // 头还没结束（没有那个空行）
        let err = read("Content-Length: 7\r\n").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("middle of the header"), "{err}");
    }

    #[test]
    fn a_stream_that_dies_in_the_body_is_an_error() {
        // 说好 7 个字节，只给了 3 个
        let err = read("Content-Length: 7\r\n\r\n{\"a").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(err.to_string().contains("middle of a 7-byte body"), "{err}");
    }

    // ---------- 各种乱说的头 ----------

    #[test]
    fn a_missing_content_length_is_an_error() {
        let err = read("\r\n{\"a\":1}").unwrap_err();
        assert!(err.to_string().contains("missing Content-Length"), "{err}");
    }

    #[test]
    fn a_non_numeric_length_is_an_error() {
        let err = read("Content-Length: abc\r\n\r\n").unwrap_err();
        assert!(err.to_string().contains("bad Content-Length"), "{err}");
    }

    #[test]
    fn a_negative_length_is_an_error() {
        // `-1` 解析成 usize 会失败 —— 正好，不用单独处理负数
        let err = read("Content-Length: -1\r\n\r\n").unwrap_err();
        assert!(err.to_string().contains("bad Content-Length"), "{err}");
    }

    /// ⚠️ 这条守着一个**内存安全**性质：头里一个天文数字不能变成我们的分配。
    #[test]
    fn an_absurd_length_is_rejected_without_allocating() {
        let err = read("Content-Length: 99999999999999\r\n\r\n").unwrap_err();
        assert!(err.to_string().contains("cap"), "{err}");
    }

    #[test]
    fn a_header_line_without_a_colon_is_an_error() {
        let err = read("Content-Length 7\r\n\r\n").unwrap_err();
        assert!(err.to_string().contains("malformed header"), "{err}");
    }

    #[test]
    fn a_bare_lf_is_rejected() {
        // 规范要求 CRLF。宽容处理会把真正的格式错误藏起来。
        let err = read("Content-Length: 7\n\n{\"a\":1}").unwrap_err();
        assert!(err.to_string().contains("must end with CRLF"), "{err}");
    }

    #[test]
    fn an_empty_body_is_an_error() {
        let err = read("Content-Length: 0\r\n\r\n").unwrap_err();
        assert!(err.to_string().contains("empty message body"), "{err}");
    }

    #[test]
    fn a_body_that_is_not_utf8_is_an_error() {
        let mut bytes = b"Content-Length: 2\r\n\r\n".to_vec();
        bytes.extend_from_slice(&[0xFF, 0xFE]); // 不是合法的 UTF-8 序列
        let err = read_message(&mut BufReader::new(Cursor::new(bytes))).unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"), "{err}");
    }
}
