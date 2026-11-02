import { AlertCircle, Network } from "lucide-react";
import { useMemo } from "react";
import { useI18n } from "../../../../i18n";
import type { ToolCallInfo } from "../utils/conversationTypes";
import { ToolCallNode } from "./shared/ToolCallNode";
import { localizeToolError } from "./shared/toolErrorDisplay";

type Props = { toolCall: ToolCallInfo };

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

const text = (value: unknown): string =>
  typeof value === "string" ? value : value == null ? "" : String(value);

const number = (value: unknown): number =>
  typeof value === "number" && Number.isFinite(value) ? value : 0;

const parseJson = (raw?: string): Record<string, unknown> | null => {
  if (!raw) return null;
  try {
    const parsed: unknown = JSON.parse(raw);
    return isRecord(parsed) ? parsed : null;
  } catch {
    return null;
  }
};

const asRecords = (value: unknown): Record<string, unknown>[] =>
  Array.isArray(value) ? value.filter(isRecord) : [];

/** Subcommand suffix of the MCP tool name (`allinluna-start` → `start`). */
const subcommandOf = (name: string): string =>
  name.startsWith("allinluna-") ? name.slice("allinluna-".length) : name;

/** One `label: value` row, reusing the existing generic meta classes. */
const MetaRow = ({
  label,
  value,
}: {
  label: string;
  value: string;
}): React.JSX.Element => (
  <div className="tool-call-meta-row">
    <span className="tool-call-meta-label">{label}</span>
    <span className="tool-call-meta-value">{value}</span>
  </div>
);

/**
 * All in Luna tool card.
 *
 * The server always answers with `{ success, runtimeDb, result }`, where
 * `result` is the runtime's own JSON. `allinluna-start` additionally decorates
 * compiled actions into `nextActions`, each carrying the `snowApp` relay plan
 * produced by the host adapter. The card keeps the conversation readable: it
 * surfaces the run identity, compile shape, and how many actions still need
 * relaying, and leaves raw payloads behind disclosures.
 *
 * Only pre-existing `tool-call-*` classes are used, so this adds no CSS.
 */
export const AllInLunaToolCall = ({ toolCall }: Props): React.JSX.Element => {
  const { t } = useI18n();
  const args = useMemo(
    () => parseJson(toolCall.arguments) ?? {},
    [toolCall.arguments],
  );
  const data = useMemo(() => parseJson(toolCall.result), [toolCall.result]);

  const inner = isRecord(data?.result) ? data.result : data;
  const success = data?.success !== false;
  const runRef = text(data?.runRef) || text(inner?.run_ref);
  const runtimeDb = text(data?.runtimeDb);

  const compilation = isRecord(inner?.compilation) ? inner.compilation : null;
  const pack = text(compilation?.pack);
  const taskCount = number(compilation?.task_count ?? compilation?.taskCount);
  const workGraphCount = number(
    compilation?.work_graph_count ?? compilation?.workGraphCount,
  );

  const metrics =
    isRecord(inner?.status) && isRecord(inner.status.extensions)
      ? inner.status.extensions.metrics
      : null;
  const metricRecord = isRecord(metrics) ? metrics : null;
  const blockedTasks = number(
    metricRecord?.blocked_tasks ?? metricRecord?.blockedTasks,
  );
  const completedTasks = number(
    metricRecord?.completed_tasks ?? metricRecord?.completedTasks,
  );

  // `nextActions` (decorated with `snowApp`) wins over the runtime's raw actions.
  const relayActions = asRecords(data?.nextActions);
  const rawActions = asRecords(inner?.actions);
  const actions = relayActions.length > 0 ? relayActions : rawActions;

  const isRunning =
    toolCall.status === "running" || toolCall.status === "pending";
  const failure = success ? "" : text(data?.stderr) || text(data?.hint);
  const errorText = failure ? localizeToolError(t, failure) : "";
  const subcommand = subcommandOf(toolCall.name);

  return (
    <ToolCallNode
      toolName={toolCall.name}
      badgeName={t("toolCall.allinLuna.title")}
      category="generic"
      displayName={`${t("toolCall.allinLuna.title")} · ${subcommand}`}
      displayNameTitle={runRef || toolCall.arguments}
      status={errorText ? "error" : toolCall.status}
      meta={
        <>
          {compilation ? (
            <span className="tool-call-log-level">{taskCount}</span>
          ) : null}
          {actions.length > 0 ? (
            <span className="tool-call-log-level">{actions.length}</span>
          ) : null}
          {blockedTasks > 0 ? (
            <span className="tool-call-log-level tool-call-log-level-error">
              {blockedTasks}
            </span>
          ) : null}
        </>
      }
    >
      <div className="tool-call-body">
        {errorText ? (
          <div className="tool-call-log-error">
            <AlertCircle size={14} aria-hidden="true" />
            <span>{errorText}</span>
          </div>
        ) : null}

        {runRef ? (
          <MetaRow label={t("toolCall.allinLuna.run")} value={runRef} />
        ) : null}
        {pack ? (
          <MetaRow label={t("toolCall.allinLuna.pack")} value={pack} />
        ) : null}
        {compilation ? (
          <MetaRow
            label={t("toolCall.allinLuna.tasks")}
            value={
              workGraphCount > 0
                ? `${taskCount} · ${workGraphCount} graph`
                : `${taskCount}`
            }
          />
        ) : null}
        {completedTasks > 0 ? (
          <MetaRow
            label={t("toolCall.allinLuna.done")}
            value={`${completedTasks}`}
          />
        ) : null}
        {runtimeDb ? (
          <MetaRow label={t("toolCall.allinLuna.store")} value={runtimeDb} />
        ) : null}

        {actions.length > 0 ? (
          <details className="tool-call-log-field" open>
            <summary>
              {t("toolCall.allinLuna.relay")} ({actions.length})
            </summary>
            <pre className="tool-call-log-pre">
              {actions
                .map((action, index) => {
                  const snowApp = isRecord(action.snowApp)
                    ? action.snowApp
                    : null;
                  const target = text(snowApp?.tool);
                  const kind =
                    text(action.kind) || text(action.tool) || "action";
                  const line = target
                    ? `${index + 1}. ${kind} → ${target}`
                    : `${index + 1}. ${kind} → (no mapping, report unresolved)`;
                  return line;
                })
                .join("\n")}
            </pre>
          </details>
        ) : null}

        {isRunning ? (
          <MetaRow label={t("toolCall.allinLuna.running")} value="…" />
        ) : null}

        {toolCall.result ? (
          <details className="tool-call-log-field">
            <summary>{t("toolCall.allinLuna.raw")}</summary>
            <pre className="tool-call-log-pre">{toolCall.result}</pre>
          </details>
        ) : null}

        {Object.keys(args).length > 0 ? (
          <details className="tool-call-log-field">
            <summary>{t("toolCall.allinLuna.arguments")}</summary>
            <pre className="tool-call-log-pre">
              {JSON.stringify(args, null, 2)}
            </pre>
          </details>
        ) : null}

        {!toolCall.result && !isRunning ? (
          <div className="tool-call-meta-row">
            <Network size={14} aria-hidden="true" />
            <span className="tool-call-meta-value">
              {t("toolCall.allinLuna.empty")}
            </span>
          </div>
        ) : null}
      </div>
    </ToolCallNode>
  );
};
