//! API 格式工作流的纯函数工具：语义参数注入、点号键解析、输出收集、错误提取。
//!
//! 「点号键」指 `6.text` / `3.inputs.seed` 这类路径，等价于
//! `graph["6"]["inputs"]["text"]`，与 ComfyUI 导出的 API 格式一一对应。

use std::collections::HashMap;

use napi::bindgen_prelude::*;
use serde_json::{json, Value};

/// 语义名 → 工作流点号键的默认映射（渠道 `paramMap` 为空时生效）。
/// 这套默认值覆盖了最常见的 SD1.5 / SDXL / Flux 基础工作流。
pub const DEFAULT_PARAM_MAP: &[(&str, &str)] = &[
    ("prompt", "6.text"),
    ("negative", "7.text"),
    ("seed", "3.seed"),
    ("width", "5.width"),
    ("height", "5.height"),
    ("image", "10.image"),
];

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

/// 解析渠道 `paramMap`：默认映射打底，用户提供的映射逐项覆盖，
/// 因此只写需要改的那几个语义名即可。
///
/// 值支持两种形态：
/// - 字符串：一个点号键（`"6.text"`）；
/// - 字符串数组：同一个语义写到多个节点输入（`["6.width", "20.width"]`，
///   布局类工作流需要把尺寸同时同步到 latent 与布局节点）。
/// 空数组表示**显式关闭**该语义（工作流里已固定，不允许被覆盖）。
pub fn parse_param_map(raw: &str) -> Result<HashMap<String, Vec<String>>> {
    let mut map: HashMap<String, Vec<String>> = DEFAULT_PARAM_MAP
        .iter()
        .map(|(key, value)| ((*key).to_string(), vec![(*value).to_string()]))
        .collect();

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

/// 从 history entry 的 outputs 里收集 `images` 数组元素（{filename, subfolder, type}）。
pub fn collect_output_images(outputs: Option<&Value>) -> Vec<Value> {
    let mut images = Vec::new();
    let Some(nodes) = outputs.and_then(Value::as_object) else {
        return images;
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
            if has_name {
                images.push(image.clone());
            }
        }
    }
    images
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
    if let Some(extra_map) = extra_owned.as_mut() {
        sync_layout_size(extra_map, param_map, width, height, &mut warnings);
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
        // 未覆盖的语义仍走默认
        assert_eq!(map.get("seed"), Some(&vec!["3.seed".to_string()]));
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
}
