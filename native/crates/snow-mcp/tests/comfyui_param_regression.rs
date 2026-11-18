//! Agent 传参回归：断言「最终提交给 ComfyUI 的图」，不是「出图是否成功」。
//!
//! 判据来自真实工作流夹具（`tests/fixtures/*.api.json`），走的是执行路径同一个
//! `prepare_graph` 入口，所以这里的断言就是 `/prompt` 实际携带的节点值。
//!
//! **层次说明**：本文件全部落在**图准备层**（`prepare_graph` 纯函数）。工具入口
//! （`check_param_write` / `validate_top_level_writes` / HTTP 提交计数）的断言在
//! `src/servers/imagegen/providers_comfyui.rs` 的 HTTP 边界测试里，两者不互相替代。
//!
//! 覆盖三条正向回归与三条结构性分支：
//! 1. Anima：正面词 / 尺寸 / 固定 seed 真的落到节点，锁定负面词原样保留；
//! 2. MOSS：结构化布局写进布局节点，且尺寸三处同步；
//! 3. 图生图：参考图进入声明的槽位，而不是按节点 ID 猜；
//! 4. 图准备层对锁定目标的写入拒绝（入口更早报错，见上述边界测试）；
//! 5. 未映射键在图准备层只告警、不落图（入口返回 InvalidArg）；
//! 6. 多图但槽位未消歧时被拒，且不产生提交图。

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use snow_mcp::comfyui::workflow::{
    declared_image_slot_map, ensure_reference_roles, parse_param_map, prepare_graph, read_path,
};

const ANIMA: &str = include_str!("fixtures/anima-t2i.api.json");
const MOSS: &str = include_str!("fixtures/moss-image-layout.api.json");

fn anima_template() -> Value {
    serde_json::from_str(ANIMA).expect("anima fixture must be valid API-format JSON")
}

fn moss_template() -> Value {
    serde_json::from_str(MOSS).expect("moss fixture must be valid API-format JSON")
}

fn no_locks() -> HashSet<String> {
    HashSet::new()
}

/// 1a. Anima：正面词、尺寸、固定 seed 落在正确节点。
#[test]
fn anima_writes_prompt_size_and_fixed_seed_to_the_submitted_graph() {
    let map = parse_param_map(
        r#"{"prompt":"11.text","negative":"12.text","seed":"19.seed","width":"28.width","height":"28.height"}"#,
    )
    .unwrap();
    let template = anima_template();
    let prepared = prepare_graph(
        &template,
        &map,
        "1girl, solo, race queen",
        None,
        768,
        1024,
        Some(424242),
        None,
        true,
        &no_locks(),
    );

    // 正面词只写进编码节点的 text，不污染负面节点。
    assert_eq!(
        read_path(&prepared.graph, "11.text"),
        Some(&json!("1girl, solo, race queen")),
        "positive prompt must land on node 11"
    );
    // 尺寸写进 latent。
    assert_eq!(read_path(&prepared.graph, "28.width"), Some(&json!(768)));
    assert_eq!(read_path(&prepared.graph, "28.height"), Some(&json!(1024)));
    // 固定 seed 原值写入（u64 全量，不经 f64）。
    assert_eq!(read_path(&prepared.graph, "19.seed"), Some(&json!(424242)));
    // 注入记录里确实出现这些路径：没写进图的参数不会被记录。
    for path in [
        "11.inputs.text",
        "19.inputs.seed",
        "28.inputs.width",
        "28.inputs.height",
    ] {
        assert!(
            prepared.injected.iter().any(|item| item == path),
            "{path} must be reported as injected; got {:?}",
            prepared.injected
        );
    }
}

/// 1b. 锁定负面词：省略 negative 时模板原文必须逐字节保留。
#[test]
fn anima_keeps_the_locked_negative_prompt_verbatim() {
    let map = parse_param_map(
        r#"{"prompt":"11.text","negative":"12.text","seed":"19.seed","width":"28.width","height":"28.height"}"#,
    )
    .unwrap();
    let template = anima_template();
    let locked_text = read_path(&template, "12.text")
        .and_then(Value::as_str)
        .expect("anima fixture must carry a template negative prompt")
        .to_string();

    let prepared = prepare_graph(
        &template,
        &map,
        "1girl, solo",
        None, // 省略负面词 = 沿用模板
        1024,
        1536,
        Some(7),
        None,
        true,
        &no_locks(),
    );

    assert_eq!(
        read_path(&prepared.graph, "12.text"),
        Some(&json!(locked_text.clone())),
        "omitted negative must keep the template text unchanged"
    );
    assert!(
        prepared
            .injected
            .iter()
            .all(|path| !path.contains("12.inputs.text")),
        "omitted negative must not produce an injection record; got {:?}",
        prepared.injected
    );
    assert!(
        !prepared.warnings.iter().any(|item| item.contains("negative")),
        "omitting negative is not a warning condition; got {:?}",
        prepared.warnings
    );
}

/// 1c. 图准备层的最后一道收口：即便显式负面词走到了这里，锁定的目标也不接受覆盖。
///
/// 注意层次：**工具入口**在更早的 `check_param_write` / `validate_top_level_writes`
/// 就会返回 `InvalidArg`；本用例只证明图准备层不会绕过锁，不声称入口一定报错。
#[test]
fn anima_graph_layer_refuses_to_write_a_locked_negative() {
    let map = parse_param_map(
        r#"{"prompt":"11.text","negative":"12.text","seed":"19.seed","width":"28.width","height":"28.height"}"#,
    )
    .unwrap();
    let template = anima_template();
    let locked_text = read_path(&template, "12.text")
        .and_then(Value::as_str)
        .expect("template negative")
        .to_string();
    // 调用层（check_param_write / validate_top_level_writes）在提交前就会拒绝；
    // 这里验证最后一道收口：即便值走到了图准备阶段，锁目标也不接受写入。
    let locks: HashSet<String> = ["12.inputs.text".to_string()].into_iter().collect();

    let prepared = prepare_graph(
        &template,
        &map,
        "1girl, solo",
        Some("agent supplied negative"),
        1024,
        1536,
        Some(7),
        None,
        true,
        &locks,
    );

    assert_eq!(
        read_path(&prepared.graph, "12.text"),
        Some(&json!(locked_text)),
        "a locked negative target must keep the template text"
    );
    assert!(
        prepared
            .injected
            .iter()
            .all(|path| !path.contains("12.inputs.text")),
        "locked target must not be reported as injected; got {:?}",
        prepared.injected
    );
}

/// 2. MOSS：结构化布局落到布局节点，且尺寸三处同步。
#[test]
fn moss_writes_structured_layout_and_syncs_size_everywhere() {
    let map = parse_param_map(
        r#"{"prompt":"20.prompt","negative":"5.text","seed":"7.seed","width":["6.width","20.width"],"height":["6.height","20.height"],"layout":"20.layout"}"#,
    )
    .unwrap();
    let template = moss_template();
    let layout = json!({
        "width": 1024,
        "height": 1536,
        "items": [
            { "label": "subject", "caption": "a girl in a snowy field", "box": [96, 128, 832, 1280] }
        ]
    });
    let mut extra = serde_json::Map::new();
    extra.insert("layout".to_string(), layout.clone());

    let prepared = prepare_graph(
        &template,
        &map,
        "snowy field",
        None,
        1024,
        1536,
        Some(11),
        Some(&extra),
        true,
        &no_locks(),
    );

    // 布局以 JSON 文本写入节点（模型只吃字符串）。
    let written = read_path(&prepared.graph, "20.layout")
        .and_then(Value::as_str)
        .expect("layout must be written as JSON text");
    let parsed: Value = serde_json::from_str(written).expect("layout text must stay valid JSON");
    assert_eq!(parsed["items"][0]["label"], json!("subject"));
    assert_eq!(parsed["width"], json!(1024));
    assert_eq!(parsed["height"], json!(1536));
    // 三处尺寸：latent、布局节点、布局文档。
    assert_eq!(read_path(&prepared.graph, "6.width"), Some(&json!(1024)));
    assert_eq!(read_path(&prepared.graph, "6.height"), Some(&json!(1536)));
    assert_eq!(read_path(&prepared.graph, "20.width"), Some(&json!(1024)));
    assert_eq!(read_path(&prepared.graph, "20.height"), Some(&json!(1536)));
    assert_eq!(
        prepared.effective_size,
        Some((1024, 1536)),
        "effective size must follow the injected graph"
    );
    assert_eq!(
        prepared.layout_overflow, None,
        "in-bounds layout must not be flagged"
    );
}

/// 3. 参考图：声明了 nodePath 就写进那个槽位，不按节点 ID 猜。
#[test]
fn reference_images_follow_declared_slots() {
    // 两个 LoadImage：按 ID 排序是 1、2；声明故意反过来。
    let template = json!({
        "1": { "class_type": "LoadImage", "inputs": { "image": "template-base.png" } },
        "2": { "class_type": "LoadImage", "inputs": { "image": "template-mask.png" } },
        "3": { "class_type": "ImageScaleBy", "inputs": { "image": ["1", 0] } }
    });
    let declared = json!({
        "image": { "role": "base", "nodePath": "2.image" },
        "image2": { "role": "mask", "nodePath": "1.image" }
    });
    let map = declared_image_slot_map(Some(&declared), &template).expect("valid declaration");
    assert_eq!(map.get("image"), Some(&vec!["2.image".to_string()]));
    assert_eq!(map.get("image2"), Some(&vec!["1.image".to_string()]));

    // 全部槽位都声明后，两张图不算「角色不明」。
    ensure_reference_roles(&template, true, 2, Some(&declared))
        .expect("fully declared slots must pass");

    // 约定：写入按声明顺序，而不是节点 ID 顺序。
    let mut graph = template.clone();
    for (index, semantic) in ["image", "image2"].iter().enumerate() {
        let path = map.get(*semantic).and_then(|paths| paths.first()).unwrap();
        let (node_id, input) = path.split_once('.').unwrap();
        graph[node_id]["inputs"][input] = json!(format!("uploaded-{index}.png"));
    }
    assert_eq!(
        read_path(&graph, "2.image"),
        Some(&json!("uploaded-0.png")),
        "first image must land on the declared base slot, not node 1"
    );
    assert_eq!(
        read_path(&graph, "1.image"),
        Some(&json!("uploaded-1.png")),
        "second image must land on the declared mask slot"
    );
}

/// 4. 图准备层的未映射键：产生点名告警，且值不落到任何节点。
///
/// 注意层次：这一层不会 `Err`（`apply_extra_params` 只记告警）；真正返回
/// `InvalidArg` 的是工具入口的 `check_param_write`，见同目录的 HTTP 边界测试。
#[test]
fn unmapped_param_key_is_named_in_warnings_and_never_reaches_a_node() {
    let map = parse_param_map(r#"{"prompt":"11.text"}"#).unwrap();
    let template = anima_template();
    let mut extra = serde_json::Map::new();
    extra.insert("strength".to_string(), json!(0.7));

    let prepared = prepare_graph(
        &template,
        &map,
        "p",
        None,
        1024,
        1536,
        None,
        Some(&extra),
        true,
        &no_locks(),
    );

    let warning = prepared
        .warnings
        .iter()
        .find(|item| item.contains("strength"))
        .unwrap_or_else(|| {
            panic!(
                "an unmapped key must be reported by name; got {:?}",
                prepared.warnings
            )
        });
    assert!(
        warning.contains("neither a dotted workflow key"),
        "message must say why it was skipped: {warning}"
    );
    // 没有任何节点被写入这个键。
    let dumped = prepared.graph.to_string();
    assert!(
        !dumped.contains("0.7"),
        "an unmapped key must not reach any node input"
    );
}

/// 5. 多图但槽位未消歧：必须在排队前报错，不产生提交图。
#[test]
fn ambiguous_reference_slots_are_rejected_before_submission() {
    let template = json!({
        "1": { "class_type": "LoadImage", "inputs": { "image": "a.png" } },
        "2": { "class_type": "LoadImage", "inputs": { "image": "b.png" } }
    });
    // 两个 LoadImage、没有 Autogrow、没有显式槽位 → 不允许按 ID 猜。
    let error = ensure_reference_roles(&template, false, 2, None)
        .expect_err("two images without declared slots must be rejected");
    assert!(
        error.contains("image2") || error.contains("image"),
        "error must name the missing slot: {error}"
    );

    // 只声明一个槽位仍然不够：数量够但缺 image2 会让第二张走自动映射。
    let partial = json!({ "image": { "nodePath": "1.image" } });
    let partial_error = ensure_reference_roles(&template, false, 2, Some(&partial))
        .expect_err("a partial declaration must not disambiguate");
    // 文案点名的是「角色不明 + 去 taskContract 声明」，不是某个具体槽位名：
    // 断言它真的指向可修正的入口，而不是只报一句「参数错误」。
    assert!(
        partial_error.contains("taskContract.inputs"),
        "error must point at the fixable entry point: {partial_error}"
    );
    assert!(
        partial_error.contains("ambiguous"),
        "error must say the roles are ambiguous: {partial_error}"
    );

    // 单图不需要声明。
    ensure_reference_roles(&template, false, 1, None).expect("one image needs no declaration");
}

/// 6. 锁定尺寸：显式 size 不得只改一边，整体保留模板值并给出告警。
#[test]
fn locked_size_keeps_the_template_value_for_both_axes() {
    let map = parse_param_map(
        r#"{"prompt":"11.text","seed":"19.seed","width":"28.width","height":"28.height"}"#,
    )
    .unwrap();
    let template = anima_template();
    let template_width = read_path(&template, "28.width").cloned();
    let template_height = read_path(&template, "28.height").cloned();
    let locks: HashSet<String> = ["28.inputs.width".to_string()].into_iter().collect();

    let prepared = prepare_graph(
        &template,
        &map,
        "p",
        None,
        640,
        960,
        Some(7),
        None,
        true,
        &locks,
    );

    assert_eq!(
        read_path(&prepared.graph, "28.width"),
        template_width.as_ref(),
        "locked width must keep the template value"
    );
    assert_eq!(
        read_path(&prepared.graph, "28.height"),
        template_height.as_ref(),
        "height must not be changed alone when width is locked"
    );
    assert!(
        prepared
            .warnings
            .iter()
            .any(|item| item.contains("width/height are locked")),
        "the pair-lock must be reported; got {:?}",
        prepared.warnings
    );
}

/// 辅助：确保测试真的读到了夹具文件（防止 include_str! 路径失效后静默通过）。
#[test]
fn fixtures_are_readable_and_carry_the_expected_nodes() {
    let anima = anima_template();
    let anima_nodes: HashMap<&str, &str> = anima
        .as_object()
        .expect("anima graph")
        .iter()
        .map(|(id, node)| (id.as_str(), node["class_type"].as_str().unwrap_or("")))
        .collect();
    assert_eq!(anima_nodes.get("11"), Some(&"CLIPTextEncode"));
    assert_eq!(anima_nodes.get("19"), Some(&"FLS_SamplerV4"));
    assert_eq!(anima_nodes.get("28"), Some(&"EmptyLatentImage"));

    let moss = moss_template();
    // `read_path` 只接受 `<node>.<input>` 形态，裸节点 ID 要直接取对象。
    assert_eq!(
        moss.get("20").and_then(|node| node["class_type"].as_str()),
        Some("MOSSLayoutBoard")
    );
}
