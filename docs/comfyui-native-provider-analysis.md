# Snow App 原生 ComfyUI Provider 架构分析

> 由 All in Luna 任务契约驱动（run `run-Design-a-native-ComfyUI-provider-for-Snow-App-im`，task `task:deliver`）。
> 代码行号以 `fork/main`（`05876691`）为准。本文只做设计分析，不修改实现。

## 0. 结论摘要

1. **桥可以去，节点去不掉。** `ComfyUI-OpenAI-Adapter` 只是协议翻译层，Snow App 原生实现 ComfyUI HTTP 协议后即可移除；工作流里用到的自定义节点仍必须装在 ComfyUI 侧，应用层无法替代。
2. **改动集中在 imagegen 子系统。** 新增第三种 `provider: "comfyui"`，与现有 `openai` / `gemini` 并列；落盘、图库、画廊、并发限流全部复用。
3. **All in Luna 不需要改。** 它只负责编排（拆任务、并行、汇总），ComfyUI 协议由 Snow App 的 `imagegen-generate` 承担；skill 只需声明"生图走 imagegen-generate"。
4. **分期落地。** P0 单工作流 + 文生图/图生图 + 取消 + 落盘；P1 多模板与参数映射；P2 工作流发现与多参考图。

## 1. 现状（代码证据）

| 事实 | 位置 |
| --- | --- |
| 渠道结构 `ImageGenChannel { provider, base_url, api_key, model, models, allow_extra_params, ... }` | `native/src/mcp/servers/imagegen/mod.rs:158` |
| provider 分派只有两支：`"gemini"`，其余全部走 `generate_openai` | `mod.rs:528` 起 |
| OpenAI 分支：`POST {base}/images/generations`、`POST {base}/images/edits` | `providers.rs:36` |
| Gemini 分支：`generateContent` + inlineData | `providers.rs:461` |
| 渠道可用性要求 `api_key` 非空 | `ImageGenChannel::is_usable`（`mod.rs:220` 附近） |
| 参考图统一解析为 `ReferenceImage { data, mime_type }` | `reference_image.rs:12` |
| 生成结果统一落盘 + 图库索引 | `crate::storage::persist_generated_images`（`mod.rs:1511`，实现 `storage/services/image_library/mod.rs:202`） |
| 渠道配置 schema 与提示词说明 | `native/src/mcp/servers/config/imagegen_scope.rs` |
| 设置面板与本地模板 | `src/renderer/components/sidebar/ImageGenSettingsPanel.tsx`、`src/renderer/components/sidebar/imagegenSettings/constants.ts` |

**关键阻断点：**

- `is_usable()` 要求 `api_key` 非空 → 本地 ComfyUI 没有 key，必须对 `comfyui` 放宽。
- `base_url` 的 OpenAI 规则要求以 `/v1` 结尾 → ComfyUI 的根地址是 `http://127.0.0.1:8188`，不能带 `/v1`。
- fork 当前的"本地 ComfyUI"渠道实际上是 OpenAI 兼容端点，依赖 `ComfyUI-OpenAI-Adapter`；`/v1/models` 与 `/v1/images/generations` 都由该插件提供。

## 2. 目标架构

```text
All in Luna（可选编排层）
  └─ skill: 只调用 imagegen-generate，不感知 ComfyUI
       └─ Snow App imagegen-generate
            └─ provider = "comfyui"（新增）
                 ├─ POST /upload/image        参考图上传（可选）
                 ├─ POST /prompt              提交 API 格式工作流
                 ├─ GET  /history/{prompt_id} 轮询状态
                 ├─ GET  /view                下载输出图片
                 ├─ POST /interrupt           取消
                 └─ DELETE /queue             移除排队任务
                      ↓
              persist_generated_images → 图库 + content blocks → 画廊
```

设计原则：

- **协议在 Rust 侧**，不进 skill、不进渲染层。
- **工作流是数据，不是代码**：用户提供 API 格式 JSON + 节点映射，应用不硬编码任何模型。
- **失败可解释**：连接失败 / 工作流非法 / 节点缺失 / 超时 / 输出为空，分别给可操作提示。

## 3. 配置契约

`ImageGenChannel` 新增字段（全部 `#[serde(default)]`，向后兼容）：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `workflow` | string | API 格式工作流 JSON 内联文本（与 `workflowPath` 二选一） |
| `workflowPath` | string | 工作流 JSON 绝对路径；每次请求读取，便于热改 |
| `promptNodeId` / `promptInputKey` | string | 正提示词落点，默认 `"6"` / `"text"` |
| `negativeNodeId` / `negativeInputKey` | string | 负提示词（可选） |
| `seedNodeId` / `seedInputKey` | string | 随机种子（可选） |
| `widthNodeId` / `heightNodeId` / `batchNodeId` | string | 尺寸与批量落点（可选） |
| `referenceImageNodeId` | string | 图生图时写入上传文件名的节点（`LoadImage.image`） |
| `outputNodeId` | string | 只取该节点的输出；留空扫描全部 outputs |
| `pollIntervalMs` | number | 轮询间隔，默认 1000，范围 250–5000 |
| `extraInputs` | object | 任意节点输入覆盖，如 `{ "3": { "ckpt_name": "sd_xl.safetensors" } }` |
| `clientId` | string | ComfyUI 队列隔离标识；留空自动生成 |

渠道级规则：

- `provider = "comfyui"` 时：`baseUrl` 默认 `http://127.0.0.1:8188`，**不做 `/v1` 拼接**；`apiKey` 可空。
- `model` 用于选择工作流模板（P1）：`models[]` 的 `model` 值对应 `workflow` / `workflowPath`。
- `allowExtraParams` 对 comfyui 恒为 true（`params` 直接进入 `extraInputs` 合并）。

## 4. 请求生命周期

1. **解析选项**：`resolve_options()` 得到 model/size/quality；按 model 选工作流模板（P1）。
2. **加载工作流**：读取 `workflowPath` 或内联 `workflow`；JSON 解析失败 → `WORKFLOW_INVALID`。
3. **上传参考图**（有 `images` 时，每张）：
   `POST /upload/image`，multipart：`image=<bytes>`、`type=overwrite`；响应 `{ name, subfolder, type }`。
4. **注入参数**：深拷贝工作流，按映射写入：
   - `promptNodeId.promptInputKey = prompt`
   - `negativeNodeId.negativeInputKey = negativePrompt`
   - `seedNodeId.seedInputKey = seed`
   - `widthNodeId` / `heightNodeId` / `batchNodeId`
   - `referenceImageNodeId.inputs.image = uploaded.name`
   - `extraInputs` 最后合并（显式覆盖优先）
5. **提交**：`POST /prompt`，body `{ "prompt": <graph>, "client_id": <clientId> }`；响应 `{ prompt_id, number, node_errors }`。
6. **轮询**：`GET /history/{prompt_id}`，直到出现该 id 且 `status.completed` 或 `status.status_str == "error"`；同时受 `timeoutSecs` 约束。
7. **取输出**：`outputs[*].images[]` → `GET /view?filename=&subfolder=&type=output` 下载字节。
8. **落盘**：组装 content blocks（`{ type: "image", data: base64, mimeType }`）→ `persist_generated_images` → 图库索引。
9. **返回**：结构与 OpenAI/Gemini 分支一致，交给上层 `imagegen-generate` 输出。

取消路径：`POST /interrupt` + `DELETE /queue`（body `{ "delete": [prompt_id] }`）；工具中止/超时都触发。

## 5. 参数映射细节

- **点号键**：`extraInputs` 支持 `"7.steps": 30` 这类扁平键；写入前按 `nodeId.inputKey` 解析，节点不存在 → `NODE_MISSING`（不静默丢弃）。
- **未配置的映射直接跳过**，不报错：不是每个工作流都有负提示词或 seed。
- **尺寸**：优先 `widthNodeId` / `heightNodeId`；未配置时解析 `size`（如 `1024x1024`）写入 `EmptyLatentImage` 风格节点；再不行就保留工作流默认值。
- **参考图**：P0 只支持单图 `LoadImage` 节点；多图/批量参考图 P2，需要节点支持 `image_1..n` 或 `batch` 输入。
- **模型/节点发现**：P0 不强制；P2 可用 `GET /object_info` 做"验证工作流"按钮，提前发现缺节点/缺模型。

## 6. 并发、超时与取消

- 复用 `limiter::acquire()`（`maxConcurrentImages`，默认 4）。
- 轮询间隔默认 1000ms，指数退避到 2000ms；总超时复用渠道 `timeoutSecs`（默认 300）。
- 取消：`POST /interrupt`（中断当前执行）+ `DELETE /queue`（移除排队）；两者失败只记日志，不阻断返回。
- 同一 ComfyUI 可能被多个客户端共享：必须带稳定 `client_id`，避免队列互相踩踏。

## 7. 错误分类

| 错误码 | 触发条件 | 用户提示 |
| --- | --- | --- |
| `COMFY_UNREACHABLE` | 连接拒绝 / DNS / 超时 | ComfyUI 未启动或端口不对（默认 8188） |
| `WORKFLOW_INVALID` | JSON 解析失败 / 不是 API 格式 | 请用 ComfyUI「Save (API Format)」导出 |
| `NODE_MISSING` | 映射节点或输入键不存在 | 工作流节点 id 变了，请重新核对映射 |
| `PROMPT_REJECTED` | `/prompt` 返回 `node_errors` | 展示首个 node_error（缺模型/缺节点/类型错） |
| `UPLOAD_FAILED` | `/upload/image` 非 2xx | 参考图过大或格式不支持 |
| `GENERATION_TIMEOUT` | 超过 `timeoutSecs` | 提高超时或降低分辨率/步数 |
| `OUTPUT_EMPTY` | history 完成但无 images | 工作流没有 SaveImage 输出节点 |
| `CANCELLED` | 用户中止 | 已取消，队列已清理 |

## 8. 与 All in Luna 的边界

| 层 | 职责 | 是否需要感知 ComfyUI |
| --- | --- | --- |
| All in Luna | 目标拆解、依赖并行、Lane/WorkUnit、回执与汇总 | ❌ 不需要 |
| Snow App `imagegen-generate` | 参数校验、渠道选择、并发、取消、落盘 | ✅ 只认 `provider=comfyui` |
| ComfyUI provider | HTTP 协议、工作流注入、轮询、错误分类 | ✅ 唯一实现点 |

因此：**不需要给 All in Luna 增加 ComfyUI 适配**。需要补的是一份 skill 契约，声明"生图/改图必须调用 `imagegen-generate`，禁止直接 curl 8188"，以及参数映射约定（prompt/images/size/quality）。

## 9. 文件级改动清单

| 文件 | 改动 |
| --- | --- |
| `native/src/mcp/servers/imagegen/mod.rs` | `ImageGenChannel` 新增 comfyui 字段；`is_usable` 放宽 api_key；`match provider` 增加 `"comfyui"` 分支；工作流加载与模板选择 |
| `native/src/mcp/servers/imagegen/comfyui.rs`（新增） | `ComfyUiClient`：upload/prompt/history/view/interrupt/queue；工作流注入；错误分类 |
| `native/src/mcp/servers/imagegen/providers.rs` | 新增 `generate_comfyui()`，复用 `limiter`、`persist_generated_images` |
| `native/src/mcp/servers/imagegen/reference_image.rs` | 复用 `ReferenceImage`；新增 `upload_reference_to_comfy()` |
| `native/src/mcp/servers/config/imagegen_scope.rs` | schema 字段、校验（baseUrl 无 /v1、workflow 大小上限、节点映射格式）、模板说明 |
| `src/renderer/components/sidebar/ImageGenSettingsPanel.tsx` | 新增 "本地 ComfyUI（原生）" 模板与字段编辑 |
| `src/renderer/components/sidebar/imagegenSettings/constants.ts` | 模板文案与默认值 |
| `native/src/storage/services/image_library/mod.rs` | 无改动（复用） |

## 10. 分阶段计划

### P0（MVP，可验收）

- 单工作流（内联或文件）
- 文生图 + 单参考图图生图
- 节点映射：prompt / negative / seed / width / height
- 轮询、超时、取消
- 错误分类
- 结果落盘 + 画廊展示

### P1

- 多工作流模板（`models[]` → 模板）
- `extraInputs` 点号键
- 进度回报（history 的 `messages`/`status`）
- "验证工作流"按钮（`/object_info`）

### P2

- 多参考图 / batch
- 工作流自动发现（ComfyUI workflows 目录）
- UI 格式 → API 格式转换器（谨慎，边界多）
- 队列可视化与历史复用

## 11. 验收标准

1. 不安装 `ComfyUI-OpenAI-Adapter`，`imagegen-generate` 能对 `127.0.0.1:8188` 出图。
2. 图生图参考图确实进入工作流（对比输出可见差异）。
3. 取消后 3 秒内 ComfyUI 队列清空、工具返回 `CANCELLED`。
4. 八类错误码均可复现且提示可读。
5. 生成结果进入图库，画廊/缩略图/灯箱与现有 provider 一致。
6. `npm run check:ts` 与 `cargo check` 通过。

## 12. 风险与未决

| 风险 | 说明 | 缓解 |
| --- | --- | --- |
| API 格式 vs UI 格式 | `/prompt` 只吃 API 格式；用户常导错 | 文档 + 校验 + 明确报错 |
| 自定义节点依赖 | 工作流用了就必须装，应用层无法替代 | UI 明确提示；P2 用 `/object_info` 预检 |
| 节点 id 漂移 | 工作流改版后映射失效 | 保存映射版本；报 `NODE_MISSING` 时给出节点清单 |
| 本地队列共享 | 多客户端并发 | 稳定 `client_id` + 取消清理 |
| 安全 | 任意 baseUrl / 大工作流 | 仅 http(s)；workflow 上限（如 2MB）；禁止 file:// |
| 模型缺失 | 工作流引用不存在的 checkpoint | `PROMPT_REJECTED` 展示 node_error |

## 13. 本次 All in Luna 运行记录

- Run：`run-Design-a-native-ComfyUI-provider-for-Snow-App-im`
- Task：`run-Design-a-native-ComfyUI-provider-for-Snow-App-im:task:deliver`（proposed）
- Goal：设计原生 ComfyUI provider，去掉 `ComfyUI-OpenAI-Adapter` 依赖
- Done when：架构、配置 schema、生命周期、参数映射、参考图、落盘、错误码、分期计划、文件级改动点
- 本文对应上述 done-when 的全部条目。
