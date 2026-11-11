//! ComfyUI 原生协议层（与 imagegen provider、内置 comfyui 服务共用）。
//!
//! 设计边界：
//! - `client`：只负责 HTTP（upload / prompt / history / view / queue / object_info / userdata）。
//! - `workflow`：只负责 API 格式图的参数注入与输出收集（纯函数）。
//! - `convert`：只负责 UI 格式画布工作流 → API 格式 + paramMap 自动推导（纯函数）。
//!
//! 这里不出现任何 MCP 工具定义，也不读配置——配置由调用方（imagegen / comfyui 服务）解析后传入。

use napi::bindgen_prelude::*;
use serde_json::Value;

use client::ComfyUiClient;

pub mod client;
pub mod contract;
pub mod convert;
pub mod policy;
pub mod validate;
pub mod workflow;

/// 把工作流标识解析成 API 图：内联 JSON → 本地库文件 → 画布工作流。
///
/// provider 与 `comfyui-describe-workflow` 共用这一套：画布名先列完整
/// 文件名，再用完整名拉取（`get_workflow` 不会补 `.json`）。
pub async fn resolve_workflow_graph(
    client: &ComfyUiClient,
    raw: &str,
    object_info: Option<&str>,
) -> Result<(Value, String)> {
    let raw = raw.trim();
    if raw.starts_with('{') {
        let graph: Value = serde_json::from_str(raw).map_err(|error| {
            Error::from_reason(format!("Invalid ComfyUI workflow JSON: {error}"))
        })?;
        return Ok((graph, "inline JSON".to_string()));
    }
    if let Some(path) = workflow::resolve_workflow_path(raw) {
        let display = path.to_string_lossy().to_string();
        let text = workflow::read_workflow_text(&display)?;
        let graph: Value = serde_json::from_str(&text).map_err(|error| {
            Error::from_reason(format!("Invalid ComfyUI workflow JSON: {error}"))
        })?;
        if !graph.is_object() {
            return Err(Error::from_reason(
                "Invalid ComfyUI workflow: an API-format workflow must be a JSON object of `node id -> node`; a UI-format workflow (nodes[]/links[]) cannot be submitted — bind it through the workflow picker so it gets converted."
                    .to_string(),
            ));
        }
        return Ok((graph, display));
    }
    let available = client.list_workflows().await.map_err(|error| {
        Error::from_reason(format!(
            "Cannot resolve ComfyUI workflow \"{raw}\": failed to list canvas workflows ({error}). Check the base URL and that ComfyUI is running."
        ))
    })?;
    let canvas = raw
        .strip_suffix(".json")
        .or_else(|| raw.strip_suffix(".JSON"))
        .unwrap_or(raw);
    let matched = available
        .iter()
        .find(|name| name.eq_ignore_ascii_case(canvas) || name.eq_ignore_ascii_case(raw));
    let Some(name) = matched else {
        return Err(Error::from_reason(format!(
            "ComfyUI workflow \"{raw}\" was not found. Canvas workflows: {}.",
            if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            }
        )));
    };
    let ui = client.get_workflow(name).await?;
    let object_info_text = match object_info {
        Some(text) if !text.is_empty() => text.to_string(),
        _ => client.object_info_text().await.map_err(|error| {
            Error::from_reason(format!(
                "Cannot convert ComfyUI canvas workflow \"{name}\": /object_info is unreachable ({error})."
            ))
        })?,
    };
    let converted = convert::convert_ui_to_api(&ui, &object_info_text)?;
    let graph = converted
        .to_json()
        .get("workflow")
        .cloned()
        .ok_or_else(|| Error::from_reason("Converted ComfyUI workflow is empty".to_string()))?;
    Ok((graph, format!("ComfyUI canvas workflows/{name}")))
}
