/**
 * 传输层空闲超时的失败文案（Rust 侧 `native/src/api/retry.rs` 的
 * `STREAM_IDLE_TIMEOUT_MESSAGE` 是同一份文案的权威来源，两侧必须保持一致）。
 *
 * 渲染层只在无法拿到结构化分类时用它兜底判定「超时」——正常路径用响应上的
 * `failureKind` / `interruptionReason`。
 */
export const STREAM_IDLE_TIMEOUT_MESSAGE =
  "Stream idle timeout: no data received within the configured period";

/** 失败原因文本是否属于传输层空闲超时。 */
export const isStreamIdleTimeoutReason = (
  reason: string | null | undefined,
): boolean => Boolean(reason?.startsWith(STREAM_IDLE_TIMEOUT_MESSAGE));
