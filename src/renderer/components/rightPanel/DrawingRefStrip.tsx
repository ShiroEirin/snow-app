import { memo, type ReactNode } from "react";
import {
  AlertCircle,
  AtSign,
  ImagePlus,
  Images,
  Maximize2,
  Plus,
  Sparkles,
  X,
} from "lucide-react";
import { useI18n } from "../../i18n";

/** 参考图条目（图库相对路径 image/... 或本地绝对路径 + MIME）。 */
export type DrawingRefImage = {
  path: string;
  mimeType: string;
};

export type DrawingRefStripProps = {
  refImages: DrawingRefImage[];
  /** 当前模型可接受的参考图上限（按模型能力动态适配）。 */
  maxRefLimit: number;
  /** ComfyUI 工作流模式：文案区分「工作流没有该输入」与「模型不支持」。 */
  workflowDriven?: boolean;
  /** 当前模型是否明确不支持参考图（如 dall-e-3 / imagen）。 */
  referenceUnsupported: boolean;
  /** 点击「上传参考图」（原生多选对话框）。 */
  onUpload: () => void;
  /** 点击缩略图：在提示词中插入 `{{Image N}}` 引用。 */
  onInsert: (index: number) => void;
  /** 点击「查看」：在灯箱中放大该参考图。 */
  onPreview: (index: number) => void;
  /** 移除某张参考图。 */
  onRemove: (path: string) => void;
  renderImage: (props: {
    path: string;
    alt: string;
    className?: string;
  }) => ReactNode;
};

/**
 * 参考图缩略图条（画布区顶部固定）。
 *
 * 承载图生图的全部参考图交互：空态为虚线拖拽 / 上传引导框，有图时为横向
 * 可滚动的缩略图卡片条（编号角标 = 提示词 `{{Image N}}` 的引用号），
 * 末位常驻「添加」磁贴，卡片悬浮显示插入引用 / 放大 / 移除操作。
 * 画布区滚动时通过 `position: sticky` 固定在顶部，缩略图始终可见。
 */
export const DrawingRefStrip = memo(function DrawingRefStrip({
  refImages,
  maxRefLimit,
  workflowDriven,
  referenceUnsupported,
  onUpload,
  onInsert,
  onPreview,
  onRemove,
  renderImage,
}: DrawingRefStripProps): React.JSX.Element {
  const { t } = useI18n();
  const hasRefs = refImages.length > 0;
  const atLimit = refImages.length >= maxRefLimit;

  return (
    <div className="ai-drawing-ref-strip">
      {hasRefs ? (
        <>
          <div className="ai-drawing-ref-strip-head">
            <span className="ai-drawing-ref-strip-title">
              <Images size={12} strokeWidth={1.8} />
              <span>{t("rightPanel.aiDrawing.reference")}</span>
            </span>
            <span className="ai-drawing-ref-strip-count">
              {refImages.length} / {maxRefLimit}
            </span>
            <span
              className="ai-drawing-ref-strip-affordance"
              title={t("rightPanel.aiDrawing.refPlaceholderHint")}
            >
              <AtSign size={11} strokeWidth={1.8} />
              <span>{"{{Image N}}"}</span>
            </span>
            <button
              type="button"
              className="ai-drawing-ref-strip-upload"
              title={t("rightPanel.aiDrawing.uploadHint")}
              onClick={onUpload}
              disabled={atLimit}
            >
              <ImagePlus size={12} strokeWidth={1.8} />
              <span>{t("rightPanel.aiDrawing.uploadImage")}</span>
            </button>
          </div>
          <div className="ai-drawing-ref-strip-track">
            {refImages.map((ref, index) => (
              <div
                className="ai-drawing-ref-card"
                key={ref.path}
                title={t("rightPanel.aiDrawing.refInsert", {
                  values: { n: index + 1 },
                })}
              >
                <button
                  type="button"
                  className="ai-drawing-ref-card-main"
                  onClick={() => onInsert(index + 1)}
                >
                  {renderImage({
                    path: ref.path,
                    alt: t("toolCall.imagegen.refImage", {
                      defaultValue: "Reference image",
                    }),
                    className: "ai-drawing-ref-card-thumb",
                  })}
                </button>
                <span className="ai-drawing-ref-card-badge">{index + 1}</span>
                <div className="ai-drawing-ref-card-actions">
                  <button
                    type="button"
                    className="ai-drawing-ref-card-action"
                    title={t("rightPanel.aiDrawing.refOpenImage")}
                    onClick={(event) => {
                      event.stopPropagation();
                      onPreview(index);
                    }}
                  >
                    <Maximize2 size={12} strokeWidth={1.8} />
                  </button>
                  <button
                    type="button"
                    className="ai-drawing-ref-card-action ai-drawing-ref-card-action-remove"
                    title={t("rightPanel.aiDrawing.removeReference")}
                    onClick={(event) => {
                      event.stopPropagation();
                      onRemove(ref.path);
                    }}
                  >
                    <X size={12} strokeWidth={1.8} />
                  </button>
                </div>
              </div>
            ))}
            <button
              type="button"
              className="ai-drawing-ref-card ai-drawing-ref-card-add"
              title={
                atLimit
                  ? t("rightPanel.aiDrawing.refLimit", {
                      values: { count: maxRefLimit },
                    })
                  : t("rightPanel.aiDrawing.uploadHint")
              }
              onClick={onUpload}
              disabled={atLimit}
            >
              <Plus size={16} strokeWidth={1.8} />
            </button>
          </div>
        </>
      ) : (
        <button
          type="button"
          className="ai-drawing-ref-dropzone"
          title={t("rightPanel.aiDrawing.uploadHint")}
          onClick={onUpload}
        >
          <span className="ai-drawing-ref-dropzone-icon" aria-hidden="true">
            <Sparkles size={15} strokeWidth={1.6} />
          </span>
          <span className="ai-drawing-ref-dropzone-title">
            {t("rightPanel.aiDrawing.uploadImage")}
          </span>
        </button>
      )}

      {referenceUnsupported && (
        <div className="ai-drawing-model-warn">
          <AlertCircle size={13} strokeWidth={1.8} />
          <span>
            {t(
              workflowDriven
                ? "rightPanel.aiDrawing.refUnsupportedWorkflow"
                : "rightPanel.aiDrawing.modelUnsupportedRef",
            )}
          </span>
        </div>
      )}
    </div>
  );
});
