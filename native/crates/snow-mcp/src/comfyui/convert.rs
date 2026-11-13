//! UI 格式画布工作流 → API 格式（`/prompt` 可提交）+ paramMap 自动推导。
//!
//! 为什么需要：`/userdata/workflows/*.json` 保存的是 ComfyUI 画布格式
//! （`nodes[] / links[]`），而 `POST /prompt` 只接受 API 格式
//! （`{ "节点 id": { "class_type", "inputs" } }`）。用户不该为此手写 JSON。
//!
//! 实机验证到的两种画布序列化（都用本模块处理）：
//!
//! | 形态 | links 元素 | node.inputs | 控件值 | node.type |
//! | --- | --- | --- | --- | --- |
//! | 现代（前端 1.5x） | `[link_id, origin, oslot, target, tslot, type]` | `{name, type, link}` | `widgets_values` | 类名 |
//! | 旧版/外部客户端 | `[origin, oslot, target, tslot, type]` | `[name, type, ...]`（下标即 tslot） | `properties` | 显示名 |
//!
//! 映射规则与 ComfyUI 前端 `graphToPrompt` 一致：
//! 1. widget 值按 `input_order`（权威声明顺序）依次赋给标量输入；
//! 2. `control_after_generate`（seed 后面的 randomize 控件）会占据一个额外的
//!    `widgets_values`，必须跳过，否则 steps / cfg 会整体错位；
//! 3. 有连线的输入用 `["上游节点 id", 输出槽位]` 覆盖 widget 值；
//! 4. `Reroute` 透传、`PrimitiveNode` 内联字面量、`Note/MarkdownNote` 丢弃。
//!
//! 任何无法确定的情况一律 **显式失败**（带节点 id 与原因），绝不静默产出错图。

use std::collections::{BTreeMap, HashMap, HashSet};

use indexmap::IndexMap;
use napi::bindgen_prelude::*;
use serde_json::{json, Map, Value};

/// 仅存在于画布、不属于执行图的节点类型。
const CLIENT_ONLY_NODES: &[&str] = &["Note", "MarkdownNote", "Reroute", "PrimitiveNode"];

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct NodeInputDef {
    #[serde(default)]
    required: IndexMap<String, Value>,
    #[serde(default)]
    optional: IndexMap<String, Value>,
}

/// `input_order` 是 ComfyUI 给出的权威输入顺序（数组，天然保序），
/// 比依赖 JSON 对象键顺序更稳。
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct InputOrder {
    #[serde(default)]
    required: Vec<String>,
    #[serde(default)]
    optional: Vec<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct NodeDef {
    #[serde(default)]
    input: Option<NodeInputDef>,
    #[serde(default)]
    input_order: Option<InputOrder>,
    /// 界面展示名（如 `Load Checkpoint`）。旧版序列化把它写进 `node.type`。
    /// 实机上部分自定义节点会给 `null`，因此必须容错。
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    output_node: Option<bool>,
}

impl NodeDef {
    fn display(&self) -> &str {
        self.display_name.as_deref().unwrap_or("").trim()
    }

    fn node_name(&self) -> &str {
        self.name.as_deref().unwrap_or("").trim()
    }

    fn is_output(&self) -> bool {
        self.output_node.unwrap_or(false)
    }
}

/// 转换结果里的节点摘要（供 UI 展示与映射核对）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeSummary {
    pub id: String,
    pub class_type: String,
    pub title: String,
    pub output_node: bool,
}

/// 一次转换的完整产物。
#[derive(Debug)]
pub struct ConvertedWorkflow {
    pub workflow: Value,
    pub param_map: BTreeMap<String, String>,
    pub warnings: Vec<String>,
    pub nodes: Vec<NodeSummary>,
}

impl ConvertedWorkflow {
    pub fn to_json(&self) -> Value {
        // 参数清单随转换结果一起返回：UI 可展示，模型走 imagegen-generate 的
        // 工具描述（该渠道）即可看到同一份信息，不需要额外的查询工具。
        let params = derive_workflow_params(&self.workflow);
        json!({
            "workflow": self.workflow,
            "workflowJson": self.workflow.to_string(),
            "paramMap": self.param_map,
            "paramMapJson": serde_json::to_string(&self.param_map).unwrap_or_else(|_| "{}".into()),
            "params": params,
            "paramsHint": format_workflow_params(&params, 12),
            "warnings": self.warnings,
            "nodes": self.nodes,
        })
    }
}

/// 连线索引：同时支持「按 link id」与「按 (target, tslot)」两种定位方式。
#[derive(Debug, Default)]
struct LinkIndex {
    by_id: HashMap<i64, (i64, i64)>,
    by_target: HashMap<(i64, i64), (i64, i64)>,
}

/// 把画布工作流转成 API 格式，并从连线结构里推导 paramMap。
pub fn convert_ui_to_api(ui: &Value, object_info_text: &str) -> Result<ConvertedWorkflow> {
    let defs: IndexMap<String, NodeDef> = serde_json::from_str(object_info_text).map_err(|error| {
        Error::from_reason(format!("Invalid ComfyUI /object_info payload: {error}"))
    })?;
    // 显示名 / name → 真实类名。旧版工作流的 `node.type` 存的是显示名。
    let mut aliases: HashMap<String, String> = HashMap::new();
    for (class, def) in &defs {
        if !def.display().is_empty() {
            aliases.insert(def.display().to_string(), class.clone());
        }
        if !def.node_name().is_empty() {
            aliases.insert(def.node_name().to_string(), class.clone());
        }
    }

    let nodes = ui
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::from_reason("Invalid workflow: missing `nodes` array (is this a UI-format workflow?)"))?;
    let empty_links: Vec<Value> = Vec::new();
    let links = ui
        .get("links")
        .and_then(Value::as_array)
        .unwrap_or(&empty_links);

    let link_index = build_link_index(links);
    let node_by_id = index_nodes(nodes);

    let mut warnings: Vec<String> = Vec::new();
    let mut included: HashSet<i64> = HashSet::new();
    let mut api: Map<String, Value> = Map::new();
    let mut summaries: Vec<NodeSummary> = Vec::new();

    // --- 第一遍：解析真实类名，决定哪些节点进执行图 ---
    for node in nodes {
        let Some(id) = node.get("id").and_then(Value::as_i64) else {
            continue;
        };
        let raw_type = node.get("type").and_then(Value::as_str).unwrap_or("").trim();
        if raw_type.is_empty() || CLIENT_ONLY_NODES.contains(&raw_type) {
            continue;
        }
        match node.get("mode").and_then(Value::as_u64) {
            // 2 = Never (muted)：不执行
            Some(2) => {
                warnings.push(format!("node {id} ({raw_type}) is muted and was skipped"));
                continue;
            }
            // 4 = Bypass：输入直通输出，静态转换无法可靠复现
            Some(4) => {
                return Err(Error::from_reason(format!(
                    "node {id} ({raw_type}) is bypassed. Remove the bypass in ComfyUI before using this workflow."
                )));
            }
            _ => {}
        }
        if !defs.contains_key(raw_type) && !aliases.contains_key(raw_type) {
            warnings.push(format!(
                "node {id} uses class `{raw_type}` which this ComfyUI does not report in /object_info; skipped (install the missing custom node if it should run)"
            ));
            continue;
        }
        included.insert(id);
    }

    // --- 第二遍：构建 API 图 ---
    for node in nodes {
        let Some(id) = node.get("id").and_then(Value::as_i64) else {
            continue;
        };
        if !included.contains(&id) {
            continue;
        }
        let raw_type = node
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let class_type = if defs.contains_key(&raw_type) {
            raw_type.clone()
        } else {
            aliases.get(&raw_type).cloned().unwrap_or(raw_type.clone())
        };
        let Some(def) = defs.get(&class_type) else {
            continue;
        };

        let mut inputs = map_widget_values(def, node, id, &class_type, &mut warnings);
        apply_links(
            node,
            id,
            &link_index,
            &node_by_id,
            &included,
            &mut inputs,
            &mut warnings,
        )?;

        let title = node
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if def.display().is_empty() {
                    class_type.clone()
                } else {
                    def.display().to_string()
                }
            });

        summaries.push(NodeSummary {
            id: id.to_string(),
            class_type: class_type.clone(),
            title: title.clone(),
            output_node: def.is_output(),
        });

        api.insert(
            id.to_string(),
            json!({
                "class_type": class_type,
                "_meta": { "title": title },
                "inputs": Value::Object(inputs),
            }),
        );
    }

    if api.is_empty() {
        return Err(Error::from_reason(
            "Converted workflow is empty: no executable nodes were found (all nodes are notes/reroutes or require missing custom nodes).",
        ));
    }

    let (param_map, derive_warnings) = derive_param_map(&api);
    warnings.extend(derive_warnings);

    Ok(ConvertedWorkflow {
        workflow: Value::Object(api),
        param_map,
        warnings,
        nodes: summaries,
    })
}

/// 两种 links 形态都索引起来：
/// - 6 段：`[link_id, origin, oslot, target, tslot, type]`
/// - 5 段：`[origin, oslot, target, tslot, type]`（旧版，无 link id）
fn build_link_index(links: &[Value]) -> LinkIndex {
    let mut index = LinkIndex::default();
    for link in links {
        if let Some(arr) = link.as_array() {
            match arr.len() {
                0..=4 => continue,
                5 => {
                    let origin = arr[0].as_i64().unwrap_or_default();
                    let oslot = arr[1].as_i64().unwrap_or_default();
                    let target = arr[2].as_i64().unwrap_or_default();
                    let tslot = arr[3].as_i64().unwrap_or_default();
                    index.by_target.insert((target, tslot), (origin, oslot));
                }
                _ => {
                    let id = arr[0].as_i64().unwrap_or_default();
                    let origin = arr[1].as_i64().unwrap_or_default();
                    let oslot = arr[2].as_i64().unwrap_or_default();
                    let target = arr[3].as_i64().unwrap_or_default();
                    let tslot = arr[4].as_i64().unwrap_or_default();
                    index.by_id.insert(id, (origin, oslot));
                    index.by_target.entry((target, tslot)).or_insert((origin, oslot));
                }
            }
            continue;
        }
        // 新版前端可能导出对象形态
        let Some(id) = link.get("id").and_then(Value::as_i64) else {
            continue;
        };
        let origin = link
            .get("origin_id")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let oslot = link
            .get("origin_slot")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        index.by_id.insert(id, (origin, oslot));
        if let (Some(target), Some(tslot)) = (
            link.get("target_id").and_then(Value::as_i64),
            link.get("target_slot").and_then(Value::as_i64),
        ) {
            index.by_target.entry((target, tslot)).or_insert((origin, oslot));
        }
    }
    index
}

fn index_nodes(nodes: &[Value]) -> HashMap<i64, &Value> {
    let mut map = HashMap::new();
    for node in nodes {
        if let Some(id) = node.get("id").and_then(Value::as_i64) {
            map.insert(id, node);
        }
    }
    map
}

/// 标量类型（含 COMBO 数组）是 widget，其余是连线输入。
fn is_widget(decl: &Value) -> bool {
    match decl.get(0) {
        Some(Value::Array(_)) => true,
        Some(Value::String(kind)) => matches!(kind.as_str(), "INT" | "FLOAT" | "STRING" | "BOOLEAN"),
        _ => false,
    }
}

/// seed 后面的 `control_after_generate` 会多占一个 widget 值。
fn skips_extra_widget(decl: &Value) -> bool {
    decl.get(1)
        .and_then(|opts| opts.get("control_after_generate"))
        .is_some()
}

/// 按权威顺序返回 `(输入名, 声明)`；优先 `input_order`，回退对象键顺序。
fn ordered_inputs(def: &NodeDef) -> Vec<(String, Value)> {
    let mut ordered: Vec<(String, Value)> = Vec::new();
    let order = def.input_order.clone().unwrap_or_default();
    let required = def
        .input
        .as_ref()
        .map(|input| input.required.clone())
        .unwrap_or_default();
    let optional = def
        .input
        .as_ref()
        .map(|input| input.optional.clone())
        .unwrap_or_default();

    if !order.required.is_empty() || !order.optional.is_empty() {
        for name in order.required.iter().chain(order.optional.iter()) {
            if let Some(decl) = required.get(name).or_else(|| optional.get(name)) {
                ordered.push((name.clone(), decl.clone()));
            }
        }
        if !ordered.is_empty() {
            return ordered;
        }
    }
    required
        .iter()
        .chain(optional.iter())
        .map(|(name, decl)| (name.clone(), decl.clone()))
        .collect()
}

fn map_widget_values(
    def: &NodeDef,
    node: &Value,
    node_id: i64,
    class_type: &str,
    warnings: &mut Vec<String>,
) -> Map<String, Value> {
    let mut inputs = Map::new();
    let ordered = ordered_inputs(def);

    // 来源优先级：widgets_values（现代）> properties（旧版，按输入名存放）
    let widgets = node.get("widgets_values");
    if let Some(Value::Object(mapped)) = widgets {
        for (name, value) in mapped {
            inputs.insert(name.clone(), value.clone());
        }
        return inputs;
    }
    if widgets.and_then(Value::as_array).is_none() {
        if let Some(Value::Object(props)) = node.get("properties") {
            for (name, value) in props {
                if ordered.iter().any(|(input, _)| input == name) {
                    inputs.insert(name.clone(), value.clone());
                }
            }
            if !inputs.is_empty() {
                return inputs;
            }
        }
    }

    let values: &[Value] = widgets
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    let mut expected = 0usize;
    let mut index = 0usize;
    for (name, decl) in &ordered {
        if !is_widget(decl) {
            continue;
        }
        expected += 1;
        if let Some(value) = values.get(index) {
            if value.is_null() {
                warnings.push(format!(
                    "node {node_id} ({class_type}) input `{name}` is empty; set it in ComfyUI or override it via params"
                ));
            }
            inputs.insert(name.clone(), value.clone());
        }
        index += 1;
        if skips_extra_widget(decl) {
            expected += 1;
            index += 1;
        }
    }

    if values.len() < expected {
        warnings.push(format!(
            "node {node_id} ({class_type}) declares {expected} widget values but the workflow carries {}; unset inputs keep ComfyUI defaults",
            values.len()
        ));
    } else if values.len() > expected {
        warnings.push(format!(
            "node {node_id} ({class_type}) carries {} widget values but only {expected} are declared; extra values were ignored",
            values.len()
        ));
    }

    inputs
}

fn apply_links(
    node: &Value,
    node_id: i64,
    link_index: &LinkIndex,
    node_by_id: &HashMap<i64, &Value>,
    included: &HashSet<i64>,
    inputs: &mut Map<String, Value>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let Some(list) = node.get("inputs").and_then(Value::as_array) else {
        return Ok(());
    };
    for (position, input) in list.iter().enumerate() {
        // 现代形态：{ name, type, link }；旧版形态：[ name, type, ... ]（下标 = target_slot）
        let (name, upstream) = match input {
            Value::Object(map) => {
                let name = map.get("name").and_then(Value::as_str).unwrap_or("");
                let upstream = map
                    .get("link")
                    .and_then(Value::as_i64)
                    .and_then(|id| link_index.by_id.get(&id).copied());
                (name, upstream)
            }
            Value::Array(arr) => {
                let name = arr.first().and_then(Value::as_str).unwrap_or("");
                let upstream = link_index
                    .by_target
                    .get(&(node_id, position as i64))
                    .copied();
                (name, upstream)
            }
            _ => continue,
        };
        if name.is_empty() {
            continue;
        }
        let Some((origin, slot)) = upstream else {
            continue;
        };
        match resolve_origin(origin, slot, link_index, node_by_id, 0)? {
            Resolved::Link(origin, slot) => {
                if !included.contains(&origin) {
                    let origin_class = node_by_id
                        .get(&origin)
                        .and_then(|node| node.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    return Err(Error::from_reason(format!(
                        "node {node_id} input `{name}` is wired to node {origin} ({origin_class}), which is not part of the executable graph (muted, note, or missing custom node). Fix the workflow in ComfyUI first."
                    )));
                }
                inputs.insert(name.to_string(), json!([origin.to_string(), slot]));
            }
            Resolved::Literal(value) => {
                inputs.insert(name.to_string(), value);
            }
        }
    }
    let _ = warnings;
    Ok(())
}

enum Resolved {
    Link(i64, i64),
    Literal(Value),
}

/// 解析一个上游节点：跳过 Reroute，把 PrimitiveNode 展开成字面量。
fn resolve_origin(
    origin: i64,
    slot: i64,
    link_index: &LinkIndex,
    node_by_id: &HashMap<i64, &Value>,
    depth: usize,
) -> Result<Resolved> {
    if depth > 16 {
        return Err(Error::from_reason(
            "Workflow link chain is too deep or cyclic (Reroute loop?)",
        ));
    }
    let Some(node) = node_by_id.get(&origin) else {
        return Err(Error::from_reason(format!(
            "Workflow link points to missing node {origin}"
        )));
    };
    match node.get("type").and_then(Value::as_str).unwrap_or("") {
        "PrimitiveNode" => {
            let value = node
                .get("widgets_values")
                .and_then(Value::as_array)
                .and_then(|values| values.first())
                .cloned()
                .or_else(|| {
                    node.get("properties")
                        .and_then(Value::as_object)
                        .and_then(|props| props.values().next().cloned())
                })
                .ok_or_else(|| {
                    Error::from_reason(format!("PrimitiveNode {origin} has no value to inline"))
                })?;
            Ok(Resolved::Literal(value))
        }
        "Reroute" => {
            let upstream = first_upstream(origin, link_index, node)?;
            let Some((next_origin, next_slot)) = upstream else {
                return Err(Error::from_reason(format!(
                    "Reroute node {origin} has no upstream link"
                )));
            };
            resolve_origin(next_origin, next_slot, link_index, node_by_id, depth + 1)
        }
        _ => Ok(Resolved::Link(origin, slot)),
    }
}

/// 取节点第一个输入的上游（两种 inputs 形态都支持）。
fn first_upstream(
    node_id: i64,
    link_index: &LinkIndex,
    node: &Value,
) -> Result<Option<(i64, i64)>> {
    let Some(list) = node.get("inputs").and_then(Value::as_array) else {
        return Ok(None);
    };
    for (position, input) in list.iter().enumerate() {
        match input {
            Value::Object(map) => {
                if let Some(id) = map.get("link").and_then(Value::as_i64) {
                    if let Some(found) = link_index.by_id.get(&id).copied() {
                        return Ok(Some(found));
                    }
                }
            }
            Value::Array(_) => {
                if let Some(found) = link_index
                    .by_target
                    .get(&(node_id, position as i64))
                    .copied()
                {
                    return Ok(Some(found));
                }
            }
            _ => {}
        }
    }
    Ok(None)
}

/// 从 API 图里推导语义参数落点（零配置的关键）。
fn derive_param_map(api: &Map<String, Value>) -> (BTreeMap<String, String>, Vec<String>) {
    let mut map = BTreeMap::new();
    let mut warnings = Vec::new();

    let sampler = api
        .iter()
        .find(|(_, node)| {
            node.get("class_type")
                .and_then(Value::as_str)
                .map(|class| class == "KSampler")
                .unwrap_or(false)
        })
        // 结构识别：采样器 = 同时吃 positive / negative / latent_image 的节点。
        // 不写死类名，自定义采样器（FLS_SamplerV4 之类）同样适用。
        .or_else(|| {
            api.iter().find(|(_, node)| {
                let Some(inputs) = node.get("inputs").and_then(Value::as_object) else {
                    return false;
                };
                let has = |name: &str| inputs.contains_key(name);
                has("positive") && has("negative") && (has("latent_image") || has("latent"))
            })
        })
        .or_else(|| {
            api.iter().find(|(_, node)| {
                node.get("class_type")
                    .and_then(Value::as_str)
                    .map(|class| class.contains("KSampler") || class == "SamplerCustom")
                    .unwrap_or(false)
            })
        });

    // 没有采样器时不要提前返回：放大/修复/背景处理类工作流仍然需要
    // `image` 与尺寸映射。只有 prompt/negative/seed 依赖采样器。
    let (sampler_id, sampler_node) = match sampler {
        Some((id, node)) => (Some(id), Some(node)),
        None => {
            warnings.push(
                "no sampler node (positive/negative/latent_image) found; prompt/seed mapping left empty — image and size are still derived".to_string(),
            );
            (None, None)
        }
    };

    for semantic in ["prompt", "negative"] {
        let Some(sampler_node) = sampler_node else {
            break;
        };
        let slot = if semantic == "prompt" {
            "positive"
        } else {
            "negative"
        };
        let Some(mut origin_id) = sampler_node
            .get("inputs")
            .and_then(|inputs| inputs.get(slot))
            .and_then(Value::as_array)
            .and_then(|pair| pair.first())
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        // 文本落点既不写死节点类型、也不写死节点 id：从采样器的正/负向输入出发，
        // 沿连线找到第一个**可写的**文本字段。连线输入不能覆盖 —— 布局板一类节点
        // 把上游文本转成 conditioning，真正的文本在它的上游。
        let mut found: Option<(String, String)> = None;
        for _ in 0..8 {
            let Some(target) = api.get(&origin_id) else {
                break;
            };
            let inputs = target.get("inputs");
            let mut next_origin: Option<String> = None;
            for name in ["text", "text_g", "text_l", "prompt"] {
                let Some(value) = inputs.and_then(|values| values.get(name)) else {
                    continue;
                };
                if value.is_string() {
                    found = Some((origin_id.clone(), name.to_string()));
                    break;
                }
                if next_origin.is_none() {
                    next_origin = value
                        .as_array()
                        .and_then(|pair| pair.first())
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }
            if found.is_some() {
                break;
            }
            match next_origin {
                Some(next) => origin_id = next,
                None => break,
            }
        }
        match found {
            Some((node_id, key)) => {
                // 负向语义优先使用同节点的专用负向字段：Qwen 一类编码器
                // 同时有 `prompt` / `negative_prompt` 时，正负向若都落到
                // `prompt`，后写入的负面词会把正面词覆盖掉。
                let key = if semantic == "negative" {
                    negative_text_field(api, &node_id).unwrap_or(key)
                } else {
                    key
                };
                let path = format!("{node_id}.{key}");
                if semantic == "negative" && map.get("prompt") == Some(&path) {
                    warnings.push(format!(
                        "prompt and negative both resolve to `{path}`; negative is not injected (add an explicit paramMap entry)"
                    ));
                } else {
                    map.insert(semantic.to_string(), path);
                }
            }
            None => warnings.push(format!(
                "could not locate a writable text input for `{semantic}` along the {slot} chain"
            )),
        }
    }

    if let (Some(sampler_id), Some(sampler_node)) = (sampler_id, sampler_node) {
        for key in ["seed", "noise_seed"] {
            if sampler_node
                .get("inputs")
                .and_then(|inputs| inputs.get(key))
                .is_some()
            {
                map.insert("seed".to_string(), format!("{sampler_id}.{key}"));
                break;
            }
        }
        // 标准采样器的 steps / cfg 也要自动发现：否则普通语义请求仍需手写映射，
        // CFG=1 告警也无法在零配置图上触发。
        for key in ["steps", "cfg"] {
            if sampler_node
                .get("inputs")
                .and_then(|inputs| inputs.get(key))
                .is_some()
            {
                map.insert(key.to_string(), format!("{sampler_id}.{key}"));
            }
        }
    }

    // 尺寸：找 Empty*LatentImage 一类节点
    if let Some((latent_id, latent_node)) = api.iter().find(|(_, node)| {
        let class = node
            .get("class_type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        class.starts_with("empty") && class.contains("latent")
    }) {
        let inputs = latent_node.get("inputs");
        if inputs.and_then(|i| i.get("width")).is_some()
            && inputs.and_then(|i| i.get("height")).is_some()
        {
            map.insert("width".to_string(), format!("{latent_id}.width"));
            map.insert("height".to_string(), format!("{latent_id}.height"));
        }
    }
    if !map.contains_key("width") {
        warnings.push("no Empty*LatentImage node found; size is not injected".to_string());
    }

    // 图生图输入：优先 Autogrow 的 `images.image_N`，其次多个 LoadImage。
    // 索引从 1 开始：`image` 是第一张，`image2` 是第二张……
    //
    // Autogrow 的槽位本身是 **IMAGE 连线**（`["5", 0]`），不是文件名。
    // 上传后的文件名必须写到上游 LoadImage 的 `image` 输入；直接把槽位改成
    // 字符串会破坏连线（模型收不到图像，甚至提交失败）。
    let mut autogrow_slots: Vec<(usize, String)> = Vec::new();
    let mut saw_autogrow = false;
    for (node_id, node) in api {
        let Some(inputs) = node.get("inputs").and_then(Value::as_object) else {
            continue;
        };
        for key in inputs.keys() {
            let Some(index) = key
                .strip_prefix("images.image_")
                .and_then(|rest| rest.parse::<usize>().ok())
            else {
                continue;
            };
            saw_autogrow = true;
            // 多个节点都声明同一序号时只保留第一个：无法可靠判断归属，
            // 剩下的交给显式 paramMap。
            if autogrow_slots.iter().any(|(used, _)| *used == index) {
                continue;
            }
            let Some(pair) = inputs.get(key).and_then(Value::as_array) else {
                warnings.push(format!(
                    "`{node_id}.{key}` is not an image link; reference image slot skipped (map it explicitly)"
                ));
                continue;
            };
            let Some(target) = pair.first().and_then(Value::as_str) else {
                continue;
            };
            let Some(target_node) = api.get(target) else {
                warnings.push(format!(
                    "`{node_id}.{key}` points at missing node {target}; reference image slot skipped"
                ));
                continue;
            };
            let target_class = target_node
                .get("class_type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            if !target_class.contains("LoadImage") {
                warnings.push(format!(
                    "`{node_id}.{key}` links to {target} ({target_class}), not a LoadImage; map the reference image explicitly"
                ));
                continue;
            }
            autogrow_slots.push((index, format!("{target}.image")));
        }
    }
    autogrow_slots.sort_by_key(|(index, _)| *index);
    if !autogrow_slots.is_empty() {
        // 保留原始槽位编号：image_2 成功而 image_1 失败时，不能把第二槽
        // 压缩成第一张，否则请求的第一张参考图会被写到第二槽。
        for (index, path) in autogrow_slots {
            map.insert(semantic_image_name(index.saturating_sub(1)), path);
        }
    } else if saw_autogrow {
        // 存在 Autogrow 槽位但无法解析到 LoadImage：绝不能退回「扫描所有
        // LoadImage 按顺序猜」——那会把参考图写进无关控制图。
        warnings.push(
            "autogrow image slots exist but none resolve to a LoadImage; map reference images explicitly"
                .to_string(),
        );
    } else {
        let mut loaders: Vec<(i64, String)> = api
            .iter()
            .filter(|(_, node)| {
                node.get("class_type")
                    .and_then(Value::as_str)
                    .map(|class| class.contains("LoadImage"))
                    .unwrap_or(false)
            })
            .map(|(id, _)| (id.parse::<i64>().unwrap_or(i64::MAX), id.clone()))
            .collect();
        loaders.sort_by_key(|(order, _)| *order);
        for (position, (_, id)) in loaders.into_iter().enumerate() {
            map.insert(semantic_image_name(position), format!("{id}.image"));
        }
    }

    (map, warnings)
}

/// 第 `position` 张参考图的语义名：`image`、`image2`、`image3`…
fn semantic_image_name(position: usize) -> String {
    if position == 0 {
        "image".to_string()
    } else {
        format!("image{}", position + 1)
    }
}

/// 同一节点上可写的负向文本字段（找不到则 None）。
fn negative_text_field(api: &Map<String, Value>, node_id: &str) -> Option<String> {
    let inputs = api.get(node_id)?.get("inputs")?.as_object()?;
    ["negative_prompt", "negative_text", "negative"]
        .iter()
        .find(|name| inputs.get(**name).map(Value::is_string).unwrap_or(false))
        .map(|name| (*name).to_string())
}

/// 从 API 格式图直接推导语义参数落点（不依赖适配器 meta —— 原始材料优先）。
///
/// 生产代码统一走 [`semantic_param_map`]（多落点 + 布局板）；这个单落点版本
/// 只用于测试对照，因此仅在测试构建中编译。
#[cfg(test)]
pub fn param_map_for_graph(graph: &Value) -> BTreeMap<String, String> {
    match graph.as_object() {
        Some(nodes) => derive_param_map(nodes).0,
        None => BTreeMap::new(),
    }
}

/// 运行时参数映射：在 `param_map_for_graph` 之上补齐「一语义多落点」与布局板参数。
///
/// 与 `param_map_for_graph` 的区别只在**多路径**：
/// - 布局板（任何带 `layout` 输入的节点）的 `layout` / `strict` / `character_boxes`；
/// - 尺寸要同时写进布局板，否则 `size` 只改了 latent，布局文档里的宽高会和它对不上。
///
/// 全部按输入名发现，不含任何写死的节点 id。
pub fn semantic_param_map(graph: &Value) -> BTreeMap<String, Vec<String>> {
    let Some(nodes) = graph.as_object() else {
        return BTreeMap::new();
    };
    let mut map: BTreeMap<String, Vec<String>> = derive_param_map(nodes)
        .0
        .into_iter()
        .map(|(name, path)| (name, vec![path]))
        .collect();

    let board = nodes.iter().find(|(_, node)| {
        node.get("inputs")
            .and_then(|inputs| inputs.get("layout"))
            .is_some()
    });
    let Some((board_id, board_node)) = board else {
        return map;
    };
    let inputs = board_node.get("inputs");
    map.insert("layout".to_string(), vec![format!("{board_id}.layout")]);
    for name in ["strict", "character_boxes"] {
        if inputs.and_then(|values| values.get(name)).is_some() {
            map.insert(name.to_string(), vec![format!("{board_id}.{name}")]);
        }
    }
    for name in ["width", "height"] {
        if inputs.and_then(|values| values.get(name)).is_none() {
            continue;
        }
        let path = format!("{board_id}.{name}");
        let paths = map.entry(name.to_string()).or_default();
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    map
}

/// 工作流里「值得暴露给模型」的标量输入（按重要性排序）。
/// 模型只需要调这些；连线输入（model / latent / conditioning…）由工作流自己决定。
const PARAM_PRIORITY: &[&str] = &[
    "steps",
    "cfg",
    "sampler_name",
    "scheduler",
    "denoise",
    "seed",
    "noise_seed",
    "width",
    "height",
    "batch_size",
    "ckpt_name",
    "lora_name",
    "strength_model",
    "strength_clip",
    "vae_name",
    "control_net_name",
    "model_name",
    "scale_by",
    "upscale_method",
];

/// 不暴露的输入：
/// - 文本类已由 prompt / negative 语义参数负责；
/// - `image` 由参考图上传负责（模型不该填 ComfyUI 内部文件名）；
/// - 其余是连线输入或纯输出配置。
const PARAM_SKIP: &[&str] = &[
    "text",
    "text_g",
    "text_l",
    "clip",
    "model",
    "vae",
    "positive",
    "negative",
    "latent_image",
    "samples",
    "images",
    "mask",
    "image",
    "pixels",
    "conditioning",
    "control_net",
    "filename_prefix",
    "clip_name",
];

/// 一个可从 `params` 覆盖的工作流标量输入。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowParam {
    /// 点号键，如 `3.steps`
    pub key: String,
    pub class_type: String,
    pub input: String,
    /// INT / FLOAT / STRING / BOOLEAN
    pub value_type: String,
    /// 工作流里的当前值（用于告诉模型「不传就是它」）
    pub current: Value,
}

/// 从 API 格式工作流里推导可覆盖参数（纯本地，不发请求）。
pub fn derive_workflow_params(workflow: &Value) -> Vec<WorkflowParam> {
    let mut params: Vec<WorkflowParam> = Vec::new();
    let Some(nodes) = workflow.as_object() else {
        return params;
    };
    for (node_id, node) in nodes {
        let class_type = node
            .get("class_type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let Some(inputs) = node.get("inputs").and_then(Value::as_object) else {
            continue;
        };
        for (name, value) in inputs {
            // 连线输入是 ["节点", 槽位]，不是可覆盖标量
            if value.is_array() {
                continue;
            }
            if PARAM_SKIP.contains(&name.as_str()) {
                continue;
            }
            // 空值也要暴露：`lora_name: null` 正是需要模型/用户补上的字段。
            // 未设置的可选框在 ComfyUI 里基本都是字符串（文件名/枚举名），
            // 因此按 STRING 呈现，当前值给空串。
            let (value_type, current) = match value {
                Value::Null => ("STRING", Value::String(String::new())),
                Value::Bool(_) => ("BOOLEAN", value.clone()),
                Value::Number(number) if number.is_i64() || number.is_u64() => {
                    ("INT", value.clone())
                }
                Value::Number(_) => ("FLOAT", value.clone()),
                Value::String(_) => ("STRING", value.clone()),
                _ => continue,
            };
            params.push(WorkflowParam {
                key: format!("{node_id}.{name}"),
                class_type: class_type.clone(),
                input: name.clone(),
                value_type: value_type.to_string(),
                current,
            });
        }
    }
    params.sort_by(|a, b| {
        let rank = |input: &str| {
            PARAM_PRIORITY
                .iter()
                .position(|name| *name == input)
                .unwrap_or(PARAM_PRIORITY.len())
        };
        rank(&a.input)
            .cmp(&rank(&b.input))
            .then_with(|| a.key.cmp(&b.key))
    });
    params
}

fn short_value(value: &Value) -> String {
    match value {
        Value::String(text) => {
            // 字符串形态的 JSON（layout / character_boxes）输出结构摘要，
            // 而不是把 40 个字符的 JSON 开头丢给模型去猜。
            if let Some(summary) = super::contract::summarize_json_text(text) {
                let truncated: String = summary.chars().take(420).collect();
                if summary.chars().count() > 420 {
                    return format!("{truncated}…");
                }
                return summary;
            }
            let truncated: String = text.chars().take(40).collect();
            if text.chars().count() > 40 {
                format!("\"{truncated}…\"")
            } else {
                format!("\"{truncated}\"")
            }
        }
        other => other.to_string(),
    }
}

/// 把参数清单压成一行，供工具描述注入（模型直接看到「能调什么、默认值是多少」）。
pub fn format_workflow_params(params: &[WorkflowParam], max: usize) -> Option<String> {
    if params.is_empty() {
        return None;
    }
    let parts: Vec<String> = params
        .iter()
        .take(max)
        .map(|param| {
            format!(
                "`{}` ({}, now {})",
                param.key,
                param.value_type,
                short_value(&param.current)
            )
        })
        .collect();
    let mut text = parts.join(" | ");
    if params.len() > max {
        text.push_str(&format!(" … +{} more", params.len() - max));
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 最小可运行的 object_info：`input_order` 是权威顺序，
    /// KSampler 的 seed 带 `control_after_generate`（会多占一个 widget 值）。
    const OBJECT_INFO: &str = r#"{
      "CheckpointLoaderSimple": {
        "input": { "required": { "ckpt_name": [["model.safetensors"]] } },
        "input_order": { "required": ["ckpt_name"] },
        "name": "CheckpointLoaderSimple",
        "display_name": "Load Checkpoint",
        "output_node": false
      },
      "CLIPTextEncode": {
        "input": { "required": {
          "text": ["STRING", {"multiline": true}],
          "clip": ["CLIP"]
        } },
        "input_order": { "required": ["text", "clip"] },
        "name": "CLIPTextEncode",
        "display_name": "CLIP Text Encode (Prompt)",
        "output_node": false
      },
      "KSampler": {
        "input": { "required": {
          "model": ["MODEL"],
          "seed": ["INT", {"default": 0, "control_after_generate": true}],
          "steps": ["INT", {"default": 20}],
          "cfg": ["FLOAT", {"default": 8.0}],
          "sampler_name": [["euler", "dpmpp_2m"]],
          "scheduler": [["normal", "karras"]],
          "positive": ["CONDITIONING"],
          "negative": ["CONDITIONING"],
          "latent_image": ["LATENT"],
          "denoise": ["FLOAT", {"default": 1.0}]
        } },
        "input_order": { "required": ["model","seed","steps","cfg","sampler_name","scheduler","positive","negative","latent_image","denoise"] },
        "name": "KSampler",
        "display_name": "KSampler",
        "output_node": false
      },
      "EmptyLatentImage": {
        "input": { "required": {
          "width": ["INT", {"default": 512}],
          "height": ["INT", {"default": 512}],
          "batch_size": ["INT", {"default": 1}]
        } },
        "input_order": { "required": ["width", "height", "batch_size"] },
        "name": "EmptyLatentImage",
        "display_name": "Empty Latent Image",
        "output_node": false
      },
      "VAEDecode": {
        "input": { "required": { "samples": ["LATENT"], "vae": ["VAE"] } },
        "input_order": { "required": ["samples", "vae"] },
        "name": "VAEDecode",
        "display_name": "VAE Decode",
        "output_node": false
      },
      "SaveImage": {
        "input": { "required": { "images": ["IMAGE"], "filename_prefix": ["STRING"] } },
        "input_order": { "required": ["images", "filename_prefix"] },
        "name": "SaveImage",
        "display_name": "Save Image",
        "output_node": true
      }
    }"#;

    /// 现代画布格式：links 带 link_id，inputs 是对象，控件值在 widgets_values。
    const UI_MODERN: &str = r#"{
      "last_node_id": 12,
      "last_link_id": 8,
      "nodes": [
        { "id": 4, "type": "CheckpointLoaderSimple", "widgets_values": ["model.safetensors"], "inputs": [] },
        { "id": 6, "type": "CLIPTextEncode", "widgets_values": ["a photo of a cat"], "inputs": [{"name":"clip","link":2}] },
        { "id": 7, "type": "CLIPTextEncode", "widgets_values": ["blurry"], "inputs": [{"name":"clip","link":3}] },
        { "id": 5, "type": "EmptyLatentImage", "widgets_values": [512, 768, 1], "inputs": [] },
        { "id": 3, "type": "KSampler", "widgets_values": [123456, "randomize", 20, 7.5, "euler", "normal", 1.0],
          "inputs": [
            {"name":"model","link":1},
            {"name":"positive","link":4},
            {"name":"negative","link":5},
            {"name":"latent_image","link":6}
          ] },
        { "id": 10, "type": "Reroute", "inputs": [{"name":"","link":7}], "widgets_values": [] },
        { "id": 11, "type": "PrimitiveNode", "widgets_values": [42], "inputs": [] },
        { "id": 8, "type": "VAEDecode", "inputs": [{"name":"samples","link":8},{"name":"vae","link":null}] },
        { "id": 9, "type": "SaveImage", "widgets_values": ["ComfyUI"], "inputs": [{"name":"images","link":null}] },
        { "id": 12, "type": "Note", "widgets_values": ["说明文字"], "inputs": [] }
      ],
      "links": [
        [1, 4, 0, 3, 0, "MODEL"],
        [2, 4, 1, 6, 1, "CLIP"],
        [3, 4, 1, 7, 1, "CLIP"],
        [4, 6, 0, 3, 1, "CONDITIONING"],
        [5, 7, 0, 3, 2, "CONDITIONING"],
        [6, 5, 0, 3, 3, "LATENT"],
        [7, 3, 0, 10, 0, "LATENT"],
        [8, 10, 0, 8, 0, "LATENT"]
      ]
    }"#;

    /// 旧版/外部客户端格式：links 无 link_id，inputs 是数组，控件值在 properties，
    /// type 用显示名（取自实机「哈基米」工作流的真实形状）。
    const UI_LEGACY: &str = r#"{
      "nodes": [
        { "id": 1, "type": "Load Checkpoint", "inputs": [["vae","VAE","vae"]], "outputs": [], "properties": {"ckpt_name":"sd_v1-5_vae.ckpt"} },
        { "id": 2, "type": "CLIP Text Encode (Prompt)", "inputs": [["clip","CLIP","clip"]], "properties": {"text":"masterpiece, 1girl"} },
        { "id": 3, "type": "CLIP Text Encode (Prompt)", "inputs": [["clip","CLIP","clip"]], "properties": {"text":"lowres, blurry"} },
        { "id": 4, "type": "Empty Latent Image", "inputs": [], "properties": {"width":512,"height":512,"batch_size":1} },
        { "id": 5, "type": "KSampler",
          "inputs": [["model","MODEL","model"],["seed","INT","seed"],["steps","INT","steps"],["cfg","FLOAT","cfg"],["sampler_name","SAMPLER","sampler_name"],["scheduler","SCHEDULER","scheduler"],["positive","CONDITIONING","conditioning"],["negative","CONDITIONING","conditioning"],["latent_image","LATENT","latent"]],
          "outputs": [["LATENT","LATENT","latent"]],
          "properties": {"seed":42,"steps":20,"cfg":8,"sampler_name":"euler_a","scheduler":"normal"} },
        { "id": 6, "type": "VAE Decode", "inputs": [["vae","VAE","vae"],["samples","LATENT","latent"]] },
        { "id": 7, "type": "Save Image", "inputs": [["images","IMAGE","images"]], "properties": {"filename_prefix":"ComfyUI"} }
      ],
      "links": [
        [1, 0, 5, 0, "MODEL"],
        [1, 1, 2, 0, "CLIP"],
        [1, 1, 3, 0, "CLIP"],
        [1, 2, 6, 0, "VAE"],
        [2, 0, 5, 6, "CONDITIONING"],
        [3, 0, 5, 7, "CONDITIONING"],
        [4, 0, 5, 8, "LATENT"],
        [5, 0, 6, 1, "LATENT"],
        [6, 0, 7, 0, "IMAGE"]
      ]
    }"#;

    fn convert(ui_json: &str) -> ConvertedWorkflow {
        let ui: Value = serde_json::from_str(ui_json).unwrap();
        convert_ui_to_api(&ui, OBJECT_INFO).unwrap()
    }

    #[test]
    fn maps_widgets_in_declared_order_and_skips_control_after_generate() {
        let converted = convert(UI_MODERN);
        let sampler = &converted.workflow["3"]["inputs"];
        // seed 之后的 "randomize" 必须被跳过，否则 steps 会拿到 "randomize"
        assert_eq!(sampler["seed"], json!(123456));
        assert_eq!(sampler["steps"], json!(20));
        assert_eq!(sampler["cfg"], json!(7.5));
        assert_eq!(sampler["sampler_name"], json!("euler"));
        assert_eq!(sampler["scheduler"], json!("normal"));
        assert_eq!(sampler["denoise"], json!(1.0));
    }

    #[test]
    fn resolves_links_reroute_and_primitive_nodes() {
        let converted = convert(UI_MODERN);
        assert_eq!(converted.workflow["3"]["inputs"]["model"], json!(["4", 0]));
        assert_eq!(converted.workflow["8"]["inputs"]["samples"], json!(["3", 0]));
        assert!(converted.workflow.get("10").is_none());
        assert!(converted.workflow.get("11").is_none());
        assert!(converted.workflow.get("12").is_none());
    }

    #[test]
    fn derives_param_map_from_graph() {
        let converted = convert(UI_MODERN);
        assert_eq!(converted.param_map.get("prompt").map(String::as_str), Some("6.text"));
        assert_eq!(converted.param_map.get("negative").map(String::as_str), Some("7.text"));
        assert_eq!(converted.param_map.get("seed").map(String::as_str), Some("3.seed"));
        assert_eq!(converted.param_map.get("width").map(String::as_str), Some("5.width"));
        assert_eq!(converted.param_map.get("height").map(String::as_str), Some("5.height"));
        assert!(!converted.param_map.contains_key("image"));
    }

    /// 自动推导必须等价于 anima 渠道此前**手写**的那份映射；
    /// 这条测试就是「删掉死编码也没损失」的证据。
    #[test]
    fn derives_the_same_map_as_the_old_handwritten_anima_config() {
        let graph: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/anima-t2i.api.json"
        )))
        .unwrap();
        let map = param_map_for_graph(&graph);
        let get = |name: &str| map.get(name).map(String::as_str).unwrap_or("");
        assert_eq!(get("prompt"), "11.text", "map: {map:?}");
        assert_eq!(get("negative"), "12.text", "map: {map:?}");
        assert_eq!(get("seed"), "19.seed", "map: {map:?}");
        assert_eq!(get("width"), "28.width", "map: {map:?}");
        assert_eq!(get("height"), "28.height", "map: {map:?}");
    }

    /// 动态推导必须复原 MOSS 布局渠道曾经手写的那套映射：
    /// prompt 落在布局板的 `prompt`（因为正向链路上 `CLIPTextEncode.text` 是连线），
    /// 并且 `layout` 与「尺寸双写」都要自动发现。
    #[test]
    fn derives_moss_layout_semantics_from_the_graph() {
        let graph: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/moss-image-layout.api.json"
        )))
        .unwrap();
        let map = semantic_param_map(&graph);
        let paths = |name: &str| map.get(name).cloned().unwrap_or_default();

        let prompt = paths("prompt");
        assert_eq!(prompt.len(), 1, "prompt: {map:?}");
        assert!(prompt[0].ends_with(".prompt"), "prompt: {map:?}");
        assert_eq!(paths("layout").len(), 1, "layout: {map:?}");
        assert!(paths("layout")[0].ends_with(".layout"), "layout: {map:?}");
        // 尺寸要同时写进 latent 与布局板，否则布局文档和实际输出会不自洽。
        assert_eq!(paths("width").len(), 2, "width: {map:?}");
        assert_eq!(paths("height").len(), 2, "height: {map:?}");
        assert_eq!(paths("seed").len(), 1, "seed: {map:?}");
        assert!(!map.contains_key("image"), "MOSS t2i 不应有 image 落点");
    }

    #[test]
    fn converts_legacy_display_name_format() {
        let converted = convert(UI_LEGACY);
        // 显示名必须解析回真实类名
        assert_eq!(converted.workflow["1"]["class_type"], json!("CheckpointLoaderSimple"));
        assert_eq!(converted.workflow["6"]["class_type"], json!("VAEDecode"));
        assert_eq!(converted.workflow["7"]["class_type"], json!("SaveImage"));
        // properties 作为控件值来源
        assert_eq!(converted.workflow["5"]["inputs"]["steps"], json!(20));
        assert_eq!(converted.workflow["5"]["inputs"]["cfg"], json!(8));
        assert_eq!(converted.workflow["4"]["inputs"]["height"], json!(512));
        // 旧版 links（无 id）按 (target, tslot) 解析
        assert_eq!(converted.workflow["5"]["inputs"]["model"], json!(["1", 0]));
        assert_eq!(converted.workflow["6"]["inputs"]["samples"], json!(["5", 0]));
        assert_eq!(converted.param_map.get("prompt").map(String::as_str), Some("2.text"));
        assert_eq!(converted.param_map.get("negative").map(String::as_str), Some("3.text"));
        assert_eq!(converted.param_map.get("seed").map(String::as_str), Some("5.seed"));
    }

    #[test]
    fn rejects_bypassed_nodes() {
        let mut ui: Value = serde_json::from_str(UI_MODERN).unwrap();
        ui["nodes"][4]["mode"] = json!(4);
        let error = convert_ui_to_api(&ui, OBJECT_INFO).unwrap_err();
        assert!(error.to_string().contains("bypassed"), "unexpected: {error}");
    }

    /// 实机回归：本机 ComfyUI v0.43 导出的真实工作流（SD1.5 + LoRA，8 节点）。
    #[test]
    fn converts_real_sd15_workflow_from_disk() {
        let ui_text = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/ui-sd15-t2i-lora.json"
        ));
        let object_info = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/object-info-sd15-t2i-lora.json"
        ));
        let ui: Value = serde_json::from_str(ui_text).unwrap();
        let converted = convert_ui_to_api(&ui, object_info).unwrap();

        // 采样器控件的顺序映射（seed 后的 "randomize" 必须被跳过）
        let sampler = &converted.workflow["3"]["inputs"];
        assert_eq!(sampler["seed"], json!(851616030077673u64));
        assert_eq!(sampler["steps"], json!(20));
        assert_eq!(sampler["cfg"], json!(8));
        assert_eq!(sampler["sampler_name"], json!("euler"));
        assert_eq!(sampler["scheduler"], json!("normal"));

        // LoRA / Latent / 编码器节点都在，并且连线被正确解析
        assert_eq!(converted.workflow["6"]["class_type"], json!("CLIPTextEncode"));
        // 该工作流里的 LoRA 选择为空（widgets_values = [null, 1, 1]），
        // 转换器必须保留键并给出可读告警，而不是静默丢弃。
        assert!(converted.workflow["10"]["inputs"]
            .get("lora_name")
            .is_some());
        assert_eq!(converted.workflow["10"]["inputs"]["strength_model"], json!(1));
        assert!(converted
            .warnings
            .iter()
            .any(|warning| warning.contains("lora_name")));
        assert_eq!(converted.workflow["5"]["inputs"]["width"], json!(512));
        assert_eq!(converted.workflow["5"]["inputs"]["height"], json!(512));
        assert_eq!(converted.workflow["3"]["inputs"]["model"], json!(["10", 0]));

        // paramMap 自动推导：prompt 指向 CLIPTextEncode，且与 negative 不是同一个节点
        let prompt = converted.param_map.get("prompt").expect("prompt mapped");
        let negative = converted
            .param_map
            .get("negative")
            .expect("negative mapped");
        assert!(prompt.ends_with(".text"), "unexpected prompt path: {prompt}");
        assert!(negative.ends_with(".text"), "unexpected negative path: {negative}");
        assert_ne!(prompt, negative);
        assert_eq!(converted.param_map.get("seed").map(String::as_str), Some("3.seed"));
        assert_eq!(converted.param_map.get("width").map(String::as_str), Some("5.width"));
        assert_eq!(converted.param_map.get("height").map(String::as_str), Some("5.height"));
    }

    /// 工作流自描述：模型只需要知道「这个工作流能调什么」，不需要额外的查询工具。
    #[test]
    fn derives_tunables_from_a_real_workflow() {
        let ui_text = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/ui-sd15-t2i-lora.json"
        ));
        let object_info = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/object-info-sd15-t2i-lora.json"
        ));
        let ui: Value = serde_json::from_str(ui_text).unwrap();
        let converted = convert_ui_to_api(&ui, object_info).unwrap();

        let params = derive_workflow_params(&converted.workflow);
        let keys: Vec<&str> = params.iter().map(|param| param.key.as_str()).collect();

        // 采样器参数排在最前（优先级顺序），且当前值可读
        assert_eq!(keys.first().copied(), Some("3.steps"), "{keys:?}");
        assert!(keys.contains(&"3.cfg"), "{keys:?}");
        assert!(keys.contains(&"3.sampler_name"), "{keys:?}");
        assert!(keys.contains(&"10.lora_name"), "{keys:?}");
        // 文本输入由 prompt/negative 负责，不重复暴露
        assert!(!keys.iter().any(|key| key.ends_with(".text")), "{keys:?}");

        let hint = format_workflow_params(&params, 12).expect("hint");
        assert!(hint.contains("`3.steps` (INT, now 20)"), "{hint}");
        assert!(hint.contains("`3.cfg`"), "{hint}");
        // 空 LoRA 也要如实告诉模型（而不是隐藏）
        assert!(hint.contains("`10.lora_name`"), "{hint}");
    }

    /// 同一编码器同时有 prompt / negative_prompt 时，负面词不能覆盖正面词。
    #[test]
    fn negative_does_not_overwrite_prompt_when_shared_encoder() {
        let graph = json!({
            "1": { "class_type": "Loader", "inputs": {} },
            "7": { "class_type": "TextEncodeQwenImage21", "inputs": {
                "prompt": "",
                "negative_prompt": "",
                "clip": ["1", 0]
            } },
            "3": { "class_type": "KSampler", "inputs": {
                "model": ["1", 0], "seed": 1,
                "positive": ["7", 0], "negative": ["7", 1],
                "latent_image": ["1", 2]
            } }
        });
        let map = semantic_param_map(&graph);
        assert_eq!(map.get("prompt"), Some(&vec!["7.prompt".to_string()]));
        assert_eq!(
            map.get("negative"),
            Some(&vec!["7.negative_prompt".to_string()]),
            "negative must use its own field instead of overwriting prompt: {map:?}"
        );
    }

    /// Autogrow 多图：槽位是 IMAGE 连线，必须追到上游 LoadImage 的 `image`
    /// 输入。直接写槽位会把连线替换成文件名，模型收不到图像。
    #[test]
    fn maps_autogrow_images_to_their_upstream_load_image() {
        let graph = json!({
            "5": { "class_type": "LoadImage", "inputs": { "image": "a.png" } },
            "6": { "class_type": "LoadImage", "inputs": { "image": "b.png" } },
            "7": { "class_type": "TextEncodeQwenImage21", "inputs": {
                "images.image_1": ["5", 0],
                "images.image_2": ["6", 0],
                "clip": ["5", 1]
            } }
        });
        let map = semantic_param_map(&graph);
        assert_eq!(map.get("image"), Some(&vec!["5.image".to_string()]));
        assert_eq!(map.get("image2"), Some(&vec!["6.image".to_string()]));
    }

    /// 槽位编号与节点 ID 顺序相反时，必须按槽位（连线证据）映射，不能按
    /// 节点 ID 升序猜；图中无关的 LoadImage 也不得占用参考图槽位。
    ///
    /// 槽位 2 故意指向更大的节点 4、无关节点是更小的 2：只按「剩余最小 ID」
    /// 分配的实现会得到 image2=2.image，从而被这条断言抓住。
    #[test]
    fn maps_autogrow_slots_by_slot_not_node_id_order() {
        let graph = json!({
            "9": { "class_type": "LoadImage", "inputs": { "image": "slot1.png" } },
            "4": { "class_type": "LoadImage", "inputs": { "image": "slot2.png" } },
            "2": { "class_type": "LoadImage", "inputs": { "image": "unrelated.png" } },
            "7": { "class_type": "TextEncodeQwenImage21", "inputs": {
                "images.image_1": ["9", 0],
                "images.image_2": ["4", 0],
                "clip": ["5", 1]
            } }
        });
        let map = semantic_param_map(&graph);
        assert_eq!(
            map.get("image"),
            Some(&vec!["9.image".to_string()]),
            "image must follow slot 1 (node 9), not the lowest node id: {map:?}"
        );
        assert_eq!(
            map.get("image2"),
            Some(&vec!["4.image".to_string()]),
            "image2 must follow slot 2 (node 4), not the smallest remaining id: {map:?}"
        );
        assert!(
            !map.values().flatten().any(|path| path.starts_with("2.")),
            "unrelated LoadImage must not be bound to a reference slot: {map:?}"
        );
    }

    /// 部分 Autogrow 槽位解析失败时，不能把后面的成功项压缩成第一张。
    #[test]
    fn keeps_autogrow_slot_numbers_when_one_slot_fails() {
        let graph = json!({
            "5": { "class_type": "LoadImage", "inputs": { "image": "a.png" } },
            "6": { "class_type": "LoadImage", "inputs": { "image": "b.png" } },
            "8": { "class_type": "ImageResize", "inputs": { "image": ["5", 0] } },
            "7": { "class_type": "TextEncodeQwenImage21", "inputs": {
                "images.image_1": ["8", 0],
                "images.image_2": ["6", 0],
                "clip": ["5", 1]
            } }
        });
        let map = semantic_param_map(&graph);
        assert!(
            map.get("image").is_none(),
            "failed slot 1 must not become image: {map:?}"
        );
        assert_eq!(map.get("image2"), Some(&vec!["6.image".to_string()]));
    }

    /// 多个 LoadImage 按节点 id 数字序分配 image / image2，而不是只取第一个。
    #[test]
    fn maps_multiple_load_image_nodes_in_id_order() {
        let graph = json!({
            "10": { "class_type": "LoadImage", "inputs": { "image": "a.png" } },
            "2": { "class_type": "LoadImage", "inputs": { "image": "b.png" } },
            "3": { "class_type": "ImageBatch", "inputs": { "image1": ["2", 0], "image2": ["10", 0] } }
        });
        let map = semantic_param_map(&graph);
        assert_eq!(map.get("image"), Some(&vec!["2.image".to_string()]));
        assert_eq!(map.get("image2"), Some(&vec!["10.image".to_string()]));
    }

    /// 放大/修复类工作流没有采样器，但仍然有 LoadImage：image 槽位必须被推导，
    /// 否则参考图无处可写、只能跑模板里写死的那张图。
    #[test]
    fn derives_image_slot_without_sampler() {
        let graph = json!({
            "1": { "class_type": "LoadImage", "inputs": { "image": "input.png" } },
            "2": { "class_type": "ImageUpscaleWithModel", "inputs": { "upscale_model": ["3", 0], "image": ["1", 0] } },
            "3": { "class_type": "UpscaleModelLoader", "inputs": { "model_name": "x.pth" } },
            "4": { "class_type": "SaveImage", "inputs": { "images": ["2", 0], "filename_prefix": "up" } }
        });
        let map = semantic_param_map(&graph);
        assert_eq!(map.get("image"), Some(&vec!["1.image".to_string()]));
        assert!(
            !map.contains_key("prompt"),
            "no sampler -> no prompt slot: {map:?}"
        );
    }

    /// 标准 KSampler 的 steps / cfg 必须自动发现，零配置也能用。
    #[test]
    fn derives_steps_and_cfg_from_standard_ksampler() {
        let graph = json!({
            "3": { "class_type": "KSampler", "inputs": {
                "seed": 1, "steps": 25, "cfg": 1.0,
                "positive": ["6", 0], "negative": ["7", 0], "latent_image": ["5", 0]
            } },
            "5": { "class_type": "EmptyLatentImage", "inputs": { "width": 512, "height": 512 } },
            "6": { "class_type": "CLIPTextEncode", "inputs": { "text": "a" } },
            "7": { "class_type": "CLIPTextEncode", "inputs": { "text": "b" } }
        });
        let map = semantic_param_map(&graph);
        assert_eq!(map.get("steps"), Some(&vec!["3.steps".to_string()]));
        assert_eq!(map.get("cfg"), Some(&vec!["3.cfg".to_string()]));
    }
}
