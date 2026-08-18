import { useEffect, useState } from "react";
import { Loader2 } from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import {
  useSidecarSettings,
  useUpdateSidecarSettings,
} from "@/lib/query/proxy";
import type { SidecarBackend, SidecarSettings } from "@/types/proxy";
import { extractErrorMessage } from "@/utils/errorUtils";

export function SidecarPanel() {
  const { t } = useTranslation();
  const { data: settings, isLoading } = useSidecarSettings();
  const update = useUpdateSidecarSettings();

  const save = async (next: SidecarSettings) => {
    try {
      await update.mutateAsync(next);
    } catch (error) {
      toast.error(
        extractErrorMessage(error) ||
          t("proxy.sidecars.saveFailed", {
            defaultValue: "保存 Sidecar 设置失败",
          }),
      );
    }
  };

  const backendLabel = (backend: SidecarBackend) => {
    switch (backend) {
      case "openai":
        return t("proxy.sidecars.backendOpenai", {
          defaultValue: "ChatGPT Official",
        });
      case "anthropic":
        return t("proxy.sidecars.backendAnthropic", {
          defaultValue: "Claude Pro/Max",
        });
      default:
        return t("proxy.sidecars.backendAuto", { defaultValue: "自动" });
    }
  };

  if (isLoading || !settings) {
    return (
      <div className="rounded-xl border border-border bg-muted/30 p-4 flex items-center gap-2 text-sm text-muted-foreground">
        <Loader2 className="h-4 w-4 animate-spin" />
        {t("common.loading", { defaultValue: "加载中…" })}
      </div>
    );
  }

  return (
    <div className="rounded-xl border border-border bg-muted/30 p-4 space-y-3">
      <div>
        <p className="text-xs font-medium">
          {t("proxy.sidecars.title", {
            defaultValue: "Web Search / Vision Sidecar",
          })}
        </p>
        <p className="text-xs text-muted-foreground mt-1">
          {t("proxy.sidecars.description", {
            defaultValue:
              "非官方模型上的 hosted web_search 会改成函数调用，由已登录的 Claude Pro/Max 或 ChatGPT Official 执行。纯文本模型收到图片时先描述再转发。未登录对应账号时保持原样。",
          })}
        </p>
      </div>

      <div className="space-y-3">
        <div className="flex items-center justify-between gap-3">
          <Label htmlFor="sidecar-web-search" className="text-sm">
            {t("proxy.sidecars.webSearch", { defaultValue: "Web Search" })}
          </Label>
          <Switch
            id="sidecar-web-search"
            checked={settings.webSearch.enabled}
            disabled={update.isPending}
            onCheckedChange={(enabled) =>
              void save({
                ...settings,
                webSearch: { ...settings.webSearch, enabled },
              })
            }
          />
        </div>
        <div className="space-y-1">
          <Label className="text-xs text-muted-foreground">
            {t("proxy.sidecars.webSearchBackend", {
              defaultValue: "搜索后端",
            })}
          </Label>
          <Select
            value={settings.webSearch.backend}
            disabled={update.isPending || !settings.webSearch.enabled}
            onValueChange={(backend: SidecarBackend) =>
              void save({
                ...settings,
                webSearch: { ...settings.webSearch, backend },
              })
            }
          >
            <SelectTrigger>
              <SelectValue>
                {backendLabel(settings.webSearch.backend)}
              </SelectValue>
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="auto">{backendLabel("auto")}</SelectItem>
              <SelectItem value="anthropic">
                {backendLabel("anthropic")}
              </SelectItem>
              <SelectItem value="openai">{backendLabel("openai")}</SelectItem>
            </SelectContent>
          </Select>
        </div>
        <TextField
          id="sidecar-web-search-model"
          label={t("proxy.sidecars.webSearchModel", {
            defaultValue: "搜索模型（可选）",
          })}
          value={settings.webSearch.model ?? ""}
          placeholder={t("proxy.sidecars.modelPlaceholder", {
            defaultValue: "留空则用后端默认模型",
          })}
          disabled={update.isPending || !settings.webSearch.enabled}
          onCommit={(model) =>
            void save({
              ...settings,
              webSearch: {
                ...settings.webSearch,
                model: model.trim() || null,
              },
            })
          }
        />
        <div className="grid grid-cols-2 gap-3">
          <NumberField
            id="sidecar-web-search-max"
            label={t("proxy.sidecars.maxSearches", {
              defaultValue: "每轮最多搜索",
            })}
            value={settings.webSearch.maxSearchesPerTurn}
            min={1}
            max={20}
            disabled={update.isPending || !settings.webSearch.enabled}
            onCommit={(maxSearchesPerTurn) =>
              void save({
                ...settings,
                webSearch: { ...settings.webSearch, maxSearchesPerTurn },
              })
            }
          />
          <NumberField
            id="sidecar-web-search-timeout"
            label={t("proxy.sidecars.searchTimeoutMs", {
              defaultValue: "搜索超时（毫秒）",
            })}
            value={settings.webSearch.timeoutMs}
            min={1}
            disabled={update.isPending || !settings.webSearch.enabled}
            onCommit={(timeoutMs) =>
              void save({
                ...settings,
                webSearch: { ...settings.webSearch, timeoutMs },
              })
            }
          />
        </div>

        <div className="flex items-center justify-between gap-3 pt-1">
          <Label htmlFor="sidecar-vision" className="text-sm">
            {t("proxy.sidecars.vision", { defaultValue: "Vision" })}
          </Label>
          <Switch
            id="sidecar-vision"
            checked={settings.vision.enabled}
            disabled={update.isPending}
            onCheckedChange={(enabled) =>
              void save({
                ...settings,
                vision: { ...settings.vision, enabled },
              })
            }
          />
        </div>
        <div className="space-y-1">
          <Label className="text-xs text-muted-foreground">
            {t("proxy.sidecars.visionBackend", {
              defaultValue: "识图后端",
            })}
          </Label>
          <Select
            value={settings.vision.backend}
            disabled={update.isPending || !settings.vision.enabled}
            onValueChange={(backend: SidecarBackend) =>
              void save({
                ...settings,
                vision: { ...settings.vision, backend },
              })
            }
          >
            <SelectTrigger>
              <SelectValue>{backendLabel(settings.vision.backend)}</SelectValue>
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="auto">{backendLabel("auto")}</SelectItem>
              <SelectItem value="anthropic">
                {backendLabel("anthropic")}
              </SelectItem>
              <SelectItem value="openai">{backendLabel("openai")}</SelectItem>
            </SelectContent>
          </Select>
        </div>
        <TextField
          id="sidecar-vision-model"
          label={t("proxy.sidecars.visionModel", {
            defaultValue: "识图模型（可选）",
          })}
          value={settings.vision.model ?? ""}
          placeholder={t("proxy.sidecars.modelPlaceholder", {
            defaultValue: "留空则用后端默认模型",
          })}
          disabled={update.isPending || !settings.vision.enabled}
          onCommit={(model) =>
            void save({
              ...settings,
              vision: {
                ...settings.vision,
                model: model.trim() || null,
              },
            })
          }
        />
        <div className="grid grid-cols-2 gap-3">
          <NumberField
            id="sidecar-vision-max"
            label={t("proxy.sidecars.maxDescriptions", {
              defaultValue: "每轮最多描述",
            })}
            value={settings.vision.maxDescriptionsPerTurn}
            min={0}
            max={32}
            disabled={update.isPending || !settings.vision.enabled}
            onCommit={(maxDescriptionsPerTurn) =>
              void save({
                ...settings,
                vision: { ...settings.vision, maxDescriptionsPerTurn },
              })
            }
          />
          <NumberField
            id="sidecar-vision-timeout"
            label={t("proxy.sidecars.visionTimeoutMs", {
              defaultValue: "识图超时（毫秒）",
            })}
            value={settings.vision.timeoutMs}
            min={1}
            disabled={update.isPending || !settings.vision.enabled}
            onCommit={(timeoutMs) =>
              void save({
                ...settings,
                vision: { ...settings.vision, timeoutMs },
              })
            }
          />
        </div>
      </div>
    </div>
  );
}

function TextField({
  id,
  label,
  value,
  placeholder,
  disabled,
  onCommit,
}: {
  id: string;
  label: string;
  value: string;
  placeholder?: string;
  disabled?: boolean;
  onCommit: (value: string) => void;
}) {
  const [text, setText] = useState(value);
  useEffect(() => {
    setText(value);
  }, [value]);

  return (
    <div className="space-y-1">
      <Label htmlFor={id} className="text-xs text-muted-foreground">
        {label}
      </Label>
      <Input
        id={id}
        value={text}
        placeholder={placeholder}
        disabled={disabled}
        onChange={(event) => setText(event.target.value)}
        onBlur={() => {
          if (text !== value) {
            onCommit(text);
          }
        }}
      />
    </div>
  );
}

function NumberField({
  id,
  label,
  value,
  min,
  max,
  disabled,
  onCommit,
}: {
  id: string;
  label: string;
  value: number;
  min?: number;
  max?: number;
  disabled?: boolean;
  onCommit: (value: number) => void;
}) {
  const [text, setText] = useState(String(value));
  useEffect(() => {
    setText(String(value));
  }, [value]);

  return (
    <div className="space-y-1">
      <Label htmlFor={id} className="text-xs text-muted-foreground">
        {label}
      </Label>
      <Input
        id={id}
        type="number"
        min={min}
        max={max}
        value={text}
        disabled={disabled}
        onChange={(event) => setText(event.target.value)}
        onBlur={() => {
          const parsed = Number(text);
          if (!Number.isFinite(parsed)) {
            setText(String(value));
            return;
          }
          const rounded = Math.round(parsed);
          if (rounded !== value) {
            onCommit(rounded);
          }
        }}
      />
    </div>
  );
}
