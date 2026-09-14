//! 输出文件夹 —— 长输出的落盘处 —— outbox.rs 的职责
//!
//! ## 它解决什么
//!
//! 有几条命令的输出**装不下一行**（`:ls` 有几个文档、`:errors` 这个文件哪些毛病）。
//! 那些东西写在状态栏里会**从右边被切掉**，而且切得没有痕迹 —— 屏幕上剩一个
//! 看起来挺完整的开头，你会以为自己只打开了三个文件。
//!
//! 所以它们不往状态栏写，而是**写成一个文件**，然后把那个文件铺到屏幕上：
//!
//! ```text
//! :ls     →  <输出文件夹>/file_list.txt   ← 屏幕上显示的就是它
//! :errors →  <输出文件夹>/error_log.txt
//! ```
//!
//! ## 三条规矩（都是刻意的）
//!
//! 1. **这几个文件始终存在，不会被删掉。** 空着也留着 —— 这样 `:ls` 不依赖
//!    「上一次跑程序有没有留下什么」，也就不用处理「文件不在」这种情况。
//! 2. **内容只在那条命令跑的时候改。** `:ls` 写完之后就不管了：诊断变了、
//!    文档列表变了，文件里那行字也不会自己变。所以「屏幕上这份东西是什么时候的」
//!    永远是确定的 —— 想知道现在什么样，再敲一次那条命令。
//! 3. **只在进入时清一次。** 上一次那张列表看起来跟这一次的一模一样，
//!    你会拿着上次的当这次的用；进清一次，你就永远是拿这一次的。
//!
//!    ⚠️ 这里原来还有「**退出时也清一次**」，2026-09-15 去掉了。它和上面第 2 条
//!    其实是**矛盾**的（退出不是「那条命令跑的时候」），而代价是实测出来的：
//!    `:errors` 之后一 `:q`，`error_log.txt` 就空了 —— 于是「把清单写成文件」
//!    只剩下「编辑器开着时另一个窗口去读」这一种用法，退出之后就拿不到了。
//!    而它**换不来任何东西**：进入那次清已经保证了「打开时看到的都是干净的」，
//!    不管上次是正常退出还是崩掉。
//!
//! ## ⚠️ 清理只**清空我们的文件**，不碰别人的
//!
//! 「清理」是把上面那几个文件截成 0 字节，不是删了重建，更不是把文件夹清空。
//!
//! 唯一的例外是 [`LEGACY_NAMES`]（我们**以前**用过的名字）：那些会真的被删掉，
//! 不然文件夹里会同时躺着两份看起来一样的东西。
//!
//! 除此之外一个都不碰 —— 删东西是这里唯一**不可逆**的操作，而它换不来任何东西：
//! 文件夹是我们自己的，里面除了这几个文件本来也不该有别的东西；
//! 万一有（用户放进去的），那也轮不到我们替他扔。
//!
//! ## 放哪儿
//!
//! 跟配置文件一个思路（见 `config::Config::candidate_paths`）：**可执行文件旁边**
//! 优先（便携，不往系统盘塞），拿不到就退回用户配置目录。
//! 另外 `STBD_OUTBOX` 可以整个换掉这个位置。

use std::io;
use std::path::{Path, PathBuf};

use crate::config;

/// 文件夹叫什么。
///
/// `out` 这个后缀是给人看的：它跟 `stbd-settings.toml` 摆在同一个目录里，
/// 名字上得说清楚「这是程序自己用的，别手改」。
pub const FOLDER_NAME: &str = "stbd-out";

/// 用环境变量直接指定输出文件夹（换掉整个位置，测试和多套环境用）。
pub const FOLDER_PATH_ENV: &str = "STBD_OUTBOX";

/// **以前用过、现在不用的文件名字。**
///
/// 清理的时候要把它们也一并弄掉 —— 不然文件夹里会同时躺着两份「看起来一样的
/// 东西」（旧名字空着、新名字有内容），而那个空文件名会让人以为「这里写过什么」。
/// 那正是「保证无污染」要防的事。
///
/// ⚠️ 只列**我们确实生成过**的名字。清理时不认识的、用户自己放进去的东西
/// 一律不碰（见 [`Outbox::clean`]）—— 删东西是这里唯一不可逆的操作。
///
/// 这个表只增不减：它就是「这个程序曾经在这里写过哪些文件」的记录。
const LEGACY_NAMES: &[&str] = &[
    // 最早用的是中文名，后来改英文 —— 中文名在有些 shell 和工具链里不好使。
    "文件列表.txt",
    "报错信息.txt",
    // 中间短暂用过这两个名字。⚠️ 就算你没跑过那个版本、盘上从来没有过它们，
    //    也得留着 —— 删除是「尽力而为」，不存在就跳过，留着只是多条保险。
    "documents.txt",
    "errors.txt",
];

/// 长输出往哪个文件里写。
///
/// 加一种就多一个变体，然后记得把它加进 [`OutFile::ALL`] ——
/// 清理靠那个列表走，漏了的话那个文件永远不会被清，规矩 3 就破了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutFile {
    /// `file_list.txt`：`:ls` 的产出
    FileList,
    /// `error_log.txt`：`:errors` 的产出
    ErrorLog,
    /// `lsp_status.txt`：`:lsp` 的产出（配了哪些语言服务器、命令在不在）
    LspStatus,
}

impl OutFile {
    /// 全部已知的产出文件（清理按这个走）。
    pub const ALL: &'static [OutFile] = &[OutFile::FileList, OutFile::ErrorLog, OutFile::LspStatus];

    /// 它在文件夹里叫什么名字。
    ///
    /// ⚠️ **一律用小写 ASCII**。用中文名会在两个地方咬人：
    /// shell 里得引号包着、有些工具链和脚本会把它当成乱码；
    /// 而我们自己的终端输出本来就是 GBK 花屏的（见 `README` 里那条）。
    ///
    /// ⚠️ 名字和枚举变体**对得上**（`FileList` ↔ `file_list.txt`、
    /// `ErrorLog` ↔ `error_log.txt`），这不是巧合：看到屏幕上一行
    /// `→ error_log.txt` 就知道是哪里写的，不用查表。改名字的时候两边一起改 ——
    /// 改名后的测试 `every_output_file_has_a_name_and_is_listed_for_cleaning`
    /// 只卡 ASCII 和 `.txt`，卡不住这种对应。
    pub fn name(self) -> &'static str {
        match self {
            Self::FileList => "file_list.txt",
            Self::ErrorLog => "error_log.txt",
            Self::LspStatus => "lsp_status.txt",
        }
    }
}

/// 输出文件夹。
///
/// `dir` 是 `None` 表示**这个位置用不了**（比如环境受限、目录建不出来）。
/// 那不算错误：编辑器照常用，只是长输出不落盘 —— 跟「`rust-analyzer` 没装」
/// 同一个立场：锦上添花的东西不该拦住基本盘。
pub struct Outbox {
    dir: Option<PathBuf>,
}

impl Outbox {
    /// 一个**用不了**的输出文件夹（写不进去，但也不会报错打断你）。
    pub fn unavailable() -> Self {
        Self { dir: None }
    }

    /// 指定一个目录（测试用；生产走 [`Outbox::locate`]）。
    pub fn at(dir: PathBuf) -> Self {
        Self { dir: Some(dir) }
    }

    /// 找（或定下）输出文件夹的位置。
    ///
    /// ⚠️ 它**不去建目录** —— 建目录会失败（只读位置、权限），而失败该发生在
    /// 「真的要写」那一刻，那时候才有话说给用户听。这里只定位置。
    pub fn locate() -> Self {
        if let Some(raw) = std::env::var_os(FOLDER_PATH_ENV)
            && !raw.is_empty()
        {
            return Self::at(PathBuf::from(raw));
        }
        if let Some(base) = config::config_directory() {
            return Self::at(base.join(FOLDER_NAME));
        }
        Self::unavailable()
    }

    /// 输出文件夹在哪（`None` = 用不了）。
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// 某个产出文件的完整路径（`None` = 用不了）。
    pub fn path_of(&self, file: OutFile) -> Option<PathBuf> {
        Some(self.dir.as_ref()?.join(file.name()))
    }

    /// 清空所有产出文件的内容（进入和退出各调一次）。
    ///
    /// - 文件夹不在 → 建出来
    /// - 文件不在 → 建成空的（规矩 1：它们**始终存在**）
    /// - 文件在 → 截成 0 字节
    /// - [`LEGACY_NAMES`] 里那些旧名字 → **删掉**（见那个常量的说明）
    ///
    /// ⚠️ 除了上面这些，**一个文件都不碰**：用户自己放进来的东西、
    /// 别的程序丢进来的东西，都轮不到我们替他扔。
    /// 删东西是这里唯一不可逆的操作，而它对不认识的文件换不来任何好处。
    ///
    /// 失败不往上抛：清理失败不该拦住程序启动或退出，那种时候用户什么也做不了，
    /// 说一句反而是噪音。
    pub fn clean(&self) {
        let Some(dir) = self.dir.as_ref() else {
            return;
        };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        for file in OutFile::ALL {
            let _ = std::fs::write(dir.join(file.name()), "");
        }
        for legacy in LEGACY_NAMES {
            // 删不掉（不存在、被占着）都无所谓 —— 它的目的只是「别留着」
            let _ = std::fs::remove_file(dir.join(legacy));
        }
    }

    /// 把一份长输出写进去，返回写到了哪个文件。
    ///
    /// 目录不存在会先建出来（用户可能自己删掉了那个文件夹 —— 那是他的自由，
    /// 下次要用时再长出来就是了）。
    pub fn write(&self, file: OutFile, text: &str) -> io::Result<PathBuf> {
        let Some(dir) = self.dir.as_ref() else {
            return Err(io::Error::other("no output folder"));
        };
        std::fs::create_dir_all(dir)?;
        let target = dir.join(file.name());
        std::fs::write(&target, text)?;
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每次用一个新的空目录 —— 测试之间不共用，也就不会互相看见对方的文件。
    fn scratch(name: &str) -> (Outbox, PathBuf) {
        let dir = std::env::temp_dir().join(format!("stbd-outbox-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        (Outbox::at(dir.clone()), dir)
    }

    #[test]
    fn every_output_file_has_a_name_and_is_listed_for_cleaning() {
        for file in OutFile::ALL {
            assert!(!file.name().is_empty());
            // ⚠️ 一律小写 ASCII：中文名在 shell / 工具链里不好使，
            //    而我们自己的终端输出本来就是 GBK 花屏的
            assert!(
                file.name().is_ascii()
                    && file.name().chars().all(|c| c.is_ascii_lowercase()
                        || c.is_ascii_digit()
                        || c == '.'
                        || c == '_'
                        || c == '-'),
                "文件名得是小写 ASCII：{}",
                file.name()
            );
            assert!(file.name().ends_with(".txt"), "{}", file.name());
        }
        // ⚠️ 清理按 ALL 走 —— 加了新变体却忘了加进 ALL 的话，那个文件永远不会被清，
        //    「进入时清一次」这条规矩就悄悄破了一个角
        assert_eq!(OutFile::ALL.len(), 3);
        assert!(OutFile::ALL.contains(&OutFile::FileList));
        assert!(OutFile::ALL.contains(&OutFile::ErrorLog));
    }

    /// ⚠️ 以前用过的文件名要**清掉**。
    ///
    /// 不清的话，文件夹里会同时躺着两份「看起来一样」的东西（旧名字空着、
    /// 新名字有内容），而那个空文件名会让人以为「这里写过什么」——
    /// 正是「保证无污染」要防的事。
    #[test]
    fn cleaning_removes_files_written_by_older_versions() {
        let (outbox, dir) = scratch("legacy");
        outbox.clean();
        for legacy in LEGACY_NAMES {
            std::fs::write(dir.join(legacy), "上一次留下的").unwrap();
        }

        outbox.clean();

        for legacy in LEGACY_NAMES {
            assert!(
                !dir.join(legacy).exists(),
                "{} 还在 —— 文件夹里会有两份看着一样的东西",
                legacy
            );
        }
        // 而**新**名字该在，而且是空的
        for file in OutFile::ALL {
            assert_eq!(std::fs::read_to_string(dir.join(file.name())).unwrap(), "");
        }
    }

    #[test]
    fn cleaning_creates_the_folder_and_the_empty_files() {
        let (outbox, dir) = scratch("create");

        outbox.clean();

        for file in OutFile::ALL {
            let path = dir.join(file.name());
            assert!(path.exists(), "{} 该被建出来", path.display());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        }
    }

    /// ⚠️ 清理是**清空内容**，不是删文件 —— 文件必须一直留着。
    #[test]
    fn cleaning_empties_the_content_but_keeps_the_files() {
        let (outbox, dir) = scratch("empties");
        outbox.clean();
        outbox.write(OutFile::FileList, "1 a.rs\n2 b.rs").unwrap();

        outbox.clean();

        let path = dir.join(OutFile::FileList.name());
        assert!(path.exists(), "文件被删掉了 —— 它该一直留着");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    /// 写进去的东西**一个字都不改**（用户要拿它去搜、去看）。
    #[test]
    fn writing_puts_the_text_in_verbatim() {
        let (outbox, dir) = scratch("verbatim");
        let text = "1 D:\\MyProjects\\a.rs\n2 *D:\\MyProjects\\b.rs";

        let written = outbox.write(OutFile::FileList, text).unwrap();

        assert_eq!(written, dir.join(OutFile::FileList.name()));
        assert_eq!(std::fs::read_to_string(&written).unwrap(), text);
    }

    /// 两个文件各管各的，互不覆盖。
    #[test]
    fn the_two_files_do_not_tread_on_each_other() {
        let (outbox, dir) = scratch("separate");

        outbox.write(OutFile::FileList, "文件列表").unwrap();
        outbox.write(OutFile::ErrorLog, "报错信息").unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join(OutFile::FileList.name())).unwrap(),
            "文件列表"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(OutFile::ErrorLog.name())).unwrap(),
            "报错信息"
        );
    }

    /// 用户自己把文件夹删了 → 下次要写时再长出来（那是他的自由）。
    #[test]
    fn writing_grows_the_folder_back_if_it_vanished() {
        let (outbox, dir) = scratch("regrow");
        outbox.clean();
        std::fs::remove_dir_all(&dir).unwrap();

        outbox.write(OutFile::ErrorLog, "x").unwrap();

        assert!(dir.join(OutFile::ErrorLog.name()).exists());
    }

    /// 「用不了」的输出文件夹：写会失败，但**不是崩溃**，清理也是安静的无操作。
    #[test]
    fn an_unavailable_outbox_fails_quietly() {
        let outbox = Outbox::unavailable();

        assert!(outbox.dir().is_none());
        assert!(outbox.path_of(OutFile::FileList).is_none());
        assert!(outbox.write(OutFile::FileList, "x").is_err());
        outbox.clean(); // 不该 panic
    }

    /// ⚠️ **清理不会动别的文件。**
    ///
    /// 删东西是这里唯一不可逆的操作，而它换不来什么：文件夹是我们自己的，
    /// 里面除了这几个文件本来也不该有别的东西。万一有（用户放进去的、
    /// 旧版本留下的），那也轮不到我们替他扔。
    #[test]
    fn cleaning_leaves_unrelated_files_alone() {
        let (outbox, dir) = scratch("untouched");
        outbox.clean();
        let stray = dir.join("用户自己放的东西.txt");
        std::fs::write(&stray, "别动我").unwrap();

        outbox.clean();

        assert_eq!(std::fs::read_to_string(&stray).unwrap(), "别动我");
    }

    #[test]
    fn the_env_variable_takes_over_the_location() {
        // SAFETY: 单线程地改一个进程级环境变量；这个测试不跟别的测试并发跑同一件事
        unsafe { std::env::set_var(FOLDER_PATH_ENV, "D:\\somewhere\\else") };
        let outbox = Outbox::locate();
        unsafe { std::env::remove_var(FOLDER_PATH_ENV) };

        // ⚠️ 文件名用 `OutFile::FileList.name()`，**不写死字面量** ——
        //    这条测的是**位置**，名字改了不该跟着红。（第一版把名字写死了，
        //    文件名一改这条就假失败。）
        assert_eq!(
            outbox.path_of(OutFile::FileList),
            Some(PathBuf::from("D:\\somewhere\\else").join(OutFile::FileList.name()))
        );
    }
}
