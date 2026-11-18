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
  readImageGenSettingsJson,
  resolveComfyWorkflowCapabilities,
  toImageGenForm,
  toImageGenSettingsJson,
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

const contract = (inputs) =>
  JSON.stringify({ provenance: "derived", inputs, warnings: [] });

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
  const caps = resolveComfyWorkflowCapabilities(
    contract([input("seed")]),
  );
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
    contract([
      input("image", { resolvedFrom: "paramMap" }),
      input("image2"),
    ]),
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
  for (const raw of [undefined, null, "", "   ", "{bad", "[]", "{}", '{"inputs":{}}']) {
    assert.equal(resolveComfyWorkflowCapabilities(raw), null);
  }
});

console.log(`\ncheck-imagegen-settings: ${checks} checks passed`);
