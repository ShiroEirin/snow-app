# Snow App 原生 ComfyUI Provider — 设计与实现（现状）

> 这份文档记录**已经落地**的实现，不是提案。代码是唯一权威，本文只解释「为什么这样切」。

## 1. 结论先行

1. **桥可以去，节点去不掉。** `ComfyUI-OpenAI-Adapter` 只是把 ComfyUI 包成 `/v1/images/generations` 的协议翻译层；Snow App 自己实现 ComfyUI 原生 HTTP 协议后，该插件已退役（改名 `.disabled`）。工作流里用到的自定义节点仍必须装在 ComfyUI 侧。
2. **模型就是工作流。** 一个渠道 = 一个 ComfyUI 端点；`models[]` 每一项 = 一个工作流。`model` 字段就是工作流名，agent 侧 `imagegen-generate(model: "anima-t2i")` 直接可用。
3. **参数落点运行时推导，不写死。** 空 `paramMap` → 从工作流图推导 `prompt / negative / seed / width / height`（以及布局板的 `layout / strict / character_boxes`）。只在推导不准时写覆盖。
4. **改动集中在 imagegen 子系统**，落盘 / 图库 / 并发 / 取消全部复用既有实现。

## 2. 代码地图

| 路径                                                   | 职责                                                                                                                             |
| ------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------- |
| `native/src/comfyui/client.rs`                         | HTTP 协议：`/system_stats`、`/queue`、`/object_info`、`/userdata`、`/upload/image`、`/prompt`、`/history`、`/view`、`/interrupt` |
| `native/src/comfyui/convert.rs`                        | UI 画布格式 → API 格式；`derive_param_map` / `semantic_param_map`（语义落点推导）                                                |
| `native/src/comfyui/workflow.rs`                       | 工作流标识解析（库文件名 / 画布名 / 内联 JSON）、参数注入、布局尺寸同步、输出收集                                                |
| `native/src/comfyui/validate.rs`                       | 提交前静态预检（缺节点 / 缺必需输入 / 断链）                                                                                     |
| `native/src/mcp/servers/imagegen/providers_comfyui.rs` | 出图编排：解析 → 预检 → 上传参考图 → 提交 → 轮询（可取消）→ 取图                                                                 |
| `native/src/mcp/servers/comfyui/mod.rs`                | 运维工具面：`comfyui-status / models / nodes / workflows / validate / queue / history`（默认禁用，不进上下文）                   |
| `native/src/exports/comfyui.rs`                        | 设置面板用的 napi 边界：列工作流、转换并落盘、推导参数                                                                           |
| `src/preload/modules/comfyuiApi.ts`                    | 面板侧的 3 个 IPC 方法                                                                                                           |

## 3. 关键约定（改代码前必读）

- **`baseUrl` 是 ComfyUI 根地址**，不带 `/v1`；`apiKey` 可空（`is_usable` / `configured` 都已对 comfyui 放行）。
- **`/userdata` 路径必须整段百分号编码**：`/userdata/workflows%2Fname.json`。真斜杠会 404（v0.43 实测）。
- **工作流标识三态**：内联 JSON（`{` 开头）→ 工作流库文件名（`<userData>/comfyui-workflows/*.api.json`）→ ComfyUI 画布名（运行时 `/userdata` 拉取 + 就地转换）。都失败才报错，并把可用清单写进错误里。
- **`paramMap` 留空 = 全自动**。显式写了的语义**会完整替换**（不再有写死默认表打底）；空数组 = 刻意关闭该语义（如 MOSS 的 `{"negative": []}`）。
- **尺寸三处同步**：`size` → latent 节点 + 布局板节点 + `layout` 文档内的 width/height（`sync_layout_size`）。
- **`params` 直接落图**：点号键（`"3.steps"`）或语义名（`"layout"`，经 paramMap）。与 `allowExtraParams` 无关 —— 那是 OpenAI 兼容分支的开关。
- **`models` 反序列化是容错的**：显式 `null` 字段按空串处理（`deserialize_string_lenient`），否则一个 null 就能让整份生图设置解析失败、工具整体消失。
- **新增内置服务必须追加到 `builtin_services_in_order()` 末尾**（prompt cache）。

## 4. 已退役 / 已删除

- `ComfyUI-OpenAI-Adapter` 插件（ComfyUI 侧改名 `.disabled`）。
- 面板里「本地 - ComfyUI 工作流（OpenAI 兼容端点）」模板（文案要求装适配器）。
- `DEFAULT_PARAM_MAP`（写死的 `6.text / 3.seed / 5.width …`）。
- `comfyui_connection_status` 导出与对应 IPC（渲染层从未调用）。

## 5. 验证手段

- `cargo test --manifest-path native/Cargo.toml --lib` —— 含 `convert` 的 UI→API 回归（真实 SD1.5 / anima / MOSS 夹具）、golden 图快照、以及「自动推导 == 旧手写映射」的对照测试。
- 真机出图走 `imagegen-generate`（provider 省略即用默认渠道）。
