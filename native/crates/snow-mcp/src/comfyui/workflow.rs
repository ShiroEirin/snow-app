//! API 格式工作流的纯函数工具：语义参数注入、点号键解析、输出收集、错误提取。
//!
//! 「点号键」指 `6.text` / `3.inputs.seed` 这类路径，等价于
//! `graph["6"]["inputs"]["text"]`，与 ComfyUI 导出的 API 格式一一对应。

use std::collections::HashMap;

use napi::bindgen_prelude::*;
use serde_json::{json, Value};

/// 单次生成的全局像素预算（约 4096x4096）：技术边界之外还要挡住资源滥用。
const MAX_PIXELS: u64 = 16_777_216;

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

/// 全局像素预算：**所有**尺寸来源（显式 WxH、比例候选、模型/渠道默认、
/// 模板自带）都必须通过，不能只挡显式 `WxH`。
pub fn ensure_pixel_budget(width: u64, height: u64) -> std::result::Result<(), String> {
    if width.saturating_mul(height) > MAX_PIXELS {
        return Err(format!(
            "size {width}x{height} exceeds the {MAX_PIXELS}-pixel budget (about 4096x4096); \
             pick a smaller size"
        ));
    }
    Ok(())
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
        // 精确 WxH 表达用户明确要的像素尺寸：不要求命中 supportedResolutions
        // （C-10/C-20），但必须过全局像素预算。
        ensure_pixel_budget(width, height)?;
        return Ok((width, height, true, warnings));
    }

    // 省略 size：模型/渠道默认值仍走通用清洗；被白名单过滤或不是 WxH 时只
    // 告警并回退模板（不能静默，否则调用方会以为默认尺寸生效了）。
    if let Some(raw) = resolved {
        if let Some((width, height)) = parse_size(Some(raw)) {
            // 默认尺寸同样受预算约束：不能因为「没显式传 size」就放大到 8K。
            ensure_pixel_budget(width, height)?;
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
        Some((width, height)) => {
            // 模板尺寸也不能突破预算：否则「省略 size」就是绕过口。
            ensure_pixel_budget(width, height)?;
            Ok((width, height, false, warnings))
        }
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

/// 把 `supportedResolutions` 里的像素项解析成候选；档位（`1K`）不参与比例归一化。
pub fn resolution_candidates(supported_resolutions: &str) -> Vec<(u64, u64)> {
    // 与设置面板的填写习惯保持一致：逗号/分号/竖线/空白都可分隔；
    // `1K (1792x1008)` 这种档位说明里的括号像素项也要能提取。
    supported_resolutions
        .split(|c: char| c == ',' || c == ';' || c == '|' || c.is_whitespace())
        .filter_map(|item| {
            let trimmed = item
                .trim()
                .trim_matches(|c| c == '(' || c == ')' || c == '[' || c == ']');
            parse_size(Some(trimmed))
        })
        .collect()
}

fn parse_ratio(raw: &str) -> Option<(f64, f64)> {
    let (width, height) = raw.split_once(':')?;
    let width: f64 = width.trim().parse().ok()?;
    let height: f64 = height.trim().parse().ok()?;
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    Some((width, height))
}

/// 按模型声明的像素候选把比例请求归一化（容差 2%）。
///
/// 误差最小优先；并列取像素面积最接近 `default_size` 的候选；再并列保持
/// 候选列表顺序。无候选或全部超出容差时**明确拒绝**，不静默近似。
pub fn resolve_ratio_size(
    ratio: &str,
    candidates: &[(u64, u64)],
    default_size: Option<(u64, u64)>,
) -> std::result::Result<(u64, u64), String> {
    let Some((ratio_width, ratio_height)) = parse_ratio(ratio) else {
        return Err(format!(
            "invalid aspect ratio `{ratio}`; expected `WIDTH:HEIGHT` like `16:9`"
        ));
    };
    let target = ratio_width / ratio_height;
    let mut best: Option<(f64, u64, u64, u64)> = None;
    for &(width, height) in candidates {
        if width == 0 || height == 0 {
            continue;
        }
        let error = ((width as f64 / height as f64) / target - 1.0).abs();
        if error > 0.02 {
            continue;
        }
        let area_delta = match default_size {
            Some((default_width, default_height)) => {
                (width * height).abs_diff(default_width * default_height)
            }
            None => 0,
        };
        let replace = match best {
            None => true,
            Some((best_error, best_area, _, _)) => {
                error < best_error - f64::EPSILON
                    || ((error - best_error).abs() <= f64::EPSILON && area_delta < best_area)
            }
        };
        if replace {
            best = Some((error, area_delta, width, height));
        }
    }
    match best {
        Some((_, _, width, height)) => Ok((width, height)),
        None if candidates.is_empty() => Err(format!(
            "no pixel resolutions declared for ratio `{ratio}`; configure supportedResolutions \
             (e.g. `1344x768`) or pass an explicit `WIDTHxHEIGHT`"
        )),
        None => Err(format!(
            "no declared resolution matches `{ratio}` within 2%; available: {}",
            candidates
                .iter()
                .map(|(width, height)| format!("{width}x{height}"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// 输出家族识别的结构化结果。
///
/// 调用方不能靠错误文本来决定能否用 `taskContract` 放行：已知视频/音频输出
/// 与「完全无法识别」是两种不同状态，必须区分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputFamilyIssue {
    /// 没有找到任何保存/预览/合成类终止节点。
    NoTerminal,
    /// 只有已知的非图片输出（视频 / 音频 / 3D / latent / 文本等）。
    Unsupported(Vec<String>),
    /// 同时存在图片输出与非图片/未知终止节点。
    Mixed { image: usize, others: Vec<String> },
}

impl OutputFamilyIssue {
    pub fn message(&self) -> String {
        match self {
            Self::NoTerminal => "no recognized image output node (SaveImage / PreviewImage / \
                 SaveAnimated*) in this workflow; the native ComfyUI provider only accepts image \
                 workflows"
                .to_string(),
            Self::Unsupported(classes) => format!(
                "workflow only produces non-image outputs ({}); the native ComfyUI provider \
                 currently supports image workflows only",
                classes.join(", ")
            ),
            Self::Mixed { image, others } => format!(
                "workflow mixes {image} image output(s) with non-image output(s) ({}); \
                 mixed-media workflows are not supported by the image provider",
                others.join(", ")
            ),
        }
    }
}

/// 终止节点分类：`Some(true)` = 图片，`Some(false)` = 已知非图片，`None` = 不是终止节点。
fn terminal_output_kind(class: &str) -> Option<bool> {
    let is_terminal = class.contains("save")
        || class.contains("preview")
        || class.contains("combine")
        || class.contains("export");
    if !is_terminal {
        return None;
    }
    // 图片白名单必须先判：SaveAnimatedWEBP 同时含 "save"/"webp"。
    if class.contains("saveimage")
        || class.contains("previewimage")
        || class.contains("imagesave")
        || class.contains("saveanimated")
    {
        return Some(true);
    }
    let non_image = class.contains("video")
        || class.contains("audio")
        || class.contains("sound")
        || class.contains("webm")
        || class.contains("mesh")
        || class.contains("gltf")
        || class.contains("glb")
        || class.contains("latent")
        || class.contains("text")
        || class.contains("3d");
    if non_image {
        Some(false)
    } else {
        // save/preview/combine 但无法归类：不把它当成终止节点，避免把
        // PreviewBridge 这类中间节点误判成输出；已知媒体关键字已在上方拦住。
        None
    }
}

/// 提交前的输出家族识别（结构化版本）。
///
/// 视频/音频/混合媒体或完全无法识别的图都在 `/prompt` 之前拒绝，避免烧完
/// GPU 才因为「没有图片输出」报错。
pub fn classify_output_family_detail(graph: &Value) -> std::result::Result<(), OutputFamilyIssue> {
    let Some(nodes) = graph.as_object() else {
        return Err(OutputFamilyIssue::NoTerminal);
    };
    let mut image_outputs = 0usize;
    let mut non_image_outputs: Vec<String> = Vec::new();
    for node in nodes.values() {
        let class = node
            .get("class_type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        match terminal_output_kind(&class) {
            Some(true) => image_outputs += 1,
            Some(false) => non_image_outputs.push(class),
            None => {}
        }
    }
    if image_outputs == 0 {
        return Err(if non_image_outputs.is_empty() {
            OutputFamilyIssue::NoTerminal
        } else {
            non_image_outputs.sort();
            non_image_outputs.dedup();
            OutputFamilyIssue::Unsupported(non_image_outputs)
        });
    }
    if !non_image_outputs.is_empty() {
        non_image_outputs.sort();
        non_image_outputs.dedup();
        return Err(OutputFamilyIssue::Mixed {
            image: image_outputs,
            others: non_image_outputs,
        });
    }
    Ok(())
}

/// 多参考图角色消歧：没有 Autogrow 槽位、但有多个 LoadImage 时，不能按节点
/// ID 顺序猜 base / mask / control / reference 角色；必须由 `taskContract.inputs`
/// 显式声明。单张参考图或存在 Autogrow 槽位时不受影响。
pub fn ensure_reference_roles(
    graph: &Value,
    explicit_image_slots: bool,
    requested_images: usize,
    declared_roles: Option<&Value>,
) -> std::result::Result<(), String> {
    if requested_images < 2 {
        return Ok(());
    }
    let Some(nodes) = graph.as_object() else {
        return Ok(());
    };
    let has_autogrow = nodes.values().any(|node| {
        node.get("inputs")
            .and_then(Value::as_object)
            .map(|inputs| inputs.keys().any(|key| key.starts_with("images.image_")))
            .unwrap_or(false)
    });
    if has_autogrow {
        return Ok(());
    }
    // 只有**用户显式**在 paramMap 里同时映射 image 与 image2 才算解歧；
    // 自动推导出来的 image/image2 不能用来证明角色已明确。
    if explicit_image_slots {
        return Ok(());
    }
    let loaders = nodes
        .values()
        .filter(|node| {
            node.get("class_type")
                .and_then(Value::as_str)
                .map(|class| class.contains("LoadImage"))
                .unwrap_or(false)
        })
        .count();
    if loaders < 2 {
        return Ok(());
    }
    // 只声明 role、不声明有效 `nodePath` 不算解歧：必须覆盖
    // `image` / `image2` … `imageN` 这些**具体槽位**，数量够但缺 image2
    // 仍然会让第二张图走自动映射。
    let declared_paths = declared_image_slot_map(declared_roles, graph).unwrap_or_default();
    let covers_all_slots = (0..requested_images).all(|index| {
        let semantic = if index == 0 {
            "image".to_string()
        } else {
            format!("image{}", index + 1)
        };
        declared_paths.contains_key(&semantic)
    });
    if covers_all_slots {
        return Ok(());
    }
    Err(format!(
        "this workflow has {loaders} LoadImage nodes and no autogrow slots; multiple \
         reference-image roles (base / mask / control / reference) are ambiguous — declare them \
         in the model's taskContract.inputs or pass a single reference image"
    ))
}

/// 把 `taskContract.inputs` 的显式 `nodePath` 变成真正的注入目标。
///
/// 仅仅「声明了 role」不足以放行：如果执行侧仍按 LoadImage 节点 ID 排序，
/// 声明为 base 的图可能被写进 mask 槽。这里校验并在执行时直接使用显式路径。
pub fn declared_image_slot_map(
    declared: Option<&Value>,
    graph: &Value,
) -> std::result::Result<HashMap<String, Vec<String>>, String> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    let Some(roles) = declared.and_then(Value::as_object) else {
        return Ok(map);
    };
    let mut used: HashMap<String, String> = HashMap::new();
    for (name, entry) in roles {
        let is_image_name = name == "image"
            || name
                .strip_prefix("image")
                .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()));
        if !is_image_name {
            continue;
        }
        let Some(path) = entry
            .get("nodePath")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|path| !path.is_empty())
        else {
            return Err(format!(
                "taskContract.inputs.`{name}` must declare a `nodePath` like \"9.image\""
            ));
        };
        let Some((node_id, input_name)) = split_node_input(path) else {
            return Err(format!(
                "taskContract.inputs.`{name}`.nodePath must be a dotted node input like \"9.image\""
            ));
        };
        if input_name != "image" {
            return Err(format!(
                "taskContract.inputs.`{name}`.nodePath must target the `image` input of a LoadImage node"
            ));
        }
        let class = graph
            .get(&node_id)
            .and_then(|node| node.get("class_type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !class.contains("LoadImage") {
            return Err(format!(
                "taskContract.inputs.`{name}`.nodePath points at node {node_id} ({class}), which is not a LoadImage node"
            ));
        }
        if read_path(graph, path).is_none() {
            return Err(format!(
                "taskContract.inputs.`{name}`.nodePath `{path}` does not exist in the workflow"
            ));
        }
        let canonical = normalize_path(path);
        if let Some(previous) = used.insert(canonical, name.clone()) {
            return Err(format!(
                "taskContract.inputs.`{name}` and `{previous}` point at the same image input `{path}`"
            ));
        }
        map.insert(name.clone(), vec![path.to_string()]);
    }
    Ok(map)
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

/// 目标输入当前是否是连线（`[node_id, slot]` 且 node_id 存在于图中）。
///
/// 语义参数与 `params` 覆盖都必须保留连线：直接写入会把连线替换成字面值，
/// 模型收不到上游输出，甚至提交失败。
pub fn is_link_target(graph: &Value, path: &str) -> bool {
    let Some(value) = read_path(graph, path) else {
        return false;
    };
    let Some(pair) = value.as_array() else {
        return false;
    };
    if pair.len() != 2 {
        return false;
    }
    let Some(node) = pair[0].as_str() else {
        return false;
    };
    pair[1].as_i64().is_some() && graph.get(node).is_some()
}

/// 点号键 → `(节点 id, 输入名)`；`5.image` 与 `5.inputs.image` 归一为同一输入。
fn split_node_input(path: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = path
        .split('.')
        .filter(|part| !part.trim().is_empty())
        .collect();
    let node_id = parts.first()?.to_string();
    let input_name = if parts
        .get(1)
        .map(|part| part.eq_ignore_ascii_case("inputs"))
        .unwrap_or(false)
    {
        parts[2..].join(".")
    } else {
        parts[1..].join(".")
    };
    if input_name.is_empty() {
        return None;
    }
    Some((node_id, input_name))
}

/// 点号键在 `/object_info` 的 required/optional 中是否声明。
///
/// 用于允许「图里没存、但节点定义支持」的合法可选输入（如
/// `MOSSLayoutBoard.strict`），同时拦住拼错的可选键。
pub fn input_declared_in_object_info(
    object_info: Option<&Value>,
    graph: &Value,
    path: &str,
) -> bool {
    let Some(info) = object_info else {
        return false;
    };
    let Some((node_id, input_name)) = split_node_input(path) else {
        return false;
    };
    let Some(class_type) = graph
        .get(&node_id)
        .and_then(|node| node.get("class_type"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    let Some(input) = info.get(class_type).and_then(|node| node.get("input")) else {
        return false;
    };
    ["required", "optional"].iter().any(|section| {
        input
            .get(*section)
            .and_then(|declared| declared.get(&input_name))
            .is_some()
    })
}

/// 节点输入的技术边界（来自 `/object_info`）：标量类型、枚举、min/max/step。
///
/// 只描述引擎声明的硬约束；Agent 权限（exposure）由 `policy` 单独负责，
/// 两者同时满足才允许写入。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InputBounds {
    pub scalar: Option<crate::comfyui::policy::ScalarKind>,
    pub options: Option<Vec<String>>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// 精确整数边界：seed 级别的 u64 在 f64 比较中会丢精度。
    pub min_int: Option<i128>,
    pub max_int: Option<i128>,
    pub step: Option<f64>,
}

fn number_field(config: &serde_json::Map<String, Value>, key: &str) -> Option<f64> {
    match config.get(key) {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn int_field(config: &serde_json::Map<String, Value>, key: &str) -> Option<i128> {
    match config.get(key) {
        Some(Value::Number(number)) => number
            .as_i64()
            .map(i128::from)
            .or_else(|| number.as_u64().map(i128::from)),
        Some(Value::String(text)) => text.trim().parse::<i128>().ok(),
        _ => None,
    }
}

/// 资源选项列表：**空数组也是约束**（本机没装任何可用值），不能当成「无约束」。
fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    let items = value?.as_array()?;
    Some(
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    )
}

/// 读取某个点号键的节点输入声明；节点 / 输入未声明时返回 None。
pub fn input_bounds(object_info: Option<&Value>, graph: &Value, path: &str) -> Option<InputBounds> {
    let info = object_info?;
    let (node_id, input_name) = split_node_input(path)?;
    let class_type = graph
        .get(&node_id)
        .and_then(|node| node.get("class_type"))
        .and_then(Value::as_str)?;
    let input = info.get(class_type).and_then(|node| node.get("input"))?;
    let declaration = ["required", "optional"]
        .iter()
        .find_map(|section| input.get(*section)?.get(&input_name))?;
    // ComfyUI 的声明是 `[<type-or-options>, {options}]`；旧式 COMBO 把选项放第二位。
    let declaration = declaration.as_array()?;
    let mut bounds = InputBounds::default();
    match declaration.first()? {
        Value::String(kind) => {
            let kind = kind.trim().to_ascii_uppercase();
            bounds.scalar = match kind.as_str() {
                "INT" => Some(crate::comfyui::policy::ScalarKind::Integer),
                "FLOAT" => Some(crate::comfyui::policy::ScalarKind::Float),
                "STRING" => Some(crate::comfyui::policy::ScalarKind::String),
                "BOOLEAN" => Some(crate::comfyui::policy::ScalarKind::Boolean),
                _ => None,
            };
            if kind == "COMBO" {
                bounds.options = string_list(declaration.get(1));
                if bounds.options.is_some() {
                    bounds.scalar = Some(crate::comfyui::policy::ScalarKind::String);
                }
            }
        }
        Value::Array(_) => {
            bounds.options = string_list(declaration.first());
            if bounds.options.is_some() {
                bounds.scalar = Some(crate::comfyui::policy::ScalarKind::String);
            }
        }
        _ => {}
    }
    if let Some(config) = declaration.get(1).and_then(Value::as_object) {
        bounds.min = number_field(config, "min");
        bounds.max = number_field(config, "max");
        bounds.min_int = int_field(config, "min");
        bounds.max_int = int_field(config, "max");
        bounds.step = number_field(config, "step");
        // COMBO 的选项也可能写在 `{options: [...]}` 里。
        if bounds.options.is_none() {
            bounds.options = string_list(config.get("options"));
            if bounds.options.is_some() {
                bounds.scalar = Some(crate::comfyui::policy::ScalarKind::String);
            }
        }
    }
    Some(bounds)
}

/// 按节点声明校验一个将被写入的值。
///
/// `skip_options` 用于参考图槽位：上传后的文件名不在 120 秒缓存的
/// `/object_info` 里，按枚举校验会把刚上传的图误判成非法。
pub fn check_input_bounds(
    bounds: &InputBounds,
    value: &Value,
    label: &str,
    skip_options: bool,
) -> Option<String> {
    if let Some(kind) = bounds.scalar {
        if let Some(problem) = crate::comfyui::policy::check_scalar(kind, value) {
            return Some(format!("`{label}` {problem}"));
        }
    }
    if !skip_options {
        if let (Some(options), Some(text)) = (&bounds.options, value.as_str()) {
            if !options.iter().any(|option| option == text) {
                return Some(format!(
                    "`{label}` must be one of [{}], got `{text}`",
                    options.join(", ")
                ));
            }
        }
    }
    // 整数走 i128 精确比较：u64 量级的 seed 不能因为 f64 丢精度而绕过
    // min/max/step（`u64::MAX` 与 max=u64::MAX 必须判等）。
    let value_int = value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from));
    if let Some(integer) = value_int {
        let min_int = bounds
            .min_int
            .or_else(|| bounds.min.map(|min| min as i128));
        if let Some(min) = min_int {
            if integer < min {
                return Some(format!("`{label}` must be >= {min}, got {integer}"));
            }
        }
        let max_int = bounds
            .max_int
            .or_else(|| bounds.max.map(|max| max as i128));
        if let Some(max) = max_int {
            if integer > max {
                return Some(format!("`{label}` must be <= {max}, got {integer}"));
            }
        }
        if let Some(step) = bounds.step.filter(|step| *step >= 1.0) {
            let step = step as i128;
            let base = min_int.unwrap_or(0);
            if (integer - base) % step != 0 {
                return Some(format!(
                    "`{label}` must align to step {step} from {base}, got {integer}"
                ));
            }
        }
        return None;
    }
    // 非整数（FLOAT）：f64 比较 + 容差 step。
    let Some(number) = value.as_f64() else {
        return None;
    };
    const EXACT_LIMIT: f64 = 9_007_199_254_740_992.0;
    if let Some(min) = bounds.min {
        if min.abs() <= EXACT_LIMIT && number < min {
            return Some(format!("`{label}` must be >= {min}, got {number}"));
        }
    }
    if let Some(max) = bounds.max {
        if max.abs() <= EXACT_LIMIT && number > max {
            return Some(format!("`{label}` must be <= {max}, got {number}"));
        }
    }
    let Some(step) = bounds.step.filter(|step| *step > 0.0) else {
        return None;
    };
    let base = bounds.min.unwrap_or(0.0);
    let ratio = (number - base) / step;
    let remainder = (ratio - ratio.round()).abs();
    if remainder > 1e-6 {
        return Some(format!(
            "`{label}` must align to step {step} from {base}, got {number}"
        ));
    }
    None
}

/// `params` 值的写入形态：对象 / 数组会被序列化成 JSON 文本（与
/// `apply_extra_params` 一致），校验必须针对最终写入值而不是原始形状。
pub fn serialize_param_value(value: &Value) -> Value {
    match value {
        Value::Object(_) | Value::Array(_) => Value::String(value.to_string()),
        other => other.clone(),
    }
}

/// Workflow Readiness：工作流引用、但 `/object_info` 里不存在的节点类。
///
/// 只做只读诊断，不自动安装节点；空数组表示已知节点全部可用。
pub fn missing_node_classes(graph: &Value, object_info: &Value) -> Vec<String> {
    let mut missing: Vec<String> = Vec::new();
    let Some(nodes) = graph.as_object() else {
        return missing;
    };
    for node in nodes.values() {
        let Some(class) = node.get("class_type").and_then(Value::as_str) else {
            continue;
        };
        if class.trim().is_empty() || object_info.get(class).is_some() {
            continue;
        }
        missing.push(class.to_string());
    }
    missing.sort();
    missing.dedup();
    missing
}

/// Workflow Readiness：模板里引用的**资源文件**是否装在本机。
///
/// 只检查节点声明为 combo 的模型类输入（checkpoint / unet / vae / clip / lora）。
/// 参考图、视频、音频这些运行时上传的媒体不参与，否则每份示例模板都会因为
/// 模板里的示例文件名不存在而误报「未就绪」。只做只读诊断，不自动下载。
pub fn missing_model_files(graph: &Value, object_info: &Value) -> Vec<String> {
    let mut missing: Vec<String> = Vec::new();
    let Some(nodes) = graph.as_object() else {
        return missing;
    };
    for (node_id, node) in nodes {
        let class = node
            .get("class_type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if class.is_empty()
            || class.contains("LoadImage")
            || class.contains("LoadVideo")
            || class.contains("LoadAudio")
        {
            continue;
        }
        let Some(inputs) = node.get("inputs").and_then(Value::as_object) else {
            continue;
        };
        for (name, value) in inputs {
            if name == "image" || name == "video" || name == "audio" || name.contains("text") {
                continue;
            }
            let Some(text) = value.as_str() else {
                continue;
            };
            if text.trim().is_empty() {
                continue;
            }
            let path = format!("{node_id}.{name}");
            let Some(bounds) = input_bounds(Some(object_info), graph, &path) else {
                continue;
            };
            let Some(options) = bounds.options.as_ref() else {
                continue;
            };
            if !options.iter().any(|option| option == text) {
                missing.push(format!("{class}.{name} = {text} (not installed)"));
            }
        }
    }
    missing.sort();
    missing.dedup();
    missing
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
        if is_link_target(graph, path) {
            warnings.push(format!(
                "paramMap \"{label}\" -> \"{path}\" targets a link; wiring was preserved"
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
        let value = serialize_param_value(raw_value);
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
                // 工具已注入该路径（prompt / negative / seed / width / height / image）。
                // 不能静默丢弃 params：值不同时必须告诉调用方实际生效的是哪一个。
                if is_link_target(graph, &target) {
                    continue;
                }
                if read_path(graph, &target) != Some(&value) {
                    warnings.push(format!(
                        "params target \"{target}\" is also set by a top-level parameter; the top-level value is used and the params value was ignored"
                    ));
                }
                continue;
            }
            if is_link_target(graph, &target) {
                warnings.push(format!(
                    "params target \"{target}\" is wired to another node; wiring was preserved"
                ));
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

/// 布局文档的结构 + 越界检查。JSON 合法 ≠ MOSS 布局合法：
/// 缺 `items` / `box`、`label=text` 没有 `text`、坐标不是 4 个整数都必须拒绝。
///
/// 字段口径按 MOSS 布局规范：`box` 必填（4 个整数）；`label` 可省略（默认
/// `object`）；`caption` 必填，但 `label=text` 的项可以用非空 `text` 代替。
fn layout_problems(layout: &Value, width: u64, height: u64) -> Vec<String> {
    let mut problems: Vec<String> = Vec::new();
    let Some(map) = layout.as_object() else {
        return vec!["layout must be a JSON object".to_string()];
    };
    let Some(items) = map.get("items").and_then(Value::as_array) else {
        return vec!["layout.items must be an array".to_string()];
    };
    if items.is_empty() {
        return vec!["layout.items must contain at least one item".to_string()];
    }
    let max_width = i64::try_from(width).unwrap_or(i64::MAX);
    let max_height = i64::try_from(height).unwrap_or(i64::MAX);
    let mut out_of_bounds = 0usize;
    for (index, item) in items.iter().enumerate() {
        let Some(object) = item.as_object() else {
            problems.push(format!("layout.items[{index}] must be an object"));
            continue;
        };
        // label 可省略（默认 object）；未知类别不在这里拒绝——编译阶段把它当
        // 普通元素处理，结构校验只保证 `box` / `caption` / `text` 的完整性。
        if let Some(label) = object.get("label") {
            if !label.is_string() {
                problems.push(format!(
                    "layout.items[{index}].label must be a string"
                ));
            }
        }
        let label = object.get("label").and_then(Value::as_str).unwrap_or("object");
        let text = object
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty());
        if label == "text" && text.is_none() {
            problems.push(format!(
                "layout.items[{index}] has label `text` but no non-empty `text`"
            ));
        }
        let caption = object
            .get("caption")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|caption| !caption.is_empty());
        if caption.is_none() && text.is_none() {
            problems.push(format!(
                "layout.items[{index}] is missing a non-empty `caption` (required unless the item is a `text` item)"
            ));
        }
        if let Some(caption) = object.get("caption") {
            if !caption.is_string() {
                problems.push(format!("layout.items[{index}].caption must be a string"));
            }
        }
        let Some(values) = object.get("box").and_then(Value::as_array) else {
            problems.push(format!("layout.items[{index}] is missing `box`"));
            continue;
        };
        if values.len() != 4 {
            problems.push(format!(
                "layout.items[{index}].box must contain exactly 4 integers, got {}",
                values.len()
            ));
            continue;
        }
        let Some(coords) = values.iter().map(Value::as_i64).collect::<Option<Vec<i64>>>() else {
            problems.push(format!(
                "layout.items[{index}].box must contain only integers"
            ));
            continue;
        };
        let (x1, y1, x2, y2) = (coords[0], coords[1], coords[2], coords[3]);
        if x1 < 0 || y1 < 0 || x2 <= x1 || y2 <= y1 || x2 > max_width || y2 > max_height {
            out_of_bounds += 1;
        }
    }
    if out_of_bounds > 0 {
        problems.push(format!(
            "layout has {out_of_bounds} box(es) outside {width}x{height}; adjust the boxes or size (boxes were not changed automatically)"
        ));
    }
    problems
}

/// 把布局问题压成一条可操作消息（最多列 4 条）。
fn layout_problem_message(layout: &Value, width: u64, height: u64) -> Option<String> {
    let problems = layout_problems(layout, width, height);
    if problems.is_empty() {
        return None;
    }
    let shown = problems.iter().take(4).cloned().collect::<Vec<_>>().join("; ");
    let more = if problems.len() > 4 {
        format!(" (+{} more)", problems.len() - 4)
    } else {
        String::new()
    };
    Some(format!("{shown}{more}"))
}

/// 模板里已有布局 JSON、但本次请求没有覆盖 layout 时，直接同步它的宽高。
///
/// 不自动改 `items[].box`：构图属于用户/工作流作者的决定。box 越界时返回
/// 说明文本，由调用方决定是否拒绝提交（默认拒绝，显式允许才继续）。
fn sync_template_layout_size(
    graph: &mut Value,
    param_map: &HashMap<String, Vec<String>>,
    extra: Option<&serde_json::Map<String, Value>>,
    width: u64,
    height: u64,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let paths = param_map.get("layout").cloned().unwrap_or_default();
    if paths.is_empty() {
        return None;
    }
    if let Some(extra) = extra {
        let has_layout = extra.keys().any(|key| {
            key == "layout"
                || paths.iter().any(|path| {
                    canonical_segments(path).join(".") == canonical_segments(key).join(".")
                })
        });
        if has_layout {
            return None;
        }
    }
    let mut overflow: Option<String> = None;
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
        if let Some(message) = layout_problem_message(&layout, width, height) {
            warnings.push(message.clone());
            if overflow.is_none() {
                overflow = Some(message);
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
    overflow
}

/// 从有效 `paramMap` 中剔除被锁定的目标路径。
///
/// 这是所有写入的**最后一道统一收口**：无论语义来自显式参数、隐式默认值
/// 还是布局尺寸同步，只要目标在锁集合里就不写。显式写入的拒绝在
/// `providers_comfyui` 里先行返回错误；这里兜住隐式/同步路径。
pub fn filter_locked_paths(
    param_map: &HashMap<String, Vec<String>>,
    locked_paths: &std::collections::HashSet<String>,
) -> HashMap<String, Vec<String>> {
    param_map
        .iter()
        .map(|(semantic, paths)| {
            let kept: Vec<String> = paths
                .iter()
                .filter(|path| {
                    !locked_paths.contains(&normalize_path(path).to_ascii_lowercase())
                })
                .cloned()
                .collect();
            (semantic.clone(), kept)
        })
        .collect()
}

/// 一次请求的可提交图 + 注入记录（不含参考图上传，那一步需要异步 HTTP）。
pub struct PreparedGraph {
    pub graph: Value,
    pub injected: Vec<String>,
    pub warnings: Vec<String>,
    /// 布局 box 越界说明；默认应拒绝提交，显式允许时才只保留告警。
    pub layout_overflow: Option<String>,
    /// 注入后图里的**最终**宽高（读不到时回退模板值）。调用方必须用它
    /// 复查像素预算，不能只信 resolve 阶段的默认尺寸。
    pub effective_size: Option<(u64, u64)>,
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
    // 被锁定的目标路径：隐式/同步写入也不得落地（显式写入已在调用层拒绝）。
    locked_paths: &std::collections::HashSet<String>,
) -> PreparedGraph {
    let mut warnings: Vec<String> = Vec::new();
    let mut injected: Vec<String> = Vec::new();
    let mut graph = template.clone();
    // 最后一道统一收口：任何语义（含默认 seed、默认尺寸、布局同步）只要
    // 目标被锁就不写。这里保留原始映射用于读取模板当前值。
    let raw_param_map = param_map;
    let size_target_locked = ["width", "height"].iter().any(|semantic| {
        raw_param_map
            .get(*semantic)
            .map(|paths| {
                paths.iter().any(|path| {
                    locked_paths.contains(&normalize_path(path).to_ascii_lowercase())
                })
            })
            .unwrap_or(false)
    });
    let mut param_map = filter_locked_paths(raw_param_map, locked_paths);
    // width/height 必须成对：任一侧被锁就整体保留模板尺寸，避免只改一边。
    if size_target_locked {
        param_map.remove("width");
        param_map.remove("height");
        warnings.push(
            "width/height are locked for this workflow; the template size is preserved"
                .to_string(),
        );
    }
    let param_map = &param_map;

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
    let mut layout_overflow = None;
    if sync_size {
        if let Some(extra_map) = extra_owned.as_mut() {
            sync_layout_size(extra_map, param_map, width, height, &mut warnings);
        }
        // 本次没有覆盖 layout 时，模板里的布局 JSON 也要跟着 size 走，
        // 否则 latent / 布局节点 / 布局文档三处尺寸会不一致。
        layout_overflow = sync_template_layout_size(
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

    // 覆盖后的最终布局也要检查：显式 `params.layout` 不能绕过 box 越界。
    // 1) 即使 `sync_size=false`（省略 size）也要用模板当前尺寸检查；
    // 2) 遍历**所有** layout 映射，不能只看第一处。
    let check_size = if sync_size {
        (width, height)
    } else {
        let current = |semantic: &str| -> Option<u64> {
            let path = raw_param_map.get(semantic)?.first()?;
            // 图里读不到（映射被关闭/锁定）时回退模板值：最终执行仍按模板尺寸。
            read_path(&graph, path)
                .or_else(|| read_path(template, path))
                .and_then(Value::as_u64)
        };
        match (current("width"), current("height")) {
            (Some(width), Some(height)) => (width, height),
            _ => (width, height),
        }
    };
    let mut layout_messages: Vec<String> = Vec::new();
    for path in param_map.get("layout").map(Vec::as_slice).unwrap_or(&[]) {
        let Some(text) = read_path(&graph, path).and_then(Value::as_str) else {
            continue;
        };
        let Ok(final_layout) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if let Some(message) = layout_problem_message(&final_layout, check_size.0, check_size.1) {
            if !layout_messages.iter().any(|existing| existing == &message) {
                layout_messages.push(message);
            }
        }
    }
    if !layout_messages.is_empty() {
        layout_overflow = Some(layout_messages.join(" | "));
    }

    // 已识别采样链的 negative 在 CFG=1 时没有实际作用；按本次覆盖后的有效值
    // 判断，避免 Agent 以为负向词已经生效。
    if negative.is_some() {
        if let Some(cfg) = effective_number(&graph, param_map.get("cfg")) {
            if (cfg - 1.0).abs() <= f64::EPSILON {
                warnings.push(
                    "negative prompt has no effect at CFG=1; raise cfg above 1 to make it effective"
                        .to_string(),
                );
            }
        }
    }

    PreparedGraph {
        graph,
        injected,
        warnings,
        layout_overflow,
        effective_size: Some(check_size),
    }
}

/// 读取某个语义在**注入后**图里的有效数值（用于 CFG 等条件判断）。
fn effective_number(graph: &Value, paths: Option<&Vec<String>>) -> Option<f64> {
    for path in paths? {
        if let Some(value) = read_path(graph, path).and_then(Value::as_f64) {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助：默认无锁；需要验证锁语义的用例直接调用 `prepare_graph`。
    fn prepare_graph_unlocked(
        template: &Value,
        param_map: &HashMap<String, Vec<String>>,
        prompt: &str,
        negative: Option<&str>,
        width: u64,
        height: u64,
        seed: u64,
        extra: Option<&serde_json::Map<String, Value>>,
        sync_size: bool,
    ) -> PreparedGraph {
        prepare_graph(
            template,
            param_map,
            prompt,
            negative,
            width,
            height,
            seed,
            extra,
            sync_size,
            &std::collections::HashSet::new(),
        )
    }

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
        let prepared = prepare_graph_unlocked(
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
        let prepared = prepare_graph_unlocked(
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
        let prepared = prepare_graph_unlocked(
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
        let prepared = prepare_graph_unlocked(&template, &map, "p", None, 1024, 768, 1, None, true);
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

    /// 精确 WxH 不要求命中 supportedResolutions（C-10/C-20）。
    #[test]
    fn accepts_explicit_pixel_size_outside_capabilities() {
        let (width, height, sync_size, _) = resolve_comfy_size(
            Some("1536x1024"),
            None,
            None,
            Some((512, 512)),
            "supportedResolutions=`1K`",
        )
        .expect("explicit pixel size is a user decision");
        assert_eq!((width, height), (1536, 1024));
        assert!(sync_size);
    }

    /// 超出全局像素预算的尺寸必须拒绝。
    #[test]
    fn rejects_size_above_pixel_budget() {
        let error = resolve_comfy_size(Some("100000x100000"), None, None, None, "")
            .expect_err("pixel budget");
        assert!(error.contains("pixel budget"), "{error}");
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
        let prepared = prepare_graph_unlocked(&template, &map, "a prompt", None, 1536, 1024, 7, None, true);
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
        let prepared = prepare_graph_unlocked(&template, &map, "a", None, 1536, 1024, 7, None, true);
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

    /// 尺寸变化导致布局 box 越界时，必须把越界说明带回给调用方（默认拒绝）。
    #[test]
    fn reports_layout_overflow_for_box_outside_requested_size() {
        let template = json!({
            "20": { "class_type": "MOSSLayoutBoard", "inputs": {
                "width": 1536,
                "height": 1024,
                "layout": "{\"width\":1536,\"height\":1024,\"items\":[{\"caption\":\"a wide banner\",\"box\":[0,0,1400,900]}]}"
            } }
        });
        let mut map = HashMap::new();
        map.insert("layout".to_string(), vec!["20.layout".to_string()]);
        let prepared = prepare_graph_unlocked(&template, &map, "a", None, 1024, 1024, 7, None, true);
        let overflow = prepared.layout_overflow.expect("overflow reported");
        assert!(overflow.contains("outside 1024x1024"), "{overflow}");
    }

    /// box 全部在新画布内时不得误报越界。
    #[test]
    fn keeps_layout_overflow_empty_when_boxes_fit() {
        let template = json!({
            "20": { "class_type": "MOSSLayoutBoard", "inputs": {
                "width": 512,
                "height": 512,
                "layout": "{\"width\":512,\"height\":512,\"items\":[{\"caption\":\"a square subject\",\"box\":[0,0,400,400]}]}"
            } }
        });
        let mut map = HashMap::new();
        map.insert("layout".to_string(), vec!["20.layout".to_string()]);
        let prepared = prepare_graph_unlocked(&template, &map, "a", None, 1024, 1024, 7, None, true);
        assert!(prepared.layout_overflow.is_none());
    }

    /// 2% 容差内的比例归一化：16:9 选中 1344x768（误差约 1.56%）。
    #[test]
    fn normalizes_ratio_within_two_percent() {
        let (width, height) = resolve_ratio_size("16:9", &[(1344, 768), (1024, 1024)], None)
            .expect("close candidate");
        assert_eq!((width, height), (1344, 768));
    }

    /// 超出 2% 的候选必须拒绝，不能取最近值。
    #[test]
    fn rejects_ratio_without_close_candidate() {
        let error = resolve_ratio_size("21:9", &[(1024, 1024)], None)
            .expect_err("no candidate within 2%");
        assert!(error.contains("within 2%"), "{error}");
    }

    /// 没有声明任何像素候选时，提示补配置或改用显式 WxH。
    #[test]
    fn rejects_ratio_without_candidates() {
        let error = resolve_ratio_size("16:9", &[], None).expect_err("no candidates");
        assert!(error.contains("no pixel resolutions"), "{error}");
    }

    /// 只有图片输出节点时允许走 imagegen 管线。
    #[test]
    fn classifies_image_outputs() {
        let graph = json!({
            "1": { "class_type": "SaveImage", "inputs": { "images": ["2", 0] } }
        });
        assert!(classify_output_family_detail(&graph).is_ok());
    }

    /// 没有可识别的图片输出时，提交前拒绝（不能烧完 GPU 才报无图片）。
    /// 纯视频工作流是**已知非图片**（`Unsupported`），不是「未知」。
    #[test]
    fn rejects_workflow_without_image_output() {
        let graph = json!({
            "1": { "class_type": "SaveVideo", "inputs": { "video": ["2", 0] } }
        });
        let issue = classify_output_family_detail(&graph).expect_err("video only must fail");
        match &issue {
            OutputFamilyIssue::Unsupported(classes) => {
                assert_eq!(classes, &vec!["savevideo".to_string()]);
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
        assert!(issue.message().contains("non-image"), "{}", issue.message());
    }

    /// 图片 + 视频混合输出属于混合媒体，同样拒绝。
    #[test]
    fn rejects_mixed_media_outputs() {
        let graph = json!({
            "1": { "class_type": "SaveImage", "inputs": { "images": ["2", 0] } },
            "2": { "class_type": "SaveVideo", "inputs": { "video": ["3", 0] } }
        });
        let issue = classify_output_family_detail(&graph).expect_err("mixed media must fail");
        match &issue {
            OutputFamilyIssue::Mixed { image, others } => {
                assert_eq!(*image, 1);
                assert_eq!(others, &vec!["savevideo".to_string()]);
            }
            other => panic!("expected Mixed, got {other:?}"),
        }
    }

    /// 自定义节点的视频合成器（名字里没有 save/preview）也必须被识别为
    /// 非图片输出，不能被漏检成「只有图片」。
    #[test]
    fn rejects_mixed_output_with_custom_video_combiner() {
        let graph = json!({
            "1": { "class_type": "SaveImage", "inputs": { "images": ["2", 0] } },
            "2": { "class_type": "VHS_VideoCombine", "inputs": { "images": ["3", 0] } }
        });
        let issue =
            classify_output_family_detail(&graph).expect_err("VHS_VideoCombine must be mixed");
        match &issue {
            OutputFamilyIssue::Mixed { image, others } => {
                assert_eq!(*image, 1);
                assert_eq!(others, &vec!["vhs_videocombine".to_string()]);
            }
            other => panic!("expected Mixed, got {other:?}"),
        }
    }

    /// 纯自定义视频合成器没有任何图片输出 → Unsupported（而不是 NoTerminal）。
    #[test]
    fn rejects_custom_video_combiner_only() {
        let graph = json!({
            "1": { "class_type": "VHS_VideoCombine", "inputs": { "images": ["2", 0] } }
        });
        let issue = classify_output_family_detail(&graph).expect_err("video only must fail");
        assert!(matches!(issue, OutputFamilyIssue::Unsupported(_)), "{issue:?}");
    }

    /// 完全没有终止节点才是 NoTerminal。
    #[test]
    fn reports_no_terminal_output_separately() {
        let graph = json!({
            "1": { "class_type": "KSampler", "inputs": {} }
        });
        let issue = classify_output_family_detail(&graph).expect_err("no terminal");
        assert_eq!(issue, OutputFamilyIssue::NoTerminal);
    }

    /// 多 LoadImage 且无 Autogrow / 无角色声明时，多参考图必须拒绝。
    #[test]
    fn rejects_ambiguous_multi_loader_without_roles() {
        let graph = json!({
            "1": { "class_type": "LoadImage", "inputs": { "image": "a.png" } },
            "2": { "class_type": "LoadImage", "inputs": { "image": "b.png" } }
        });
        let error =
            ensure_reference_roles(&graph, false, 2, None).expect_err("ambiguous roles");
        assert!(error.contains("taskContract.inputs"), "{error}");
    }

    /// Autogrow 槽位是节点显式声明的多图输入，不需要额外角色声明。
    #[test]
    fn accepts_autogrow_slots_for_multiple_images() {
        let graph = json!({
            "1": { "class_type": "LoadImage", "inputs": { "image": "a.png" } },
            "2": { "class_type": "LoadImage", "inputs": { "image": "b.png" } },
            "3": { "class_type": "TextEncodeQwenImage21", "inputs": {
                "images.image_1": ["1", 0],
                "images.image_2": ["2", 0]
            } }
        });
        assert!(ensure_reference_roles(&graph, false, 2, None).is_ok());
    }

    /// taskContract 显式声明了角色时可以放行。
    #[test]
    fn accepts_declared_reference_roles() {
        let graph = json!({
            "1": { "class_type": "LoadImage", "inputs": { "image": "a.png" } },
            "2": { "class_type": "LoadImage", "inputs": { "image": "b.png" } }
        });
        let roles = json!({
            "image": { "role": "base", "nodePath": "1.image" },
            "image2": { "role": "mask", "nodePath": "2.image" }
        });
        assert!(ensure_reference_roles(&graph, false, 2, Some(&roles)).is_ok());
        // 只有 role、没有 nodePath 的声明不能放行：执行侧仍会按节点顺序分图。
        let role_only = json!({
            "image": { "role": "base" },
            "image2": { "role": "mask" }
        });
        assert!(ensure_reference_roles(&graph, false, 2, Some(&role_only)).is_err());
    }

    /// 只请求一张参考图时不触发角色消歧。
    #[test]
    fn ignores_single_reference_image() {
        let graph = json!({
            "1": { "class_type": "LoadImage", "inputs": { "image": "a.png" } },
            "2": { "class_type": "LoadImage", "inputs": { "image": "b.png" } }
        });
        assert!(ensure_reference_roles(&graph, false, 1, None).is_ok());
    }

    /// 显式 paramMap 同时映射 image 与 image2 时视为已解歧。
    #[test]
    fn accepts_explicit_param_map_roles() {
        let graph = json!({
            "1": { "class_type": "LoadImage", "inputs": { "image": "a.png" } },
            "2": { "class_type": "LoadImage", "inputs": { "image": "b.png" } }
        });
        let mut map = HashMap::new();
        map.insert("image".to_string(), vec!["1.image".to_string()]);
        map.insert("image2".to_string(), vec!["2.image".to_string()]);
        assert!(ensure_reference_roles(&graph, true, 2, None).is_ok());
    }

    /// supportedResolutions 的常见写法都要能提取像素候选。
    #[test]
    fn parses_resolution_candidates_from_common_formats() {
        let items = resolution_candidates("1K (1792x1008), 2K (2560x1440)");
        assert_eq!(items, vec![(1792, 1008), (2560, 1440)]);
        let items = resolution_candidates("1024x1024 1344x768");
        assert_eq!(items, vec![(1024, 1024), (1344, 768)]);
        assert!(resolution_candidates("1K, 2K").is_empty());
    }

    /// 中间 LoadVideo 节点不影响图片输出判定。
    #[test]
    fn ignores_intermediate_video_nodes_for_image_outputs() {
        let graph = json!({
            "1": { "class_type": "SaveImage", "inputs": { "images": ["2", 0] } },
            "2": { "class_type": "LoadVideo", "inputs": { "file": "x.mp4" } }
        });
        assert!(classify_output_family_detail(&graph).is_ok());
    }

    /// 动画图（SaveAnimatedWEBP）算图片输出。
    #[test]
    fn accepts_animated_image_outputs() {
        let graph = json!({
            "1": { "class_type": "SaveAnimatedWEBP", "inputs": { "images": ["2", 0] } }
        });
        assert!(classify_output_family_detail(&graph).is_ok());
    }

    /// 负坐标 / 退化 box 也算越界。
    #[test]
    fn reports_negative_and_degenerate_layout_boxes() {
        let template = json!({
            "20": { "class_type": "MOSSLayoutBoard", "inputs": {
                "width": 512,
                "height": 512,
                "layout": "{\"width\":512,\"height\":512,\"items\":[{\"caption\":\"negative origin\",\"box\":[-10,0,400,400]},{\"caption\":\"degenerate box\",\"box\":[400,400,100,100]}]}"
            } }
        });
        let mut map = HashMap::new();
        map.insert("layout".to_string(), vec!["20.layout".to_string()]);
        let prepared = prepare_graph_unlocked(&template, &map, "a", None, 1024, 1024, 7, None, true);
        let overflow = prepared.layout_overflow.expect("overflow reported");
        assert!(overflow.contains("2 box"), "{overflow}");
    }

    /// 只声明一个 role 或未知 role 不算解歧。
    #[test]
    fn rejects_partial_or_unknown_reference_roles() {
        let graph = json!({
            "1": { "class_type": "LoadImage", "inputs": { "image": "a.png" } },
            "2": { "class_type": "LoadImage", "inputs": { "image": "b.png" } }
        });
        let partial = json!({ "image": { "role": "base" } });
        assert!(ensure_reference_roles(&graph, false, 2, Some(&partial)).is_err());
        let unknown = json!({
            "image": { "role": "base" },
            "image2": { "role": "mystery" }
        });
        assert!(ensure_reference_roles(&graph, false, 2, Some(&unknown)).is_err());
    }

    /// 语义参数不能覆盖连线：必须保留 wiring。
    #[test]
    fn preserves_wiring_when_semantic_path_is_a_link() {
        let template = json!({
            "1": { "class_type": "EmptyLatentImage", "inputs": { "width": ["9", 0], "height": 512 } },
            "9": { "class_type": "Primitive", "inputs": { "value": 1024 } }
        });
        let mut map = HashMap::new();
        map.insert("width".to_string(), vec!["1.width".to_string()]);
        map.insert("height".to_string(), vec!["1.height".to_string()]);
        let prepared = prepare_graph_unlocked(&template, &map, "a", None, 1536, 1024, 7, None, true);
        assert_eq!(prepared.graph["1"]["inputs"]["width"], json!(["9", 0]));
        assert!(prepared
            .warnings
            .iter()
            .any(|warning| warning.contains("wiring was preserved")));
    }

    /// `params` 点号键同样不能覆盖连线。
    #[test]
    fn preserves_wiring_when_params_target_is_a_link() {
        let template = json!({
            "1": { "class_type": "EmptyLatentImage", "inputs": { "width": ["9", 0], "height": 512 } },
            "9": { "class_type": "Primitive", "inputs": { "value": 1024 } }
        });
        let mut extra = serde_json::Map::new();
        extra.insert("1.width".to_string(), json!(1536));
        let prepared = prepare_graph_unlocked(
            &template,
            &HashMap::new(),
            "a",
            None,
            1024,
            1024,
            7,
            Some(&extra),
            false,
        );
        assert_eq!(prepared.graph["1"]["inputs"]["width"], json!(["9", 0]));
        assert!(prepared
            .warnings
            .iter()
            .any(|warning| warning.contains("wiring was preserved")));
    }

    /// CFG=1 时 negative 没有实际作用，必须明确告警。
    #[test]
    fn warns_when_negative_has_no_effect_at_cfg_one() {
        let template = json!({
            "1": { "class_type": "CLIPTextEncode", "inputs": { "text": "" } },
            "2": { "class_type": "KSampler", "inputs": { "cfg": 1.0, "steps": 25 } }
        });
        let mut map = HashMap::new();
        map.insert("cfg".to_string(), vec!["2.cfg".to_string()]);
        map.insert("negative".to_string(), vec!["1.text".to_string()]);
        let prepared = prepare_graph_unlocked(
            &template,
            &map,
            "a",
            Some("blurry"),
            1024,
            1024,
            7,
            None,
            false,
        );
        assert!(
            prepared
                .warnings
                .iter()
                .any(|warning| warning.contains("CFG=1")),
            "{:?}",
            prepared.warnings
        );
    }

    /// CFG>1 时不告警。
    #[test]
    fn keeps_negative_effective_when_cfg_above_one() {
        let template = json!({
            "1": { "class_type": "CLIPTextEncode", "inputs": { "text": "" } },
            "2": { "class_type": "KSampler", "inputs": { "cfg": 4.5, "steps": 30 } }
        });
        let mut map = HashMap::new();
        map.insert("cfg".to_string(), vec!["2.cfg".to_string()]);
        map.insert("negative".to_string(), vec!["1.text".to_string()]);
        let prepared = prepare_graph_unlocked(
            &template,
            &map,
            "a",
            Some("blurry"),
            1024,
            1024,
            7,
            None,
            false,
        );
        assert!(!prepared
            .warnings
            .iter()
            .any(|warning| warning.contains("CFG=1")));
    }

    /// 图里不存在但节点定义声明为 optional 的输入允许新增；拼错的键拒绝。
    #[test]
    fn accepts_optional_input_declared_by_object_info() {
        let graph = json!({
            "20": { "class_type": "MOSSLayoutBoard", "inputs": { "layout": "" } }
        });
        let info = json!({
            "MOSSLayoutBoard": {
                "input": {
                    "optional": { "strict": ["BOOLEAN", {}] },
                    "required": { "layout": ["STRING", {}] }
                }
            }
        });
        assert!(input_declared_in_object_info(
            Some(&info),
            &graph,
            "20.strict"
        ));
        assert!(input_declared_in_object_info(
            Some(&info),
            &graph,
            "20.inputs.layout"
        ));
        assert!(!input_declared_in_object_info(
            Some(&info),
            &graph,
            "20.missing"
        ));
    }

    /// Readiness：只报告 object_info 里不存在的节点类。
    #[test]
    fn reports_missing_node_classes() {
        let graph = json!({
            "1": { "class_type": "SaveImage", "inputs": {} },
            "2": { "class_type": "MissingCustomNode", "inputs": {} }
        });
        let info = json!({ "SaveImage": {} });
        let missing = missing_node_classes(&graph, &info);
        assert_eq!(missing, vec!["MissingCustomNode".to_string()]);
    }

    fn bounds_graph() -> Value {
        json!({
            "3": { "class_type": "KSampler", "inputs": { "steps": 20, "cfg": 8.0, "sampler_name": "euler" } },
            "5": { "class_type": "EmptyLatentImage", "inputs": { "width": 512, "height": 512 } },
            "6": { "class_type": "LoadImage", "inputs": { "image": "old.png" } }
        })
    }

    fn bounds_info() -> Value {
        json!({
            "KSampler": {
                "input": {
                    "required": {
                        "steps": ["INT", { "default": 20, "min": 1, "max": 10000 }],
                        "cfg": ["FLOAT", { "default": 8.0, "min": 0.0, "max": 100.0, "step": 0.1 }],
                        "sampler_name": [["euler", "dpmpp_2m"]]
                    }
                }
            },
            "EmptyLatentImage": {
                "input": {
                    "required": {
                        "width": ["INT", { "default": 512, "min": 16, "max": 8192, "step": 8 }],
                        "height": ["INT", { "default": 512, "min": 16, "max": 8192, "step": 8 }]
                    }
                }
            },
            "LoadImage": {
                "input": { "required": { "image": [["old.png", "other.png"]] } }
            }
        })
    }

    /// 技术边界解析：INT / FLOAT / COMBO 与 min/max/step 都要落到结构里。
    #[test]
    fn parses_node_input_bounds() {
        let graph = bounds_graph();
        let info = bounds_info();
        let steps = input_bounds(Some(&info), &graph, "3.steps").expect("steps bounds");
        assert_eq!(
            steps.scalar,
            Some(crate::comfyui::policy::ScalarKind::Integer)
        );
        assert_eq!((steps.min, steps.max), (Some(1.0), Some(10000.0)));
        let cfg = input_bounds(Some(&info), &graph, "3.inputs.cfg").expect("cfg bounds");
        assert_eq!(cfg.scalar, Some(crate::comfyui::policy::ScalarKind::Float));
        assert_eq!(cfg.step, Some(0.1));
        let sampler = input_bounds(Some(&info), &graph, "3.sampler_name").expect("enum bounds");
        assert_eq!(
            sampler.options,
            Some(vec!["euler".to_string(), "dpmpp_2m".to_string()])
        );
        // 未声明的点号键没有技术边界，不能凭空拒绝。
        assert!(input_bounds(Some(&info), &graph, "3.missing").is_none());
    }

    /// 值校验：类型 / 枚举 / 范围 / 对齐；上传的参考图文件名跳过枚举。
    #[test]
    fn rejects_values_outside_node_contract() {
        let graph = bounds_graph();
        let info = bounds_info();
        let steps = input_bounds(Some(&info), &graph, "3.steps").unwrap();
        // R3 的三个反例：字符串、布尔、越界。
        assert!(check_input_bounds(&steps, &json!("99999"), "3.steps", false)
            .unwrap()
            .contains("must be an integer"));
        assert!(check_input_bounds(&steps, &json!(false), "3.steps", false)
            .unwrap()
            .contains("must be an integer"));
        assert!(check_input_bounds(&steps, &json!(10001), "3.steps", false)
            .unwrap()
            .contains("<= 10000"));
        assert!(check_input_bounds(&steps, &json!(20), "3.steps", false).is_none());

        let sampler = input_bounds(Some(&info), &graph, "3.sampler_name").unwrap();
        assert!(check_input_bounds(&sampler, &json!(123), "3.sampler_name", false)
            .unwrap()
            .contains("must be a string"));
        assert!(check_input_bounds(&sampler, &json!("unknown"), "3.sampler_name", false)
            .unwrap()
            .contains("must be one of"));
        assert!(check_input_bounds(&sampler, &json!("euler"), "3.sampler_name", false).is_none());

        let width = input_bounds(Some(&info), &graph, "5.width").unwrap();
        assert!(check_input_bounds(&width, &json!(1000), "5.width", false).is_none());
        assert!(check_input_bounds(&width, &json!(1023), "5.width", false)
            .unwrap()
            .contains("align to step 8"));
        assert!(check_input_bounds(&width, &json!(8), "5.width", false)
            .unwrap()
            .contains(">= 16"));

        // 参考图上传后的文件名不在缓存枚举里，不能被当成非法值。
        let image = input_bounds(Some(&info), &graph, "6.image").unwrap();
        assert!(check_input_bounds(&image, &json!("snow-ref-1.png"), "6.image", false)
            .unwrap()
            .contains("must be one of"));
        assert!(check_input_bounds(&image, &json!("snow-ref-1.png"), "6.image", true).is_none());
    }

    /// 多参考图的 `nodePath` 必须真正决定注入目标：声明 base@9 / mask@2 时，
    /// `image` 不能按节点 ID 顺序落到 2.image（那是 mask 槽）。
    #[test]
    fn declared_image_slots_follow_node_path_not_node_order() {
        let graph = json!({
            "2": { "class_type": "LoadImage", "inputs": { "image": "mask.png" } },
            "9": { "class_type": "LoadImage", "inputs": { "image": "base.png" } }
        });
        let declared = json!({
            "image": { "role": "base", "nodePath": "9.image" },
            "image2": { "role": "mask", "nodePath": "2.image" }
        });
        let map = declared_image_slot_map(Some(&declared), &graph).expect("valid declarations");
        assert_eq!(map.get("image"), Some(&vec!["9.image".to_string()]));
        assert_eq!(map.get("image2"), Some(&vec!["2.image".to_string()]));
    }

    /// 声明指向非 LoadImage 节点必须拒绝，不能当成放行凭证。
    #[test]
    fn rejects_declared_image_slot_that_is_not_a_loader() {
        let graph = json!({
            "2": { "class_type": "LoadImage", "inputs": { "image": "mask.png" } },
            "9": { "class_type": "KSampler", "inputs": { "seed": 1 } }
        });
        let declared = json!({ "image": { "role": "base", "nodePath": "9.image" } });
        let error = declared_image_slot_map(Some(&declared), &graph).expect_err("not a loader");
        assert!(error.contains("not a LoadImage"), "{error}");
    }

    /// 两个语义指向同一输入会让后写入的图覆盖前一张，必须拒绝。
    #[test]
    fn rejects_duplicate_declared_image_slots() {
        let graph = json!({
            "2": { "class_type": "LoadImage", "inputs": { "image": "mask.png" } }
        });
        let declared = json!({
            "image": { "role": "base", "nodePath": "2.image" },
            "image2": { "role": "mask", "nodePath": "2.image" }
        });
        let error = declared_image_slot_map(Some(&declared), &graph).expect_err("duplicate target");
        assert!(error.contains("same image input"), "{error}");
    }

    /// 只写 role 不写 nodePath 仍然拒绝（声明不是放行凭证）。
    #[test]
    fn rejects_declared_image_role_without_node_path() {
        let graph = json!({ "2": { "class_type": "LoadImage", "inputs": { "image": "a.png" } } });
        let declared = json!({ "image": { "role": "base" } });
        let error = declared_image_slot_map(Some(&declared), &graph).expect_err("missing nodePath");
        assert!(error.contains("nodePath"), "{error}");
    }

    /// MOSS 布局的结构缺口必须 fail-closed：缺 box / box 长度不对 /
    /// label=text 无 text / 缺 caption / items 为空。
    #[test]
    fn reports_structural_layout_problems() {
        let cases = [
            (json!({"items": [{"label": "text"}]}), "missing `box`"),
            (
                json!({"items": [{"caption": "x", "box": [0, 0, 10]}]}),
                "exactly 4 integers",
            ),
            (
                json!({"items": [{"label": "text", "box": [0, 0, 10, 10]}]}),
                "no non-empty `text`",
            ),
            (
                json!({"items": [{"box": [0, 0, 10, 10]}]}),
                "missing a non-empty `caption`",
            ),
            (json!({}), "layout.items must be an array"),
            (json!({"items": []}), "at least one item"),
        ];
        for (layout, needle) in cases {
            let message = layout_problem_message(&layout, 512, 512)
                .unwrap_or_else(|| panic!("expected a problem containing `{needle}`"));
            assert!(message.contains(needle), "expected `{needle}` in `{message}`");
        }
    }

    /// 合法布局（object 项有 caption，text 项带 text）不得误报。
    #[test]
    fn accepts_well_formed_layout_document() {
        let layout = json!({"items": [
            {"label": "object", "caption": "a cup", "box": [0, 0, 100, 100]},
            {"label": "text", "text": "夜航西飞", "box": [0, 100, 200, 200]}
        ]});
        assert!(layout_problem_message(&layout, 512, 512).is_none());
    }

    /// Readiness：模板引用但本机未装的模型文件必须报出来；示例参考图不参与。
    #[test]
    fn reports_missing_model_files_and_ignores_reference_images() {
        let graph = json!({
            "1": { "class_type": "CheckpointLoaderSimple", "inputs": { "ckpt_name": "missing.safetensors" } },
            "2": { "class_type": "LoadImage", "inputs": { "image": "example-not-uploaded.png" } },
            "3": { "class_type": "CLIPTextEncode", "inputs": { "text": "hi" } }
        });
        let info = json!({
            "CheckpointLoaderSimple": { "input": { "required": {
                "ckpt_name": [["installed.safetensors"]]
            } } },
            "LoadImage": { "input": { "required": { "image": [["available.png"]] } } },
            "CLIPTextEncode": { "input": { "required": { "text": ["STRING", {}] } } }
        });
        let missing = missing_model_files(&graph, &info);
        assert_eq!(
            missing,
            vec!["CheckpointLoaderSimple.ckpt_name = missing.safetensors (not installed)"]
        );
    }

    /// 顶层已注入同一路径时，`params` 里的不同值必须给出冲突告警，
    /// 而不是静默 no-op（用户传 1024 却得到 512 且毫无提示）。
    #[test]
    fn warns_when_params_conflict_with_top_level_parameter() {
        let template = json!({
            "5": { "class_type": "EmptyLatentImage", "inputs": { "width": 512, "height": 512 } },
            "9": { "class_type": "SaveImage", "inputs": { "images": ["5", 0] } }
        });
        let mut map = HashMap::new();
        map.insert("width".to_string(), vec!["5.width".to_string()]);
        map.insert("height".to_string(), vec!["5.height".to_string()]);
        let mut extra = serde_json::Map::new();
        extra.insert("width".to_string(), json!(1024));
        let prepared = prepare_graph_unlocked(&template, &map, "p", None, 512, 512, 7, Some(&extra), false);
        assert!(
            prepared
                .warnings
                .iter()
                .any(|warning| warning.contains("top-level")),
            "{:?}",
            prepared.warnings
        );
        // 最终生效的仍是顶层值，告警如实反映了这一点。
        assert_eq!(
            read_path(&prepared.graph, "5.width").and_then(Value::as_i64),
            Some(512)
        );
    }

    /// 隐式写入（默认尺寸）同样不能越过目标锁：被锁的路径保留模板值。
    #[test]
    fn locked_targets_block_implicit_writes() {
        let template = json!({
            "6": { "class_type": "EmptyLatentImage", "inputs": { "width": 512, "height": 512 } },
            "3": { "class_type": "KSampler", "inputs": { "seed": 7, "width": 512 } }
        });
        let map: HashMap<String, Vec<String>> = [
            (
                "width".to_string(),
                vec!["6.width".to_string(), "3.width".to_string()],
            ),
            ("height".to_string(), vec!["6.height".to_string()]),
            ("seed".to_string(), vec!["3.seed".to_string()]),
        ]
        .into_iter()
        .collect();
        let mut locked = std::collections::HashSet::new();
        locked.insert("6.inputs.width".to_string());
        let prepared = prepare_graph(
            &template,
            &map,
            "p",
            None,
            1024,
            1024,
            42,
            None,
            true,
            &locked,
        );
        // width/height 必须成对：任一目标被锁就整体保留模板尺寸，避免
        // latent 与布局节点只更新一边。seed 不在尺寸语义里，仍可写。
        assert_eq!(read_path(&prepared.graph, "6.width"), Some(&json!(512)));
        assert_eq!(read_path(&prepared.graph, "3.width"), Some(&json!(512)));
        assert_eq!(read_path(&prepared.graph, "3.seed"), Some(&json!(42)));
    }

    /// 被锁定的 layout 目标不能被尺寸同步改写（否则锁只挡住了显式参数）。
    #[test]
    fn locked_layout_target_is_not_rewritten_by_size_sync() {
        let template_layout = json!({
            "width": 512,
            "height": 512,
            "items": []
        })
        .to_string();
        let template = json!({
            "6": { "class_type": "EmptyLatentImage", "inputs": { "width": 512, "height": 512 } },
            "20": { "class_type": "MOSSLayoutBoard", "inputs": { "layout": template_layout.clone() } }
        });
        let map: HashMap<String, Vec<String>> = [
            ("width".to_string(), vec!["6.width".to_string()]),
            ("height".to_string(), vec!["6.height".to_string()]),
            ("layout".to_string(), vec!["20.layout".to_string()]),
        ]
        .into_iter()
        .collect();
        let mut locked = std::collections::HashSet::new();
        locked.insert("20.inputs.layout".to_string());
        let prepared = prepare_graph(
            &template,
            &map,
            "p",
            None,
            1024,
            768,
            7,
            None,
            true,
            &locked,
        );
        assert_eq!(
            read_path(&prepared.graph, "20.layout").and_then(Value::as_str),
            Some(template_layout.as_str())
        );
    }

    /// P2：只锁住 width 时 height 也必须保留，不能只改一边尺寸。
    #[test]
    fn one_sided_size_lock_preserves_both_dimensions() {
        let template = json!({
            "6": { "class_type": "EmptyLatentImage", "inputs": { "width": 512, "height": 512 } }
        });
        let map: HashMap<String, Vec<String>> = [
            ("width".to_string(), vec!["6.width".to_string()]),
            ("height".to_string(), vec!["6.height".to_string()]),
        ]
        .into_iter()
        .collect();
        let mut locked = std::collections::HashSet::new();
        locked.insert("6.inputs.width".to_string());
        let prepared = prepare_graph(
            &template,
            &map,
            "p",
            None,
            1024,
            768,
            7,
            None,
            true,
            &locked,
        );
        assert_eq!(read_path(&prepared.graph, "6.width"), Some(&json!(512)));
        assert_eq!(read_path(&prepared.graph, "6.height"), Some(&json!(512)));
    }

    /// P1：两张图只声明一个槽位（即使 nodePath 有效）也必须拒绝。
    #[test]
    fn rejects_partial_node_path_declaration_for_two_images() {
        let graph = json!({
            "1": { "class_type": "LoadImage", "inputs": { "image": "a.png" } },
            "2": { "class_type": "LoadImage", "inputs": { "image": "b.png" } }
        });
        let roles = json!({
            "image": { "role": "base", "nodePath": "1.image" }
        });
        assert!(ensure_reference_roles(&graph, false, 2, Some(&roles)).is_err());
    }

    /// P1：声明必须覆盖具体槽位；image + image99 不能代替 image2。
    #[test]
    fn rejects_declared_roles_that_do_not_cover_image2() {
        let graph = json!({
            "1": { "class_type": "LoadImage", "inputs": { "image": "a.png" } },
            "2": { "class_type": "LoadImage", "inputs": { "image": "b.png" } },
            "3": { "class_type": "LoadImage", "inputs": { "image": "c.png" } }
        });
        let roles = json!({
            "image": { "role": "base", "nodePath": "1.image" },
            "image99": { "role": "reference", "nodePath": "3.image" }
        });
        assert!(ensure_reference_roles(&graph, false, 2, Some(&roles)).is_err());
    }

    /// 布局最终检查：省略 size（sync_size=false）也要用模板尺寸检查，
    /// 并且遍历所有 layout 映射，而不是只看第一处。
    #[test]
    fn checks_all_layout_paths_even_without_size_sync() {
        let layout = |box_coords: [i64; 4]| {
            json!({
                "width": 512,
                "height": 512,
                "items": [{ "box": box_coords }]
            })
            .to_string()
        };
        let make_graph = |first: [i64; 4], second: [i64; 4]| {
            json!({
                "6": { "class_type": "EmptyLatentImage", "inputs": { "width": 512, "height": 512 } },
                "20": { "class_type": "MOSSLayoutBoard", "inputs": { "layout": layout(first) } },
                "21": { "class_type": "MOSSLayoutBoard", "inputs": { "layout": layout(second) } }
            })
        };
        let map: HashMap<String, Vec<String>> = [
            ("width".to_string(), vec!["6.width".to_string()]),
            ("height".to_string(), vec!["6.height".to_string()]),
            (
                "layout".to_string(),
                vec!["20.layout".to_string(), "21.layout".to_string()],
            ),
        ]
        .into_iter()
        .collect();
        // 第一处越界（sync_size=false 也必须检查）。
        let prepared = prepare_graph_unlocked(
            &make_graph([0, 0, 600, 600], [0, 0, 100, 100]),
            &map,
            "p",
            None,
            1024,
            1024,
            7,
            None,
            false,
        );
        assert!(prepared.layout_overflow.is_some(), "{:?}", prepared.warnings);
        // 只有第二处越界：必须遍历全部 layout 路径才能发现。
        let prepared = prepare_graph_unlocked(
            &make_graph([0, 0, 100, 100], [0, 0, 600, 600]),
            &map,
            "p",
            None,
            1024,
            1024,
            7,
            None,
            false,
        );
        assert!(prepared.layout_overflow.is_some(), "{:?}", prepared.warnings);
    }

    /// 像素预算覆盖显式、默认/清洗后与模板三种来源。
    #[test]
    fn pixel_budget_covers_all_size_sources() {
        assert!(ensure_pixel_budget(4096, 4096).is_ok());
        assert!(ensure_pixel_budget(8192, 8192).is_err());
        assert!(
            resolve_comfy_size(Some("8192x8192"), None, None, None, "none")
                .unwrap_err()
                .contains("pixel budget")
        );
        assert!(resolve_comfy_size(
            None,
            Some("8192x8192"),
            Some("8192x8192"),
            None,
            "none"
        )
        .is_err());
        assert!(
            resolve_comfy_size(None, None, None, Some((8192, 8192)), "none")
                .unwrap_err()
                .contains("pixel budget")
        );
    }

    /// 大整数不能被「超过 2^53 就跳过」放行；FLOAT 的 step 也要检查。
    #[test]
    fn enforces_large_integer_bounds_and_float_step() {
        let int_bounds = InputBounds {
            scalar: Some(crate::comfyui::policy::ScalarKind::Integer),
            min: Some(1.0),
            max: Some(10000.0),
            ..Default::default()
        };
        assert!(check_input_bounds(&int_bounds, &json!(9007199254740994u64), "steps", false)
            .unwrap()
            .contains("<= 10000"));
        // u64::MAX 与同值边界必须精确判等，不能被 f64 截断误杀。
        let seed_bounds = InputBounds {
            scalar: Some(crate::comfyui::policy::ScalarKind::Integer),
            min_int: Some(0),
            max_int: Some(u64::MAX as i128),
            ..Default::default()
        };
        assert!(check_input_bounds(&seed_bounds, &json!(u64::MAX), "seed", false).is_none());
        let float_bounds = InputBounds {
            scalar: Some(crate::comfyui::policy::ScalarKind::Float),
            min: Some(0.0),
            max: Some(100.0),
            step: Some(0.1),
            ..Default::default()
        };
        assert!(check_input_bounds(&float_bounds, &json!(8.0), "cfg", false).is_none());
        assert!(check_input_bounds(&float_bounds, &json!(8.05), "cfg", false)
            .unwrap()
            .contains("align to step"));
    }
}
