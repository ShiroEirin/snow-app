/**
 * 绘图工作台回归：契约解析 / 控件翻译 / 契约缓存键 / 种子三态 / 工作流白名单。
 *
 * 项目没有前端测试 runner，沿用 check-imagegen-settings 的做法：用 esbuild 把
 * 纯函数模块打成内存 bundle，再在 Node 里断言；不新增依赖、不落盘临时文件。
 *
 * 这几组断言对应工作台真实会踩的坑：
 * - 契约未就绪时只允许纯提示词（不许带尺寸 / 参考图 / 参数覆盖）；
 * - 缓存键必须含 baseUrl 与实际生效的 workflow / paramMap / 策略指纹；
 * - 种子留空随机、填了原值提交、超安全整数绝不静默取整；
 * - 参数覆盖只由当前契约控件列表决定（换模型后旧值不串台）；
 * - 第 7 组直接用真机 `comfyui-describe` 输出（scripts/fixtures）断言。
 *
 * 关于「留空随机」：这是**工作台交互决定**，不是 API 语义。工具层省略 `seed`
 * 仍沿用模板 seed（Agent 复现依赖它）；只有工作台点「生成」时才显式注入随机值，
 * 否则模板 seed 固定会导致同一提示词永远出同一张图。
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

const scriptsDir = dirname(fileURLToPath(import.meta.url));

const bundle = await build({
  stdin: {
    contents: `export * from "../src/renderer/components/rightPanel/comfyContract.ts";`,
    resolveDir: scriptsDir,
    loader: "ts",
  },
  bundle: true,
  platform: "node",
  format: "esm",
  write: false,
  logLevel: "silent",
});

const moduleUrl = `data:text/javascript;base64,${Buffer.from(
  bundle.outputFiles[0].text,
).toString("base64")}`;
const {
  buildWorkflowParams,
  coerceSeedValue,
  comfyContractCacheKey,
  deriveComfyWorkbenchControls,
  parseComfyContract,
  randomWorkflowSeed,
  resolveRequestSeed,
  workflowRequestAllowedKeys,
} = await import(moduleUrl);

let checks = 0;
const check = (label, fn) => {
  fn();
  checks += 1;
  console.log(`  ok  ${label}`);
};

/** 契约输入的最小构造器（只写工作台会读的字段）。 */
const input = (name, overrides = {}) => ({
  name,
  valueType: "string",
  exposure: "basic",
  exposed: true,
  resolvedFrom: "builtin",
  ...overrides,
});

const describePayload = (inputs) => ({ contract: { inputs, warnings: [] } });

/** 把契约包装成 describe 的返回文本（工具直出 JSON）。 */
const describeText = (inputs) => JSON.stringify(describePayload(inputs));

const controlsOf = (inputs) =>
  deriveComfyWorkbenchControls(parseComfyContract(describeText(inputs)));

// ---------------------------------------------------------------
// 1. describe 载荷解析：裸 JSON 与 MCP content 包装两种外形
// ---------------------------------------------------------------

check("裸 JSON 与 content[].text 包装都能解析出同一契约", () => {
  const payload = describePayload([input("prompt"), input("seed")]);
  const direct = parseComfyContract(JSON.stringify(payload));
  const wrapped = parseComfyContract(
    JSON.stringify({
      content: [{ type: "text", text: JSON.stringify(payload) }],
    }),
  );
  assert.equal(direct.inputs.length, 2);
  assert.deepEqual(
    wrapped.inputs.map((item) => item.name),
    direct.inputs.map((item) => item.name),
  );
});

check("非契约 / 坏 JSON / 缺 inputs 一律返回 null（退回保守基线）", () => {
  for (const raw of ["", "not json", "{}", '{"contract":{}}', "[]"]) {
    assert.equal(parseComfyContract(raw), null);
  }
});

// ---------------------------------------------------------------
// 2. 契约 → 控件：可写性只看 exposed 与 exposure
// ---------------------------------------------------------------

check("locked / exposed=false 的输入都不开放", () => {
  const caps = controlsOf([
    input("prompt"),
    input("seed", { exposure: "locked", exposed: false }),
    input("cfg", { exposed: false }),
  ]);
  assert.equal(caps.capabilities.supportsSeed, false);
  assert.deepEqual(
    caps.scalars.map((item) => item.name),
    [],
  );
});

check("basic / advanced 标量按 exposure 归位，枚举带 options", () => {
  const caps = controlsOf([
    input("steps", {
      valueType: "integer",
      schema: { minimum: 1, maximum: 40 },
    }),
    input("cfg", { exposure: "advanced", valueType: "number" }),
    input("sampler_name", {
      exposure: "advanced",
      schema: { enum: ["euler", "dpmpp_2m"] },
    }),
  ]);
  assert.deepEqual(
    caps.scalars.map((item) => [item.name, item.exposure, item.kind]),
    [
      ["steps", "basic", "integer"],
      ["cfg", "advanced", "number"],
      ["sampler_name", "advanced", "enum"],
    ],
  );
  assert.deepEqual(
    caps.scalars.find((item) => item.name === "sampler_name").options,
    ["euler", "dpmpp_2m"],
  );
});

check("纯文生图契约（无 image 槽位）→ 不开放参考图", () => {
  const caps = controlsOf([input("prompt"), input("seed")]);
  assert.equal(caps.capabilities.supportsReference, false);
  assert.equal(caps.capabilities.maxReferenceImages, 0);
});

check("多槽位未全部由 paramMap 落点时收敛为 1 张", () => {
  const partial = controlsOf([
    input("image", { resolvedFrom: "paramMap" }),
    input("image2"),
  ]);
  assert.equal(partial.capabilities.maxReferenceImages, 1);
  const confirmed = controlsOf([
    input("image", { resolvedFrom: "paramMap" }),
    input("image2", { resolvedFrom: "paramMap" }),
  ]);
  assert.equal(confirmed.capabilities.maxReferenceImages, 2);
});

check("width 与 height 双双可写才开放尺寸控件", () => {
  assert.equal(
    controlsOf([input("width"), input("height")]).sizeWritable,
    true,
  );
  assert.equal(controlsOf([input("width")]).sizeWritable, false);
});

check("negative 可写时给出模板值作为占位", () => {
  const caps = controlsOf([
    input("negative", { example: "worst quality" }),
    input("negative2", { exposure: "locked" }),
  ]);
  assert.equal(caps.negativeWritable, true);
  assert.equal(caps.negativeTemplate, "worst quality");
});

// ---------------------------------------------------------------
// 3. 种子：留空随机 / 固定原值 / 超安全整数不静默舍入
// ---------------------------------------------------------------

const workflowSeed = {
  supportsSeed: true,
  workflowDriven: true,
  contractReady: true,
};

check("留空 + 契约就绪 → 注入随机种子（0 ≤ seed < 2^31，整数）", () => {
  for (let i = 0; i < 32; i += 1) {
    const decided = resolveRequestSeed("", workflowSeed);
    assert.ok(Number.isSafeInteger(decided.seed));
    assert.ok(decided.seed >= 0 && decided.seed < 2_147_483_647);
  }
});

check("留空 + 契约未就绪 → 不注入任何推测性种子", () => {
  assert.deepEqual(
    resolveRequestSeed("", { ...workflowSeed, contractReady: false }),
    {},
  );
  assert.deepEqual(
    resolveRequestSeed("   ", { ...workflowSeed, contractReady: false }),
    {},
  );
});

check("固定种子按原值提交，不做任何取整", () => {
  assert.deepEqual(resolveRequestSeed("424242", workflowSeed), {
    seed: 424242,
  });
  assert.deepEqual(resolveRequestSeed(" 7\n", workflowSeed), { seed: 7 });
  assert.deepEqual(coerceSeedValue("9007199254740991"), {
    value: 9007199254740991,
  });
});

check("超安全整数返回错误且不产出 value（绝不提交舍入值）", () => {
  for (const raw of [
    "9007199254740992",
    "9007199254740993",
    "18446744073709551615",
    "99999999999999999999999",
  ]) {
    const decided = coerceSeedValue(raw);
    assert.equal(decided.value, undefined);
    assert.match(decided.error, /安全整数/);
  }
});

check("非整数 / 负数 / 科学计数法 / 小数都拒绝", () => {
  for (const raw of ["-1", "1.5", "1e5", "0x10", "abc", "1 2"]) {
    const decided = coerceSeedValue(raw);
    assert.equal(decided.value, undefined);
    assert.ok(decided.error);
  }
});

check("模型明确不支持 seed 时既不注入也不提交", () => {
  const unsupported = {
    supportsSeed: false,
    workflowDriven: true,
    contractReady: true,
  };
  assert.deepEqual(resolveRequestSeed("", unsupported), {});
  assert.deepEqual(resolveRequestSeed("123", unsupported), {});
});

check("非工作流模型留空不注入随机（保持服务商默认）", () => {
  assert.deepEqual(
    resolveRequestSeed("", {
      supportsSeed: true,
      workflowDriven: false,
      contractReady: true,
    }),
    {},
  );
});

check("randomWorkflowSeed 落在 32 位非负区间", () => {
  for (let i = 0; i < 64; i += 1) {
    const seed = randomWorkflowSeed();
    assert.ok(Number.isSafeInteger(seed) && seed >= 0 && seed < 2_147_483_647);
  }
});

// ---------------------------------------------------------------
// 4. 契约缓存键：端点 + 实际生效的工作流 / 映射 + 策略指纹
// ---------------------------------------------------------------

const baseParts = {
  entryKey: "comfyui-local-native|anima-t2i",
  baseUrl: "http://127.0.0.1:8188",
  itemWorkflow: "",
  channelWorkflow: "anima.api.json",
  itemParamMap: "",
  channelParamMap: '{"steps":"19.steps","cfg":"19.cfg"}',
  agentControls: { cfg: { max: 6 } },
  taskContract: { operation: "t2i" },
};

check(
  "改 baseUrl / workflow / paramMap / agentControls / taskContract 都换键",
  () => {
    const key = comfyContractCacheKey(baseParts);
    assert.notEqual(
      comfyContractCacheKey({ ...baseParts, baseUrl: "http://127.0.0.1:8288" }),
      key,
    );
    assert.notEqual(
      comfyContractCacheKey({ ...baseParts, channelWorkflow: "qwen.api.json" }),
      key,
    );
    assert.notEqual(
      comfyContractCacheKey({
        ...baseParts,
        itemWorkflow: "anima-v2.api.json",
      }),
      key,
    );
    assert.notEqual(
      comfyContractCacheKey({
        ...baseParts,
        channelParamMap: '{"steps":"3.steps"}',
      }),
      key,
    );
    assert.notEqual(
      comfyContractCacheKey({
        ...baseParts,
        agentControls: { cfg: { max: 8 } },
      }),
      key,
    );
    assert.notEqual(
      comfyContractCacheKey({
        ...baseParts,
        taskContract: { operation: "i2i" },
      }),
      key,
    );
  },
);

check("模型项为空时回退渠道级：与直接给渠道级值同一个键", () => {
  assert.equal(
    comfyContractCacheKey({
      entryKey: "e",
      baseUrl: " u ",
      channelWorkflow: "w.api.json",
      channelParamMap: "{p}",
    }),
    comfyContractCacheKey({
      entryKey: "e",
      baseUrl: "u",
      itemWorkflow: "   ",
      channelWorkflow: "w.api.json",
      itemParamMap: "",
      channelParamMap: "{p}",
    }),
  );
});

check("模型项值优先于渠道级，且重复计算稳定", () => {
  const withItem = comfyContractCacheKey({
    ...baseParts,
    itemWorkflow: "item.api.json",
  });
  assert.notEqual(withItem, comfyContractCacheKey(baseParts));
  assert.equal(
    withItem,
    comfyContractCacheKey({ ...baseParts, itemWorkflow: "item.api.json" }),
  );
});

// ---------------------------------------------------------------
// 5. 提交白名单：契约未就绪 = 纯提示词
// ---------------------------------------------------------------

check("契约未就绪：白名单只剩 prompt / provider / model / seed / mode", () => {
  const keys = workflowRequestAllowedKeys({ contractReady: false });
  assert.deepEqual([...keys].sort(), [
    "mode",
    "model",
    "prompt",
    "provider",
    "seed",
  ]);
  for (const forbidden of ["size", "params", "negativePrompt", "images"]) {
    assert.equal(keys.includes(forbidden), false);
  }
});

check("契约就绪：才放行尺寸 / 参考图 / 负向词 / 参数覆盖", () => {
  const keys = workflowRequestAllowedKeys({ contractReady: true });
  for (const allowed of [
    "prompt",
    "seed",
    "size",
    "images",
    "negativePrompt",
    "params",
  ]) {
    assert.equal(keys.includes(allowed), true);
  }
});

// ---------------------------------------------------------------
// 6. 参数覆盖组装：只填 CFG 就只发 CFG；换模型后旧值不串台
// ---------------------------------------------------------------

check("只填 CFG 时 params 里只有 cfg（steps 留空不提交）", () => {
  const controls = controlsOf([
    input("steps", { exposure: "advanced", valueType: "integer" }),
    input("cfg", { exposure: "advanced", valueType: "number" }),
  ]);
  const built = buildWorkflowParams(controls, { cfg: "5.5" });
  assert.deepEqual(built.params, { cfg: 5.5 });
  assert.equal(built.error, undefined);
});

check("全部留空 → 不产生 params（沿用模板，不写任何键）", () => {
  const controls = controlsOf([
    input("steps", { valueType: "integer" }),
    input("cfg", { valueType: "number" }),
  ]);
  assert.deepEqual(buildWorkflowParams(controls, {}).params, {});
  assert.deepEqual(buildWorkflowParams(controls, { cfg: "" }).params, {});
});

check("换模型后残留的旧键不在新契约控件列表 → 不会被提交", () => {
  // anima 表单里填过 cfg=5.5 steps=26；切到只暴露 steps 的工作流。
  const anima = controlsOf([
    input("steps", { exposure: "advanced", valueType: "integer" }),
    input("cfg", { exposure: "advanced", valueType: "number" }),
  ]);
  const animaBuilt = buildWorkflowParams(anima, { cfg: "5.5", steps: "26" });
  assert.deepEqual(animaBuilt.params, { cfg: 5.5, steps: 26 });

  const narrowed = controlsOf([
    input("steps", { exposure: "advanced", valueType: "integer" }),
  ]);
  const afterSwitch = buildWorkflowParams(narrowed, {
    cfg: "5.5",
    steps: "26",
  });
  assert.deepEqual(afterSwitch.params, { steps: 26 });
  assert.equal("cfg" in afterSwitch.params, false);
});

check("越界标量整单拒绝，不返回半截 params", () => {
  const controls = controlsOf([
    input("cfg", {
      exposure: "advanced",
      valueType: "number",
      schema: { minimum: 0, maximum: 10 },
    }),
    input("steps", { exposure: "advanced", valueType: "integer" }),
  ]);
  const built = buildWorkflowParams(controls, { cfg: "150", steps: "30" });
  assert.deepEqual(built.params, {});
  assert.match(built.error, /cfg/);
});

check("枚举只接受契约给出的选项", () => {
  const controls = controlsOf([
    input("sampler_name", {
      exposure: "advanced",
      schema: { enum: ["euler", "dpmpp_2m"] },
    }),
  ]);
  assert.deepEqual(
    buildWorkflowParams(controls, { sampler_name: "dpmpp_2m" }).params,
    { sampler_name: "dpmpp_2m" },
  );
  assert.match(
    buildWorkflowParams(controls, { sampler_name: "lms" }).error,
    /不在可选值内/,
  );
});

// ---------------------------------------------------------------
// 7. 真机契约样本：三条验收项直接对着真实 describe 输出断言
// ---------------------------------------------------------------

const fixture = (name) =>
  readFileSync(join(scriptsDir, "fixtures", name), "utf8");

/** 走完整链路：describe 文本 → 契约 → 控件。 */
const controlsFromFixture = (name) =>
  deriveComfyWorkbenchControls(parseComfyContract(fixture(name)));

check("[真机·anima] 无 image 槽位 → 参考图入口必须隐藏", () => {
  const caps = controlsFromFixture("comfy-contract-anima.json");
  assert.equal(caps.capabilities.supportsReference, false);
  assert.equal(caps.capabilities.maxReferenceImages, 0);
  // DrawingRefStrip 的隐藏条件正是这个组合。
  const stripHidden = caps.capabilities.supportsReference === false;
  assert.equal(stripHidden, true);
});

check("[真机·anima] 只填 CFG=5.5 → 请求只带 cfg 一项覆盖", () => {
  const caps = controlsFromFixture("comfy-contract-anima.json");
  const built = buildWorkflowParams(caps, { cfg: "5.5" });
  assert.deepEqual(built.params, { cfg: 5.5 });
  // 尺寸未填 → 不提交 size（后端走模型默认），种子留空由工作台注入。
  assert.equal("steps" in built.params, false);
  assert.equal("seed" in built.params, false);
});

check("[真机·anima] 契约范围已与 agentControls 求交（256–4096）", () => {
  const caps = controlsFromFixture("comfy-contract-anima.json");
  const width = caps.scalars.find((item) => item.name === "width");
  // width/height 是专用控件，不在 scalars；用契约原文断言边界。
  const contract = parseComfyContract(fixture("comfy-contract-anima.json"));
  const bound = contract.inputs.find((item) => item.name === "width");
  assert.equal(bound.schema.minimum, 256);
  assert.equal(bound.schema.maximum, 4096);
  assert.equal(caps.sizeWritable, true);
  assert.equal(width, undefined);
});

check("[真机·moss-merge] paramMap 关闭的 negative 不出控件、不提交", () => {
  const caps = controlsFromFixture("comfy-contract-moss-merge.json");
  assert.equal(caps.negativeWritable, false);
  const built = buildWorkflowParams(caps, { negative: "should be ignored" });
  assert.equal("negative" in built.params, false);
});

check("[真机·moss-merge] agentControls 锁定的 character_boxes 不出控件", () => {
  const caps = controlsFromFixture("comfy-contract-moss-merge.json");
  const names = caps.scalars.map((item) => item.name);
  assert.equal(names.includes("character_boxes"), false);
  // 结构化槽位交给 Agent，不进标量列表。
  assert.equal(names.includes("layout"), false);
  assert.deepEqual(names, ["cfg", "steps"]);
});

check(
  "[真机·moss-merge] 模板 seed 3005825288 超 32 位，固定后仍原值提交",
  () => {
    const caps = controlsFromFixture("comfy-contract-moss-merge.json");
    const contract = parseComfyContract(
      fixture("comfy-contract-moss-merge.json"),
    );
    const seed = contract.inputs.find((item) => item.name === "seed");
    assert.equal(seed.example, 3005825288);
    // 这正好证明「只能用 32 位随机范围」不够：固定模板值必须无损往返。
    assert.deepEqual(
      resolveRequestSeed("3005825288", {
        supportsSeed: caps.capabilities.supportsSeed,
        workflowDriven: true,
        contractReady: true,
      }),
      { seed: 3005825288 },
    );
  },
);

check(
  "[真机·切换模型] anima 的 cfg 值在 moss 契约下若可写则保留、不可写则丢弃",
  () => {
    const anima = controlsFromFixture("comfy-contract-anima.json");
    const moss = controlsFromFixture("comfy-contract-moss-merge.json");
    // 同一份表单文本（用户在 anima 填过 cfg=5.5 / steps=26）切到 moss：
    const formText = { cfg: "5.5", steps: "26", negative: "old negative" };
    assert.deepEqual(buildWorkflowParams(anima, formText).params, {
      cfg: 5.5,
      steps: 26,
    });
    // moss 也暴露 cfg/steps，故这两项按新契约边界重新校验后仍提交；
    // negative 已被 paramMap 关闭 → 必须消失，不能带过去。
    const afterSwitch = buildWorkflowParams(moss, formText);
    assert.deepEqual(afterSwitch.params, { cfg: 5.5, steps: 26 });
    assert.equal("negative" in afterSwitch.params, false);
  },
);

check("[真机·切换模型] 新契约边界更窄时旧值被拒绝，而不是沿用", () => {
  // anima cfg 上限 100；构造一个只允许 0–4 的 cfg 策略（模拟更紧的 agentControls）。
  const narrow = deriveComfyWorkbenchControls({
    inputs: [
      input("cfg", {
        exposure: "advanced",
        valueType: "number",
        schema: { type: "number", minimum: 0, maximum: 4 },
      }),
    ],
  });
  const built = buildWorkflowParams(narrow, { cfg: "5.5" });
  assert.deepEqual(built.params, {});
  assert.match(built.error, /cfg/);
});

console.log(`\ncheck-drawing-workbench: ${checks} checks passed`);
