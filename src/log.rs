//! 把每条命令的执行情况写进 SQLite，供事后排查和迭代。
//! 写日志失败一律忽略：日志不能影响命令本身的结果。

use crate::browser;
use anyhow::Result;
use rusqlite::{Connection, params};
use std::time::Duration;

const SCHEMA: &str = "
    PRAGMA journal_mode = WAL;
    CREATE TABLE IF NOT EXISTS commands (
        id          INTEGER PRIMARY KEY,
        ts          TEXT    NOT NULL DEFAULT (datetime('now', 'localtime')),
        session     TEXT    NOT NULL,
        command     TEXT    NOT NULL,
        argv        TEXT    NOT NULL,
        ok          INTEGER NOT NULL,
        ms          INTEGER NOT NULL,
        error       TEXT,
        navigated   INTEGER,
        added_count INTEGER,
        new_tabs    INTEGER,
        run_id      TEXT,
        url         TEXT
    );
";

/// 老库补列。已经有这列时 ALTER 会报错，忽略即可。
/// ponytail: 每条命令都跑一遍这几条 ALTER，列多了再换 PRAGMA user_version 做版本号
const ADDED_COLUMNS: [&str; 5] = [
    "navigated INTEGER",
    "added_count INTEGER",
    "new_tabs INTEGER",
    "run_id TEXT",
    "url TEXT",
];

pub struct Record<'a> {
    pub session: &'a str,
    pub command: &'a str,
    pub argv: &'a [String],
    pub ok: bool,
    pub error: Option<&'a str>,
    pub elapsed: Duration,
    /// 命令返回的 JSON，用来取 changes 里的几个计数；页面正文不记
    pub output: Option<&'a serde_json::Value>,
}

/// WEBCTL_LOG=0 关闭记录，其余情况都记。
/// WEBCTL_RUN_ID 会记进 run_id 列，调用方用它把一批命令归到同一次任务。
pub fn record(entry: Record<'_>) {
    if std::env::var("WEBCTL_LOG").is_ok_and(|value| value == "0") {
        return;
    }
    let _ = write(entry);
}

fn write(entry: Record<'_>) -> Result<()> {
    let home = browser::data_home()?;
    std::fs::create_dir_all(&home)?;
    let db = Connection::open(home.join("webctl.db"))?;
    // 每条命令是独立进程，并发时让后来者等一会儿而不是直接报错
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute_batch(SCHEMA)?;
    for column in ADDED_COLUMNS {
        let _ = db.execute(&format!("ALTER TABLE commands ADD COLUMN {column}"), []);
    }
    let changes = entry.output.map(|output| &output["changes"]);
    // 只记命令自己返回的地址（open、back、reload、有页面变化的操作等），不为此多查一次浏览器
    let url = entry.output.and_then(|output| {
        output["url"]
            .as_str()
            .or_else(|| output["changes"]["url"].as_str())
    });
    db.execute(
        "INSERT INTO commands (session, command, argv, ok, ms, error, navigated, added_count, new_tabs, run_id, url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            entry.session,
            entry.command,
            serde_json::to_string(entry.argv)?,
            entry.ok,
            entry.elapsed.as_millis() as i64,
            entry.error,
            changes.and_then(|changes| changes["navigated"].as_bool()),
            changes.and_then(|changes| count(&changes["added"])),
            changes.and_then(|changes| count(&changes["new_tabs"])),
            std::env::var("WEBCTL_RUN_ID").ok(),
            url,
        ],
    )?;
    Ok(())
}

fn count(value: &serde_json::Value) -> Option<i64> {
    value.as_array().map(|items| items.len() as i64)
}
