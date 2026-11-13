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
use crate::comfyui::policy::{ControlPolicy, WriteDecision, WriteSource};
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
        // 一次请求的图片数量决定需要覆盖哪些槽位（image / image2 … imageN）。
        let image_count = images.iter().map(|group| group.len()).max().unwrap_or(0);
        // 角色解歧只认**覆盖本次请求全部槽位**的显式 paramMap；自动推导，
        // 或只映射其中一部分（两张图只写 image）都不算。
        let explicit_image_slots = explicit_image_slots_from_overrides(&overrides, image_count);
        // 锁集合需要在 merge 之后继续使用推导映射，先留一份克隆。
        let derived_for_locks = derived.clone();
        let param_map = workflow::merge_param_map(derived, overrides);
        // 有效尺寸来源：请求 / 模型默认 / 渠道默认 > 模板自带尺寸 > 1024 兜底。
        // 显式请求必须先过语法与能力白名单：不能出现「要 16:9、静默拿到模板
        // 尺寸」这种假成功（sanitize_size 丢弃与 parse_size 失败都必须报错）。
        // 显式传了非字符串（数字/对象）时明确报错，不能静默当作省略。
        if let Some(raw_value) = args.get("size") {
            if !raw_value.is_null() && raw_value.as_str().is_none() {
                return Err(Error::new(
                    napi::Status::InvalidArg,
                    "`size` must be a string like `1024x1024`".to_string(),
                ));
            }
        }
        let explicit_size = args
            .get("size")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        // 默认尺寸（模型项 > 渠道）用于区分「没配」与「配了但被能力白名单
        // 过滤」：后者必须留下告警，不能静默回退模板。
        let configured_default = model_item
            .as_ref()
            .and_then(|item| non_empty(&item.default_size))
            .or_else(|| non_empty(&channel_config.default_size));
        // 能力声明只用于错误/告警消息：让调用方知道该改哪一项，而不是猜。
        let capability_summary = {
            let ratios = model_item
                .as_ref()
                .map(|item| item.supported_ratios.trim())
                .unwrap_or("");
            let resolutions = model_item
                .as_ref()
                .map(|item| item.supported_resolutions.trim())
                .unwrap_or("");
            match (ratios.is_empty(), resolutions.is_empty()) {
                (true, true) => "none declared".to_string(),
                (false, true) => format!("supportedRatios=`{ratios}`"),
                (true, false) => format!("supportedResolutions=`{resolutions}`"),
                (false, false) => format!(
                    "supportedRatios=`{ratios}`, supportedResolutions=`{resolutions}`"
                ),
            }
        };
        // 顶层负向词在授权前解析：locked 判断需要知道调用方有没有**显式**给值。
        let negative = args
            .get("negativePrompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        // Agent 控制策略：模型项 `agentControls` 稀疏覆盖默认 exposure。
        let policy = ControlPolicy::from_json(
            model_item
                .as_ref()
                .and_then(|item| item.agent_controls.as_ref()),
        )
        .map_err(|message| Error::new(napi::Status::InvalidArg, message))?;
        // 锁集合基于**原始** paramMap 计算：后续 effective map 会剔除被锁
        // 语义，在裁剪后的映射上重算会丢失显式重映射的原目标。
        let locked_paths = locked_targets(&policy, &param_map, &derived_for_locks);
        // 布局越界默认拒绝；只有调用方显式接受时才继续。
        let allow_layout_overflow = args
            .get("allowLayoutOverflow")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let ratio_request = explicit_size.filter(|value| value.contains(':'));
        let (width, height, mut sync_size, size_warnings) = if let Some(ratio) = ratio_request {
            // 比例白名单：模型声明了 supportedRatios 却不含它时，sanitize_size 会
            // 丢弃显式请求（size = None）；这里保持一致，明确拒绝。
            if size.is_none() {
                return Err(Error::new(
                    napi::Status::InvalidArg,
                    format!(
                        "aspect ratio `{ratio}` is not allowed by this model's image capabilities ({capability_summary})"
                    ),
                ));
            }
            let candidates = model_item
                .as_ref()
                .map(|item| workflow::resolution_candidates(item.supported_resolutions.trim()))
                .unwrap_or_default();
            let default_pixels = configured_default
                .as_deref()
                .and_then(|raw| workflow::parse_size(Some(raw)))
                .or_else(|| template_size(&template, &param_map));
            let (width, height) = workflow::resolve_ratio_size(ratio, &candidates, default_pixels)
                .map_err(|message| Error::new(napi::Status::InvalidArg, message))?;
            (
                width,
                height,
                true,
                vec![format!(
                    "aspect ratio `{ratio}` normalized to {width}x{height}"
                )],
            )
        } else {
            workflow::resolve_comfy_size(
                explicit_size,
                configured_default.as_deref(),
                size.as_deref(),
                template_size(&template, &param_map),
                &capability_summary,
            )
            .map_err(|message| Error::new(napi::Status::InvalidArg, message))?
        };
        // 最终尺寸统一过预算：比例候选、默认值、模板值都不能绕过
        // （`resolve_comfy_size` 已覆盖显式/默认，这里兜住比例分支）。
        workflow::ensure_pixel_budget(width, height)
            .map_err(|message| Error::new(napi::Status::InvalidArg, message))?;
        // 模板自身的宽高也要查：即使 paramMap 关闭了 width/height，最终工作流
        // 仍可能按模板尺寸执行，不能只看 resolve 的默认值。
        let derived_size_map: HashMap<String, Vec<String>> = derived_for_locks
            .iter()
            .map(|(semantic, paths)| (semantic.clone(), paths.clone()))
            .collect();
        if let Some((template_width, template_height)) =
            template_size(&template, &derived_size_map)
        {
            workflow::ensure_pixel_budget(template_width, template_height)
                .map_err(|message| Error::new(napi::Status::InvalidArg, message))?;
        }
        notes.extend(size_warnings);
        // --- 1b. 写入授权（R1）：顶层参数、参考图与引擎默认值走同一套 exposure。
        // locked + 显式 → 拒绝；locked + 隐式（随机 seed / 默认尺寸）→ 保留模板值。
        // 只在 params 上做校验会让 `negativePrompt` / `size` 直接绕过策略。
        let mut requested_writes: Vec<(String, WriteSource)> = vec![
            ("prompt".to_string(), WriteSource::Explicit),
            (
                "negative".to_string(),
                if negative.is_some() {
                    WriteSource::Explicit
                } else {
                    WriteSource::Absent
                },
            ),
            (
                "seed".to_string(),
                if seed.is_some() {
                    WriteSource::Explicit
                } else {
                    WriteSource::Implicit
                },
            ),
            (
                "width".to_string(),
                if explicit_size.is_some() {
                    WriteSource::Explicit
                } else {
                    WriteSource::Implicit
                },
            ),
            (
                "height".to_string(),
                if explicit_size.is_some() {
                    WriteSource::Explicit
                } else {
                    WriteSource::Implicit
                },
            ),
        ];
        for index in 0..image_count {
            requested_writes.push((
                if index == 0 {
                    "image".to_string()
                } else {
                    format!("image{}", index + 1)
                },
                WriteSource::Explicit,
            ));
        }
        let mut effective_param_map = param_map.clone();
        let mut size_locked = false;
        for (semantic, source) in requested_writes {
            // 显式关闭（paramMap 空数组）的语义不能被顶层参数静默覆盖：
            // `negative: []` + `negativePrompt` 必须报错，而不是假装生效。
            if source == WriteSource::Explicit
                && param_map
                    .get(&semantic)
                    .map(|paths| paths.is_empty())
                    .unwrap_or(false)
            {
                return Err(Error::new(
                    napi::Status::InvalidArg,
                    format!(
                        "`{semantic}` is disabled by paramMap; remove the override or the lock"
                    ),
                ));
            }
            match policy.authorize_write(&semantic, source) {
                WriteDecision::Allow => {}
                WriteDecision::Preserve => {
                    // width / height 是一对：任一被锁就整体保留模板尺寸，
                    // 否则会出现「只写一半尺寸」的错图。
                    if matches!(semantic.as_str(), "width" | "height") {
                        size_locked = true;
                        continue;
                    }
                    effective_param_map.remove(&semantic);
                    notes.push(format!(
                        "`{semantic}` is locked by agentControls; the template value is preserved"
                    ));
                }
                WriteDecision::Reject(message) => {
                    return Err(Error::new(napi::Status::InvalidArg, message));
                }
            }
        }
        if size_locked {
            effective_param_map.remove("width");
            effective_param_map.remove("height");
            // 尺寸被锁时不能再同步布局宽高：否则只改布局不改 latent。
            sync_size = false;
            notes.push(
                "`size` is locked by agentControls (width/height); the template size is preserved"
                    .to_string(),
            );
        }
        // 模型项 taskContract 显式声明图片输出时跳过静态识别；否则必须在提交前
        // 确认工作流只有图片输出（视频/音频/混合媒体一律拒绝）。
        let declared_image_output = model_item
            .as_ref()
            .and_then(|item| item.task_contract.as_ref())
            .and_then(|contract| contract.get("outputs"))
            .and_then(|outputs| outputs.get("adapter"))
            .and_then(Value::as_str)
            .map(|adapter| adapter.eq_ignore_ascii_case("image"))
            .unwrap_or(false);
        let declared_input_roles = model_item
            .as_ref()
            .and_then(|item| item.task_contract.as_ref())
            .and_then(|contract| contract.get("inputs"));
        // 显式 nodePath 必须真正参与注入：只声明 role 不足以放行，否则执行侧仍会
        // 按 LoadImage 节点 ID 排序，把声明为 base 的图写进 mask 槽。
        let declared_image_paths = workflow::declared_image_slot_map(declared_input_roles, &template)
            .map_err(|message| Error::new(napi::Status::InvalidArg, message))?;
        let has_declared_image_paths = !declared_image_paths.is_empty();
        if has_declared_image_paths {
            for (semantic, paths) in declared_image_paths {
                effective_param_map.insert(semantic, paths);
            }
        }
        // 有效 nodePath 参与注入，但「是否已消歧」必须由 `ensure_reference_roles`
        // 检查**全部槽位**：只要存在任意 nodePath 就置 explicit 会提前放行，
        // 两张图只声明一个槽位仍会按节点顺序猜第二张。
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
        // 首批请求生效的参数（多请求时彼此只差 seed，摘要取第一张即可）。
        let mut effective_summary: Option<Value> = None;

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

            match run_comfyui_request(
                &client,
                &client_id,
                &template,
                &effective_param_map,
                prompt,
                negative.as_deref(),
                width,
                height,
                request_seed,
                seed.is_some(),
                request_images,
                extra_params,
                timeout_secs,
                cancel_id.as_deref(),
                runtime_cancel_token.as_ref(),
                // 只有「尺寸有明确来源」时才允许改模板布局的宽高。
                sync_size,
                explicit_size.is_some(),
                &policy,
                allow_layout_overflow,
                declared_image_output,
                declared_input_roles,
                explicit_image_slots,
                &locked_paths,
                Some(args),
                index == 0,
                // 摘要读取用原始映射：被锁语义的重映射目标必须保留。
                &param_map,
            )
            .await
            {
                Ok((blocks, mut request_notes, effective)) => {
                    content.extend(blocks);
                    notes.append(&mut request_notes);
                    if effective_summary.is_none() {
                        effective_summary = Some(effective);
                    }
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
        // 生效参数摘要：让结果卡片能展示负向词、实际 seed、最终尺寸与采样
        // 参数，而不是只回显请求里传过的值（隐式 seed / 锁定尺寸只有这里能看到）。
        if let Some(mut effective) = effective_summary {
            if let Some(map) = effective.as_object_mut() {
                map.insert("workflow".to_string(), json!(workflow_source.clone()));
            }
            result["effectiveParams"] = effective;
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

/// 收集「不允许任何别名写入」的目标路径：
/// - `paramMap` 空数组显式关闭的语义（锁住推导出的原目标）；
/// - exposure 为 locked 的语义（paramMap 目标优先，否则推导目标）。
fn locked_targets(
    policy: &ControlPolicy,
    param_map: &HashMap<String, Vec<String>>,
    derived: &std::collections::BTreeMap<String, Vec<String>>,
) -> std::collections::HashSet<String> {
    let mut locked: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut lock = |paths: Option<&Vec<String>>| {
        for path in paths.into_iter().flatten() {
            locked.insert(workflow::normalize_path(path).to_ascii_lowercase());
        }
    };
    for (name, paths) in param_map {
        if paths.is_empty() {
            lock(derived.get(name));
        }
    }
    for name in derived.keys().chain(param_map.keys()) {
        if policy.exposure_for(name) != crate::comfyui::policy::Exposure::Locked {
            continue;
        }
        match param_map.get(name).filter(|paths| !paths.is_empty()) {
            Some(paths) => lock(Some(paths)),
            None => lock(derived.get(name)),
        }
    }
    locked
}

/// 校验一条 `params` 写入（语义名 / 点号键），返回可操作错误文本。
///
/// 两条写入路径共享同一套规则：目标锁 → 映射存在性 → 目标存在性 →
/// 策略（exposure / 类型 / 枚举 / 范围）→ 节点技术边界（object_info）。
///
/// **raw 点号键的 opt-in**：`"7.steps"` 这类原始键只有在映射到语义名、
/// 且该语义在 `agentControls` 里没有被 locked（即作者显式开放）时才接受。
/// 未映射的原始输入一律拒绝——这是「专家入口」的唯一开关，不另设配置项。
fn check_param_write(
    key: &str,
    raw_value: &Value,
    param_map: &HashMap<String, Vec<String>>,
    derived: &std::collections::BTreeMap<String, Vec<String>>,
    locked_paths: &std::collections::HashSet<String>,
    object_info: Option<&Value>,
    template: &Value,
    policy: &ControlPolicy,
) -> std::result::Result<(), String> {
    let dotted = key.contains('.');
    let targets: Vec<String> = if dotted {
        vec![key.to_string()]
    } else if let Some(paths) = param_map.get(key) {
        if paths.is_empty() {
            return Err(format!(
                "`{key}` is disabled by paramMap; remove the lock or the param"
            ));
        }
        paths.clone()
    } else {
        return Err(format!(
            "`{key}` is neither a dotted workflow key nor a mapped semantic name; \
             add it to paramMap and expose it in agentControls"
        ));
    };
    // 同一目标可能被多个语义名映射（`steps` 与某个更宽松的别名）：约束必须
    // **取交集**，逐个别名只查自己的规则会被绕过。
    let target_keys: std::collections::HashSet<String> = targets
        .iter()
        .map(|target| workflow::normalize_path(target).to_ascii_lowercase())
        .collect();
    let mut semantics: Vec<String> = derived
        .iter()
        .chain(param_map.iter())
        .filter(|(_, paths)| {
            paths.iter().any(|path| {
                target_keys.contains(&workflow::normalize_path(path).to_ascii_lowercase())
            })
        })
        .map(|(name, _)| name.clone())
        .collect();
    if !dotted {
        semantics.push(key.to_string());
    }
    semantics.sort();
    semantics.dedup();
    // 点号键没有映射到任何语义 = 未知 raw 目标，默认拒绝（不能直写
    // sampler_name / batch_size / filename_prefix 这类未开放输入）。
    if semantics.is_empty() {
        return Err(format!(
            "`{key}` is not mapped to a semantic input; unknown raw workflow inputs are \
             rejected (add it to paramMap and expose it in agentControls)"
        ));
    }
    for target in &targets {
        let normalized = workflow::normalize_path(target).to_ascii_lowercase();
        if locked_paths.contains(&normalized) {
            return Err(format!(
                "`{key}` targets `{target}`, which is locked for this workflow"
            ));
        }
        if workflow::read_path(template, target).is_none()
            && !workflow::input_declared_in_object_info(object_info, template, target)
        {
            return Err(format!(
                "`{key}` -> `{target}` is not an existing workflow input and is not declared as \
                 required/optional by the node schema"
            ));
        }
    }
    for semantic in &semantics {
        if let Some(problem) = policy.check_value(semantic, raw_value) {
            return Err(format!("`{key}` rejected: {problem}"));
        }
    }
    // 策略约束（exposure / options / min-max）与节点技术边界必须同时满足；
    // 校验针对最终写入图的值（对象 / 数组先序列化成 JSON 文本）。
    let value = workflow::serialize_param_value(raw_value);
    let skip_options = semantics
        .iter()
        .any(|semantic| crate::comfyui::policy::is_image_semantic(semantic));
    for target in &targets {
        let Some(bounds) = workflow::input_bounds(object_info, template, target) else {
            continue;
        };
        if let Some(problem) = workflow::check_input_bounds(&bounds, &value, target, skip_options) {
            return Err(format!("`{key}` rejected: {problem}"));
        }
    }
    Ok(())
}

/// 顶层参数（prompt / seed / size / negative）的写入校验。
///
/// 与 `params` 路径共用同一套约束：显式值必须满足 `agentControls` 的范围 /
/// 枚举，且目标路径不能被锁定；隐式值（随机 seed、未显式请求的尺寸）只做
/// 节点技术边界检查，不被策略范围拒绝。抽成纯函数便于回归测试——这些检查
/// 以前只存在于网络请求内部，反例因此可以绕过。
fn validate_top_level_writes(
    writes: &[(&str, Value, bool)],
    policy: &ControlPolicy,
    param_map: &HashMap<String, Vec<String>>,
    derived: &std::collections::BTreeMap<String, Vec<String>>,
    locked_paths: &std::collections::HashSet<String>,
    object_info: Option<&Value>,
    template: &Value,
) -> std::result::Result<(), String> {
    for (semantic, value, explicit) in writes {
        if value.is_null() {
            continue;
        }
        let Some(paths) = param_map.get(*semantic) else {
            continue;
        };
        if *explicit {
            // 自身 + 同目标的所有别名都要满足（约束取交集）：换个更宽松的
            // 别名指向同一输入，不能绕过原语义的 max/min。
            let mut semantics: Vec<String> = vec![(*semantic).to_string()];
            for path in paths {
                let normalized = workflow::normalize_path(path).to_ascii_lowercase();
                for (name, mapped) in derived.iter().chain(param_map.iter()) {
                    if mapped.iter().any(|mapped_path| {
                        workflow::normalize_path(mapped_path).to_ascii_lowercase() == normalized
                    }) {
                        semantics.push(name.clone());
                    }
                }
            }
            semantics.sort();
            semantics.dedup();
            for name in &semantics {
                if let Some(problem) = policy.check_value(name, value) {
                    return Err(format!("`{semantic}` rejected: {problem}"));
                }
            }
            for path in paths {
                if locked_paths.contains(&workflow::normalize_path(path).to_ascii_lowercase()) {
                    return Err(format!(
                        "`{semantic}` targets `{path}`, which is locked for this workflow"
                    ));
                }
            }
        }
        for path in paths {
            let Some(bounds) = workflow::input_bounds(object_info, template, path) else {
                continue;
            };
            if let Some(problem) = workflow::check_input_bounds(&bounds, value, path, false) {
                return Err(problem);
            }
        }
    }
    Ok(())
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
    // 调用方是否显式给了 seed（随机/默认 seed 不应用策略的 min/max）。
    explicit_seed: bool,
    request_images: &[ReferenceImage],
    extra_params: Option<&serde_json::Map<String, Value>>,
    timeout_secs: Option<u64>,
    cancel_id: Option<&str>,
    runtime_cancel: Option<&CancellationToken>,
    // 尺寸是否有明确来源（显式请求或模型/渠道默认）；控制是否同步模板布局。
    sync_size: bool,
    // 调用方是否**显式**给了 size（默认尺寸写不进图时只告警，不阻断）。
    explicit_size: bool,
    // 模型项的 Agent 控制策略（exposure / 枚举 / 范围）。
    policy: &ControlPolicy,
    // 显式接受的布局 box 越界（默认 false：拒绝提交）。
    allow_layout_overflow: bool,
    // 模型项 taskContract 是否已显式声明图片输出。
    declared_image_output: bool,
    // 模型项 taskContract.inputs：多参考图的角色声明。
    declared_input_roles: Option<&Value>,
    // 用户是否**显式**在 paramMap 同时映射了 image 与 image2。
    explicit_image_slots: bool,
    // 调用方基于**原始** paramMap 计算的锁定目标集合（不能在裁剪后的
    // effective map 上重算，否则会丢失重映射目标）。
    locked_paths: &std::collections::HashSet<String>,
    // 运行期任务上下文：只有在取得并发名额后才标记 running。
    runtime_args: Option<&Value>,
    // 是否由本次请求负责标记 running（只有首个请求）。
    mark_started: bool,
    // 摘要**读取**映射：保留被锁定语义的显式重映射目标。不能用上面已被
    // 裁剪的写入映射——锁定的 `seed` 会退回到自动推导路径，摘要因此读到
    // 另一个节点（实测 `4.seed=99` 被报成 `3.seed=11`）。
    read_param_map: &HashMap<String, Vec<String>>,
) -> napi::Result<(Vec<Value>, Vec<String>, Value)> {
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
    // 拿到并发名额后才标记 running：排队等名额期间任务仍是 pending，
    // 不能提前显示「引擎执行中」。
    if mark_started {
        if let Some(args) = runtime_args {
            runtime::request_started(args, None).await?;
        }
    }

    // 与 Agent 侧共用同一份契约：未知键 / 类型不符 / 结构化输入不是合法 JSON
    // 时，提交前直接失败，而不是让模型以为参数已经生效。
    let object_info = cached_object_info_text(client)
        .await
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    // 锁定语义：`paramMap` 空数组关闭的语义，以及 agentControls / 默认 exposure
    // 为 locked 的语义，其目标路径禁止被任何别名直写。只藏起语义名不够。
    let derived = crate::comfyui::convert::semantic_param_map(template);
    // 锁集合由调用方基于**原始映射**传进来：这里可能已经是裁剪后的
    // effective map，重算会丢失被锁语义的显式重映射目标。
    // R3：顶层参数同样受节点技术边界约束（seed 范围 / 尺寸对齐 / 文本类型）。
    // 与 `params` 共用 input_bounds —— 策略与技术边界必须同时满足。
    // R3：顶层参数与 `params` 必须走同一套策略 + 技术边界。
    // 显式值才应用 `agentControls` 的 min/max/options（随机 seed / 默认尺寸
    // 属于引擎隐式值，不能用策略范围去拒绝模板默认）。
    let top_level_writes: [(&str, Value, bool); 5] = [
        ("prompt", json!(prompt), true),
        ("seed", json!(seed), explicit_seed),
        ("width", json!(width), explicit_size),
        ("height", json!(height), explicit_size),
        (
            "negative",
            negative.map(Value::from).unwrap_or(Value::Null),
            negative.is_some(),
        ),
    ];
    validate_top_level_writes(
        &top_level_writes,
        policy,
        param_map,
        &derived,
        locked_paths,
        object_info.as_ref(),
        template,
    )
    .map_err(|message| Error::new(napi::Status::InvalidArg, message))?;

    // 所有 `params` 写入统一展开成目标路径：语义名与点号键共享同一套
    // 目标锁 / 映射 / 技术边界校验，避免「换个名字指向同一输入」绕过。
    if let Some(extra) = extra_params {
        for (key, raw_value) in extra {
            if workflow::RESERVED_EXTRA_KEYS.contains(&key.as_str()) {
                continue;
            }
            check_param_write(
                key,
                raw_value,
                param_map,
                &derived,
                &locked_paths,
                object_info.as_ref(),
                template,
                policy,
            )
            .map_err(|message| Error::new(napi::Status::InvalidArg, message))?;
        }
    }
    let request_contract = contract::derive_contract_with_policy(
        template,
        object_info.as_ref(),
        Some(param_map),
        Some(policy),
    );
    let param_problems =
        contract::validate_params(param_map, Some(&request_contract), extra_params);
    if !param_problems.is_empty() {
        return Err(generic_error(format!(
            "ComfyUI params rejected before submit: {}",
            param_problems.join(" | ")
        )));
    }

    // 提交前识别输出家族：只有确认是图片输出才允许走 imagegen 结果管线。
    // `taskContract` 只能为「完全无法识别」放行；已知视频/音频/混合输出绝不能
    // 因为声明 adapter=image 而绕过（结构化状态，不靠错误文本判断）。
    match workflow::classify_output_family_detail(template) {
        Ok(()) => {}
        Err(workflow::OutputFamilyIssue::NoTerminal) if declared_image_output => {
            // 用户明确声明这是图片适配器、但无法从节点名识别：允许，并在
            // history 结果阶段仍按 images[] 收集。
        }
        Err(issue) => {
            return Err(Error::new(napi::Status::InvalidArg, issue.message()));
        }
    }
    // 多参考图角色不明时拒绝：不能按节点 ID 顺序猜 base / mask / control。
    workflow::ensure_reference_roles(
        template,
        explicit_image_slots,
        request_images.len(),
        declared_input_roles,
    )
        .map_err(|message| Error::new(napi::Status::InvalidArg, message))?;
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
        &locked_paths,
    );
    // resolve 阶段的默认尺寸可能因为 width/height 映射被关闭/锁定而与
    // 最终图不同：用注入后的有效尺寸再查一次预算。
    if let Some((width, height)) = prepared.effective_size {
        workflow::ensure_pixel_budget(width, height)
            .map_err(|message| Error::new(napi::Status::InvalidArg, message))?;
    }
    let layout_overflow = prepared.layout_overflow.clone();
    let mut graph = prepared.graph;
    let mut injected = prepared.injected;
    let mut warnings = prepared.warnings;
    // 布局 box 越界默认拒绝：缩小画布后继续执行可能裁切/构图错误。
    if let Some(overflow) = layout_overflow.as_ref() {
        if !allow_layout_overflow {
            return Err(Error::new(
                napi::Status::InvalidArg,
                format!(
                    "{overflow}. Change `size`, fix the layout boxes, or pass `allowLayoutOverflow: true` to accept it."
                ),
            ));
        }
    }
    // 默认尺寸（非显式）写不进图时只告警；显式请求由下面的 ensure 直接拒绝。
    if !explicit_size && sync_size {
        for semantic in ["width", "height"] {
            let disabled = param_map
                .get(semantic)
                .map(|paths| paths.is_empty())
                .unwrap_or(true);
            if disabled {
                warnings.push(format!(
                    "default size {width}x{height} cannot be written (`{semantic}` has no target); \
                     the workflow keeps its template size and the layout may be out of sync"
                ));
            }
        }
    }
    // 尺寸解析通过不代表真的写进图：paramMap 关闭（`width: []`）、路径拼错或
    // 目标输入不存在时，注入会静默跳过并继续按模板尺寸执行。**显式**请求必须
    // 拒绝；默认尺寸写不进时只保留 prepare_graph 的告警（放大类工作流本来
    // 就没有 width/height 节点，不能因此被误杀）。
    if explicit_size {
        workflow::ensure_size_applied(param_map, &injected, width, height)
            .map_err(|message| Error::new(napi::Status::InvalidArg, message))?;
    }

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
                // 参考图是显式写入：目标被任何别名锁定时必须拒绝，不能靠
                // `set_value` 的静默跳过（那会留下模板旧图）。
                if locked_paths.contains(&workflow::normalize_path(path).to_ascii_lowercase()) {
                    return Err(generic_error(format!(
                        "reference image #{} targets `{path}`, which is locked for this workflow",
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

    // 摘要从**注入后的图**读回：seed / 尺寸可能是引擎隐式值，锁定语义
    // 保留模板值 —— 三者都只有最终图能如实反映。读取用原始映射（含被锁
    // 语义的重映射目标），不能用已裁剪的写入映射。
    let effective = effective_params(&graph, read_param_map, &derived);

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
    Ok((blocks, warnings, effective))
}

/// 本次请求实际提交的关键参数摘要。
///
/// 从**注入后的图**读回，而不是回显请求参数：`seed` 可能由引擎随机生成，
/// 尺寸可能被模板锁定，锁定语义会保留模板值 —— 这些只有最终图能反映。
/// `paramMap` 显式关闭（空数组）的语义回退到自动推导路径，读到的仍是
/// 模板里真正参与执行的旧值。
fn effective_params(
    graph: &Value,
    param_map: &HashMap<String, Vec<String>>,
    derived: &std::collections::BTreeMap<String, Vec<String>>,
) -> Value {
    let mut summary = serde_json::Map::new();
    for semantic in [
        "prompt", "negative", "seed", "width", "height", "steps", "cfg",
    ] {
        let paths: Option<&Vec<String>> = match param_map.get(semantic) {
            Some(paths) if !paths.is_empty() => Some(paths),
            _ => derived.get(semantic),
        };
        let Some(paths) = paths else { continue };
        let Some(value) = paths
            .first()
            .and_then(|path| workflow::read_path(graph, path))
        else {
            continue;
        };
        // seed 超出 JS Number 安全整数（2^53-1）时前端会丢位：用十进制字符串
        // 传递，前端同时兼容旧后端的数字形式。
        let serialized = match (semantic, value.as_u64()) {
            ("seed", Some(raw)) if raw > 9_007_199_254_740_991 => json!(raw.to_string()),
            _ => value.clone(),
        };
        summary.insert(semantic.to_string(), serialized);
    }
    Value::Object(summary)
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

/// paramMap 是否显式覆盖本次请求的**全部**图片槽位（image / image2 … imageN）。
///
/// 只映射其中一部分不能算消歧：第二张图仍会走自动节点顺序映射。
fn explicit_image_slots_from_overrides(
    overrides: &HashMap<String, Vec<String>>,
    image_count: usize,
) -> bool {
    image_count > 0
        && (0..image_count).all(|index| {
            let semantic = if index == 0 {
                "image".to_string()
            } else {
                format!("image{}", index + 1)
            };
            overrides
                .get(&semantic)
                .map(|paths| !paths.is_empty())
                .unwrap_or(false)
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// 大多数顶层校验用例不依赖推导映射；需要同目标交集时传入真实 derived。
    fn no_derived() -> BTreeMap<String, Vec<String>> {
        BTreeMap::new()
    }

    /// 生效参数摘要必须从**注入后的图**读回：显式覆盖写进去的值要反映，
    /// 模板保留的负向词也要如实出现——不能只回显请求参数（隐式 seed、
    /// 被锁定的尺寸在请求里根本不存在）。
    #[test]
    fn effective_params_reads_back_the_injected_graph() {
        // 标准 KSampler 结构：positive / negative 连线让自动推导能识别语义。
        let mut graph = json!({
            "3": { "class_type": "KSampler", "inputs": {
                "seed": 1, "steps": 20, "cfg": 8.0,
                "positive": ["6", 0], "negative": ["7", 0], "latent_image": ["5", 0]
            } },
            "5": { "class_type": "EmptyLatentImage", "inputs": { "width": 1024, "height": 1536 } },
            "6": { "class_type": "CLIPTextEncode", "inputs": { "text": "template positive" } },
            "7": { "class_type": "CLIPTextEncode", "inputs": { "text": "template negative" } }
        });
        // 模拟注入：prompt 被覆盖，seed / 尺寸 / 负向词保持模板值。
        graph["6"]["inputs"]["text"] = json!("a race queen");
        let mut map = HashMap::new();
        map.insert("prompt".to_string(), vec!["6.text".to_string()]);
        let derived = crate::comfyui::convert::semantic_param_map(&graph);
        let summary = effective_params(&graph, &map, &derived);
        // 显式映射：读回注入后的新值。
        assert_eq!(summary["prompt"], json!("a race queen"));
        // 自动推导回退：读回模板保留的负向词与采样参数。
        assert_eq!(summary["negative"], json!("template negative"));
        assert_eq!(summary["seed"], json!(1));
        assert_eq!(summary["width"], json!(1024));
        assert_eq!(summary["height"], json!(1536));
    }

    /// P2：锁定语义被移出写入映射后，摘要必须按**读取映射**取目标节点。
    /// 反例：原映射把 seed 重映射到 `4.seed`（锁定），写入映射删掉该语义后
    /// 自动推导指向 `3.seed`，摘要会报错节点（99 被报成 11）。
    #[test]
    fn effective_params_keeps_explicit_remap_for_locked_semantics() {
        let graph = json!({
            "3": { "class_type": "KSampler", "inputs": { "seed": 11 } },
            "4": { "class_type": "KSampler", "inputs": { "seed": 99 } }
        });
        let mut read_map = HashMap::new();
        read_map.insert("seed".to_string(), vec!["4.seed".to_string()]);
        let derived = crate::comfyui::convert::semantic_param_map(&graph);
        let summary = effective_params(&graph, &read_map, &derived);
        assert_eq!(summary["seed"], json!(99));
    }

    /// P3：超出 JS 安全整数的 seed 用十进制字符串返回，避免前端丢位。
    #[test]
    fn effective_params_stringifies_unsafe_seed() {
        let graph = json!({
            "3": { "class_type": "KSampler", "inputs": { "seed": 9_007_199_254_740_993u64 } }
        });
        let mut map = HashMap::new();
        map.insert("seed".to_string(), vec!["3.seed".to_string()]);
        let derived = crate::comfyui::convert::semantic_param_map(&graph);
        let summary = effective_params(&graph, &map, &derived);
        assert_eq!(summary["seed"], json!("9007199254740993"));
    }

    fn template() -> Value {
        json!({
            "3": { "class_type": "KSampler", "inputs": {
                "seed": 1, "steps": 20, "cfg": 8.0, "sampler_name": "euler",
                "positive": ["6", 0], "negative": ["7", 0], "latent_image": ["5", 0]
            } },
            "5": { "class_type": "EmptyLatentImage", "inputs": {
                "width": 512, "height": 512, "batch_size": 1
            } },
            "6": { "class_type": "CLIPTextEncode", "inputs": { "text": "old", "clip": ["1", 0] } },
            "7": { "class_type": "CLIPTextEncode", "inputs": { "text": "old-neg", "clip": ["1", 0] } },
            "9": { "class_type": "SaveImage", "inputs": {
                "filename_prefix": "snow", "images": ["8", 0]
            } }
        })
    }

    fn object_info() -> Value {
        json!({
            "KSampler": { "input": { "required": {
                "seed": ["INT", { "default": 0, "min": 0, "max": 18446744073709551615u64 }],
                "steps": ["INT", { "default": 20, "min": 1, "max": 10000 }],
                "cfg": ["FLOAT", { "default": 8.0, "min": 0.0, "max": 100.0 }],
                "sampler_name": [["euler", "dpmpp_2m"]]
            } } },
            "EmptyLatentImage": { "input": { "required": {
                "width": ["INT", { "default": 512, "min": 16, "max": 8192, "step": 8 }],
                "height": ["INT", { "default": 512, "min": 16, "max": 8192, "step": 8 }],
                "batch_size": ["INT", { "default": 1, "min": 1, "max": 64 }]
            } } },
            "CLIPTextEncode": { "input": { "required": { "text": ["STRING", {}] } } },
            "SaveImage": { "input": { "required": { "filename_prefix": ["STRING", {}] } } }
        })
    }

    fn derived() -> BTreeMap<String, Vec<String>> {
        BTreeMap::from([
            ("prompt".to_string(), vec!["6.text".to_string()]),
            ("negative".to_string(), vec!["7.text".to_string()]),
            ("seed".to_string(), vec!["3.seed".to_string()]),
            ("width".to_string(), vec!["5.width".to_string()]),
            ("height".to_string(), vec!["5.height".to_string()]),
            ("steps".to_string(), vec!["3.steps".to_string()]),
            ("cfg".to_string(), vec!["3.cfg".to_string()]),
        ])
    }

    fn param_map(overrides: &[(&str, Vec<String>)]) -> HashMap<String, Vec<String>> {
        let mut map: HashMap<String, Vec<String>> = derived().into_iter().collect();
        for (name, paths) in overrides {
            map.insert((*name).to_string(), paths.clone());
        }
        map
    }

    fn policy(value: Value) -> ControlPolicy {
        ControlPolicy::from_json(Some(&value)).expect("valid policy")
    }

    fn write_error(
        key: &str,
        value: Value,
        map: &HashMap<String, Vec<String>>,
        policy: &ControlPolicy,
    ) -> String {
        let derived = derived();
        let locked = locked_targets(policy, map, &derived);
        check_param_write(
            key,
            &value,
            map,
            &derived,
            &locked,
            Some(&object_info()),
            &template(),
            policy,
        )
        .expect_err("write must be rejected")
    }

    /// R2：未进入语义映射的 raw 输入（sampler_name / batch_size /
    /// filename_prefix）不能靠点号键直写。
    #[test]
    fn rejects_unknown_dotted_raw_targets() {
        let map = param_map(&[]);
        let policy = ControlPolicy::default();
        for (key, value) in [
            ("3.sampler_name", json!("euler")),
            ("5.batch_size", json!(2)),
            ("9.filename_prefix", json!("hack")),
        ] {
            let error = write_error(key, value, &map, &policy);
            assert!(error.contains("not mapped to a semantic input"), "{key}: {error}");
        }
    }

    /// R2：显式关闭 negative 后，另建 advanced 别名指向同一目标也必须拒绝。
    #[test]
    fn rejects_alias_writes_to_locked_target() {
        let map = param_map(&[
            ("negative", Vec::new()),
            ("myneg", vec!["7.text".to_string()]),
        ]);
        let policy = policy(json!({ "negative": { "exposure": "locked" } }));
        let error = write_error("myneg", json!("alias"), &map, &policy);
        assert!(error.contains("which is locked"), "{error}");
        // 点号键同样不能改写被锁目标（`7.text` 反查到 negative）。
        let error = write_error("7.inputs.text", json!("alias"), &map, &policy);
        assert!(error.contains("which is locked"), "{error}");
        // 显式关闭的语义名本身也不能写。
        let error = write_error("negative", json!("blurry"), &map, &policy);
        assert!(error.contains("disabled by paramMap"), "{error}");
    }

    /// R3：字符串 / 布尔不能写进数字语义，数字不能写进字符串语义。
    #[test]
    fn rejects_scalar_type_mismatches_from_review_examples() {
        let map = param_map(&[("sampler_name", vec!["3.sampler_name".to_string()])]);
        let steps_policy = policy(json!({ "steps": { "min": 1, "max": 50 } }));
        for value in [json!("99999"), json!(false)] {
            let error = write_error("steps", value.clone(), &map, &steps_policy);
            assert!(error.contains("must be an integer"), "{value}: {error}");
        }
        // 点号键走同一套策略校验。
        let error = write_error("3.steps", json!("99999"), &map, &steps_policy);
        assert!(error.contains("must be an integer"), "{error}");

        let sampler_policy = policy(json!({
            "sampler_name": { "exposure": "advanced", "options": ["euler", "dpmpp_2m"] }
        }));
        let error = write_error("sampler_name", json!(123), &map, &sampler_policy);
        assert!(error.contains("must be a string"), "{error}");
        let error = write_error("3.sampler_name", json!(123), &map, &sampler_policy);
        assert!(error.contains("must be a string"), "{error}");
    }

    /// R3：策略未声明边界时，节点 `/object_info` 的 min/max/step 仍然生效。
    #[test]
    fn applies_node_technical_bounds() {
        let map = param_map(&[("sampler_name", vec!["3.sampler_name".to_string()])]);
        let default_policy = ControlPolicy::default();
        let error = write_error("steps", json!(10001), &map, &default_policy);
        assert!(error.contains("must be <= 10000"), "{error}");
        let error = write_error("5.width", json!(1023), &map, &default_policy);
        assert!(error.contains("align to step 8"), "{error}");
        // 对齐的显式尺寸通过。
        let derived = derived();
        let locked = locked_targets(&default_policy, &map, &derived);
        assert!(check_param_write(
            "5.width",
            &json!(1024),
            &map,
            &derived,
            &locked,
            Some(&object_info()),
            &template(),
            &default_policy,
        )
        .is_ok());
    }

    /// 已映射语义与合法点号键正常放行（不能把拒绝做成无差别伤害）。
    #[test]
    fn accepts_mapped_semantic_and_known_dotted_targets() {
        let map = param_map(&[("sampler_name", vec!["3.sampler_name".to_string()])]);
        let policy = policy(json!({
            "steps": { "min": 1, "max": 50 },
            "sampler_name": { "exposure": "advanced" }
        }));
        let derived = derived();
        let locked = locked_targets(&policy, &map, &derived);
        for (key, value) in [
            ("prompt", json!("cat")),
            ("steps", json!(30)),
            ("3.steps", json!(30)),
            ("6.text", json!("dog")),
            ("3.sampler_name", json!("euler")),
        ] {
            assert!(
                check_param_write(
                    key,
                    &value,
                    &map,
                    &derived,
                    &locked,
                    Some(&object_info()),
                    &template(),
                    &policy,
                )
                .is_ok(),
                "{key} should be accepted"
            );
        }
        // 图上不存在、object_info 也没声明的可选输入仍然拒绝（拼写保护）。
        assert!(check_param_write(
            "20.strict",
            &json!(true),
            &map,
            &derived,
            &locked,
            Some(&object_info()),
            &template(),
            &policy,
        )
        .is_err());
    }

    /// P1：同一目标上的多个语义约束必须取交集，别名不能放宽原语义的 max。
    #[test]
    fn enforces_intersection_of_alias_constraints_on_same_target() {
        let map = param_map(&[("fast_steps", vec!["3.steps".to_string()])]);
        let policy = policy(json!({
            "steps": { "min": 1, "max": 30 },
            "fast_steps": { "exposure": "advanced", "min": 1, "max": 100 }
        }));
        // 别名自己的 max=100 允许 80，但同目标的 steps.max=30 必须生效。
        let error = write_error("fast_steps", json!(80), &map, &policy);
        assert!(error.contains("<= 30"), "{error}");
        // 点号键走同一目标集合，不能只查 find() 命中的那个语义。
        let error = write_error("3.steps", json!(80), &map, &policy);
        assert!(error.contains("<= 30"), "{error}");
        // 交集内的值仍然放行。
        let derived = derived();
        let locked = locked_targets(&policy, &map, &derived);
        assert!(check_param_write(
            "fast_steps",
            &json!(20),
            &map,
            &derived,
            &locked,
            Some(&object_info()),
            &template(),
            &policy,
        )
        .is_ok());
    }

    /// 顶层 `seed` 与 `params.seed` 共用同一套策略范围：换个入口不能绕过
    /// `agentControls.seed.max`。隐式 seed（随机/模板值）不受范围约束。
    #[test]
    fn rejects_top_level_seed_outside_agent_controls() {
        let policy = ControlPolicy::from_json(Some(&json!({ "seed": { "max": 10 } }))).unwrap();
        let mut map = HashMap::new();
        map.insert("seed".to_string(), vec!["3.seed".to_string()]);
        let locked = std::collections::HashSet::new();
        let graph = template();
        let writes = [("seed", json!(100), true)];
        let error = validate_top_level_writes(
            &writes,
            &policy,
            &map,
            &no_derived(),
            &locked,
            Some(&object_info()),
            &graph,
        )
        .expect_err("explicit seed above agentControls.max must be rejected");
        assert!(error.contains("seed"), "{error}");

        let writes = [("seed", json!(100), false)];
        assert!(
            validate_top_level_writes(
                &writes,
                &policy,
                &map,
                &no_derived(),
                &locked,
                Some(&object_info()),
                &graph
            )
            .is_ok(),
            "implicit seed must not be rejected by the policy range"
        );
    }

    /// 顶层尺寸同样受策略范围约束。
    #[test]
    fn rejects_top_level_size_outside_agent_controls() {
        let policy = ControlPolicy::from_json(Some(&json!({ "height": { "max": 512 } }))).unwrap();
        let mut map = HashMap::new();
        map.insert("width".to_string(), vec!["5.width".to_string()]);
        map.insert("height".to_string(), vec!["5.height".to_string()]);
        let locked = std::collections::HashSet::new();
        let graph = template();
        let writes = [("width", json!(512), true), ("height", json!(1024), true)];
        let error = validate_top_level_writes(
            &writes,
            &policy,
            &map,
            &no_derived(),
            &locked,
            Some(&object_info()),
            &graph,
        )
        .expect_err("height above agentControls.max must be rejected");
        assert!(error.contains("height"), "{error}");
    }

    /// 顶层写入的目标路径被锁定时同样拒绝（不能靠顶层参数直写锁定语义）。
    #[test]
    fn rejects_top_level_write_to_locked_target() {
        let policy = ControlPolicy::from_json(None).unwrap();
        let mut map = HashMap::new();
        map.insert("seed".to_string(), vec!["3.seed".to_string()]);
        let mut locked = std::collections::HashSet::new();
        locked.insert(workflow::normalize_path("3.seed").to_ascii_lowercase());
        let graph = template();
        let writes = [("seed", json!(7), true)];
        let error = validate_top_level_writes(
            &writes,
            &policy,
            &map,
            &no_derived(),
            &locked,
            Some(&object_info()),
            &graph,
        )
        .expect_err("locked target must be rejected");
        assert!(error.contains("locked"), "{error}");
    }

    /// P1：顶层 seed 也必须满足映射到同一目标的所有别名约束。
    #[test]
    fn rejects_top_level_seed_when_alias_on_same_target_is_stricter() {
        let policy = ControlPolicy::from_json(Some(&json!({
            "seed": { "max": 100 },
            "frozen_seed": { "exposure": "advanced", "max": 10 }
        })))
        .unwrap();
        let mut map = HashMap::new();
        map.insert("seed".to_string(), vec!["3.seed".to_string()]);
        map.insert("frozen_seed".to_string(), vec!["3.seed".to_string()]);
        let mut derived = BTreeMap::new();
        derived.insert("seed".to_string(), vec!["3.seed".to_string()]);
        let locked = std::collections::HashSet::new();
        let graph = template();
        let writes = [("seed", json!(50), true)];
        let error = validate_top_level_writes(
            &writes,
            &policy,
            &map,
            &derived,
            &locked,
            Some(&object_info()),
            &graph,
        )
        .expect_err("stricter alias on the same target must win");
        assert!(error.contains("<= 10"), "{error}");
    }

    /// P1：只有覆盖本次请求全部槽位的 paramMap 才算显式消歧。
    #[test]
    fn requires_full_slot_coverage_for_explicit_image_slots() {
        let mut overrides = HashMap::new();
        overrides.insert("image".to_string(), vec!["1.image".to_string()]);
        assert!(explicit_image_slots_from_overrides(&overrides, 1));
        // 两张图只映射一个槽位：不算消歧。
        assert!(!explicit_image_slots_from_overrides(&overrides, 2));
        overrides.insert("image2".to_string(), vec!["2.image".to_string()]);
        assert!(explicit_image_slots_from_overrides(&overrides, 2));
        // 空数组是显式关闭，不算覆盖。
        overrides.insert("image2".to_string(), Vec::new());
        assert!(!explicit_image_slots_from_overrides(&overrides, 2));
        assert!(!explicit_image_slots_from_overrides(&overrides, 0));
    }

    /// P1：锁集合由原始映射计算，显式重映射目标不能被丢失。
    #[test]
    fn locked_targets_keep_explicit_remaps() {
        let policy =
            ControlPolicy::from_json(Some(&json!({ "seed": { "exposure": "locked" } }))).unwrap();
        let map = param_map(&[("seed", vec!["9.seed".to_string()])]);
        let derived = derived();
        let locked = locked_targets(&policy, &map, &derived);
        assert!(locked.contains("9.inputs.seed"), "{locked:?}");
        assert!(!locked.contains("3.inputs.seed"), "{locked:?}");
    }
}
