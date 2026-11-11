import {
  DEFAULT_IMAGE_GEN_CHANNEL,
  DEFAULT_IMAGE_GEN_MAX_CONCURRENT,
  DEFAULT_IMAGE_GEN_TIMEOUT_SECS,
  DEFAULT_IMAGE_GEN_EXECUTION_MODE,
  IMAGE_GEN_MAX_CONCURRENT_RANGE,
  IMAGE_GEN_TIMEOUT_RANGE,
} from "./constants";
import type {
  ImageGenAgentControls,
  ImageGenAgentExposure,
  ImageGenChannelValue,
  ImageGenExecutionMode,
  ImageGenModelItem,
  ImageGenProvider,
  ImageGenSettingsForm,
  ImageGenSettingsValue,
  ImageGenTaskContract,
  ImageGenTaskOperation,
} from "./types";

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

const toBoolean = (value: unknown, fallback: boolean): boolean =>
  typeof value === "boolean" ? value : fallback;

const toText = (value: unknown, fallback: string): string =>
  typeof value === "string" ? value : fallback;

/** 解析最大并发生成数：非法/缺失回退默认值，超出范围时收敛到边界。 */
const toConcurrentCount = (value: unknown): number => {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    return DEFAULT_IMAGE_GEN_MAX_CONCURRENT;
  }
  const { min, max } = IMAGE_GEN_MAX_CONCURRENT_RANGE;
  return Math.min(max, Math.max(min, Math.round(value)));
};

/** 解析生图请求超时（秒）：非法/缺失回退默认值，超出范围时收敛到边界。 */
const toTimeoutSecs = (value: unknown): number => {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    return DEFAULT_IMAGE_GEN_TIMEOUT_SECS;
  }
  const { min, max } = IMAGE_GEN_TIMEOUT_RANGE;
  return Math.min(max, Math.max(min, Math.round(value)));
};

const readProvider = (
  value: unknown,
  fallback: ImageGenProvider,
): ImageGenProvider => {
  // 必须原样保留 comfyui：只认 gemini/novelai 会把原生渠道读成 openai，
  // 「服务商」列显示错误，且保存时把 provider 写坏。
  if (
    value === "comfyui" ||
    value === "gemini" ||
    value === "novelai" ||
    value === "openai"
  ) {
    return value;
  }
  return fallback;
};

/** 解析生图执行模式：合法值 auto/sync/async，非法回退默认 auto。 */
export const readExecutionMode = (
  value: unknown,
  fallback: ImageGenExecutionMode = DEFAULT_IMAGE_GEN_EXECUTION_MODE,
): ImageGenExecutionMode => {
  if (value === "sync" || value === "async" || value === "auto") {
    return value;
  }
  return fallback;
};

/**
 * ComfyUI 工作流标识。
 *
 * 只保存**逻辑标识**（工作流库里的文件名 / ComfyUI 画布工作流名 / 内联 JSON）：
 * 真实磁盘路径由 Rust 侧按 `<userData>/comfyui-workflows` 解析，换机器/换安装目录
 * 都不用改配置。老配置里的绝对路径在这里归一成文件名，保证向后兼容。
 */
export const toComfyWorkflow = (value: unknown): string => {
  const text = toText(value, "").trim();
  if (!text || text.startsWith("{")) {
    return text;
  }
  if (/comfyui-workflows/i.test(text)) {
    return text.split(/[\\/]/).pop() ?? text;
  }
  return text;
};

/** 空 JSON object 视为「未设置」：不落盘、不生成空声明。 */
const hasKeys = (value: Record<string, unknown>): boolean =>
  Object.keys(value).length > 0;

/**
 * 规范化模型项的 agentControls。
 *
 * 磁盘契约是**结构化 JSON object**：这里只接受普通对象并原样保留内部键
 * （不做白名单裁剪，避免后端新增的控制字段被前端保存时吃掉）。空对象 /
 * 非对象一律视为未设置。
 */
export const normalizeAgentControls = (
  value: unknown,
): ImageGenAgentControls | undefined => {
  if (!isRecord(value) || !hasKeys(value)) {
    return undefined;
  }
  return { ...value } as unknown as ImageGenAgentControls;
};

/** 规范化模型项的 taskContract（规则同 agentControls）。 */
export const normalizeTaskContract = (
  value: unknown,
): ImageGenTaskContract | undefined => {
  if (!isRecord(value) || !hasKeys(value)) {
    return undefined;
  }
  return { ...value } as unknown as ImageGenTaskContract;
};

/** 把结构化 JSON object 转成 textarea 文本；未设置时返回空串（读入即 pretty-print）。 */
export const formatJsonObject = (value: unknown): string => {
  if (!isRecord(value) || !hasKeys(value)) {
    return "";
  }
  return JSON.stringify(value, null, 2);
};

/** 高级 JSON 字段的解析结果：value 为 undefined 表示「留空 = 不声明」。 */
export type AdvancedJsonParseResult<T> =
  | { ok: true; value: T | undefined }
  | { ok: false; error: string };

const TASK_CONTRACT_OPERATIONS: readonly ImageGenTaskOperation[] = [
  "t2i",
  "i2i",
  "upscale",
  "edit",
];

const AGENT_EXPOSURES: readonly ImageGenAgentExposure[] = [
  "basic",
  "advanced",
  "locked",
];

/** 与 Rust `ensure_reference_roles` 的 role 枚举保持一致。 */
const TASK_CONTRACT_INPUT_ROLES = ["base", "mask", "control", "reference"] as const;

/** 当前 image provider 只接受图片输出。 */
const TASK_CONTRACT_OUTPUT_ADAPTERS = ["image"] as const;

const isFiniteNumber = (value: unknown): value is number =>
  typeof value === "number" && Number.isFinite(value);

/**
 * 校验 agentControls，规则与 Rust `ControlPolicy::from_json` 的 fail-closed 对齐。
 *
 * 返回 undefined 表示通过；否则返回可直接展示的错误。后端会在提交前再次
 * 校验，这里只是把「保存后才发现非法」提前到保存前。
 */
const validateAgentControls = (
  value: Record<string, unknown>,
): string | undefined => {
  for (const [semantic, raw] of Object.entries(value)) {
    if (!isRecord(raw)) {
      return `agentControls["${semantic}"] 必须是 JSON object`;
    }
    if (raw.exposure !== undefined) {
      if (
        typeof raw.exposure !== "string" ||
        !AGENT_EXPOSURES.includes(raw.exposure as ImageGenAgentExposure)
      ) {
        return `agentControls["${semantic}"].exposure 只能是 "basic" | "advanced" | "locked"`;
      }
    }
    for (const field of ["min", "max"] as const) {
      if (raw[field] !== undefined && !isFiniteNumber(raw[field])) {
        return `agentControls["${semantic}"].${field} 必须是数字`;
      }
    }
    const min = isFiniteNumber(raw.min) ? raw.min : undefined;
    const max = isFiniteNumber(raw.max) ? raw.max : undefined;
    if (min !== undefined && max !== undefined && min > max) {
      return `agentControls["${semantic}"].min 必须小于等于 max（当前 ${min} > ${max}）`;
    }
    if (raw.options !== undefined) {
      if (!Array.isArray(raw.options) || raw.options.length === 0) {
        return `agentControls["${semantic}"].options 必须是非空字符串数组`;
      }
      for (const option of raw.options) {
        if (typeof option !== "string" || !option.trim()) {
          return `agentControls["${semantic}"].options 的每一项必须是非空字符串`;
        }
      }
    }
    for (const field of ["recommended", "description"] as const) {
      if (raw[field] !== undefined && typeof raw[field] !== "string") {
        return `agentControls["${semantic}"].${field} 必须是字符串`;
      }
    }
  }
  return undefined;
};

/** 校验 taskContract 的枚举字段；数量类约束（角色覆盖全部槽位）留给执行期。 */
const validateTaskContract = (
  value: Record<string, unknown>,
): string | undefined => {
  if (value.taskFamily !== undefined && value.taskFamily !== "image") {
    return 'taskContract.taskFamily 只能是 "image"';
  }
  if (
    value.operation !== undefined &&
    !TASK_CONTRACT_OPERATIONS.includes(
      value.operation as ImageGenTaskOperation,
    )
  ) {
    return 'taskContract.operation 只能是 "t2i" | "i2i" | "upscale" | "edit"';
  }
  if (value.inputs !== undefined) {
    if (!isRecord(value.inputs)) {
      return "taskContract.inputs 必须是 JSON object";
    }
    for (const [slot, entry] of Object.entries(value.inputs)) {
      if (!isRecord(entry)) {
        return `taskContract.inputs["${slot}"] 必须是 JSON object`;
      }
      if (
        typeof entry.role !== "string" ||
        !TASK_CONTRACT_INPUT_ROLES.includes(
          entry.role as (typeof TASK_CONTRACT_INPUT_ROLES)[number],
        )
      ) {
        return `taskContract.inputs["${slot}"].role 只能是 "base" | "mask" | "control" | "reference"`;
      }
      if (entry.nodePath !== undefined && typeof entry.nodePath !== "string") {
        return `taskContract.inputs["${slot}"].nodePath 必须是字符串`;
      }
    }
  }
  if (value.outputs !== undefined) {
    if (!isRecord(value.outputs)) {
      return "taskContract.outputs 必须是 JSON object";
    }
    if (
      !TASK_CONTRACT_OUTPUT_ADAPTERS.includes(
        value.outputs.adapter as (typeof TASK_CONTRACT_OUTPUT_ADAPTERS)[number],
      )
    ) {
      return 'taskContract.outputs.adapter 只能是 "image"';
    }
  }
  return undefined;
};

/**
 * 解析 agentControls textarea 文本。
 *
 * 只要求是普通 JSON object；内部结构原样保留（后端负责解释具体参数）。
 */
export const parseAgentControlsJson = (
  text: string,
): AdvancedJsonParseResult<ImageGenAgentControls> => {
  const trimmed = text.trim();
  if (!trimmed) {
    return { ok: true, value: undefined };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(trimmed);
  } catch {
    return {
      ok: false,
      error: "agentControls 必须是合法的 JSON object",
    };
  }
  if (!isRecord(parsed)) {
    return {
      ok: false,
      error:
        "agentControls 必须是 JSON object，不能是数组、字符串、数字或 null",
    };
  }
  if (!hasKeys(parsed)) {
    return { ok: true, value: undefined };
  }
  const problem = validateAgentControls(parsed as Record<string, unknown>);
  if (problem) {
    return { ok: false, error: problem };
  }
  return { ok: true, value: parsed as unknown as ImageGenAgentControls };
};

/**
 * 解析 taskContract textarea 文本。
 *
 * 除 object 形状外，taskFamily / operation 如存在必须是约定枚举 —— 否则
 * 后端拿到的契约无法路由，保存时直接拦下比静默写坏更安全。
 */
export const parseTaskContractJson = (
  text: string,
): AdvancedJsonParseResult<ImageGenTaskContract> => {
  const trimmed = text.trim();
  if (!trimmed) {
    return { ok: true, value: undefined };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(trimmed);
  } catch {
    return {
      ok: false,
      error: "taskContract 必须是合法的 JSON object",
    };
  }
  if (!isRecord(parsed)) {
    return {
      ok: false,
      error:
        "taskContract 必须是 JSON object，不能是数组、字符串、数字或 null",
    };
  }
  if (!hasKeys(parsed)) {
    return { ok: true, value: undefined };
  }
  const problem = validateTaskContract(parsed as Record<string, unknown>);
  if (problem) {
    return { ok: false, error: problem };
  }
  return { ok: true, value: parsed as unknown as ImageGenTaskContract };
};

/** 解析单个模型的配置。 */
export const readModelItem = (source: unknown): ImageGenModelItem => {
  if (!isRecord(source)) {
    return { id: "", model: "" };
  }
  // ComfyUI 原生方案的两个稀疏字段：磁盘里是结构化 object，空对象/缺失
  // 一律归一为 undefined（不写）；非空则原样带上，避免保存时被吃掉。
  const agentControls = normalizeAgentControls(source.agentControls);
  const taskContract = normalizeTaskContract(source.taskContract);
  return {
    id: toText(source.id, ""),
    model: toText(source.model, ""),
    name: toText(source.name, ""),
    defaultSize: toText(source.defaultSize, ""),
    defaultQuality: toText(source.defaultQuality, ""),
    defaultComplexity: toText(source.defaultComplexity, ""),
    defaultOutputFormat: toText(source.defaultOutputFormat, ""),
    supportedRatios: toText(source.supportedRatios, ""),
    supportedResolutions: toText(source.supportedResolutions, ""),
    supportedThinking: toText(source.supportedThinking, ""),
    supportedQualities: toText(source.supportedQualities, ""),
    customPrompt: toText(source.customPrompt, ""),
    // 工作流后端的参数声明；丢掉它，工具描述里的参数清单就永远是空的。
    paramsSchema: toText(source.paramsSchema, ""),
    // 每模型一个 ComfyUI 工作流：丢掉这两项 = 一个渠道挂多个工作流的能力失效。
    workflow: toComfyWorkflow(source.workflow),
    paramMap: toText(source.paramMap, ""),
    // 稀疏字段：只有非空 object 才出现在归一化结果里。
    ...(agentControls ? { agentControls } : {}),
    ...(taskContract ? { taskContract } : {}),
    enabled: source.enabled !== false,
  };
};

/** 解析多模型列表。 */
export const readModels = (source: unknown): ImageGenModelItem[] => {
  if (!Array.isArray(source)) {
    return [];
  }
  return source.map(readModelItem).filter((m) => m.model.trim().length > 0);
};

/** 解析单个渠道的原始字段。 */
const readChannel = (source: unknown): ImageGenChannelValue => {
  if (!isRecord(source)) {
    return { ...DEFAULT_IMAGE_GEN_CHANNEL };
  }
  const models = readModels(source.models);
  return {
    id: toText(source.id, DEFAULT_IMAGE_GEN_CHANNEL.id),
    name: toText(source.name, DEFAULT_IMAGE_GEN_CHANNEL.name),
    provider: readProvider(source.provider, DEFAULT_IMAGE_GEN_CHANNEL.provider),
    enabled: toBoolean(source.enabled, DEFAULT_IMAGE_GEN_CHANNEL.enabled),
    baseUrl: toText(source.baseUrl, DEFAULT_IMAGE_GEN_CHANNEL.baseUrl),
    apiKey: toText(source.apiKey, DEFAULT_IMAGE_GEN_CHANNEL.apiKey),
    model: toText(source.model, DEFAULT_IMAGE_GEN_CHANNEL.model),
    defaultSize: toText(
      source.defaultSize,
      DEFAULT_IMAGE_GEN_CHANNEL.defaultSize,
    ),
    defaultQuality: toText(
      source.defaultQuality,
      DEFAULT_IMAGE_GEN_CHANNEL.defaultQuality,
    ),
    outputFormat: toText(
      source.outputFormat,
      DEFAULT_IMAGE_GEN_CHANNEL.outputFormat,
    ),
    webSearch: toBoolean(source.webSearch, DEFAULT_IMAGE_GEN_CHANNEL.webSearch),
    defaultStream: toBoolean(
      source.defaultStream,
      DEFAULT_IMAGE_GEN_CHANNEL.defaultStream,
    ),
    supportedRatios: toText(
      source.supportedRatios,
      DEFAULT_IMAGE_GEN_CHANNEL.supportedRatios ?? "",
    ),
    supportedResolutions: toText(
      source.supportedResolutions,
      DEFAULT_IMAGE_GEN_CHANNEL.supportedResolutions ?? "",
    ),
    supportedThinking: toText(
      source.supportedThinking,
      DEFAULT_IMAGE_GEN_CHANNEL.supportedThinking ?? "",
    ),
    supportedQualities: toText(
      source.supportedQualities,
      DEFAULT_IMAGE_GEN_CHANNEL.supportedQualities ?? "",
    ),
    customPrompt: toText(
      source.customPrompt,
      DEFAULT_IMAGE_GEN_CHANNEL.customPrompt ?? "",
    ),
    // 本地工作流端点靠它把 params 透传给上游；UI 往返丢失 = 整个特性静默失效。
    allowExtraParams: toBoolean(
      source.allowExtraParams,
      DEFAULT_IMAGE_GEN_CHANNEL.allowExtraParams === true,
    ),
    // ComfyUI 渠道的默认工作流（多模型渠道里作为未单独配置模型的兜底）；
    // 丢掉它 = 面板保存一次，工作流就没了。
    workflow: toComfyWorkflow(source.workflow),
    paramMap: toText(source.paramMap, ""),
    executionMode:
      source.executionMode === "sync" ||
      source.executionMode === "async" ||
      source.executionMode === "auto"
        ? source.executionMode
        : undefined,
    models: models.length > 0 ? models : undefined,
  };
};

/** 生成新的渠道 ID（前端使用；旧数据无 id 或新建渠道时调用）。 */
export const generateChannelId = (
  provider: ImageGenProvider,
  index = 0,
): string => {
  const random = Math.random().toString(36).slice(2, 8);
  return `${provider}-${random}${index > 0 ? `-${index}` : ""}`;
};

/** 迁移旧格式渠道（旧双渠道键 "openai"/"gemini" 即协议类型）。 */
const migrateLegacyChannel = (
  provider: ImageGenProvider,
  source: unknown,
): ImageGenChannelValue => {
  const channel = readChannel(source);
  if (!channel.id) {
    channel.id = provider;
  }
  channel.provider = provider;
  return channel;
};

/** 将 system_settings 原始 JSON 字符串解析为规范化设置（含旧格式迁移）。 */
export const readImageGenSettingsJson = (
  raw: string | null,
): ImageGenSettingsValue => {
  if (!raw) {
    return {
      channels: [],
      maxConcurrentImages: DEFAULT_IMAGE_GEN_MAX_CONCURRENT,
      timeoutSecs: DEFAULT_IMAGE_GEN_TIMEOUT_SECS,
      executionMode: DEFAULT_IMAGE_GEN_EXECUTION_MODE,
    };
  }
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!isRecord(parsed)) {
      return {
        channels: [],
        maxConcurrentImages: DEFAULT_IMAGE_GEN_MAX_CONCURRENT,
        timeoutSecs: DEFAULT_IMAGE_GEN_TIMEOUT_SECS,
        executionMode: DEFAULT_IMAGE_GEN_EXECUTION_MODE,
      };
    }

    // 新版多渠道格式：{ channels: [...] }
    if (Array.isArray(parsed.channels)) {
      const channels = parsed.channels.map((item, index) => {
        const channel = readChannel(item);
        if (!channel.id) {
          channel.id = generateChannelId(channel.provider, index);
        }
        return channel;
      });
      return {
        channels,
        maxConcurrentImages: toConcurrentCount(parsed.maxConcurrentImages),
        timeoutSecs: toTimeoutSecs(parsed.timeoutSecs),
        executionMode: readExecutionMode(parsed.executionMode),
      };
    }

    // 旧版双渠道格式：{ openai: {...}, gemini: {...} }
    if (parsed.openai || parsed.gemini) {
      const channels: ImageGenChannelValue[] = [];
      if (parsed.openai) {
        channels.push(migrateLegacyChannel("openai", parsed.openai));
      }
      if (parsed.gemini) {
        channels.push(migrateLegacyChannel("gemini", parsed.gemini));
      }
      return {
        channels,
        maxConcurrentImages: DEFAULT_IMAGE_GEN_MAX_CONCURRENT,
        timeoutSecs: DEFAULT_IMAGE_GEN_TIMEOUT_SECS,
        executionMode: readExecutionMode(parsed.executionMode),
      };
    }

    // 旧版单渠道格式迁移：顶层 provider/baseUrl/apiKey/model/...
    const oldProvider = toText(parsed.provider, "");
    const oldBaseUrl = toText(parsed.baseUrl, "");
    const isGemini =
      oldProvider === "gemini" ||
      oldBaseUrl.includes("generativelanguage") ||
      oldBaseUrl.includes("googleapis.com");
    const legacy = migrateLegacyChannel(isGemini ? "gemini" : "openai", {
      ...parsed,
      enabled: true,
    });
    return {
      channels: [legacy],
      maxConcurrentImages: DEFAULT_IMAGE_GEN_MAX_CONCURRENT,
      timeoutSecs: DEFAULT_IMAGE_GEN_TIMEOUT_SECS,
      executionMode: readExecutionMode(parsed.executionMode),
    };
  } catch {
    return {
      channels: [],
      maxConcurrentImages: DEFAULT_IMAGE_GEN_MAX_CONCURRENT,
      timeoutSecs: DEFAULT_IMAGE_GEN_TIMEOUT_SECS,
      executionMode: DEFAULT_IMAGE_GEN_EXECUTION_MODE,
    };
  }
};

/**
 * 存储值 -> 表单（复制渠道数组，模型项用展开保留全部稀疏字段）。
 *
 * 模型项不重建字段：agentControls / taskContract 这类新增稀疏字段
 * 必须原样跟着走，避免「读进表单就丢」。
 */
export const toImageGenForm = (
  settings: ImageGenSettingsValue,
): ImageGenSettingsForm => ({
  channels: settings.channels.map((channel) => ({ ...channel })),
  maxConcurrentImages: settings.maxConcurrentImages,
  timeoutSecs: settings.timeoutSecs,
  executionMode: settings.executionMode,
});

/** 表单 -> 存储 JSON 字符串（channels 数组 + 最大并发生成数 + 执行模式）。 */
export const toImageGenSettingsJson = (form: ImageGenSettingsForm): string =>
  JSON.stringify({
    channels: form.channels.map((channel) => ({
      id: channel.id.trim(),
      name: channel.name.trim(),
      provider: channel.provider,
      enabled: channel.enabled,
      baseUrl: channel.baseUrl.trim(),
      apiKey: channel.apiKey.trim(),
      model: channel.model.trim(),
      defaultSize: channel.defaultSize.trim(),
      defaultQuality: channel.defaultQuality.trim(),
      outputFormat: channel.outputFormat.trim(),
      webSearch: channel.webSearch,
      defaultStream: channel.defaultStream,
      supportedRatios: (channel.supportedRatios ?? "").trim(),
      supportedResolutions: (channel.supportedResolutions ?? "").trim(),
      supportedThinking: (channel.supportedThinking ?? "").trim(),
      supportedQualities: (channel.supportedQualities ?? "").trim(),
      customPrompt: (channel.customPrompt ?? "").trim(),
      // 存回去时必须带上，否则 UI 保存一次就把透传开关抹成默认 false。
      allowExtraParams: channel.allowExtraParams === true,
      // ComfyUI 工作流绑定与参数映射：读侧一直在解析这两项，写侧漏掉 =
      // 用户选好工作流后保存一次就全部丢失（渠道级与模型级同罪）。
      workflow: toComfyWorkflow(channel.workflow),
      paramMap: (channel.paramMap ?? "").trim(),
      executionMode: channel.executionMode,
      models:
        channel.models && channel.models.length > 0
          ? channel.models
              .filter((m) => m.model.trim().length > 0)
              .map((m) => ({
                id: m.id || m.model.trim(),
                model: m.model.trim(),
                name: (m.name ?? "").trim(),
                defaultSize: (m.defaultSize ?? "").trim(),
                defaultQuality: (m.defaultQuality ?? "").trim(),
                defaultComplexity: (m.defaultComplexity ?? "").trim(),
                defaultOutputFormat: (m.defaultOutputFormat ?? "").trim(),
                supportedRatios: (m.supportedRatios ?? "").trim(),
                supportedResolutions: (m.supportedResolutions ?? "").trim(),
                supportedThinking: (m.supportedThinking ?? "").trim(),
                supportedQualities: (m.supportedQualities ?? "").trim(),
                customPrompt: (m.customPrompt ?? "").trim(),
                paramsSchema: (m.paramsSchema ?? "").trim(),
                // 每模型独立工作流：丢掉它 = 一个渠道挂多个 ComfyUI 工作流的能力失效。
                workflow: toComfyWorkflow(m.workflow),
                paramMap: (m.paramMap ?? "").trim(),
                // ComfyUI 原生方案：结构化 object 原样写回；空/未设置由
                // normalize 返回 undefined，JSON.stringify 自动省略键。
                agentControls: normalizeAgentControls(m.agentControls),
                taskContract: normalizeTaskContract(m.taskContract),
                enabled: m.enabled !== false,
              }))
          : undefined,
    })),
    maxConcurrentImages: toConcurrentCount(form.maxConcurrentImages),
    timeoutSecs: toTimeoutSecs(form.timeoutSecs),
    executionMode: readExecutionMode(form.executionMode),
  });
