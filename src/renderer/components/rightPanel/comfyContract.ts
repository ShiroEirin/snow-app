/**
 * 绘图工作台的 ComfyUI 实时契约。
 *
 * 工作台与 Agent 读**同一个入口**：`comfyui-describe` 内置工具（解析顺序、
 * paramMap、agentControls、taskContract 与 `imagegen-generate` 执行路径共用）。
 * 不再读设置页保存的 `paramsSchema` 快照 —— 快照会过期，而且 4 个模型项实际
 * 都没有快照，工作台此前只能退回「只有正面词 + 1 张参考图」的保守基线。
 *
 * 本模块做三件事：取契约、把契约翻译成工作台控件描述、把「提交侧的确定性
 * 规则」（缓存键 / 种子 / 工作流白名单）收成纯函数。控件可写性、范围、推荐值
 * 全部来自契约（范围已是「节点边界 ∩ agentControls」），这里不加自己的规则；
 * 纯函数部分则刻意不碰 React，便于用 `npm run check:drawing` 直接断言。
 */

import type { ModelCapabilities } from "./modelCapabilities";

/** 契约里的一个语义输入（只读子集，字段与 Rust `ContractInput` 一致）。 */
export type ComfyContractInput = {
  name: string;
  valueType: string;
  exposure: string;
  exposed: boolean;
  resolvedFrom?: string;
  description?: string;
  recommended?: string | null;
  example?: unknown;
  schema?: {
    type?: string;
    minimum?: number;
    maximum?: number;
    multipleOf?: number;
    enum?: unknown[];
  } | null;
};

/** `comfyui-describe` 返回的契约。 */
export type ComfyContract = {
  inputs: ComfyContractInput[];
  warnings: string[];
};

/** 工作台可渲染的单个标量控件（数值输入或枚举下拉）。 */
export type ComfyScalarControl = {
  name: string;
  kind: "integer" | "number" | "enum";
  /** basic = 参数栏；advanced = 高级参数面板。 */
  exposure: "basic" | "advanced";
  min?: number;
  max?: number;
  step?: number;
  options?: string[];
  /** 模板当前值（作为占位符展示「留空即沿用」的实际值）。 */
  templateValue?: string;
  recommended?: string;
  description?: string;
};

/** 契约翻译出的工作台能力。 */
export type ComfyWorkbenchControls = {
  /** 能力覆盖（并入 `ModelCapabilities`，与执行侧同源）。 */
  capabilities: Partial<ModelCapabilities>;
  /** width / height 都可写：显示尺寸选择。 */
  sizeWritable: boolean;
  /** negative 可写：显示负向词输入（留空沿用模板）。 */
  negativeWritable: boolean;
  /** 模板负向词（占位符展示）。 */
  negativeTemplate: string;
  /** 其余可写标量（steps / cfg / 将来开放的 sampler 等），通过 `params` 语义键提交。 */
  scalars: ComfyScalarControl[];
};

/** 由顶层参数或专用控件承载的语义：不进通用标量列表。 */
const DEDICATED_SEMANTICS = new Set([
  "prompt",
  "negative",
  "seed",
  "width",
  "height",
]);

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

/**
 * 从工具返回文本里找出 describe 载荷。
 *
 * 兼容两种外形：工具直接返回的 JSON，以及 MCP `content[].text` 包装。
 */
const findDescribePayload = (
  value: unknown,
  depth = 0,
): Record<string, unknown> | null => {
  if (depth > 3) {
    return null;
  }
  if (typeof value === "string") {
    try {
      return findDescribePayload(JSON.parse(value), depth + 1);
    } catch {
      return null;
    }
  }
  if (!isRecord(value)) {
    return null;
  }
  if (isRecord(value.contract)) {
    return value;
  }
  if (Array.isArray(value.content)) {
    for (const block of value.content) {
      if (isRecord(block) && typeof block.text === "string") {
        const found = findDescribePayload(block.text, depth + 1);
        if (found) {
          return found;
        }
      }
    }
  }
  return null;
};

/** 解析 describe 返回文本；拿不到合法契约时返回 null（调用方退回保守基线）。 */
export const parseComfyContract = (raw: string): ComfyContract | null => {
  const payload = findDescribePayload(raw);
  const contract = payload?.contract;
  if (!isRecord(contract) || !Array.isArray(contract.inputs)) {
    return null;
  }
  const inputs: ComfyContractInput[] = [];
  for (const item of contract.inputs) {
    if (!isRecord(item) || typeof item.name !== "string") {
      continue;
    }
    inputs.push({
      name: item.name,
      valueType: typeof item.valueType === "string" ? item.valueType : "",
      exposure: typeof item.exposure === "string" ? item.exposure : "locked",
      exposed: item.exposed === true,
      resolvedFrom:
        typeof item.resolvedFrom === "string" ? item.resolvedFrom : undefined,
      description:
        typeof item.description === "string" ? item.description : undefined,
      recommended:
        typeof item.recommended === "string" ? item.recommended : null,
      example: item.example,
      schema: isRecord(item.schema)
        ? (item.schema as ComfyContractInput["schema"])
        : null,
    });
  }
  const warnings = Array.isArray(contract.warnings)
    ? contract.warnings.filter(
        (warning): warning is string => typeof warning === "string",
      )
    : [];
  return { inputs, warnings };
};

/**
 * 读取一个 (渠道, 模型) 的实时契约。
 *
 * 走 `comfyui-describe`：与 Agent 看到的是同一份推导结果。失败（ComfyUI 未启动、
 * 工作流缺失）返回 null，而不是抛错 —— 工作台照常可用，只是退回保守能力。
 */
export const fetchComfyContract = async (
  channelId: string,
  model: string,
): Promise<ComfyContract | null> => {
  try {
    const raw = await window.snow.callMcpTool(
      "comfyui-describe",
      JSON.stringify({ server: channelId, model }),
    );
    return parseComfyContract(raw);
  } catch {
    return null;
  }
};

/** 可写 = 语义仍暴露且策略未锁定（与 Rust `exposed = exposure != Locked` 一致）。 */
const isWritable = (input: ComfyContractInput | undefined): boolean =>
  !!input && input.exposed && input.exposure !== "locked";

const finiteNumber = (value: unknown): number | undefined =>
  typeof value === "number" && Number.isFinite(value) ? value : undefined;

const templateText = (value: unknown): string | undefined => {
  if (typeof value === "string") {
    return value;
  }
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  return undefined;
};

/** 把一个契约输入翻译成标量控件；结构化 / 自由文本输入返回 null（交给 Agent）。 */
const toScalarControl = (
  input: ComfyContractInput,
): ComfyScalarControl | null => {
  const schema = input.schema ?? {};
  const exposure = input.exposure === "basic" ? "basic" : "advanced";
  const common = {
    name: input.name,
    exposure,
    templateValue: templateText(input.example),
    recommended: input.recommended ?? undefined,
    description: input.description,
  } as const;
  const options = Array.isArray(schema.enum)
    ? schema.enum.filter((item): item is string => typeof item === "string")
    : [];
  if (options.length > 0) {
    return { ...common, kind: "enum", options };
  }
  const type = schema.type ?? input.valueType;
  if (type !== "integer" && type !== "number") {
    return null;
  }
  return {
    ...common,
    kind: type,
    min: finiteNumber(schema.minimum),
    max: finiteNumber(schema.maximum),
    step: finiteNumber(schema.multipleOf) ?? (type === "integer" ? 1 : 0.1),
  };
};

/**
 * 契约 → 工作台控件。
 *
 * 参考图上限：多个 image 槽位只有全部由 paramMap / taskContract 显式落点时
 * 才放开到槽位数；否则执行侧 `ensure_reference_roles` 只接受 1 张。
 */
export const deriveComfyWorkbenchControls = (
  contract: ComfyContract,
): ComfyWorkbenchControls => {
  const byName = new Map(contract.inputs.map((input) => [input.name, input]));
  let imageSlots = 0;
  let explicitImageSlots = true;
  const scalars: ComfyScalarControl[] = [];
  for (const input of contract.inputs) {
    if (!isWritable(input)) {
      continue;
    }
    if (input.name === "image" || /^image\d+$/.test(input.name)) {
      imageSlots += 1;
      if (input.resolvedFrom !== "paramMap") {
        explicitImageSlots = false;
      }
      continue;
    }
    if (DEDICATED_SEMANTICS.has(input.name)) {
      continue;
    }
    const control = toScalarControl(input);
    if (control) {
      scalars.push(control);
    }
  }
  const maxReferenceImages =
    imageSlots === 0 ? 0 : explicitImageSlots ? imageSlots : 1;
  const negative = byName.get("negative");
  return {
    capabilities: {
      supportsSeed: isWritable(byName.get("seed")),
      supportsReference: maxReferenceImages > 0,
      maxReferenceImages,
    },
    sizeWritable:
      isWritable(byName.get("width")) && isWritable(byName.get("height")),
    negativeWritable: isWritable(negative),
    negativeTemplate: templateText(negative?.example) ?? "",
    scalars,
  };
};

/**
 * 校验并转换一个标量控件的输入值。
 *
 * 返回 `{ value }` 表示可提交；`{ error }` 表示越界 / 非法（提交前拦截，
 * 文案与执行侧同一组边界）；空字符串返回 `{}`（不传 = 沿用模板）。
 */
export const coerceScalarValue = (
  control: ComfyScalarControl,
  raw: string,
): { value?: string | number; error?: string } => {
  const text = raw.trim();
  if (text === "") {
    return {};
  }
  if (control.kind === "enum") {
    return control.options?.includes(text)
      ? { value: text }
      : { error: `${control.name} 不在可选值内` };
  }
  const number = Number(text);
  if (!Number.isFinite(number)) {
    return { error: `${control.name} 不是数字` };
  }
  if (control.kind === "integer" && !Number.isInteger(number)) {
    return { error: `${control.name} 必须是整数` };
  }
  if (control.min !== undefined && number < control.min) {
    return { error: `${control.name} 不能小于 ${control.min}` };
  }
  if (control.max !== undefined && number > control.max) {
    return { error: `${control.name} 不能大于 ${control.max}` };
  }
  return { value: number };
};

/**
 * 工作台「留空随机」使用的种子范围（与聊天卡片「生成变体」一致）。
 *
 * **这是工作台层的交互决定，不是 API 语义**：工具调用省略 `seed` 时后端仍
 * 沿用模板 seed（Agent 复现依赖这条），只有工作台点「生成」时才会主动注入
 * 一个随机值 —— 否则模板 seed 固定时同一提示词永远出同一张图。
 */
export const randomWorkflowSeed = (): number =>
  Math.floor(Math.random() * 2_147_483_647);

/**
 * 显式种子输入 → 提交值。
 *
 * 只接受非负安全整数。此前用 `Number()` + `Math.round()` 提交，超过
 * `2^53 − 1` 的种子会被静默舍入：界面显示「已固定」，实际提交的是另一个值。
 * 这里宁可拒绝，也不提交变形值；空字符串返回 `{}`（「留空」的语义由调用方定）。
 */
export const coerceSeedValue = (
  raw: string,
): { value?: number; error?: string } => {
  const text = raw.trim();
  if (text === "") {
    return {};
  }
  if (!/^\d+$/.test(text)) {
    return { error: `种子必须是非负整数（当前「${text}」）` };
  }
  const value = Number(text);
  if (!Number.isSafeInteger(value)) {
    return {
      error: `种子超出安全整数范围（最大 ${Number.MAX_SAFE_INTEGER}），已保留输入但不会提交；请改用范围内的整数`,
    };
  }
  return { value };
};

/**
 * 本次请求的 seed 决策（三态，供提交前统一判定）。
 *
 * - 用户填写 → 校验后**原值**提交（不取整、不降精度）；
 * - 工作流模式留空 → 显式注入随机种子（见 `randomWorkflowSeed` 的说明）；
 * - 契约未就绪 → 不注入任何推测性种子，只提交用户明确填的值。
 */
export const resolveRequestSeed = (
  raw: string,
  options: {
    /** `false` = 模型明确不支持 seed；`undefined` = 未声明（沿用旧行为）。 */
    supportsSeed: boolean | undefined;
    /** ComfyUI 工作流模式。 */
    workflowDriven: boolean;
    /** 当前工作流契约是否已就绪（含 paramMap / agentControls 求交）。 */
    contractReady: boolean;
  },
): { seed?: number; error?: string } => {
  const text = raw.trim();
  const allowed = options.supportsSeed !== false;
  if (text !== "") {
    if (!allowed) {
      return {};
    }
    const coerced = coerceSeedValue(text);
    if (coerced.error) {
      return { error: coerced.error };
    }
    return coerced.value === undefined ? {} : { seed: coerced.value };
  }
  if (allowed && options.workflowDriven && options.contractReady) {
    return { seed: randomWorkflowSeed() };
  }
  return {};
};

/** 影响 ComfyUI 契约的配置切片（契约缓存键的输入）。 */
export type ComfyContractKeyParts = {
  /** 工作台条目复合键（渠道|模型）。 */
  entryKey: string;
  /** 渠道端点：换一个 ComfyUI 实例就是另一套工作流。 */
  baseUrl?: string;
  /** 模型项工作流（空白则回退渠道级，与后端解析顺序一致）。 */
  itemWorkflow?: string;
  channelWorkflow?: string;
  /** 模型项参数映射（空白则回退渠道级）。 */
  itemParamMap?: string;
  channelParamMap?: string;
  agentControls?: unknown;
  taskContract?: unknown;
};

/** 取第一个非空白值（与后端「模型项为空则回退渠道级」同序）。 */
const firstNonBlank = (...values: Array<string | undefined>): string => {
  for (const value of values) {
    const text = (value ?? "").trim();
    if (text !== "") {
      return text;
    }
  }
  return "";
};

/**
 * 契约缓存键 = 条目 + 端点 + **实际生效**的工作流 / 映射 + 策略指纹。
 *
 * 少任何一项都会在设置页改动后继续命中旧契约：界面显示旧参数、提交却走新图，
 * 或者反过来。`baseUrl` 必须在键里——端点换了，同一份 workflow 文本也可能
 * 指向另一台机器上的另一套节点。
 */
export const comfyContractCacheKey = (parts: ComfyContractKeyParts): string =>
  JSON.stringify([
    parts.entryKey,
    (parts.baseUrl ?? "").trim(),
    firstNonBlank(parts.itemWorkflow, parts.channelWorkflow),
    firstNonBlank(parts.itemParamMap, parts.channelParamMap),
    parts.agentControls ?? null,
    parts.taskContract ?? null,
  ]);

/**
 * 由契约控件 + 表单文本组装 `params` 覆盖。
 *
 * 只**遍历当前契约的控件**，因此上一张工作流残留在表单里的键不会被带过去：
 * 换了模型后旧 `cfg` 就算还留在 state 里，也不在本次控件列表中，自然不提交。
 * 任一项越界即整单返回 error（提交前拦下，边界与执行侧同源）。
 */
export const buildWorkflowParams = (
  controls: { scalars: ComfyScalarControl[] },
  values: Record<string, string>,
): { params: Record<string, string | number>; error?: string } => {
  const params: Record<string, string | number> = {};
  for (const control of controls.scalars) {
    const coerced = coerceScalarValue(control, values[control.name] ?? "");
    if (coerced.error) {
      return { params: {}, error: coerced.error };
    }
    if (coerced.value !== undefined) {
      params[control.name] = coerced.value;
    }
  }
  return { params };
};

/**
 * 工作流模式下本次提交允许携带的顶层参数。
 *
 * 契约未就绪（读取中 / 读不到）时只放行纯提示词路径：`prompt` 必填，
 * `provider` / `model` 是路由，`seed` 仅在用户显式填写时才写入。尺寸、参考图、
 * 参数覆盖一律不提交 —— 它们能否真的落进图里尚未确认，靠基线能力猜着发
 * 等于把「界面没显示的参数」偷偷写进工作流。
 */
export const workflowRequestAllowedKeys = (options: {
  contractReady: boolean;
}): string[] =>
  options.contractReady
    ? [
        "prompt",
        "provider",
        "model",
        "seed",
        "mode",
        "images",
        "size",
        "negativePrompt",
        "params",
      ]
    : ["prompt", "provider", "model", "seed", "mode"];
