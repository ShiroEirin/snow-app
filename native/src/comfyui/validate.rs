//! 工作流静态校验：出图前预检与 `comfyui-validate` 工具共用同一套规则。
//!
//! 预检的意义：坏工作流在 `/prompt` 才报错，意味着白白排队、白白占显存、
//! 白白等一轮对话。这里把能静态判定的问题提前到提交之前。

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub code: &'static str,
    pub node: String,
    pub message: String,
}

/// 校验 API 格式工作流：
/// - `class_type` 是否在本机 `/object_info` 里；
/// - 连线类必填输入是否齐；
/// - 连线是否指向图里存在的节点。
pub fn validate_graph(graph: &Value, object_info: &Value) -> Vec<Problem> {
    let mut problems = Vec::new();
    let Some(nodes) = graph.as_object() else {
        problems.push(Problem {
            code: "WORKFLOW_INVALID",
            node: String::new(),
            message: "workflow must be a JSON object of node id -> node".to_string(),
        });
        return problems;
    };

    for (node_id, node) in nodes {
        let class_type = node
            .get("class_type")
            .and_then(Value::as_str)
            .unwrap_or("");
        let Some(def) = object_info.get(class_type) else {
            problems.push(Problem {
                code: "NODE_CLASS_MISSING",
                node: node_id.clone(),
                message: format!(
                    "node {node_id} uses class `{class_type}` which this ComfyUI does not report (install the custom node)"
                ),
            });
            continue;
        };
        let inputs = node.get("inputs").and_then(Value::as_object);

        if let Some(required) = def
            .get("input")
            .and_then(|input| input.get("required"))
            .and_then(Value::as_object)
        {
            for (name, decl) in required {
                // 只有「连线类」输入才是硬性要求；标量输入有默认值。
                let is_link = decl.get(0).and_then(Value::as_str).is_some();
                if !is_link {
                    continue;
                }
                let present = inputs
                    .map(|inputs| inputs.get(name).is_some())
                    .unwrap_or(false);
                if !present {
                    problems.push(Problem {
                        code: "INPUT_MISSING",
                        node: node_id.clone(),
                        message: format!(
                            "node {node_id} ({class_type}) is missing required input `{name}`"
                        ),
                    });
                }
            }
        }

        if let Some(inputs) = inputs {
            for (name, value) in inputs {
                let Some(pair) = value.as_array() else {
                    continue;
                };
                if pair.len() != 2 {
                    continue;
                }
                let Some(target) = pair.first().and_then(Value::as_str) else {
                    continue;
                };
                if !nodes.contains_key(target) {
                    problems.push(Problem {
                        code: "LINK_TARGET_MISSING",
                        node: node_id.clone(),
                        message: format!(
                            "node {node_id} input `{name}` points at node {target}, which is not in the workflow"
                        ),
                    });
                }
            }
        }
    }

    problems
}

/// 压成一行，供错误信息与工具输出使用。
pub fn format_problems(problems: &[Problem], max: usize) -> String {
    let mut lines: Vec<String> = problems
        .iter()
        .take(max)
        .map(|problem| format!("[{}] {}", problem.code, problem.message))
        .collect();
    if problems.len() > max {
        lines.push(format!("… +{} more", problems.len() - max));
    }
    lines.join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info() -> Value {
        json!({
            "Loader": { "input": { "required": {} } },
            "KSampler": { "input": { "required": {
                "model": ["MODEL"], "seed": ["INT", {"default": 0}],
                "positive": ["CONDITIONING"], "negative": ["CONDITIONING"],
                "latent_image": ["LATENT"]
            } } },
            "VAEDecode": { "input": { "required": { "samples": ["LATENT"], "vae": ["VAE"] } } }
        })
    }

    #[test]
    fn accepts_a_sound_graph() {
        let graph = json!({
            "1": { "class_type": "Loader", "inputs": {} },
            "3": { "class_type": "KSampler", "inputs": {
                "model": ["1", 0], "seed": 1,
                "positive": ["1", 1], "negative": ["1", 1], "latent_image": ["1", 2]
            } },
            "8": { "class_type": "VAEDecode", "inputs": { "samples": ["3", 0], "vae": ["1", 2] } }
        });
        assert!(validate_graph(&graph, &info()).is_empty());
    }

    #[test]
    fn reports_all_three_failure_modes() {
        let graph = json!({
            "1": { "class_type": "Missing", "inputs": {} },
            "3": { "class_type": "KSampler", "inputs": { "seed": 1, "model": ["99", 0] } }
        });
        let problems = validate_graph(&graph, &info());
        let codes: Vec<&str> = problems.iter().map(|p| p.code).collect();
        assert!(codes.contains(&"NODE_CLASS_MISSING"));
        assert_eq!(codes.iter().filter(|c| **c == "INPUT_MISSING").count(), 3);
        assert!(codes.contains(&"LINK_TARGET_MISSING"));
    }

    #[test]
    fn rejects_non_object_graph() {
        let problems = validate_graph(&json!([]), &info());
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].code, "WORKFLOW_INVALID");
    }
}
