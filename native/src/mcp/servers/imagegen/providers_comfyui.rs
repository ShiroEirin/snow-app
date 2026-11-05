//! ComfyUI 原生生图分支。
//!
//! 不再依赖 ComfyUI-OpenAI-Adapter：直接对本地 ComfyUI 走它自己的 HTTP 协议闭环
//! - `POST {base}/upload/image`  上传参考图（multipart，字段名 `image`）
//! - `POST {base}/prompt`        提交 API 格式工作流（graph + client_id）
//! - `GET  {base}/history/{id}`  轮询任务完成（500ms 一次，总超时受 timeout_secs 约束）
//! - `GET  {base}/view`          按 filename/subfolder/type 取回图片字节
//!
//! 渠道侧的 `workflow` 是 API 格式工作流（ComfyUI 里 Workflow -> Export (API)），
//! `paramMap` 把语义参数（prompt / negative / seed / width / height / image）
//! 映射到工作流的「点号键」（节点 id + 输入名，如 `6.text`）。

use super::*;

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};

/// ComfyUI 语义名 → 工作流点号键的默认映射（渠道 `paramMap` 为空时生效）。
/// 点号键 `6.text` 等价于 `graph["6"]["inputs"]["text"]`。
const DEFAULT_COMFYUI_PARAM_MAP: &[(&str, &str)] = &[
    ("prompt", "6.text"),
    ("negative", "7.text"),
    ("seed", "3.seed"),
    ("width", "5.width"),
    ("height", "5.height"),
    ("image", "10.image"),
];

/// 工具保留键：调用方不得通过 `params` 改写工具自己负责的字段。
///
/// 该常量在 `providers` 模块内是私有的（跨兄弟模块不可见），而任务边界不允许
/// 改动 providers.rs，因此这里保留一份同名副本，语义与 providers.rs 完全一致。
const RESERVED_EXTRA_KEYS: &[&str] = &[
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

impl ImageGenService {
    /// ComfyUI 分支：按渠道配置的工作流模板逐请求出图，汇总图片块返回。
    ///
    /// `api_key` 不参与（本地服务无需凭据）；`quality` 对 ComfyUI 无意义，
    /// 仅为与其它 provider 的调用形态保持一致而保留。
    pub(super) async fn generate_comfyui(
        &self,
        args: &Value,
        channel_config: &ImageGenChannel,
        prompts: &[&str],
        model: &str,
        size: &Option<String>,
        _quality: &Option<String>,
        base_url: &str,
        n: usize,
        images: &[&[ReferenceImage]],
        seed: Option<u64>,
        timeout_secs: Option<u64>,
    ) -> napi::Result<Value> {
        let base_url = base_url.trim().trim_end_matches('/');

        // --- 1. 工作流模板：渠道文本优先；`.json` 且文件存在时从磁盘读取 ---
        let workflow_raw = channel_config.workflow.trim();
        if workflow_raw.is_empty() {
            return Err(generic_error(
                "ComfyUI channel requires a workflow: set the channel `workflow` field to the API-format workflow JSON (ComfyUI: Workflow -> Export (API)) or to a .json file path.".to_string(),
            ));
        }
        let workflow_text = if workflow_raw.to_ascii_lowercase().ends_with(".json")
            && Path::new(workflow_raw).exists()
        {
            std::fs::read_to_string(workflow_raw).map_err(|error| {
                generic_error(format!(
                    "Failed to read ComfyUI workflow file \"{workflow_raw}\": {error}"
                ))
            })?
        } else {
            workflow_raw.to_string()
        };
        let template: Value = serde_json::from_str(&workflow_text)
            .map_err(|error| generic_error(format!("Invalid ComfyUI workflow JSON: {error}")))?;
        if !template.is_object() {
            return Err(generic_error(
                "Invalid ComfyUI workflow: the API-format workflow must be a JSON object of `node id -> node`".to_string(),
            ));
        }

        // --- 2. 语义名映射：paramMap 覆盖默认映射 ---
        let param_map = parse_param_map(&channel_config.param_map)?;
        let (width, height) = parse_size(size.as_deref()).unwrap_or((1024, 1024));
        let negative = args
            .get("negativePrompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        // `params` 里的自定义节点参数（如 {"3.steps": 30}），保留键与已注入路径不覆盖
        let extra_params = args.get("params").and_then(Value::as_object);

        let client = build_client(timeout_secs).await?;
        let requests = n.max(1);

        let mut content: Vec<Value> = Vec::new();
        let mut last_error: Option<String> = None;
        for index in 0..requests {
            let prompt = prompts
                .get(index)
                .copied()
                .or_else(|| prompts.first().copied())
                .unwrap_or_default();
            let request_images: &[ReferenceImage] = images.get(index).copied().unwrap_or(&[]);
            // 显式 seed 且多请求时逐张递增，避免生成完全相同的图（与 OpenAI 分支一致）；
            // 未指定时用时间派生的随机种子，避免 ComfyUI 命中同图缓存。
            let request_seed = seed
                .map(|value| value.wrapping_add(index as u64))
                .unwrap_or_else(random_seed);

            match run_comfyui_request(
                &client,
                base_url,
                &template,
                &param_map,
                prompt,
                negative.as_deref(),
                width,
                height,
                request_seed,
                request_images,
                extra_params,
                timeout_secs,
            )
            .await
            {
                Ok(blocks) => content.extend(blocks),
                Err(error) => {
                    // 只记录失败路径；stderr 不会污染 stdio MCP 的正常输出。
                    eprintln!(
                        "[imagegen] ComfyUI request {}/{requests} failed: {error}",
                        index + 1
                    );
                    last_error = Some(error.to_string());
                }
            }
        }

        // --- 4. 汇总：至少一张图才算成功 ---
        let generated = content.len();
        if generated == 0 {
            return Err(generic_error(format!(
                "ComfyUI generation returned no images.{}",
                last_error
                    .map(|error| format!(" Last error: {error}"))
                    .unwrap_or_default()
            )));
        }
        Ok(build_result(
            prompts.first().copied().unwrap_or_default(),
            model,
            "comfyui",
            generated,
            content,
            Vec::new(),
        ))
    }
}

/// 单个请求：克隆模板 → 注入参数 → 上传参考图 → 提交 → 轮询 → 取图。
#[allow(clippy::too_many_arguments)]
async fn run_comfyui_request(
    client: &reqwest::Client,
    base_url: &str,
    template: &Value,
    param_map: &HashMap<String, String>,
    prompt: &str,
    negative: Option<&str>,
    width: u64,
    height: u64,
    seed: u64,
    request_images: &[ReferenceImage],
    extra_params: Option<&serde_json::Map<String, Value>>,
    timeout_secs: Option<u64>,
) -> napi::Result<Vec<Value>> {
    let mut warnings: Vec<String> = Vec::new();
    // 已由工具注入的点号键（小写规范化），params 不允许覆盖它们
    let mut injected: Vec<String> = Vec::new();
    let mut graph = template.clone();

    // --- a/b. 语义参数注入 ---
    if let Some(path) = set_graph_value(
        &mut graph,
        param_map.get("prompt"),
        json!(prompt),
        "prompt",
        &mut warnings,
    ) {
        injected.push(path);
    }
    // 只有调用方真的给了负向提示词才写入，避免把模板自带的 negative 清空
    if let Some(text) = negative {
        if let Some(path) = set_graph_value(
            &mut graph,
            param_map.get("negative"),
            json!(text),
            "negative",
            &mut warnings,
        ) {
            injected.push(path);
        }
    }
    if let Some(path) = set_graph_value(
        &mut graph,
        param_map.get("seed"),
        json!(seed),
        "seed",
        &mut warnings,
    ) {
        injected.push(path);
    }
    if let Some(path) = set_graph_value(
        &mut graph,
        param_map.get("width"),
        json!(width),
        "width",
        &mut warnings,
    ) {
        injected.push(path);
    }
    if let Some(path) = set_graph_value(
        &mut graph,
        param_map.get("height"),
        json!(height),
        "height",
        &mut warnings,
    ) {
        injected.push(path);
    }

    // --- c. 参考图上传（ComfyUI input 目录），把上传后的名字写进 image 键 ---
    if !request_images.is_empty() {
        if param_map.contains_key("image") {
            for image in request_images {
                let uploaded = upload_comfyui_image(client, base_url, image).await?;
                if let Some(path) = set_graph_value(
                    &mut graph,
                    param_map.get("image"),
                    json!(uploaded),
                    "image",
                    &mut warnings,
                ) {
                    injected.push(path);
                }
            }
        } else {
            warnings.push(
                "reference images were provided but paramMap has no \"image\" entry; skipped"
                    .to_string(),
            );
        }
    }

    // --- d. params 自定义节点参数（保留键 + 已注入路径不覆盖）---
    apply_extra_params(&mut graph, extra_params, &injected, &mut warnings);

    // --- e. 提交任务 ---
    let prompt_id = queue_comfyui_prompt(client, base_url, &graph).await?;
    // --- f. 轮询 history ---
    let references = wait_comfyui_history(client, base_url, &prompt_id, timeout_secs).await?;
    if references.is_empty() {
        let mut message = format!(
            "ComfyUI workflow finished (prompt_id={prompt_id}) without any image output; make sure the workflow contains a SaveImage / PreviewImage node."
        );
        if !warnings.is_empty() {
            message.push_str(&format!(" Warnings: {}", warnings.join(" | ")));
        }
        return Err(generic_error(message));
    }

    // --- g. 取图 ---
    let mut blocks = Vec::with_capacity(references.len());
    for reference in &references {
        blocks.push(download_comfyui_image(client, base_url, reference).await?);
    }
    Ok(blocks)
}

/// 解析渠道 `paramMap`：默认映射打底，用户提供的映射逐项覆盖，
/// 因此只写需要改的那几个语义名即可。
fn parse_param_map(raw: &str) -> napi::Result<HashMap<String, String>> {
    let mut map: HashMap<String, String> = DEFAULT_COMFYUI_PARAM_MAP
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();

    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(map);
    }
    let parsed: Value = serde_json::from_str(raw)
        .map_err(|error| generic_error(format!("Invalid ComfyUI `paramMap` JSON: {error}")))?;
    let Some(entries) = parsed.as_object() else {
        return Err(generic_error(
            "Invalid ComfyUI `paramMap`: expected a JSON object of `semantic name -> dotted workflow key`"
                .to_string(),
        ));
    };
    for (key, value) in entries {
        let Some(path) = value
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        else {
            return Err(generic_error(format!(
                "Invalid ComfyUI `paramMap` entry \"{key}\": expected a non-empty dotted key string"
            )));
        };
        map.insert(key.clone(), path.to_string());
    }
    Ok(map)
}

/// 解析 `WxH` 尺寸（大小写 x / `*` 都接受）；无法解析时返回 None（调用方用 1024x1024）。
fn parse_size(size: Option<&str>) -> Option<(u64, u64)> {
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

/// 写入一个语义参数；成功返回规范化路径（供 `params` 冲突判断用）。
fn set_graph_value(
    graph: &mut Value,
    path: Option<&String>,
    value: Value,
    label: &str,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let Some(path) = path else {
        warnings.push(format!("paramMap has no \"{label}\" entry; skipped"));
        return None;
    };
    let segments = canonical_segments(path);
    if segments.len() < 3 {
        warnings.push(format!(
            "paramMap \"{label}\" -> \"{path}\" must be a dotted node key like \"6.text\"; skipped"
        ));
        return None;
    }
    if set_graph_value_at(graph, &segments, value, path, warnings) {
        Some(segments.join(".").to_ascii_lowercase())
    } else {
        None
    }
}

/// 递归下钻写入：路径不存在时记录 warning 并跳过（绝不 panic）。
fn set_graph_value_at(
    root: &mut Value,
    segments: &[String],
    value: Value,
    full_path: &str,
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
            warnings.push(format!("workflow has no \"{full_path}\"; skipped"));
            return false;
        }
        object.insert(first.clone(), value);
        return true;
    }
    let Some(next) = object.get_mut(first) else {
        warnings.push(format!("workflow has no \"{full_path}\"; skipped"));
        return false;
    };
    set_graph_value_at(next, &segments[1..], value, full_path, warnings)
}

/// 合并 `params` 里的自定义节点参数（点号键直接写工作流）。
fn apply_extra_params(
    graph: &mut Value,
    extra: Option<&serde_json::Map<String, Value>>,
    injected: &[String],
    warnings: &mut Vec<String>,
) {
    let Some(extra) = extra else {
        return;
    };
    for (key, value) in extra {
        if RESERVED_EXTRA_KEYS.contains(&key.as_str()) {
            continue;
        }
        let segments = canonical_segments(key);
        if segments.len() < 3 {
            warnings.push(format!(
                "params key \"{key}\" must be a dotted workflow key like \"7.steps\"; skipped"
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
        set_graph_value_at(graph, &segments, value.clone(), key, warnings);
    }
}

/// 上传参考图到 ComfyUI（`POST /upload/image`），返回可写进工作流的
/// `subfolder/name`（subfolder 为空时只写 name）。
async fn upload_comfyui_image(
    client: &reqwest::Client,
    base_url: &str,
    image: &ReferenceImage,
) -> napi::Result<String> {
    let bytes = reference_image::decode_base64(&image.data)?;
    let file_name = format!(
        "snow-ref-{}.{}",
        now_nanos(),
        reference_image::ext_for_mime(&image.mime_type)
    );
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(file_name)
        .mime_str(&image.mime_type)
        .map_err(|error| {
            generic_error(format!("Failed to build ComfyUI upload part: {error}"))
        })?;
    let form = reqwest::multipart::Form::new()
        .part("image", part)
        .text("overwrite", "true")
        .text("type", "input");

    let response = client
        .post(format!("{base_url}/upload/image"))
        .multipart(form)
        .send()
        .await
        .map_err(|error| generic_error(format!("ComfyUI image upload failed: {error}")))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        return Err(api_error(
            "ComfyUI image upload failed",
            status.as_u16(),
            &body,
        ));
    }
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if name.is_empty() {
        return Err(generic_error(format!(
            "ComfyUI upload response is missing `name`: {body}"
        )));
    }
    let subfolder = body
        .get("subfolder")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    Ok(if subfolder.is_empty() {
        name.to_string()
    } else {
        format!("{subfolder}/{name}")
    })
}

/// 提交工作流：`POST /prompt`，返回 `prompt_id`。
async fn queue_comfyui_prompt(
    client: &reqwest::Client,
    base_url: &str,
    graph: &Value,
) -> napi::Result<String> {
    let body = json!({
        "prompt": graph,
        "client_id": random_id(),
    });
    let response = client
        .post(format!("{base_url}/prompt"))
        .json(&body)
        .send()
        .await
        .map_err(|error| generic_error(format!("ComfyUI /prompt request failed: {error}")))?;
    let status = response.status();
    let response_body: Value = response.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        return Err(api_error(
            "ComfyUI prompt failed",
            status.as_u16(),
            &response_body,
        ));
    }
    response_body
        .get("prompt_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            generic_error(format!(
                "ComfyUI /prompt response is missing `prompt_id`: {response_body}"
            ))
        })
}

/// 轮询 `GET /history/{prompt_id}` 直到任务完成，返回 outputs 里收集到的图片引用。
async fn wait_comfyui_history(
    client: &reqwest::Client,
    base_url: &str,
    prompt_id: &str,
    timeout_secs: Option<u64>,
) -> napi::Result<Vec<Value>> {
    const POLL_INTERVAL: Duration = Duration::from_millis(500);
    const MIN_WAIT_SECS: u64 = 30;

    let total = Duration::from_secs(
        timeout_secs
            .unwrap_or(REQUEST_TIMEOUT_SECS)
            .max(MIN_WAIT_SECS),
    );
    let deadline = Instant::now() + total;
    let history_url = format!("{base_url}/history/{prompt_id}");

    loop {
        let response = client.get(&history_url).send().await.map_err(|error| {
            generic_error(format!("ComfyUI /history request failed: {error}"))
        })?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or_else(|_| json!({}));
        if !status.is_success() {
            return Err(api_error("ComfyUI history failed", status.as_u16(), &body));
        }

        // 完成后的 entry 形如 {outputs: {...}, status: {status_str: "success", completed: true}}；
        // 未完成时 /history 里根本看不到该 prompt_id。
        if let Some(entry) = body.get(prompt_id) {
            if entry.get("outputs").is_some() || entry.get("status").is_some() {
                if let Some(error) = comfyui_entry_error(entry) {
                    return Err(generic_error(format!("ComfyUI workflow failed: {error}")));
                }
                return Ok(collect_comfyui_output_images(entry.get("outputs")));
            }
        }

        if Instant::now() >= deadline {
            return Err(generic_error(format!(
                "ComfyUI generation timed out after {}s (prompt_id={prompt_id}). The workflow is still queued/running — raise the imagegen timeout, check the ComfyUI queue, or verify the workflow's models are installed.",
                total.as_secs()
            )));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// history entry 里的执行错误（`status.status_str == "error"`）。
fn comfyui_entry_error(entry: &Value) -> Option<String> {
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

/// 从 outputs 的每个节点里收集 `images` 数组元素（{filename, subfolder, type}）。
fn collect_comfyui_output_images(outputs: Option<&Value>) -> Vec<Value> {
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

/// `GET /view?filename=&subfolder=&type=` 取回一张图，转成工具统一的 content 块。
async fn download_comfyui_image(
    client: &reqwest::Client,
    base_url: &str,
    reference: &Value,
) -> napi::Result<Value> {
    let filename = reference
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let subfolder = reference
        .get("subfolder")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let kind = reference
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("output");

    let mut url = reqwest::Url::parse(&format!("{base_url}/view")).map_err(|error| {
        generic_error(format!("Invalid ComfyUI base URL \"{base_url}\": {error}"))
    })?;
    url.query_pairs_mut()
        .append_pair("filename", filename)
        .append_pair("subfolder", subfolder)
        .append_pair("type", kind);

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| generic_error(format!("ComfyUI /view request failed: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        let body: Value = response.json().await.unwrap_or_else(|_| json!({}));
        return Err(api_error("ComfyUI view failed", status.as_u16(), &body));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| generic_error(format!("Failed to read ComfyUI image bytes: {error}")))?;
    if bytes.is_empty() {
        return Err(generic_error(format!(
            "ComfyUI returned an empty image for \"{filename}\""
        )));
    }

    let mime_type = mime_for_file_name(filename);
    let data = BASE64_STANDARD.encode(&bytes[..]);
    Ok(json!({
        "type": "image",
        "data": data,
        "mimeType": mime_type,
    }))
}

/// 按文件名扩展名推断 MIME（ComfyUI 的 /view 不带 Content-Type 之外的元信息，
/// 而落库与前端展示都依赖准确的 mimeType）。
fn mime_for_file_name(filename: &str) -> &'static str {
    match filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        _ => "image/png",
    }
}

/// 纳秒时间戳（无额外随机数依赖）。
fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0)
}

/// 随机种子：纳秒时间戳与进程 id 混合，避免 ComfyUI 命中相同图缓存。
fn random_seed() -> u64 {
    (now_nanos() as u64) ^ (u64::from(std::process::id()) << 32)
}

/// /prompt 的 client_id（ComfyUI 用它串起本次会话的进度事件）。
fn random_id() -> String {
    format!("snow-{:x}-{}", now_nanos(), std::process::id())
}
