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
use crate::comfyui::contract;
use crate::comfyui::validate::{format_problems, validate_graph, Problem};
use crate::comfyui::workflow;
use tokio_util::sync::CancellationToken;

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
        // 必须使用解析出的模型项规范 `.model`：请求可能是模型项 `.id`
        // （如 `edit-id`），拿别名去当工作流名会找不到或命中别的同名字典。
        let workflow_raw = if explicit_workflow.is_empty() {
            model_item
                .as_ref()
                .map(|item| item.model.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| model.trim().to_string())
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
        // 与 describe 共用同一套解析：画布名先匹配完整文件名，避免
        // get_workflow 因缺少 `.json` 请求错文件。
        let object_info_text = cached_object_info_text(&client).await;
        let (template, workflow_source) = crate::comfyui::resolve_workflow_graph(
            &client,
            &workflow_raw,
            object_info_text.as_deref(),
        )
        .await?;

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
        // 自动推导打底 + 用户显式覆盖逐项合并：只写 `{"negative": []}` 的渠道
        // 不该因此丢掉 prompt / seed / width / height 的自动映射。
        let derived = crate::comfyui::convert::semantic_param_map(&template);
        let overrides = if param_map_raw.is_empty() {
            HashMap::new()
        } else {
            workflow::parse_param_map(param_map_raw)?
        };
        if derived.is_empty() && overrides.is_empty() {
            notes.push(format!(
                "no semantic parameter could be derived from {workflow_source}: prompt/negative/seed/size are not injected"
            ));
        } else if !derived.is_empty() {
            notes.push(format!(
                "parameter map auto-derived from {workflow_source}: {}",
                derived
                    .iter()
                    .map(|(name, paths)| format!("{name}={}", paths.join("+")))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !overrides.is_empty() {
            notes.push(format!(
                "explicit paramMap overrides: {}",
                overrides.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        let param_map = workflow::merge_param_map(derived, overrides);
        // 有效尺寸来源：请求 / 模型默认 / 渠道默认 > 模板自带尺寸 > 1024 兜底。
        // 只有「有明确来源」时才同步模板布局；否则模板的 layout 保持原样，
        // 避免默认 1024 把作者设计的 1536x1024 构图改坏。
        let (width, height, sync_size) = match workflow::parse_size(size.as_deref()) {
            Some((width, height)) => (width, height, true),
            None => match template_size(&template, &param_map) {
                Some((width, height)) => (width, height, false),
                None => (1024, 1024, false),
            },
        };
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
        // runtime 的 task_id 令牌：用户点停止时前端也会取消这个 id。
        // 同时检查它，provider 才能在 runtime 的等待之外感知取消并返回清理状态。
        let runtime_cancel_token = args
            .get("__snowImageGeneration")
            .and_then(|context| context.get("taskId"))
            .and_then(Value::as_str)
            .and_then(crate::api::cancel::get_tool_execution_token);

        let client_id = format!("snow-{:x}-{}", now_nanos(), std::process::id());
        let requests = n.max(1);

        let cancel_token = cancel_id
            .as_deref()
            .and_then(crate::api::cancel::get_tool_execution_token);
        let mut content: Vec<Value> = Vec::new();
        let mut last_error: Option<String> = None;
        let mut cancelled = false;
        let mut cleanup_detail: Option<String> = None;

        for index in 0..requests {
            // 已取消：不要继续提交剩余的 n 个请求。
            if user_cancelled(cancel_token.as_ref(), runtime_cancel_token.as_ref()) {
                cancelled = true;
                break;
            }
            let prompt = prompts
                .get(index)
                .copied()
                .or_else(|| prompts.first().copied())
                .unwrap_or_default();
            let request_images: &[ReferenceImage] = images.get(index).copied().unwrap_or(&[]);
            let request_seed = seed
                .map(|value| value.wrapping_add(index as u64))
                .unwrap_or_else(random_seed);

            // 提交前把持久化任务推进到 running：与其它 provider 一致，
            // 任务卡才能显示「引擎执行中」而不是一直 pending（幂等，只发首个请求）。
            if index == 0 {
                runtime::request_started(args, None).await?;
            }

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
                runtime_cancel_token.as_ref(),
                // 只有「尺寸有明确来源」时才允许改模板布局的宽高。
                sync_size,
            )
            .await
            {
                Ok((blocks, mut request_notes)) => {
                    content.extend(blocks);
                    notes.append(&mut request_notes);
                }
                Err(error) => {
                    let message = error.to_string();
                    eprintln!(
                        "[imagegen] ComfyUI request {}/{requests} failed: {message}",
                        index + 1
                    );
                    last_error = Some(message.clone());
                    // 只有「取消导致的失败」才把 cleanup 归属到本次取消；
                    // 普通错误不能留下可能被后续取消误用的详情。
                    if user_cancelled(cancel_token.as_ref(), runtime_cancel_token.as_ref()) {
                        cancelled = true;
                        if message.contains("(cleanup:") {
                            cleanup_detail = Some(message);
                        }
                        break;
                    }
                }
            }
        }

        // --- 3. 汇总：至少一张图才算成功（取消是例外） ---
        let generated = content.len();
        if generated == 0 && !cancelled {
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
        if cancelled {
            result["status"] = json!("cancelled");
            let detail = cleanup_detail
                .as_deref()
                .unwrap_or("cancelled before the engine confirmed cleanup");
            result["contentPreview"] = json!(format!(
                "Image generation cancelled after {generated} image(s). {detail}"
            ));
        }
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
    runtime_cancel: Option<&CancellationToken>,
    // 调用方是否显式指定了 size；只有显式时才同步布局尺寸。
    sync_size: bool,
) -> napi::Result<(Vec<Value>, Vec<String>)> {
    // 取一次克隆后反复查询：取消会先把 token 标记为已取消，持有克隆比
    // 每轮查注册表更可靠（也避免 500ms 一次的锁竞争）。
    let cancel_token = cancel_id.and_then(crate::api::cancel::get_tool_execution_token);
    // 与其它 provider 共用真实请求限流：maxConcurrentImages 对 ComfyUI 同样生效。
    // 等名额期间必须能取消，否则名额释放后仍会提交工作流。
    let _permit = tokio::select! {
        biased;
        _ = wait_cancel(cancel_token.as_ref(), runtime_cancel) => {
            return Err(generic_error(
                "ComfyUI generation cancelled while waiting for a concurrency slot".to_string(),
            ));
        }
        permit = limiter::acquire() => permit,
    };
    if user_cancelled(cancel_token.as_ref(), runtime_cancel) {
        return Err(generic_error(
            "ComfyUI generation cancelled before submit".to_string(),
        ));
    }

    // 与 Agent 侧共用同一份契约：未知键 / 类型不符 / 结构化输入不是合法 JSON
    // 时，提交前直接失败，而不是让模型以为参数已经生效。
    let object_info = cached_object_info_text(client)
        .await
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let request_contract =
        contract::derive_contract(template, object_info.as_ref(), Some(param_map));
    let param_problems =
        contract::validate_params(param_map, Some(&request_contract), extra_params);
    if !param_problems.is_empty() {
        return Err(generic_error(format!(
            "ComfyUI params rejected before submit: {}",
            param_problems.join(" | ")
        )));
    }

    let prepared = workflow::prepare_graph(
        template,
        param_map,
        prompt,
        negative,
        width,
        height,
        seed,
        extra_params,
        sync_size,
    );
    let mut graph = prepared.graph;
    let mut injected = prepared.injected;
    let mut warnings = prepared.warnings;

    // --- a. 参考图上传（ComfyUI input 目录），把上传后的名字写进对应 image 键 ---
    if !request_images.is_empty() {
        let mut used_paths: HashMap<String, usize> = HashMap::new();
        for (index, image) in request_images.iter().enumerate() {
            let paths = image_slots(param_map, index);
            if paths.is_empty() {
                let semantic = if index == 0 {
                    "image".to_string()
                } else {
                    format!("image{}", index + 1)
                };
                // 用户明确给了参考图却没有落点：静默跳过会让模型以为图已生效。
                // 提交前失败，让调用方补 paramMap 或去掉多余的图。
                return Err(generic_error(format!(
                    "reference image #{} was provided but the workflow has no `{semantic}` slot. Add an explicit paramMap entry (\"{semantic}\": \"<node>.<input>\") or remove the image.",
                    index + 1
                )));
            }
            // 上传前确认每个落点都真实存在，且不同请求不能共用同一落点：
            // 两个槽位都连到同一个 LoadImage 时，后一张会静默覆盖前一张。
            for path in &paths {
                if workflow::read_path(&graph, path).is_none() {
                    return Err(generic_error(format!(
                        "reference image #{} maps to `{path}`, which does not exist in the workflow",
                        index + 1
                    )));
                }
                // 用归一化路径作为 key：`5.image` 与 `5.inputs.image`
                // 写入同一个输入，必须视为同一个目标。
                if let Some(previous) =
                    used_paths.insert(workflow::normalize_path(path), index)
                {
                    return Err(generic_error(format!(
                        "reference image #{} and #{} both map to `{path}`; the workflow cannot hold two different images in one slot",
                        previous + 1,
                        index + 1
                    )));
                }
            }
            let payload = ImagePayload {
                data: image.data.clone(),
                mime_type: image.mime_type.clone(),
            };
            let uploaded = client.upload_image(&payload).await?;
            let written = workflow::set_value(
                &mut graph,
                Some(&paths),
                json!(uploaded),
                if index == 0 { "image" } else { "image<n>" },
                &mut warnings,
            );
            if written.len() < paths.len() {
                return Err(generic_error(format!(
                    "reference image #{} could not be written to all mapped inputs ({} of {}); the workflow would run with the template's old image",
                    index + 1,
                    written.len(),
                    paths.len()
                )));
            }
            injected.extend(written);
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
    // 预检/上传可能耗时；提交前再确认一次，避免取消后仍把工作流塞进引擎。
    if user_cancelled(cancel_token.as_ref(), runtime_cancel) {
        return Err(generic_error(
            "ComfyUI generation cancelled before submit".to_string(),
        ));
    }
    // 提交前武装：即使 ComfyUI 已受理但响应丢失，守卫也能按 client_id 找回。
    let mut cleanup_guard = ComfyCleanupGuard::new(client, client_id);
    let prompt_id = client.submit_prompt(&graph, client_id).await?;
    cleanup_guard.set_prompt_id(&prompt_id);
    // --- d. 轮询 history（可被用户取消） ---
    let (references, used_preview) = match wait_comfyui_history(
        client,
        &prompt_id,
        timeout_secs,
        cancel_token.as_ref(),
        runtime_cancel,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            // 只清理自己提交的 prompt：排队中删队列，运行中才中断。
            // 查询失败不构成全局中断的理由（那会误伤别人的任务）。
            let cleanup =
                cleanup_comfyui_task(client, Some(&prompt_id), client_id).await;
            cleanup_guard.disarm();
            return Err(generic_error(format!(
                "{error} (cleanup: {})",
                cleanup.as_str()
            )));
        }
    };
    if used_preview {
        warnings.push(
            "workflow has no SaveImage node; returned PreviewImage (temp) files instead".to_string(),
        );
    }
    if references.is_empty() {
        cleanup_guard.disarm();
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
    cleanup_guard.disarm();
    Ok((blocks, warnings))
}

/// 模板自带的尺寸（EmptyLatentImage 等）；没有则返回 None。
fn template_size(
    template: &Value,
    param_map: &HashMap<String, Vec<String>>,
) -> Option<(u64, u64)> {
    let width = param_map
        .get("width")?
        .first()
        .and_then(|path| workflow::read_path(template, path))
        .and_then(Value::as_u64)?;
    let height = param_map
        .get("height")?
        .first()
        .and_then(|path| workflow::read_path(template, path))
        .and_then(Value::as_u64)?;
    Some((width, height))
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

/// 等待任一取消 token 触发；两者都为空时永久等待（供 `select!` 使用）。
async fn wait_cancel(
    cancel_token: Option<&CancellationToken>,
    runtime_token: Option<&CancellationToken>,
) {
    match (cancel_token, runtime_token) {
        (Some(first), Some(second)) => {
            tokio::select! {
                _ = first.cancelled() => {}
                _ = second.cancelled() => {}
            }
        }
        (Some(token), None) | (None, Some(token)) => token.cancelled().await,
        (None, None) => std::future::pending::<()>().await,
    }
}

/// 用户取消：持有的是 token 克隆，取消后仍能看到 `is_cancelled`。
fn user_cancelled(
    cancel_token: Option<&CancellationToken>,
    runtime_token: Option<&CancellationToken>,
) -> bool {
    cancel_token
        .map(|token| token.is_cancelled())
        .unwrap_or(false)
        || runtime_token
            .map(|token| token.is_cancelled())
            .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupOutcome {
    RemovedFromQueue,
    LeftRunning,
    Absent,
    Failed,
    Unknown,
}

impl CleanupOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::RemovedFromQueue => "removed from queue",
            Self::LeftRunning => {
                "engine still running (targeted interrupt not attempted: engine version support unconfirmed)"
            }
            Self::Absent => "not in queue (already finished or unknown)",
            Self::Failed => "cleanup request failed",
            Self::Unknown => "cleanup state unknown (queue query failed)",
        }
    }
}

/// 生成 future 被丢弃（取消/超时）时的兜底清理。
///
/// 在 `submit_prompt` **之前**武装：ComfyUI 已受理但响应丢失/取消时，
/// Snow 没有 prompt_id，仍可按 `client_id` 找回自己的任务。
struct ComfyCleanupGuard {
    client: ComfyUiClient,
    prompt_id: Option<String>,
    client_id: String,
    armed: bool,
}

impl ComfyCleanupGuard {
    fn new(client: &ComfyUiClient, client_id: &str) -> Self {
        Self {
            client: client.clone(),
            prompt_id: None,
            client_id: client_id.to_string(),
            armed: true,
        }
    }

    fn set_prompt_id(&mut self, prompt_id: &str) {
        self.prompt_id = Some(prompt_id.to_string());
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ComfyCleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let client = self.client.clone();
        let prompt_id = self.prompt_id.clone();
        let client_id = self.client_id.clone();
        // Drop 不能 await：把清理交给运行时后台执行。没有活跃 runtime
        // （进程正在退出）时什么也做不了，忽略即可。
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                // 提交刚发出时任务可能还没入队：`Absent` 时短暂重试，
                // 避免把「还没出现」当成「已经结束」。
                for attempt in 0..3 {
                    let outcome =
                        cleanup_comfyui_task(&client, prompt_id.as_deref(), &client_id)
                            .await;
                    if outcome != CleanupOutcome::Absent || attempt == 2 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            });
        }
    }
}

/// 尽力清理自己提交的任务，绝不误伤其它任务。
///
/// - 排队中：只删除自己的 prompt；
/// - 运行中且没有别人的任务在排队：才使用全局中断；
/// - 运行中但别人在排队：**不中断**，返回 `LeftRunning`，让调用方如实告知
///   用户「Snow 已停止等待，引擎任务仍在运行」；
/// - 查不到 / 查询失败：`Absent` / `Unknown`，不猜。
async fn cleanup_comfyui_task(
    client: &ComfyUiClient,
    prompt_id: Option<&str>,
    client_id: &str,
) -> CleanupOutcome {
    let Ok(queue) = client.queue().await else {
        return CleanupOutcome::Unknown;
    };
    let entries = |key: &str| -> Vec<&Value> {
        queue
            .get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().collect())
            .unwrap_or_default()
    };
    let running = entries("queue_running");
    let pending = entries("queue_pending");
    let is_mine = |entry: &Value| -> bool {
        match prompt_id {
            Some(prompt_id) => queue_prompt_id(entry) == Some(prompt_id),
            None => queue_client_id(entry) == Some(client_id),
        }
    };

    if let Some(entry) = pending.iter().find(|entry| is_mine(*entry)) {
        let Some(actual_id) = queue_prompt_id(*entry) else {
            return CleanupOutcome::Failed;
        };
        if client.queue_delete(&[actual_id.to_string()]).await.is_err() {
            return CleanupOutcome::Failed;
        }
        // 删除后复查：如果它已经开始运行，删除请求成功也不算真的删掉。
        return match client.queue().await {
            Ok(after) => {
                let still_pending = after
                    .get("queue_pending")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .any(|item| queue_prompt_id(item) == Some(actual_id))
                    })
                    .unwrap_or(false);
                let now_running = after
                    .get("queue_running")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .any(|item| queue_prompt_id(item) == Some(actual_id))
                    })
                    .unwrap_or(false);
                if still_pending {
                    CleanupOutcome::Failed
                } else if now_running {
                    CleanupOutcome::LeftRunning
                } else {
                    CleanupOutcome::RemovedFromQueue
                }
            }
            Err(_) => CleanupOutcome::Unknown,
        };
    }

    if running.iter().any(|entry| is_mine(*entry)) {
        // 引擎是否支持定向中断未确认：不自动执行可能退化为全局中断的请求。
        // 明确返回「Snow 已停止等待，引擎任务仍在运行」，不假装已停止。
        return CleanupOutcome::LeftRunning;
    }
    CleanupOutcome::Absent
}

/// `/queue` 条目是 `[number, prompt_id, prompt, extra_data, outputs_to_execute]`。
fn queue_prompt_id(entry: &Value) -> Option<&str> {
    entry.get(1).and_then(Value::as_str)
}

fn queue_client_id(entry: &Value) -> Option<&str> {
    entry
        .get(3)
        .and_then(|extra| extra.get("client_id"))
        .and_then(Value::as_str)
}

/// 轮询 `GET /history/{prompt_id}` 直到任务完成，返回 outputs 里收集到的图片引用。
async fn wait_comfyui_history(
    client: &ComfyUiClient,
    prompt_id: &str,
    timeout_secs: Option<u64>,
    cancel_token: Option<&CancellationToken>,
    runtime_token: Option<&CancellationToken>,
) -> napi::Result<(Vec<Value>, bool)> {
    let total = Duration::from_secs(
        timeout_secs
            .unwrap_or(REQUEST_TIMEOUT_SECS)
            .max(MIN_WAIT_SECS),
    );
    let deadline = Instant::now() + total;

    loop {
        if user_cancelled(cancel_token, runtime_token) {
            return Err(generic_error(format!(
                "ComfyUI generation cancelled by user (prompt_id={prompt_id})"
            )));
        }
        // HTTP 等待期间也要能被取消：请求卡住时不能等它超时才响应停止。
        let body = tokio::select! {
            biased;
            _ = wait_cancel(cancel_token, runtime_token) => {
                return Err(generic_error(format!(
                    "ComfyUI generation cancelled by user (prompt_id={prompt_id})"
                )));
            }
            result = client.history(prompt_id) => result?,
        };
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
