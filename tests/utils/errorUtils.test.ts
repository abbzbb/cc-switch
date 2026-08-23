import { describe, expect, it, vi } from "vitest";
import i18n from "i18next";
import en from "@/i18n/locales/en.json";
import ja from "@/i18n/locales/ja.json";
import zh from "@/i18n/locales/zh.json";
import {
  extractErrorMessage,
  translatePiProviderMutationError,
} from "@/utils/errorUtils";

describe("error utilities", () => {
  it("extracts Tauri string errors", () => {
    expect(extractErrorMessage("backend failed")).toBe("backend failed");
  });

  it("extracts localized AppError objects", () => {
    const err = {
      key: "usage_script.request_failed",
      message: "中文 (English)",
      zh: "中文",
      en: "English",
    };
    window.localStorage.setItem("language", "zh");
    expect(extractErrorMessage(err)).toBe("中文");
    window.localStorage.setItem("language", "en");
    expect(extractErrorMessage(err)).toBe("English");
    window.localStorage.removeItem("language");
  });

  it("uses i18n.t(key) when locale entries exist", () => {
    i18n.addResourceBundle("zh", "translation", zh, true, true);
    i18n.addResourceBundle("en", "translation", en, true, true);
    i18n.addResourceBundle("ja", "translation", ja, true, true);

    window.localStorage.setItem("language", "zh");
    expect(extractErrorMessage({ key: "s3.sync.disabled" })).toBe(
      "S3 同步未启用",
    );
    window.localStorage.setItem("language", "en");
    expect(extractErrorMessage({ key: "s3.sync.disabled" })).toBe(
      "S3 sync is disabled.",
    );
    window.localStorage.setItem("language", "ja");
    expect(extractErrorMessage({ key: "s3.sync.disabled" })).toBe(
      "S3 sync is disabled.",
    );
    window.localStorage.removeItem("language");
  });

  it("extracts {message} objects without locale fields", () => {
    expect(extractErrorMessage({ message: "plain object" })).toBe(
      "plain object",
    );
  });

  it("maps a simultaneous models.json write to a concise error", () => {
    const t = vi.fn((key: string) => key);

    expect(
      translatePiProviderMutationError(
        "Pi models.json changed outside CC Switch",
        t,
      ),
    ).toBe("pi.provider.writeConflict");
  });

  it("maps a duplicate Pi provider key to validation feedback", () => {
    const t = vi.fn((key: string) => key);

    expect(
      translatePiProviderMutationError(
        "无效输入: Pi provider key 'duplicate' already exists in models.json",
        t,
      ),
    ).toBe("pi.form.providerKeyDuplicate");
  });
});
