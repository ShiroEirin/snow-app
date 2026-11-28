import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import type { CheckpointDiffPage } from "../../../preload";
import { useI18n } from "../../i18n";
import { DiffViewer } from "../rightPanel/DiffViewer";

/** 回滚链来源：明细由 Rust 检查点服务按页计算，不在列表阶段生成。
 *  path 为检查点链上的文件路径（摘要 / 变更列表返回的路径）。 */
export type CheckpointDiffSource = {
  checkpointIds: string[];
  workDir: string;
  path: string;
};

/** 分页读取 diff 的通道：入参为起始展示行、行数与字节上限。 */
export type CheckpointDiffReader = (
  startRow: number,
  maxRows: number,
  maxBytes: number,
) => Promise<CheckpointDiffPage>;

/** 单页展示行数与字节上限：大文件按页追加，避免一次性传输整份 diff。 */
const PAGE_ROWS = 1000;
const PAGE_BYTES = 512 * 1024;

/** 回滚链 diff 的默认读取通道（readCheckpointDiffPage）。 */
const readRollbackChainPage = (
  source: CheckpointDiffSource,
): CheckpointDiffReader => {
  const { checkpointIds, workDir, path } = source;
  return (startRow, maxRows, maxBytes) =>
    window.snow.readCheckpointDiffPage(
      checkpointIds,
      workDir,
      path,
      startRow,
      maxRows,
      maxBytes,
    );
};

type LoadedDiff = {
  content: string;
  isBinary: boolean;
  tooLarge: boolean;
  shownRows: number;
  totalRows: number;
  revision: string;
};

type CheckpointDiffViewerProps = {
  /** 回滚链来源（与 read 二选一）：默认用 readCheckpointDiffPage 读取。 */
  source?: CheckpointDiffSource;
  /** 自定义分页读取通道（如 manifest 条目 diff）；优先于 source。 */
  read?: CheckpointDiffReader;
  /** 标题栏展示的路径。 */
  displayPath: string;
  changeType: string;
};

/**
 * 检查点 diff 明细：首屏只取一页，滚动到末尾时自动追加下一页。
 * 两次翻页之间文件被修改（内容版本变化）时丢弃已加载内容并从头读取。
 */
export function CheckpointDiffViewer({
  source,
  read,
  displayPath,
  changeType,
}: CheckpointDiffViewerProps): React.JSX.Element {
  const { t } = useI18n();
  const [loaded, setLoaded] = useState<LoadedDiff | null>(null);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const loadedRef = useRef<LoadedDiff | null>(null);
  const loadingRef = useRef(false);
  /** 请求代际：来源变化或卸载后，在途请求的结果直接丢弃。 */
  const generationRef = useRef(0);
  const sentinelRef = useRef<HTMLDivElement>(null);
  const reader = useMemo<CheckpointDiffReader>(() => {
    if (read) {
      return read;
    }
    if (source) {
      return readRollbackChainPage(source);
    }
    return () => Promise.reject(new Error("Checkpoint diff reader is missing"));
  }, [read, source]);
  const readerRef = useRef(reader);
  readerRef.current = reader;
  const sourceKey = useMemo(
    () =>
      read
        ? `custom:${displayPath}`
        : JSON.stringify([
            source?.checkpointIds,
            source?.workDir,
            source?.path,
          ]),
    [read, source, displayPath],
  );

  const loadNextPage = useCallback(async (): Promise<void> => {
    if (loadingRef.current) {
      return;
    }
    loadingRef.current = true;
    setLoading(true);
    setError("");
    const generation = generationRef.current;
    const readPage = (startRow: number) =>
      readerRef.current(startRow, PAGE_ROWS, PAGE_BYTES);
    try {
      let previous = loadedRef.current;
      let page = await readPage(previous?.shownRows ?? 0);
      if (previous && page.revision !== previous.revision) {
        previous = null;
        page = await readPage(0);
      }
      if (generation !== generationRef.current) {
        return;
      }
      const next: LoadedDiff = {
        content: (previous?.content ?? "") + page.content,
        isBinary: page.isBinary,
        tooLarge: page.tooLarge,
        shownRows: page.nextRow,
        totalRows: page.totalRows,
        revision: page.revision,
      };
      loadedRef.current = next;
      setLoaded(next);
    } catch (loadError) {
      if (generation === generationRef.current) {
        setError(
          loadError instanceof Error ? loadError.message : String(loadError),
        );
      }
    } finally {
      if (generation === generationRef.current) {
        loadingRef.current = false;
        setLoading(false);
      }
    }
  }, []);

  useEffect(() => {
    generationRef.current += 1;
    loadedRef.current = null;
    loadingRef.current = false;
    setLoaded(null);
    void loadNextPage();
    return () => {
      generationRef.current += 1;
    };
  }, [sourceKey, loadNextPage]);

  const hasMore = loaded !== null && loaded.shownRows < loaded.totalRows;

  // 加载区进入可视范围即追加下一页；加载完成后重新观察，区域仍可见时继续追加。
  useEffect(() => {
    const sentinel = sentinelRef.current;
    if (!sentinel || !hasMore || loading || error) {
      return;
    }
    const observer = new IntersectionObserver((entries) => {
      if (entries.some((entry) => entry.isIntersecting)) {
        void loadNextPage();
      }
    });
    observer.observe(sentinel);
    return () => observer.disconnect();
  }, [hasMore, loading, error, loadNextPage]);

  const footer =
    loaded && (hasMore || error) ? (
      <div className="checkpoint-diff-more" ref={sentinelRef}>
        {error ? (
          <span className="checkpoint-diff-more-error" title={error}>
            {error}
          </span>
        ) : (
          <span>
            {t("chat.fileChanges.rowsProgress", {
              values: { shown: loaded.shownRows, total: loaded.totalRows },
            })}
          </span>
        )}
        <button
          type="button"
          className="checkpoint-diff-more-btn"
          disabled={loading}
          onClick={() => void loadNextPage()}
        >
          {loading
            ? t("chat.fileChanges.loadingMore")
            : t("chat.fileChanges.loadMore")}
        </button>
      </div>
    ) : null;

  return (
    <DiffViewer
      selectedFile={{
        path: displayPath,
        oldPath: null,
        indexStatus: "",
        workdirStatus: "",
        status: changeType,
      }}
      diffResult={
        loaded
          ? { content: loaded.content, isBinary: loaded.isBinary, error: "" }
          : error
            ? { content: "", isBinary: false, error }
            : null
      }
      diffLoading={loaded === null && !error}
      contentFooter={footer}
      emptyMessage={
        loaded?.tooLarge ? t("chat.fileChanges.tooLargeRemote") : undefined
      }
    />
  );
}
