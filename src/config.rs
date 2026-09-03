//! 网关配置：全部持久化在 SQLite 中（providers / settings 表）。
//! `Config` 是运行时缓存的内存视图；修改统一走
//! `state::AppState::update_config`（校验 → 写库 → 原子替换缓存）。
use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::db::Db;

/// 三种支持的协议。网关只透传，不做协议转换：
/// 入站是什么协议，出站就用同一协议调用上游。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Chat,
    Responses,
    Anthropic,
}

impl Protocol {
    /// 协议对应的网关入口路径（同时也是转发上游时使用的路径）。
    pub fn path(self) -> &'static str {
        match self {
            Protocol::Chat => "/v1/chat/completions",
            Protocol::Responses => "/v1/responses",
            Protocol::Anthropic => "/v1/messages",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Chat => "chat",
            Protocol::Responses => "responses",
            Protocol::Anthropic => "anthropic",
        }
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 供应商：一个可转发的上游服务。
/// `protocols` 声明它支持哪些协议报文；`models` 登记它能服务的模型名
/// （使用方请求 model 直接写模型名，网关据此自动选址与故障转移）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Provider {
    pub name: String,
    #[serde(default)]
    pub protocols: Vec<Protocol>,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default = "d_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// 追加到转发请求上的自定义头（例如 Azure 的 api-key）。
    #[serde(default)]
    pub extra_headers: BTreeMap<String, String>,
}

fn d_true() -> bool {
    true
}
fn d_timeout() -> u64 {
    120
}

/// 扁平化的运行时配置（从 DB 读取，保存时写回 DB）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub listen: String,
    pub upstream_timeout_secs: u64,
    pub sse_passthrough: bool,
    pub max_body_mb: u64,
    pub cors: bool,
    pub retention_days: u32,
    /// 转发前从请求体剥离的参数名（逗号分隔），用于兼容不支持这些参数的上游
    pub drop_params: String,
    /// 模型别名（全局，最外层）：请求模型命中别名时先转换为真实模型再选址。
    /// 键为客户端使用的名称，值为转发给上游的模型名。与供应商无关。
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub providers: Vec<Provider>,
}

impl Config {
    /// 出厂默认：仅初始化设置项，供应商列表为空，由使用方在控制台添加。
    pub fn default_config() -> Self {
        Self {
            listen: "127.0.0.1:8787".into(),
            upstream_timeout_secs: 120,
            sse_passthrough: true,
            max_body_mb: 20,
            cors: false,
            retention_days: 7,
            drop_params: "reasoning_effort".into(),
            aliases: BTreeMap::new(),
            providers: Vec::new(),
        }
    }

    pub fn find_provider(&self, name: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.name == name)
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for p in &self.providers {
            if p.name.is_empty() {
                return Err("存在 name 为空的供应商".into());
            }
            if !seen.insert(p.name.clone()) {
                return Err(format!("供应商名称重复: {}", p.name));
            }
            if p.base_url.is_empty() {
                return Err(format!("供应商 {} 缺少 base_url", p.name));
            }
            if p.protocols.is_empty() {
                return Err(format!("供应商 {} 未勾选任何协议", p.name));
            }
        }
        if self.listen.is_empty() || !self.listen.contains(':') {
            return Err("listen 需为 host:port 形式".into());
        }
        if self.retention_days == 0 {
            return Err("retention_days 至少为 1".into());
        }
        Ok(())
    }

    /// 从 DB 读取配置；缺失的设置项使用默认值。
    pub fn from_db(db: &Db) -> Self {
        let s: HashMap<String, String> = db.settings_all().into_iter().collect();
        let get = |k: &str, d: &str| s.get(k).cloned().unwrap_or_else(|| d.to_string());
        let parse_bool = |k: &str, d: bool| get(k, if d { "true" } else { "false" }).parse().unwrap_or(d);
        Self {
            listen: get("listen", "127.0.0.1:8787"),
            upstream_timeout_secs: get("upstream_timeout_secs", "120").parse().unwrap_or(120),
            sse_passthrough: parse_bool("sse_passthrough", true),
            max_body_mb: get("max_body_mb", "20").parse().unwrap_or(20),
            cors: parse_bool("cors", false),
            retention_days: get("retention_days", "7").parse().unwrap_or(7),
            drop_params: get("drop_params", "reasoning_effort"),
            aliases: db.aliases_load().into_iter().collect(),
            providers: db
                .providers_load()
                .unwrap_or_default()
                .into_iter()
                .map(|row| row.into_provider())
                .collect(),
        }
    }

    /// 把整份配置写回 DB（设置逐项 upsert，供应商/别名整表重建）。
    pub fn save_to_db(&self, db: &Db) {
        db.settings_set("listen", &self.listen);
        db.settings_set("upstream_timeout_secs", &self.upstream_timeout_secs.to_string());
        db.settings_set("sse_passthrough", &self.sse_passthrough.to_string());
        db.settings_set("max_body_mb", &self.max_body_mb.to_string());
        db.settings_set("cors", &self.cors.to_string());
        db.settings_set("retention_days", &self.retention_days.to_string());
        db.settings_set("drop_params", &self.drop_params);
        let alias_rows: Vec<(String, String)> = self
            .aliases
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if let Err(e) = db.aliases_replace(&alias_rows) {
            tracing::error!("保存模型别名失败: {e}");
        }
        let rows: Vec<ProviderRow> = self
            .providers
            .iter()
            .enumerate()
            .map(|(i, p)| ProviderRow::from_provider(i as i64, p))
            .collect();
        if let Err(e) = db.providers_replace(&rows) {
            tracing::error!("保存供应商失败: {e}");
        }
    }

    /// 首次运行初始化：settings 表为空时写入默认值与示例供应商。
    /// 返回 true 表示执行了初始化。
    pub fn ensure_defaults(db: &Db) -> bool {
        if !db.settings_all().is_empty() {
            return false;
        }
        Self::default_config().save_to_db(db);
        true
    }
}

// ---------- DB 行映射 ----------

/// providers 表的一行；JSON 列由 Provider 转换。
#[derive(Debug, Clone)]
pub struct ProviderRow {
    pub sort_order: i64,
    pub name: String,
    pub protocols: String,
    pub base_url: String,
    pub api_key: String,
    pub models: String,
    pub timeout_secs: i64,
    pub enabled: bool,
    pub extra_headers: String,
}

impl ProviderRow {
    fn from_provider(sort_order: i64, p: &Provider) -> Self {
        Self {
            sort_order,
            name: p.name.clone(),
            protocols: serde_json::to_string(&p.protocols).unwrap_or_else(|_| "[]".into()),
            base_url: p.base_url.clone(),
            api_key: p.api_key.clone(),
            models: serde_json::to_string(&p.models).unwrap_or_else(|_| "[]".into()),
            timeout_secs: p.timeout_secs as i64,
            enabled: p.enabled,
            extra_headers: serde_json::to_string(&p.extra_headers)
                .unwrap_or_else(|_| "{}".into()),
        }
    }

    fn into_provider(self) -> Provider {
        Provider {
            name: self.name,
            protocols: serde_json::from_str(&self.protocols).unwrap_or_default(),
            base_url: self.base_url,
            api_key: self.api_key,
            models: serde_json::from_str(&self.models).unwrap_or_default(),
            timeout_secs: self.timeout_secs as u64,
            enabled: self.enabled,
            extra_headers: serde_json::from_str(&self.extra_headers).unwrap_or_default(),
        }
    }
}

// ---------- 转发地址工具 ----------

/// 归一化 base_url：去尾部 `/`，去尾部 `/v1`（转发时统一补回完整路径）。
pub fn normalize_base(base: &str) -> String {
    let mut b = base.trim().trim_end_matches('/').to_string();
    if b.ends_with("/v1") {
        b.truncate(b.len() - 3);
    }
    b
}

pub fn upstream_url(provider: &Provider, protocol: Protocol) -> String {
    format!("{}{}", normalize_base(&provider.base_url), protocol.path())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_providers() -> Vec<Provider> {
        vec![
            Provider {
                name: "openai".into(),
                protocols: vec![Protocol::Chat, Protocol::Responses],
                base_url: "https://api.openai.com".into(),
                api_key: "sk-x".into(),
                models: vec!["gpt-4o".into()],
                timeout_secs: 60,
                enabled: true,
                extra_headers: BTreeMap::new(),
            },
            Provider {
                name: "anthropic".into(),
                protocols: vec![Protocol::Anthropic],
                base_url: "https://api.anthropic.com".into(),
                api_key: "sk-ant-x".into(),
                models: vec!["claude-3".into()],
                timeout_secs: 120,
                enabled: true,
                extra_headers: BTreeMap::new(),
            },
        ]
    }

    fn temp_db(tag: &str) -> (Db, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("lg-cfg-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("gateway.db")).unwrap();
        (db, dir)
    }

    #[test]
    fn settings_and_providers_roundtrip_via_db() {
        let (db, dir) = temp_db("rt");
        let mut cfg = Config::from_db(&db);
        assert!(cfg.providers.is_empty(), "新库应为空");
        assert_eq!(cfg.listen, "127.0.0.1:8787", "缺失设置取默认值");

        cfg.listen = "0.0.0.0:9999".into();
        cfg.providers = test_providers();
        cfg.save_to_db(&db);

        let cfg2 = Config::from_db(&db);
        assert_eq!(cfg2.listen, "0.0.0.0:9999");
        assert_eq!(cfg2.providers.len(), 2);
        assert_eq!(cfg2.providers[0].name, "openai");
        assert_eq!(cfg2.providers[0].api_key, "sk-x");
        assert_eq!(cfg2.providers[1].name, "anthropic");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_defaults_seeds_once() {
        let (db, dir) = temp_db("seed");
        assert!(Config::ensure_defaults(&db));
        let cfg = Config::from_db(&db);
        // 不内置任何供应商，由使用方在控制台添加
        assert!(cfg.providers.is_empty());
        assert_eq!(cfg.listen, "127.0.0.1:8787");
        assert_eq!(cfg.retention_days, 7);
        // 第二次不再播种
        assert!(!Config::ensure_defaults(&db));
        assert_eq!(Config::from_db(&db).providers.len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn base_url_normalization() {
        assert_eq!(normalize_base("https://api.openai.com"), "https://api.openai.com");
        assert_eq!(normalize_base("https://api.openai.com/"), "https://api.openai.com");
        assert_eq!(normalize_base("http://127.0.0.1:11434/v1"), "http://127.0.0.1:11434");
        assert_eq!(normalize_base("http://127.0.0.1:11434/v1/"), "http://127.0.0.1:11434");
        assert_eq!(normalize_base("https://relay.example.com/v1"), "https://relay.example.com");
    }

    #[test]
    fn upstream_urls() {
        let p = Provider {
            name: "t".into(),
            protocols: vec![Protocol::Chat],
            base_url: "https://api.openai.com".into(),
            api_key: String::new(),
            models: vec![],
            timeout_secs: 60,
            enabled: true,
            extra_headers: BTreeMap::new(),
        };
        assert_eq!(upstream_url(&p, Protocol::Chat), "https://api.openai.com/v1/chat/completions");
        assert_eq!(upstream_url(&p, Protocol::Anthropic), "https://api.openai.com/v1/messages");
    }
}
