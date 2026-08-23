// Import the i18next singleton without importing `@/i18n`, whose module body
// initializes React integration. Error formatting must remain side-effect free.
import i18n from "i18next";

const readPreferredLanguage = (): string => {
  if (typeof window === "undefined") {
    return "zh";
  }
  try {
    return (window.localStorage.getItem("language") || "zh").toLowerCase();
  } catch {
    return "zh";
  }
};

const resolveI18nLanguage = (lang: string): "zh" | "zh-TW" | "en" | "ja" => {
  if (
    lang.startsWith("zh-tw") ||
    lang.startsWith("zh-hant") ||
    lang === "zh-tw"
  ) {
    return "zh-TW";
  }
  if (lang.startsWith("zh")) return "zh";
  if (lang.startsWith("ja")) return "ja";
  return "en";
};

const translateBackendKey = (key: string, lng: string): string => {
  const fullKey = `backend.${key}`;
  const translated = i18n.t(fullKey, { lng, defaultValue: "" });
  if (typeof translated !== "string") {
    return "";
  }
  const trimmed = translated.trim();
  if (!trimmed || trimmed === fullKey || trimmed.includes("{{")) {
    return "";
  }
  return trimmed;
};

const pickLocalizedMessage = (errObject: Record<string, unknown>): string => {
  const zh = typeof errObject.zh === "string" ? errObject.zh.trim() : "";
  const en = typeof errObject.en === "string" ? errObject.en.trim() : "";
  const message =
    typeof errObject.message === "string" ? errObject.message.trim() : "";
  const key = typeof errObject.key === "string" ? errObject.key.trim() : "";
  if (!zh && !en && !key) {
    return "";
  }
  const lang = readPreferredLanguage();
  const lng = resolveI18nLanguage(lang);
  const fromI18n = key ? translateBackendKey(key, lng) : "";

  // ja / zh-TW have no backend payload strings; prefer t(key) when present.
  if ((lng === "ja" || lng === "zh-TW") && fromI18n) {
    return fromI18n;
  }
  if (lng === "zh" && zh) return zh;
  if (lng === "en" && en) return en;
  if (fromI18n) return fromI18n;
  if (zh) return zh;
  if (en) return en;
  return message;
};

/**
 * 从各种错误对象中提取错误信息
 * @param error 错误对象
 * @returns 提取的错误信息字符串
 */
export const extractErrorMessage = (error: unknown): string => {
  if (!error) return "";
  if (typeof error === "string") {
    return error;
  }
  if (error instanceof Error && error.message.trim()) {
    return error.message;
  }

  if (typeof error === "object") {
    const errObject = error as Record<string, unknown>;

    const localized = pickLocalizedMessage(errObject);
    if (localized) {
      return localized;
    }

    const candidate = errObject.message ?? errObject.error ?? errObject.detail;
    if (typeof candidate === "string" && candidate.trim()) {
      return candidate;
    }

    const payload = errObject.payload;
    if (typeof payload === "string" && payload.trim()) {
      return payload;
    }
    if (payload && typeof payload === "object") {
      const payloadObj = payload as Record<string, unknown>;
      const localizedPayload = pickLocalizedMessage(payloadObj);
      if (localizedPayload) {
        return localizedPayload;
      }
      const payloadCandidate =
        payloadObj.message ?? payloadObj.error ?? payloadObj.detail;
      if (typeof payloadCandidate === "string" && payloadCandidate.trim()) {
        return payloadCandidate;
      }
    }
  }

  return "";
};

export const translatePiProviderMutationError = (
  message: string,
  t: (key: string, options?: Record<string, unknown>) => string,
): string => {
  if (!message) return "";

  if (
    message.includes("models.json changed") ||
    message.includes("changed outside CC Switch") ||
    message.includes("no longer present in models.json") ||
    message.includes("another value now owns the key")
  ) {
    return t("pi.provider.writeConflict");
  }

  if (message.includes("Pi provider") && message.includes("already exists")) {
    return t("pi.form.providerKeyDuplicate");
  }

  return "";
};

/**
 * 将已知的 MCP 相关后端错误（通常为中文硬编码）映射为 i18n 文案
 * 采用包含式匹配，尽量稳健地覆盖不同上下文的相似消息。
 * 若无法识别，返回空字符串以便调用方回退到原始 detail 或默认 i18n。
 */
export const translateMcpBackendError = (
  message: string,
  t: (key: string, opts?: any) => string,
): string => {
  if (!message) return "";
  const msg = String(message).trim();

  // 基础字段与结构校验相关
  if (msg.includes("MCP 服务器 ID 不能为空")) {
    return t("mcp.error.idRequired");
  }
  if (
    msg.includes("MCP 服务器定义必须为 JSON 对象") ||
    msg.includes("MCP 服务器条目必须为 JSON 对象") ||
    msg.includes("MCP 服务器条目缺少 server 字段") ||
    msg.includes("MCP 服务器 server 字段必须为 JSON 对象") ||
    msg.includes("MCP 服务器连接定义必须为 JSON 对象") ||
    msg.includes("MCP 服务器 '" /* 不是对象 */) ||
    msg.includes("不是对象") ||
    msg.includes("服务器配置必须是对象") ||
    msg.includes("MCP 服务器 name 必须为字符串") ||
    msg.includes("MCP 服务器 description 必须为字符串") ||
    msg.includes("MCP 服务器 homepage 必须为字符串") ||
    msg.includes("MCP 服务器 docs 必须为字符串") ||
    msg.includes("MCP 服务器 tags 必须为字符串数组") ||
    msg.includes("MCP 服务器 enabled 必须为布尔值")
  ) {
    return t("mcp.error.jsonInvalid");
  }
  if (msg.includes("MCP 服务器 type 必须是")) {
    return t("mcp.error.jsonInvalid");
  }

  // 必填字段
  if (
    msg.includes("stdio 类型的 MCP 服务器缺少 command 字段") ||
    msg.includes("必须包含 command 字段")
  ) {
    return t("mcp.error.commandRequired");
  }
  if (
    msg.includes("http 类型的 MCP 服务器缺少 url 字段") ||
    msg.includes("sse 类型的 MCP 服务器缺少 url 字段") ||
    msg.includes("必须包含 url 字段") ||
    msg === "URL 不能为空"
  ) {
    return t("mcp.wizard.urlRequired");
  }

  // 文件解析/序列化
  if (
    msg.includes("解析 ~/.claude.json 失败") ||
    msg.includes("解析 config.toml 失败") ||
    msg.includes("无法识别的 TOML 格式") ||
    msg.includes("TOML 内容不能为空")
  ) {
    return t("mcp.error.tomlInvalid");
  }
  if (msg.includes("序列化 config.toml 失败")) {
    return t("mcp.error.tomlInvalid");
  }

  return "";
};
