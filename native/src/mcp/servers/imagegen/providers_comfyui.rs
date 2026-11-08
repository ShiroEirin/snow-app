//! ComfyUI 生图分支（薄适配器）。
//!
//! 协议调用在 `crate::comfyui::client`，参数注入/输出收集在 `crate::comfyui::workflow`，
//! 静态校验在 `crate::comfyui::validate`。这里只负责编排：
//! 解析渠道（模型项优先）→ 构建图 → **提交前预检** → 上传参考图 → 提交 → 轮询（可取消）→ 下载。
//!
//! 渠道侧的 `workflow` 是 API 格式工作流；「渠道 = 端点，模型 = 工作流」，
//! `models[]` 每项可带自己的 `workflow` / `paramMap`，空则回退渠道级。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::*;
use crate::comfyui::client::{now_nanos, random_seed, ComfyUiClient, ImagePayload};
use crate::comfyui::validate::{format_problems, validate_graph, Problem};
use crate::comfyui::workflow;

/// 轮询间隔：ComfyUI 出图通常在秒级，500ms 足够且不吵闹。
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// 即便调用方给了更小的超时，也至少等 30 秒（模型加载/首次运行很慢）。
const MIN_WAIT_SECS: u64 = 30;
/// `/object_info` 缓存有效期：一次会话里反复出图不该每次都拉 8MB。
const OBJECT_INFO_TTL: Duration = Duration::from_secs(120);

fn object_info_cache() -> &'static Mutex<HashMap<String, (Instant, String)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, String)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取（带缓存的）`/object_info` 原文；网络失败时返回 None（跳过预检，不阻断出图）。
async fn cached_object_info_text(client: &ComfyUiClient) -> Option<String> {
    let key = client.base_url().to_string();
    if let Ok(cache) = object_info_cache().lock() {
        if let Some((fetched_at, text)) = cache.get(&key) {
            if fetched_at.elapsed() < OBJECT_INFO_TTL {
                return Some(text.clone());
            }
        }
    }
    let text = client.object_info_text().await.ok()?;
    if let Ok(mut cache) = object_info_cache().lock() {
        cache.insert(key, (Instant::now(), text.clone()));
    }
    Some(text)
}

/// 提交前静态校验。返回 Some(problems) 表示必须中止；None = 通过或无法校验。
async fn preflight(client: &ComfyUiClient, graph: &Value) -> Option<Vec<Problem>> {
    let text = cached_object_info_text(client).await?;
    let object_info: Value = serde_json::from_str(&text).ok()?;
    let problems = validate_graph(graph, &object_info);
    if problems.is_empty() {
        None
    } else {
        Some(problems)
    }
}

/// 把配置里的 workflow 标识解析成可直接提交的 API 格式图。
///
/// 三种来源，不需要用户维护任何绝对路径：
/// 1. 内联 JSON（以 `{` 开头）；
/// 2. 本地文件：绝对路径，或工作流库里的文件名（`anima-t2i.api.json`）；
/// 3. ComfyUI 自己的画布工作流：按名字走 `/userdata` 拉取并**就地**转成 API 格式，
///    连「先 Pull 再绑定」这一步都省了。
async fn resolve_workflow_template(
    client: &ComfyUiClient,
    raw: &str,
) -> napi::Result<(Value, String)> {
    let raw = raw.trim();

    if raw.starts_with('{') {
        let graph: Value = serde_json::from_str(raw)
            .map_err(|error| generic_error(format!("Invalid ComfyUI workflow JSON: {error}")))?;
        return Ok((graph, "inline JSON".to_string()));
    }

    if let Some(path) = workflow::resolve_workflow_path(raw) {
        let display = path.to_string_lossy().to_string();
        let text = workflow::read_workflow_text(&display)?;
        let graph: Value = serde_json::from_str(&text)
            .map_err(|error| generic_error(format!("Invalid ComfyUI workflow JSON: {error}")))?;
        if !graph.is_object() {
            return Err(generic_error(
                "Invalid ComfyUI workflow: an API-format workflow must be a JSON object of `node id -> node`; a UI-format workflow (nodes[]/links[]) cannot be submitted — bind it through the workflow picker so it gets converted."
                    .to_string(),
            ));
        }
        return Ok((graph, display));
    }

    // 不是本地文件 → 当成 ComfyUI 画布工作流名，走 API 拉取 + 转换。
    let canvas = raw
        .strip_suffix(".json")
        .or_else(|| raw.strip_suffix(".JSON"))
        .unwrap_or(raw);
    let available = client.list_workflows().await.unwrap_or_default();
    let matched = available
        .iter()
        .find(|name| name.eq_ignore_ascii_case(canvas) || name.eq_ignore_ascii_case(raw));
    let Some(name) = matched else {
        return Err(generic_error(format!(
            "ComfyUI workflow \"{raw}\" was not found. Workflow library: {}. ComfyUI canvas workflows ({}): {}.",
            workflow::library_dir()
                .map(|dir| dir.to_string_lossy().to_string())
                .unwrap_or_else(|_| "unavailable".to_string()),
            available.len(),
            if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            },
        )));
    };
    let ui = client.get_workflow(name).await?;
    let object_info = cached_object_info_text(client)
        .await
        .unwrap_or_else(String::new);
    if object_info.is_empty() {
        return Err(generic_error(format!(
            "Cannot convert ComfyUI canvas workflow \"{name}\": /object_info is unreachable."
        )));
    }
    let converted = crate::comfyui::convert::convert_ui_to_api(&ui, &object_info)?;
    let graph = converted
        .to_json()
        .get("workflow")
        .cloned()
        .ok_or_else(|| generic_error("Converted ComfyUI workflow is empty".to_string()))?;
    Ok((graph, format!("ComfyUI canvas workflows/{name}")))
}

impl ImageGenService {
    /// ComfyUI 分支：按渠道配置的工作流模板逐请求出图，汇总图片块返回。
    ///
    /// `api_key` 不参与（本地服务无需凭据）；`quality` 对 ComfyUI 无意义，
    /// 仅为与其它 provider 的调用形态保持一致而保留。
    #[allow(clippy::too_many_arguments)]
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
        // --- 0. 选模型项：未知 model 必须报错，不能静默回退到别的工作流 ---
        let model_item = match channel_config.find_model_config_strict(model) {
            Ok(item) => item,
            Err(message) => return Err(generic_error(message)),
        };

        // --- 1. 工作流：模型项 > 渠道级 > 「模型名就是工作流名」 ---
        let item_workflow = model_item
            .as_ref()
            .map(|item| item.workflow.trim())
            .unwrap_or("");
        let explicit_workflow = if item_workflow.is_empty() {
            channel_config.workflow.trim()
        } else {
            item_workflow
        };
        // 没有单独绑工作流时，模型名本身就是工作流标识（模型即工作流）。
        let workflow_raw = if explicit_workflow.is_empty() {
            let from_model = if model.trim().is_empty() {
                model_item
                    .as_ref()
                    .map(|item| item.model.trim().to_string())
                    .unwrap_or_default()
            } else {
                model.trim().to_string()
            };
            from_model
        } else {
            explicit_workflow.to_string()
        };
        if workflow_raw.is_empty() {
            return Err(generic_error(
                "ComfyUI channel requires a workflow: pick one in Settings -> Image generation (the picker lists your ComfyUI canvas workflows and the already-converted library)."
                    .to_string(),
            ));
        }

        let client = ComfyUiClient::new(base_url, timeout_secs).await?;
        let (template, workflow_source) = resolve_workflow_template(&client, &workflow_raw).await?;

        // --- 2. 语义名映射：显式配置优先；留空则由工作流图运行时推导 ---
        let item_param_map = model_item
            .as_ref()
            .map(|item| item.param_map.trim())
            .unwrap_or("");
        let param_map_raw = if item_param_map.is_empty() {
            channel_config.param_map.trim()
        } else {
            item_param_map
        };
        let mut notes: Vec<String> = Vec::new();
        let param_map = if param_map_raw.is_empty() {
            let derived = crate::comfyui::convert::semantic_param_map(&template);
            if derived.is_empty() {
                notes.push(format!(
                    "no semantic parameter could be derived from {workflow_source}: prompt/negative/seed/size are not injected"
                ));
                HashMap::new()
            } else {
                notes.push(format!(
                    "parameter map auto-derived from {workflow_source}: {}",
                    derived
                        .iter()
                        .map(|(name, paths)| format!("{name}={}", paths.join("+")))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                derived.into_iter().collect::<HashMap<String, Vec<String>>>()
            }
        } else {
            workflow::parse_param_map(param_map_raw)?
        };
        let (width, height) = workflow::parse_size(size.as_deref()).unwrap_or((1024, 1024));
        let negative = args
            .get("negativePrompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let extra_params = args.get("params").and_then(Value::as_object);
        // 用户停止时由调用层注册的取消令牌 id（见 call.rs）。
        let cancel_id = args
            .get("__snowComfyCancelId")
            .and_then(Value::as_str)
            .map(str::to_string);

        let client_id = format!("snow-{:x}-{}", now_nanos(), std::process::id());
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
            let request_seed = seed
                .map(|value| value.wrapping_add(index as u64))
                .unwrap_or_else(random_seed);

            match run_comfyui_request(
                &client,
                &client_id,
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
                cancel_id.as_deref(),
            )
            .await
            {
                Ok((blocks, mut request_notes)) => {
                    content.extend(blocks);
                    notes.append(&mut request_notes);
                }
                Err(error) => {
                    eprintln!(
                        "[imagegen] ComfyUI request {}/{requests} failed: {error}",
                        index + 1
                    );
                    last_error = Some(error.to_string());
                }
            }
        }

        // --- 3. 汇总：至少一张图才算成功 ---
        let generated = content.len();
        if generated == 0 {
            return Err(generic_error(format!(
                "ComfyUI generation returned no images.{}",
                last_error
                    .map(|error| format!(" Last error: {error}"))
                    .unwrap_or_default()
            )));
        }
        let mut result = build_result(
            prompts.first().copied().unwrap_or_default(),
            model,
            "comfyui",
            generated,
            content,
            Vec::new(),
        );
        if !notes.is_empty() {
            notes.sort();
            notes.dedup();
            if let Some(preview) = result.get("contentPreview").and_then(Value::as_str) {
                result["contentPreview"] =
                    json!(format!("{preview} Notes: {}", notes.join(" | ")));
            }
        }
        Ok(result)
    }
}

/// 单个请求：构建图 → 预检 → 上传参考图 → 提交 → 轮询 → 取图。
#[allow(clippy::too_many_arguments)]
async fn run_comfyui_request(
    client: &ComfyUiClient,
    client_id: &str,
    template: &Value,
    param_map: &HashMap<String, Vec<String>>,
    prompt: &str,
    negative: Option<&str>,
    width: u64,
    height: u64,
    seed: u64,
    request_images: &[ReferenceImage],
    extra_params: Option<&serde_json::Map<String, Value>>,
    timeout_secs: Option<u64>,
    cancel_id: Option<&str>,
) -> napi::Result<(Vec<Value>, Vec<String>)> {
    let prepared = workflow::prepare_graph(
        template,
        param_map,
        prompt,
        negative,
        width,
        height,
        seed,
        extra_params,
    );
    let mut graph = prepared.graph;
    let mut injected = prepared.injected;
    let mut warnings = prepared.warnings;

    // --- a. 参考图上传（ComfyUI input 目录），把上传后的名字写进对应 image 键 ---
    if !request_images.is_empty() {
        for (index, image) in request_images.iter().enumerate() {
            let paths = image_slots(param_map, index);
            if paths.is_empty() {
                warnings.push(format!(
                    "reference image #{} has no paramMap slot (expected \"image{}\"); skipped — map it explicitly if your workflow has more image inputs",
                    index + 1,
                    if index == 0 { String::new() } else { (index + 1).to_string() }
                ));
                continue;
            }
            let payload = ImagePayload {
                data: image.data.clone(),
                mime_type: image.mime_type.clone(),
            };
            let uploaded = client.upload_image(&payload).await?;
            injected.extend(workflow::set_value(
                &mut graph,
                Some(&paths),
                json!(uploaded),
                if index == 0 { "image" } else { "image<n>" },
                &mut warnings,
            ));
        }
    }

    // --- b. 提交前预检：缺节点 / 断链 / 缺必填连线输入，就地失败 ---
    if let Some(problems) = preflight(client, &graph).await {
        let mut message = format!(
            "ComfyUI workflow rejected before submit ({} problem(s)): {}",
            problems.len(),
            format_problems(&problems, 5)
        );
        if !warnings.is_empty() {
            message.push_str(&format!(" Warnings: {}", warnings.join(" | ")));
        }
        return Err(generic_error(message));
    }

    // --- c. 提交任务 ---
    let prompt_id = client.submit_prompt(&graph, client_id).await?;
    // --- d. 轮询 history（可被用户取消） ---
    let references = match wait_comfyui_history(client, &prompt_id, timeout_secs, cancel_id).await {
        Ok(references) => references,
        Err(error) => {
            // 超时/取消/失败都尽量把任务从队列里摘掉，不再占显存。
            let _ = client.interrupt().await;
            let _ = client.queue_delete(&[prompt_id.clone()]).await;
            return Err(error);
        }
    };
    if references.is_empty() {
        let mut message = format!(
            "ComfyUI workflow finished (prompt_id={prompt_id}) without any image output; make sure the workflow contains a SaveImage / PreviewImage node."
        );
        if !warnings.is_empty() {
            message.push_str(&format!(" Warnings: {}", warnings.join(" | ")));
        }
        return Err(generic_error(message));
    }

    // --- e. 取图 ---
    let mut blocks = Vec::with_capacity(references.len());
    for reference in &references {
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
        let bytes = client.view_image(filename, subfolder, kind).await?;
        blocks.push(workflow::image_block(&bytes, filename));
    }
    Ok((blocks, warnings))
}

/// 第 `index` 张参考图应写入的点号键：`image`、`image2`、`image3`…（也接受 `image_2`）。
fn image_slots(param_map: &HashMap<String, Vec<String>>, index: usize) -> Vec<String> {
    if index == 0 {
        return param_map.get("image").cloned().unwrap_or_default();
    }
    let n = index + 1;
    param_map
        .get(&format!("image{n}"))
        .or_else(|| param_map.get(&format!("image_{n}")))
        .cloned()
        .unwrap_or_default()
}

/// 用户取消：只要调用层注册过取消 id 且令牌已触发，就中断并清队列。
fn user_cancelled(cancel_id: Option<&str>) -> bool {
    let Some(id) = cancel_id else {
        return false;
    };
    crate::api::cancel::get_token(id)
        .map(|token| token.is_cancelled())
        .unwrap_or(false)
}

/// 轮询 `GET /history/{prompt_id}` 直到任务完成，返回 outputs 里收集到的图片引用。
async fn wait_comfyui_history(
    client: &ComfyUiClient,
    prompt_id: &str,
    timeout_secs: Option<u64>,
    cancel_id: Option<&str>,
) -> napi::Result<Vec<Value>> {
    let total = Duration::from_secs(
        timeout_secs
            .unwrap_or(REQUEST_TIMEOUT_SECS)
            .max(MIN_WAIT_SECS),
    );
    let deadline = Instant::now() + total;

    loop {
        if user_cancelled(cancel_id) {
            return Err(generic_error(format!(
                "ComfyUI generation cancelled by user (prompt_id={prompt_id})"
            )));
        }
        let body = client.history(prompt_id).await?;
        // 完成后的 entry 形如 {outputs: {...}, status: {status_str: "success", completed: true}}；
        // 未完成时 /history 里根本看不到该 prompt_id。
        if let Some(entry) = body.get(prompt_id) {
            if entry.get("outputs").is_some() || entry.get("status").is_some() {
                if let Some(error) = workflow::entry_error(entry) {
                    return Err(generic_error(format!("ComfyUI workflow failed: {error}")));
                }
                return Ok(workflow::collect_output_images(entry.get("outputs")));
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
