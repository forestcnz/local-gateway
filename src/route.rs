use crate::config::{Config, Protocol, Provider};

/// 单个选址候选。
pub struct Candidate<'a> {
    pub provider: &'a Provider,
}

/// 「按模型名选址」解析结果。
pub struct Resolved<'a> {
    /// 健康缓存键（`协议:模型`，模型为别名解析后的真实名）
    pub key: String,
    /// 候选（按尝试顺序）：
    /// 上次成功的优先（健康缓存），其余按配置顺序排列。
    pub candidates: Vec<Candidate<'a>>,
}

/// 选址失败：没有任何启用中的供应商登记该模型。
#[derive(Debug)]
pub enum AddressError {
    NoProvider { model: String, roster: String },
}

impl AddressError {
    pub fn status(&self) -> (axum::http::StatusCode, &'static str) {
        use axum::http::StatusCode;
        match self {
            AddressError::NoProvider { .. } => (StatusCode::NOT_FOUND, "无供应商登记该模型"),
        }
    }
}

impl std::fmt::Display for AddressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddressError::NoProvider { model, roster } => write!(
                f,
                "模型「{model}」未被任何启用中的供应商登记（当前供应商：{roster}）。\
                 请在控制台「供应商」页编辑对应供应商的 models 列表，加入该模型名"
            ),
        }
    }
}

/// 模型别名解析（全局，最外层，仅单次转换）：
/// 请求模型命中别名表时替换为真实模型；别名目标不再作为别名解析（不支持链式）。
pub fn apply_aliases(aliases: &std::collections::BTreeMap<String, String>, model: &str) -> String {
    let m = model.trim();
    aliases
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(m))
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| m.to_string())
}

/// 按模型名选址：在「启用且支持当前协议」的供应商中，
/// 取 models 列表登记了该模型者作为候选（忽略大小写）。
/// `preferred`（健康缓存里上次成功的供应商）若仍在候选中则提到最前。
pub fn resolve<'a>(
    cfg: &'a Config,
    protocol: Protocol,
    model: &str,
    preferred: Option<&str>,
) -> Result<Resolved<'a>, AddressError> {
    let model = model.trim();
    let mut candidates: Vec<Candidate> = Vec::new();
    for p in cfg.providers.iter() {
        if !p.enabled || !p.protocols.contains(&protocol) {
            continue;
        }
        if p.models.iter().any(|m| m.eq_ignore_ascii_case(model)) {
            candidates.push(Candidate { provider: p });
        }
    }
    if candidates.is_empty() {
        return Err(AddressError::NoProvider {
            model: model.to_string(),
            roster: roster(cfg),
        });
    }
    if let Some(name) = preferred {
        if let Some(pos) = candidates.iter().position(|c| c.provider.name == name) {
            let winner = candidates.remove(pos);
            candidates.insert(0, winner);
        }
    }
    Ok(Resolved {
        key: format!("{}:{}", protocol.as_str(), model),
        candidates,
    })
}

fn roster(cfg: &Config) -> String {
    let s = cfg
        .providers
        .iter()
        .enumerate()
        .map(|(i, p)| format!("{}={}", i + 1, p.name))
        .collect::<Vec<_>>()
        .join(", ");
    if s.is_empty() {
        "尚未配置任何供应商".to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prov(
        name: &str,
        base: &str,
        protocols: Vec<Protocol>,
        models: Vec<&str>,
        enabled: bool,
    ) -> crate::config::Provider {
        crate::config::Provider {
            name: name.into(),
            protocols,
            base_url: base.into(),
            api_key: String::new(),
            models: models.into_iter().map(String::from).collect(),
            timeout_secs: 120,
            enabled,
            extra_headers: Default::default(),
        }
    }

    fn test_cfg() -> Config {
        Config {
            listen: "127.0.0.1:0".into(),
            upstream_timeout_secs: 120,
            sse_passthrough: true,
            max_body_mb: 20,
            cors: false,
            retention_days: 7,
            drop_params: String::new(),
            aliases: Default::default(),
            providers: vec![
                prov("a", "http://127.0.0.1:1", vec![Protocol::Chat], vec!["gpt-4o", "shared"], true),
                prov(
                    "b",
                    "http://127.0.0.1:2",
                    vec![Protocol::Chat, Protocol::Responses],
                    vec!["shared", "gpt-4o-mini"],
                    true,
                ),
                prov("anthro", "http://127.0.0.1:3", vec![Protocol::Anthropic], vec!["claude-3"], true),
                prov("off", "http://127.0.0.1:4", vec![Protocol::Chat], vec!["gpt-4o"], false),
            ],
        }
    }

    #[test]
    fn candidates_in_config_order_filtered() {
        let cfg = test_cfg();
        // "shared" 登记在 a、b；停用的 off 不算；按配置顺序 a→b
        let r = resolve(&cfg, Protocol::Chat, "shared", None).unwrap();
        let names: Vec<&str> = r.candidates.iter().map(|c| c.provider.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(r.key, "chat:shared");

        // 协议过滤：anthro 只支持 anthropic，chat 入口下不参与
        let r = resolve(&cfg, Protocol::Anthropic, "claude-3", None).unwrap();
        assert_eq!(r.candidates.len(), 1);
        assert_eq!(r.candidates[0].provider.name, "anthro");
    }

    #[test]
    fn preferred_provider_moves_to_front() {
        let cfg = test_cfg();
        let r = resolve(&cfg, Protocol::Chat, "shared", Some("b")).unwrap();
        assert_eq!(r.candidates[0].provider.name, "b");
        assert_eq!(r.candidates.len(), 2);

        // 缓存指向已失效的供应商（被删/停用）→ 忽略，保持配置顺序
        let r = resolve(&cfg, Protocol::Chat, "shared", Some("off")).unwrap();
        assert_eq!(r.candidates[0].provider.name, "a");
    }

    #[test]
    fn unregistered_model_is_404() {
        let cfg = test_cfg();
        assert!(matches!(
            resolve(&cfg, Protocol::Chat, "no-such-model", None),
            Err(AddressError::NoProvider { .. })
        ));
        // gpt-4o 登记在 a（启用）→ 可选址；off 上也登记了但已停用不参与
        let r = resolve(&cfg, Protocol::Chat, "gpt-4o", None).unwrap();
        assert_eq!(r.candidates.len(), 1);
        assert_eq!(r.candidates[0].provider.name, "a");
    }

    #[test]
    fn model_name_is_case_insensitive() {
        let cfg = test_cfg();
        let r = resolve(&cfg, Protocol::Chat, "GPT-4O", None).unwrap();
        assert_eq!(r.candidates[0].provider.name, "a");
        let r2 = resolve(&cfg, Protocol::Chat, "SHARED", None).unwrap();
        assert_eq!(r2.candidates[0].provider.name, "a");
    }

    #[test]
    fn global_alias_resolved_before_selection() {
        let mut cfg = test_cfg();
        cfg.aliases.insert("GLM-5.3".into(), "gpt-4o".into());
        // 别名先转换为真实模型，再按真实模型选址（proxy 中的调用顺序）
        let real = apply_aliases(&cfg.aliases, "glm-5.3");
        assert_eq!(real, "gpt-4o");
        let r = resolve(&cfg, Protocol::Chat, &real, None).unwrap();
        assert_eq!(r.candidates[0].provider.name, "a");
        assert_eq!(r.key, "chat:gpt-4o");
    }

    #[test]
    fn apply_aliases_single_hop_and_miss() {
        let mut a = std::collections::BTreeMap::new();
        a.insert("glm-5.3".into(), "skiff-high".into());
        // 单次转换：别名目标即使是另一个别名也不继续链式解析
        a.insert("skiff-high".into(), "final".into());
        assert_eq!(apply_aliases(&a, "glm-5.3"), "skiff-high");
        assert_eq!(apply_aliases(&a, "unknown"), "unknown");
        // 空目标视为未命中
        a.insert("empty".into(), "  ".into());
        assert_eq!(apply_aliases(&a, "empty"), "empty");
    }

    #[test]
    fn default_config_has_no_providers() {
        // 不内置任何供应商：默认配置下任何模型都应提示登记
        let cfg = Config::default_config();
        assert!(matches!(
            resolve(&cfg, Protocol::Chat, "anything", None),
            Err(AddressError::NoProvider { .. })
        ));
    }
}
