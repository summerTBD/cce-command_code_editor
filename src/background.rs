//! 后台任务的**收件端**。
//!
//! `:check` 和 `:fmt` 是同一个形状：主循环起一个线程，线程干完活通过 `mpsc`
//! 把结果送回来，主循环每轮（50ms tick）顺手看一眼。
//!
//! 这个模块只管一件事：**怎么正确地「看一眼」**。
//!
//! ## 为什么它值得单独一个模块
//!
//! 因为那段代码本来在 `main.rs` 里**写了两遍** —— `:check` 一份、`:fmt` 一份，
//! 而第二份是照着第一份抄的（连注释都是）。
//!
//! 两处「看一眼」各自都要拿捏两条很容易拿捏错的判断（见 [`Worker::poll`]）。
//! 只要有一份漏了，症状是「任务悄悄死了却没人发现」—— 状态栏永远停在
//! 「Checking…」，而且**没有东西会再来改它**，所以从屏幕上根本看不出是哪一个
//! 任务坏了。这类 bug 是最难查的那种：间歇、没有日志、看起来像「功能没做」。
//!
//! 更糟的是：**`main.rs` 的主循环测不了**（要一个真终端），所以那两条判断
//! 一直没被任何测试盯着。搬到这里之后，它们第一次有了测试。
//!
//! ## 为什么不连「两个任务」也一起抽象掉
//!
//! 因为两个任务的**中间**不一样：`:check` 读 stderr 攒一个环形缓冲，`:fmt`
//! 要往 stdin 写正文。硬把那段也统一，就得往里塞闭包和 trait —— 那是**提前
//! 泛化**，正是当初把「一组固定参数 + 几个开关」写死成静态表的反面教训。
//! 这里统一的只有**判断**，因为那才是重复的东西。

use std::sync::mpsc::{Receiver, TryRecvError};

/// 轮询一个后台任务：它这一轮说了什么、收工了没有。
#[derive(Debug, PartialEq, Eq)]
pub enum Progress<T> {
    /// 还没消息，也还没收工 —— 最常见的那一轮
    Quiet,
    /// 它说了这些。`over` 为真表示它**同时**收工了，不会再有下一批
    Reports { reports: Vec<T>, over: bool },
    /// **死在开口之前**：一条消息都没发，发送端就没了。
    ///
    /// ⚠️ 这是唯一需要报警的情况 —— 用户看到的是状态栏永远停在
    /// 「Checking…」，而没有任何东西会再来改它。
    DiedBeforeSpeaking,
}

impl<T> Progress<T> {
    /// 这一轮拿到的消息（`Quiet` 和 `DiedBeforeSpeaking` 都是空的）。
    pub fn reports(&self) -> &[T] {
        match self {
            Progress::Reports { reports, .. } => reports,
            _ => &[],
        }
    }
}

/// 一个后台任务的收件端。
///
/// 比裸 `Receiver` 多记住**一件事**：它开口过没有。
///
/// 那点记忆是必要的 —— 「死在开口之前」和「说完话之后正常收工」在最后一轮
/// 长得**一模一样**（都是「这一轮没消息 + 通道已关」），只有「它以前说过话吗」
/// 能把两者分开。
///
/// ⚠️ 把这点记忆放在这里，而不是让主循环为每个任务各留一个 bool ——
/// 后者正是重复开始的地方（两个 bool、两处判断、迟早有一处写错）。
pub struct Worker<T> {
    rx: Receiver<T>,
    /// 它至少发过一条消息吗
    spoke: bool,
}

impl<T> Worker<T> {
    pub fn new(rx: Receiver<T>) -> Self {
        Self { rx, spoke: false }
    }

    /// 看一眼。
    ///
    /// ## ⚠️ 两条判断都必须拿对
    ///
    /// 1. **一口气全取走**（`while` 而不是取一条）：后台攒了几条就消化几条，
    ///    落后的永远只有「当前这一轮」。现在两个任务都只发一条，但形状先立对 ——
    ///    消息一连串来的任务（语言服务器那一步）不该再把这段改一遍。
    /// 2. **`Disconnected` 和 `Empty` 必须分开看**：前者是「发送端没了」
    ///    （正常发完就结束，或者线程 panic 了），后者只是「暂时没消息」。
    ///    混为一谈的话，「任务悄悄死了」就永远发现不了。
    pub fn poll(&mut self) -> Progress<T> {
        let mut reports = Vec::new();
        while let Ok(report) = self.rx.try_recv() {
            reports.push(report);
        }
        if !reports.is_empty() {
            self.spoke = true;
        }

        // 队列已经排空，所以这里的 `Disconnected` 是**真的**到头了 ——
        // 而 `Empty` 只意味着「它还没说话」，不是结束
        let over = matches!(self.rx.try_recv(), Err(TryRecvError::Disconnected));

        match (reports.is_empty(), over, self.spoke) {
            // 没消息、也没结束：什么都不用做
            (true, false, _) => Progress::Quiet,
            // 一条都没说过就断了 = 线程死在开口之前
            (true, true, false) => Progress::DiedBeforeSpeaking,
            // 其余都是「有消息」。注意这里包括**这一轮没消息但以前说过**、
            // 而且现在收工了的情况（`reports` 是空数组）—— 那不是「死了」，
            // 是正常结束。上面那条 `self.spoke` 就是为这个分支存在的。
            _ => Progress::Reports { reports, over },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// 最常见的那些轮：没消息、没结束。
    #[test]
    fn nothing_to_report_is_quiet() {
        let (_tx, rx) = mpsc::channel::<i32>();
        let mut worker = Worker::new(rx);

        assert_eq!(worker.poll(), Progress::Quiet);
        // 多看一眼也不该变 —— 「Quiet」不能自己长成别的东西
        assert_eq!(worker.poll(), Progress::Quiet);
    }

    /// **一口气全取走**，而且保持顺序。
    #[test]
    fn everything_that_is_waiting_comes_out_in_one_go() {
        let (tx, rx) = mpsc::channel();
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        tx.send(3).unwrap();
        let mut worker = Worker::new(rx);

        // ⚠️ 三次是**一次 poll 全拿到**，不是一次一条：任务慢了我们也不该越落越远
        assert_eq!(
            worker.poll(),
            Progress::Reports {
                reports: vec![1, 2, 3],
                over: false
            }
        );
    }

    /// 线程**说完话就正常收工** —— 消息和「收工了」在同一次 poll 里出来。
    #[test]
    fn a_message_and_the_end_can_arrive_together() {
        let (tx, rx) = mpsc::channel();
        tx.send("done").unwrap();
        drop(tx); // 线程结束 = 发送端被丢掉
        let mut worker = Worker::new(rx);

        assert_eq!(
            worker.poll(),
            Progress::Reports {
                reports: vec!["done"],
                over: true
            }
        );
    }

    /// ⚠️⚠️ **这条是这个模块存在的核心理由**。
    ///
    /// 「它以前说过话」和「它从没说过话」在最后一轮长得一模一样
    /// （都是 `reports` 空 + 通道已关），但含义**完全相反**：
    /// 前者是正常结束，后者是线程死在开口之前（必须报警）。
    ///
    /// 拿「这一轮有没有消息」当判据的实现会在这里**误报** ——
    /// 而那会盖掉已经写好的那句好消息，让人以为刚跑完的任务是坏的。
    #[test]
    fn speaking_once_and_then_ending_is_not_dying() {
        let (tx, rx) = mpsc::channel();
        tx.send("done").unwrap();
        let mut worker = Worker::new(rx);

        // 第一批：拿到消息，但线程还没死（发送端还在手上）
        assert_eq!(
            worker.poll(),
            Progress::Reports {
                reports: vec!["done"],
                over: false
            }
        );

        drop(tx);
        // 第二批：这一轮**没有消息**，但通道关了。它以前说过话 —— 那是**结束**，
        // 不是「死在开口之前」
        assert_eq!(
            worker.poll(),
            Progress::Reports {
                reports: Vec::new(),
                over: true
            },
            "说过话之后收工，不该被报成「线程死了」"
        );
    }

    /// 反过来：一条都没说就断了 —— 那必须能被识别出来。
    #[test]
    fn a_thread_that_dies_before_saying_anything_is_reported() {
        let (tx, rx) = mpsc::channel::<i32>();
        drop(tx); // 线程 panic 了，或者起完就退
        let mut worker = Worker::new(rx);

        assert_eq!(worker.poll(), Progress::DiedBeforeSpeaking);
    }

    /// 线程**发过一条之后**再断，不该被误报（跟上面那条配成一对）。
    #[test]
    fn a_thread_that_spoke_first_is_never_called_dead() {
        let (tx, rx) = mpsc::channel();
        tx.send(1).unwrap();
        drop(tx);
        let mut worker = Worker::new(rx);

        // 就算第一条和「断开」在同一次 poll 里到手，也必须是 Reports
        let first = worker.poll();
        assert_ne!(first, Progress::DiedBeforeSpeaking);
        // 再poll 也还是 Reports（收工），不该翻成「死了」
        let second = worker.poll();
        assert_ne!(second, Progress::DiedBeforeSpeaking);
    }

    #[test]
    fn reports_can_be_read_without_consuming_the_progress() {
        let (tx, rx) = mpsc::channel();
        tx.send(7).unwrap();
        let mut worker = Worker::new(rx);

        let progress = worker.poll();
        assert_eq!(progress.reports(), &[7]);
        assert_eq!(Progress::<i32>::Quiet.reports(), &[] as &[i32]);
        assert_eq!(Progress::<i32>::DiedBeforeSpeaking.reports(), &[] as &[i32]);
    }
}
