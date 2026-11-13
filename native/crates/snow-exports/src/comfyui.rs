//! ComfyUI 设置面板用的 NAPI 边界：连接探测、工作流库、拉取画布工作流、转换并落盘。
//!
//! 渲染层（Settings -> Image generation -> ComfyUI）直接调用这些函数，
//! 因此用户不需要安装任何外部 MCP 服务、也不需要手写 API 格式 JSON。
//!
//! 工作流库目录与应用数据库同级（`<userData>/comfyui-workflows`）：
//! 这里是**已转换成 API 格式**的稳定副本；ComfyUI 自己的 `/userdata` 存的是画布格式。

use std::path::PathBuf;

use napi::{Error, Result};
use napi_derive::napi;

use snow_mcp::comfyui::client::ComfyUiClient;
use snow_mcp::comfyui::contract;
use snow_mcp::comfyui::convert;
use snow_mcp::comfyui::policy::ControlPolicy;
use snow_mcp::comfyui::workflow;

fn encode(value: serde_json::Value) -> Result<String> {
    serde_json::to_string(&value).map_err(|_| Error::from_reason("Failed to encode ComfyUI result"))
}

/// 工作流库目录：与 snowapp.db 同级，由共享层统一解析（前端只记逻辑名）。
fn workflow_library_dir() -> Result<PathBuf> {
    workflow::library_dir()
}

/// 库里的工作流（已转换的 API 格式），按文件名排序。
fn list_library() -> Result<Vec<serde_json::Value>> {
    let dir = workflow_library_dir()?;
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let read = match std::fs::read_dir(&dir) {
        Ok(read) => read,
        Err(_) => return Ok(entries),
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        // 备份（*.api.bak.*) 与原子写入临时文件不是正式工作流，不能混进库存。
        if name.contains(".api.bak.") || name.starts_with('.') || name.contains(".tmp.") {
            continue;
        }
        entries.push(serde_json::json!({ "name": name, "path": path.to_string_lossy() }));
    }
    entries.sort_by(|a, b| {
        a.get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .cmp(
                b.get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(""),
            )
    });
    Ok(entries)
}

/// 列出两类工作流：
/// - `canvas`：ComfyUI 画布目录（`/userdata/workflows`，UI 格式，需转换）；
/// - `library`：Snow 工作流库（已转换的 API 格式，可直接绑定）。
#[napi]
pub async fn list_comfyui_workflows(base_url: String) -> Result<String> {
    // 画布列表失败不能再伪装成「成功 + 空列表」：服务没开、地址写错、
    // 认证失败必须让设置面板能显示真实原因，而不是让用户以为目录是空的。
    let mut canvas_error: Option<String> = None;
    let canvas = match ComfyUiClient::new(&base_url, Some(60)).await {
        Ok(client) => match client.list_workflows().await {
            Ok(list) => list,
            Err(error) => {
                canvas_error = Some(error.to_string());
                Vec::new()
            }
        },
        Err(error) => {
            canvas_error = Some(error.to_string());
            Vec::new()
        }
    };
    let library = list_library()?;
    encode(serde_json::json!({
        // `ok` 保留给旧调用方：本地库仍然可用，所以不把整次读取判失败。
        "ok": true,
        "canvasError": canvas_error,
        // 兼容旧字段名（画布列表）
        "workflows": canvas,
        "canvas": canvas,
        "library": library,
    }))
}

/// 从一个 API 工作流推导参数映射与可调参数（原始材料：图本身）。
///
/// 入参可以是逻辑名（工作流库里的文件名），也可以是绝对路径 —— 由共享层解析，
/// 因此设置面板只需要保存文件名，换机器不用改配置。
///
/// `param_map_json` / `agent_controls_json` 是**设置页草稿里的当前值**：
/// 必须参与推导，否则「设置页看到的 exposure」与「执行时真正生效的策略」
/// 会不一致（同一个语义在两个入口显示不同的可写性）。
#[napi]
pub async fn describe_comfyui_workflow(
    path: String,
    base_url: Option<String>,
    param_map_json: Option<String>,
    agent_controls_json: Option<String>,
) -> Result<String> {
    let text = workflow::read_workflow_text(&path)?;
    let graph: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| Error::from_reason(format!("Invalid workflow JSON: {error}")))?;
    if !graph.is_object() {
        return Err(Error::from_reason(
            "Workflow is not API format (expected a JSON object of node id -> node)",
        ));
    }
    let resolved = workflow::resolve_workflow_path(&path)
        .map(|file| file.to_string_lossy().to_string())
        .unwrap_or_else(|| "(inline JSON)".to_string());
    // 显式映射：草稿里为空/非法时不阻断分析，退回自动推导（并在返回里标注）。
    let mut override_error: Option<String> = None;
    let overrides = match param_map_json.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => match workflow::parse_param_map(raw) {
            Ok(map) => Some(map),
            Err(error) => {
                override_error = Some(error.to_string());
                None
            }
        },
        _ => None,
    };
    // 控制策略：非法 JSON 同样不阻断，退回默认 exposure。
    let mut policy_error: Option<String> = None;
    let agent_controls_value = agent_controls_json
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|raw| match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(value) => Some(value),
            Err(error) => {
                policy_error = Some(format!("Invalid agentControls JSON: {error}"));
                None
            }
        });
    let policy = match ControlPolicy::from_json(agent_controls_value.as_ref()) {
        Ok(policy) => policy,
        Err(message) => {
            policy_error = Some(message);
            ControlPolicy::from_json(None).expect("empty policy is always valid")
        }
    };

    let param_map = convert::semantic_param_map(&graph);
    let params = convert::derive_workflow_params(&graph);
    let hint = convert::format_workflow_params(&params, 12);
    let node_count = graph.as_object().map(|nodes| nodes.len()).unwrap_or(0);

    // 节点定义用于补齐类型 / 必填 / 文档；拉不到不阻断，契约会如实标注。
    let mut object_info: Option<serde_json::Value> = None;
    if let Some(url) = base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if let Ok(client) = ComfyUiClient::new(url, Some(30)).await {
            if let Ok(raw) = client.object_info_text().await {
                object_info = serde_json::from_str(&raw).ok();
            }
        }
    }
    let contract = contract::derive_contract_with_policy(
        &graph,
        object_info.as_ref(),
        overrides.as_ref(),
        Some(&policy),
    );

    let mut payload = serde_json::json!({
        "ok": true,
        "identifier": path,
        "resolvedPath": resolved,
        "paramMap": param_map,
        "paramMapJson": serde_json::to_string(&param_map).unwrap_or_else(|_| "{}".into()),
        "params": params,
        "paramsHint": hint,
        "nodeCount": node_count,
        // 分析结果：Agent 侧用 contractJson 按需读取完整结构。
        "contract": contract,
        "contractJson": serde_json::to_string(&contract).unwrap_or_default(),
    });
    if override_error.is_some() || policy_error.is_some() {
        payload["draftErrors"] = serde_json::json!({
            "paramMap": override_error,
            "agentControls": policy_error,
        });
    }
    encode(payload)
}

/// 拉取一个画布工作流：转换成 API 格式 → **落盘到工作流库** → 返回可直接绑定的路径。
///
/// 这样「从 ComfyUI 拉取」是一次性动作，之后渠道绑定的是稳定副本，
/// 不依赖 ComfyUI 的 `userdata` 是否还在。
#[napi]
pub async fn convert_comfyui_workflow(base_url: String, name: String) -> Result<String> {
    let client = ComfyUiClient::new(&base_url, Some(120)).await?;
    let ui = client.get_workflow(&name).await?;
    let object_info = client.object_info_text().await?;
    let converted = convert::convert_ui_to_api(&ui, &object_info)?;
    let mut payload = converted.to_json();

    // 落盘：<library>/<canvas 名去后缀>.api.json
    let dir = workflow_library_dir()?;
    let stem = name.trim_end_matches(".json").trim_end_matches(".JSON");
    let safe: String = stem
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other => other,
        })
        .collect();
    let file_name = format!("{safe}.api.json");
    let path = dir.join(&file_name);
    let workflow_json = payload
        .get("workflowJson")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    // 同名文件已存在且内容不同：先备份稳定副本，绝不静默覆盖。
    // 备份名带 `.api.bak.` 标记，库存枚举会排除它，不会混进正式工作流列表。
    let mut backup_path: Option<PathBuf> = None;
    if path.exists() {
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if existing != workflow_json {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or(0);
            let mut candidate = dir.join(format!("{safe}.api.bak.{stamp}.json"));
            let mut suffix = 1u32;
            while candidate.exists() {
                candidate = dir.join(format!("{safe}.api.bak.{stamp}.{suffix}.json"));
                suffix += 1;
            }
            std::fs::copy(&path, &candidate).map_err(|error| {
                Error::from_reason(format!("Failed to back up existing workflow: {error}"))
            })?;
            backup_path = Some(candidate);
        } else {
            // 内容相同：不重写稳定文件（既省 IO，也避免无意义的 mtime 变化）。
            payload["savedPath"] = serde_json::json!(path.to_string_lossy());
            payload["savedName"] = serde_json::json!(file_name);
            payload["backupPath"] = serde_json::Value::Null;
            payload["ok"] = serde_json::json!(true);
            payload["unchanged"] = serde_json::json!(true);
            return attach_contract(payload, &converted.workflow, &object_info, name);
        }
    }
    // 原子写入：先写同目录临时文件，再 rename 替换。写入中断时原稳定文件
    // 仍完整保留；临时文件不会进入库枚举。
    write_atomic(&path, &workflow_json).map_err(|error| {
        Error::from_reason(format!("Failed to save converted workflow: {error}"))
    })?;
    payload["backupPath"] = backup_path
        .as_ref()
        .map(|path| serde_json::json!(path.to_string_lossy()))
        .unwrap_or(serde_json::Value::Null);
    attach_contract(payload, &converted.workflow, &object_info, name)
}

/// 同目录临时文件 + rename：写入失败不会截断原文件。
fn write_atomic(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workflow.json");
    let temp = parent.join(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&temp, contents)?;
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

/// 附加契约分析并编码返回；与保存逻辑分离，保证「内容未变」路径也能返回。
fn attach_contract(
    mut payload: serde_json::Value,
    graph: &serde_json::Value,
    object_info: &str,
    name: String,
) -> Result<String> {
    let info_value: serde_json::Value =
        serde_json::from_str(object_info).unwrap_or_else(|_| serde_json::json!({}));
    // 注意：画布拉取/转换路径只做**无策略推导**——此刻设置页尚未选定模型项，
    // 拿不到 paramMap / agentControls，也拿不到 taskContract。所以这里返回的
    // contract 只是「图本身的能力」，不能当作执行时的完整契约；用户绑定模型项
    // 后由 `describe_comfyui_workflow`（带策略重算）覆盖 paramsSchema。
    let contract = contract::derive_contract(graph, Some(&info_value), None);
    payload["contract"] = serde_json::to_value(&contract).unwrap_or(serde_json::Value::Null);
    payload["contractJson"] =
        serde_json::json!(serde_json::to_string(&contract).unwrap_or_default());
    payload["name"] = serde_json::json!(name);
    payload["ok"] = serde_json::json!(true);
    encode(payload)
}
