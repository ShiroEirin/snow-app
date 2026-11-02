import type {
  StreamInterruptionReason,
  StreamRecoveryOutcome,
} from "../../../../../preload";
import { isStreamIdleTimeoutReason } from "../../../../../shared/streamFailure";

export type IncompleteVariant =
  "partial_content" | "thinking_only" | "tool_call" | "empty";

export type NormalizedInterruptionReason = StreamInterruptionReason | "unknown";

export type NormalizedRecoveryOutcome =
  StreamRecoveryOutcome | "unknown" | null;

export type ResponseDispositionInput = {
  status?: string | null;
  content?: string | null;
  thinking?: string | null;
  toolCallsJson?: string | null;
  interruptionReason?: string | null;
  recoveryOutcome?: string | null;
  /** Rust 侧给出的失败分类（仅 `status=error` 时存在）。 */
  failureKind?: string | null;
  /** 失败终态时上游给出的（已脱敏）原因，用于拿不到分类时的兜底判定。 */
  failureReason?: string | null;
};

export type ResponseDisposition =
  | {
      kind: "complete";
      mayExecuteTools: true;
      mayContinueLoop: true;
    }
  | {
      kind: "error";
      mayExecuteTools: false;
      mayContinueLoop: false;
    }
  | {
      kind: "incomplete";
      variant: IncompleteVariant;
      reason: NormalizedInterruptionReason;
      recoveryOutcome: NormalizedRecoveryOutcome;
      mayExecuteTools: false;
      mayContinueLoop: false;
    };

const normalizeInterruptionReason = (
  value: string | null | undefined,
  status: string | null | undefined,
): NormalizedInterruptionReason => {
  switch (value) {
    case "unexpected_eof":
    case "read_error":
    case "idle_timeout":
    case "explicit_incomplete":
    case "output_limit":
    case "empty_response":
      return value;
    default:
      return value == null && (status === "length" || status === "max_tokens")
        ? "output_limit"
        : "unknown";
  }
};

const normalizeRecoveryOutcome = (
  value: string | null | undefined,
): NormalizedRecoveryOutcome => {
  if (value == null) {
    return null;
  }

  switch (value) {
    case "partial_threshold":
    case "retry_exhausted":
    case "non_retriable":
      return value;
    default:
      return "unknown";
  }
};

const hasUnsafeToolPayload = (
  toolCallsJson: string | null | undefined,
): boolean => {
  if (typeof toolCallsJson !== "string") {
    return false;
  }

  const normalized = toolCallsJson.trim();
  return normalized !== "" && normalized !== "[]" && normalized !== "null";
};

/**
 * 该响应终态是否由传输层空闲超时造成。优先用 Rust 给出的结构化分类
 * （`failureKind`，上游以错误终态返回时设置）；只有拿不到分类时才退回
 * 文案前缀兜底（例如旧版本后端，或失败原因来自本地异常）。
 */
const isIdleTimeoutTerminal = (input: ResponseDispositionInput): boolean =>
  input.failureKind === "timeout" ||
  input.interruptionReason === "idle_timeout" ||
  isStreamIdleTimeoutReason(input.failureReason);

/**
 * 响应终态判定的唯一入口（实时 response 与落库消息回读共用）。
 *
 * 分组依据是各 provider 共用的 `status` 字符串约定（见 preload/types/api.ts
 * 的 `ResponseStatus`）：
 * - `error` / `failed`：错误终态，工具与循环都不再继续；
 * - `cancelled` / `canceled`：用户主动停止，静默收尾（不产生任何告警）；
 * - `incomplete` / `length` / `max_tokens`：服务商或传输层明确未完成；
 * - `completed`：正常收尾，零载荷时是「空响应终态」；
 * - 其余未知取值：有载荷按正常完成，零载荷按未完成兜底——绝不静默留一个
 *   没有任何提示的空白气泡。
 */
const ERROR_STATUSES = new Set(["error", "failed"]);
const CANCELLED_STATUSES = new Set(["cancelled", "canceled"]);
const INCOMPLETE_STATUSES = new Set(["incomplete", "length", "max_tokens"]);
const COMPLETED_STATUS = "completed";

const hasResponsePayload = (input: ResponseDispositionInput): boolean =>
  Boolean(input.content?.trim()) ||
  Boolean(input.thinking?.trim()) ||
  hasUnsafeToolPayload(input.toolCallsJson);

/**
 * 空响应终态：Provider 以 `completed` 正常收尾，但正文、思考与工具调用全为空。
 *
 * Rust 侧在空响应重试预算耗尽时会标记 `interruption_reason=empty_response`
 * （见 native/src/api/retry.rs::resolve_empty_response_terminal，只在
 * `status=completed` 时标记）；本次修复之前落库的历史消息没有该标记，因此这里
 * 同时按「completed 且完全无载荷」兜底判定。
 *
 * `cancelled`（用户主动停止）一律豁免：取消路径会清空中断原因并丢弃工具调用，
 * 零载荷本就是预期结果。旧版本曾把这种「取消 + 零载荷」误标为 empty_response
 * 落库，豁免后历史脏数据也不会再渲染成「重试已耗尽」告警。
 */
const isEmptyResponseTerminal = (input: ResponseDispositionInput): boolean => {
  const status = input.status ?? "";
  if (CANCELLED_STATUSES.has(status) || ERROR_STATUSES.has(status)) {
    return false;
  }

  if (input.interruptionReason === "empty_response") {
    return true;
  }

  return status === COMPLETED_STATUS && !hasResponsePayload(input);
};

/** 未列入任何已知分组的取值（例如 provider 新增状态或中间态残留字符串）。 */
const isUnknownStatus = (status: string): boolean =>
  status !== COMPLETED_STATUS &&
  !ERROR_STATUSES.has(status) &&
  !CANCELLED_STATUSES.has(status) &&
  !INCOMPLETE_STATUSES.has(status);

export const resolveResponseDisposition = (
  input: ResponseDispositionInput,
): ResponseDisposition => {
  const status = input.status ?? "";

  if (ERROR_STATUSES.has(status)) {
    return {
      kind: "error",
      mayExecuteTools: false,
      mayContinueLoop: false,
    };
  }

  const isIncompleteLike =
    INCOMPLETE_STATUSES.has(status) ||
    isEmptyResponseTerminal(input) ||
    // 未知取值 + 零载荷：不能当成「正常完成」，否则这类回复会静默渲染成一个
    // 空白气泡（provider 新增状态、中间态残留等都会落到这里）。
    (isUnknownStatus(status) && !hasResponsePayload(input));
  if (!isIncompleteLike) {
    return {
      kind: "complete",
      mayExecuteTools: true,
      mayContinueLoop: true,
    };
  }

  let variant: IncompleteVariant;
  if (hasUnsafeToolPayload(input.toolCallsJson)) {
    variant = "tool_call";
  } else if (input.content?.trim()) {
    variant = "partial_content";
  } else if (input.thinking?.trim()) {
    variant = "thinking_only";
  } else {
    variant = "empty";
  }

  return {
    kind: "incomplete",
    variant,
    // 空响应终态统一回报 empty_response（Rust 侧显式标记，或「completed 且
    // 完全无载荷」的历史兜底）：前端据此复用重试提示渲染终态，而不是只留
    // 一个没有任何提示的空白气泡。
    reason: isEmptyResponseTerminal(input)
      ? "empty_response"
      : isIdleTimeoutTerminal(input)
        ? "idle_timeout"
        : normalizeInterruptionReason(input.interruptionReason, input.status),
    recoveryOutcome: normalizeRecoveryOutcome(input.recoveryOutcome),
    mayExecuteTools: false,
    mayContinueLoop: false,
  };
};

/** 自动执行资格不能复用 complete 展示分类：取消也会静默展示为 complete。 */
export const isResponseHtmlPreviewEligible = (
  input: ResponseDispositionInput,
): boolean =>
  input.status === COMPLETED_STATUS &&
  !input.interruptionReason &&
  resolveResponseDisposition(input).kind === "complete";
