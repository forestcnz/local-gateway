//! SQLite 存储层：单一 db 文件（与 exe 同级）承载全部数据。
//! - `requests` 请求日志
//! - `providers` 供应商
//! - `settings` 设置（键值对，缺失时使用代码内默认值）
use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Local, NaiveDate};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS requests (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    ts         TEXT    NOT NULL,
    ts_epoch   INTEGER NOT NULL,
    protocol   TEXT    NOT NULL,
    path       TEXT    NOT NULL,
    model      TEXT    NOT NULL,
    provider   TEXT    NOT NULL DEFAULT '',
    status     INTEGER NOT NULL,
    latency_ms INTEGER NOT NULL DEFAULT 0,
    stream     INTEGER NOT NULL DEFAULT 0,
    attempts   TEXT    NOT NULL DEFAULT '[]',
    error      TEXT
);
CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts_epoch);
CREATE TABLE IF NOT EXISTS providers (
    sort_order   INTEGER NOT NULL,
    name         TEXT PRIMARY KEY,
    protocols    TEXT NOT NULL,
    base_url     TEXT NOT NULL,
    api_key      TEXT NOT NULL DEFAULT '',
    models       TEXT NOT NULL DEFAULT '[]',
    timeout_secs INTEGER NOT NULL DEFAULT 120,
    enabled      INTEGER NOT NULL DEFAULT 1,
    extra_headers TEXT NOT NULL DEFAULT '{}'
);
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS aliases (
    alias TEXT PRIMARY KEY,
    model TEXT NOT NULL
);
";

/// 旧库升级：为 requests 表补充 token 列（已存在则忽略）。
fn migrate_token_columns(conn: &Connection) {
    for col in ["tokens_in", "tokens_out", "tokens_cached"] {
        let sql = format!("ALTER TABLE requests ADD COLUMN {col} INTEGER NOT NULL DEFAULT 0");
        if let Err(e) = conn.execute(&sql, []) {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") {
                tracing::warn!("迁移 {col} 列失败: {msg}");
            }
        }
    }
}

/// 一条请求日志（SQLite 存储）。
/// 对流式请求，latency 记录的是「到上游响应头」的耗时（近似首字延迟）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReqLog {
    pub ts: DateTime<Local>,
    pub protocol: String,
    pub path: String,
    pub model: String,
    /// 最终使用的供应商；全部失败时为空串。
    #[serde(default)]
    pub provider: String,
    pub status: u16,
    pub latency_ms: u64,
    #[serde(default)]
    pub stream: bool,
    /// 每次尝试的轨迹，如 ["relay-a:429", "openai:200"]
    #[serde(default)]
    pub attempts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Token 用量：输入 / 输出 / 命中缓存（从上游 usage 解析，缺失为 0）
    #[serde(default)]
    pub tokens_in: u64,
    #[serde(default)]
    pub tokens_out: u64,
    #[serde(default)]
    pub tokens_cached: u64,
}

#[derive(Debug, Clone)]
pub struct LogFilter {
    pub protocol: Option<String>,
    pub provider: Option<String>,
    /// 状态类别（百位）：2 → 2xx，4 → 4xx，5 → 5xx
    pub status_class: Option<u32>,
    /// 仅返回 ts_epoch >= since_epoch 的记录
    pub since_epoch: Option<i64>,
    /// 对 model / path / provider 的子串模糊匹配
    pub q: Option<String>,
    pub limit: usize,
}

impl Default for LogFilter {
    fn default() -> Self {
        Self {
            protocol: None,
            provider: None,
            status_class: None,
            since_epoch: None,
            q: None,
            limit: 100,
        }
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct DayStats {
    pub requests: u64,
    pub success: u64,
    pub latency_sum_ms: u64,
}

impl DayStats {
    pub fn success_rate(&self) -> f64 {
        if self.requests == 0 {
            100.0
        } else {
            (self.success as f64 / self.requests as f64) * 100.0
        }
    }
    pub fn avg_latency_ms(&self) -> u64 {
        if self.requests == 0 {
            0
        } else {
            self.latency_sum_ms / self.requests
        }
    }
}

/// 本地时区某天的 [start_epoch, end_epoch)；无法解析时返回 (0,0)。
fn day_range(day: NaiveDate) -> (i64, i64) {
    let Some(start) = day.and_hms_opt(0, 0, 0) else { return (0, 0) };
    let Some(start_ts) = start.and_local_timezone(Local).single() else { return (0, 0) };
    (start_ts.timestamp(), start_ts.timestamp() + 86_400)
}

/// 本地时区某天 0 点的 epoch 秒。
pub fn day_start_epoch(day: NaiveDate) -> i64 {
    day_range(day).0
}

fn row_to_log(row: &rusqlite::Row) -> rusqlite::Result<ReqLog> {
    let ts_str: String = row.get(0)?;
    let ts = DateTime::parse_from_rfc3339(&ts_str)
        .map(|d| d.with_timezone(&Local))
        .unwrap_or_else(|_| Local::now());
    let attempts_str: String = row.get(9)?;
    Ok(ReqLog {
        ts,
        protocol: row.get(2)?,
        path: row.get(3)?,
        model: row.get(4)?,
        provider: row.get(5)?,
        status: row.get::<_, i64>(6)? as u16,
        latency_ms: row.get::<_, i64>(7)? as u64,
        stream: row.get::<_, i64>(8)? != 0,
        attempts: serde_json::from_str(&attempts_str).unwrap_or_default(),
        error: row.get(10)?,
        tokens_in: row.get::<_, i64>(11).unwrap_or(0) as u64,
        tokens_out: row.get::<_, i64>(12).unwrap_or(0) as u64,
        tokens_cached: row.get::<_, i64>(13).unwrap_or(0) as u64,
    })
}

/// SQLite 句柄。单写者模型：进程内所有读写共用一个连接（WAL 模式）。
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        migrate_token_columns(&conn);
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // ---------- 请求日志 ----------

    pub fn insert_log(&self, e: &ReqLog) {
        let Ok(conn) = self.conn.lock() else { return };
        let attempts = serde_json::to_string(&e.attempts).unwrap_or_else(|_| "[]".into());
        let res = conn.execute(
            "INSERT INTO requests
                (ts, ts_epoch, protocol, path, model, provider, status, latency_ms, stream, attempts, error,
                 tokens_in, tokens_out, tokens_cached)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            rusqlite::params![
                e.ts.to_rfc3339(),
                e.ts.timestamp(),
                e.protocol,
                e.path,
                e.model,
                e.provider,
                e.status as i64,
                e.latency_ms as i64,
                e.stream as i64,
                attempts,
                e.error,
                e.tokens_in as i64,
                e.tokens_out as i64,
                e.tokens_cached as i64,
            ],
        );
        if let Err(err) = res {
            tracing::warn!("写请求日志失败: {err}");
        }
    }

    /// 按条件查询日志（新→旧）。None/空串表示不过滤该维度。
    pub fn query_logs(&self, f: &LogFilter) -> Vec<ReqLog> {
        let Ok(conn) = self.conn.lock() else { return vec![] };
        let mut sql = String::from(
            "SELECT ts, ts_epoch, protocol, path, model, provider, status, latency_ms, stream, attempts, error, \
                    tokens_in, tokens_out, tokens_cached \
             FROM requests WHERE 1=1",
        );
        let mut vals: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(p) = f.protocol.as_deref().filter(|s| !s.is_empty()) {
            sql.push_str(" AND protocol = ?");
            vals.push(p.to_string().into());
        }
        if let Some(pr) = f.provider.as_deref().filter(|s| !s.is_empty()) {
            sql.push_str(" AND provider = ?");
            vals.push(pr.to_string().into());
        }
        if let Some(c) = f.status_class {
            sql.push_str(&format!(
                " AND status BETWEEN {} AND {}",
                c * 100,
                c * 100 + 99
            ));
        }
        if let Some(ts) = f.since_epoch {
            sql.push_str(" AND ts_epoch >= ?");
            vals.push(ts.into());
        }
        if let Some(q) = f.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            // 去掉 LIKE 通配符，按普通子串匹配（SQLite LIKE 对 ASCII 不区分大小写）
            let like = format!("%{}%", q.replace(['%', '_'], ""));
            sql.push_str(" AND (model LIKE ? OR path LIKE ? OR provider LIKE ?)");
            for _ in 0..3 {
                vals.push(like.clone().into());
            }
        }
        sql.push_str(" ORDER BY id DESC LIMIT ?");
        vals.push((f.limit as i64).into());

        let Ok(mut stmt) = conn.prepare(&sql) else { return vec![] };
        match stmt.query_map(rusqlite::params_from_iter(vals.iter()), row_to_log) {
            Ok(it) => it.filter_map(Result::ok).collect(),
            Err(_) => vec![],
        }
    }

    pub fn total_logs(&self) -> u64 {
        let Ok(conn) = self.conn.lock() else { return 0 };
        conn.query_row("SELECT COUNT(*) FROM requests", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as u64
    }

    /// 统计某一天的请求数据（本地时区）。
    pub fn day_stats(&self, day: NaiveDate) -> DayStats {
        let (start_ts, end_ts) = day_range(day);
        if start_ts == end_ts {
            return DayStats::default();
        }
        let Ok(conn) = self.conn.lock() else { return DayStats::default() };
        let Ok(mut stmt) = conn.prepare(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE WHEN status < 400 THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(latency_ms), 0)
             FROM requests WHERE ts_epoch >= ?1 AND ts_epoch < ?2",
        ) else {
            return DayStats::default();
        };
        stmt.query_row(
            rusqlite::params![start_ts, end_ts],
            |r| {
                Ok(DayStats {
                    requests: r.get::<_, i64>(0)? as u64,
                    success: r.get::<_, i64>(1)? as u64,
                    latency_sum_ms: r.get::<_, i64>(2)? as u64,
                })
            },
        )
        .unwrap_or_default()
    }

    /// 最近 24 小时（滚动窗口，本地时区）按小时的请求量。
    /// 返回 24 桶：index 0 = 24 小时前的那一小时，index 23 = 当前进行中的小时。
    pub fn hourly_counts(&self, now: DateTime<Local>) -> Vec<u64> {
        let end = now.timestamp();
        let start = end - 86_400;
        let mut out = vec![0u64; 24];
        let Ok(conn) = self.conn.lock() else { return out };
        let Ok(mut stmt) =
            conn.prepare("SELECT ts_epoch FROM requests WHERE ts_epoch >= ?1 AND ts_epoch <= ?2")
        else {
            return out;
        };
        let rows = stmt.query_map(rusqlite::params![start, end], |r| r.get::<_, i64>(0));
        if let Ok(it) = rows {
            for ts in it.filter_map(Result::ok) {
                let b = ((ts - start) / 3600).clamp(0, 23) as usize;
                out[b] += 1;
            }
        }
        out
    }

    /// 清空请求日志。
    pub fn clear_logs(&self) -> rusqlite::Result<usize> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM requests", [])
    }

    /// 保留策略：删除超过 keep_days 天的日志，返回删除条数。
    pub fn compact_logs(&self, keep_days: u32) -> usize {
        let cutoff = (Local::now() - Duration::days(keep_days as i64)).timestamp();
        let Ok(conn) = self.conn.lock() else { return 0 };
        conn.execute("DELETE FROM requests WHERE ts_epoch < ?1", rusqlite::params![cutoff])
            .unwrap_or(0)
    }

    // ---------- 设置（键值） ----------

    pub fn settings_all(&self) -> Vec<(String, String)> {
        let Ok(conn) = self.conn.lock() else { return vec![] };
        let Ok(mut stmt) = conn.prepare("SELECT key, value FROM settings") else {
            return vec![];
        };
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)));
        match rows {
            Ok(it) => it.filter_map(Result::ok).collect(),
            Err(_) => vec![],
        }
    }

    pub fn settings_set(&self, key: &str, value: &str) {
        let Ok(conn) = self.conn.lock() else { return };
        if let Err(e) = conn.execute(
            "INSERT INTO settings(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        ) {
            tracing::warn!("写设置 {key} 失败: {e}");
        }
    }

    // ---------- 模型别名（全局，与供应商无关） ----------

    /// 加载全部别名（别名 → 真实模型名）。
    pub fn aliases_load(&self) -> Vec<(String, String)> {
        let Ok(conn) = self.conn.lock() else { return vec![] };
        let Ok(mut stmt) = conn.prepare("SELECT alias, model FROM aliases ORDER BY alias") else {
            return vec![];
        };
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)));
        match rows {
            Ok(it) => it.filter_map(Result::ok).collect(),
            Err(_) => vec![],
        }
    }

    /// 整表重建别名。
    pub fn aliases_replace(&self, rows: &[(String, String)]) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM aliases", [])?;
        for (alias, model) in rows {
            conn.execute(
                "INSERT INTO aliases(alias, model) VALUES(?1, ?2)",
                rusqlite::params![alias, model],
            )?;
        }
        Ok(())
    }

    // ---------- 供应商 ----------

    /// 按保存顺序（sort_order）加载全部供应商行。
    pub fn providers_load(&self) -> rusqlite::Result<Vec<crate::config::ProviderRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT sort_order, name, protocols, base_url, api_key, models, timeout_secs, enabled, extra_headers
             FROM providers ORDER BY sort_order, rowid",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::config::ProviderRow {
                sort_order: r.get(0)?,
                name: r.get(1)?,
                protocols: r.get(2)?,
                base_url: r.get(3)?,
                api_key: r.get(4)?,
                models: r.get(5)?,
                timeout_secs: r.get(6)?,
                enabled: r.get::<_, i64>(7)? != 0,
                extra_headers: r.get(8)?,
            })
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// 整表重建供应商（保留调用方给定的顺序）。
    pub fn providers_replace(&self, rows: &[crate::config::ProviderRow]) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM providers", [])?;
        for r in rows {
            conn.execute(
                "INSERT INTO providers
                    (sort_order, name, protocols, base_url, api_key, models, timeout_secs, enabled, extra_headers)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    r.sort_order,
                    r.name,
                    r.protocols,
                    r.base_url,
                    r.api_key,
                    r.models,
                    r.timeout_secs,
                    r.enabled as i64,
                    r.extra_headers,
                ],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(ts: DateTime<Local>, status: u16) -> ReqLog {
        ReqLog {
            ts,
            protocol: "chat".into(),
            path: "/v1/chat/completions".into(),
            model: "gpt-4o".into(),
            provider: "openai".into(),
            status,
            latency_ms: 100,
            stream: false,
            attempts: vec![],
            error: None,
            tokens_in: 10,
            tokens_out: 5,
            tokens_cached: 2,
        }
    }

    #[test]
    fn logs_query_tokens_hourly_compact() {
        let dir = std::env::temp_dir().join(format!("lg-db-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("gateway.db");
        let _ = std::fs::remove_file(&db_path);
        let store = Db::open(&db_path).unwrap();

        // 旧行（10 天前）+ 最近 5 小时各一条，按时间顺序追加
        let mut old = entry(Local::now() - Duration::days(10), 401);
        old.protocol = "anthropic".into();
        old.model = "claude-3".into();
        store.insert_log(&old);
        for i in (0..5).rev() {
            store.insert_log(&entry(Local::now() - Duration::hours(i), 200));
        }

        // 多条件查询
        assert_eq!(store.total_logs(), 6);
        let n = |f: LogFilter| store.query_logs(&f).len();
        assert_eq!(n(LogFilter { protocol: Some("anthropic".into()), ..Default::default() }), 1);
        assert_eq!(n(LogFilter { status_class: Some(4), ..Default::default() }), 1);
        assert_eq!(n(LogFilter { q: Some("gpt".into()), ..Default::default() }), 5);
        assert_eq!(
            n(LogFilter { protocol: Some("anthropic".into()), q: Some("gpt".into()), ..Default::default() }),
            0
        );

        // token 列随行返回
        let all = store.query_logs(&LogFilter { limit: 10, ..Default::default() });
        assert!(all.iter().all(|l| (l.tokens_in, l.tokens_out, l.tokens_cached) == (10, 5, 2)));

        // 滚动 24 小时：旧行不在窗口内；now-1h 与 now 同在最后一桶
        let hourly = store.hourly_counts(Local::now());
        eprintln!("HOURLY: {:?}", hourly);
        assert_eq!(hourly.len(), 24);
        let sum: u64 = hourly.iter().sum();
        assert_eq!(sum, 5);
        assert_eq!(hourly[20], 1);
        assert_eq!(hourly[21], 1);
        assert_eq!(hourly[22], 1);
        assert_eq!(hourly[23], 2);

        // 保留清理：删掉 10 天前的旧行
        assert_eq!(store.compact_logs(7), 1);
        assert_eq!(store.total_logs(), 5);

        store.clear_logs().unwrap();
        assert_eq!(store.total_logs(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }
}
