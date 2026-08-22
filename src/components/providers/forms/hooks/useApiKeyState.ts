import { useEffect, useState, useCallback, useRef } from "react";
import type { ProviderCategory } from "@/types";
import {
  getApiKeyFromConfig,
  setApiKeyInConfig,
  hasApiKeyField,
} from "@/utils/providerConfigUtils";

interface UseApiKeyStateProps {
  initialConfig?: string;
  onConfigChange: (config: string) => void;
  selectedPresetId: string | null;
  category?: ProviderCategory;
  appType?: string;
  apiKeyField?: string;
}

/**
 * 管理 API Key 输入状态
 * 自动同步 API Key 和 JSON 配置
 */
export function useApiKeyState({
  initialConfig,
  onConfigChange,
  selectedPresetId,
  category,
  appType,
  apiKeyField,
}: UseApiKeyStateProps) {
  const [apiKey, setApiKey] = useState(() => {
    if (initialConfig) {
      return getApiKeyFromConfig(initialConfig, appType);
    }
    return "";
  });

  const configRef = useRef(initialConfig);
  configRef.current = initialConfig;

  // 当外部通过 form.reset / 读取 live / JSON 编辑等方式更新配置时，同步回 API Key 状态
  // - 仅在 JSON 可解析时同步，避免用户编辑 JSON 过程中因临时无效导致输入框闪烁
  // - 不把 apiKey 放进 deps，避免与用户输入互相覆盖形成循环
  useEffect(() => {
    if (!initialConfig) return;

    try {
      JSON.parse(initialConfig);
    } catch {
      return;
    }

    const extracted = getApiKeyFromConfig(initialConfig, appType);
    setApiKey((prev) => (extracted !== prev ? extracted : prev));
  }, [initialConfig, appType]);

  const handleApiKeyChange = useCallback(
    (key: string) => {
      setApiKey(key);

      const configString = setApiKeyInConfig(
        configRef.current || "{}",
        key.trim(),
        {
          // 最佳实践：仅在"非官方/非云厂商类别"时补齐缺失字段
          // - 官方类别：不创建字段（UI 也会禁用输入框）
          // - 云厂商类别：通常使用专用鉴权字段，不自动创建 Anthropic key
          // - 未传入 category：按历史导入/自定义 provider 处理，允许补齐
          createIfMissing:
            category !== "official" && category !== "cloud_provider",
          appType,
          apiKeyField,
        },
      );

      onConfigChange(configString);
    },
    [category, appType, apiKeyField, onConfigChange],
  );

  const showApiKey = useCallback(
    (config: string, isEditMode: boolean) => {
      return (
        selectedPresetId !== null ||
        (isEditMode &&
          category !== "official" &&
          category !== "cloud_provider") ||
        (isEditMode && hasApiKeyField(config, appType))
      );
    },
    [selectedPresetId, category, appType],
  );

  return {
    apiKey,
    setApiKey,
    handleApiKeyChange,
    showApiKey,
  };
}
