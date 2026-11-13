//! Agent 控制策略：把「参数写到哪里」（paramMap / 内置推导）与「能不能写」
//! （exposure 授权、技术边界）分开。
//!
//! 设计原则（方案 §1 / §5）：
//! - ComfyUI 节点 schema 只描述输入契约，**不等于** Agent 权限；
//! - 省略参数 = 保留模板值，不因「字段可写」就主动覆盖；
//! - 未知 / 未开放的语义明确拒绝，不静默 no-op；
//! - 策略配置非法时 **fail closed**，不能回退到更宽的默认档位。

use std::collections::HashMap;

use serde_json::Value;

/// 三档 exposure：普通出图 / 显式高级 / 工作流内部实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exposure {
    Basic,
    Advanced,
    Locked,
}

impl Exposure {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "basic" => Some(Self::Basic),
            "advanced" => Some(Self::Advanced),
            "locked" => Some(Self::Locked),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Advanced => "advanced",
            Self::Locked => "locked",
        }
    }
}

/// 模型项 `agentControls[semantic]` 的稀疏覆盖。
#[derive(Debug, Clone, Default)]
pub struct AgentControl {
    pub exposure: Option<Exposure>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub options: Option<Vec<String>>,
    pub recommended: Option<String>,
    pub description: Option<String>,
}

/// 一个模型项的有效控制策略。
#[derive(Debug, Clone, Default)]
pub struct ControlPolicy {
    controls: HashMap<String, AgentControl>,
}

/// 一次写入的来源。locked 语义对不同来源的处理不同：
/// 显式参数必须拒绝（否则静默忽略等于假成功），引擎默认值只保留模板值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteSource {
    /// 本次没有提供该参数。
    Absent,
    /// 引擎默认 / 随机值（省略 seed、默认尺寸）。
    Implicit,
    /// 调用方显式提供（顶层参数、params、参考图）。
    Explicit,
}

/// `authorize_write` 的判决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteDecision {
    Allow,
    /// locked 语义 + 隐式值：不写入，保留模板值（调用方应留下告警）。
    Preserve,
    Reject(String),
}

/// 语义名期望的标量类型（内置最小契约）。
///
/// 节点 `/object_info` 提供更精确的技术边界；这里只保证「即使拿不到
/// object_info，也不把 `"99999"` 写进 INT 输入」这一条不会退化为放行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarKind {
    Integer,
    Float,
    String,
    Boolean,
}

impl ScalarKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Integer => "an integer",
            Self::Float => "a number",
            Self::String => "a string",
            Self::Boolean => "a boolean",
        }
    }
}

pub fn is_image_semantic(name: &str) -> bool {
    name == "image"
        || name
            .strip_prefix("image")
            .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

pub fn expected_scalar(semantic: &str) -> Option<ScalarKind> {
    match semantic {
        "seed" | "width" | "height" | "steps" | "batch_size" => Some(ScalarKind::Integer),
        "cfg" => Some(ScalarKind::Float),
        "prompt" | "negative" | "sampler_name" | "scheduler" | "size" | "filename_prefix" => {
            Some(ScalarKind::String)
        }
        name if is_image_semantic(name) => Some(ScalarKind::String),
        // `layout` 允许对象 / 数组：写入前会被序列化成 JSON 文本。
        _ => None,
    }
}

/// 标量类型校验；返回可操作错误文本。
pub fn check_scalar(kind: ScalarKind, value: &Value) -> Option<String> {
    let ok = match kind {
        ScalarKind::Integer => value.as_i64().is_some() || value.as_u64().is_some(),
        ScalarKind::Float => value.as_f64().is_some(),
        ScalarKind::String => value.as_str().is_some(),
        ScalarKind::Boolean => value.as_bool().is_some(),
    };
    if ok {
        return None;
    }
    Some(format!(
        "must be {}, got {}",
        kind.as_str(),
        match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    ))
}

/// 默认 exposure：只有明确识别、普通用户会调的语义是 basic；
/// 采样参数与布局/负向词是 advanced；其余（sampler / scheduler / 模型 / LoRA /
/// 逐层权重）locked。
pub fn default_exposure(semantic: &str) -> Exposure {
    match semantic {
        "prompt" | "seed" | "width" | "height" | "size" => Exposure::Basic,
        "negative" | "steps" | "cfg" | "layout" => Exposure::Advanced,
        name if is_image_semantic(name) => Exposure::Basic,
        _ => Exposure::Locked,
    }
}

impl ControlPolicy {
    /// 从模型项的 `agentControls` JSON 读取；非法条目**直接报错**，不静默放宽。
    pub fn from_json(value: Option<&Value>) -> std::result::Result<Self, String> {
        let mut controls = HashMap::new();
        let Some(raw_value) = value else {
            return Ok(Self { controls });
        };
        let Some(map) = raw_value.as_object() else {
            return Err("agentControls must be a JSON object".to_string());
        };
        for (semantic, raw) in map {
            let Some(entry) = raw.as_object() else {
                return Err(format!("agentControls.`{semantic}` must be an object"));
            };
            let mut control = AgentControl::default();
            if let Some(raw_exposure) = entry.get("exposure") {
                let Some(text) = raw_exposure.as_str() else {
                    return Err(format!(
                        "agentControls.`{semantic}`.exposure must be a string (basic|advanced|locked)"
                    ));
                };
                let Some(exposure) = Exposure::parse(text) else {
                    return Err(format!(
                        "agentControls.`{semantic}`.exposure must be basic|advanced|locked, got `{text}`"
                    ));
                };
                control.exposure = Some(exposure);
            }
            if let Some(raw_min) = entry.get("min") {
                let Some(number) = raw_min.as_f64() else {
                    return Err(format!("agentControls.`{semantic}`.min must be a number"));
                };
                control.min = Some(number);
            }
            if let Some(raw_max) = entry.get("max") {
                let Some(number) = raw_max.as_f64() else {
                    return Err(format!("agentControls.`{semantic}`.max must be a number"));
                };
                control.max = Some(number);
            }
            if let (Some(min), Some(max)) = (control.min, control.max) {
                if min > max {
                    return Err(format!(
                        "agentControls.`{semantic}`.min must be <= max (got {min} > {max})"
                    ));
                }
            }
            if let Some(raw_options) = entry.get("options") {
                let Some(items) = raw_options.as_array() else {
                    return Err(format!(
                        "agentControls.`{semantic}`.options must be an array"
                    ));
                };
                let mut list = Vec::with_capacity(items.len());
                for item in items {
                    let Some(text) = item.as_str() else {
                        return Err(format!(
                            "agentControls.`{semantic}`.options entries must be strings"
                        ));
                    };
                    let text = text.trim();
                    if text.is_empty() {
                        return Err(format!(
                            "agentControls.`{semantic}`.options entries must not be empty"
                        ));
                    }
                    list.push(text.to_string());
                }
                if list.is_empty() {
                    return Err(format!(
                        "agentControls.`{semantic}`.options must not be empty"
                    ));
                }
                control.options = Some(list);
            }
            control.recommended = entry
                .get("recommended")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string);
            control.description = entry
                .get("description")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string);
            controls.insert(semantic.clone(), control);
        }
        Ok(Self { controls })
    }

    /// 有效 exposure。默认 locked 的语义最多只能被显式策略升到 advanced，
    /// 不能直接开放为 basic。
    pub fn exposure_for(&self, semantic: &str) -> Exposure {
        let base = default_exposure(semantic);
        let Some(requested) = self
            .controls
            .get(semantic)
            .and_then(|control| control.exposure)
        else {
            return base;
        };
        match (base, requested) {
            (Exposure::Locked, Exposure::Basic) => Exposure::Advanced,
            _ => requested,
        }
    }

    pub fn control(&self, semantic: &str) -> Option<&AgentControl> {
        self.controls.get(semantic)
    }

    /// 写入授权：所有写入口（顶层参数、params、参考图）都必须先过这里。
    ///
    /// - 未提供 → 不写，无所谓；
    /// - 非 locked → 放行（是否越界由 `check_value` / 节点技术边界继续校验）；
    /// - locked + 显式 → 拒绝，绝不静默丢弃；
    /// - locked + 隐式 → 保留模板值（引擎默认值不构成越权覆盖）。
    pub fn authorize_write(&self, semantic: &str, source: WriteSource) -> WriteDecision {
        if source == WriteSource::Absent {
            return WriteDecision::Allow;
        }
        if self.exposure_for(semantic) != Exposure::Locked {
            return WriteDecision::Allow;
        }
        match source {
            WriteSource::Absent => WriteDecision::Allow,
            WriteSource::Implicit => WriteDecision::Preserve,
            WriteSource::Explicit => WriteDecision::Reject(format!(
                "`{semantic}` is locked for this workflow; remove the value or expose it in agentControls"
            )),
        }
    }

    /// 校验一个显式覆盖值：locked / 类型不符 / 越界 / 不在枚举内都返回可操作错误。
    ///
    /// 只校验「有没有权限改、值是否在技术边界内」；节点目标与连线保护仍由
    /// `validate_params` / `workflow` 负责，两者不混成一条优先级。
    pub fn check_value(&self, semantic: &str, value: &Value) -> Option<String> {
        if self.exposure_for(semantic) == Exposure::Locked {
            return Some(format!(
                "`{semantic}` is locked for this workflow; it is not exposed to the agent"
            ));
        }
        // 标量类型先闭合：`{"steps":"99999"}` / `{"steps":false}` / `{"sampler_name":123}`
        // 都不能靠「不是 number 就跳过范围检查」溜过去。
        if let Some(kind) = expected_scalar(semantic) {
            if let Some(problem) = check_scalar(kind, value) {
                return Some(format!("`{semantic}` {problem}"));
            }
        }
        let Some(control) = self.controls.get(semantic) else {
            return None;
        };
        if let (Some(options), Some(text)) = (&control.options, value.as_str()) {
            if !options.iter().any(|option| option == text) {
                return Some(format!(
                    "`{semantic}` must be one of [{}], got `{text}`",
                    options.join(", ")
                ));
            }
        }
        if let Some(number) = value.as_f64() {
            if let Some(min) = control.min {
                if number < min {
                    return Some(format!("`{semantic}` must be >= {min}, got {number}"));
                }
            }
            if let Some(max) = control.max {
                if number > max {
                    return Some(format!("`{semantic}` must be <= {max}, got {number}"));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_expose_prompt_and_size_but_lock_sampler() {
        assert_eq!(default_exposure("prompt"), Exposure::Basic);
        assert_eq!(default_exposure("image2"), Exposure::Basic);
        assert_eq!(default_exposure("steps"), Exposure::Advanced);
        assert_eq!(default_exposure("layout"), Exposure::Advanced);
        assert_eq!(default_exposure("negative"), Exposure::Advanced);
        assert_eq!(default_exposure("sampler_name"), Exposure::Locked);
        assert_eq!(default_exposure("layer_weights"), Exposure::Locked);
    }

    #[test]
    fn agent_controls_can_override_exposure_but_not_to_basic_from_locked() {
        let policy = ControlPolicy::from_json(Some(&json!({
            "sampler_name": {"exposure": "advanced", "options": ["euler", "dpmpp_2m"]},
            "steps": {"exposure": "locked"},
            "cfg": {"exposure": "basic"}
        })))
        .expect("valid policy");
        assert_eq!(policy.exposure_for("sampler_name"), Exposure::Advanced);
        assert_eq!(policy.exposure_for("steps"), Exposure::Locked);
        // cfg 默认 advanced，可以收紧到 locked，也可以显式开放为 basic。
        assert_eq!(policy.exposure_for("cfg"), Exposure::Basic);
        // 默认 locked 的语义不能被策略降到 basic，最多 advanced。
        assert_eq!(policy.exposure_for("layer_weights"), Exposure::Locked);
    }

    #[test]
    fn locked_default_cannot_be_opened_as_basic() {
        let policy = ControlPolicy::from_json(Some(&json!({
            "sampler_name": {"exposure": "basic"}
        })))
        .expect("valid policy");
        assert_eq!(policy.exposure_for("sampler_name"), Exposure::Advanced);
    }

    #[test]
    fn rejects_locked_semantic() {
        let policy = ControlPolicy::default();
        let error = policy
            .check_value("sampler_name", &json!("euler"))
            .expect("locked must fail");
        assert!(error.contains("locked"), "{error}");
    }

    #[test]
    fn checks_enum_and_range() {
        let policy = ControlPolicy::from_json(Some(&json!({
            "steps": {"min": 1, "max": 50},
            "sampler_name": {"exposure": "advanced", "options": ["euler", "dpmpp_2m"]}
        })))
        .expect("valid policy");
        assert!(policy.check_value("steps", &json!(30)).is_none());
        assert!(policy.check_value("steps", &json!(0)).unwrap().contains(">= 1"));
        assert!(policy
            .check_value("steps", &json!(51))
            .unwrap()
            .contains("<= 50"));
        assert!(policy.check_value("sampler_name", &json!("euler")).is_none());
        assert!(policy
            .check_value("sampler_name", &json!("unknown"))
            .unwrap()
            .contains("must be one of"));
    }

    #[test]
    fn locked_semantic_rejects_explicit_but_preserves_implicit() {
        let policy = ControlPolicy::from_json(Some(&json!({
            "negative": {"exposure": "locked"},
            "seed": {"exposure": "locked"}
        })))
        .expect("valid policy");
        // 顶层 negativePrompt / 显式 seed：必须拒绝，不能静默丢弃。
        for (semantic, value) in [("negative", json!("blurry")), ("seed", json!(42))] {
            match policy.authorize_write(semantic, WriteSource::Explicit) {
                WriteDecision::Reject(message) => {
                    assert!(message.contains("locked"), "{message}");
                    assert!(policy.check_value(semantic, &value).is_some());
                }
                other => panic!("{semantic} should be rejected, got {other:?}"),
            }
        }
        // 省略 seed / 默认尺寸：保留模板值，不因随机值覆盖。
        assert_eq!(
            policy.authorize_write("seed", WriteSource::Implicit),
            WriteDecision::Preserve
        );
        assert_eq!(
            policy.authorize_write("width", WriteSource::Implicit),
            WriteDecision::Allow
        );
        // 未提供：什么都不写。
        assert_eq!(
            policy.authorize_write("negative", WriteSource::Absent),
            WriteDecision::Allow
        );
    }

    #[test]
    fn rejects_scalar_type_mismatches() {
        let policy = ControlPolicy::from_json(Some(&json!({
            "steps": {"min": 1, "max": 50},
            "sampler_name": {"exposure": "advanced", "options": ["euler"]}
        })))
        .expect("valid policy");
        // 范围只检查 number 是不够的：字符串 / 布尔不能借类型漏洞过关。
        for value in [json!("99999"), json!(false), json!(null)] {
            let problem = policy
                .check_value("steps", &value)
                .unwrap_or_else(|| panic!("{value} must be rejected"));
            assert!(problem.contains("must be an integer"), "{problem}");
        }
        let problem = policy
            .check_value("sampler_name", &json!(123))
            .expect("number must not be accepted as a sampler name");
        assert!(problem.contains("must be a string"), "{problem}");
    }

    #[test]
    fn rejects_malformed_controls() {
        assert!(ControlPolicy::from_json(Some(&json!({
            "steps": "not-an-object"
        })))
        .is_err());
        assert!(ControlPolicy::from_json(Some(&json!({
            "steps": {"exposure": "lock"}
        })))
        .is_err());
        assert!(ControlPolicy::from_json(Some(&json!({
            "steps": {"min": "high"}
        })))
        .is_err());
        assert!(ControlPolicy::from_json(Some(&json!({
            "steps": {"options": []}
        })))
        .is_err());
        assert!(ControlPolicy::from_json(Some(&json!({
            "steps": {"min": 10, "max": 1}
        })))
        .is_err());
    }
}
