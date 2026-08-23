import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { useSettingsQuery } from "@/lib/query";
import type { Settings } from "@/types";

type Language = "zh" | "zh-TW" | "en" | "ja";

export type SettingsFormState = Omit<Settings, "language"> & {
  language: Language;
};

const normalizeLanguage = (lang?: string | null): Language => {
  if (!lang) return "zh";
  const normalized = lang.toLowerCase().replace(/_/g, "-");

  if (normalized === "zh") {
    return "zh";
  }

  if (
    normalized === "zh-tw" ||
    normalized.startsWith("zh-hant") ||
    normalized.startsWith("zh-hk") ||
    normalized.startsWith("zh-mo")
  ) {
    return "zh-TW";
  }

  if (normalized === "en" || normalized === "ja") {
    return normalized;
  }

  if (normalized.startsWith("zh")) {
    return "zh";
  }

  return "zh";
};

const isSupportedLanguage = (lang?: string | null): boolean => {
  if (!lang) return false;
  const normalized = lang.toLowerCase().replace(/_/g, "-");
  return (
    normalized === "en" || normalized === "ja" || normalized.startsWith("zh")
  );
};

const sanitizeDir = (value?: string | null): string | undefined => {
  if (!value) return undefined;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : undefined;
};

const toFormState = (
  serverData: Settings,
  language: Language,
): SettingsFormState => ({
  ...serverData,
  showInTray: serverData.showInTray ?? true,
  minimizeToTrayOnClose: serverData.minimizeToTrayOnClose ?? true,
  useAppWindowControls: serverData.useAppWindowControls ?? false,
  enableClaudePluginIntegration:
    serverData.enableClaudePluginIntegration ?? false,
  silentStartup: serverData.silentStartup ?? false,
  skipClaudeOnboarding: serverData.skipClaudeOnboarding ?? false,
  preserveCodexOfficialAuthOnSwitch:
    serverData.preserveCodexOfficialAuthOnSwitch ?? false,
  unifyCodexSessionHistory: serverData.unifyCodexSessionHistory ?? false,
  claudeConfigDir: sanitizeDir(serverData.claudeConfigDir),
  codexConfigDir: sanitizeDir(serverData.codexConfigDir),
  geminiConfigDir: sanitizeDir(serverData.geminiConfigDir),
  grokConfigDir: sanitizeDir(serverData.grokConfigDir),
  opencodeConfigDir: sanitizeDir(serverData.opencodeConfigDir),
  openclawConfigDir: sanitizeDir(serverData.openclawConfigDir),
  hermesConfigDir: sanitizeDir(serverData.hermesConfigDir),
  piConfigDir: sanitizeDir(serverData.piConfigDir),
  language,
});

const omitSyncStatus = (settings: Settings): unknown => ({
  ...settings,
  webdavSync: settings.webdavSync
    ? { ...settings.webdavSync, status: undefined }
    : settings.webdavSync,
  s3Sync: settings.s3Sync
    ? { ...settings.s3Sync, status: undefined }
    : settings.s3Sync,
});

export const onlySyncStatusChanged = (
  prev: Settings,
  next: Settings,
): boolean =>
  JSON.stringify(omitSyncStatus(prev)) === JSON.stringify(omitSyncStatus(next));

const mergeSyncStatus = (
  prev: SettingsFormState,
  incoming: Settings,
): SettingsFormState => ({
  ...prev,
  webdavSync: prev.webdavSync
    ? { ...prev.webdavSync, status: incoming.webdavSync?.status }
    : incoming.webdavSync,
  s3Sync: prev.s3Sync
    ? { ...prev.s3Sync, status: incoming.s3Sync?.status }
    : incoming.s3Sync,
});

export interface UseSettingsFormResult {
  settings: SettingsFormState | null;
  isLoading: boolean;
  initialLanguage: Language;
  updateSettings: (updates: Partial<SettingsFormState>) => void;
  resetSettings: (serverData: Settings | null) => void;
  readPersistedLanguage: () => Language;
  syncLanguage: (lang: Language) => void;
  getLatestSettings: () => SettingsFormState | null;
  markSettingsClean: () => void;
}

/**
 * useSettingsForm - 表单状态管理
 * 负责：
 * - 表单数据状态
 * - 表单字段更新
 * - 语言同步
 * - 表单重置
 */
export function useSettingsForm(): UseSettingsFormResult {
  const { i18n } = useTranslation();
  const { data, isLoading } = useSettingsQuery();

  const [settingsState, setSettingsState] = useState<SettingsFormState | null>(
    null,
  );

  const initialLanguageRef = useRef<Language>("zh");
  const isDirtyRef = useRef(false);
  const hasHydratedRef = useRef(false);
  const lastServerDataRef = useRef<Settings | null>(null);
  const settingsRef = useRef<SettingsFormState | null>(null);
  settingsRef.current = settingsState;

  const readPersistedLanguage = useCallback((): Language => {
    if (typeof window !== "undefined") {
      const stored = window.localStorage.getItem("language");
      if (isSupportedLanguage(stored)) {
        return normalizeLanguage(stored);
      }
    }
    return normalizeLanguage(i18n.language);
  }, [i18n]);

  const syncLanguage = useCallback(
    (lang: Language) => {
      const current = normalizeLanguage(i18n.language);
      if (current !== lang) {
        void i18n.changeLanguage(lang);
      }
    },
    [i18n],
  );

  const hydrateFromServer = useCallback(
    (serverData: Settings) => {
      const normalizedLanguage = normalizeLanguage(
        serverData.language ?? readPersistedLanguage(),
      );
      const next = toFormState(serverData, normalizedLanguage);
      settingsRef.current = next;
      setSettingsState(next);
      initialLanguageRef.current = normalizedLanguage;
      isDirtyRef.current = false;
      hasHydratedRef.current = true;
      syncLanguage(normalizedLanguage);
    },
    [readPersistedLanguage, syncLanguage],
  );

  // 初始化 / 跟随服务端设置。进行中的本地编辑不能被 sync-status 整表覆盖。
  useEffect(() => {
    if (!data) return;

    const prevServer = lastServerDataRef.current;
    lastServerDataRef.current = data;

    if (
      hasHydratedRef.current &&
      isDirtyRef.current &&
      prevServer &&
      onlySyncStatusChanged(prevServer, data)
    ) {
      setSettingsState((prev) => {
        if (!prev) return prev;
        const next = mergeSyncStatus(prev, data);
        settingsRef.current = next;
        return next;
      });
      return;
    }

    hydrateFromServer(data);
  }, [data, hydrateFromServer]);

  const updateSettings = useCallback(
    (updates: Partial<SettingsFormState>) => {
      isDirtyRef.current = true;
      setSettingsState((prev) => {
        const base =
          prev ??
          ({
            showInTray: true,
            minimizeToTrayOnClose: true,
            useAppWindowControls: false,
            enableClaudePluginIntegration: false,
            skipClaudeOnboarding: false,
            preserveCodexOfficialAuthOnSwitch: false,
            unifyCodexSessionHistory: false,
            language: readPersistedLanguage(),
          } as SettingsFormState);

        const next: SettingsFormState = {
          ...base,
          ...updates,
        };

        if (updates.language) {
          const normalized = normalizeLanguage(updates.language);
          next.language = normalized;
          syncLanguage(normalized);
        }

        settingsRef.current = next;
        return next;
      });
    },
    [readPersistedLanguage, syncLanguage],
  );

  const getLatestSettings = useCallback(() => settingsRef.current, []);

  const markSettingsClean = useCallback(() => {
    isDirtyRef.current = false;
  }, []);

  const resetSettings = useCallback(
    (serverData: Settings | null) => {
      if (!serverData) return;

      const normalizedLanguage = normalizeLanguage(
        serverData.language ?? readPersistedLanguage(),
      );

      const next = toFormState(serverData, normalizedLanguage);
      settingsRef.current = next;
      setSettingsState(next);
      isDirtyRef.current = false;
      hasHydratedRef.current = true;
      lastServerDataRef.current = serverData;
      syncLanguage(initialLanguageRef.current);
    },
    [readPersistedLanguage, syncLanguage],
  );

  return {
    settings: settingsState,
    isLoading,
    initialLanguage: initialLanguageRef.current,
    updateSettings,
    resetSettings,
    readPersistedLanguage,
    syncLanguage,
    getLatestSettings,
    markSettingsClean,
  };
}
