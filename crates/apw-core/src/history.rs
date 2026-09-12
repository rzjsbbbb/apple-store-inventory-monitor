//! 命中历史。
//!
//! 活动日志是纯内存的、只留 300 行，关掉应用就没了。于是「昨天半夜到底补没
//! 补过货、几点补的」这个问题无法回答 —— 而它恰恰决定用户第二天该几点守着。
//! 这个模块把**有货**这一件事落到盘上，其余日志一概不碰。
//!
//! # 只记「目击」，不记每一轮
//!
//! [`crate::watcher`] 每轮确认有货都会发一次 `InStock`（那是刻意的，见
//! `watcher.rs` 里那段注释）。30 秒间隔下持续有货一小时就是 120 条事件，逐条
//! 落盘既撑爆文件也淹掉真正有用的信息：用户想知道的是「几点出现的货」，不是
//! 「它在那一小时里被确认了 120 次」。
//!
//! 所以这里按目标合并：同一个目标距上次记录不足 [`COALESCE_GAP_MS`] 的命中，
//! 算作同一次目击，直接丢弃。超过这个间隔才算一次新的出现。
//!
//! **合并状态只在内存里。** 应用重启后第一条命中一定会被记下来 —— 宁可多记
//! 一条，也不要因为读不到上次状态而漏掉一次真实的补货。
//!
//! # 为什么是 JSONL 而不是 JSON 数组
//!
//! 常态是「在末尾追加一行」，JSONL 下就是一次 `write`，不必读出整份、反序列化、
//! 再整份写回。而且单行损坏（断电写了半行）只影响那一行，读的时候跳过即可 ——
//! 换成 JSON 数组，末尾少一个方括号就是整份历史全丢。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// 配置目录名，与 [`crate::config`] 的一致。那边是私有常量，引用不到。
const APP_DIR: &str = "apple-store-inventory-monitor";

/// 历史文件名。
const HISTORY_FILE: &str = "hits.jsonl";

/// 同一目标两次记录之间的最小间隔，低于它视为同一次目击。
///
/// 15 分钟是按「补货」这件事本身的粒度定的：Apple 的库存在几分钟内反复横跳是
/// 常态，那属于同一次到货；隔了一刻钟再出现，对蹲守的人来说就是另一次机会了。
pub const COALESCE_GAP_MS: u64 = 15 * 60 * 1000;

/// 保留的最大记录条数。
///
/// 超出后丢掉最旧的。按一次目击一条、一天几十条算，两千条够用好几个月，
/// 而文件也就几百 KB —— 不设上限的话，一个常年挂着的实例迟早写出个几十 MB
/// 的文件，然后在某次启动读取时把界面卡住。
pub const MAX_RECORDS: usize = 2000;

/// 读取时的体积上限，防御一个被别的程序写坏的巨大文件。
///
/// 理由同 [`crate::config::MAX_SETTINGS_BYTES`]：整读一个几 GB 的文件会在任何
/// 错误处理之前就把内存吃光，用户连一句「历史读不出来」都看不到。
pub const MAX_HISTORY_BYTES: u64 = 8 << 20;

/// 历史读写过程中的失败。
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("找不到系统配置目录")]
    NoConfigDir,

    #[error("{action}失败（{}）：{source}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("历史文件超过 {limit} 字节（{}）", path.display())]
    TooLarge { path: PathBuf, limit: u64 },
}

impl HistoryError {
    fn io(action: &'static str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            action,
            path: path.into(),
            source,
        }
    }
}

/// 一次「某门店某型号有货」的目击记录。
///
/// 刻意只留业务字段，不留请求头、会话或完整响应 —— 这份文件用户可能会直接发到
/// issue 里，不该夹带任何会话痕迹。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hit {
    /// 目击时刻，Unix 毫秒。
    pub at_ms: u64,
    pub locale: String,
    pub store_number: String,
    pub store_title: String,
    pub part_number: String,
    pub product_name: String,
    /// Apple 返回的 `pickupDisplay` 原值，如 `available`。
    ///
    /// 留原值而不是留「有货」两个字：将来 Apple 新增取值时，这份历史仍然说得清
    /// 当时到底看到了什么。
    #[serde(default)]
    pub pickup_display: String,
}

impl Hit {
    /// 合并用的目标键，与 [`crate::watcher`] 里 `TargetKey` 的口径一致。
    fn key(&self) -> String {
        format!("{}|{}|{}", self.locale, self.store_number, self.part_number)
    }
}

/// 追加式的命中历史。
///
/// 全部方法收 `&self`：提醒线程在写、界面在读，两边同时发生。
#[derive(Debug)]
pub struct HitLog {
    path: PathBuf,
    /// 每个目标最近一次**已写入**的时刻，用于合并连续命中。
    last_written: Mutex<HashMap<String, u64>>,
}

impl HitLog {
    /// 指向系统用户配置目录下的历史文件。
    ///
    /// 只算路径，不碰磁盘 —— 与 [`crate::config::SettingsStore::new`] 同样的
    /// 约定：用户没命中过任何货，配置目录里就不该凭空多出一个空文件。
    pub fn new() -> Result<Self, HistoryError> {
        let dir = dirs::config_dir().ok_or(HistoryError::NoConfigDir)?;
        Ok(Self::at(dir.join(APP_DIR).join(HISTORY_FILE)))
    }

    /// 指向任意路径，供测试使用。
    pub fn at(path: PathBuf) -> Self {
        Self {
            path,
            last_written: Mutex::new(HashMap::new()),
        }
    }

    /// 历史文件的完整路径，便于在界面上告诉用户导出到哪了。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 记下一次命中。
    ///
    /// 返回 `Ok(false)` 表示这条被合并进了上一次目击，没有写盘 —— 那不是错误，
    /// 调用方不必提示用户。
    pub fn record(&self, hit: &Hit) -> Result<bool, HistoryError> {
        {
            let mut last = self.last_written.lock().unwrap_or_else(|e| e.into_inner());
            let key = hit.key();
            if let Some(previous) = last.get(&key)
                // 用 saturating_sub：系统时钟回拨时 at_ms 可能小于上次记录，
                // 直接相减会 panic。回拨时按「间隔为 0」处理，也就是合并掉 ——
                // 时钟不可信的时候，少记一条远好过写进一条时间倒流的记录。
                && hit.at_ms.saturating_sub(*previous) < COALESCE_GAP_MS
            {
                return Ok(false);
            }
            last.insert(key, hit.at_ms);
        }

        // 序列化失败在这里是不可能的（都是 String 和 u64），但也不值得 unwrap：
        // 真出了事，丢掉一条历史远好过让提醒线程崩掉。
        let Ok(line) = serde_json::to_string(hit) else {
            return Ok(false);
        };

        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| HistoryError::io("创建历史目录", dir, e))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| HistoryError::io("打开历史文件", &self.path, e))?;
        writeln!(file, "{line}").map_err(|e| HistoryError::io("写入历史文件", &self.path, e))?;
        drop(file);

        self.trim_if_needed()?;
        Ok(true)
    }

    /// 最近的若干条记录，最新的在前。
    ///
    /// 文件不存在返回空列表且不算错误：那只表示还没命中过任何一次。
    pub fn recent(&self, limit: usize) -> Result<Vec<Hit>, HistoryError> {
        let mut all = self.read_all()?;
        all.reverse();
        all.truncate(limit);
        Ok(all)
    }

    /// 全部记录，最旧的在前。
    pub fn read_all(&self) -> Result<Vec<Hit>, HistoryError> {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(HistoryError::io("打开历史文件", &self.path, e)),
        };

        let size = file
            .metadata()
            .map_err(|e| HistoryError::io("读取历史文件信息", &self.path, e))?
            .len();
        if size > MAX_HISTORY_BYTES {
            return Err(HistoryError::TooLarge {
                path: self.path.clone(),
                limit: MAX_HISTORY_BYTES,
            });
        }

        let mut out = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|e| HistoryError::io("读取历史文件", &self.path, e))?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // 坏行跳过而不是整份报错。断电时最后一行可能只写了一半，为这半行
            // 丢掉几个月的历史是说不过去的。
            if let Ok(hit) = serde_json::from_str::<Hit>(line) {
                out.push(hit);
            }
        }
        Ok(out)
    }

    /// 清空历史。文件不存在也算成功。
    pub fn clear(&self) -> Result<(), HistoryError> {
        self.last_written
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(HistoryError::io("删除历史文件", &self.path, e)),
        }
    }

    /// 导出成 CSV 文本，最新的在前。
    ///
    /// 时间列同时给出 Unix 毫秒和一个人类可读的 UTC 时间：前者便于再加工，
    /// 后者便于直接看。刻意不做本地时区换算 —— 这个 crate 没有时区库，猜一个
    /// 偏移量写进文件比给出 UTC 更容易误导人。
    pub fn to_csv(&self) -> Result<String, HistoryError> {
        let mut out = String::from(
            "atMs,atUtc,locale,storeNumber,storeTitle,partNumber,productName,pickupDisplay\n",
        );
        for hit in self.recent(MAX_RECORDS)? {
            let cells = [
                hit.at_ms.to_string(),
                format_utc(hit.at_ms),
                hit.locale,
                hit.store_number,
                hit.store_title,
                hit.part_number,
                hit.product_name,
                hit.pickup_display,
            ];
            out.push_str(&cells.map(|c| csv_cell(&c)).join(","));
            out.push('\n');
        }
        Ok(out)
    }

    /// 超过 [`MAX_RECORDS`] 时丢掉最旧的记录。
    ///
    /// 整份读出再写回，只在超限时发生 —— 常态那条路仍然是「追加一行」。
    fn trim_if_needed(&self) -> Result<(), HistoryError> {
        let all = self.read_all()?;
        if all.len() <= MAX_RECORDS {
            return Ok(());
        }

        let keep = &all[all.len() - MAX_RECORDS..];
        let mut text = String::new();
        for hit in keep {
            if let Ok(line) = serde_json::to_string(hit) {
                text.push_str(&line);
                text.push('\n');
            }
        }

        // 先写临时文件再 rename，理由同 config.rs：中途失败时原文件仍然完好，
        // 而不是留下一份被截断的历史。
        let tmp = self.path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, text).map_err(|e| HistoryError::io("写入临时历史文件", &tmp, e))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| HistoryError::io("替换历史文件", &self.path, e))?;
        Ok(())
    }
}

/// 按 RFC 4180 转义一个 CSV 单元格。
///
/// 门店名里有逗号（`Illinois-Orland Square Mall, Chicago`），型号名里有引号和
/// 中文顿号，不转义的话导出的表格会整列错位。
fn csv_cell(raw: &str) -> String {
    // 换行也要包起来，否则一条记录会被 Excel 拆成两行。
    if raw.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", raw.replace('"', "\"\""))
    } else {
        raw.to_string()
    }
}

/// 把 Unix 毫秒格式化成 `YYYY-MM-DD HH:MM:SS UTC`。
///
/// 自己算而不是引入 chrono：这个 crate 目前没有日期依赖，为一列导出文本加一个
/// 时间库不划算。公历换算用的是标准的 civil-from-days 算法。
fn format_utc(at_ms: u64) -> String {
    let secs = at_ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant 的 civil_from_days：把「1970-01-01 起的天数」还原成年月日。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("apw-history-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p.push("hits.jsonl");
        p
    }

    fn hit(at_ms: u64, store: &str, part: &str) -> Hit {
        Hit {
            at_ms,
            locale: "zh_CN".into(),
            store_number: store.into(),
            store_title: "上海-环球港".into(),
            part_number: part.into(),
            product_name: "iPhone 17 512GB 黑色".into(),
            pickup_display: "available".into(),
        }
    }

    #[test]
    fn 记录与读回() {
        let log = HitLog::at(temp_path("basic"));
        assert!(log.record(&hit(1_000, "R683", "A/A")).unwrap());
        assert!(log.record(&hit(2_000, "R390", "A/A")).unwrap());

        let all = log.recent(10).unwrap();
        assert_eq!(all.len(), 2);
        // 最新的在前。
        assert_eq!(all[0].store_number, "R390");
        assert_eq!(all[1].store_number, "R683");
        assert_eq!(all[0].pickup_display, "available");
    }

    #[test]
    fn 同一目标的连续命中会被合并() {
        // 这是这个模块存在的主要理由：每轮都记的话，持续有货一小时就是 120 条。
        let log = HitLog::at(temp_path("coalesce"));
        assert!(log.record(&hit(0, "R683", "A/A")).unwrap());
        for i in 1..=20 {
            assert!(
                !log.record(&hit(i * 30_000, "R683", "A/A")).unwrap(),
                "第 {i} 次应当被合并"
            );
        }
        assert_eq!(log.recent(50).unwrap().len(), 1);

        // 超过合并窗口之后才算一次新的目击。
        assert!(log.record(&hit(COALESCE_GAP_MS, "R683", "A/A")).unwrap());
        assert_eq!(log.recent(50).unwrap().len(), 2);
    }

    #[test]
    fn 不同目标互不影响() {
        let log = HitLog::at(temp_path("independent"));
        assert!(log.record(&hit(0, "R683", "A/A")).unwrap());
        // 同门店不同型号、同型号不同门店，都是各自独立的目标。
        assert!(log.record(&hit(1_000, "R683", "B/A")).unwrap());
        assert!(log.record(&hit(1_000, "R390", "A/A")).unwrap());
        assert_eq!(log.recent(50).unwrap().len(), 3);
    }

    #[test]
    fn 时钟回拨不会崩也不会写进倒流的记录() {
        let log = HitLog::at(temp_path("clock"));
        assert!(log.record(&hit(10 * 60 * 1000, "R683", "A/A")).unwrap());
        // at_ms 比上一条还小：saturating_sub 之后是 0，按合并处理。
        assert!(!log.record(&hit(0, "R683", "A/A")).unwrap());
        assert_eq!(log.recent(50).unwrap().len(), 1);
    }

    #[test]
    fn 重启后第一条一定会被记下() {
        // 合并状态只在内存里。宁可多记一条，也不要因为读不到上次状态而漏掉
        // 一次真实的补货。
        let path = temp_path("restart");
        let first = HitLog::at(path.clone());
        assert!(first.record(&hit(0, "R683", "A/A")).unwrap());

        let second = HitLog::at(path);
        assert!(second.record(&hit(1_000, "R683", "A/A")).unwrap());
        assert_eq!(second.recent(50).unwrap().len(), 2);
    }

    #[test]
    fn 超过上限后丢掉最旧的() {
        let log = HitLog::at(temp_path("trim"));
        // 每条用不同的目标键，避免被合并。
        for i in 0..(MAX_RECORDS + 10) {
            assert!(
                log.record(&hit(i as u64 * 1_000, "R683", &format!("P{i}/A")))
                    .unwrap()
            );
        }
        let all = log.read_all().unwrap();
        assert_eq!(all.len(), MAX_RECORDS);
        // 留下的是最新的那一批。
        assert_eq!(all[0].part_number, format!("P{}/A", 10));
        assert_eq!(
            all[all.len() - 1].part_number,
            format!("P{}/A", MAX_RECORDS + 9)
        );
    }

    #[test]
    fn 坏行只丢自己不影响整份历史() {
        let path = temp_path("corrupt");
        let log = HitLog::at(path.clone());
        log.record(&hit(1_000, "R683", "A/A")).unwrap();
        // 模拟断电写了半行。
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{{\"atMs\":123,\"loc").unwrap();
        drop(f);
        let log2 = HitLog::at(path);
        log2.record(&hit(2_000, "R390", "A/A")).unwrap();

        let all = log2.read_all().unwrap();
        assert_eq!(all.len(), 2, "坏行应当被跳过，前后两条都要在");
    }

    #[test]
    fn 文件不存在时读出空列表而不是报错() {
        let log = HitLog::at(temp_path("missing"));
        assert!(log.recent(10).unwrap().is_empty());
        assert!(log.read_all().unwrap().is_empty());
        // 只读不该凭空创建文件。
        assert!(!log.path().exists());
    }

    #[test]
    fn 清空之后可以重新记录() {
        let log = HitLog::at(temp_path("clear"));
        log.record(&hit(0, "R683", "A/A")).unwrap();
        log.clear().unwrap();
        assert!(log.recent(10).unwrap().is_empty());
        // 合并状态也要一起清掉，否则刚清空又记不进去。
        assert!(log.record(&hit(1_000, "R683", "A/A")).unwrap());
        // 清空一个不存在的文件不算错。
        log.clear().unwrap();
        log.clear().unwrap();
    }

    #[test]
    fn csv_转义逗号引号与换行() {
        assert_eq!(csv_cell("普通"), "普通");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("说\"这个\""), "\"说\"\"这个\"\"\"");
        assert_eq!(csv_cell("上\n下"), "\"上\n下\"");
    }

    #[test]
    fn csv_有表头且最新在前() {
        let log = HitLog::at(temp_path("csv"));
        log.record(&hit(0, "R683", "A/A")).unwrap();
        log.record(&hit(1_000, "R390", "B/A")).unwrap();

        let csv = log.to_csv().unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        assert!(lines[0].starts_with("atMs,atUtc,"));
        assert_eq!(lines.len(), 3);
        assert!(lines[1].contains("R390"), "最新的应当在前：{}", lines[1]);
        assert!(lines[2].contains("R683"));
    }

    #[test]
    fn utc_时间格式化() {
        // 期望值由 Python 的 datetime 独立算出，不是照着实现反推的。
        for (ms, want) in [
            (0u64, "1970-01-01 00:00:00 UTC"),
            (1_789_670_647_000, "2026-09-17 18:44:07 UTC"),
            // 闰年 2 月 29 日必须存在。
            (1_709_164_800_000, "2024-02-29 00:00:00 UTC"),
            // 跨年边界。
            (1_704_067_199_000, "2023-12-31 23:59:59 UTC"),
            // 2000 能被 400 整除，是闰年。
            (951_825_600_000, "2000-02-29 12:00:00 UTC"),
            // 2100 能被 100 整除但不能被 400 整除，**不是**闰年 —— 这一条能
            // 抓住把闰年规则简化成「四年一闰」的实现。
            (4_107_542_400_000, "2100-03-01 00:00:00 UTC"),
        ] {
            assert_eq!(format_utc(ms), want, "{ms} 的换算不对");
        }
    }
}
