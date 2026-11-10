//! 工作流参数契约：从工作流图 + `/object_info` + 当前值，推导「Agent 怎么调」。
//!
//! 设计原则：
//! - 只描述**能确认**的东西。`layout` 这类 STRING 输入的真实结构从当前值反推，
//!   数组元素合并所有样本；无法确定必填/枚举时明确标注 `confidence: low`。
//! - 不凭字段名编造语义。节点级 `display_name` / `description` 优先，找不到就
//!   如实写「工作流输入 `name` (type)」。
//! - 执行器与 Agent 用同一份契约：工具描述取摘要，`comfyui-describe-workflow`
//!   返回完整结构，提交前用 `validate_params` 校验参数键与 JSON 文本。

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use super::convert;
use super::workflow::RESERVED_EXTRA_KEYS;

/// 一个可被 Agent 传入的语义输入。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractInput {
    /// 语义名（`prompt` / `layout` / `image2` …）。
    pub name: String,
    /// 实际写入的点号键（可能多个落点）。
    pub maps_to: Vec<String>,
    /// string / integer / number / boolean / object / array / image
    pub value_type: String,
    /// 节点定义里的 required（Autogrow 视为非必填）。
    pub required: bool,
    pub description: String,
    /// 结构化输入的子 schema（JSON Schema 子集 + `x-observed*` 标注）。
    pub schema: Option<Value>,
    /// 从当前值提取的精简示例（长字符串截断）。
    pub example: Option<Value>,
    /// `high`（来自节点定义）/ `low`（仅从当前样本推断）。
    pub confidence: String,
}

/// 一个工作流的完整调用契约。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowContract {
    /// 目前固定 `derived`：由工作流与节点定义自动分析得出。
    pub provenance: String,
    pub inputs: Vec<ContractInput>,
    pub warnings: Vec<String>,
}

/// 从工作流图推导契约；`object_info` 不可用时仍返回输入清单，但标注缺文档。
///
/// `overrides` 是当前生效模型项的显式 `paramMap`：必须与执行路径完全一致，
/// 否则 Agent 查到的落点会和实际写入的节点不同。
pub fn derive_contract(
    graph: &Value,
    object_info: Option<&Value>,
    overrides: Option<&HashMap<String, Vec<String>>>,
) -> WorkflowContract {
    let mut warnings = Vec::new();
    if object_info.is_none() {
        warnings.push(
            "object_info unavailable: required-ness and node documentation are unknown"
                .to_string(),
        );
    }

    let mut semantic: HashMap<String, Vec<String>> = convert::semantic_param_map(graph)
        .into_iter()
        .collect();
    if let Some(overrides) = overrides {
        // 逐项覆盖：空数组表示显式关闭该语义（与执行路径一致）。
        for (name, paths) in overrides {
            semantic.insert(name.clone(), paths.clone());
        }
    }
    let mut inputs: Vec<ContractInput> = Vec::new();
    for (name, paths) in semantic {
        let Some(path) = paths.first() else {
            continue;
        };
        let current = value_at(graph, path);
        let (value_type, schema) = classify(current, &name);
        if matches!(value_type.as_str(), "object" | "array") {
            warnings.push(format!(
                "`{name}` schema is inferred from one sample; required keys and enums still need confirmation"
            ));
        }
        if current.is_none() {
            warnings.push(format!(
                "`{name}` maps to `{path}` but the current value could not be read"
            ));
        }
        let required = required_in_object_info(object_info, graph, path);
        inputs.push(ContractInput {
            description: describe_input(object_info, graph, path, &name, &value_type),
            name,
            maps_to: paths,
            value_type,
            required,
            schema,
            example: current.map(|value| example_for(value, 120)),
            confidence: if required { "high" } else { "low" }.to_string(),
        });
    }

    // 让 Agent 先看到最常用的语义，而不是字母序里排在后面的 layout。
    inputs.sort_by(|a, b| {
        contract_rank(&a.name)
            .cmp(&contract_rank(&b.name))
            .then_with(|| a.name.cmp(&b.name))
    });

    WorkflowContract {
        provenance: "derived".to_string(),
        inputs,
        warnings,
    }
}

fn contract_rank(name: &str) -> usize {
    match name {
        "prompt" => 0,
        "negative" => 1,
        "image" => 2,
        "width" | "height" => 3,
        "seed" => 4,
        _ if name.starts_with("image") => 5,
        _ => 6,
    }
}

/// 读取点号键指向的当前值；兼容 `node.input` 与 `node.inputs.input` 两种写法。
fn value_at<'a>(graph: &'a Value, path: &str) -> Option<&'a Value> {
    let parts: Vec<&str> = path
        .split('.')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    let node = graph.get(parts.first()?)?;
    let inputs = node.get("inputs")?;
    let is_inputs_segment = parts
        .get(1)
        .map(|part| part.eq_ignore_ascii_case("inputs"))
        .unwrap_or(false);
    let rest = if is_inputs_segment { &parts[2..] } else { &parts[1..] };
    if rest.is_empty() {
        return None;
    }
    // Autogrow 的真实输入名本身含点号：先按扁平键命中，再逐段下钻。
    if let Some(value) = inputs.get(rest.join(".")) {
        return Some(value);
    }
    let mut current = inputs;
    for part in rest {
        current = current.get(*part)?;
    }
    Some(current)
}

/// 类型判定：JSON 字符串会被解析为结构化类型，并生成子 schema。
fn classify(current: Option<&Value>, name: &str) -> (String, Option<Value>) {
    if name.starts_with("image") {
        return ("image".to_string(), None);
    }
    let Some(value) = current else {
        return ("string".to_string(), None);
    };
    match value {
        Value::Object(_) => ("object".to_string(), Some(infer_schema(value))),
        Value::Array(_) => ("array".to_string(), Some(infer_schema(value))),
        Value::Bool(_) => ("boolean".to_string(), None),
        Value::Number(number) if number.is_i64() || number.is_u64() => {
            ("integer".to_string(), None)
        }
        Value::Number(_) => ("number".to_string(), None),
        Value::String(text) => {
            if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                if parsed.is_object() || parsed.is_array() {
                    let kind = if parsed.is_object() { "object" } else { "array" };
                    return (kind.to_string(), Some(infer_schema(&parsed)));
                }
            }
            ("string".to_string(), None)
        }
        Value::Null => ("string".to_string(), None),
    }
}

/// 节点定义里是否把该输入声明为 required；Autogrow 允许 0 个，按非必填处理。
fn required_in_object_info(
    object_info: Option<&Value>,
    graph: &Value,
    path: &str,
) -> bool {
    let Some(info) = object_info else {
        return false;
    };
    let parts: Vec<&str> = path.split('.').collect();
    let Some(node_id) = parts.first() else {
        return false;
    };
    let Some(class_type) = graph
        .get(*node_id)
        .and_then(|node| node.get("class_type"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    let input_name = if parts
        .get(1)
        .map(|part| part.eq_ignore_ascii_case("inputs"))
        .unwrap_or(false)
    {
        parts[2..].join(".")
    } else {
        parts[1..].join(".")
    };
    let Some(decl) = info
        .get(class_type)
        .and_then(|node| node.get("input"))
        .and_then(|input| input.get("required"))
        .and_then(|required| required.get(&input_name))
    else {
        return false;
    };
    let type_name = decl
        .get(0)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase();
    !type_name.contains("AUTOGROW")
}

fn describe_input(
    object_info: Option<&Value>,
    graph: &Value,
    path: &str,
    name: &str,
    value_type: &str,
) -> String {
    if let Some(info) = object_info {
        let parts: Vec<&str> = path.split('.').collect();
        if let Some(node_id) = parts.first() {
            if let Some(class_type) = graph
                .get(*node_id)
                .and_then(|node| node.get("class_type"))
                .and_then(Value::as_str)
            {
                if let Some(definition) = info.get(class_type) {
                    if let Some(description) = definition
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                    {
                        return format!("{description} (input `{name}`, {value_type})");
                    }
                    if let Some(display) = definition
                        .get("display_name")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                    {
                        return format!("{display} input `{name}` ({value_type})");
                    }
                }
            }
        }
    }
    match name {
        "image" | "image2" | "image3" => {
            "参考图上传后写入的工作流输入文件名".to_string()
        }
        "layout" => "JSON 文本；内部结构见 schema（从当前值推断，需确认）".to_string(),
        "character_boxes" => "JSON 文本；角色框列表（结构见 schema）".to_string(),
        "prompt" => "正向提示词".to_string(),
        "negative" => "负向提示词".to_string(),
        "seed" => "随机种子".to_string(),
        "width" | "height" => "输出尺寸".to_string(),
        _ => format!("工作流输入 `{name}` ({value_type})"),
    }
}

/// 从单个值推断 JSON Schema 子集；不写死 `required`，用 `x-observed*` 标注样本信息。
pub fn infer_schema(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let properties: Map<String, Value> = map
                .iter()
                .map(|(key, item)| (key.clone(), infer_schema(item)))
                .collect();
            json!({
                "type": "object",
                "properties": properties,
                "x-observedKeys": map.keys().cloned().collect::<Vec<_>>(),
            })
        }
        Value::Array(items) => {
            let items_schema = if items.is_empty() {
                json!({})
            } else if items.iter().all(Value::is_object) {
                // 合并所有元素的键：当前样本里出现过的字段都列出来，
                // 但不把它们标成 required（单样本不足以断言）。
                let mut merged: Map<String, Value> = Map::new();
                for item in items {
                    if let Some(map) = item.as_object() {
                        for (key, value) in map {
                            let next = infer_schema(value);
                            match merged.get_mut(key) {
                                Some(existing) => {
                                    // 同名字段在不同样本里可能是 integer / number：
                                    // 合并而不是保留首次观察，避免 schema 比模板
                                    // 自身还严格（例如 weight: 1 与 1.5）。
                                    *existing = merge_schema(existing, &next);
                                }
                                None => {
                                    merged.insert(key.clone(), next);
                                }
                            }
                        }
                    }
                }
                json!({ "type": "object", "properties": merged })
            } else if items.iter().all(Value::is_number) {
                // `box: [x1, y1, x2, y2]` 这类整数数组：不能让 Agent 看到 array<any>。
                if items
                    .iter()
                    .all(|item| item.as_i64().is_some() || item.as_u64().is_some())
                {
                    json!({ "type": "integer" })
                } else {
                    json!({ "type": "number" })
                }
            } else if items.iter().all(Value::is_string) {
                json!({ "type": "string" })
            } else if items.iter().all(Value::is_boolean) {
                json!({ "type": "boolean" })
            } else {
                json!({})
            };
            json!({
                "type": "array",
                "items": items_schema,
                "x-observedLength": items.len(),
            })
        }
        Value::String(text) => {
            if text.contains('\n') {
                json!({ "type": "string", "x-multiline": true })
            } else {
                json!({ "type": "string" })
            }
        }
        Value::Bool(_) => json!({ "type": "boolean" }),
        Value::Number(number) if number.is_i64() || number.is_u64() => {
            json!({ "type": "integer" })
        }
        Value::Number(_) => json!({ "type": "number" }),
        Value::Null => json!({ "type": "null" }),
    }
}

/// 合并两个观察到的 schema 类型；无法安全合并时放宽为 any。
fn merge_schema(left: &Value, right: &Value) -> Value {
    let left_type = left.get("type").and_then(Value::as_str).unwrap_or("");
    let right_type = right.get("type").and_then(Value::as_str).unwrap_or("");
    if left_type == right_type {
        return match left_type {
            // 同名对象/数组必须递归合并，否则嵌套字段会保留首样本的类型，
            // 导致模板自身第二份数据被拒。
            "object" => {
                let mut merged = left.clone();
                if let (Some(left_props), Some(right_props)) = (
                    left.get("properties").and_then(Value::as_object),
                    right.get("properties").and_then(Value::as_object),
                ) {
                    let mut properties = left_props.clone();
                    for (key, value) in right_props {
                        match properties.get_mut(key) {
                            Some(existing) => {
                                *existing = merge_schema(existing, value);
                            }
                            None => {
                                properties.insert(key.clone(), value.clone());
                            }
                        }
                    }
                    merged["properties"] = Value::Object(properties);
                }
                merged
            }
            "array" => {
                let mut merged = left.clone();
                if let (Some(left_items), Some(right_items)) =
                    (left.get("items"), right.get("items"))
                {
                    merged["items"] = merge_schema(left_items, right_items);
                }
                merged
            }
            _ => left.clone(),
        };
    }
    if matches!(
        (left_type, right_type),
        ("integer", "number") | ("number", "integer")
    ) {
        return json!({ "type": "number" });
    }
    // 类型冲突：低置信度样本不应拒绝模板自身的数据。
    json!({})
}

/// 从当前值生成精简示例：JSON 字符串先解析，长字符串截断，数组最多 2 项。
fn example_for(value: &Value, max_string: usize) -> Value {
    if let Value::String(text) = value {
        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            if parsed.is_object() || parsed.is_array() {
                return compact_example(&parsed, max_string);
            }
        }
    }
    compact_example(value, max_string)
}

fn compact_example(value: &Value, max_string: usize) -> Value {
    match value {
        Value::String(text) => {
            let truncated: String = text.chars().take(max_string).collect();
            if text.chars().count() > max_string {
                json!(format!("{truncated}…"))
            } else {
                json!(text)
            }
        }
        Value::Array(items) => {
            // 对象数组（layout.items）只保留 2 个样本；坐标/标量数组
            // （box: [x1,y1,x2,y2]）必须完整保留，截断会教模型构造错误报文。
            let scalar = items
                .iter()
                .all(|item| !item.is_object() && !item.is_array());
            let limit = if scalar { 16 } else { 2 };
            Value::Array(
                items
                    .iter()
                    .take(limit)
                    .map(|item| compact_example(item, max_string))
                    .collect(),
            )
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, item) in map.iter().take(12) {
                out.insert(key.clone(), compact_example(item, max_string));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// 把「字符串形态的 JSON」压成一行类型摘要，供工具描述使用。
///
/// 例：`JSON object{width:integer, height:integer, items:array<object{label:string, caption:string, box:array<integer>}>}`
pub fn summarize_json_text(text: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(text).ok()?;
    if !(parsed.is_object() || parsed.is_array()) {
        return None;
    }
    Some(format!(
        "JSON {}",
        summarize_schema(&infer_schema(&parsed), 3)
    ))
}

/// Schema 的紧凑类型描述（限制深度与字段数，避免工具描述爆炸）。
pub fn summarize_schema(schema: &Value, depth: usize) -> String {
    let kind = schema
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("any");
    match kind {
        "object" => {
            if depth == 0 {
                return "object{…}".to_string();
            }
            let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
                return "object".to_string();
            };
            if properties.is_empty() {
                return "object{}".to_string();
            }
            let inner: Vec<String> = properties
                .iter()
                .take(10)
                .map(|(key, value)| format!("{key}:{}", summarize_schema(value, depth - 1)))
                .collect();
            let more = if properties.len() > 10 { ", …" } else { "" };
            format!("object{{{}{more}}}", inner.join(", "))
        }
        "array" => {
            let items = schema.get("items").unwrap_or(&Value::Null);
            format!(
                "array<{}>",
                summarize_schema(items, depth.saturating_sub(1))
            )
        }
        "string" => {
            if schema.get("x-multiline").and_then(Value::as_bool) == Some(true) {
                "string(multiline)".to_string()
            } else {
                "string".to_string()
            }
        }
        other => other.to_string(),
    }
}

/// 提交前校验 `params` 的键与结构化值。
///
/// 检查分两层：
/// 1. 键：未知键（既不是点号键，也不是当前工作流的语义名）→ 错误；
/// 2. 值：如果契约里有该输入的 schema，按类型递归校验（对象字段 / 数组元素）。
///    字符串形态的 JSON 会先解析再校验，`20.layout` 这类点号键也走同一条路径。
pub fn validate_params(
    param_map: &HashMap<String, Vec<String>>,
    contract: Option<&WorkflowContract>,
    extra: Option<&Map<String, Value>>,
) -> Vec<String> {
    let Some(extra) = extra else {
        return Vec::new();
    };
    let mut problems = Vec::new();
    for (key, raw_value) in extra {
        if RESERVED_EXTRA_KEYS.contains(&key.as_str()) {
            continue;
        }
        let is_dotted = key
            .split('.')
            .filter(|part| !part.trim().is_empty())
            .count()
            >= 2;
        // 点号键（`20.layout`）可以绕过语义名直接写节点：此时不应该因为
        // `param_map` 里没有这个 key 就跳过 schema 校验。
        let mapped_paths = param_map.get(key);
        if mapped_paths.is_none() && !is_dotted {
            let available: Vec<&str> = param_map.keys().map(String::as_str).collect();
            problems.push(format!(
                "params key `{key}` is neither a dotted workflow key nor a mapped semantic name; available: {}",
                available.join(", ")
            ));
            continue;
        }
        if mapped_paths
            .map(|paths| paths.is_empty())
            .unwrap_or(false)
        {
            // 显式关闭的语义：不注入，也不校验。
            continue;
        }

        let schema = contract
            .and_then(|contract| {
                contract
                    .inputs
                    .iter()
                    .find(|input| input.name == *key)
                    .or_else(|| {
                        contract.inputs.iter().find(|input| {
                            input
                                .maps_to
                                .iter()
                                .any(|path| canonical_path(path) == canonical_path(key))
                        })
                    })
            })
            .and_then(|input| input.schema.as_ref());

        let Some(schema) = schema else {
            // 没有 schema 时保留旧的最小检查（兼容手写 paramMap / 纯字符串字段）。
            if matches!(key.as_str(), "layout" | "character_boxes") {
                if let Value::String(text) = raw_value {
                    if serde_json::from_str::<Value>(text).is_err() {
                        problems.push(format!("params `{key}` must be valid JSON text"));
                    }
                }
            }
            continue;
        };

        let value = match raw_value {
            Value::String(text)
                if matches!(
                    schema.get("type").and_then(Value::as_str),
                    Some("object") | Some("array")
                ) =>
            {
                match serde_json::from_str::<Value>(text) {
                    Ok(parsed) => parsed,
                    Err(_) => {
                        problems.push(format!("params `{key}` must be valid JSON text"));
                        continue;
                    }
                }
            }
            other => other.clone(),
        };
        validate_value(schema, &value, key, &mut problems);
    }
    problems
}

/// 按契约里的 JSON Schema 子集递归校验一个值。
///
/// 只校验类型与已知字段：单样本推断出的 `properties` 可能不全，未知字段放行；
/// `x-observedLength` 只作样本信息，不强制长度。
fn validate_value(schema: &Value, value: &Value, path: &str, problems: &mut Vec<String>) {
    let expected = schema.get("type").and_then(Value::as_str).unwrap_or("");
    let type_ok = match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        _ => true,
    };
    if !type_ok {
        problems.push(format!(
            "params `{path}` must be {expected}, got {}",
            value_type_name(value)
        ));
        return;
    }

    if let (Some(properties), Some(map)) = (
        schema.get("properties").and_then(Value::as_object),
        value.as_object(),
    ) {
        for (key, child) in map {
            if let Some(child_schema) = properties.get(key) {
                validate_value(child_schema, child, &format!("{path}.{key}"), problems);
            }
        }
    }
    if let (Some(items_schema), Some(items)) =
        (schema.get("items"), value.as_array())
    {
        for (index, item) in items.iter().enumerate() {
            validate_value(
                items_schema,
                item,
                &format!("{path}[{index}]"),
                problems,
            );
        }
    }
}

/// 与写入器一致的路径归一化：`20.layout` 与 `20.inputs.layout` 视为同一目标。
fn canonical_path(path: &str) -> String {
    let parts: Vec<&str> = path
        .split('.')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    let Some(node) = parts.first() else {
        return String::new();
    };
    let mut out: Vec<&str> = vec![node, "inputs"];
    if parts
        .get(1)
        .map(|part| part.eq_ignore_ascii_case("inputs"))
        .unwrap_or(false)
    {
        out.extend(parts[2..].iter());
    } else {
        out.extend(parts[1..].iter());
    }
    out.join(".")
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_graph() -> Value {
        serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/moss-image-layout.api.json"
        )))
        .unwrap()
    }

    #[test]
    fn derives_layout_contract_from_workflow() {
        let graph = layout_graph();
        let contract = derive_contract(&graph, None, None);
        let layout = contract
            .inputs
            .iter()
            .find(|input| input.name == "layout")
            .expect("layout input");

        assert_eq!(layout.value_type, "object");
        let schema = layout.schema.as_ref().expect("layout schema");
        assert_eq!(schema["type"], json!("object"));
        let properties = schema["properties"].as_object().expect("properties");
        assert!(properties.contains_key("items"), "{properties:?}");
        let items = &properties["items"];
        assert_eq!(items["type"], json!("array"));
        assert_eq!(items["items"]["type"], json!("object"));
        let item_properties = items["items"]["properties"].as_object().unwrap();
        for key in ["label", "caption", "box"] {
            assert!(item_properties.contains_key(key), "missing {key}: {item_properties:?}");
        }
        assert_eq!(item_properties["box"]["type"], json!("array"));
        // 示例必须是结构化的，而不是一整段字符串；坐标元组不能被截断。
        let example = layout.example.as_ref().expect("layout example");
        assert!(example.is_object());
        let box_values = example["items"][0]["box"]
            .as_array()
            .expect("box example");
        assert_eq!(
            box_values.len(),
            4,
            "box must keep all 4 coordinates: {box_values:?}"
        );
    }

    #[test]
    fn summarizes_layout_json_without_dumping_the_string() {
        let graph = layout_graph();
        let value = value_at(&graph, "20.layout").expect("layout value");
        let text = value.as_str().unwrap();
        let summary = summarize_json_text(text).expect("summary");
        assert!(summary.contains("object{"), "{summary}");
        assert!(summary.contains("items:array<object{"), "{summary}");
        assert!(summary.contains("box:array<integer>"), "{summary}");
    }

    #[test]
    fn validates_unknown_keys_and_bad_layout_json() {
        let mut param_map = HashMap::new();
        param_map.insert("layout".to_string(), vec!["20.layout".to_string()]);
        param_map.insert("strict".to_string(), vec!["20.strict".to_string()]);

        let mut extra = Map::new();
        extra.insert("layout".to_string(), json!("{not json"));
        extra.insert("mystery".to_string(), json!(1));
        let problems = validate_params(&param_map, None, Some(&extra));
        assert!(problems.iter().any(|problem| problem.contains("mystery")));
        assert!(problems.iter().any(|problem| problem.contains("valid JSON")));

        let mut good = Map::new();
        good.insert("layout".to_string(), json!({ "items": [] }));
        good.insert("strict".to_string(), json!(true));
        good.insert("7.steps".to_string(), json!(30));
        assert!(validate_params(&param_map, None, Some(&good)).is_empty());
    }

    /// 同名字段出现 integer / number 两种样本时，schema 必须放宽为 number，
    /// 否则 Agent 原样传回模板自身的数据会被拒绝。
    #[test]
    fn merges_integer_and_number_types_in_samples() {
        let schema = infer_schema(&json!([
            { "weight": 1 },
            { "weight": 1.5 }
        ]));
        assert_eq!(
            schema["items"]["properties"]["weight"]["type"],
            json!("number"),
            "{schema}"
        );
        let mut problems = Vec::new();
        validate_value(
            &schema,
            &json!([{ "weight": 1.5 }]),
            "items",
            &mut problems,
        );
        assert!(problems.is_empty(), "{problems:?}");
    }

    /// 嵌套对象也要递归合并：只修直接字段会让第二份样本的 `cfg.strength`
    /// 仍被首样本的 integer 限制拒绝。
    #[test]
    fn merges_nested_object_types_in_samples() {
        let schema = infer_schema(&json!([
            { "cfg": { "strength": 1 } },
            { "cfg": { "strength": 1.5, "enabled": true } }
        ]));
        let cfg = &schema["items"]["properties"]["cfg"];
        assert_eq!(cfg["properties"]["strength"]["type"], json!("number"));
        assert_eq!(cfg["properties"]["enabled"]["type"], json!("boolean"));

        let mut problems = Vec::new();
        validate_value(
            &schema,
            &json!([{ "cfg": { "strength": 1.5, "enabled": true } }]),
            "items",
            &mut problems,
        );
        assert!(problems.is_empty(), "{problems:?}");
    }

    /// 契约里的 schema 必须真正参与校验：类型不符、点号键非法 JSON 都要拦截。
    #[test]
    fn validates_against_the_contract_schema() {
        let graph = layout_graph();
        let contract = derive_contract(&graph, None, None);
        let mut param_map = HashMap::new();
        param_map.insert("layout".to_string(), vec!["20.layout".to_string()]);

        let mut wrong_type = Map::new();
        wrong_type.insert("layout".to_string(), json!(42));
        let problems = validate_params(&param_map, Some(&contract), Some(&wrong_type));
        assert!(
            problems.iter().any(|problem| problem.contains("must be object")),
            "{problems:?}"
        );

        let mut wrong_items = Map::new();
        wrong_items.insert("layout".to_string(), json!({ "items": "not an array" }));
        let problems = validate_params(&param_map, Some(&contract), Some(&wrong_items));
        assert!(
            problems.iter().any(|problem| problem.contains("layout.items")),
            "{problems:?}"
        );

        // 点号键同样使用 layout 的 schema，不能绕过 JSON 检查。
        let mut dotted = Map::new();
        dotted.insert("20.layout".to_string(), json!("{not json"));
        let problems = validate_params(&param_map, Some(&contract), Some(&dotted));
        assert!(
            problems.iter().any(|problem| problem.contains("valid JSON")),
            "{problems:?}"
        );

        // 等价写法 `20.inputs.layout` 必须命中同一份 schema。
        let mut canonical_dotted = Map::new();
        canonical_dotted.insert("20.inputs.layout".to_string(), json!("{not json"));
        let problems = validate_params(&param_map, Some(&contract), Some(&canonical_dotted));
        assert!(
            problems.iter().any(|problem| problem.contains("valid JSON")),
            "{problems:?}"
        );
    }
}
