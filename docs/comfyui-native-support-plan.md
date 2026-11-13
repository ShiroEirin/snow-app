# Snow App 本地 ComfyUI 原生支持方案

> 状态：供 DS 实施的设计与验收方案；不是已落地实现说明。  
> 目标：保留 Snow App 当前原生 HTTP 与统一生图链路，修正 Agent 对工作流参数的发现、可变范围与执行保护。  
> 日期：2026-11-11  
> 现状实现说明见 [`docs/comfyui-native-provider-analysis.md`](comfyui-native-provider-analysis.md)。

## 1. 决策摘要

**保留现有接入主干，不构建完整 ComfyUI 工作台。** 需要实现的是一层可靠的 Agent 控制面：它从当前有效工作流中发现候选输入，但只允许修改明确授权的语义参数；其他值保持工作流模板原样。

核心原则：

1. **ComfyUI 节点 schema 不是 Agent 权限表。** `/object_info` 能说明字段类型、选项和技术边界，不能单独证明该参数适合 Agent 修改。
2. **省略参数 = 保留模板值。** 不因默认值存在、字段可写或 Agent 猜测就主动覆盖。
3. **只向 Agent 暴露语义名。** 如 `steps`、`cfg`、`size`、`negativePrompt`；节点 ID 路径如 `19.steps` 留在内部执行层或专家模式。
4. **明确拒绝优于静默 no-op。** 明确请求不支持、越界、路径拼错或会覆盖连线时，在提交 ComfyUI 前返回可操作错误。
5. **自动发现常见参数，按需声明非标准语义。** 普通工作流不需逐个填写 `paramMap` / `paramsSchema`；只有多采样器、私有节点或自定义布局等歧义才需在所选 model 项添加稀疏 `agentControls` override。
6. **同一个解析结果用于 describe、工具提示、校验与执行。** `paramsSchema` 快照不得成为与运行时 workflow / `paramMap` 冲突的第二真相源。
7. **与 OpenAI、Gemini、NovelAI provider 隔离。** ComfyUI 的改动不能改变其他 provider 的输入和行为。

## 2. 范围与非目标

### 范围

- Snow App 原生 ComfyUI HTTP provider、模型/工作流绑定、`imagegen-generate` Agent 参数契约。
- ComfyUI 画布工作流转换为 API workflow、Snow 本地 workflow library、工作流枚举与状态信息。
- 常见图像生成参数发现、按工作流策略授权修改、运行时验证、尺寸同步、负向提示词和布局结构。
- 对应 Rust / TypeScript 单元测试、provider HTTP mock 测试及有限的本地 smoke test。

### 非目标

- 完整 ComfyUI 画布/节点编辑器、插件管理工作台或替代 ComfyUI 的 UI。
- 新建技能包体系、资产索引、批处理/`@last`、多实例管理。
- **本轮不实现**视频、音频、3D 的原生生成与专用结果体验；本轮仍是图片工作流支持。
- 将 ComfyUI 所有节点、节点输入、模型加载器或工作流拓扑都暴露给 Agent。
- 为每个 workflow 维护一份手写、重复的完整 `paramsSchema`。

### 2.1 通用性边界：新增模型不等于新增任务类型

当前方案对**同一图片任务家族内更换 checkpoint/编码器或添加新图片 workflow**是通用的，但不等于“任何 ComfyUI workflow 都能只注册 model 后自动完整支持”。“模型登记”与“任务家族支持”是两层：

| 场景                                         | 当前方案可否覆盖                                                                                | 仍需什么                                                                                                                                                                                                                 |
| -------------------------------------------- | ----------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 新增图片模型，继续使用已知图片 workflow 结构 | 可以；注册模型项/绑定 workflow 后，prompt、seed、尺寸、采样节点等常见字段从图与节点 schema 派生 | 新 checkpoint 文件与自定义节点依赖必须在 ComfyUI 可用；模型特有推荐分辨率/质量提示可选覆盖。无需为每个常见字段另写 `paramMap`。                                                                                          |
| 新增标准 T2I API workflow                    | 通常可以；前提是图中存在可识别的文本输入、latent/尺寸与 image 输出链                            | 多文本编码器/多阶段图等歧义需显式配置，不承诺从任意拓扑推断作者意图。                                                                                                                                                    |
| I2I /参考图/放大 workflow                    | **部分支持**；现有运行时可以上传参考图并把 `image`, `image2` 等槽位写到 LoadImage 输入          | 这不等于通用编辑语义。base image、style reference、control image、mask 等角色不能只按所有 LoadImage 的节点 ID 顺序猜；多角色/私有输入需显式角色映射。Comfy 分支当前不接受顶层 OpenAI/NovelAI `mask` 或 OpenAI `markup`。 |
| 视频、音频、3D 或其他非图片输出              | **当前不支持作为原生生成任务**                                                                  | 需要相应 task adapter、typed asset 输入/输出、下载/持久化、MIME/文件大小策略以及 UI/Agent artifact 返回；仅把 workflow/model 名登记进 imagegen 不会自动获得支持。                                                        |

**重要实现事实：** 当前 Comfy provider 由 `imagegen-generate` 调用，参考资产与结果路径是 image-specific；`workflow::collect_output_images` 只收集 history 中的 `images[]`，再经 `/view` 转成 MCP `image` block。视频/音频 workflow 可能已经花掉 GPU 时间后才因“无图片输出”报错；实现时必须增加提交前任务/输出族校验，不能拿一次 ComfyUI `/prompt` 成功当作 Snow 已支持该任务。

后续扩展应复用 HTTP 执行底座，但按任务家族增加明确 adapter，而不是把 `imagegen-generate` 膨胀成能猜所有媒体的万能工具。建议扩展契约按 workflow/model 项稀疏声明 `taskFamily` / `operation`、语义化 typed inputs（如 `prompt`、`baseImage`、`mask`、`controlImage`）和 outputs（媒体类型、输出节点/提取方式）；标准图片 workflow 继续走内置推导，无需每个模型手写完整契约。非图片家族只在它自己的 adapter 与 artifact 展示链准备好后启用。task profile 放在模型/workflow 配置项里，不使用 sidecar；当前 PR 仅落实图片家族和不支持类型的 fail-fast，不提前实现视频/音频。

### 2.2 产品定位与市场借鉴

Snow App 最合适的定位是**“让 Agent 安全调用本机 ComfyUI 工作流的 launcher/runtime”**，不是再做一个 ComfyUI 画布，也不是把所有 MCP 工具一股脑转发。界面和 Agent 应围绕 `连接 → 工作流预设 → 可调控制 → 运行 → 结果/复现` 展开：

1. **连接 ComfyUI**：一键测试 URL、版本、队列、设备资源；显示这个 endpoint 真实安装的 node/model 能力。连接状态与工作流 ready 状态分开，不能因为端口通就暗示所有 workflow 可跑。
2. **添加/选择工作流预设**：从现有 ComfyUI workflow / 官方模板导入或选已有 Snow library 文件。产品上将当前 `models[]` 对 ComfyUI 显示为“工作流预设/生成方案”，保留内部 JSON 和旧 model ID 兼容；图里的 checkpoint/LoRA/VAE 是该预设依赖，不再把工作流名误说成实际模型资产。
3. **分析并确认一次**：显示任务类型、Agent 输入角色、输出类型、自动发现的控制项、模板保留值和缺失依赖。常规图片工作流一键通过；只有图无法表达的语义才要求用户用高级 JSON 覆写说明，不能把整张图交给 Agent 猜。
4. **日常生成**：用户说“用 MOSS 布局画一张……”时，Agent 选择对应预设，沿用默认 steps/CFG/sampler，只覆盖用户明确提到的参数；生成记录关联 workflow 版本/哈希、实际参数、耗时与输出，便于复现。
5. **渐进式专家能力**：常规 Agent context 只暴露图像生成、describe 等少数高价值入口；查节点、任意图执行、运维留给显式高级通道。不要每个保存的 workflow 都注册成一个 MCP tool，也不要默认把所有 51 个 sampler/merge inputs 塞进上下文。

2026-11-11 检查的代表性项目提供了这些可借鉴点（项目 README 是能力声明，不等于独立安全/质量背书）：

| 项目                                                                                                                           | 可借鉴                                                                                                                                                                                                                                            | 不建议照搬                                                                                                                                                                                                           |
| ------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [Comfy-Org 本地 `comfy-mcp`](https://github.com/Comfy-Org/comfy-mcp) 与[官方 MCP 指南](https://docs.comfy.org/agent-tools/mcp) | 官方 Local MCP 把 `server_info`、模板/节点/模型发现、live validation、异步 jobs、取消、`fetch_outputs`、视频/音频预览拆成可发现能力，并可运行任意 API workflow；对本机安装做硬件提示。适合作为 Agent 高级模式的对照，并可作为可选的外部能力接入。 | 它当前包装 `comfy-cli`，带 41 个工具，要求单独安装/配置；不等于 Snow 的 imagegen 结果管线。直接替代 Snow native provider 会引入第二套 endpoint/config、工具重叠、任务/图片库分裂。不要把完整工具表默认放进普通会话。 |
| [轻量工作流 MCP 示例](https://github.com/ModdingFox/comfyui_mcp)                                                               | 从已保存 workflow 自动发现 Primitive 节点和 `_meta.title`，按工作流注册可调用接口。可借鉴为工作流作者显式标记“这些是 Agent inputs”的约定。                                                                                                        | 仅靠 Primitive/title 不能可靠推断模型推荐范围、底图/遮罩/control image 语义或安全 exposure；自动生成 workflow 工具也会造成工具数膨胀。                                                                               |
| [WorkflowUI](https://github.com/jimpi-dev/WorkflowUI)                                                                          | 导入时分析 I/O、把 workflow 包成带默认值的表单、排队运行、工作流版本、项目和生成 lineage。值得借鉴“预设 + 运行记录 + provenance”。                                                                                                                | Snow 目前不需要复制另一套完整 web app、项目工作台或可视化 workflow form builder。                                                                                                                                    |
| [ComfyUI 官方 workflow templates](https://github.com/Comfy-Org/workflow_templates)                                             | 官方模板按媒体组织，带模型下载/哈希和节点版本等依赖元数据，并提供本地 node compatibility 检查。适合借鉴只读 readiness/缺失依赖诊断。                                                                                                              | 不应在 Agent 没有明确授权时自动安装节点/下载模型；本轮只诊断，不做 package/model manager。                                                                                                                           |
| [ComfyUI Workspace Manager](https://github.com/11cafe/comfyui-workspace-manager)                                               | 曾提供 workflow folders/version history/gallery 的实用 UX，说明保存和复现很重要。                                                                                                                                                                 | 仓库作者已声明因 ComfyUI frontend 内建 workspace management 而停止维护；不再造 workflow 文件夹/画布工作台。借鉴运行 lineage，不复制旧插件 UI。                                                                       |

**推荐的整体形态**：第一方 native HTTP 继续负责 Snow 集成最好的图片主路径；用轻量预设/任务 contract 改善普通用户体验。对确实需要工作流图编辑、全量节点管理或未来跨媒体实验的 power user，可评估“可选连接 Comfy-Org Local MCP”，明确这是另一条专家路径，且图片库/异步任务/输出下载的集成深度不同；不要把它作为日常生成前置依赖。若未来要将视频/音频产物一等地显示在 Snow，应另建 typed artifact 生命周期与对应任务 adapter，而不是只把 MCP server 接上就算完成。

### 2.3 建议的产品路线（在当前修复之外）

- **先稳住连接与工作流预设**：Connection 显示 endpoint/ComfyUI 版本/队列/设备；工作流导入时显示 `ready / missing node / missing model / output type unsupported`。检查只读、默认不自动下载/安装。ComfyUI 图与 Snow library 分开显示。
- **把 ComfyUI 的“模型”呈现为可运行预设**：UI 显示预设名、任务（文生图/参考图编辑/放大）、用到的 checkpoint/LoRA 等资产和 Agent 可调项。内部兼容现有 `models[]`/model ID，后续再迁移成显式 workflow profile ID，不在本轮做破坏性配置迁移。
- **保持调用默认简单**：Agent 看到的是“用某预设生成/编辑”的简洁语义参数；`steps/CFG` 等默认保留 workflow，专家控制折叠；别把节点清单或数十个可写输入塞到每次 system/tool context。
- **增加轻量复现记录**：至少把 workflow 文件内容 hash/版本、最终落图参数、Comfy prompt ID、输出 artifact 和时间关联起来。workflow 更新后能提示预设 contract 需重新分析，历史生成仍可追溯。
- **高级路径先复用，再决定自研**：可研究提供可选的官方 Local MCP 连接向导，让高级用户使用 upstream 工作流/多媒体能力；保持显式启用、单独工具授权和路由说明，避免它与 Snow native image tool 同时争抢普通图片请求。检查其本地 comfy-cli 目标与 Snow baseUrl 是否同实例。
- **只有一等多媒体体验确定后才做原生 video/audio**：按 family adapter 扩展输入、MIME、输出文件/回显、异步进度、取消、保存/图库、预览和资源预算；按能力分期，不因 ComfyUI 能运行 node 就一口气承诺 3D/所有 custom nodes。

## 3. 当前基线（实现前必须保留并核对）

### 3.1 链路

```text
Snow App 设置
  → 读取 ComfyUI 画布 workflow
  → UI workflow 转 API workflow
  → 存入 Snow 本地 comfyui-workflows library
  → imagegen-generate 解析选中模型与 workflow
  → 推导/应用参数映射并预检
  → /prompt 提交、/history 轮询、取回图片
  → 复用任务取消、图库与 UI 回显
```

保持原生 HTTP，不重新引入 OpenAI 兼容桥作为前置依赖。`imagegen-generate` 继续是常规 Agent 出图入口；ComfyUI 运维工具继续与日常生图工具分离。

### 3.2 当前现场与四个已启用模型

2026-11-11 对 Snow App 当前图像生成设置及本地 ComfyUI 进行了只读核对：ComfyUI 0.43.1 在线，队列为空。Snow 的 `comfyui-local-native` channel 中四个模型均启用、均显式绑定本地 API workflow：

| 模型 ID                   | workflow                           | 当前图中的主要采样默认值                                         | 当前配置/契约观察                                                                                                                                                           |
| ------------------------- | ---------------------------------- | ---------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `anima-t2i`               | `anima-t2i.api.json`               | `FLS_SamplerV4`：steps 30、CFG 4.5、`dpmpp_2m_sde_gpu`、`beta57` | describe 能发现 prompt/negative/width/height/seed；本方案决定允许覆盖 negative，模板值仍是默认。现有提示文案需同步更新。图中还包含 FLS 参数、TeaCache、LoRA 与 2 倍后处理。 |
| `qwen-t2i-base-bf16`      | `qwen-t2i-base-bf16.api.json`      | `KSampler`：steps 25、CFG 1.0、Euler、simple                     | describe 能发现 prompt/negative/width/height/seed；CFG=1 时 negative conditioning 不起作用。                                                                                |
| `moss-image-layout`       | `moss-image-layout.api.json`       | `KSampler`：steps 50、CFG 3.0、Euler、sgm_uniform                | describe 能推断 `layout`，但 schema 来自一个样例；实际观察到 6 个 items，紧凑示例只给 2 个，并警告 required keys / enums 未确认。配置映射用 `negative: []` 保留模板负向词。 |
| `moss-image-layout-merge` | `moss-image-layout-merge.api.json` | `KSampler`：steps 50、CFG 3.0、Euler、sgm_uniform                | 工作流说明称 `layout` 是对象，但 describe 从底层节点输入观察为字符串；需把 Agent 侧语义类型与 ComfyUI 节点存储类型区分。融合节点含 51 个逐层权重，不能自动暴露。            |

当前模型 `paramsSchema` 均为空；常见参数来自运行时推导与模型提示。MOSS 两项当前映射包含 `negative: []`，表示保留模板值，不等于映射失败。

`comfyui-status.workflowConfigured: false` 目前不能用来证明这些模型没有 workflow：状态投影只看渠道级绑定，模型级绑定另存于 `models[]`。ComfyUI 画布清单、Snow 本地 API workflow library、渠道级绑定、模型级绑定必须在返回结构中区分。

**未做真实生成 smoke test。** 本次现场检查报告 PyTorch 可用显存约 142 MB（设备侧仍有约 5.5 GB free），因此本方案不把当前现场视为已验证真实出图；实现完成后应在显存充足时执行指定 smoke test。

### 3.3 当前代码地图

实现时沿用现有模块边界并以代码为准：

- `native/src/comfyui/client.rs`：ComfyUI HTTP API。
- `native/src/comfyui/convert.rs`：画布 JSON → API JSON 与常见语义参数推导。
- `native/src/comfyui/workflow.rs`：workflow 解析、参数注入、布局/尺寸同步、输出收集。
- `native/src/comfyui/contract.rs`：Agent contract/schema 推导与示例紧凑化。
- `native/src/comfyui/validate.rs`：提交前图结构预检。
- `native/src/mcp/servers/imagegen/providers_comfyui.rs`：ComfyUI submit/poll/cancel/output provider。
- `native/src/mcp/servers/imagegen/mod.rs`、`runtime.rs`：工具注册、参数/描述、任务生命周期。
- `native/src/mcp/servers/comfyui/mod.rs`：只读 status/workflows/describe 等工具。
- `native/src/exports/comfyui.rs`、`src/preload/modules/comfyuiApi.ts`、`src/main/ipc/handlers/nativeHandlers.ts`：设置 UI 的 NAPI/IPC 链路。

既有实现说明中记录的行为需继续成立：ComfyUI 的 `params` 直接写入工作流图；`allowExtraParams` 属于 OpenAI 兼容分支的设置，不应被误用为 ComfyUI Agent 参数授权开关。

## 4. 目标架构：共享执行底座、任务适配器、一个有效契约

新增/收敛一个本地 workflow resolution 结果，至少统一给以下消费者使用：

- `imagegen-generate` 的 Agent 可见参数说明；
- `comfyui-describe`；
- 提交前静态/动态校验；
- 运行时参数落图；
- workflow/status/library 的状态投影。

解析器输入：选中 channel/model、显式 workflow（若有）、workflow library、当前有效 `paramMap`、可用的 `/object_info` 缓存、所选 model 项可选的稀疏 `agentControls` override，以及仅在任务/输入/输出无法可靠推断时提供的稀疏 `taskContract`（均仅在 workflow 与 model 绑定匹配时应用）。

解析器输出：

- 实际 workflow 来源和稳定标识；
- 推导出的任务家族/操作与输入输出媒体契约；
- 当前有效映射与语义 controls；
- 每个候选输入的 Agent exposure、默认行为、类型与技术约束；
- 可校验的目标节点/输入路径与输出 adapter；
- 警告（例如模板保留值、负向提示词无效、示例截断、尺寸未同步或任务类型不支持）。

`taskContract` 与 `agentControls` 只放在 `imagegen_settings.channels[].models[]` 的稀疏配置字段中，不另建 sidecar；`taskContract` 只补 ComfyUI 图本身无法可靠表达的语义输入角色和输出类型，不能复制整份工作流 schema。普通标准 T2I workflow 继续自动推导；图片任务歧义才声明 image task profile；新任务家族必须先有对应输入/输出 adapter 才可启用。

**显式 workflow 覆盖规则：** `comfyui-describe workflow=<other>` 不得偷偷套用当前 selected model 的 `paramMap`。默认对显式 workflow 独立推导；只有调用明确同时指定并绑定到该 workflow 的 model/policy 时，才允许应用其映射。内联 JSON 同理。

**当前执行边界与 fail-fast：** 本轮仍只有 `image` task adapter。提交前必须识别图的输出家族：已识别的图片输出（如 SaveImage/PreviewImage/history `images[]`）才可交给 `imagegen-generate`；视频、音频、3D、混合媒体或无法分类的输出，在发 `/prompt` 前明确拒绝。不能等 GPU 跑完后才因没有图片回显报错，也不能将视频任务的 poster/预览误报为完整视频成功。多个 `LoadImage` 若无法由已知拓扑判明底图/遮罩/control/reference 等角色，提交前要求显式 `taskContract` 或有效映射，不按 ID 顺序猜。

后续非图片家族复用解析、提交、轮询、取消和资源限流底座，但必须有独立输入 adapter、输出/artifact adapter 与 Agent 工具/回显能力。不能仅增加模型项或放宽 `/prompt` 校验就宣称支持视频/音频。

工具发现/describe 的同步路径不得为了 `/object_info` 网络请求而阻塞 Agent 初始化。节点定义可由 ComfyUI API 缓存并在运行时刷新；若没有缓存，返回基础图推导与明确的“约束未验证”状态，提交前再校验，不得伪造 schema。

## 5. Agent 参数控制面

### 5.1 三档 exposure

| 档位       | 默认用途                     | 规则                                                                                                                                                                        |
| ---------- | ---------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `basic`    | 普通出图控制                 | 仅暴露明确识别且会被工作流消费的语义参数，如 prompt、seed、经尺寸策略验证的 size。negativePrompt 只有工作流具有有效负向路径时才有意义。                                     |
| `advanced` | 用户明确提出或开启高级模式时 | steps、CFG，以及由 `agentControls` 显式开放的少量高级项。显示模板值、技术边界、推荐范围（如有）和效果；Agent 不得自行调参。                                                 |
| `locked`   | 工作流内部实现细节           | sampler/scheduler 默认 locked；模型/CLIP/VAE/LoRA、图连接、batch、缓存/加速、后处理、输出路径、逐层融合权重及未声明字段也默认 locked。只有策略显式开放后才可进入 advanced。 |

模板中已有的值作为默认执行值。用户没有提出覆盖时不动它。若用户明确要求一个 locked/unsupported 参数，返回“此 workflow 未开放该控制”，不能静默忽略或改动相近节点。

### 5.2 当前四个模型建议的初始控制策略

这是实施初始策略，不是宣称模型参数已充分做质量评测：

| 语义参数                                                                     | 初始策略                                                                                                                                                                                                                                                               |
| ---------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `prompt`                                                                     | `basic`；写入经确认的正向文本输入。                                                                                                                                                                                                                                    |
| `seed`                                                                       | `basic`；接受合法整数/随机化策略，默认按现有 imagegen seed 行为。                                                                                                                                                                                                      |
| `size`                                                                       | `basic`；优先使用该模型 `supportedResolutions`。比例匹配容差和有效尺寸归一化规则见 §6.3；不匹配时明确拒绝，不静默取近似值。                                                                                                                                            |
| `negativePrompt`                                                             | Anima 开放：模板内置负向词为默认值，用户/Agent 显式传入时可覆盖；对应提示文案改为“默认负向词已内置，可覆盖”。Qwen 在 CFG=1 时可识别但标注无效；只有用户显式提高 CFG 后才声称负向词会生效。MOSS 当前 `negative: []` 保留模板并显示为明确 disabled，而不是从契约中消失。 |
| `layout`                                                                     | 对已识别的 MOSS Layout Board 提供结构化对象契约；省略时保持模板布局。用户提交 layout 时校验后序列化到节点所需的字符串格式。                                                                                                                                            |
| `steps`、`cfg`                                                               | `advanced`；当前模板默认值分别为 Anima 30/4.5、Qwen 25/1、两种 MOSS 50/3。技术范围来自 `/object_info`，不等于推荐范围；没有经验证的推荐范围时不得自动改变。                                                                                                            |
| `sampler`、`scheduler`、`denoise`                                            | 初始 `locked`；只有该模型的 `agentControls` 明确开放有限值后才可作为 `advanced` 控制，不把全部 ComfyUI 枚举倾倒给 Agent。                                                                                                                                              |
| FLS 专有参数、TeaCache、LoRA/upscale、MOSSMergePreset 的 51 个 layer weights | 初始 `locked`；只有用户明确配置策略后才能开放。不得由模型自行推断开放。                                                                                                                                                                                                |
| MOSS `strict`、`on_overflow`、`character_boxes`                              | 从 `/object_info` 得知合法类型/选项；只有策略开放后才进入 Agent contract。提交新 layout 时布局校验必须 fail closed，不得默默降级成纯 prompt。                                                                                                                          |

Anima 的产品默认说明和现有模型提示应把“负向词固定”改为“默认负向词已内置，可覆盖”。对用户已自定义的 `customPrompt` 不得静默覆盖；仅更新产品默认/种子文案或提供明确迁移提示。

### 5.3 控制策略的来源与扩展

目标是**不为普通新工作流逐个手写 `paramMap` 和完整 schema**，同时不假装可从任意节点图推知全部语义。

- 内置全局识别器覆盖标准 prompt/negative/latent size/sampler 类节点及稳定字段别名。识别只产生候选控制，默认规则负责 exposure。
- 已知 node class 可从 `/object_info` 提取字段类型、必填/可选、枚举、min/max/step、tooltip。节点定义只说明输入契约，不自动批准 Agent 修改。
- 配置位置固定在 `imagegen_settings.channels[].models[]`（与 workflow/profile 绑定同一配置项），不使用 sidecar 或 workflow library 的第二份配置文件。参数策略字段为稀疏 `agentControls`；仅当图无法可靠表达任务/输入/输出语义时，才增加稀疏 `taskContract`。常规新图片 workflow 无需逐项配置。
- `agentControls` 只声明 exposure、范围/选项、描述和控制依赖；节点控制目标仍由内置识别器或兼容的 `paramMap` 解析。`taskContract` 只声明 task family/operation、typed input roles 与 output adapter，不复制控制参数 schema。显式 workflow override 不得借用与该 workflow 不匹配的 model `agentControls`、`taskContract` 或 `paramMap`。
- 同一 workflow 若被多个 model 绑定，默认共用内置推导；只有控制边界或 I/O 语义确实不同才给对应 model 项添加 sparse override。冲突映射显式报错，不静默选择。复用现有 model 项设置、serde、配置导出/迁移链路。
- 高级覆写区只做折叠的多行 JSON textarea，不做完整策略表单/可视化编辑器。注意当前 `ImageGenSettingsPanel.tsx` 里的 `paramMap` 是单行 text input，并没有可复用的 JSON textarea；应新建明确的 Comfy advanced textarea。磁盘设置存结构化 JSON 对象（非双重编码字符串）；前端 draft 可暂存原始文本，读入时 pretty-print，保存前做 JSON + schema 校验，错误时阻止保存并显示字段错误。
- 必须沿 `types.ts` → `utils.ts` read/normalize → `utils.ts` form-to-JSON 白名单 → `ImageGenSettingsPanel.tsx` 完整往返 `agentControls` / `taskContract`；任何一层漏字段都会像既有 workflow/paramMap 保存 bug 一样无声丢配置。为此加保存—重载往返测试。
- 多个 `LoadImage` 不天然等于多个 Agent 参考图槽：节点可能分别代表底图、遮罩、control/depth/pose 或风格参考。只有已知拓扑可证明槽位角色时才自动命名；角色有歧义时必须由 `taskContract`/显式映射消歧，不能只按 node ID 顺序把所有图片静默绑定。

最小策略信息需能表达：basic/advanced/locked、技术范围/枚举、可选推荐范围、默认行为（保留模板/覆盖）、用户可读说明和条件依赖（如 Qwen negative 仅在 CFG 大于 1 时有效）。不要维护重复的静态 `paramsSchema` 快照作为权威。

## 6. 运行时参数注入与安全校验

### 6.1 不暴露 raw node path 给普通 Agent

- Agent 传 `steps` / `cfg` / `size` / `layout` 等语义参数；节点路径由统一 resolver 内部解析。
- 既有 `params` raw dotted path 作为专家/兼容路径保留，但必须显式 opt-in；普通 Agent 工具契约只展示策略允许项。不要用 `allowExtraParams` 控制 Comfy 分支（它不是该分支的权限开关）。
- 未提供参数时保留模板原值，不以 contract example 覆盖真实模板。

### 6.2 点号路径与 `/object_info`

对于 `params` 或由 resolver 产生的节点目标：

1. 目标 node ID 必须存在。
2. 若目标输入当前是连线，拒绝覆盖；连线识别至少要求值为二元数组、第 0 项是当前图 node ID、第 1 项为整数 slot。错误需包含语义名与 `node.input`。
3. 若输入键已经存在，继续做类型/enum/range 校验。
4. 若输入键不在当前 API graph 但用户意图使用可选 widget，查询/使用缓存的 `object_info[class_type].input.optional|required`：schema 中存在且策略开放时允许新增；节点类输入定义中不存在时拒绝，并给出有效输入名称。这样保留 `MOSSLayoutBoard.strict/on_overflow` 等“图里未存但节点定义支持”的合法可选输入，同时拦住 `19.step` 这类拼错。
5. 所有验证完成后，在工作流副本上落图，不修改 library 模板；预检失败不得提交 `/prompt`。

对象定义缓存需按 ComfyUI server/base URL、ComfyUI 版本和 node class 隔离；更新/失效后不能把其他服务器的 custom-node schema 套过来。

### 6.3 尺寸规则

- `size` 的精确像素请求 `WxH` 与比例请求分开处理。精确 `WxH` **不要求命中** `supportedResolutions`；它表达用户明确要的像素尺寸。
- 精确 `WxH`：用当前 workflow 所有目标 width/height 节点的 `/object_info` 技术约束交集校验 min/max/step（若有），再检查全局 max-pixel/resource budget。越界、无法同时满足所有同步目标、或资源预算超限时明确拒绝，不自动吸附到另一个尺寸。尺寸未出现在 `supportedResolutions` 时可返回“自定义/未验证的模型推荐尺寸”警告，但不得因此单独拒绝。
- 比例请求 `r`：只从该模型 `supportedResolutions` 选择候选。相对误差 `abs((w / h) / r - 1)` 必须 `≤ 2%`；例如 Anima `16:9` 可归一化到 `1344x768`（约 1.56%）。选误差最小者；并列选像素面积最接近该模型 `defaultSize` 者，仍并列则按原列表顺序。回报/记录实际采用的 `WxH`，不能静默近似。
- 无候选满足 2% 容差时，拒绝比例请求并列出可选 `supportedResolutions`；不取最近值、不回退模板尺寸。ComfyUI 节点最大值只是技术边界，不代表模型最佳分辨率。
- 精确尺寸或归一化尺寸都必须同步该 workflow 中所有已识别的 latent width/height、MOSS Layout Board width/height，以及布局对象内的 width/height；任何不能满足的图都在提交前失败。
- workflow 有 2 倍/其他放大节点时，明确区分输入 latent/base 尺寸与最终输出尺寸；不得让 Agent 误以为 `size` 指最终像素尺寸。

### 6.4 negative 与 layout 语义

- 顶层 `negativePrompt` 与语义 `negative` 区分。`negative: []` 表示明确禁用覆盖、保留模板值；describe/Agent context 不得让它静默消失，应显式返回 `exposed: false`、`reason: "disabled by paramMap"`，并说明仍使用模板负向词。
- Anima 允许覆盖内置默认负向词；Qwen 当前 CFG=1 时负向词没有实际作用，只有用户显式调整 CFG 后才可声称生效。路径缺失、禁用或无效果时都应说明，不得只因存在一个 STRING 节点就声称 negative 可控。
- MOSS Layout Board 底层 `layout` 输入是 multiline STRING；Agent-facing `layout` 可以是结构化对象，执行层沿用已有序列化/布局尺寸同步实现。
- layout 示例截断时不得返回看似完整、可回传的数组。提供 `exampleTruncated` 与如 `object{width,height,items[6]}` 的摘要；示例不是模板默认值。
- `nodeRequired`（Comfy 节点输入要求）和 `agentRequired`（Agent 必须提供覆盖值）分离；`confidence` 描述推导信心，不从 required 派生。

## 7. 描述与状态工具

### `comfyui-describe`

- 传模型时，展示该模型当前 workflow、有效映射、语义名、类型、是否模板默认可保留、exposure、schema 约束及 warnings。
- 传显式 workflow 时不得借用其他 selected model 的 `paramMap`。
- structured example 是摘要时必须标记，不可暗示为完整默认值。
- 对高级参数列出当前模板值和 technical range；没有模型推荐范围就明确写“未校准”，不推荐 Agent 自动改变。

### `comfyui-workflows` / `comfyui-status`

- 区分 ComfyUI `/userdata` canvas inventory 与 Snow 本地 API workflow library；不能把“画布列表空”报告成“本地 library 无文件”。
- library 枚举默认轻量：文件标识、来源、转换状态、`boundBy: []`（从所有模型配置反查，必须是数组）。contract 摘要按需产生，不对库中每个文件启动完整派生。
- 状态至少能准确表示模型级显式绑定：`hasExplicitWorkflow` 表示渠道级或已启用模型级存在显式绑定；`modelsWithWorkflow` 只计启用且显式绑定的模型。
- `modelNameFallback` 只表示 fallback 策略存在，不证明对应文件已找到。文件存在性由实际本地解析结果表达。

## 8. 工作流 library 写入安全

转换 UI workflow 到固定名称 `.api.json` 前检查目标文件：

- 文件内容相同：不重复写入或只更新明确的来源信息。
- 文件不同：不静默覆盖稳定副本；采用现有 UX 能实现的显式 replace/另存为/备份路径策略。
- 绑定中的旧 workflow 在被替换或删除前需能识别影响模型项。
- 备份名必须稳定且可清理；无备份或写入失败时原文件完整保留。

这项保持轻量，不扩展成版本控制或图工作台。

## 9. 分阶段实施与交付顺序

按可独立合并的三份 PR 交付；PR-1 可独立发布，PR-2 接在其上实现运行时参数策略，PR-3 收尾状态、文案和非阻塞的 provider integration tests。

### PR-1 — Contract 语义与解析一致性（P0，可独立发布）

1. 抽取当前有效 workflow / paramMap 解析结果，统一供 describe、工具说明、预检与执行使用。
2. 修复显式 workflow 借用所选模型 paramMap/policy 的问题；未绑定的显式 workflow 独立推导。
3. Contract 分离 `nodeRequired` / `agentRequired` / `confidence`；截断结构增加标记；`negative: []` 在 describe 中显式呈现 `exposed: false` 与原因。
4. 让普通字段动态推导，`paramsSchema` 退为兼容/非权威缓存。
5. 覆盖 PR-1 单元测试和工具输出断言；PR-1 不依赖 PR-2 的新增可变参数权限。

### PR-2 — Agent 控制策略与运行时保护（P0）

1. 在 `imagegen_settings.channels[].models[]` 增加可选稀疏 `agentControls` 和 `taskContract`；磁盘中存 JSON object，缺省时使用图片家族内置推导。`taskContract` 本轮仅表达 image operation 和有歧义的图片输入角色/输出类型，不启用视频/音频。UI 用折叠 JSON textarea（不做策略表单），完善 TS 类型、读/写白名单、serde 与配置导出/导入往返。
2. 建立 basic/advanced/locked exposure；默认保留 workflow 模板所有未声明值。raw dotted params 仅走专家 opt-in；普通 Agent 只见语义控制。
3. 注入前用 `/object_info` 校验节点输入、类型、enum/range；保留 schema 声明的合法 optional 输入；未知字段/typo 报错；禁止覆盖连线。
4. 修正尺寸规则：比例只在 §6.3 定义的相对误差 ≤2% 时归一化；精确 WxH 不要求命中 supportedResolutions，按节点 min/max/step 与资源预算检查；两种路径都不能静默改尺寸。
5. 同步所有 width/height 目标和 layout JSON dimensions；检查最大像素、steps、batch 与并发预算；Anima base/final 2x 语义明确。
6. Anima negative 开放覆盖并更新产品默认说明；MOSS disabled 状态显式；Qwen CFG=1 的 negative 无效性明确。
7. 只接受可由图片 adapter 收集和展示的图像输出；提交前拒绝视频/音频/3D、混合媒体或无法分类的输出。多 `LoadImage` 只有角色可判定时才自动映射，否则要求 taskContract/显式角色映射；禁止按节点 ID 顺序猜语义。

### PR-3 — 状态、文案、library 与 P1 集成验证

1. status/workflows 支持模型级显式绑定和启用状态；区分 canvas inventory 与 Snow library；`boundBy` 用数组，contract 摘要按需生成。
2. workflow 转换同名冲突处理/备份，不静默覆盖。
3. 修正 Comfy `params`、negativePrompt、semantic `negative`、尺寸和布局文案；实现落地后更新现状说明 [`docs/comfyui-native-provider-analysis.md`](comfyui-native-provider-analysis.md)。
4. **P1** 增加 provider HTTP mock 生命周期测试（submit/poll/cancel/reference upload/output/gallery）；它是独立工作量，不阻塞 PR-1/PR-2 的 P0 合并，但在声称完整 provider 生命周期验证前必须完成。
5. 设备可用时做本地 smoke。显存不足时记为 `BLOCKED`，不是 PASS，也不是功能 FAIL；不得用 `/object_info` 查询冒充真实出图验证。
6. 增加只读 Workflow Readiness 报告：可识别的缺失 node class、缺失模型文件、ComfyUI 版本/node schema 不匹配、unknown output type；只诊断并提供修复提示，不自动安装节点或下载模型。
7. 为 ComfyUI 设置/describe 提供 profile 友好的展示名和识别摘要，不强制迁移现有 `model` / `models[]` 存储键。

### 后续产品阶段（不阻塞 PR-1/2/3）

1. 连接/预设 UI：ComfyUI 渠道使用“工作流预设/生成方案”展示名；显示 task type、模型资产依赖、Agent controls 和 ready 状态。底层沿用 `models[]` 兼容，不急于迁移存储字段。
2. 运行复现：将 workflow 内容 hash、实际提交图中的控制值、Comfy prompt ID、输出文件/artifact 与 gallery/task card 关联；workflow 更新时识别旧记录依赖的版本。
3. 官方 Local MCP 作为**可选专家路径**做独立评估：确认 Snow 可管理其 stdio 安装/版本/授权并验证 ComfyUI target 一致性，再决定是否提供向导。它不应和 native `imagegen-generate` 重复处理普通图像请求，也不能绕过 Snow 的参数/文件/工具授权边界。
4. 如需 Snow 一等显示视频/音频，再单独定义 task adapter、typed media asset store、预览/播放 UI、下载上限、异步任务状态和回显 API；这是新项目阶段，不从本轮“图执行成功”推定完成。

### 仓库外的本地凭据卫生（不属于 ComfyUI PR）

`.tmp/` 已由 `.gitignore` 忽略，因此历史快照不是“已进入 Git 仓库”的证据；若本机 `.tmp` 快照仍含明文凭据，仍应在本地轮换/清理，并确保不复制进 PR。该清理不作为本功能的 P0/P1 代码验收项。

## 10. 必须通过的验收矩阵

| ID   | 优先级 | 场景                                  | 通过条件                                                                                                                                                                                                                               |
| ---- | ------ | ------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| C-01 | P0     | 普通 KSampler workflow、空 `paramMap` | Agent contract 自动发现 prompt、seed、width、height；steps/CFG 候选按 exposure 策略显示；无需为每个参数手写 `paramsSchema`。                                                                                                           |
| C-02 | P0     | 当前四个模型                          | 默认值与 workflow 一致：Anima 30/4.5、Qwen 25/1、MOSS 两项 50/3；未请求覆盖时发送图不改变这些值。                                                                                                                                      |
| C-03 | P0     | 显式 workflow 覆盖                    | `comfyui-describe workflow=anima-t2i.api.json` 与 `model=moss-image-layout workflow=anima-t2i.api.json` 不得因 MOSS `negative: []` 产生不同/污染的 Anima contract。                                                                    |
| C-04 | P0     | 节点 required 与 Agent required       | 模板已有有效值的 layout、negative 等输入不会仅因节点 required 就要求 Agent 覆盖；confidence 不随 required 机械变动。                                                                                                                   |
| C-05 | P0     | 截断布局示例                          | MOSS 6-item 示例若只返回 2 项，必须 `exampleTruncated=true`；不传 layout 时模板完整值保持不变。                                                                                                                                        |
| C-06 | P0     | optional node input                   | `MOSSLayoutBoard.strict` / `on_overflow` 若策略开放且 `/object_info` 声明有效，可新增；不在节点 schema 的 `19.step` 明确拒绝并列可用字段。                                                                                             |
| C-07 | P0     | 连线覆盖保护                          | `paramMap`/raw params 指向真实 `[node_id, slot]` 连线时提交前报错；普通坐标/box 数组不误报。                                                                                                                                           |
| C-08 | P0     | CFG/steps 越权                        | Agent 默认请求不改 sampler 数值；显式 advanced 输入在 policy 和技术边界通过后才改图；越界/未开放的输入不提交 `/prompt`。                                                                                                               |
| C-09 | P0     | 负向提示                              | Anima negative 可覆盖且模板值为默认；Qwen CFG=1 标注无效；MOSS `negative: []` 在 contract 中显式为 `exposed:false`、`reason:"disabled by paramMap"`，并说明继续使用模板负向词。                                                        |
| C-10 | P0     | size                                  | 比例只在误差 `≤ 2%` 时归一化并回报实际 `WxH`；无候选则拒绝。精确 `WxH` 不要求出现在 `supportedResolutions`，满足所有目标节点 min/max/step + max-pixel 预算即可接受；不满足明确拒绝、不静默吸附。MOSS dimensions 全同步。               |
| C-11 | P1     | 状态投影                              | 渠道级无 workflow、4 个启用模型级分别绑定时 `hasExplicitWorkflow=true`、`modelsWithWorkflow=4`；禁用模型不计数；fallback 不虚报文件存在。                                                                                              |
| C-12 | P1     | library                               | 一 workflow 多 model 返回 `boundBy` 数组；canvas list 空不表示 Snow library 空；同名冲突不会静默覆盖。                                                                                                                                 |
| C-13 | P1     | provider 回归                         | 独立 mock HTTP 测试覆盖 submit、poll、cancel、参考图上传、output 下载和图库持久化；不阻塞 PR-1/PR-2 P0 合并，须在宣称完整 provider 生命周期验证前完成。                                                                                |
| C-14 | P1     | provider 隔离                         | OpenAI/Gemini/NovelAI 的 schema、payload、图像编辑与 negative 行为无回归。                                                                                                                                                             |
| C-15 | P1     | 真实本地 smoke                        | VRAM 足够时测试标准 KSampler、Anima FLSSampler、MOSS layout；不足时状态为 `BLOCKED`（既非通过也非失败），不得用 `/object_info` 查询冒充真实生成；记录参数落图、队列、尺寸与回显。                                                      |
| C-16 | P0     | 新图片模型/工作流通用性               | 新增一个使用标准 image T2I 节点的模型项，只需绑定其 workflow 与必要的模型默认值；prompt/seed/size/steps/CFG 候选由通用推导发现，不要求复制每个参数的 `paramMap` / `paramsSchema`。未知模型文件/依赖仍需在 ComfyUI 安装并在配置中登记。 |
| C-17 | P0     | 非图片输出 fail-fast                  | 只有视频/音频/3D、混合媒体或无法分类 output 的 workflow 在提交 `/prompt` 前被拒绝；mock 断言 ComfyUI 未收到提交。不能跑完整任务后再报“无图片输出”，也不能把 poster 当完整视频。                                                        |
| C-18 | P0     | 多 LoadImage 输入角色                 | 对能确认的标准 base image 槽可自动绑定；多个 LoadImage 角色有歧义时不按 node ID 顺序注入，返回待声明角色/映射的明确错误；explicit taskContract/映射可解歧且不破坏图中其他连线。                                                        |
| C-19 | P0     | 策略配置保存往返                      | `agentControls` / `taskContract` 经 settings read → 表单 → save → reload、复制渠道、添加模型和切换模板后，字段按预期保留或显式清空；无任何保存路径静默丢字段。textarea JSON 非法时阻止保存并定位错误。                                 |
| C-20 | P0     | 精确像素尺寸                          | 一个技术约束合法但不在 `supportedResolutions` 的精确 `WxH` 可提交且不被吸附；违反任一目标节点 step/min/max 或全局 pixel budget 时在提交前报错。                                                                                        |
| C-21 | P1     | Workflow Readiness                    | 已知缺失 node class/model files、版本不兼容与 unknown output type 能在生成前被诊断；本项只读，不自动安装/下载。                                                                                                                        |
| C-22 | P1     | Agent 工具面不随 workflow 数量膨胀    | 注册 50 个 workflow profiles 不产生 50 个 MCP tools；常规生图仍走单一 `imagegen-generate`，只向当前选择 profile 注入紧凑能力说明，完整 graph/contract 通过按需 describe 查看。                                                         |

## 11. 测试与提交纪律

实现交付必须附上：

- PR-1/PR-2 的 P0 单元与 contract 测试及实际通过输出；至少运行 `cargo test --manifest-path native/Cargo.toml --lib` 与 `npm run check:ts`。
- C-19 必须有 `toImageGenForm` / `toImageGenSettingsJson` 或等价 UI 测试覆盖新字段保存/重载、clone/template/reset 路径；单跑 TypeScript 检查不足以证明白名单序列化没有丢字段。
- P1 provider mock 测试按 PR-3 / C-13 完成；其未完成不阻塞 PR-1/PR-2，但不得宣称完整 provider 生命周期已验证。
- 本地真实生成 smoke 是 integration evidence；显存不足或设备不可用时记为 `BLOCKED`，不是通过也不是失败，不得以 `/object_info` 查询替代。
- 对发生变化的 `params` / `paramMap` 兼容性的说明，包括 raw params 是否进入专家模式、旧调用如何处理。
- 不做与本方案无关的工作流编辑器、批处理、多媒体扩展或其他 provider 重构。

## 11.5 评审后补充的实施约束与当前落地范围（P0 评审基线）

以下五条是 2026-11-11 对 §6.3、C-10、C-18 与执行链路的复核补充，优先于上文任何含糊表述；与代码冲突时以代码为准并回写本节。

1. **映射与权限分层，不压成一条优先级（已实现，单测覆盖）。**
   `paramMap` 与内置识别回答“写到哪里”（目标解析）；`agentControls` 回答“能不能写”（授权与 exposure）。两者分别解析、共同执行；不得把 `paramMap > 内置识别 > agentControls` 写成同一条链，也不得以隐藏语义别名代替目标路径锁定。
   **复核后补充**：授权覆盖**所有**写入口——顶层 `negativePrompt` / `seed` / `size` / 参考图、`params` 语义名与点号键、引擎默认/随机值；不再只检查 `params`。locked + 显式值拒绝，locked + 隐式值（随机 seed、默认尺寸）只保留模板值。未知 raw 点号键默认拒绝：自定义输入必须先进入 `paramMap`，并在 `agentControls` 显式开放。
2. **布局越界不得默认放行（已实现：默认拒绝 + `allowLayoutOverflow` 显式放行）。**
   尺寸变更时同步 layout 宽高；`items[].box` 越界必须显式定义策略：**默认拒绝**该请求并提示补布局或换尺寸。允许越界或按比例缩放只能作为显式开启的策略，不能作为默认行为，也不能以“保持作者构图”为由在越界时继续执行。
3. **尺寸与比例链路禁止静默回退（已实现）。**
   显式传入的 `size` 必须是合法 `WIDTHxHEIGHT` 或可归一化的比例；比例请求按 `supportedResolutions` 的像素候选以 2% 容差选优并回报实际 `WxH`，无候选/超容差/档位（`1K`）/非法字符串/白名单不匹配仍在提交 `/prompt` 前报错。必须覆盖完整入口链：`sanitize_size` 丢弃显式请求与 `parse_size` 解析失败都不得退化为模板尺寸。只有**省略** `size` 时才保留当前默认（请求/模型/渠道默认 > 模板自带 > 1024）。将来若启用比例换算，必须同时给出换算基准与预算，并回报实际采用的 `WxH`。
4. **锁定按规范化目标路径执行（已实现：点号键直写锁定路径会被拒绝）。**
   `paramMap` 的 `negative: []` 只关闭语义注入，不阻止点号键直写同一节点输入。任何 locked / disabled 语义都必须按规范化路径（`5.image` ≡ `5.inputs.image`）拦截写入，不能只隐藏参数名。实现上 `paramMap` 空数组与 `agentControls`/默认 locked 的语义路径都会纳入锁定集合；**复核后补充**：语义别名（如 `myneg: "7.text"`）展开后也检查同一目标锁集合，不能再靠「换个名字指向同一输入」绕过；策略约束（exposure / options / min-max / 标量类型）与 `/object_info` 的 enum/min/max/step **同时满足**才允许写入。**已知限制**：路径比较做了小写归一，大小写敏感的自定义输入可能被误锁。
5. **缓存与判据只认本次有效结果（已实现：每次重算 + 按注入后有效值判断）。**
   契约不做跨请求缓存，describe / 工具提示 / 校验 / 执行都从当前 workflow + `paramMap` + `agentControls` 重新推导；模型项里的 `paramsSchema` 只作兼容展示，不是运行时真相源。`CFG=1 → negative 无效` 在 `prepare_graph` 注入完成后读取**本次覆盖后的有效 cfg** 判断，只对已识别的采样链（`paramMap` 有 `cfg`）告警。

配套测试要求（与 §11 同等效力）：

- Autogrow 多图必须补“槽位编号与节点 ID 顺序相反 + 存在无关 `LoadImage`”的反例，证明映射依据是槽位连线而不是节点 ID；只有存在连线证据的槽位可以自动映射。
- 尺寸回归必须覆盖五类：比例请求、非法字符串、合法 `WxH`、白名单不匹配、完全省略；比例/非法字符串/白名单不匹配必须在 ComfyUI 分支报错，合法 `WxH` 必须生效，省略时保留默认。
- 显式尺寸还必须验证「确实落图」：`paramMap` 把 `width` / `height` 关成 `[]`、路径拼错或目标输入不存在时，提交前报错，不能静默按模板尺寸执行。
- 上述改动仅限原生 ComfyUI 执行路径；Gemini / OpenAI / NovelAI 的尺寸与参数行为不得变化。

**阶段性限制：** 本节只固定实施边界，不代表 §6.3 / C-10 / C-20 已验收。第 1/2/3/4/5 条均已实现并有单测覆盖；仍未完成的是 provider 编排级 mock（C-13，当前只有客户端协议层证据）与真实 smoke（C-15）。**单测覆盖不等于真机验收**，UI 交互与真实出图仍未实测。

**已知行为变化：** 显式传 `size` 现在要求工作流存在可写的 `width` / `height` 输入；放大/修复类工作流（没有 `Empty*LatentImage`）请**省略** `size`，否则会在提交前被拒绝（错误消息会说明原因）。

## 11.6 当前实施进度（2026-11-11）

本节只记录当前分支（`fork-main`，HEAD `ba170e47`）的实施进度，防止把计划项当成已交付能力。状态定义：**已落地** = 本分支已存在实现与对应测试代码（文档项则为已写入本文）；**部分覆盖** = 只有部分子项有实现，或只有测试证据；**未开始** = 本分支尚无对应实现。

| 状态     | 项目                            | 现状与边界                                                                                                                                                                                                                                                                                                                                                          |
| -------- | ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 已落地   | 显式尺寸契约                    | 非法字符串、比例、档位、白名单不匹配均在 ComfyUI 分支提交前报错；显式合法 `WxH` 生效并有落图校验（`paramMap` 关闭、路径拼错、目标节点缺失时提交前拒绝）；省略 `size` 时保留默认，配置默认值非法或被白名单过滤时告警。测试见 `native/src/comfyui/workflow.rs`（代码层校验，不代表真实 smoke 已执行）。                                                                                                      |
| 已落地   | Autogrow 槽位反例测试           | 覆盖「槽位 2 指向更大的节点 4，而无关的 `LoadImage` 节点 ID 更小」的反例：只按槽位连线映射，不按节点 ID 升序；部分槽位失败时保留原槽位编号。测试见 `native/src/comfyui/convert.rs`。                                                                                                                                                                                |
| 已落地   | §11.5 五条约束文档              | 五条实施约束与配套测试要求已写入本文；第 1/2/3/4/5 条均已有对应实现与单测（见增量进展）。                                                                                                                                                                                                                                                                             |
| 已落地   | Autogrow 图片角色消歧（C-18）   | 多 `LoadImage` 无 Autogrow 槽位时，只有用户显式 `paramMap` 同时映射 `image`/`image2` 或 `taskContract.inputs` 覆盖全部请求槽位且 role 合法才放行；自动推导不算解歧。**role 只作放行校验，不参与槽位分配**（见增量进展）。                                                                                                                                            |
| 已落地   | 状态投影（C-11）                | `comfyui-status` 区分 `channelWorkflow` / `modelsWithWorkflow` / `hasExplicitWorkflow`，禁用模型不计数；Readiness 遍历所有启用绑定并报告解析错误。                                                                                                                                                                                                                    |
| 已落地   | PR-1                            | `nodeRequired` / `agentRequired` 分离、`confidence` 由节点定义与当前值决定、negative `exposed:false`（`disabled by paramMap`）均有实现与单测。                                                                                                                                                                                                                       |
| 已落地   | PR-2 其余项                     | `agentControls`/`taskContract` 配置面、三档 exposure、规范化目标锁（含语义别名与顶层参数）、输出族 fail-fast、布局越界拒绝、比例换算与 2% 容差均有实现与单测。                                                                                                                                                                                                     |
| 已落地   | PR-3 其余项                     | workflow library 冲突备份、客户端协议层 mock（C-13 部分）、Workflow Readiness（C-21）已实现；provider 编排级 mock 仍未完成（见增量进展）。                                                                                                                                                                                                                           |
| 未开始   | C-15 真实 smoke                 | 尚未执行真实本地生成；未执行前只能记为未验证/`BLOCKED`，不得记为通过。                                                                                                                                                                                                                                                                                                |

**增量进展（同日后续，优先于上表）：**

- 权限与 exposure：`comfyui/policy.rs` + 模型项 `agentControls`，执行与 describe 共用；locked / 枚举 / 范围 / 标量类型在提交前拒绝；非法 `exposure/min/max/options` **fail-closed**，默认 locked 最多只能由策略升到 advanced。**所有写入口**（顶层 `negativePrompt` / `seed` / `size` / 参考图、`params` 语义名与点号键、随机 seed / 默认尺寸）统一授权；locked 的隐式值保留模板值。
- 目标锁与 raw 输入：`paramMap` 空数组与 locked 语义的目标路径形成锁集合；点号键、语义别名、顶层参数展开后都检查同一集合。未进入语义映射的 raw 点号键（`sampler_name` / `batch_size` / `filename_prefix`）默认拒绝。
- 技术边界：`/object_info` 的 INT/FLOAT/STRING/BOOLEAN、COMBO options、min/max/step 与策略约束同时校验；INT step 按 `(value - min) % step` 检查对齐；2^53 以上整数跳过 f64 范围比较，避免 seed 误判。拿不到 `/object_info` 时仍做内置标量类型校验，但跳过节点边界。
- 布局越界：默认拒绝，`allowLayoutOverflow: true` 显式放行；检查发生在 `apply_extra_params` **之后**，显式 `params.layout` 不能绕过。
- 比例归一化：2% 容差，从 `supportedResolutions` 选候选并回报实际 `WxH`。
- 输出族 fail-fast：只扫描 `Save*` / `Preview*` 终止输出节点；无图片输出 / 视频 / 音频 / 混合媒体在提交前拒绝，中间 `LoadVideo` 等输入节点不参与判定；`taskContract.outputs.adapter=image` 只能为「无法识别」放行，不能覆盖已知混合媒体。
- 多参考图角色消歧：无 Autogrow 槽位且无 `taskContract.inputs` 角色声明时拒绝；要求覆盖全部请求槽位且 role 在 `base|mask|control|reference` 内。**role 只作为放行校验，不参与实际槽位分配**。
- describe 绑定判定：改为**解析后的真实身份**（同一本地文件的 canonical 路径，或同一个画布名 / 模型名精确相等）；不再用 basename 或模型名猜测。单模型渠道也要求显式 workflow 与渠道绑定一致才借用 `paramMap` / `agentControls`。
- C-07 连线保护：语义参数与 `params` 点号键都不能覆盖 `[node, slot]` 连线，写入前保留 wiring。
- C-06 可选输入：点号键目标必须已存在，或在 `/object_info` 的 required/optional 中声明；拼错的键拒绝。
- workflow library 写入：同名文件内容不同先备份 `*.bak.<timestamp>.json`，不静默覆盖。
- Workflow Readiness（C-21）：`comfyui-status.readiness` 遍历渠道级 + 全部启用模型的有效绑定；解析失败写入 `errors[]` 并使 `ok=false`，没有候选时 `ok=null`（不再把「无法解析」伪装成「无缺失」）。仍然只读，不自动安装或下载。
- 前端语义校验（C-19 部分）：`parseAgentControlsJson` 校验 exposure 枚举、min/max 数字与 `min<=max`、options 非空字符串数组、recommended/description 类型；`parseTaskContractJson` 校验 `taskFamily` / `operation` / `inputs[].role` / `outputs.adapter` 枚举。与 Rust `from_json` / `ensure_reference_roles` 对齐，非法 JSON 在保存前拦截。
- 上表「未开始」的 PR-1/PR-2/PR-3 子项中，`nodeRequired` / `agentRequired` / `confidence` 分离、negative `exposed:false`、§11.5 第 5 条（重算 + CFG 判据）、C-06 可选输入、C-07 连线保护、library 同名备份、Workflow Readiness 均已落地。
- provider 生命周期 mock（C-13）：本地 TCP mock 只覆盖客户端方法 `submit / poll / view / queue delete / interrupt`；**未覆盖** `generate_comfyui` 编排、取消流程、参考图上传、图库持久化。`runtime::request_started` 依赖 SQLite 会话上下文（`__snowImageGeneration`），当前测试环境无法安全隔离，因此不能宣称「provider 生命周期已验证」。
- 仍未做：真实 smoke（C-15）、前端完整交互实测、其余 provider 回归（C-14）。

**2026-11-11 复核修正（GPT 6.1 sol 的 R1–R9）：** R1（顶层参数绕锁）、R2（点号键 / 别名绕锁）、R3（类型与节点边界）、R7（工作流身份）、Readiness 假阳性、前端语义校验已按上文落地并有回归测试；R4/R5/R6/R8/R9 属上一轮已修项（自动猜序门禁、输出族白名单、最终布局复检、显式尺寸免白名单 + 像素预算、steps/cfg 推导），本轮回归测试继续通过。C-13 按上文如实降级为「客户端协议层覆盖，provider 编排未覆盖」。

**2026-11-12 复核修正（第二轮，GPT 6.1 sol 的 P1/P2）：** 本轮把检查从「各入口分别补」收敛到「所有写入统一收口」：

- **同目标约束取交集**：`check_param_write` 收集映射到同一目标的所有语义名（点号键不再用 `find()` 只查一个），逐个执行 exposure / 类型 / 范围校验；`steps.max=30` 与更宽松的别名取交集，别名无法放宽。
- **隐式写入也受目标锁约束**：新增 `workflow::filter_locked_paths`，在 `prepare_graph` 入口剔除锁定目标。默认 seed / 默认尺寸 / 布局尺寸同步都不会再改写被锁路径；显式写入仍在调用层直接报错。
- **多图角色门禁只认有效 nodePath**：`explicit_image_slots` 只由「paramMap 显式 image/image2」或 `taskContract.inputs` 中通过 `declared_image_slot_map` 校验的 LoadImage image 路径决定；只写 role 没有 nodePath 不再放行。
- **布局最终检查补全**：省略 `size`（`sync_size=false`）时用模板当前尺寸检查；遍历所有 layout 映射，不再只看第一处。
- **像素预算覆盖全部来源**：新增 `ensure_pixel_budget`，显式 `WxH`、比例候选、模型/渠道默认、模板尺寸统一检查；provider 在最终尺寸确定后再兜底一次。
- **节点边界闭合**：不再因为传入值超过 2^53 就跳过范围检查（仅当边界值本身超 2^53 才放弃该侧比较）；FLOAT 的 step 也按容差校验。
- **参考图上传目标锁**：上传前对每个落点检查规范化目标锁，命中直接报错，不再依赖 `set_value` 静默跳过。
- **describe 与执行对齐**：`ComfyModelEntry` 增加 `task_contract`；describe 用与执行相同的 `declared_image_slot_map` 覆盖图片槽位，避免展示与注入不一致。
- **R7 空绑定修正**：绑定为空时，先比较 fallback（模型名）解析到的本地文件，再判断「显式请求解析成功」；模型名与显式文件名指向同一文件时不再误判为不同。
- **Readiness 不再只看节点类**：增加 `validate_graph` 的坏连线 / 缺必填输入诊断（`structureIssues`，影响 `ok`）；`NoTerminal` 不再静默吞掉，记录到 `warnings`（可能由 taskContract 声明放行，不判死）。
- 回归测试从 149 增至 **156 passed / 0 failed**，新增反例覆盖：同目标别名交集、隐式写入锁、锁定 layout 同步、`sync_size=false` 与多 layout 越界、像素预算三来源、大整数边界、FLOAT step、只有 role 的声明被拒、R7 空绑定 fallback。
- **仍未验证**：真实 smoke（C-15）、provider 编排级 mock（C-13）、前端完整 UI 交互、其余 provider 回归（C-14）。

**2026-11-12 复核修正（第三轮，GPT 6.1 sol 的 P1/P2）：** 本轮继续收口，把「锁 / 预算 / 角色」的判据提升到最终写入图与完整目标集合：

- **顶层 seed 取交集**：`validate_top_level_writes` 与 `params` 路径一致，收集映射到同一目标的所有语义并逐个校验；别名更严格的 max 会生效。
- **锁集合按原始映射计算**：锁集合在 `generate_comfyui` 用**原始** `paramMap` 计算后传入请求；不再用裁剪后的 effective map 重算，避免丢失被锁语义的显式重映射目标。
- **最终尺寸复查**：`PreparedGraph.effective_size` 记录注入后的真实宽高；provider 用它复查像素预算，并按推导映射读取模板宽高再查一次（width/height 映射被关闭时也不会漏）。
- **尺寸成对锁**：`prepare_graph` 中 width/height 任一目标被锁就整体保留模板尺寸，避免只更新一边。
- **整数边界精确化**：`InputBounds` 增加 `min_int` / `max_int`，INT 用 i128 比较；`u64::MAX` 与同值边界精确判等，step 也不依赖 i64。
- **多图槽位覆盖**：`ensure_reference_roles` 要求声明覆盖 `image` / `image2` … `imageN` 的具体语义名，`image` + `image99` 不再冒充 `image2`。
- **Readiness 补全**：候选带「模型是否声明 adapter=image」；`NoTerminal` 在未声明时计入 `outputFamilyIssues`（影响 `ok`），声明后仅记 warning；结构校验新增 COMBO 必填与连线 slot 范围；空 COMBO 选项列表视为「无可用值」而不是无约束。
- **describe 模型选择对齐执行**：只查启用项，先按 model 再按 id；禁用模型不能靠 id 混入。
- **多图门禁收口（补充）**：`explicit_image_slots` 只由「paramMap 覆盖本次请求全部槽位」决定；`taskContract.inputs` 的 nodePath 只参与注入，不再提前置 explicit。两张图只声明一个槽位会被 `ensure_reference_roles` 拒绝。
- 回归测试从 156 增至 **162 passed / 0 failed**，新增反例：顶层 seed 别名交集、原始映射锁、单边尺寸锁、槽位覆盖、`u64` 边界、COMBO 必填与 slot 校验、部分 nodePath 声明。
- **仍未验证**：真实 smoke（C-15）、provider 编排级 mock（C-13）、前端完整 UI 交互、其余 provider 回归（C-14）。

**2026-11-13 非迁移特有风险修正（上游 crate 拆分合并后）：**

- **native 加载器必需导出**：`native/index.cjs` 的 `requiredExports` 补上 `listComfyuiWorkflows` / `describeComfyuiWorkflow` / `convertComfyuiWorkflow`。缺少这些接口的旧 binding 现在会被拒绝加载，而不是运行期报 `not a function`。
- **旧式顶层配置迁移支持 ComfyUI**：`imagegen_scope.rs` 与执行侧 `load_imagegen_settings` 的旧单渠道分支现在识别 `provider=comfyui`——没有 `apiKey` 不再迁移成空渠道，也不会被默认改成 openai；`workflow` / `paramMap` / `models` 一起带过来。
- **任务状态时序**：`runtime::request_started` 从「提交前」移动到「取得并发名额之后」；排队等名额期间任务保持 pending，不再提前显示 running。
- 回归测试 **162 passed / 0 failed**，新增旧式 ComfyUI 迁移与旧式 OpenAI 保持原 provider 的用例；`node --check native/index.cjs` 通过。
- **仍未验证**：旧格式迁移的真实历史配置（依赖本机 DB，未执行真实迁移 smoke）。

**验收矩阵口径：** C-01～C-22 共 22 条。按「有可复核实现/测试证据」计，C-04 / C-05 / C-06 / C-07 / C-09（部分）/ C-10 / C-11 / C-13（部分：仅客户端协议层）/ C-17 / C-18 / C-19（前端往返 + 语义校验）/ C-20 / C-21 有代码或单测证据；C-14（其余 provider 回归）与 C-15（真实 smoke）未验证。**均不等于通过**，UI 交互与真实出图仍未实测。本节只描述实施进度，不改变 §10 各条的通过条件。

## 12. DS 执行前检查单

- [ ] 先读现状说明和本方案，实际代码与本文冲突时以代码为准并更新本文结论。
- [ ] 新增图片模型只需登记 model/workflow 与必要的模型特有默认，不逐个复制常见参数的 `paramMap` / schema。
- [ ] `agentControls` 和稀疏 `taskContract` 都在 `imagegen_settings.channels[].models[]`；不建 sidecar，标准图片工作流不要求逐项配置。
- [ ] 不把 `/object_info.required` 当作 `agentRequired`，也不把节点合法范围当模型推荐范围。
- [ ] 不默认把所有可写 widget 交给 Agent；raw node path 不是普通 Agent API。
- [ ] 不按 node ID 顺序把多个 `LoadImage` 猜成 base/reference/mask/control 角色；角色不明就拒绝并说明。
- [ ] 图片 adapter 不接受视频/音频/3D/混合或不明输出；必须在 `/prompt` 前 fail-fast，不能烧完 GPU 再报无图片。
- [ ] 不把 ComfyUI 已能运行任意图误称为 Snow 已支持任意任务家族；新家族需 task adapter 与 typed input/output/artifact 回显。
- [ ] Anima negative 可覆盖且模板值为默认；保护用户已自定义的 `customPrompt`。
- [ ] 比例容差按 §6.3 的 2% 公式计算，归一化时报告有效 `WxH`，不静默近似。
- [ ] 不静默丢弃明确的步骤、CFG、尺寸、负向词或布局请求。
- [ ] 不覆盖连线、不破坏 library 模板；执行只修改副本。
- [ ] 按 PR-1 / PR-2 / PR-3 拆分；C-13 为 P1，不阻塞两个 P0 PR。
- [ ] 真实 smoke 若硬件不可用标 `BLOCKED`，不冒充通过或失败；保持其他 provider 行为不变并提供可复核测试证据。
