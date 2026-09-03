use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use crate::config::Config;
use crate::db::Db;

/// 全局共享状态。
/// - `cfg`：配置的内存缓存（Arc 包裹，修改时校验→写库→整体替换）。
/// - `db`：SQLite 存储（请求日志 + 供应商 + 设置），唯一持久化来源。
pub struct AppState {
    cfg: RwLock<Arc<Config>>,
    pub db: Arc<Db>,
    pub client: reqwest::Client,
    /// 健康缓存：键 `协议:模型` → 上次成功的供应商名。
    /// 选址时优先尝试，失败后仍会按顺序遍历其余候选。
    healthy: Mutex<HashMap<String, String>>,
}

impl AppState {
    pub fn new(cfg: Config, db: Arc<Db>, client: reqwest::Client) -> Self {
        Self {
            cfg: RwLock::new(Arc::new(cfg)),
            db,
            client,
            healthy: Mutex::new(HashMap::new()),
        }
    }

    pub fn config(&self) -> Arc<Config> {
        self.cfg.read().unwrap().clone()
    }

    pub fn replace_config(&self, cfg: Config) {
        *self.cfg.write().unwrap() = Arc::new(cfg);
    }

    /// 从 DB 重新加载配置缓存。
    pub fn reload_config(&self) {
        self.replace_config(Config::from_db(&self.db));
    }

    /// 修改配置的统一入口：克隆 → 变更（可校验失败）→ 写库 → 原子替换缓存。
    pub fn update_config<F>(&self, f: F) -> Result<Arc<Config>, String>
    where
        F: FnOnce(&mut Config) -> Result<(), String>,
    {
        let mut cfg = (*self.config()).clone();
        f(&mut cfg)?;
        cfg.validate()?;
        cfg.save_to_db(&self.db);
        let cfg = Arc::new(cfg);
        *self.cfg.write().unwrap() = cfg.clone();
        Ok(cfg)
    }

    pub fn healthy_get(&self, key: &str) -> Option<String> {
        self.healthy.lock().ok()?.get(key).cloned()
    }

    pub fn healthy_set(&self, key: &str, provider: &str) {
        if let Ok(mut m) = self.healthy.lock() {
            m.insert(key.to_string(), provider.to_string());
        }
    }
}
