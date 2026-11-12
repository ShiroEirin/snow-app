//! 内置 MCP 服务：ComfyUI 运维与理解（**随 app 编译，用户零安装**）。
//!
//! 与 `imagegen` 的分工（DRY）：
//! - 出图 / 改图 → `imagegen-generate`（渠道 provider = `comfyui`，带画廊、异步、并发、落盘）
//! - 队列 / 模型 / 节点 / 校验 / 历史 → 本服务的 `comfyui-*` 工具
//!
//! 配置来源只有一处：`imagegen_settings` 里 `provider = "comfyui"` 的渠道。
//! 没有任何此类渠道时，`tools()` 返回空数组 —— 不配置就一个 token 都不花。

use napi::bindgen_prelude::*;
use serde_json::{json, Map, Value};

use super::super::service::McpService;
use super::super::tools::McpTool;
use crate::comfyui::client::ComfyUiClient;

const SERVER_ID: &str = "comfyui";
const SETTING_CODE: &str = "imagegen_settings";
const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8188";

/// 渠道内一个模型项的工作流绑定（与执行路径的「模型项优先」一致）。
#[derive(Debug, Clone)]
struct ComfyModelEntry {
    id: String,
    model: String,
    workflow: String,
    param_map: String,
    /// 模型项的 Agent 控制策略（`agentControls` JSON 对象）。
    agent_controls: Option<Value>,
    /// 模型项的 taskContract（与执行路径一致；describe 需要它投影图片槽位）。
    task_contract: Option<Value>,
    enabled: bool,
}

/// 一条可用的 ComfyUI 端点（由 imagegen 渠道投影而来）。
#[derive(Debug, Clone)]
struct ComfyServer {
    id: String,
    name: String,
    base_url: String,
    workflow: String,
    param_map: String,
    /// 渠道主模型（与执行路径的有效默认模型一致）。
    primary_model: String,
    models: Vec<ComfyModelEntry>,
    /// 渠道是否启用。`all_servers` 保留禁用渠道用于只读反查，
    /// 运维与执行路径仍只看到启用的渠道（`servers`）。
    enabled: bool,
}

/// 显式 workflow 与模型绑定是否指向**同一张图**。
///
/// basename / 后缀不能当身份：不同目录可以有同名文件，模型名回退也只在
/// 没有显式绑定时成立。判定顺序：
/// 1. 字符串完全一致（画布工作流名 / 库文件名的常见场景）；
/// 2. 两者都解析到本地库文件时比较 canonical 路径；
/// 3. 显式请求能解析到本地文件、绑定却解析不到 → 不是同一张；
/// 4. 绑定为空时，执行路径用模型名当工作流名，只有完全相等才算匹配。
fn same_workflow_identity(requested: &str, bound: &str, fallback_name: &str) -> bool {
    let requested = requested.trim();
    if requested.is_empty() {
        // 未传 workflow：使用所选模型 / 渠道自己的绑定。
        return true;
    }
    let bound = bound.trim();
    if !bound.is_empty() && requested == bound {
        return true;
    }
    let resolved_requested = crate::comfyui::workflow::resolve_workflow_path(requested);
    if let (Some(left), Some(right)) = (
        resolved_requested.as_deref().and_then(canonical_path),
        crate::comfyui::workflow::resolve_workflow_path(bound).as_deref().and_then(canonical_path),
    ) {
        return left == right;
    }
    if bound.is_empty() {
        // 执行路径用模型名当工作流标识：显式同名（或解析到同一个本地文件）
        // 才算同一张图。只写 role / basename 的宽松比较会借用错的映射。
        let fallback = fallback_name.trim();
        if !fallback.is_empty() && requested.eq_ignore_ascii_case(fallback) {
            return true;
        }
        if let (Some(left), Some(right)) = (
            resolved_requested.as_deref().and_then(canonical_path),
            crate::comfyui::workflow::resolve_workflow_path(fallback)
                .as_deref()
                .and_then(canonical_path),
        ) {
            return left == right;
        }
        return false;
    }
    // 本地文件身份无法与一个解析不到的名字证明相同：不能回退到 basename 猜测。
    if resolved_requested.is_some() {
        return false;
    }
    false
}

fn canonical_path(path: &std::path::Path) -> Option<String> {
    path.canonicalize()
        .ok()
        .map(|path| path.to_string_lossy().to_string())
}

pub struct ComfyUiService;

impl ComfyUiService {
    pub fn new() -> Self {
        Self
    }

    /// 运维与执行路径看到的端点：只包含启用的渠道。
    fn servers(&self) -> Result<Vec<ComfyServer>> {
        Ok(self
            .all_servers()?
            .into_iter()
            .filter(|server| server.enabled)
            .collect())
    }

    /// 全部 ComfyUI 渠道（含禁用渠道）。
    ///
    /// 只读反查（例如工作流库的 `boundBy`）需要看到所有显式绑定：
    /// 渠道暂时禁用不等于配置不存在，漏报会让 Agent 误以为某个工作流没人用。
    fn all_servers(&self) -> Result<Vec<ComfyServer>> {
        let info = crate::storage::initialize_app_storage()?;
        let database_path = std::path::PathBuf::from(info.database_path);
        let raw = crate::storage::services::system_settings::get_system_setting_value(
            &database_path,
            SETTING_CODE,
        )?;
        let Some(raw) = raw else {
            return Ok(Vec::new());
        };
        let Ok(settings) = serde_json::from_str::<Value>(&raw) else {
            return Ok(Vec::new());
        };

        // 同一 8188 可以被多个渠道引用，每个渠道有自己的模型/工作流绑定；
        // 运维工具默认取第一个，describe 则必须保留全部渠道，不能按端点去重。
        let mut servers: Vec<ComfyServer> = Vec::new();
        let channels = settings
            .get("channels")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for channel in channels {
            let provider = channel
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !provider.eq_ignore_ascii_case("comfyui") {
                continue;
            }
            let enabled = channel
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let base_url = channel
                .get("baseUrl")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_string();
            let id = channel
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let name = channel
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let models = channel
                .get("models")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            let model = item
                                .get("model")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .trim();
                            if model.is_empty() {
                                return None;
                            }
                            Some(ComfyModelEntry {
                                id: item
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .trim()
                                    .to_string(),
                                model: model.to_string(),
                                workflow: item
                                    .get("workflow")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .trim()
                                    .to_string(),
                                param_map: item
                                    .get("paramMap")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .trim()
                                    .to_string(),
                                agent_controls: item
                                    .get("agentControls")
                                    .filter(|value| value.is_object())
                                    .cloned(),
                                task_contract: item
                                    .get("taskContract")
                                    .filter(|value| value.is_object())
                                    .cloned(),
                                enabled: item
                                    .get("enabled")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(true),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            servers.push(ComfyServer {
                id: if id.is_empty() {
                    base_url.clone()
                } else {
                    id
                },
                name,
                base_url,
                workflow: channel
                    .get("workflow")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                param_map: channel
                    .get("paramMap")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                primary_model: channel
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                models,
                enabled,
            });
        }
        Ok(servers)
    }

    /// 解析目标端点：显式 `server`（id / 名称 / baseUrl）→ 第一个可用端点。
    fn resolve<'a>(&self, servers: &'a [ComfyServer], args: &Value) -> Result<&'a ComfyServer> {
        let requested = args
            .get("server")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(key) = requested {
            if let Some(found) = servers.iter().find(|server| {
                server.id.eq_ignore_ascii_case(key)
                    || server.name.eq_ignore_ascii_case(key)
                    || server.base_url.eq_ignore_ascii_case(key)
            }) {
                return Ok(found);
            }
            return Err(Error::from_reason(format!(
                "Unknown ComfyUI server \"{key}\". Configured: [{}]",
                servers
                    .iter()
                    .map(|server| format!("{} ({})", server.id, server.base_url))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        servers
            .first()
            .ok_or_else(|| Error::from_reason("No ComfyUI server configured"))
    }

    pub async fn execute_async(&self, tool_name: &str, args: &Value) -> Result<Value> {
        let servers = self.servers()?;
        if servers.is_empty() {
            return Err(Error::from_reason(
                "No ComfyUI channel configured. Add one in Settings -> Image generation (template: \"本地 - ComfyUI（原生直连）\").",
            ));
        }
        let server = self.resolve(&servers, args)?;
        let client = ComfyUiClient::new(&server.base_url, Some(120)).await?;

        match tool_name {
            "status" => {
                let stats = client.system_stats().await?;
                let queue = client.queue().await.unwrap_or_else(|_| json!({}));
                let channel_workflow = server.workflow.trim().to_string();
                let models_with_workflow = count_models_with_workflow(server);
                let has_explicit = has_explicit_workflow(server);
                // Workflow Readiness：遍历**所有**启用模型的有效绑定 + 渠道绑定，
                // 只做只读诊断（不自动安装）。解析失败必须报错，不能伪装成
                //「无缺失」（旧实现把 Err 当空数组 → ok:true 假阳性）。
                let readiness = match client.object_info_text().await {
                    Ok(info_text) => match serde_json::from_str::<Value>(&info_text) {
                        Ok(info) => {
                            let declared_output = |entry: &ComfyModelEntry| -> bool {
                                entry
                                    .task_contract
                                    .as_ref()
                                    .and_then(|contract| contract.get("outputs"))
                                    .and_then(|outputs| outputs.get("adapter"))
                                    .and_then(Value::as_str)
                                    .map(|adapter| adapter.eq_ignore_ascii_case("image"))
                                    .unwrap_or(false)
                            };
                            // 候选带上「该模型是否声明了图片输出」：NoTerminal
                            // 只有在没有声明时才判为不兼容。
                            let mut candidates: Vec<(String, bool)> = Vec::new();
                            if !channel_workflow.is_empty() {
                                let declared = server
                                    .models
                                    .iter()
                                    .filter(|entry| entry.enabled)
                                    .any(|entry| declared_output(entry));
                                candidates.push((channel_workflow.clone(), declared));
                            }
                            for entry in server.models.iter().filter(|entry| entry.enabled) {
                                let binding = if !entry.workflow.trim().is_empty() {
                                    entry.workflow.trim()
                                } else if !channel_workflow.is_empty() {
                                    // 执行路径：模型项没有自己的 workflow 时用渠道级，
                                    // 该候选已收录。
                                    continue;
                                } else {
                                    // 模型即工作流：模型名就是解析标识。
                                    entry.model.trim()
                                };
                                if !binding.is_empty()
                                    && !candidates
                                        .iter()
                                        .any(|(item, _)| item.as_str() == binding)
                                {
                                    candidates
                                        .push((binding.to_string(), declared_output(entry)));
                                }
                            }
                            let mut missing: Vec<String> = Vec::new();
                            let mut missing_files: Vec<Value> = Vec::new();
                            let mut schema_issues: Vec<Value> = Vec::new();
                            let mut structure_issues: Vec<Value> = Vec::new();
                            let mut output_issues: Vec<Value> = Vec::new();
                            let mut warnings: Vec<Value> = Vec::new();
                            let mut errors: Vec<Value> = Vec::new();
                            let mut checked = 0usize;
                            for (raw, declared_image_output) in &candidates {
                                match crate::comfyui::resolve_workflow_graph(
                                    &client,
                                    raw,
                                    Some(&info_text),
                                )
                                .await
                                {
                                    Ok((graph, _)) => {
                                        checked += 1;
                                        missing.extend(
                                            crate::comfyui::workflow::missing_node_classes(
                                                &graph, &info,
                                            ),
                                        );
                                        // 节点类存在 ≠ 图可执行：坏连线 / 缺必填输入
                                        // 同样会让 readiness 假阳性。
                                        for problem in
                                            crate::comfyui::validate::validate_graph(&graph, &info)
                                        {
                                            structure_issues.push(json!({
                                                "workflow": raw,
                                                "code": problem.code,
                                                "node": problem.node,
                                                "message": problem.message,
                                            }));
                                        }
                                        // 模型/LoRA/VAE 等资源文件是否真的装在本机。
                                        for resource in
                                            crate::comfyui::workflow::missing_model_files(
                                                &graph, &info,
                                            )
                                        {
                                            missing_files.push(json!({
                                                "workflow": raw,
                                                "resource": resource,
                                            }));
                                        }
                                        // 结构化输入（layout / character_boxes）与契约是否兼容。
                                        let contract = crate::comfyui::contract::derive_contract(
                                            &graph,
                                            Some(&info),
                                            None,
                                        );
                                        for problem in
                                            crate::comfyui::contract::current_structural_problems(
                                                &graph, &contract,
                                            )
                                        {
                                            schema_issues.push(json!({
                                                "workflow": raw,
                                                "problem": problem,
                                            }));
                                        }
                                        // 已知视频/音频/混合输出在运行时会被拒绝，readiness 必须
                                        // 提前暴露。NoTerminal 不再静默吞掉：它可能由
                                        // taskContract 声明放行，因此报告为 warning 而不是判死。
                                        if let Err(issue) =
                                            crate::comfyui::workflow::classify_output_family_detail(
                                                &graph,
                                            )
                                        {
                                            if matches!(
                                                &issue,
                                                crate::comfyui::workflow::OutputFamilyIssue::NoTerminal
                                            ) {
                                                // 只有模型声明 adapter=image 时才允许
                                                // 无终止节点；否则执行会拒绝，readiness
                                                // 不能报 ok。
                                                if *declared_image_output {
                                                    warnings.push(json!({
                                                        "workflow": raw,
                                                        "warning": issue.message(),
                                                    }));
                                                } else {
                                                    output_issues.push(json!({
                                                        "workflow": raw,
                                                        "issue": issue.message(),
                                                    }));
                                                }
                                            } else {
                                                output_issues.push(json!({
                                                    "workflow": raw,
                                                    "issue": issue.message(),
                                                }));
                                            }
                                        }
                                    }
                                    Err(error) => errors.push(json!({
                                        "workflow": raw,
                                        "error": error.to_string(),
                                    })),
                                }
                            }
                            missing.sort();
                            missing.dedup();
                            let ok = if candidates.is_empty() {
                                Value::Null
                            } else {
                                json!(
                                    errors.is_empty()
                                        && missing.is_empty()
                                        && missing_files.is_empty()
                                        && schema_issues.is_empty()
                                        && structure_issues.is_empty()
                                        && output_issues.is_empty()
                                )
                            };
                            json!({
                                "ok": ok,
                                "checkedWorkflows": checked,
                                "boundWorkflows": candidates
                                    .iter()
                                    .map(|(workflow, _)| workflow.clone())
                                    .collect::<Vec<_>>(),
                                "missingNodeClasses": missing,
                                "missingModelFiles": missing_files,
                                "schemaIssues": schema_issues,
                                "structureIssues": structure_issues,
                                "outputFamilyIssues": output_issues,
                                "warnings": warnings,
                                "errors": errors,
                                "note": if candidates.is_empty() {
                                    json!("no explicit workflow binding to check")
                                } else {
                                    Value::Null
                                },
                            })
                        }
                        Err(_) => json!({
                            "ok": Value::Null,
                            "missingNodeClasses": [],
                            "errors": [],
                            "error": "object_info is not valid JSON"
                        }),
                    },
                    Err(_) => json!({
                        "ok": Value::Null,
                        "missingNodeClasses": [],
                        "errors": [],
                        "error": "object_info unavailable"
                    }),
                };
                Ok(json!({
                    "server": { "id": server.id, "name": server.name, "baseUrl": server.base_url },
                    "comfyuiVersion": stats
                        .get("system")
                        .and_then(|system| system.get("comfyui_version"))
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    "devices": stats.get("devices").cloned().unwrap_or_else(|| json!([])),
                    "queueRunning": count_array(queue.get("queue_running")),
                    "queuePending": count_array(queue.get("queue_pending")),
                    // 区分数据来源：渠道级或任一启用模型级的显式绑定都算「有工作流」。
                    "channelWorkflow": channel_workflow,
                    "modelsWithWorkflow": models_with_workflow,
                    "hasExplicitWorkflow": has_explicit,
                    // 兼容旧调用方：语义等同于 hasExplicitWorkflow
                    // （渠道级或任一启用模型级存在显式绑定），不再只检查渠道级。
                    "workflowConfigured": has_explicit,
                    "models": model_workflow_summaries(server),
                    "readiness": readiness,
                }))
            }
            "models" => {
                let object_info = client.object_info_text().await?;
                let info: Value = serde_json::from_str(&object_info)
                    .map_err(|error| Error::from_reason(format!("Invalid /object_info: {error}")))?;
                Ok(json!({
                    "server": server.id,
                    "checkpoints": combo_values(&info, "CheckpointLoaderSimple", "ckpt_name"),
                    "loras": combo_values(&info, "LoraLoader", "lora_name"),
                    "vaes": combo_values(&info, "VAELoader", "vae_name"),
                    "controlnets": combo_values(&info, "ControlNetLoader", "control_net_name"),
                    "upscaleModels": combo_values(&info, "UpscaleModelLoader", "model_name"),
                }))
            }
            "nodes" => {
                let query = args
                    .get("query")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(40)
                    .clamp(1, 200) as usize;
                let object_info = client.object_info_text().await?;
                let info: Value = serde_json::from_str(&object_info)
                    .map_err(|error| Error::from_reason(format!("Invalid /object_info: {error}")))?;
                let mut matches: Vec<Value> = Vec::new();
                if let Some(classes) = info.as_object() {
                    for (class, def) in classes {
                        let display = def
                            .get("display_name")
                            .and_then(Value::as_str)
                            .unwrap_or(class);
                        let category = def.get("category").and_then(Value::as_str).unwrap_or("");
                        let hit = query.is_empty()
                            || class.to_ascii_lowercase().contains(&query)
                            || display.to_ascii_lowercase().contains(&query);
                        if !hit {
                            continue;
                        }
                        matches.push(json!({
                            "class": class,
                            "displayName": display,
                            "category": category,
                            "outputNode": def.get("output_node").and_then(Value::as_bool).unwrap_or(false),
                        }));
                        if matches.len() >= limit {
                            break;
                        }
                    }
                }
                Ok(json!({ "server": server.id, "totalReturned": matches.len(), "nodes": matches }))
            }
            "workflows" => {
                // 画布列表失败不能伪装成空列表：必须把原因交给 Agent/用户，
                // 同时保留已配置渠道信息（本地库仍然可用）。
                let (canvas_workflows, canvas_error) = match client.list_workflows().await {
                    Ok(names) => (names, Value::Null),
                    Err(error) => (Vec::new(), json!(error.to_string())),
                };
                // 本地库是独立来源：读取失败同样要报原因，不能让「画布为空」
                // 被误读成「本地 library 没有文件」。`boundBy` 反查全部渠道
                // （含禁用渠道与模型级绑定），因为那是显式配置事实。
                let (library_workflows, library_error) =
                    match library_workflow_items(&self.all_servers()?) {
                        Ok(items) => (items, Value::Null),
                        Err(error) => (Vec::new(), json!(error.to_string())),
                    };
                let configured: Vec<Value> = servers
                    .iter()
                    .map(|item| {
                        json!({
                            "id": item.id,
                            "name": item.name,
                            "baseUrl": item.base_url,
                            "hasWorkflow": !item.workflow.trim().is_empty(),
                            "hasParamMap": !item.param_map.trim().is_empty(),
                        })
                    })
                    .collect();
                Ok(json!({
                    "server": server.id,
                    "canvasWorkflows": canvas_workflows.clone(),
                    "canvasError": canvas_error.clone(),
                    "libraryWorkflows": library_workflows,
                    "libraryError": library_error,
                    "configuredChannels": configured,
                    // 兼容旧字段：`comfyuiSavedWorkflows` / `listError` 只描述画布来源。
                    "comfyuiSavedWorkflows": canvas_workflows,
                    "listError": canvas_error,
                    "note": "canvasWorkflows are ComfyUI canvas (UI) workflows from /userdata; libraryWorkflows are Snow's local API-format workflow library (boundBy lists the channel ids whose channel-level or model-level workflow points at that file). To run a canvas workflow through imagegen-generate, pull it once in Settings -> Image generation -> 本地 ComfyUI（原生直连）; it is converted into the local library automatically.",
                }))
            }
            "describe" => {
                // 与执行路径共用同一套解析顺序：显式 workflow > 模型项 > 渠道级。
                // 否则用户只在模型项配了工作流时，describe 会误报「没有工作流」。
                let requested_model = args
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                // 与执行路径一致：省略 model 时使用渠道主模型 / 第一个启用的
                // 模型项，而不是直接回退到渠道级空 workflow。
                let selected_model = match requested_model {
                    Some(model) if server.models.is_empty() => {
                        // 单模型兼容配置：只有渠道顶层 model/workflow。
                        // 执行会合成模型项，describe 不能把它当「未知模型」。
                        if server.primary_model.is_empty()
                            || server.primary_model.eq_ignore_ascii_case(model)
                        {
                            None
                        } else {
                            return Err(Error::from_reason(format!(
                                "Unknown model \"{model}\" in ComfyUI channel \"{}\"; this channel only configures \"{}\"",
                                server.id, server.primary_model
                            )));
                        }
                    }
                    Some(model) => {
                        // 与执行侧一致：先过滤启用项，再按 model 精确匹配，
                        // 最后才按 id 匹配。禁用模型不能靠 id 混进来。
                        let enabled = || server.models.iter().filter(|entry| entry.enabled);
                        let entry = enabled()
                            .find(|entry| entry.model.eq_ignore_ascii_case(model))
                            .or_else(|| {
                                enabled().find(|entry| entry.id.eq_ignore_ascii_case(model))
                            })
                            .ok_or_else(|| {
                                Error::from_reason(format!(
                                    "Unknown model \"{model}\" in ComfyUI channel \"{}\". Available: {}",
                                    server.id,
                                    enabled()
                                        .map(|entry| entry.model.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ))
                            })?;
                        Some(entry)
                    }
                    None => {
                        let primary = if server.primary_model.is_empty() {
                            server
                                .models
                                .iter()
                                .find(|entry| entry.enabled)
                                .map(|entry| entry.model.clone())
                                .unwrap_or_default()
                        } else {
                            server.primary_model.clone()
                        };
                        server
                            .models
                            .iter()
                            .find(|entry| {
                                entry.enabled
                                    && entry.model.eq_ignore_ascii_case(&primary)
                            })
                    }
                };
                let explicit_workflow = args
                    .get("workflow")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                let workflow = explicit_workflow
                    .clone()
                    .or_else(|| {
                        selected_model
                            .map(|entry| entry.workflow.clone())
                            .filter(|value| !value.is_empty())
                    })
                    .or_else(|| {
                        let channel_workflow = server.workflow.trim();
                        if channel_workflow.is_empty() {
                            None
                        } else {
                            Some(channel_workflow.to_string())
                        }
                    })
                    // 执行支持「模型名即工作流名」：describe 必须同样回退。
                    .or_else(|| {
                        selected_model
                            .map(|entry| entry.model.clone())
                            .filter(|value| !value.is_empty())
                    })
                    .or_else(|| requested_model.map(str::to_string))
                    .or_else(|| {
                        if server.primary_model.is_empty() {
                            None
                        } else {
                            Some(server.primary_model.clone())
                        }
                    })
                    .unwrap_or_default();
                if workflow.trim().is_empty() {
                    return Err(Error::from_reason(
                        "No workflow to describe: pass `workflow` / `model`, or configure one on the channel.",
                    ));
                }
                // 与执行路径共用同一个解析入口：内联 / 本地库 / 画布工作流
                // 的优先级和后缀匹配完全一致。
                let object_info_text = client.object_info_text().await.ok();
                let (graph, workflow_source) = crate::comfyui::resolve_workflow_graph(
                    &client,
                    &workflow,
                    object_info_text.as_deref(),
                )
                .await?;
                if !graph.is_object() {
                    return Err(Error::from_reason(
                        "Workflow is not API format (expected a JSON object of node id -> node)",
                    ));
                }
                // 显式 paramMap 必须覆盖自动推导：Agent 查到的落点要与执行一致。
                // 显式 workflow 覆盖规则：与所选模型绑定的工作流不一致时，
                // 不得借用该模型的 paramMap（否则 Agent 查到的落点属于另一张图）。
                let requested_workflow = explicit_workflow.as_deref().unwrap_or("");
                let binding_matches = match selected_model {
                    // 单模型渠道没有 models[]：执行路径的绑定是「渠道 workflow，
                    // 空则 primary_model」。
                    None => {
                        let bound = server.workflow.trim();
                        same_workflow_identity(requested_workflow, bound, &server.primary_model)
                    }
                    Some(entry) => {
                        // 与执行路径一致的有效绑定：模型项 workflow > 渠道 workflow
                        // > 模型名（模型即工作流）。
                        let bound = if !entry.workflow.trim().is_empty() {
                            entry.workflow.trim()
                        } else if !server.workflow.trim().is_empty() {
                            server.workflow.trim()
                        } else {
                            ""
                        };
                        same_workflow_identity(requested_workflow, bound, &entry.model)
                    }
                };
                let param_map_raw = if explicit_workflow.is_some() && !binding_matches {
                    String::new()
                } else {
                    selected_model
                        .map(|entry| entry.param_map.clone())
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| server.param_map.clone())
                };
                let mut overrides = if param_map_raw.trim().is_empty() {
                    None
                } else {
                    Some(crate::comfyui::workflow::parse_param_map(
                        &param_map_raw,
                    )?)
                };
                // 与执行一致：taskContract.inputs 的有效 nodePath 覆盖自动映射，
                // 否则 describe 展示的图片槽位与真实注入不一致。
                if binding_matches {
                    if let Some(contract) = selected_model
                        .and_then(|entry| entry.task_contract.as_ref())
                    {
                        let declared = crate::comfyui::workflow::declared_image_slot_map(
                            contract.get("inputs"),
                            &graph,
                        )
                        .map_err(Error::from_reason)?;
                        if !declared.is_empty() {
                            let map = overrides
                                .get_or_insert_with(std::collections::HashMap::new);
                            for (semantic, paths) in declared {
                                map.insert(semantic, paths);
                            }
                        }
                    }
                }
                // object_info 不可用时仍返回输入清单；契约里会标注缺文档。
                let object_info = object_info_text
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
                // describe 与执行共用同一份控制策略：锁定的语义在这里就展示为
                // locked / exposed=false，避免 Agent 先试错再被拒绝。
                let policy = if binding_matches {
                    crate::comfyui::policy::ControlPolicy::from_json(
                        selected_model.and_then(|entry| entry.agent_controls.as_ref()),
                    )
                    .map_err(Error::from_reason)?
                } else {
                    // 显式 workflow 未绑定到该模型时，不借用它的 agentControls。
                    crate::comfyui::policy::ControlPolicy::default()
                };
                let contract = crate::comfyui::contract::derive_contract_with_policy(
                    &graph,
                    object_info.as_ref(),
                    overrides.as_ref(),
                    Some(&policy),
                );
                Ok(json!({
                    "server": server.id,
                    "model": selected_model.map(|entry| entry.model.clone()),
                    "workflow": workflow,
                    "source": workflow_source,
                    "contract": contract,
                }))
            }
            "validate" => {
                let graph_text = args
                    .get("workflow")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| server.workflow.clone());
                if graph_text.trim().is_empty() {
                    return Err(Error::from_reason(
                        "No workflow to validate: pass `workflow` (API-format JSON) or configure one on the channel.",
                    ));
                }
                // 与执行路径共用同一个解析入口：逻辑名（工作流库文件名）
                // 不能再被当成相对 cwd 的 JSON 文本。
                let graph_text = crate::comfyui::workflow::read_workflow_text(&graph_text)?;
                let graph: Value = serde_json::from_str(&graph_text).map_err(|error| {
                    Error::from_reason(format!("Invalid workflow JSON: {error}"))
                })?;
                let object_info = client.object_info_text().await?;
                let info: Value = serde_json::from_str(&object_info)
                    .map_err(|error| Error::from_reason(format!("Invalid /object_info: {error}")))?;
                Ok(validate_graph(&graph, &info, &server.id))
            }
            "queue" => {
                let action = args
                    .get("action")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("list");
                match action {
                    "list" => {
                        let queue = client.queue().await?;
                        Ok(json!({
                            "server": server.id,
                            "running": summarize_queue(queue.get("queue_running")),
                            "pending": summarize_queue(queue.get("queue_pending")),
                        }))
                    }
                    "cancel" => {
                        let prompt_id = args
                            .get("promptId")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .ok_or_else(|| {
                                Error::from_reason("`promptId` is required for action=cancel")
                            })?;
                        client.queue_delete(&[prompt_id.to_string()]).await?;
                        Ok(json!({ "server": server.id, "cancelled": prompt_id }))
                    }
                    "interrupt" => {
                        client.interrupt().await?;
                        Ok(json!({
                            "server": server.id,
                            "interrupted": true,
                            "note": "ComfyUI interrupt is global: it stops the currently running prompt, not a single queued one.",
                        }))
                    }
                    "clear" => {
                        let queue = client.queue().await?;
                        let ids = queue_prompt_ids(queue.get("queue_pending"));
                        if ids.is_empty() {
                            return Ok(json!({ "server": server.id, "removed": 0 }));
                        }
                        client.queue_delete(&ids).await?;
                        Ok(json!({ "server": server.id, "removed": ids.len() }))
                    }
                    other => Err(Error::from_reason(format!(
                        "Unknown action \"{other}\"; use list / cancel / interrupt / clear"
                    ))),
                }
            }
            "history" => {
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(10)
                    .clamp(1, 50) as usize;
                let history = client.history_all().await?;
                let mut entries: Vec<(String, Value)> = history
                    .as_object()
                    .map(|map| {
                        map.iter()
                            .map(|(id, entry)| (id.clone(), entry.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                let total = entries.len();
                if entries.len() > limit {
                    entries = entries.split_off(entries.len() - limit);
                }
                let runs: Vec<Value> = entries
                    .into_iter()
                    .map(|(prompt_id, entry)| {
                        let status = entry
                            .get("status")
                            .and_then(|status| status.get("status_str"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let images = entry
                            .get("outputs")
                            .map(|outputs| {
                                crate::comfyui::workflow::collect_output_images(Some(outputs))
                                    .0
                                    .len()
                            })
                            .unwrap_or(0);
                        json!({ "promptId": prompt_id, "status": status, "outputs": images })
                    })
                    .collect();
                Ok(json!({ "server": server.id, "total": total, "runs": runs }))
            }
            _ => Err(Error::from_reason(format!(
                "Unknown tool: \"{tool_name}\" for MCP server \"{SERVER_ID}\". Available: [comfyui-status, comfyui-models, comfyui-nodes, comfyui-workflows, comfyui-validate, comfyui-queue, comfyui-history]"
            ))),
        }
    }
}

impl Default for ComfyUiService {
    fn default() -> Self {
        Self::new()
    }
}

impl McpService for ComfyUiService {
    fn id(&self) -> &str {
        SERVER_ID
    }

    /// 未配置任何 ComfyUI 渠道时返回空数组：工具不进入模型上下文（不花 token）。
    fn tools(&self) -> Vec<McpTool> {
        let Ok(servers) = self.servers() else {
            return Vec::new();
        };
        if servers.is_empty() {
            return Vec::new();
        }
        let server_hint = format!(
            "Configured servers: {}",
            servers
                .iter()
                .map(|server| format!("{} ({})", server.id, server.base_url))
                .collect::<Vec<_>>()
                .join(", ")
        );
        vec![
            tool(
                "status",
                &format!("ComfyUI connection status: version, devices, queue depth, and which workflow bindings are explicit (channelWorkflow / modelsWithWorkflow / hasExplicitWorkflow / models[]). workflowConfigured is a compatibility alias of hasExplicitWorkflow. {server_hint}"),
                json!({ "type": "object", "properties": { "server": { "type": "string", "description": "Configured server id / name / baseUrl (optional; defaults to the first one)" } } }),
            ),
            tool(
                "models",
                "List locally installed ComfyUI models (checkpoints / LoRAs / VAEs / ControlNets / upscale models) by reading /object_info combo values. Use this before swapping a model in a workflow.",
                json!({ "type": "object", "properties": { "server": { "type": "string" } } }),
            ),
            tool(
                "nodes",
                "Search installed ComfyUI node classes (by class name or display name). Use it to diagnose 'missing custom node' errors.",
                json!({ "type": "object", "properties": {
                    "query": { "type": "string", "description": "Case-insensitive substring, e.g. \"lora\" or \"upscale\". Empty = first N nodes." },
                    "limit": { "type": "number", "description": "Max results, 1-200 (default 40)" },
                    "server": { "type": "string" }
                } }),
            ),
            tool(
                "workflows",
                "List the two distinct workflow sources: canvasWorkflows (ComfyUI /userdata canvas workflows, UI format) and libraryWorkflows (Snow's local API-format workflow library, each with boundBy channel ids). canvasError / libraryError explain a failed read instead of reporting an empty list. Canvas workflows must be pulled once in Settings -> Image generation to be converted into the local library.",
                json!({ "type": "object", "properties": { "server": { "type": "string" } } }),
            ),
            tool(
                "describe",
                "Return the full input contract of a workflow: semantic inputs, target node keys, types, JSON schemas, examples and confidence. Call this before passing complex `params` (layout / character_boxes / nested JSON) to imagegen-generate. Pass `workflow` (logical library name / API JSON) or omit to describe the channel's configured workflow.",
                json!({ "type": "object", "properties": {
                    "workflow": { "type": "string", "description": "Logical workflow name from the library, or API-format JSON (optional; defaults to the selected model / channel workflow)" },
                    "model": { "type": "string", "description": "Model item ID inside the channel (each model can bind its own workflow and paramMap); omit for the primary model" },
                    "server": { "type": "string" }
                } }),
            ),
            tool(
                "validate",
                "Validate an API-format workflow against this ComfyUI without executing it: missing node classes, missing required link inputs, links pointing at unknown nodes. Pass `workflow` (JSON text) or validate the channel's configured workflow.",
                json!({ "type": "object", "properties": {
                    "workflow": { "type": "string", "description": "API-format workflow JSON text (optional)" },
                    "server": { "type": "string" }
                } }),
            ),
            tool(
                "queue",
                "Inspect or control the ComfyUI queue. action=list (default) shows running/pending; cancel removes one queued prompt by `promptId`; interrupt stops the currently running prompt (global); clear removes all pending prompts.",
                json!({ "type": "object", "properties": {
                    "action": { "type": "string", "enum": ["list", "cancel", "interrupt", "clear"] },
                    "promptId": { "type": "string", "description": "Required for action=cancel" },
                    "server": { "type": "string" }
                } }),
            ),
            tool(
                "history",
                "Recent ComfyUI runs from the engine's /history (flat projection: promptId / status / output count). The engine keeps only recent runs and clears them on restart.",
                json!({ "type": "object", "properties": {
                    "limit": { "type": "number", "description": "Max runs, 1-50 (default 10)" },
                    "server": { "type": "string" }
                } }),
            ),
        ]
    }

    /// 异步工具：所有实现都在 `execute_async` 里；同步路径只允许显式错误。
    fn execute(&self, tool_name: &str, _args: &Value) -> Result<Value> {
        Err(Error::from_reason(format!(
            "comfyui-{tool_name} is async-only; call it through the tool dispatcher"
        )))
    }
}

fn tool(name: &str, description: &str, input_schema: Value) -> McpTool {
    McpTool {
        server_id: SERVER_ID.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        input_schema,
    }
}

/// 启用且显式绑定 workflow 的模型项数量。
///
/// 只计入 `enabled`：禁用模型的工作流不会进入执行路径，不能算「可用」。
fn count_models_with_workflow(server: &ComfyServer) -> usize {
    server
        .models
        .iter()
        .filter(|entry| entry.enabled && !entry.workflow.trim().is_empty())
        .count()
}

/// 渠道级或任一启用模型级是否存在显式 workflow 绑定。
fn has_explicit_workflow(server: &ComfyServer) -> bool {
    !server.workflow.trim().is_empty() || count_models_with_workflow(server) > 0
}

/// 模型项摘要：`workflow` 空字符串表示未显式绑定。
fn model_workflow_summaries(server: &ComfyServer) -> Vec<Value> {
    server
        .models
        .iter()
        .map(|entry| {
            let workflow = entry.workflow.trim();
            json!({
                "id": entry.id,
                "model": entry.model,
                "enabled": entry.enabled,
                "workflow": workflow,
                "hasExplicitWorkflow": !workflow.is_empty(),
            })
        })
        .collect()
}

/// 本地工作流库（`<userData>/comfyui-workflows`）里的 API 格式文件，按文件名排序。
fn workflow_library_entries() -> Result<Vec<(String, std::path::PathBuf)>> {
    let dir = crate::comfyui::workflow::library_dir()?;
    let read = std::fs::read_dir(&dir).map_err(|error| {
        Error::from_reason(format!(
            "Failed to read ComfyUI workflow library \"{}\": {error}",
            dir.to_string_lossy()
        ))
    })?;
    let mut entries: Vec<(String, std::path::PathBuf)> = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        // 备份与原子写入临时文件不是正式工作流。
        if name.contains(".api.bak.") || name.starts_with('.') || name.contains(".tmp.") {
            continue;
        }
        entries.push((name.to_string(), path));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(entries)
}

/// 工作流库清单 + 每个文件被哪些渠道（渠道级或模型级）显式绑定。
fn library_workflow_items(servers: &[ComfyServer]) -> Result<Vec<Value>> {
    Ok(workflow_library_entries()?
        .into_iter()
        .map(|(name, path)| {
            json!({
                "name": name,
                // 未绑定时也必须是数组（不能是 null）：调用方无需做类型分支。
                "boundBy": bound_by(servers, &path),
                // 渠道级 + 模型级完整绑定关系；同渠道多模型不会被去重丢失。
                "bindings": binding_details(servers, &path),
            })
        })
        .collect())
}

/// 反查绑定到某个库文件的渠道 id（渠道级 / 模型级命中去重）。
///
/// 只返回渠道 id 是为了兼容旧消费者；完整绑定关系见 [`binding_details`]。
fn bound_by(servers: &[ComfyServer], path: &std::path::Path) -> Vec<String> {
    let mut channels: Vec<String> = Vec::new();
    for binding in binding_details(servers, path) {
        let Some(channel_id) = binding.get("channelId").and_then(Value::as_str) else {
            continue;
        };
        if !channels.iter().any(|id| id == channel_id) {
            channels.push(channel_id.to_string());
        }
    }
    channels
}

/// 渠道级与模型级的完整绑定关系（同渠道多个模型分别保留）。
fn binding_details(servers: &[ComfyServer], path: &std::path::Path) -> Vec<Value> {
    let mut bindings: Vec<Value> = Vec::new();
    for server in servers {
        if binding_points_at(&server.workflow, path) {
            bindings.push(json!({
                "channelId": server.id,
                "channelName": server.name,
                "scope": "channel",
                "modelId": Value::Null,
                "model": Value::Null,
                "enabled": server.enabled,
            }));
        }
        for entry in &server.models {
            if binding_points_at(&entry.workflow, path) {
                bindings.push(json!({
                    "channelId": server.id,
                    "channelName": server.name,
                    "scope": "model",
                    "modelId": entry.id,
                    "model": entry.model,
                    "enabled": server.enabled && entry.enabled,
                }));
            }
        }
    }
    bindings
}

/// 绑定标识是否指向该库文件：复用运行时解析器（逻辑名 / 后缀补全 / 绝对路径），
/// 避免工具报告「已绑定」但执行时找不到文件。
fn binding_points_at(binding: &str, path: &std::path::Path) -> bool {
    let raw = binding.trim();
    if raw.is_empty() || raw.starts_with('{') {
        return false;
    }
    let Some(resolved) = crate::comfyui::workflow::resolve_workflow_path(raw) else {
        return false;
    };
    if resolved == path {
        return true;
    }
    match (resolved.canonicalize(), path.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn count_array(value: Option<&Value>) -> usize {
    value.and_then(Value::as_array).map(Vec::len).unwrap_or(0)
}

/// 队列数组元素形如 `[number, prompt_id, prompt, extra_data, outputs_to_execute]`。
fn summarize_queue(value: Option<&Value>) -> Vec<Value> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .take(20)
                .map(|item| {
                    json!({
                        "promptId": item.get(1).and_then(Value::as_str).unwrap_or(""),
                        "number": item.get(0).and_then(Value::as_i64).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn queue_prompt_ids(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get(1).and_then(Value::as_str))
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn combo_values(info: &Value, class: &str, input: &str) -> Vec<String> {
    info.get(class)
        .and_then(|def| def.get("input"))
        .and_then(|input| input.get("required"))
        .and_then(|required| required.get(input))
        .and_then(|decl| decl.get(0))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 静态校验（委托共享实现：provider 预检与工具校验必须是同一套规则）。
fn validate_graph(graph: &Value, info: &Value, server_id: &str) -> Value {
    let problems = crate::comfyui::validate::validate_graph(graph, info);
    let list: Vec<Value> = problems
        .iter()
        .map(|problem| {
            json!({
                "code": problem.code,
                "node": problem.node,
                "message": problem.message,
            })
        })
        .collect();
    let mut map: Map<String, Value> = Map::new();
    map.insert("server".into(), json!(server_id));
    map.insert("ok".into(), json!(list.is_empty()));
    map.insert("problems".into(), Value::Array(list));
    map.insert(
        "nodeCount".into(),
        json!(graph.as_object().map(|nodes| nodes.len()).unwrap_or(0)),
    );
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = r#"{
      "CheckpointLoaderSimple": {
        "input": { "required": { "ckpt_name": [["a.safetensors", "b.safetensors"]] } },
        "display_name": "Load Checkpoint",
        "category": "loaders",
        "output_node": false
      },
      "KSampler": {
        "input": { "required": {
          "model": ["MODEL"],
          "seed": ["INT", {"default": 0}],
          "positive": ["CONDITIONING"],
          "negative": ["CONDITIONING"],
          "latent_image": ["LATENT"]
        } },
        "display_name": "KSampler",
        "category": "sampling",
        "output_node": false
      },
      "VAEDecode": {
        "input": { "required": { "samples": ["LATENT"], "vae": ["VAE"] } },
        "display_name": "VAE Decode",
        "category": "latent",
        "output_node": false
      }
    }"#;

    fn codes(value: &Value) -> Vec<String> {
        value["problems"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("code").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn extracts_combo_values_as_model_lists() {
        let info: Value = serde_json::from_str(INFO).unwrap();
        assert_eq!(
            combo_values(&info, "CheckpointLoaderSimple", "ckpt_name"),
            vec!["a.safetensors".to_string(), "b.safetensors".to_string()]
        );
        // 不存在的类/输入返回空列表而不是 panic
        assert!(combo_values(&info, "LoraLoader", "lora_name").is_empty());
    }

    #[test]
    fn validates_a_sound_graph() {
        let info: Value = serde_json::from_str(INFO).unwrap();
        let graph = json!({
            "1": { "class_type": "CheckpointLoaderSimple", "inputs": { "ckpt_name": "a.safetensors" } },
            "3": { "class_type": "KSampler", "inputs": {
                "model": ["1", 0],
                "seed": 1,
                "positive": ["1", 1],
                "negative": ["1", 1],
                "latent_image": ["1", 2]
            } },
            "8": { "class_type": "VAEDecode", "inputs": { "samples": ["3", 0], "vae": ["1", 2] } }
        });
        let report = validate_graph(&graph, &info, "local");
        assert_eq!(report["ok"], json!(true), "unexpected: {report}");
        assert!(codes(&report).is_empty());
    }

    #[test]
    fn reports_missing_class_missing_input_and_broken_link() {
        let info: Value = serde_json::from_str(INFO).unwrap();
        let graph = json!({
            "1": { "class_type": "MissingCustomNode", "inputs": {} },
            "3": { "class_type": "KSampler", "inputs": { "seed": 1, "model": ["99", 0] } }
        });
        let report = validate_graph(&graph, &info, "local");
        let found = codes(&report);
        assert!(found.contains(&"NODE_CLASS_MISSING".to_string()), "{found:?}");
        // KSampler 缺 positive / negative / latent_image
        assert_eq!(
            found.iter().filter(|code| *code == "INPUT_MISSING").count(),
            3,
            "{found:?}"
        );
        // model 指向不存在的节点 99
        assert!(found.contains(&"LINK_TARGET_MISSING".to_string()), "{found:?}");
    }

    #[test]
    fn rejects_non_object_workflow() {
        let info: Value = serde_json::from_str(INFO).unwrap();
        let report = validate_graph(&json!([1, 2, 3]), &info, "local");
        assert_eq!(report["ok"], json!(false));
        assert_eq!(codes(&report), vec!["WORKFLOW_INVALID".to_string()]);
    }

    #[test]
    fn summarizes_queue_entries() {
        let queue = json!([
            [12, "prompt-a", {}, {}, ["8"]],
            [13, "prompt-b", {}, {}, ["9"]]
        ]);
        let ids = queue_prompt_ids(Some(&queue));
        assert_eq!(ids, vec!["prompt-a".to_string(), "prompt-b".to_string()]);
        let summary = summarize_queue(Some(&queue));
        assert_eq!(summary.len(), 2);
        assert_eq!(summary[0]["promptId"], json!("prompt-a"));
    }

    fn model_entry(id: &str, model: &str, workflow: &str, enabled: bool) -> ComfyModelEntry {
        ComfyModelEntry {
            id: id.to_string(),
            model: model.to_string(),
            workflow: workflow.to_string(),
            param_map: String::new(),
            agent_controls: None,
            task_contract: None,
            enabled,
        }
    }

    fn comfy_server(workflow: &str, models: Vec<ComfyModelEntry>) -> ComfyServer {
        ComfyServer {
            id: "local".to_string(),
            name: "Local".to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            workflow: workflow.to_string(),
            param_map: String::new(),
            primary_model: String::new(),
            models,
            enabled: true,
        }
    }

    /// 临时库文件：`binding_points_at` 复用运行时解析器，需要真实文件。
    fn temp_workflow_file() -> (std::path::PathBuf, std::path::PathBuf) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "snow-comfyui-binding-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("anima-t2i.api.json");
        std::fs::write(&path, "{}").unwrap();
        (dir, path)
    }

    #[test]
    fn status_projection_counts_only_enabled_model_workflows() {
        let server = comfy_server(
            "",
            vec![
                model_entry("m1", "sdxl", "sdxl.api.json", true),
                model_entry("m2", "anima", "anima.api.json", false),
                model_entry("m3", "flux", "", true),
            ],
        );
        assert_eq!(count_models_with_workflow(&server), 1);
        assert!(has_explicit_workflow(&server));

        let models = model_workflow_summaries(&server);
        assert_eq!(models.len(), 3);
        assert_eq!(models[0]["hasExplicitWorkflow"], json!(true));
        // 禁用模型仍报告自己的显式绑定，但不计入 available。
        assert_eq!(models[1]["enabled"], json!(false));
        assert_eq!(models[1]["hasExplicitWorkflow"], json!(true));
        assert_eq!(models[2]["workflow"], json!(""));
        assert_eq!(models[2]["hasExplicitWorkflow"], json!(false));
    }

    #[test]
    fn status_projection_covers_channel_level_and_rejects_blank_bindings() {
        let channel = comfy_server(
            "channel.api.json",
            vec![model_entry("m1", "sdxl", "", true)],
        );
        assert_eq!(count_models_with_workflow(&channel), 0);
        assert!(has_explicit_workflow(&channel));

        // 空白字符串等于「未配置」，不能被当成显式绑定。
        let blank = comfy_server("   ", vec![model_entry("m1", "sdxl", "  ", true)]);
        assert_eq!(count_models_with_workflow(&blank), 0);
        assert!(!has_explicit_workflow(&blank));
    }

    #[test]
    fn disabled_model_workflow_does_not_make_status_configured() {
        let server = comfy_server("", vec![model_entry("m1", "sdxl", "sdxl.api.json", false)]);
        assert_eq!(count_models_with_workflow(&server), 0);
        assert!(!has_explicit_workflow(&server));
    }

    #[test]
    fn binding_matching_reuses_runtime_resolution() {
        let (dir, path) = temp_workflow_file();
        let absolute = path.to_string_lossy().to_string();
        assert!(binding_points_at(&absolute, &path));
        // 空值 / 内联 JSON 都不是库文件引用。
        assert!(!binding_points_at("", &path));
        assert!(!binding_points_at("   ", &path));
        assert!(!binding_points_at(
            r#"{"1":{"class_type":"KSampler"}}"#,
            &path
        ));
        // 不存在的路径不能误判为绑定。
        assert!(!binding_points_at(&format!("{absolute}.missing"), &path));
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    /// R7：basename 相同不能证明是同一张图；同名不同目录必须判为不同。
    #[test]
    fn workflow_identity_requires_resolved_equality() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let unique = format!("snow-identity-{}-{stamp}", std::process::id());
        let root = std::env::temp_dir().join(&unique);
        let dir_a = root.join("a");
        let dir_b = root.join("b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        let file_a = dir_a.join("same.api.json");
        let file_b = dir_b.join("same.api.json");
        std::fs::write(&file_a, "{}").unwrap();
        std::fs::write(&file_b, "{}").unwrap();
        let a = file_a.to_string_lossy().to_string();
        let b = file_b.to_string_lossy().to_string();

        // 同一文件：完全相同的标识，以及解析到同一路径的不同写法。
        assert!(same_workflow_identity(&a, &a, ""));
        // 同名不同目录：basename 相同，但不是同一张图。
        assert!(!same_workflow_identity(&a, &b, ""));
        // 内联 JSON 无法证明与任何文件绑定相同，不能借用 paramMap / policy。
        assert!(!same_workflow_identity(r#"{"1":{"class_type":"KSampler"}}"#, &a, ""));
        // 绑定为空：执行路径用模型名当工作流名，只有完全一致才匹配。
        let model = format!("{unique}-model");
        assert!(same_workflow_identity(&model, "", &model));
        assert!(!same_workflow_identity(&model, "", "another-model"));
        // 未传 workflow：使用绑定本身。
        assert!(same_workflow_identity("", &a, ""));

        std::fs::remove_file(&file_a).ok();
        std::fs::remove_file(&file_b).ok();
        std::fs::remove_dir_all(&root).ok();
    }

    /// R7：绑定为空时，显式 workflow 解析到与 fallback（模型名）相同的
    /// 本地文件也必须视为同一张，不能因为「解析成功」就直接判不同。
    #[test]
    fn empty_binding_matches_resolved_fallback_file() {
        let (dir, path) = temp_workflow_file();
        let absolute = path.to_string_lossy().to_string();
        assert!(same_workflow_identity(&absolute, "", &absolute));
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn bound_by_lists_each_channel_once_and_keeps_empty_as_array() {
        let (dir, path) = temp_workflow_file();
        let absolute = path.to_string_lossy().to_string();

        // 同一渠道的渠道级 / 多个模型级命中只记一次（含禁用模型）。
        let mut first = comfy_server(
            "",
            vec![
                model_entry("m1", "anima", &absolute, true),
                model_entry("m2", "anima-2", &absolute, false),
            ],
        );
        first.id = "chan-a".to_string();
        let mut second = comfy_server(&absolute, vec![]);
        second.id = "chan-b".to_string();
        let mut third = comfy_server("other.api.json", vec![]);
        third.id = "chan-c".to_string();

        let ids = bound_by(&[first, second, third], &path);
        assert_eq!(ids, vec!["chan-a".to_string(), "chan-b".to_string()]);

        let item = json!({ "name": "unbound.json", "boundBy": bound_by(&[], &path) });
        // 未绑定必须是空数组，不能是 null。
        assert!(item["boundBy"].as_array().is_some_and(Vec::is_empty));
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }
}
