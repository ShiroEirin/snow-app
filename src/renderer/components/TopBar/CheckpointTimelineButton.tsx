import {
  ArrowDownWideNarrow,
  ArrowUpNarrowWide,
  ChevronRight,
  History,
  Loader2,
  Pin,
  Undo2,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { CheckpointManifestEntriesResult } from "../../../preload";
import { useI18n } from "../../i18n";
import { useChatConversationContext } from "../mainContent/chatMessages";
import {
  ensureMessageLoaded,
  type MessageLoadBudget,
} from "../mainContent/chatMessages/utils/conversationHelpers";
import { stripMarkdown } from "../../../shared/stripMarkdown";
import {
  getChipDisplayLabel,
  parseContentSegments,
  summarizeContentAsPlainText,
  type ContentSegment,
} from "../mainContent/chatInput/fileTagUtils";
import { ContentChips } from "../mainContent/chatInput/ContentChips";
import { rightPanelEvents } from "../rightPanel/rightPanelEvents";

/** 排序方案的持久化键：与用户消息面板独立，重启后沿用上次选择。 */
const TIMELINE_ORDER_STORAGE_KEY = "snow:topbar:checkpoint-timeline-order";

/** newest = 最新在上（默认）；oldest = 最早在上。 */
type TimelineOrder = "newest" | "oldest";

const readTimelineOrder = (): TimelineOrder =>
  window.localStorage.getItem(TIMELINE_ORDER_STORAGE_KEY) === "oldest"
    ? "oldest"
    : "newest";

/** 列表预览保留的最大字符数（与回滚目标选择器一致）。 */
const PREVIEW_MAX_LENGTH = 120;
/** 展开的文件清单最多展示的条数，超出部分折叠为计数。 */
const EXPANDED_FILE_LIMIT = 50;
/** 跨页加载历史消息的预算（与回滚目标选择器一致）。 */
const TARGET_LOAD_BUDGET: MessageLoadBudget = {
  maxPages: 150,
  timeoutMs: 30_000,
};

/** 单行消息预览：先折叠 chip 标签为可读文本，再剥离 Markdown。 */
const buildPreview = (content: string): string => {
  const plain = stripMarkdown(summarizeContentAsPlainText(content))
    .replace(/\s+/g, " ")
    .trim();
  return plain.length > PREVIEW_MAX_LENGTH
    ? `${plain.slice(0, PREVIEW_MAX_LENGTH)}…`
    : plain;
};

/** 预览里每个 chip 的固定开销（图标、内边距、间距）折算的字符数。 */
const CHIP_PREVIEW_OVERHEAD = 6;

/**
 * 预览片段（与回滚目标选择器同一处理）：文本段落剥离 Markdown 并折叠空白；
 * chip 段落（@@file: / @@image: 等）保留结构，由 ContentChips 渲染成消息区
 * 同款 chip。累计显示长度超过上限时截断文本并丢弃其后片段。
 */
const buildPreviewSegments = (content: string): ContentSegment[] => {
  const segments: ContentSegment[] = [];
  let used = 0;
  for (const segment of parseContentSegments(content)) {
    if (used >= PREVIEW_MAX_LENGTH) {
      break;
    }
    if (segment.type === "text") {
      const text = stripMarkdown(segment.content).replace(/\s+/g, " ").trim();
      if (!text) {
        continue;
      }
      const budget = PREVIEW_MAX_LENGTH - used;
      const clipped = text.length > budget ? `${text.slice(0, budget)}…` : text;
      used += clipped.length;
      segments.push({ type: "text", content: clipped });
    } else {
      used += getChipDisplayLabel(segment).length + CHIP_PREVIEW_OVERHEAD;
      segments.push(segment);
    }
  }
  return segments;
};

/** 格式化 checkpoint id（`cp-{secs}-{nanos}-{count}`）内嵌的创建时间。 */
const formatCheckpointTime = (checkpointId: string): string => {
  const seconds = Number.parseInt(checkpointId.slice(3).split("-")[0] ?? "", 10);
  if (!Number.isFinite(seconds) || seconds <= 0) {
    return "";
  }
  const date = new Date(seconds * 1000);
  const pad = (value: number): string => String(value).padStart(2, "0");
  const now = new Date();
  const sameDay =
    date.getFullYear() === now.getFullYear() &&
    date.getMonth() === now.getMonth() &&
    date.getDate() === now.getDate();
  const time = `${pad(date.getHours())}:${pad(date.getMinutes())}`;
  if (sameDay) {
    return time;
  }
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${time}`;
};

type TimelineNode = {
  messageId: string;
  checkpointId: string;
  createdAt: string;
  timeLabel: string;
  preview: string;
  segments: ContentSegment[];
  isContextCompaction: boolean;
  /** manifest 统计；null 表示尚未加载完成。 */
  counts: {
    addedCount: number;
    modifiedCount: number;
    deletedCount: number;
    pendingCount: number;
  } | null;
  entries: CheckpointManifestEntriesResult["entries"] | null;
};

type TimelineFileStats = {
  additions: number;
  deletions: number;
  isBinary: boolean;
  tooLarge: boolean;
};

type TimelineFileRowProps = {
  checkpointId: string;
  entry: { path: string; changeType: string };
  /** 会话版本号：工具每次完成文件改动都会 +1，行内统计随之重取。 */
  conversationVersion: number;
  isInteractive: boolean;
  changeLabel: (changeType: string) => string;
  onOpenDiff: () => void;
};

/** 单个文件行：类型徽标 + 路径 + 增删行数；点击在右侧面板打开 diff。
 *  统计挂载即按页前一次性读取（两端对象比对，不扫工作树），会话版本变化
 *  时重新读取——同一轮里该文件被追加编辑 / 自动格式化后数字保持最新。
 *  待定（捕获中）、二进制与超大/远端不可用（tooLarge）的行不提供 diff。 */
const TimelineFileRow = ({
  checkpointId,
  entry,
  conversationVersion,
  isInteractive,
  changeLabel,
  onOpenDiff,
}: TimelineFileRowProps): React.JSX.Element => {
  const [stats, setStats] = useState<TimelineFileStats | null>(null);

  useEffect(() => {
    if (!isInteractive) {
      setStats(null);
      return;
    }
    let cancelled = false;
    window.snow
      .readCheckpointManifestEntryStats(checkpointId, entry.path)
      .then((summary) => {
        if (!cancelled) {
          setStats({
            additions: summary.additions,
            deletions: summary.deletions,
            isBinary: summary.isBinary,
            tooLarge: summary.tooLarge,
          });
        }
      })
      .catch(() => {
        if (!cancelled) {
          setStats(null);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [checkpointId, entry.path, conversationVersion, isInteractive]);

  const canOpen = isInteractive && stats !== null && !stats.isBinary && !stats.tooLarge;
  const hasLines = stats !== null && (stats.additions > 0 || stats.deletions > 0);

  return (
    <div className="top-bar-timeline-file">
      <button
        type="button"
        className="top-bar-timeline-file-row"
        disabled={!canOpen}
        title={entry.path}
        onClick={() => {
          if (canOpen) {
            onOpenDiff();
          }
        }}
      >
        <span className={`top-bar-timeline-file-badge is-${entry.changeType}`}>
          {changeLabel(entry.changeType)}
        </span>
        <span className="top-bar-timeline-file-path">{entry.path}</span>
        {stats && !stats.isBinary && !stats.tooLarge && hasLines ? (
          <span className="top-bar-timeline-file-lines">
            {stats.additions > 0 ? (
              <span className="is-added">+{stats.additions}</span>
            ) : null}
            {stats.deletions > 0 ? (
              <span className="is-deleted">−{stats.deletions}</span>
            ) : null}
          </span>
        ) : null}
      </button>
    </div>
  );
};

/** 顶栏「检查点时间轴」：把会话里逐轮的检查点快照可视化成时间轴，
 *  每轮可展开查看该轮的文件变更清单，并可从此处发起回滚。
 *
 *  数据与回滚目标选择器同源：`listUserMessages` 全量用户消息（携带
 *  checkpointId），叠加 `listCheckpointManifestEntries` 纯读 manifest
 *  的条目统计——不扫描工作树，长会话也只有毫秒级开销。
 *
 *  回滚动作完整复用 `ensureMessageLoaded` + `handleRollback` 链路：
 *  确认弹窗、文件恢复与消息截断与现有回滚完全一致。 */
export const CheckpointTimelineButton = ({
  onOpenChange,
}: {
  onOpenChange?: (open: boolean) => void;
}): React.JSX.Element | null => {
  const { t } = useI18n();
  const {
    messages,
    activeConversationId,
    conversationVersion,
    hasMoreMessages,
    loadOlderMessages,
    handleRollback,
    rollbackPreparingMessageId,
    rollbackPreview,
    isStreaming,
    isAborting,
  } = useChatConversationContext();

  const [isOpen, setIsOpen] = useState(false);
  const [isPinned, setIsPinned] = useState(false);
  const [timelineOrder, setTimelineOrder] =
    useState<TimelineOrder>(readTimelineOrder);
  const [rows, setRows] = useState<{
    conversationId: string;
    items: {
      id: string;
      content: string;
      createdAt: string;
      checkpointId: string;
      isContextCompaction: boolean;
    }[];
  } | null>(null);
  const [manifests, setManifests] = useState<{
    conversationId: string;
    byCheckpoint: Map<string, CheckpointManifestEntriesResult>;
  } | null>(null);
  const [expandedMessageIds, setExpandedMessageIds] = useState<Set<string>>(
    new Set(),
  );
  const [preparingMessageId, setPreparingMessageId] = useState<string | null>(
    null,
  );
  const [loadError, setLoadError] = useState<string | null>(null);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const preparingRef = useRef(false);
  const messagesRef = useRef(messages);
  const hasMoreMessagesRef = useRef(hasMoreMessages);
  messagesRef.current = messages;
  hasMoreMessagesRef.current = hasMoreMessages;

  // 与回滚目标选择器同一数据源与刷新时机：会话切换 / 会话版本变化
  // （发送 / 回滚 / 压缩 / 翻页）时重取全量用户消息。
  useEffect(() => {
    if (!activeConversationId) {
      setRows(null);
      setManifests(null);
      return;
    }
    let cancelled = false;
    window.snow
      .listUserMessages(activeConversationId)
      .then((items) => {
        if (!cancelled) {
          setRows({ conversationId: activeConversationId, items });
        }
      })
      .catch(() => {
        if (!cancelled) {
          setRows((current) =>
            current?.conversationId === activeConversationId ? current : null,
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, [activeConversationId, conversationVersion]);

  // 时间轴节点：带检查点的用户消息（含压缩边界）+ 内存窗口中尚未落库的
  // 新消息，按所选方向排序（默认最新在上）。
  const checkpointEntries = useMemo(() => {
    const seen = new Set<string>();
    const items: {
      messageId: string;
      checkpointId: string;
      createdAt: string;
      content: string;
      isContextCompaction: boolean;
    }[] = [];
    const push = (
      messageId: string,
      checkpointId: string,
      createdAt: string,
      content: string,
      isContextCompaction: boolean,
    ): void => {
      if (!checkpointId || seen.has(messageId)) {
        return;
      }
      seen.add(messageId);
      items.push({
        messageId,
        checkpointId,
        createdAt,
        content,
        isContextCompaction,
      });
    };
    if (rows && rows.conversationId === activeConversationId) {
      for (const row of rows.items) {
        push(
          row.id,
          row.checkpointId ?? "",
          row.createdAt,
          row.content,
          row.isContextCompaction,
        );
      }
    }
    for (const message of messages) {
      if (message.role === "user") {
        push(
          message.id,
          message.checkpointId ?? "",
          "",
          message.content,
          message.isContextCompaction === true,
        );
      }
    }
    return timelineOrder === "newest" ? items.reverse() : items;
  }, [activeConversationId, messages, rows, timelineOrder]);

  const handleToggleOrder = useCallback((): void => {
    setTimelineOrder((prev) => {
      const next: TimelineOrder = prev === "newest" ? "oldest" : "newest";
      // 立刻落盘：重启、切会话都沿用这次的选择（与用户消息面板同一约定）。
      window.localStorage.setItem(TIMELINE_ORDER_STORAGE_KEY, next);
      return next;
    });
  }, []);

  // 批量读取全部检查点的 manifest 条目：纯读本机 JSON，不扫描工作树。
  // 版本变化（工具完成 / 发送 / 回滚 / 压缩）时静默重读——同一轮对话里文件
  // 陆续被工具改动时 checkpoint id 集合并不变化，必须依赖会话版本号才能实时
  // 反映 manifest 的新条目；已有数据不清空，避免面板闪烁。
  const checkpointKey = useMemo(
    () => checkpointEntries.map((entry) => entry.checkpointId).join("|"),
    [checkpointEntries],
  );
  useEffect(() => {
    if (!activeConversationId || checkpointEntries.length === 0) {
      setManifests(null);
      return;
    }
    const ids = checkpointEntries.map((entry) => entry.checkpointId);
    let cancelled = false;
    window.snow
      .listCheckpointManifestEntries(ids)
      .then((results) => {
        if (!cancelled) {
          setManifests({
            conversationId: activeConversationId,
            byCheckpoint: new Map(
              results.map((result) => [result.checkpointId, result]),
            ),
          });
        }
      })
      .catch(() => {
        // 读取失败保留现有快照：统计徽标退化为不显示，不影响时间轴打开。
      });
    return () => {
      cancelled = true;
    };
    // checkpointKey 变化即节点集合变化；entries 引用每次 render 都可能重建，
    // 不作为依赖（避免无意义的重复拉取）。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeConversationId, checkpointKey, conversationVersion]);

  const nodes = useMemo<TimelineNode[]>(() => {
    const byCheckpoint =
      manifests && manifests.conversationId === activeConversationId
        ? manifests.byCheckpoint
        : null;
    return checkpointEntries.map((entry) => {
      const manifest = byCheckpoint?.get(entry.checkpointId) ?? null;
      return {
        messageId: entry.messageId,
        checkpointId: entry.checkpointId,
        createdAt: entry.createdAt,
        timeLabel: formatCheckpointTime(entry.checkpointId),
        preview: buildPreview(entry.content),
        segments: buildPreviewSegments(entry.content),
        isContextCompaction: entry.isContextCompaction,
        counts: manifest
          ? {
              addedCount: manifest.addedCount,
              modifiedCount: manifest.modifiedCount,
              deletedCount: manifest.deletedCount,
              pendingCount: manifest.pendingCount,
            }
          : null,
        entries: manifest?.entries ?? null,
      };
    });
  }, [activeConversationId, checkpointEntries, manifests]);

  // 会话切换时收起面板并清空展开/准备态。
  useEffect(() => {
    setIsOpen(false);
    setExpandedMessageIds(new Set());
    setPreparingMessageId(null);
    setLoadError(null);
    preparingRef.current = false;
  }, [activeConversationId]);

  // 面板开合状态上报给顶栏：打开时顶栏主区切换为 overflow: visible。
  useEffect(() => {
    onOpenChange?.(isOpen);
  }, [isOpen, onOpenChange]);

  // 回滚进行中（预览 / 准备）时强制收起：回滚本身会重建会话内容与节点列表。
  // 会话运行中保持打开——数据随工具改动实时刷新，只是不提供回滚入口。
  useEffect(() => {
    if (!isOpen) {
      return;
    }
    if (rollbackPreview || rollbackPreparingMessageId) {
      setIsOpen(false);
    }
  }, [isOpen, rollbackPreview, rollbackPreparingMessageId]);

  // 点击面板外部关闭（固定时忽略）。
  useEffect(() => {
    if (!isOpen) {
      return undefined;
    }
    const handleClickOutside = (event: MouseEvent): void => {
      if (isPinned) {
        return;
      }
      if (
        containerRef.current &&
        !containerRef.current.contains(event.target as Node)
      ) {
        setIsOpen(false);
      }
    };
    document.addEventListener("pointerdown", handleClickOutside, true);
    return () => {
      document.removeEventListener("pointerdown", handleClickOutside, true);
    };
  }, [isOpen, isPinned]);

  const toggleExpanded = useCallback((messageId: string): void => {
    setExpandedMessageIds((current) => {
      const next = new Set(current);
      if (next.has(messageId)) {
        next.delete(messageId);
      } else {
        next.add(messageId);
      }
      return next;
    });
  }, []);

  /** 点击文件行：在右侧面板打开该轮对该文件的 diff（before → after）。
   *  面板浮层挂在顶栏下方、不遮挡右侧面板，保持打开以便连点多个文件对比。 */
  const openDiffInPanel = useCallback(
    (checkpointId: string, path: string, changeType: string): void => {
      rightPanelEvents.emit("open-checkpoint-diff", {
        checkpointId,
        filePath: path,
        changeType,
      });
    },
    [],
  );

  const selectRollback = useCallback(
    (messageId: string): void => {
      if (preparingRef.current) {
        return;
      }
      preparingRef.current = true;
      setLoadError(null);
      setPreparingMessageId(messageId);

      void (async () => {
        try {
          // 目标可能仍在分页窗口之外：先加载进内存窗口，否则 handleRollback
          // 找不到它而静默失败（与回滚目标选择器同一处理）。
          const isReady = await ensureMessageLoaded(
            messageId,
            {
              isLoaded: (targetId) =>
                messagesRef.current.some((message) => message.id === targetId),
              hasMoreMessages: () => hasMoreMessagesRef.current,
              getLoadedCount: () => messagesRef.current.length,
              loadOlderMessages,
              isCancelled: () => false,
            },
            TARGET_LOAD_BUDGET,
          );
          if (!isReady) {
            setLoadError(t("rollbackPicker.loadFailed"));
            return;
          }
          setIsOpen(false);
          handleRollback(messageId);
        } finally {
          preparingRef.current = false;
          setPreparingMessageId((current) =>
            current === messageId ? null : current,
          );
        }
      })();
    },
    [handleRollback, loadOlderMessages, t],
  );

  const rollbackDisabled =
    isStreaming || isAborting || Boolean(rollbackPreview) || Boolean(rollbackPreparingMessageId);

  // 没有任何带检查点的消息时不渲染入口（新会话 / 检查点已关闭的会话）。
  if (nodes.length === 0) {
    return null;
  }

  const changeLabel = (changeType: string): string => {
    switch (changeType) {
      case "added":
        return t("topBar.timeline.changeAdded");
      case "deleted":
        return t("topBar.timeline.changeDeleted");
      case "pending":
        return t("topBar.timeline.changePending");
      default:
        return t("topBar.timeline.changeModified");
    }
  };

  return (
    <div className="top-bar-timeline-menu" ref={containerRef}>
      <button
        className={`icon-btn ghost top-bar-timeline-btn${isOpen ? " active" : ""}`}
        type="button"
        aria-label={t("topBar.timeline.title")}
        title={t("topBar.timeline.title")}
        aria-expanded={isOpen}
        onClick={() => setIsOpen((open) => !open)}
      >
        <History size={16} strokeWidth={1.8} />
      </button>
      {isOpen ? (
        <div className="top-bar-timeline-dropdown">
          <div className="top-bar-timeline-dropdown-header">
            <span className="top-bar-timeline-dropdown-title">
              {t("topBar.timeline.title")}
            </span>
            <div className="top-bar-timeline-dropdown-header-actions">
              <button
                type="button"
                className="top-bar-timeline-order-toggle"
                onClick={handleToggleOrder}
                title={
                  timelineOrder === "newest"
                    ? t("chat.userMessageRail.orderToggleToOldest")
                    : t("chat.userMessageRail.orderToggleToNewest")
                }
                aria-label={
                  timelineOrder === "newest"
                    ? t("chat.userMessageRail.orderToggleToOldest")
                    : t("chat.userMessageRail.orderToggleToNewest")
                }
              >
                {timelineOrder === "newest" ? (
                  <ArrowDownWideNarrow size={11} strokeWidth={2} aria-hidden="true" />
                ) : (
                  <ArrowUpNarrowWide size={11} strokeWidth={2} aria-hidden="true" />
                )}
                {timelineOrder === "newest"
                  ? t("chat.userMessageRail.orderNewest")
                  : t("chat.userMessageRail.orderOldest")}
              </button>
              <span className="top-bar-timeline-dropdown-count">
                {t("topBar.timeline.count", {
                  values: { count: nodes.length },
                })}
              </span>
              <button
                className={`top-bar-timeline-pin-btn${isPinned ? " active" : ""}`}
                type="button"
                aria-label={t("topBar.todo.pin")}
                title={t("topBar.todo.pin")}
                aria-pressed={isPinned}
                onClick={() => setIsPinned((pinned) => !pinned)}
              >
                <Pin size={13} strokeWidth={1.8} />
              </button>
            </div>
          </div>
          {loadError ? (
            <div className="top-bar-timeline-error" role="alert">
              {loadError}
            </div>
          ) : null}
          <ul className="top-bar-timeline-list">
            {nodes.map((node) => {
              const isExpanded = expandedMessageIds.has(node.messageId);
              const isPreparing = preparingMessageId === node.messageId;
              const fileCount = node.counts
                ? node.counts.addedCount +
                  node.counts.modifiedCount +
                  node.counts.deletedCount +
                  node.counts.pendingCount
                : 0;
              return (
                <li key={node.messageId} className="top-bar-timeline-item">
                  <div className="top-bar-timeline-rail" aria-hidden="true">
                    <span className="top-bar-timeline-dot" />
                  </div>
                  <div className="top-bar-timeline-body">
                    <div className="top-bar-timeline-row">
                      <button
                        className="top-bar-timeline-expand"
                        type="button"
                        aria-expanded={isExpanded}
                        aria-label={t("topBar.timeline.expand")}
                        title={t("topBar.timeline.expand")}
                        onClick={() => toggleExpanded(node.messageId)}
                      >
                        <ChevronRight
                          size={13}
                          strokeWidth={1.8}
                          className={`top-bar-timeline-chevron${isExpanded ? " expanded" : ""}`}
                        />
                      </button>
                      <span className="top-bar-timeline-time">
                        {node.timeLabel}
                      </span>
                      <span className="top-bar-timeline-preview" title={node.preview}>
                        <ContentChips segments={node.segments} />
                      </span>
                      {node.isContextCompaction ? (
                        <span className="top-bar-timeline-compaction">
                          {t("rollbackPicker.compactionBadge")}
                        </span>
                      ) : null}
                      {isPreparing ? (
                        <Loader2
                          size={13}
                          strokeWidth={1.8}
                          className="top-bar-timeline-spinner"
                          aria-hidden="true"
                        />
                      ) : rollbackDisabled ? null : (
                        <button
                          className="top-bar-timeline-rollback"
                          type="button"
                          aria-label={t("topBar.timeline.rollback")}
                          title={t("topBar.timeline.rollback")}
                          onClick={() => selectRollback(node.messageId)}
                        >
                          <Undo2 size={13} strokeWidth={1.8} />
                        </button>
                      )}
                    </div>
                    {node.counts && fileCount > 0 ? (
                      <div className="top-bar-timeline-stats">
                        {node.counts.addedCount > 0 ? (
                          <span className="top-bar-timeline-stat is-added">
                            {t("topBar.timeline.statAdded", {
                              values: { count: node.counts.addedCount },
                            })}
                          </span>
                        ) : null}
                        {node.counts.modifiedCount > 0 ? (
                          <span className="top-bar-timeline-stat is-modified">
                            {t("topBar.timeline.statModified", {
                              values: { count: node.counts.modifiedCount },
                            })}
                          </span>
                        ) : null}
                        {node.counts.deletedCount > 0 ? (
                          <span className="top-bar-timeline-stat is-deleted">
                            {t("topBar.timeline.statDeleted", {
                              values: { count: node.counts.deletedCount },
                            })}
                          </span>
                        ) : null}
                        {node.counts.pendingCount > 0 ? (
                          <span className="top-bar-timeline-stat is-pending">
                            {t("topBar.timeline.statPending", {
                              values: { count: node.counts.pendingCount },
                            })}
                          </span>
                        ) : null}
                      </div>
                    ) : null}
                    {isExpanded ? (
                      <div className="top-bar-timeline-files">
                        {node.entries === null ? (
                          <span className="top-bar-timeline-files-empty">
                            {t("common.loading")}
                          </span>
                        ) : node.entries.length === 0 ? (
                          <span className="top-bar-timeline-files-empty">
                            {t("topBar.timeline.emptyFiles")}
                          </span>
                        ) : (
                          <>
                            {node.entries
                              .slice(0, EXPANDED_FILE_LIMIT)
                              .map((entry) => (
                                <TimelineFileRow
                                  key={`${node.checkpointId}:${entry.path}`}
                                  checkpointId={node.checkpointId}
                                  entry={entry}
                                  conversationVersion={conversationVersion}
                                  isInteractive={entry.changeType !== "pending"}
                                  changeLabel={changeLabel}
                                  onOpenDiff={() =>
                                    openDiffInPanel(
                                      node.checkpointId,
                                      entry.path,
                                      entry.changeType,
                                    )
                                  }
                                />
                              ))}
                            {node.entries.length > EXPANDED_FILE_LIMIT ? (
                              <span className="top-bar-timeline-files-more">
                                {t("topBar.timeline.moreFiles", {
                                  values: {
                                    count:
                                      node.entries.length - EXPANDED_FILE_LIMIT,
                                  },
                                })}
                              </span>
                            ) : null}
                          </>
                        )}
                      </div>
                    ) : null}
                  </div>
                </li>
              );
            })}
          </ul>
        </div>
      ) : null}
    </div>
  );
};
