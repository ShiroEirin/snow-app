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
use snow_mcp::comfyui::policy::{ControlPolicy, Exposure};
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

/// 工作流预检：把「服务已连接」和「这张图当前可用」分开报告。
///
/// - `serviceConnected`：是否真的读到了 ComfyUI 的 `/object_info`；
/// - `ready`：图本身有没有**阻断性**问题（缺节点 / 缺模型 / 输出不可用）；
/// - `issues`：逐条说明，`severity` 分 error / warning；
/// - `referenceSlots` / `hasPrompt` / `hasSeed`：供绘图台按能力收敛入口。
///
/// 绘图台不再自己数节点猜可写性：`seedWritable` / `maxReferenceImages` /
/// `referenceRolesConfirmed` 由这里按**执行侧同一套规则**算出。候选槽位数量
/// 不等于可提交数量——没有 Autogrow 的多 LoadImage 在角色未确认时执行侧只能
/// 接受一张图。
fn workflow_readiness(
    graph: &serde_json::Value,
    object_info: Option<&serde_json::Value>,
    overrides: Option<&std::collections::HashMap<String, Vec<String>>>,
    declared_image_output: bool,
    policy: Option<&ControlPolicy>,
) -> serde_json::Value {
    use serde_json::json;
    let mut issues: Vec<serde_json::Value> = Vec::new();
    let nodes = graph.as_object();
    let loaders = nodes
        .map(|map| {
            map.values()
                .filter(|node| {
                    node.get("class_type")
                        .and_then(|value| value.as_str())
                        .map(|class| class.contains("LoadImage"))
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0);
    let has_autogrow = nodes
        .map(|map| {
            map.values().any(|node| {
                node.get("inputs")
                    .and_then(|value| value.as_object())
                    .map(|inputs| inputs.keys().any(|key| key.starts_with("images.image_")))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    // 只有草稿里**显式**覆盖了每个图片槽位，才算角色已消歧；自动推导不算。
    let explicit_image_slots = overrides
        .map(|map| {
            (0..loaders).all(|index| {
                let name = if index == 0 {
                    "image".to_string()
                } else {
                    format!("image{}", index + 1)
                };
                map.get(&name)
                    .map(|paths| !paths.is_empty())
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    // 与执行侧同规则：无法识别终止节点时，允许 `taskContract.outputs.adapter=image`
    // 放行；已知的视频 / 音频 / 混合输出仍然拒绝。预检不能比实际执行更武断。
    match workflow::classify_output_family_detail(graph) {
        Ok(()) => {}
        Err(workflow::OutputFamilyIssue::NoTerminal) if declared_image_output => {}
        Err(issue) => {
            issues.push(json!({
                "code": "unsupported_output",
                "severity": "error",
                "message": issue.message(),
            }));
        }
    }
    if let Some(info) = object_info {
        let missing_nodes = workflow::missing_node_classes(graph, info);
        if !missing_nodes.is_empty() {
            issues.push(json!({
                "code": "missing_nodes",
                "severity": "error",
                "message": format!(
                    "缺少 {} 个节点类；请先在 ComfyUI 安装对应自定义节点",
                    missing_nodes.len()
                ),
                "items": missing_nodes,
            }));
        }
        let missing_models = workflow::missing_model_files(graph, info);
        if !missing_models.is_empty() {
            issues.push(json!({
                "code": "missing_models",
                "severity": "error",
                "message": format!(
                    "有 {} 个模型文件未安装；请先在 ComfyUI 的 models 目录补齐",
                    missing_models.len()
                ),
                "items": missing_models,
            }));
        }
    } else {
        issues.push(json!({
            "code": "service_unverified",
            "severity": "warning",
            "message": "未能读取 ComfyUI 节点清单，无法检查节点与模型是否已安装；请确认服务地址与连接",
        }));
    }
    if loaders > 1 && !has_autogrow && !explicit_image_slots {
        issues.push(json!({
            "code": "ambiguous_reference_roles",
            "severity": "warning",
            "message": format!(
                "检测到 {loaders} 个图片输入且未声明角色；一次传多张图前需要在专家设置里指定底图与参考图"
            ),
        }));
    }
    let has_error = issues
        .iter()
        .any(|issue| issue.get("severity").and_then(|value| value.as_str()) == Some("error"));
    // 三态：blocked（有阻断项）/ offline（只完成离线分析）/ ready（服务已验证且无阻断）。
    let status = if has_error {
        "blocked"
    } else if object_info.is_some() {
        "ready"
    } else {
        "offline"
    };
    let semantic_map = convert::semantic_param_map(graph);
    let has_input = |name: &str| {
        semantic_map
            .get(name)
            .map(|paths| !paths.is_empty())
            .unwrap_or(false)
    };
    // 可写性必须同时满足「有落点」与「策略不锁定」：后端两者可以并存，
    // 只看前者会让锁定的 seed 在绘图台照常显示，点下去却被执行侧拒绝。
    let writable = |name: &str| {
        has_input(name)
            && policy
                .map(|policy| policy.exposure_for(name) != Exposure::Locked)
                .unwrap_or(true)
    };
    let has_seed = has_input("seed");
    let seed_writable = writable("seed");
    // 可提交的参考图数量：Autogrow 槽位天然已确认落点；否则多 LoadImage 只有
    // 每个槽位都被 paramMap 显式覆盖才算角色已确认，否则执行侧只接受一张。
    let reference_slots = loaders;
    let reference_roles_confirmed = reference_slots <= 1 || has_autogrow || explicit_image_slots;
    let max_reference_images = if reference_slots == 0 {
        0
    } else if reference_roles_confirmed {
        reference_slots
    } else {
        1
    };
    json!({
        "status": status,
        "ready": status == "ready",
        "serviceConnected": object_info.is_some(),
        "issues": issues,
        "referenceSlots": reference_slots,
        "hasPrompt": has_input("prompt"),
        "hasSeed": has_seed,
        // 绘图台入口判据（与执行侧同源，前端不再数节点猜）：
        "seedWritable": seed_writable,
        "maxReferenceImages": max_reference_images,
        "referenceRolesConfirmed": reference_roles_confirmed,
        "hasNegative": has_input("negative"),
    })
}

/// 从一个 API 工作流推导参数映射与可调参数（原始材料：图本身）。
///
/// 入参可以是逻辑名（工作流库里的文件名），也可以是绝对路径 —— 由共享层解析，
/// 因此设置面板只需要保存文件名，换机器不用改配置。
///
/// `param_map_json` / `agent_controls_json` / `task_contract_json` 是**设置页草稿里的
/// 当前值**：必须参与推导，否则「设置页看到的 exposure / 预检结论」与「执行时真正
/// 生效的策略」会不一致（同一个语义在两个入口显示不同的可写性）。
#[napi]
pub async fn describe_comfyui_workflow(
    path: String,
    base_url: Option<String>,
    param_map_json: Option<String>,
    agent_controls_json: Option<String>,
    task_contract_json: Option<String>,
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
    // `taskContract.outputs.adapter=image`：与执行侧 `declared_image_output` 同规则，
    // 用于放行「无法从节点名识别、但用户明确声明是图片」的自定义输出。
    let declared_image_output = task_contract_json
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| value.get("outputs").cloned())
        .and_then(|outputs| {
            outputs
                .get("adapter")
                .and_then(serde_json::Value::as_str)
                .map(|adapter| adapter.eq_ignore_ascii_case("image"))
        })
        .unwrap_or(false);

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
    let readiness = workflow_readiness(
        &graph,
        object_info.as_ref(),
        overrides.as_ref(),
        declared_image_output,
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
        // 预检：服务连接与图片可用性分开报告；设置页只展示，不阻断编辑。
        "readiness": readiness,
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

    // 落盘：优先 `<library>/<canvas 名去后缀>.api.json`。同名但内容不同时
    // **另存递增副本**（`name.api-2.json`）——已绑定该文件名的渠道不能被
    // 悄悄换图；只有用户主动选择新文件才会改变绑定。
    let dir = workflow_library_dir()?;
    let stem = name.trim_end_matches(".json").trim_end_matches(".JSON");
    let safe: String = stem
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other => other,
        })
        .collect();
    let workflow_json = payload
        .get("workflowJson")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut index = 1u32;
    loop {
        let file_name = if index == 1 {
            format!("{safe}.api.json")
        } else {
            format!("{safe}.api-{index}.json")
        };
        let path = dir.join(&file_name);
        if !path.exists() {
            // 原子写入：先写同目录临时文件，再 rename。新文件路径此前不存在，
            // 因此不会截断任何已绑定的稳定副本。
            write_atomic(&path, &workflow_json).map_err(|error| {
                Error::from_reason(format!("Failed to save converted workflow: {error}"))
            })?;
            if index > 1 {
                append_warning(
                    &mut payload,
                    format!(
                        "`{safe}.api.json` already exists with different content; saved this copy as `{file_name}` instead of overwriting the bound file"
                    ),
                );
            }
            payload["savedPath"] = serde_json::json!(path.to_string_lossy());
            payload["savedName"] = serde_json::json!(file_name);
            payload["backupPath"] = serde_json::Value::Null;
            payload["unchanged"] = serde_json::json!(false);
            payload["savedAsNew"] = serde_json::json!(index > 1);
            return attach_contract(payload, &converted.workflow, &object_info, name);
        }
        // 内容相同：复用已有文件（不重写，也避免无意义的 mtime 变化）。
        if std::fs::read_to_string(&path).unwrap_or_default() == workflow_json {
            payload["savedPath"] = serde_json::json!(path.to_string_lossy());
            payload["savedName"] = serde_json::json!(file_name);
            payload["backupPath"] = serde_json::Value::Null;
            payload["unchanged"] = serde_json::json!(true);
            payload["savedAsNew"] = serde_json::json!(false);
            return attach_contract(payload, &converted.workflow, &object_info, name);
        }
        index += 1;
        if index > 999 {
            return Err(Error::from_reason(format!(
                "could not find a free workflow file name for `{safe}` (over 999 same-named copies)"
            )));
        }
    }
}

/// 向转换结果追加一条 warning（前端会把 warnings 拼进提示）。
fn append_warning(payload: &mut serde_json::Value, message: String) {
    match payload.get_mut("warnings") {
        Some(serde_json::Value::Array(items)) => items.push(serde_json::json!(message)),
        _ => payload["warnings"] = serde_json::json!([message]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 单图 + 图片输出：离线时只能算「完成离线分析」，绝不能亮绿灯。
    #[test]
    fn readiness_reports_offline_without_service() {
        let graph = json!({
            "1": { "class_type": "LoadImage", "inputs": { "image": "a.png" } },
            "2": { "class_type": "CLIPTextEncode", "inputs": { "text": "" } },
            "3": { "class_type": "SaveImage", "inputs": { "images": ["2", 0] } }
        });
        let report = workflow_readiness(&graph, None, None, false, None);
        assert_eq!(report["status"], json!("offline"));
        assert_eq!(report["ready"], json!(false));
        assert_eq!(report["serviceConnected"], json!(false));
        assert_eq!(report["referenceSlots"], json!(1));
        assert_eq!(report["maxReferenceImages"], json!(1));
        let issues = report["issues"].as_array().expect("issues array");
        assert!(
            issues
                .iter()
                .any(|issue| issue["code"] == json!("service_unverified")),
            "未连接服务时必须标注未检查"
        );
    }

    /// 视频输出必须阻断；多个图片输入且未声明角色必须提示。
    #[test]
    fn readiness_rejects_video_output_and_flags_ambiguous_roles() {
        let graph = json!({
            "1": { "class_type": "LoadImage", "inputs": { "image": "a.png" } },
            "2": { "class_type": "LoadImage", "inputs": { "image": "b.png" } },
            "3": { "class_type": "VHS_VideoCombine", "inputs": {} }
        });
        let report = workflow_readiness(&graph, None, None, false, None);
        assert_eq!(report["status"], json!("blocked"));
        assert_eq!(report["ready"], json!(false));
        // 两个 LoadImage 只是候选槽位：角色未确认时可提交数量必须收敛为 1。
        assert_eq!(report["referenceSlots"], json!(2));
        assert_eq!(report["maxReferenceImages"], json!(1));
        assert_eq!(report["referenceRolesConfirmed"], json!(false));
        let issues = report["issues"].as_array().expect("issues array");
        assert!(issues
            .iter()
            .any(|issue| issue["code"] == json!("unsupported_output")));
        assert!(issues
            .iter()
            .any(|issue| issue["code"] == json!("ambiguous_reference_roles")));
    }

    /// 与执行侧同规则：无法识别输出但 taskContract 声明图片时，预检不报阻断。
    #[test]
    fn readiness_honours_declared_image_output_for_unknown_terminal() {
        let graph = json!({
            "1": { "class_type": "CustomImageSink", "inputs": {} }
        });
        let blocked = workflow_readiness(&graph, None, None, false, None);
        assert_eq!(blocked["status"], json!("blocked"));
        let declared = workflow_readiness(&graph, None, None, true, None);
        assert_eq!(declared["status"], json!("offline"));
        let issues = declared["issues"].as_array().expect("issues array");
        assert!(
            issues
                .iter()
                .all(|issue| issue["code"] != json!("unsupported_output")),
            "taskContract 声明图片后不能再报输出不支持"
        );
    }

    /// 可写性同时看「有落点」和「策略不锁定」：只有后者被忽略时，
    /// 锁定的 seed 会在绘图台照常显示，点下去却被执行侧拒绝。
    #[test]
    fn readiness_marks_locked_seed_as_not_writable() {
        let graph = json!({
            "1": { "class_type": "KSampler", "inputs": { "seed": 42 } },
            "2": { "class_type": "SaveImage", "inputs": { "images": ["1", 0] } }
        });
        let free = workflow_readiness(&graph, None, None, false, None);
        assert_eq!(free["hasSeed"], json!(true));
        assert_eq!(free["seedWritable"], json!(true));

        let locked = ControlPolicy::from_json(Some(&json!({
            "seed": { "exposure": "locked" }
        })))
        .expect("valid policy");
        let report = workflow_readiness(&graph, None, None, false, Some(&locked));
        assert_eq!(report["hasSeed"], json!(true));
        assert_eq!(
            report["seedWritable"],
            json!(false),
            "锁定的 seed 不能被当成可写"
        );
    }

    /// 多参考图只在角色与落点确认后才放开：全部槽位由 paramMap 覆盖才算确认。
    #[test]
    fn readiness_opens_multiple_images_only_when_roles_are_declared() {
        let graph = json!({
            "1": { "class_type": "LoadImage", "inputs": { "image": "a.png" } },
            "2": { "class_type": "LoadImage", "inputs": { "image": "b.png" } },
            "3": { "class_type": "SaveImage", "inputs": { "images": ["1", 0] } }
        });
        let ambiguous = workflow_readiness(&graph, None, None, false, None);
        assert_eq!(ambiguous["referenceSlots"], json!(2));
        assert_eq!(ambiguous["maxReferenceImages"], json!(1));

        let mut overrides = std::collections::HashMap::new();
        overrides.insert("image".to_string(), vec!["1.image".to_string()]);
        overrides.insert("image2".to_string(), vec!["2.image".to_string()]);
        let declared = workflow_readiness(&graph, None, Some(&overrides), false, None);
        assert_eq!(declared["referenceRolesConfirmed"], json!(true));
        assert_eq!(declared["maxReferenceImages"], json!(2));
    }
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
    payload["readiness"] = workflow_readiness(graph, Some(&info_value), None, false, None);
    payload["contract"] = serde_json::to_value(&contract).unwrap_or(serde_json::Value::Null);
    payload["contractJson"] =
        serde_json::json!(serde_json::to_string(&contract).unwrap_or_default());
    payload["name"] = serde_json::json!(name);
    payload["ok"] = serde_json::json!(true);
    encode(payload)
}
