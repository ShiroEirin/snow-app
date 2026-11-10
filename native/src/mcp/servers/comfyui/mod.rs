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
}

pub struct ComfyUiService;

impl ComfyUiService {
    pub fn new() -> Self {
        Self
    }

    fn servers(&self) -> Result<Vec<ComfyServer>> {
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
            if channel.get("enabled").and_then(Value::as_bool) == Some(false) {
                continue;
            }
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
                    "workflowConfigured": !server.workflow.trim().is_empty(),
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
                let (names, list_error) = match client.list_workflows().await {
                    Ok(names) => (names, Value::Null),
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
                    "configuredChannels": configured,
                    "comfyuiSavedWorkflows": names,
                    "listError": list_error,
                    "note": "comfyuiSavedWorkflows are canvas (UI) workflows. To run one through imagegen-generate, pull it once in Settings -> Image generation -> 本地 ComfyUI（原生直连）; it is converted to the API format automatically.",
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
                        let entry = server
                            .models
                            .iter()
                            .find(|entry| {
                                entry.model.eq_ignore_ascii_case(model)
                                    || entry.id.eq_ignore_ascii_case(model)
                            })
                            .ok_or_else(|| {
                                Error::from_reason(format!(
                                    "Unknown model \"{model}\" in ComfyUI channel \"{}\". Available: {}",
                                    server.id,
                                    server
                                        .models
                                        .iter()
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
                let param_map_raw = selected_model
                    .map(|entry| entry.param_map.clone())
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| server.param_map.clone());
                let overrides = if param_map_raw.trim().is_empty() {
                    None
                } else {
                    Some(crate::comfyui::workflow::parse_param_map(
                        &param_map_raw,
                    )?)
                };
                // object_info 不可用时仍返回输入清单；契约里会标注缺文档。
                let object_info = object_info_text
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
                let contract = crate::comfyui::contract::derive_contract(
                    &graph,
                    object_info.as_ref(),
                    overrides.as_ref(),
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
                &format!("ComfyUI connection status: version, devices, queue depth, whether a workflow is configured. {server_hint}"),
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
                "Show the ComfyUI channels configured in Snow App (id / baseUrl / whether a workflow is set) plus the workflows saved inside ComfyUI itself (/userdata). Canvas workflows must be pulled once in Settings -> Image generation to be converted to the API format.",
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
}
