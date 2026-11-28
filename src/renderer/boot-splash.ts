/**
 * Boot splash — lightweight, CSP-compliant.
 *
 * Runs before the main React bundle:
 *   1. Writes the Han Feizi tagline in the user's locale. Locale detection
 *      mirrors I18nProvider: localStorage("snow.locale") -> navigator.language
 *      -> fallback "en".
 *   2. The splash layer lives outside #root. Once React commits its first
 *      content and that frame is painted, the splash fades out and is removed,
 *      instead of being hard-replaced by the app.
 *
 * This module intentionally avoids importing the full i18n resources
 * to keep the boot chunk minimal.
 */

type BootLocale = "en" | "zh-CN" | "zh-TW";

const SLOGANS: Record<BootLocale, string> = {
  en: "Times alter, circumstances shift",
  "zh-CN": "\u4E16\u5F02\u5219\u4E8B\u5F02\uFF0C\u4E8B\u5F02\u5219\u5907\u53D8",
  "zh-TW": "\u4E16\u7570\u5247\u4E8B\u7570\uFF0C\u4E8B\u7570\u5247\u5099\u8B8A",
};

const STORAGE_KEY = "snow.locale";

/** Must match the opacity transition duration of #boot-loader in index.html. */
const LEAVE_DURATION_MS = 560;

const normalizeLocale = (
  value: string | null | undefined,
): BootLocale | null => {
  if (!value) {
    return null;
  }

  if (value in SLOGANS) {
    return value as BootLocale;
  }

  const v = value.toLowerCase();

  if (v.startsWith("zh-tw") || v.startsWith("zh-hant")) {
    return "zh-TW";
  }

  if (v.startsWith("zh-cn") || v.startsWith("zh-hans") || v === "zh") {
    return "zh-CN";
  }

  if (v.startsWith("en")) {
    return "en";
  }

  return null;
};

const detectLocale = (): BootLocale => {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    const normalized = normalizeLocale(stored);

    if (normalized) {
      return normalized;
    }
  } catch {
    // localStorage may be unavailable (private mode, etc.) — fall through.
  }

  return normalizeLocale(navigator.language) ?? "en";
};

const splash = document.getElementById("boot-loader");
const root = document.getElementById("root");

const slogan = splash?.querySelector<HTMLElement>(".boot-slogan");

if (slogan) {
  const locale = detectLocale();
  slogan.textContent = SLOGANS[locale] ?? SLOGANS.en;
  slogan.lang = locale;
}

const dismiss = (): void => {
  if (!splash) {
    return;
  }
  splash.classList.add("is-leaving");
  window.setTimeout(() => splash.remove(), LEAVE_DURATION_MS + 80);
};

// Double rAF: start fading only after the app's first frame has been painted,
// so the splash dissolves into real content rather than an empty background.
const scheduleDismiss = (): void => {
  requestAnimationFrame(() => {
    requestAnimationFrame(dismiss);
  });
};

if (splash && root) {
  if (root.childElementCount > 0) {
    scheduleDismiss();
  } else {
    const observer = new MutationObserver(() => {
      if (root.childElementCount === 0) {
        return;
      }
      observer.disconnect();
      scheduleDismiss();
    });
    observer.observe(root, { childList: true });
  }
}
