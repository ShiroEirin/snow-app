/**
 * 生图设置持久化回归：渠道级工作流契约必须「保存 → 重新读取」后能力不变。
 *
 * 项目没有前端测试 runner，这里用 vite 自带的 esbuild 把纯函数模块打成一个
 * 内存 bundle，再在 Node 里跑断言；不新增依赖、不落盘临时文件。
 */
import assert from "node:assert/strict";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

const scriptsDir = dirname(fileURLToPath(import.meta.url));

const bundle = await build({
  stdin: {
    contents: `
      export * from "../src/renderer/components/sidebar/imagegenSettings/constants.ts";
      export * from "../src/renderer/components/sidebar/imagegenSettings/utils.ts";
      export * from "../src/renderer/components/sidebar/imagegenSettings/panelLogic.ts";
      export * from "../src/renderer/components/rightPanel/modelCapabilities.ts";
    `,
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
  DEFAULT_IMAGE_GEN_CHANNEL,
  DEFAULT_IMAGE_GEN_EXECUTION_MODE,
  DEFAULT_IMAGE_GEN_MAX_CONCURRENT,
  DEFAULT_IMAGE_GEN_TIMEOUT_SECS,
  applyComfyBindingPatch,
  applyComfyBindingToChannel,
  comfyEndpointKey,
  readImageGenSettingsJson,
  resolveComfyWorkflowCapabilities,
  shouldApplyComfyDirectory,
  shouldClearComfyStateAfterProbe,
  shouldContinueComfyRecheck,
  toImageGenForm,
  toImageGenSettingsJson,
  withContractReadiness,
} = await import(moduleUrl);

let checks = 0;
const check = (label, fn) => {
  fn();
  checks += 1;
  console.log(`  ok  ${label}`);
};

/** 契约输入的最小构造器（只写绘图台会读的字段）。 */
const input = (name, overrides = {}) => ({
  name,
  mapsTo: ["1.value"],
  valueType: "string",
  exposure: "basic",
  resolvedFrom: "builtin",
  exposed: true,
  ...overrides,
});

const contract = (inputs, readiness) =>
  JSON.stringify({
    provenance: "derived",
    inputs,
    warnings: [],
    ...(readiness ? { readiness } : {}),
  });

// ---------------------------------------------------------------
// 1. 渠道级契约写入 → 回读 → 能力不变
// ---------------------------------------------------------------

const seedLocked = contract([
  input("prompt"),
  input("seed", { exposure: "locked" }),
  input("image", { resolvedFrom: "paramMap", mapsTo: ["10.image"] }),
  input("image2", { resolvedFrom: "paramMap", mapsTo: ["11.image"] }),
  input("negative", { exposure: "advanced" }),
]);

const channel = {
  ...DEFAULT_IMAGE_GEN_CHANNEL,
  id: "comfyui-test",
  name: "ComfyUI",
  provider: "comfyui",
  enabled: true,
  model: "krea2",
  paramsSchema: seedLocked,
};

const form = {
  channels: [channel],
  maxConcurrentImages: DEFAULT_IMAGE_GEN_MAX_CONCURRENT,
  timeoutSecs: DEFAULT_IMAGE_GEN_TIMEOUT_SECS,
  executionMode: DEFAULT_IMAGE_GEN_EXECUTION_MODE,
};

const savedJson = toImageGenSettingsJson(form);
const reread = readImageGenSettingsJson(savedJson);
const reshaped = toImageGenForm(reread);

check("渠道级 paramsSchema 保存后原样回读", () => {
  assert.equal(reread.channels.length, 1);
  assert.equal(reread.channels[0].paramsSchema, seedLocked);
  assert.equal(reshaped.channels[0].paramsSchema, seedLocked);
});

check("保存 → 重新读取后能力完全不变", () => {
  const before = resolveComfyWorkflowCapabilities(seedLocked);
  const after = resolveComfyWorkflowCapabilities(
    reread.channels[0].paramsSchema,
  );
  assert.deepEqual(after, before);
});

// ---------------------------------------------------------------
// 2. 锁定与暴露：exposed 与 exposure 必须同时看
// ---------------------------------------------------------------

check("locked 的 seed 不开放入口", () => {
  const caps = resolveComfyWorkflowCapabilities(seedLocked);
  assert.equal(caps.supportsSeed, false);
});

check("basic 的 seed 开放入口", () => {
  const caps = resolveComfyWorkflowCapabilities(contract([input("seed")]));
  assert.equal(caps.supportsSeed, true);
});

check("exposed=false 的槽位不开放", () => {
  const caps = resolveComfyWorkflowCapabilities(
    contract([
      input("seed", { exposed: false }),
      input("image", { exposed: false }),
    ]),
  );
  assert.equal(caps.supportsSeed, false);
  assert.equal(caps.supportsReference, false);
  assert.equal(caps.maxReferenceImages, 0);
});

// ---------------------------------------------------------------
// 3. 参考图上限：候选槽位数量 != 可提交数量
// ---------------------------------------------------------------

check("多槽位但落点未确认时收敛为 1", () => {
  const caps = resolveComfyWorkflowCapabilities(
    contract([input("image"), input("image2")]),
  );
  assert.equal(caps.supportsReference, true);
  assert.equal(caps.maxReferenceImages, 1);
});

check("每个槽位都由 paramMap 显式映射时才放开到槽位数量", () => {
  const caps = resolveComfyWorkflowCapabilities(
    contract([
      input("image", { resolvedFrom: "paramMap" }),
      input("image2", { resolvedFrom: "paramMap" }),
    ]),
  );
  assert.equal(caps.maxReferenceImages, 2);
});

check("部分槽位显式映射仍然收敛为 1", () => {
  const caps = resolveComfyWorkflowCapabilities(
    contract([input("image", { resolvedFrom: "paramMap" }), input("image2")]),
  );
  assert.equal(caps.maxReferenceImages, 1);
});

check("没有参考图槽位时不开放参考图", () => {
  const caps = resolveComfyWorkflowCapabilities(contract([input("prompt")]));
  assert.equal(caps.supportsReference, false);
  assert.equal(caps.maxReferenceImages, 0);
});

// ---------------------------------------------------------------
// 4. 负向词本轮不开放（提交白名单会删掉 negativePrompt）
// ---------------------------------------------------------------

check("契约里有 negative 也不返回负向词能力", () => {
  const caps = resolveComfyWorkflowCapabilities(seedLocked);
  assert.equal("supportsNegativePrompt" in caps, false);
});

// ---------------------------------------------------------------
// 5. 契约缺失/损坏回退基线，不误判成「不支持」
// ---------------------------------------------------------------

check("空 / 损坏 / 结构不符的契约返回 null", () => {
  for (const raw of [
    undefined,
    null,
    "",
    "   ",
    "{bad",
    "[]",
    "{}",
    '{"inputs":{}}',
  ]) {
    assert.equal(resolveComfyWorkflowCapabilities(raw), null);
  }
});

// ---------------------------------------------------------------
// 6. 后端预检结论优先于本地推导（工作台不再自己数节点猜）
// ---------------------------------------------------------------

check("readiness.seedWritable 为 false 时即使 exposed 也不显示 seed", () => {
  const caps = resolveComfyWorkflowCapabilities(
    contract([input("seed")], { seedWritable: false }),
  );
  assert.equal(caps.supportsSeed, false);
});

check("readiness.maxReferenceImages 覆盖本地按槽位推导的结果", () => {
  const caps = resolveComfyWorkflowCapabilities(
    contract([input("image"), input("image2"), input("image3")], {
      maxReferenceImages: 1,
    }),
  );
  assert.equal(caps.maxReferenceImages, 1);
});

check("readiness 放开多图时即使槽位非 paramMap 也采信后端", () => {
  const caps = resolveComfyWorkflowCapabilities(
    contract([input("image"), input("image2")], {
      maxReferenceImages: 2,
      referenceRolesConfirmed: true,
    }),
  );
  assert.equal(caps.maxReferenceImages, 2);
});

check("readiness 字段类型不对时退回本地推导", () => {
  const caps = resolveComfyWorkflowCapabilities(
    contract([input("seed"), input("image"), input("image2")], {
      seedWritable: "yes",
      maxReferenceImages: "many",
    }),
  );
  assert.equal(caps.supportsSeed, true);
  assert.equal(caps.maxReferenceImages, 1);
});

// ---------------------------------------------------------------
// 7. 预检结论随契约落库并可原样回读
// ---------------------------------------------------------------

check("withContractReadiness 并入 readiness 且保留原有输入", () => {
  const merged = withContractReadiness(seedLocked, { seedWritable: false });
  const parsed = JSON.parse(merged);
  assert.equal(parsed.readiness.seedWritable, false);
  assert.deepEqual(parsed.inputs, JSON.parse(seedLocked).inputs);
});

check("withContractReadiness 对坏契约/坏 readiness 不改写内容", () => {
  assert.equal(withContractReadiness("{bad", { seedWritable: false }), "{bad");
  assert.equal(withContractReadiness(seedLocked, null), seedLocked);
  assert.equal(withContractReadiness(undefined, { a: 1 }), "");
});

check("并入 readiness 的契约保存后仍能回读同样能力", () => {
  const merged = withContractReadiness(seedLocked, {
    seedWritable: false,
    maxReferenceImages: 2,
  });
  const roundTrip = readImageGenSettingsJson(
    toImageGenSettingsJson({
      channels: [{ ...channel, paramsSchema: merged }],
      maxConcurrentImages: DEFAULT_IMAGE_GEN_MAX_CONCURRENT,
      timeoutSecs: DEFAULT_IMAGE_GEN_TIMEOUT_SECS,
      executionMode: DEFAULT_IMAGE_GEN_EXECUTION_MODE,
    }),
  );
  assert.deepEqual(
    resolveComfyWorkflowCapabilities(roundTrip.channels[0].paramsSchema),
    resolveComfyWorkflowCapabilities(merged),
  );
});

// ---------------------------------------------------------------
// 8. 换绑原子性与旧目录丢弃
// ---------------------------------------------------------------

check("换绑补丁同时写 model/workflow，失败不调用时原项不变", () => {
  const original = {
    id: "m1",
    model: "old-model",
    workflow: "old.api.json",
    paramMap: '{"seed":"1.seed"}',
    paramsSchema: "{}",
  };
  const bound = applyComfyBindingPatch(original, {
    workflow: "new.api.json",
    nextModelId: "new-model",
    contractJson: '{"inputs":[]}',
  });
  assert.equal(bound.model, "new-model");
  assert.equal(bound.workflow, "new.api.json");
  assert.equal(bound.paramMap, "");
  assert.equal(bound.paramsSchema, '{"inputs":[]}');
  // 解析失败时调用方根本不写补丁：原项仍是旧 ID + 旧工作流。
  assert.equal(original.model, "old-model");
  assert.equal(original.workflow, "old.api.json");
});

check("旧地址慢返回与旧一代请求都不能覆盖新地址目录", () => {
  // A（seq=1）晚到，此时已切到 B（seq=2）：拒绝。
  assert.equal(shouldApplyComfyDirectory(1, 2, "http://a", "http://b"), false);
  // 同一地址的旧一代请求：拒绝。
  assert.equal(shouldApplyComfyDirectory(1, 2, "http://b", "http://b"), false);
  // B 恢复后重新检测（新一代、当前地址）：接受并刷新。
  assert.equal(shouldApplyComfyDirectory(2, 2, "http://b", "http://b"), true);
});

check("换绑默认项后默认指针跟到新模型，非默认项不动", () => {
  const models = [
    { id: "m1", model: "A", workflow: "a.api.json" },
    { id: "m2", model: "B", workflow: "b.api.json" },
  ];
  const promoted = applyComfyBindingToChannel(models, "B", 1, {
    workflow: "c.api.json",
    nextModelId: "C",
  });
  assert.equal(promoted.defaultModel, "C");
  assert.equal(promoted.models[1].model, "C");
  assert.equal(promoted.models[1].workflow, "c.api.json");
  assert.equal(promoted.models[0].model, "A");

  const untouched = applyComfyBindingToChannel(models, "B", 0, {
    workflow: "a2.api.json",
    nextModelId: "A2",
  });
  assert.equal(untouched.defaultModel, "B");
  assert.equal(untouched.models[0].model, "A2");
});

check("过期探测不清空当前地址，也不再发起预检", () => {
  // A 的迟到探测：当前已是 B。
  assert.equal(
    shouldClearComfyStateAfterProbe("stale", "http://a", "http://b"),
    false,
  );
  assert.equal(
    shouldClearComfyStateAfterProbe("unreachable", "http://a", "http://b"),
    false,
  );
  assert.equal(
    shouldContinueComfyRecheck("connected", "http://a", "http://b"),
    false,
  );
  // 当前地址的失败/成功才生效。
  assert.equal(
    shouldClearComfyStateAfterProbe("unreachable", "http://b", "http://b"),
    true,
  );
  assert.equal(
    shouldContinueComfyRecheck("connected", "http://b", "http://b"),
    true,
  );
  assert.equal(
    shouldContinueComfyRecheck("stale", "http://b", "http://b"),
    false,
  );
  // 端点归一化：尾斜杠 / 空地址。
  assert.equal(comfyEndpointKey(" http://b/ "), "http://b");
  assert.equal(comfyEndpointKey(""), "http://127.0.0.1:8188");
});

console.log(`\ncheck-imagegen-settings: ${checks} checks passed`);
