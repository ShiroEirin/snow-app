//! API 格式工作流的纯函数工具：语义参数注入、点号键解析、输出收集、错误提取。
//!
//! 「点号键」指 `6.text` / `3.inputs.seed` 这类路径，等价于
//! `graph["6"]["inputs"]["text"]`，与 ComfyUI 导出的 API 格式一一对应。

use std::collections::HashMap;

use napi::bindgen_prelude::*;
use serde_json::{json, Value};

/// 工具保留键：调用方不得通过 `params` 改写工具自己负责的字段。
pub const RESERVED_EXTRA_KEYS: &[&str] = &[
    "model",
    "prompt",
    "n",
    "size",
    "quality",
    "response_format",
    "images",
    "stream",
    "partial_images",
    "output_format",
    "outputFormat",
    "output_compression",
    "outputCompression",
    "background",
    "moderation",
    "seed",
    "input_fidelity",
    "inputFidelity",
    "aspect_ratio",
    "resolution",
    "style",
];

/// 解析渠道 `paramMap`：**只包含用户写下的那些语义名**。
///
/// 刻意不再拿一套写死的默认映射打底 —— 那套 `6.text` / `3.seed` / `5.width`
/// 是桥接方案时代的假设，套到别的图上只会把参数写到错误的节点。留空时由调用方
/// 从工作流图推导（`convert::semantic_param_map`）。
///
/// 值支持两种形态：
/// - 字符串：一个点号键（`"6.text"`）；
/// - 字符串数组：同一个语义写到多个节点输入（`["6.width", "20.width"]`，
///   布局类工作流需要把尺寸同时同步到 latent 与布局节点）。
/// 空数组表示**显式关闭**该语义（工作流里已固定，不允许被覆盖）。
pub fn parse_param_map(raw: &str) -> Result<HashMap<String, Vec<String>>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();

    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(map);
    }
    let parsed: Value = serde_json::from_str(raw)
        .map_err(|error| Error::from_reason(format!("Invalid ComfyUI `paramMap` JSON: {error}")))?;
    let Some(entries) = parsed.as_object() else {
        return Err(Error::from_reason(
            "Invalid ComfyUI `paramMap`: expected a JSON object of `semantic name -> dotted workflow key (or an array of keys)`",
        ));
    };
    for (key, value) in entries {
        let paths: Vec<String> = match value {
            Value::String(text) => {
                let path = text.trim();
                if path.is_empty() {
                    Vec::new()
                } else {
                    vec![path.to_string()]
                }
            }
            Value::Array(items) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .collect(),
            _ => {
                return Err(Error::from_reason(format!(
                    "Invalid ComfyUI `paramMap` entry \"{key}\": expected a dotted key string or an array of them"
                )))
            }
        };
        map.insert(key.clone(), paths);
    }
    Ok(map)
}

/// 合并「图推导出来的映射」与「用户显式覆盖」。
///
/// 语义是**逐项覆盖**，不是整表替换：只写 `{"negative": []}` 的渠道不该因此
/// 丢掉 prompt / seed / width / height 的自动推导；空数组表示刻意关闭该语义。
pub fn merge_param_map(
    derived: std::collections::BTreeMap<String, Vec<String>>,
    overrides: HashMap<String, Vec<String>>,
) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = derived.into_iter().collect();
    for (name, paths) in overrides {
        map.insert(name, paths);
    }
    map
}

/// 解析 `WxH` 尺寸（大小写 x / `*` 都接受）；无法解析时返回 None。
pub fn parse_size(size: Option<&str>) -> Option<(u64, u64)> {
    let raw = size?.trim();
    let (width, height) = raw.split_once(|c: char| matches!(c, 'x' | 'X' | '*'))?;
    let width = width.trim().parse::<u64>().ok()?;
    let height = height.trim().parse::<u64>().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

/// ComfyUI 原生分支的尺寸决策：显式请求必须先通过语法与能力白名单，
/// 不允许被通用链路静默丢弃后退回模板尺寸。
///
/// - `explicit`：调用方**显式传入**的 `size`（原始值，未经 `sanitize_size`）；
/// - `configured_default`：模型/渠道配置的默认尺寸（用于区分「没配」与
///   「配了但被能力白名单过滤」）；
/// - `resolved`：通用链路清洗后的尺寸；`None` 表示声明了
///   `supportedRatios` / `supportedResolutions` 但取值不匹配（被过滤掉）；
/// - `template`：工作流模板自带的宽高；
/// - `capabilities`：能力声明的可读摘要，仅用于错误/告警消息。
///
/// 返回 `(width, height, sync_size, warnings)`；`sync_size` 表示「尺寸有明确
/// 来源（显式或模型/渠道默认）」，与「是否显式请求」是两件事。
pub fn resolve_comfy_size(
    explicit: Option<&str>,
    configured_default: Option<&str>,
    resolved: Option<&str>,
    template: Option<(u64, u64)>,
    capabilities: &str,
) -> std::result::Result<(u64, u64, bool, Vec<String>), String> {
    let mut warnings = Vec::new();
    if let Some(raw) = explicit {
        // 先判语法：`abc` / `16:9` / `1K` 的问题不该被误报成「能力白名单」。
        let Some((width, height)) = parse_size(Some(raw)) else {
            return Err(format!(
                "invalid size `{raw}` for the native ComfyUI provider: only pixel dimensions like \
                 `1024x1024` are supported; aspect ratios (`16:9`) and tiers (`1K`) are not supported \
                 yet — pass an allowed pixel size instead"
            ));
        };
        // 语法合法但被能力白名单过滤（例如声明 `1K` 而请求 `1536x1024`）：
        // 报错并附上实际声明，避免让调用方以为「换个像素写法」就能通过。
        if resolved.is_none() {
            return Err(format!(
                "size `{raw}` is a valid pixel size but not allowed by this model's image \
                 capabilities ({capabilities}); update supportedRatios/supportedResolutions or pick a \
                 size it contains"
            ));
        }
        return Ok((width, height, true, warnings));
    }

    // 省略 size：模型/渠道默认值仍走通用清洗；被白名单过滤或不是 WxH 时只
    // 告警并回退模板（不能静默，否则调用方会以为默认尺寸生效了）。
    if let Some(raw) = resolved {
        if let Some((width, height)) = parse_size(Some(raw)) {
            return Ok((width, height, true, warnings));
        }
        warnings.push(format!(
            "configured default size `{raw}` is not `WIDTHxHEIGHT`; falling back to the workflow template size"
        ));
    } else if let Some(raw) = configured_default {
        if parse_size(Some(raw)).is_some() {
            warnings.push(format!(
                "configured default size `{raw}` is a valid pixel size but not allowed by this \
                 model's image capabilities ({capabilities}); falling back to the workflow template size"
            ));
        } else {
            warnings.push(format!(
                "configured default size `{raw}` is not `WIDTHxHEIGHT`; falling back to the workflow template size"
            ));
        }
    }
    match template {
        Some((width, height)) => Ok((width, height, false, warnings)),
        None => Ok((1024, 1024, false, warnings)),
    }
}

/// 校验「**显式**尺寸确实写进了图」：`width` / `height` 必须存在可写目标、
/// 两者不能映射到同一个输入、且全部落图，否则提交前报错。
///
/// 只让 `resolve_comfy_size` 通过是不够的：`paramMap` 写成 `{"width": []}`、
/// 路径拼错或节点没有该输入时，注入会静默跳过，最终仍按模板尺寸执行 ——
/// 调用方看到的却是「尺寸已生效」。
pub fn ensure_size_applied(
    param_map: &HashMap<String, Vec<String>>,
    injected: &[String],
    width: u64,
    height: u64,
) -> std::result::Result<(), String> {
    let collect = |semantic: &str| -> Vec<String> {
        param_map
            .get(semantic)
            .map(|paths| {
                paths
                    .iter()
                    .map(|path| normalize_path(path).to_ascii_lowercase())
                    .filter(|path| !path.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    };
    let width_targets = collect("width");
    let height_targets = collect("height");
    for (semantic, targets) in [("width", &width_targets), ("height", &height_targets)] {
        if targets.is_empty() {
            return Err(format!(
                "size {width}x{height} was requested but the workflow has no writable `{semantic}` target; \
                 add an explicit paramMap entry like `\"{semantic}\": \"<node>.{semantic}\"` or omit `size` \
                 (upscale/restore workflows usually have no size input)"
            ));
        }
    }
    if let Some(shared) = width_targets
        .iter()
        .find(|target| height_targets.iter().any(|other| other == *target))
    {
        return Err(format!(
            "size {width}x{height} cannot be applied: `width` and `height` both map to `{shared}`; \
             give them separate node inputs"
        ));
    }
    for (semantic, targets) in [("width", &width_targets), ("height", &height_targets)] {
        let missing: Vec<&str> = targets
            .iter()
            .filter(|target| !injected.iter().any(|path| path == *target))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "size {width}x{height} could not be written to `{semantic}` target(s): {}; \
                 check the paramMap paths and that the node inputs exist",
                missing.join(", ")
            ));
        }
    }
    Ok(())
}

/// 把点号键拆成「节点 id → inputs → 输入名」的段序列。
/// `6.text` 与 `6.inputs.text` 归一为同一路径。
fn canonical_segments(path: &str) -> Vec<String> {
    let parts: Vec<&str> = path
        .split('.')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() < 2 {
        return Vec::new();
    }
    let mut segments: Vec<String> = Vec::with_capacity(parts.len() + 1);
    segments.push(parts[0].to_string());
    if !parts[1].eq_ignore_ascii_case("inputs") {
        segments.push("inputs".to_string());
    }
    segments.extend(parts[1..].iter().map(|part| (*part).to_string()));
    segments
}

/// 归一化点号路径：`5.image` 与 `5.inputs.image` 指向同一个输入。
///
/// 参考图目标冲突检测必须用它作为 key，否则两个别名会绕过检查并互相覆盖。
pub fn normalize_path(path: &str) -> String {
    canonical_segments(path).join(".")
}

/// 写入一个语义参数（可能落到多个节点输入）；返回所有已注入的规范化路径。
pub fn set_value(
    graph: &mut Value,
    paths: Option<&Vec<String>>,
    value: Value,
    label: &str,
    warnings: &mut Vec<String>,
) -> Vec<String> {
    let Some(paths) = paths else {
        warnings.push(format!("paramMap has no \"{label}\" entry; skipped"));
        return Vec::new();
    };
    if paths.is_empty() {
        // 显式关闭（工作流里已固定，不允许覆盖）
        return Vec::new();
    }
    let mut injected: Vec<String> = Vec::with_capacity(paths.len());
    for path in paths {
        let segments = canonical_segments(path);
        if segments.len() < 3 {
            warnings.push(format!(
                "paramMap \"{label}\" -> \"{path}\" must be a dotted node key like \"6.text\"; skipped"
            ));
            continue;
        }
        if set_value_at(graph, &segments, value.clone(), path, false, warnings) {
            injected.push(segments.join(".").to_ascii_lowercase());
        }
    }
    injected
}

/// 递归下钻写入。
///
/// `create_missing = true` 时允许在**已存在的节点**下补一个新的输入键
/// （显式 `params` 覆盖可选输入，如布局工作流的 `strict` / `on_overflow`）；
/// 语义参数（prompt/seed/width…）走严格模式，避免把参数写到错误节点还不知道。
fn set_value_at(
    root: &mut Value,
    segments: &[String],
    value: Value,
    full_path: &str,
    create_missing: bool,
    warnings: &mut Vec<String>,
) -> bool {
    let Some(first) = segments.first() else {
        return false;
    };
    let Some(object) = root.as_object_mut() else {
        warnings.push(format!(
            "workflow path \"{full_path}\" hits a non-object node; skipped"
        ));
        return false;
    };
    if segments.len() == 1 {
        if !object.contains_key(first) {
            if !create_missing {
                warnings.push(format!("workflow has no \"{full_path}\"; skipped"));
                return false;
            }
            warnings.push(format!(
                "workflow has no optional input \"{full_path}\"; created it from params"
            ));
        }
        object.insert(first.clone(), value);
        return true;
    }
    // Autogrow / 动态输入的真实键可能含点号（`inputs["images.image_1"]`）。
    // 当剩余段拼起来正好是当前对象已存在的键时，优先按扁平键写入；硬拆成
    // 嵌套对象会被 ComfyUI 静默忽略（输入收不到，出图退化成默认行为）。
    if segments.len() >= 2 {
        let flat = segments.join(".");
        if object.contains_key(&flat) {
            object.insert(flat, value);
            return true;
        }
    }
    let Some(next) = object.get_mut(first) else {
        warnings.push(format!("workflow has no \"{full_path}\"; skipped"));
        return false;
    };
    set_value_at(next, &segments[1..], value, full_path, create_missing, warnings)
}

/// 合并 `params` 里的自定义节点参数。
///
/// 键有两种写法：
/// - **点号键**：`"7.steps"` / `"20.layout"`，直接落到该输入；
/// - **语义名**：`"layout"` / `"strict"`，按渠道 `paramMap` 映射到路径。
///
/// 值是对象 / 数组时自动序列化成 JSON 文本（`MOSSLayoutBoard.layout` 这类
/// 声明为 STRING 但要求 JSON 的输入就靠它）。
pub fn apply_extra_params(
    graph: &mut Value,
    extra: Option<&serde_json::Map<String, Value>>,
    param_map: &HashMap<String, Vec<String>>,
    injected: &[String],
    warnings: &mut Vec<String>,
) {
    let Some(extra) = extra else {
        return;
    };
    for (key, raw_value) in extra {
        if RESERVED_EXTRA_KEYS.contains(&key.as_str()) {
            continue;
        }
        let value = match raw_value {
            Value::Object(_) | Value::Array(_) => Value::String(raw_value.to_string()),
            other => other.clone(),
        };
        let direct = canonical_segments(key);
        let targets: Vec<String> = if direct.len() >= 3 {
            vec![key.clone()]
        } else if let Some(mapped) = param_map.get(key) {
            mapped.clone()
        } else {
            warnings.push(format!(
                "params key \"{key}\" is neither a dotted workflow key (\"7.steps\") nor a mapped semantic name; skipped"
            ));
            continue;
        };
        for target in targets {
            let segments = canonical_segments(&target);
            if segments.len() < 3 {
                warnings.push(format!(
                    "params target \"{target}\" is not a dotted node key; skipped"
                ));
                continue;
            }
            if injected
                .iter()
                .any(|path| *path == segments.join(".").to_ascii_lowercase())
            {
                // 工具已注入该路径（prompt / negative / seed / width / height / image），保留工具的值
                continue;
            }
            set_value_at(graph, &segments, value.clone(), &target, true, warnings);
        }
    }
}

/// 从 history entry 的 outputs 里收集图片引用（{filename, subfolder, type}）。
///
/// `SaveImage` 的产物 `type=output`，`PreviewImage` 的产物 `type=temp`。
/// 优先只收最终产物；只有工作流完全没有 SaveImage 时才回退 temp，并把
/// 「拿到的是预览图」告知调用方，避免把临时图当成成品。
pub fn collect_output_images(outputs: Option<&Value>) -> (Vec<Value>, bool) {
    let mut final_images = Vec::new();
    let mut preview_images = Vec::new();
    let Some(nodes) = outputs.and_then(Value::as_object) else {
        return (final_images, false);
    };
    for node_output in nodes.values() {
        let Some(list) = node_output.get("images").and_then(Value::as_array) else {
            continue;
        };
        for image in list {
            let has_name = image
                .get("filename")
                .and_then(Value::as_str)
                .map(|name| !name.trim().is_empty())
                .unwrap_or(false);
            if !has_name {
                continue;
            }
            if image.get("type").and_then(Value::as_str) == Some("temp") {
                preview_images.push(image.clone());
            } else {
                final_images.push(image.clone());
            }
        }
    }
    if final_images.is_empty() && !preview_images.is_empty() {
        (preview_images, true)
    } else {
        (final_images, false)
    }
}

/// history entry 里的执行错误（`status.status_str == "error"`）。
pub fn entry_error(entry: &Value) -> Option<String> {
    let status = entry.get("status")?;
    let status_str = status
        .get("status_str")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !status_str.eq_ignore_ascii_case("error") {
        return None;
    }
    let messages = status
        .get("messages")
        .map(|value| value.to_string())
        .unwrap_or_default();
    Some(if messages.is_empty() || messages == "null" {
        "workflow execution failed".to_string()
    } else {
        messages
    })
}

/// 把一张输出图转成 MCP 统一的 content block。
pub fn image_block(bytes: &[u8], filename: &str) -> Value {
    use base64::Engine;
    json!({
        "type": "image",
        "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        "mimeType": super::client::mime_for_file_name(filename),
    })
}

/// 工作流文本上限（2 MB）：防止把超大文件读进内存。
pub const MAX_WORKFLOW_BYTES: usize = 2 * 1024 * 1024;

/// 工作流库目录：`<userData>/comfyui-workflows`（与 snowapp.db 同级）。
///
/// 前端只保存**逻辑标识**（库里的文件名 / ComfyUI 画布工作流名 / 内联 JSON），
/// 真实路径一律在这里解析 —— 换机器、换安装目录都不用改配置。
pub fn library_dir() -> Result<std::path::PathBuf> {
    let info = crate::storage::initialize_app_storage()?;
    let database = std::path::PathBuf::from(info.database_path);
    let root = database
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let dir = root.join("comfyui-workflows");
    std::fs::create_dir_all(&dir).map_err(|error| {
        Error::from_reason(format!("Failed to create workflow library: {error}"))
    })?;
    Ok(dir)
}

/// 把「逻辑标识」解析为磁盘上的工作流文件。
///
/// 依次尝试：
/// 1. 直接可用的绝对/相对路径（老配置兼容）；
/// 2. 工作流库内的文件名（`anima-t2i.api.json`）；
/// 3. 工作流库内自动补后缀（`anima-t2i` → `.api.json` / `.json`）。
///
/// 返回 `None` 表示这不是本地文件标识（可能是 ComfyUI 画布工作流名，由调用方走 API 拉取）。
pub fn resolve_workflow_path(identifier: &str) -> Option<std::path::PathBuf> {
    let trimmed = identifier.trim();
    if trimmed.is_empty() || trimmed.starts_with('{') {
        return None;
    }
    let direct = std::path::PathBuf::from(trimmed);
    if direct.is_file() {
        return Some(direct);
    }
    let dir = library_dir().ok()?;
    let mut candidates = vec![dir.join(trimmed)];
    for suffix in [".api.json", ".json"] {
        candidates.push(dir.join(format!("{trimmed}{suffix}")));
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// 读取工作流：内联 JSON 文本，或逻辑标识 / 路径（有大小上限、扩展名受限）。
pub fn read_workflow_text(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(Error::from_reason("ComfyUI workflow is empty"));
    }
    // 内联 JSON：直接返回原文。
    if raw.starts_with('{') {
        return Ok(raw.to_string());
    }
    let path = resolve_workflow_path(raw).ok_or_else(|| {
        Error::from_reason(format!(
            "ComfyUI workflow \"{raw}\" was not found in the workflow library ({}). Bind it by file name from the workflow picker, or pass an absolute .json path.",
            library_dir()
                .map(|dir| dir.join("").to_string_lossy().to_string())
                .unwrap_or_else(|_| "unknown directory".to_string())
        ))
    })?;
    let display = path.to_string_lossy().to_string();
    let metadata = std::fs::metadata(&path).map_err(|error| {
        Error::from_reason(format!(
            "Failed to read ComfyUI workflow file \"{display}\": {error}"
        ))
    })?;
    if metadata.len() as usize > MAX_WORKFLOW_BYTES {
        return Err(Error::from_reason(format!(
            "ComfyUI workflow file \"{display}\" is {} bytes (limit {MAX_WORKFLOW_BYTES})",
            metadata.len()
        )));
    }
    std::fs::read_to_string(&path).map_err(|error| {
        Error::from_reason(format!(
            "Failed to read ComfyUI workflow file \"{display}\": {error}"
        ))
    })
}

/// 把 `size` 同步进布局文档：调用方不该被要求手动保持三处尺寸一致。
///
/// 只处理 `params.layout`（语义名）或映射到 `param_map["layout"]` 的点号键；
/// 值可以是对象，也可以是一段 JSON 文本（都会就地改写 width/height）。
pub fn sync_layout_size(
    extra: &mut serde_json::Map<String, Value>,
    param_map: &HashMap<String, Vec<String>>,
    width: u64,
    height: u64,
    warnings: &mut Vec<String>,
) {
    let layout_paths = param_map.get("layout").cloned().unwrap_or_default();
    let key = extra
        .keys()
        .find(|key| {
            key.as_str() == "layout"
                || layout_paths
                    .iter()
                    .any(|path| canonical_segments(path).join(".") == canonical_segments(key).join("."))
        })
        .cloned();
    let Some(key) = key else {
        return;
    };

    let current = extra.get(&key).cloned().unwrap_or(Value::Null);
    let was_string = current.is_string();
    let mut layout = match current {
        Value::Object(map) => Some(map),
        Value::String(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => Some(map),
            Ok(_) => None,
            Err(error) => {
                warnings.push(format!(
                    "params.layout is not valid JSON ({error}); size was not synced into the layout"
                ));
                None
            }
        },
        Value::Null => None,
        _ => None,
    };
    let Some(layout) = layout.as_mut() else {
        return;
    };

    layout.insert("width".to_string(), json!(width));
    layout.insert("height".to_string(), json!(height));
    let synced = Value::Object(layout.clone());
    // 原样回写：对象保持对象（后面会被统一序列化），字符串保持字符串。
    if was_string {
        extra.insert(key, Value::String(synced.to_string()));
    } else {
        extra.insert(key, synced);
    }
}

/// 读取点号键指向的当前值；兼容 `node.input` 与 `node.inputs.input`。
pub fn read_path<'a>(graph: &'a Value, path: &str) -> Option<&'a Value> {
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
    if let Some(value) = inputs.get(rest.join(".")) {
        return Some(value);
    }
    let mut current = inputs;
    for part in rest {
        current = current.get(*part)?;
    }
    Some(current)
}

/// 模板里已有布局 JSON、但本次请求没有覆盖 layout 时，直接同步它的宽高。
///
/// 不自动改 `items[].box`：构图属于用户/工作流作者的决定，越界只告警。
fn sync_template_layout_size(
    graph: &mut Value,
    param_map: &HashMap<String, Vec<String>>,
    extra: Option<&serde_json::Map<String, Value>>,
    width: u64,
    height: u64,
    warnings: &mut Vec<String>,
) {
    let paths = param_map.get("layout").cloned().unwrap_or_default();
    if paths.is_empty() {
        return;
    }
    if let Some(extra) = extra {
        let has_layout = extra.keys().any(|key| {
            key == "layout"
                || paths.iter().any(|path| {
                    canonical_segments(path).join(".") == canonical_segments(key).join(".")
                })
        });
        if has_layout {
            return;
        }
    }
    for path in paths {
        let Some(current) = read_path(graph, &path) else {
            continue;
        };
        let Some(text) = current.as_str() else {
            continue;
        };
        // 模板里留空的 layout 就是「没有布局文档」（纯文生图用法）：
        // 没有东西可同步，不该报一条看不懂的「不是合法 JSON」。
        if text.trim().is_empty() {
            continue;
        }
        let Ok(mut layout) = serde_json::from_str::<Value>(text) else {
            warnings.push(format!(
                "workflow layout `{path}` is not valid JSON; size was not synced"
            ));
            continue;
        };
        if !layout.is_object() {
            continue;
        }
        layout["width"] = json!(width);
        layout["height"] = json!(height);
        if let Some(items) = layout.get("items").and_then(Value::as_array) {
            let out_of_bounds = items
                .iter()
                .filter(|item| {
                    let Some(values) = item.get("box").and_then(Value::as_array) else {
                        return false;
                    };
                    if values.len() != 4 {
                        return false;
                    }
                    values[2].as_u64().unwrap_or(0) > width
                        || values[3].as_u64().unwrap_or(0) > height
                })
                .count();
            if out_of_bounds > 0 {
                warnings.push(format!(
                    "layout has {out_of_bounds} box(es) outside {width}x{height}; adjust the boxes or size (boxes were not changed automatically)"
                ));
            }
        }
        let mut local_warnings = Vec::new();
        set_value(
            graph,
            Some(&vec![path]),
            Value::String(layout.to_string()),
            "layout",
            &mut local_warnings,
        );
        warnings.extend(local_warnings);
    }
}

/// 一次请求的可提交图 + 注入记录（不含参考图上传，那一步需要异步 HTTP）。
pub struct PreparedGraph {
    pub graph: Value,
    pub injected: Vec<String>,
    pub warnings: Vec<String>,
}

/// 纯函数：克隆模板 → 注入语义参数 → 同步布局尺寸 → 合并 `params`。
#[allow(clippy::too_many_arguments)]
pub fn prepare_graph(
    template: &Value,
    param_map: &HashMap<String, Vec<String>>,
    prompt: &str,
    negative: Option<&str>,
    width: u64,
    height: u64,
    seed: u64,
    extra: Option<&serde_json::Map<String, Value>>,
    // 只有尺寸**有明确来源**（显式请求或模型/渠道默认）时才同步布局尺寸；
    // 否则默认 1024x1024 会把模板里 1536x1024 之类的构图改坏。
    sync_size: bool,
) -> PreparedGraph {
    let mut warnings: Vec<String> = Vec::new();
    let mut injected: Vec<String> = Vec::new();
    let mut graph = template.clone();

    injected.extend(set_value(
        &mut graph,
        param_map.get("prompt"),
        json!(prompt),
        "prompt",
        &mut warnings,
    ));
    if let Some(text) = negative {
        injected.extend(set_value(
            &mut graph,
            param_map.get("negative"),
            json!(text),
            "negative",
            &mut warnings,
        ));
    }
    injected.extend(set_value(
        &mut graph,
        param_map.get("seed"),
        json!(seed),
        "seed",
        &mut warnings,
    ));
    injected.extend(set_value(
        &mut graph,
        param_map.get("width"),
        json!(width),
        "width",
        &mut warnings,
    ));
    injected.extend(set_value(
        &mut graph,
        param_map.get("height"),
        json!(height),
        "height",
        &mut warnings,
    ));

    // 布局文档的 width/height 跟着 size 走（第三处同步，代码负责而不是调用方）。
    let mut extra_owned = extra.cloned();
    if sync_size {
        if let Some(extra_map) = extra_owned.as_mut() {
            sync_layout_size(extra_map, param_map, width, height, &mut warnings);
        }
        // 本次没有覆盖 layout 时，模板里的布局 JSON 也要跟着 size 走，
        // 否则 latent / 布局节点 / 布局文档三处尺寸会不一致。
        sync_template_layout_size(
            &mut graph,
            param_map,
            extra_owned.as_ref(),
            width,
            height,
            &mut warnings,
        );
    }
    apply_extra_params(
        &mut graph,
        extra_owned.as_ref(),
        param_map,
        &injected,
        &mut warnings,
    );

    PreparedGraph {
        graph,
        injected,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> Value {
        json!({
            "6": { "class_type": "EmptyLatentImage", "inputs": { "width": 512, "height": 512, "batch_size": 1 } },
            "20": { "class_type": "MOSSLayoutBoard", "inputs": { "width": 512, "height": 512, "prompt": "", "layout": "" } }
        })
    }

    #[test]
    fn parses_single_and_multi_path_mappings() {
        let map = parse_param_map(
            r#"{"prompt":"20.prompt","width":["6.width","20.width"],"negative":[]}"#,
        )
        .unwrap();
        assert_eq!(map.get("prompt"), Some(&vec!["20.prompt".to_string()]));
        assert_eq!(
            map.get("width"),
            Some(&vec!["6.width".to_string(), "20.width".to_string()])
        );
        // 空数组 = 显式关闭
        assert_eq!(map.get("negative"), Some(&Vec::<String>::new()));
        // 没写的语义**不注入**：写死默认映射会在别人的图上落到错节点。
        assert_eq!(map.get("seed"), None);
        assert_eq!(map.len(), 3);
    }

    /// 显式 paramMap 是**逐项覆盖**：只关掉 negative 不该连带丢掉 prompt/size。
    #[test]
    fn overrides_merge_with_derived_instead_of_replacing_them() {
        let mut derived = std::collections::BTreeMap::new();
        derived.insert("prompt".to_string(), vec!["20.prompt".to_string()]);
        derived.insert("seed".to_string(), vec!["7.seed".to_string()]);
        derived.insert("width".to_string(), vec!["6.width".to_string()]);
        let overrides = parse_param_map(r#"{"negative":[]}"#).unwrap();
        let map = merge_param_map(derived, overrides);
        assert_eq!(map.get("prompt"), Some(&vec!["20.prompt".to_string()]));
        assert_eq!(map.get("seed"), Some(&vec!["7.seed".to_string()]));
        assert_eq!(map.get("width"), Some(&vec!["6.width".to_string()]));
        assert_eq!(map.get("negative"), Some(&Vec::<String>::new()));
    }

    #[test]
    fn writes_one_semantic_to_several_nodes() {
        let map = parse_param_map(r#"{"width":["6.width","20.width"]}"#).unwrap();
        let mut g = graph();
        let mut warnings = Vec::new();
        let injected = set_value(
            &mut g,
            map.get("width"),
            json!(1024),
            "width",
            &mut warnings,
        );
        assert_eq!(g["6"]["inputs"]["width"], json!(1024));
        assert_eq!(g["20"]["inputs"]["width"], json!(1024));
        assert_eq!(injected.len(), 2);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn routes_semantic_params_and_stringifies_objects() {
        let map = parse_param_map(r#"{"layout":"20.layout","strict":"20.strict"}"#).unwrap();
        let mut g = graph();
        let mut warnings = Vec::new();
        let extra = json!({
            "layout": { "width": 512, "height": 512, "items": [] },
            "strict": true,
            "20.prompt": "a cat",
            "unknown": 1
        });
        let extra = extra.as_object().unwrap().clone();
        apply_extra_params(&mut g, Some(&extra), &map, &[], &mut warnings);

        // 对象被序列化成 JSON 文本写进 STRING 输入
        let layout = g["20"]["inputs"]["layout"].as_str().expect("layout as string");
        assert!(layout.starts_with(r#"{"height":512"#), "{layout}");
        assert_eq!(g["20"]["inputs"]["strict"], json!(true));
        assert_eq!(g["20"]["inputs"]["prompt"], json!("a cat"));
        assert!(warnings.iter().any(|w| w.contains("unknown")), "{warnings:?}");
    }

    #[test]
    fn explicit_empty_mapping_is_silently_skipped() {
        let map = parse_param_map(r#"{"negative":[]}"#).unwrap();
        let mut g = graph();
        let mut warnings = Vec::new();
        let injected = set_value(&mut g, map.get("negative"), json!("x"), "negative", &mut warnings);
        assert!(injected.is_empty());
        assert!(warnings.is_empty(), "explicit disable must not warn: {warnings:?}");
    }

    // ---- 外部裁判：golden 快照 + 布局尺寸同步 ----

    const ANIMA_MAP: &str = r#"{"prompt":"11.text","negative":"12.text","seed":"19.seed","width":"28.width","height":"28.height"}"#;

    fn anima_template() -> Value {
        serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/anima-t2i.api.json"
        )))
        .unwrap()
    }

    /// 给定固定输入，提交的图必须与快照逐字节一致。
    /// 快照缺失或需要更新时：`GOLDEN_UPDATE=1 cargo test --lib golden_prepared_graph`。
    #[test]
    fn golden_prepared_graph_for_anima() {
        let map = parse_param_map(ANIMA_MAP).unwrap();
        let prepared = prepare_graph(
            &anima_template(),
            &map,
            "1girl, solo, masterpiece",
            Some("blurry, worst quality"),
            768,
            1024,
            424242,
            None,
            true,
        );
        let actual = serde_json::to_string_pretty(&prepared.graph).unwrap();
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/anima-t2i.prepared.golden.json"
        );
        if std::env::var("GOLDEN_UPDATE").is_ok() {
            std::fs::write(path, &actual).unwrap();
            return;
        }
        let expected = std::fs::read_to_string(path).unwrap_or_else(|_| {
            panic!("golden snapshot missing at {path}; run with GOLDEN_UPDATE=1")
        });
        assert_eq!(actual, expected, "prepared graph drifted from golden snapshot");
    }

    #[test]
    fn golden_asserts_semantic_values_are_really_injected() {
        let map = parse_param_map(ANIMA_MAP).unwrap();
        let prepared = prepare_graph(
            &anima_template(),
            &map,
            "P",
            Some("N"),
            768,
            1024,
            424242,
            None,
            true,
        );
        assert_eq!(prepared.graph["11"]["inputs"]["text"], json!("P"));
        assert_eq!(prepared.graph["12"]["inputs"]["text"], json!("N"));
        assert_eq!(prepared.graph["19"]["inputs"]["seed"], json!(424242));
        assert_eq!(prepared.graph["28"]["inputs"]["width"], json!(768));
        assert_eq!(prepared.graph["28"]["inputs"]["height"], json!(1024));
    }

    /// size 必须被写进布局文档（第三处同步由代码负责）。
    #[test]
    fn syncs_size_into_layout_document() {
        let map = parse_param_map(
            r#"{"prompt":"20.prompt","negative":[],"seed":"7.seed","width":["6.width","20.width"],"height":["6.height","20.height"],"layout":"20.layout"}"#,
        )
        .unwrap();
        let template: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/moss-image-layout.api.json"
        )))
        .unwrap();

        // 调用方故意传了与 size 不一致的 layout 尺寸
        let mut extra = serde_json::Map::new();
        extra.insert(
            "layout".to_string(),
            json!({ "width": 1, "height": 1, "items": [] }),
        );
        let prepared = prepare_graph(
            &template,
            &map,
            "poster",
            None,
            1024,
            1536,
            7,
            Some(&extra),
            true,
        );

        // 布局节点收到的是一段 JSON 文本，里面的尺寸已被改写
        let layout_text = prepared.graph["20"]["inputs"]["layout"]
            .as_str()
            .expect("layout must be serialized as a string");
        let layout: Value = serde_json::from_str(layout_text).unwrap();
        assert_eq!(layout["width"], json!(1024));
        assert_eq!(layout["height"], json!(1536));
        assert_eq!(prepared.graph["6"]["inputs"]["width"], json!(1024));
        assert_eq!(prepared.graph["20"]["inputs"]["width"], json!(1024));
        assert_eq!(prepared.graph["20"]["inputs"]["height"], json!(1536));
    }

    #[test]
    fn normalizes_equivalent_dotted_paths() {
        assert_eq!(normalize_path("5.image"), normalize_path("5.inputs.image"));
        assert_eq!(normalize_path("5.image"), "5.inputs.image".to_string());
        assert_ne!(normalize_path("5.image"), normalize_path("6.image"));
    }

    /// 只改 size、不重新传 layout 时，模板里的 layout 宽高也要同步。
    #[test]
    fn syncs_template_layout_when_only_size_changes() {
        let template: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/moss-image-layout.api.json"
        )))
        .unwrap();
        let map = parse_param_map(r#"{"layout":"20.layout"}"#).unwrap();
        let prepared = prepare_graph(&template, &map, "p", None, 1024, 768, 1, None, true);
        let layout_text = prepared.graph["20"]["inputs"]["layout"]
            .as_str()
            .expect("layout string");
        let layout: Value = serde_json::from_str(layout_text).unwrap();
        assert_eq!(layout["width"], json!(1024));
        assert_eq!(layout["height"], json!(768));
    }

    #[test]
    fn read_workflow_text_rejects_oversized_files() {
        let dir = std::env::temp_dir();
        let path = dir.join("snow-comfyui-too-big.json");
        std::fs::write(&path, vec![b' '; MAX_WORKFLOW_BYTES + 1]).unwrap();
        let error = read_workflow_text(path.to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("limit"), "unexpected: {error}");
        let _ = std::fs::remove_file(&path);
    }

    /// 内联 JSON 绝不走磁盘解析：否则一个以 `{` 开头的配置会被当成路径。
    #[test]
    fn reads_inline_json_without_touching_the_filesystem() {
        let text = r#"{"3":{"class_type":"KSampler","inputs":{"seed":1}}}"#;
        assert_eq!(read_workflow_text(text).unwrap(), text);
        assert!(resolve_workflow_path(text).is_none());
    }

    /// 逻辑名解析失败时必须告诉用户去哪儿找 —— 不是笼统的「file not found」。
    #[test]
    fn unknown_logical_name_points_at_the_workflow_library() {
        let error = read_workflow_text("__definitely_missing_workflow__").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("workflow library"), "unexpected: {message}");
    }

    /// 预检的判决力：拿一台「没有这些节点」的 object_info 去校验同一张图，必须报错。
    #[test]
    fn preflight_rejects_graph_against_object_info_without_its_nodes() {
        let template = anima_template();
        let wrong_info: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/object-info-sd15-t2i-lora.json"
        )))
        .unwrap();
        let problems = crate::comfyui::validate::validate_graph(&template, &wrong_info);
        assert!(!problems.is_empty(), "preflight must reject a mismatched workflow");
        assert!(
            problems.iter().any(|problem| problem.code == "NODE_CLASS_MISSING"),
            "{problems:?}"
        );
        // 报错信息里必须带节点 id，否则用户无从下手
        assert!(
            problems.iter().any(|problem| !problem.node.is_empty()),
            "{problems:?}"
        );
    }

    /// Autogrow 的真实输入名含点号（`inputs["images.image_1"]`）：必须写扁平键，
    /// 写成嵌套对象 ComfyUI 会静默忽略（参考图不生效）。
    #[test]
    fn writes_dotted_autogrow_input_as_flat_key() {
        let mut g = json!({
            "7": {
                "class_type": "TextEncodeQwenImage21",
                "inputs": { "images.image_1": ["5", 0] }
            }
        });
        let mut warnings = Vec::new();
        let injected = set_value(
            &mut g,
            Some(&vec!["7.images.image_1".to_string()]),
            json!(["9", 0]),
            "image",
            &mut warnings,
        );
        assert_eq!(g["7"]["inputs"]["images.image_1"], json!(["9", 0]));
        assert!(g["7"]["inputs"].get("images").is_none(), "must not nest: {g}");
        assert_eq!(injected.len(), 1);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// 有 SaveImage（type=output）时不能把 PreviewImage（type=temp）也当成品。
    #[test]
    fn collects_final_outputs_before_previews() {
        let outputs = json!({
            "8": { "images": [{ "filename": "final.png", "subfolder": "", "type": "output" }] },
            "9": { "images": [{ "filename": "preview.png", "subfolder": "", "type": "temp" }] }
        });
        let (images, used_preview) = collect_output_images(Some(&outputs));
        assert!(!used_preview);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0]["filename"], json!("final.png"));
    }

    /// 只有 PreviewImage 时回退 temp，并明确标记「这是预览图」。
    #[test]
    fn falls_back_to_preview_images_with_a_flag() {
        let outputs = json!({
            "9": { "images": [{ "filename": "preview.png", "subfolder": "", "type": "temp" }] }
        });
        let (images, used_preview) = collect_output_images(Some(&outputs));
        assert!(used_preview);
        assert_eq!(images.len(), 1);
    }

    /// 比例语法在原生 ComfyUI 分支不支持：必须报错，不能静默回退模板尺寸。
    #[test]
    fn rejects_explicit_ratio_size_for_comfyui() {
        let error = resolve_comfy_size(Some("16:9"), None, Some("16:9"), None, "none declared")
            .expect_err("ratio must fail");
        assert!(error.contains("16:9"), "{error}");
        assert!(error.contains("1024x1024"), "{error}");
        assert!(error.contains("not supported"), "{error}");
    }

    /// 档位语法（`1K`）同样不支持。
    #[test]
    fn rejects_explicit_tier_size_for_comfyui() {
        let error = resolve_comfy_size(Some("1K"), None, Some("1K"), None, "none declared")
            .expect_err("tier must fail");
        assert!(error.contains("1K"), "{error}");
        assert!(error.contains("not supported"), "{error}");
    }

    /// 非法字符串（含 0 尺寸）必须报错。
    #[test]
    fn rejects_explicit_invalid_size_for_comfyui() {
        assert!(resolve_comfy_size(Some("abc"), None, Some("abc"), None, "").is_err());
        assert!(resolve_comfy_size(Some("0x1024"), None, Some("0x1024"), None, "").is_err());
    }

    /// 合法 WxH 直接生效，并标记需要同步模板布局。
    #[test]
    fn accepts_explicit_pixel_size_for_comfyui() {
        let (width, height, sync_size, warnings) =
            resolve_comfy_size(Some("1536x1024"), None, Some("1536x1024"), Some((512, 512)), "")
                .expect("valid pixel size");
        assert_eq!((width, height), (1536, 1024));
        assert!(sync_size);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// 合法像素尺寸被能力白名单过滤时，必须说明「尺寸合法、能力不含它」。
    #[test]
    fn rejects_valid_pixel_size_outside_declared_capabilities() {
        let error = resolve_comfy_size(
            Some("1536x1024"),
            None,
            None,
            Some((512, 512)),
            "supportedResolutions=`1K`",
        )
        .expect_err("whitelist mismatch must fail");
        assert!(error.contains("1536x1024"), "{error}");
        assert!(error.contains("valid pixel size"), "{error}");
        assert!(error.contains("1K"), "{error}");
    }

    /// 完全省略 size 时保留模板尺寸，且不触发布局同步。
    #[test]
    fn keeps_template_size_when_size_is_omitted() {
        let (width, height, sync_size, warnings) =
            resolve_comfy_size(None, None, None, Some((1536, 1024)), "")
                .expect("omitted size");
        assert_eq!((width, height), (1536, 1024));
        assert!(!sync_size);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// 省略 size 但配置了合法默认像素尺寸时，默认优先于模板且需要同步布局。
    #[test]
    fn prefers_configured_default_over_template_size() {
        let (width, height, sync_size, warnings) =
            resolve_comfy_size(None, None, Some("1024x768"), Some((512, 512)), "")
                .expect("configured default");
        assert_eq!((width, height), (1024, 768));
        assert!(sync_size);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// 既没有 size 也没有模板尺寸时才用 1024 兜底。
    #[test]
    fn falls_back_to_1024_when_size_and_template_are_absent() {
        let (width, height, sync_size, _) =
            resolve_comfy_size(None, None, None, None, "").expect("no size at all");
        assert_eq!((width, height), (1024, 1024));
        assert!(!sync_size);
    }

    /// 模型/渠道配了非 WxH 默认值时：告警并回退模板，不静默当作比例生效。
    #[test]
    fn warns_when_configured_default_is_not_pixel_size() {
        let (width, height, sync_size, warnings) =
            resolve_comfy_size(None, Some("16:9"), Some("16:9"), Some((1536, 1024)), "")
                .expect("default size");
        assert_eq!((width, height), (1536, 1024));
        assert!(!sync_size);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("16:9"), "{warnings:?}");
    }

    /// 默认值是合法像素但被能力白名单过滤时，告警必须说明是能力问题。
    #[test]
    fn warns_when_configured_default_is_filtered_by_capabilities() {
        let (width, height, sync_size, warnings) = resolve_comfy_size(
            None,
            Some("1536x1024"),
            None,
            Some((512, 512)),
            "supportedResolutions=`1K`",
        )
        .expect("filtered default");
        assert_eq!((width, height), (512, 512));
        assert!(!sync_size);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("1536x1024"), "{warnings:?}");
        assert!(warnings[0].contains("1K"), "{warnings:?}");
    }

    fn prepare_with_size_targets(
        width_paths: Vec<String>,
        height_paths: Vec<String>,
    ) -> (PreparedGraph, HashMap<String, Vec<String>>) {
        let template = json!({
            "1": { "class_type": "EmptyLatentImage", "inputs": { "width": 512, "height": 512 } }
        });
        let mut map = HashMap::new();
        map.insert("width".to_string(), width_paths);
        map.insert("height".to_string(), height_paths);
        let prepared = prepare_graph(&template, &map, "a prompt", None, 1536, 1024, 7, None, true);
        (prepared, map)
    }

    /// `paramMap` 把 width 显式关闭（`[]`）时，尺寸请求必须失败而不是静默按模板跑。
    #[test]
    fn rejects_size_when_width_target_is_disabled() {
        let (prepared, map) =
            prepare_with_size_targets(Vec::new(), vec!["1.height".to_string()]);
        let error = ensure_size_applied(&map, &prepared.injected, 1536, 1024)
            .expect_err("disabled width target must fail");
        assert!(error.contains("width"), "{error}");
        assert!(error.contains("upscale/restore"), "{error}");
    }

    /// height 同样是空数组时也不能静默。
    #[test]
    fn rejects_size_when_height_target_is_disabled() {
        let (prepared, map) =
            prepare_with_size_targets(vec!["1.width".to_string()], Vec::new());
        let error = ensure_size_applied(&map, &prepared.injected, 1536, 1024)
            .expect_err("disabled height target must fail");
        assert!(error.contains("height"), "{error}");
    }

    /// 路径拼错 / 节点没有该输入时（注入被跳过），尺寸请求也必须失败。
    #[test]
    fn rejects_size_when_height_target_path_does_not_exist() {
        let (prepared, map) = prepare_with_size_targets(
            vec!["1.width".to_string()],
            vec!["1.missing".to_string()],
        );
        let error = ensure_size_applied(&map, &prepared.injected, 1536, 1024)
            .expect_err("missing height target must fail");
        assert!(error.contains("1.inputs.missing"), "{error}");
    }

    /// 节点 id 本身不存在时也要报错，而不是当成「没写」。
    #[test]
    fn rejects_size_when_target_node_does_not_exist() {
        let (prepared, map) = prepare_with_size_targets(
            vec!["9.width".to_string()],
            vec!["1.height".to_string()],
        );
        let error = ensure_size_applied(&map, &prepared.injected, 1536, 1024)
            .expect_err("missing node must fail");
        assert!(error.contains("9.inputs.width"), "{error}");
    }

    /// `1.inputs.width` 形式与 `1.width` 归一，不能因此误报。
    #[test]
    fn accepts_size_targets_in_inputs_form() {
        let (prepared, map) = prepare_with_size_targets(
            vec!["1.inputs.width".to_string()],
            vec!["1.inputs.height".to_string()],
        );
        ensure_size_applied(&map, &prepared.injected, 1536, 1024).expect("normalized targets");
    }

    /// 多个目标（数组路径）必须全部落图。
    #[test]
    fn accepts_multiple_size_targets() {
        let template = json!({
            "1": { "class_type": "EmptyLatentImage", "inputs": { "width": 512, "height": 512 } },
            "2": { "class_type": "EmptyLatentImage", "inputs": { "width": 512, "height": 512 } }
        });
        let mut map = HashMap::new();
        map.insert(
            "width".to_string(),
            vec!["1.width".to_string(), "2.width".to_string()],
        );
        map.insert(
            "height".to_string(),
            vec!["1.height".to_string(), "2.height".to_string()],
        );
        let prepared = prepare_graph(&template, &map, "a", None, 1536, 1024, 7, None, true);
        ensure_size_applied(&map, &prepared.injected, 1536, 1024).expect("all targets written");
    }

    /// width 与 height 映射到同一个输入时必须拒绝（否则后写的会覆盖前一个）。
    #[test]
    fn rejects_size_when_width_and_height_share_a_target() {
        let (prepared, map) = prepare_with_size_targets(
            vec!["1.width".to_string()],
            vec!["1.width".to_string()],
        );
        let error = ensure_size_applied(&map, &prepared.injected, 1536, 1024)
            .expect_err("shared target must fail");
        assert!(error.contains("both map to"), "{error}");
    }

    /// 两个目标都可写时正常通过，并且值确实落进图里。
    #[test]
    fn accepts_size_when_both_targets_are_written() {
        let (prepared, map) = prepare_with_size_targets(
            vec!["1.width".to_string()],
            vec!["1.height".to_string()],
        );
        ensure_size_applied(&map, &prepared.injected, 1536, 1024).expect("both targets written");
        assert_eq!(prepared.graph["1"]["inputs"]["width"], json!(1536));
        assert_eq!(prepared.graph["1"]["inputs"]["height"], json!(1024));
    }
}
