import type { ChatConversationMessage } from "./conversationTypes";
import {
  isResponseHtmlPreviewEligible,
  type NormalizedInterruptionReason,
  type ResponseDispositionInput,
} from "./responseDisposition";

/** 登记真实响应终态；已经被取消的消息不接受迟到的成功结果重新授权。 */
export const settleResponseHtmlPreviewEligibility = (
  messages: ChatConversationMessage[],
  messageId: string,
  response: ResponseDispositionInput,
  cancelled: boolean,
): ChatConversationMessage[] =>
  messages.map((message) =>
    message.id === messageId
      ? {
          ...message,
          htmlPreviewEligible:
            message.htmlPreviewEligible !== false &&
            !cancelled &&
            isResponseHtmlPreviewEligible(response),
        }
      : message,
  );

/** 单条消息的中止收尾（幂等）：落定流式态、清掉重试标记、结算未完成的工具调用。 */
const settleMessage = (
  message: ChatConversationMessage,
  interruptionReason?: NormalizedInterruptionReason,
): ChatConversationMessage => ({
  ...message,
  status: message.status === "sending" ? "sent" : message.status,
  htmlPreviewEligible:
    message.role === "assistant" &&
    (message.status === "sending" || message.isRetrying)
      ? false
      : message.htmlPreviewEligible,
  // 中止后不存在合法的「重试进行中」：无论消息当前是什么状态都清掉，
  // 否则最后一条 assistant 消息会一直显示「重试中」转圈。
  isRetrying: false,
  // 传输层空闲超时导致的中止要留下原因：否则这条消息会静默收尾，用户与日志
  // 都看不出「是超时，不是自己停的」。其余中止不写中断原因。
  interruptionReason: interruptionReason ?? message.interruptionReason,
  toolCalls: message.toolCalls?.map((toolCall) =>
    toolCall.status === "running" || toolCall.status === "pending"
      ? {
          ...toolCall,
          status: "error" as const,
          result: toolCall.result ?? "Interrupted by user",
        }
      : toolCall,
  ),
});

/**
 * 中止（用户停止 / 级联中止子代理与工作流节点）时的消息收尾，幂等。
 *
 * 把仍停留在流式态的消息落定为已结束、清掉「重试进行中」标记，并把未结算的
 * 工具调用结算为中断错误。
 *
 * 共用同一份实现：`handleAbort`（用户点停止）与 `abortSubAgentTree`（级联
 * 中止子代理/工作流节点）。这两条路径收尾的都是「整个会话」——被中止的会话
 * 不会再收到新消息，全量落定是安全的。
 */
export const settleInterruptedMessages = (
  messages: ChatConversationMessage[],
): ChatConversationMessage[] =>
  messages.map((message) => settleMessage(message));

/**
 * 定向收尾：只落定指定的那一条消息。
 *
 * 被取代的旧 run 在自己的取消检查点收尾时必须用它，绝不能全量收尾：那时取代
 * 它的新 run 可能已经挂上自己的 `sending` 占位（强行发送 pending 消息就是
 * 这个形态），全量收尾会把新占位一并落定，表现为「强行发送后 StreamCursor
 * 消失、要等首个 token 到达才恢复」。
 */
export const settleInterruptedMessage = (
  messages: ChatConversationMessage[],
  messageId: string,
  interruptionReason?: NormalizedInterruptionReason,
): ChatConversationMessage[] =>
  messages.map((message) =>
    message.id === messageId
      ? settleMessage(message, interruptionReason)
      : message,
  );
