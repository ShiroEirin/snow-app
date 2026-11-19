//! `comfyui-describe` 的真实入口：解析模型 / 工作流 / paramMap / 策略并推导契约。
//!
//! 抽成独立函数后，生产分发与回归测试走同一条解析路径：测试不再自己拼
//! template / paramMap / policy，也就不会漏掉「两个入口选错模型项」这类问题。

use napi::{Error, Result};
use serde_json::{json, Value};

use super::{same_workflow_identity, ComfyServer};
use crate::comfyui::client::ComfyUiClient;

pub(crate) async fn describe_contract_for_server(
    client: &ComfyUiClient,
    server: &ComfyServer,
    args: &Value,
) -> Result<Value> {
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
            if server.primary_model.is_empty() || server.primary_model.eq_ignore_ascii_case(model) {
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
                .or_else(|| enabled().find(|entry| entry.id.eq_ignore_ascii_case(model)))
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
                .find(|entry| entry.enabled && entry.model.eq_ignore_ascii_case(&primary))
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
    let (graph, workflow_source) =
        crate::comfyui::resolve_workflow_graph(&client, &workflow, object_info_text.as_deref())
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
        Some(crate::comfyui::workflow::parse_param_map(&param_map_raw)?)
    };
    // 与执行一致：taskContract.inputs 的有效 nodePath 覆盖自动映射，
    // 否则 describe 展示的图片槽位与真实注入不一致。
    if binding_matches {
        if let Some(contract) = selected_model.and_then(|entry| entry.task_contract.as_ref()) {
            let declared =
                crate::comfyui::workflow::declared_image_slot_map(contract.get("inputs"), &graph)
                    .map_err(Error::from_reason)?;
            if !declared.is_empty() {
                let map = overrides.get_or_insert_with(std::collections::HashMap::new);
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
                // 留给同步构建的工具摘要：那里不能发网络请求，但需要节点范围。
                if let Some(info) = object_info.as_ref() {
                    crate::servers::comfyui::remember_object_info(&server.base_url, info);
                }
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
