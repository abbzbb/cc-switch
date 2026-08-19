import { useEffect, useRef, useState } from "react";
import { Loader2 } from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
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

const I32_MAX = 2_147_483_647;

function normalizeSidecar(settings: SidecarSettings): SidecarSettings {
  return {
    webSearch: {
      ...settings.webSearch,
      model: settings.webSearch.model?.trim() || null,
    },
    vision: {
      ...settings.vision,
      model: settings.vision.model?.trim() || null,
    },
  };
}

function sidecarKey(settings: SidecarSettings): string {
  const normalized = normalizeSidecar(settings);
  return JSON.stringify({
    webSearch: {
      enabled: normalized.webSearch.enabled,
      backend: normalized.webSearch.backend,
      model: normalized.webSearch.model,
      maxSearchesPerTurn: normalized.webSearch.maxSearchesPerTurn,
      timeoutMs: normalized.webSearch.timeoutMs,
    },
    vision: {
      enabled: normalized.vision.enabled,
      backend: normalized.vision.backend,
      model: normalized.vision.model,
      maxDescriptionsPerTurn: normalized.vision.maxDescriptionsPerTurn,
      timeoutMs: normalized.vision.timeoutMs,
    },
  });
}

export function SidecarPanel({
  proxyRunning = false,
}: {
  proxyRunning?: boolean;
}) {
  const { t } = useTranslation();
  const { data: settings, isLoading, isError, refetch } = useSidecarSettings();
  const update = useUpdateSidecarSettings();
  const [draft, setDraft] = useState<SidecarSettings | null>(null);
  const [saving, setSaving] = useState(false);
  const draftRef = useRef<SidecarSettings | null>(null);
  const acknowledgedRef = useRef<SidecarSettings | null>(null);
  const dirtyRef = useRef(false);
  const queuedRef = useRef<{
    settings: SidecarSettings;
    gen: number;
  } | null>(null);
  const writingRef = useRef(false);
  const editGenRef = useRef(0);

  const replaceDraft = (next: SidecarSettings) => {
    draftRef.current = next;
    setDraft(next);
  };

  useEffect(() => {
    if (!settings) return;
    if (dirtyRef.current || writingRef.current || queuedRef.current) return;
    acknowledgedRef.current = settings;
    replaceDraft(settings);
  }, [settings]);

  const flush = async () => {
    if (writingRef.current) return;
    writingRef.current = true;
    setSaving(true);
    try {
      while (queuedRef.current) {
        const payload = normalizeSidecar(queuedRef.current.settings);
        const gen = queuedRef.current.gen;
        queuedRef.current = null;
        if (
          acknowledgedRef.current &&
          sidecarKey(payload) === sidecarKey(acknowledgedRef.current)
        ) {
          if (editGenRef.current === gen) {
            dirtyRef.current = false;
            replaceDraft(payload);
          }
          continue;
        }
        try {
          const saved = await update.mutateAsync(payload);
          acknowledgedRef.current = saved;
          if (queuedRef.current) continue;
          if (editGenRef.current !== gen) continue;
          dirtyRef.current = false;
          replaceDraft(saved);
        } catch (error) {
          if (queuedRef.current) continue;
          toast.error(
            extractErrorMessage(error) ||
              t("proxy.sidecars.saveFailed", {
                defaultValue: "保存 Sidecar 设置失败",
              }),
          );
          if (editGenRef.current !== gen) continue;
          dirtyRef.current = false;
          if (acknowledgedRef.current) {
            replaceDraft(acknowledgedRef.current);
          }
        }
      }
    } finally {
      writingRef.current = false;
      setSaving(false);
      if (queuedRef.current) {
        void flush();
      }
    }
  };

  const save = (next: SidecarSettings) => {
    const normalized = normalizeSidecar(next);
    replaceDraft(normalized);
    if (
      acknowledgedRef.current &&
      sidecarKey(normalized) === sidecarKey(acknowledgedRef.current)
    ) {
      dirtyRef.current = false;
      return;
    }
    dirtyRef.current = true;
    queuedRef.current = { settings: normalized, gen: editGenRef.current };
    void flush();
  };

  const patch = (
    updater: (current: SidecarSettings) => SidecarSettings,
    persist: boolean,
  ) => {
    const current = draftRef.current;
    if (!current) return;
    const next = updater(current);
    if (persist) {
      save(next);
    } else {
      editGenRef.current += 1;
      dirtyRef.current = true;
      replaceDraft(next);
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

  const confirmEnable = (kind: "webSearch" | "vision") => {
    return window.confirm(
      t("proxy.sidecars.enableConfirm", {
        defaultValue:
          "打开后，已登录的 Claude Pro/Max 或 ChatGPT Official 会为非官方请求执行搜索或识图，并消耗对应订阅额度。需要接管已回写 inbound 头（或重启代理）后才会生效。确定打开{{kind}}？",
        kind:
          kind === "webSearch"
            ? t("proxy.sidecars.webSearch", { defaultValue: "Web Search" })
            : t("proxy.sidecars.vision", { defaultValue: "Vision" }),
      }),
    );
  };

  if (isLoading && !draft) {
    return (
      <div className="rounded-xl border border-border bg-muted/30 p-4 flex items-center gap-2 text-sm text-muted-foreground">
        <Loader2 className="h-4 w-4 animate-spin" />
        {t("common.loading", { defaultValue: "加载中…" })}
      </div>
    );
  }

  if (isError && !draft) {
    return (
      <div className="rounded-xl border border-border bg-muted/30 p-4 flex items-center justify-between gap-3">
        <p className="text-sm text-destructive">
          {t("proxy.sidecars.loadFailed", {
            defaultValue: "无法加载 Sidecar 设置",
          })}
        </p>
        <Button size="sm" variant="outline" onClick={() => void refetch()}>
          {t("common.retry", { defaultValue: "重试" })}
        </Button>
      </div>
    );
  }

  if (!draft) {
    return null;
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
              "默认关闭。打开后，非官方模型上的 hosted web_search 会改成函数调用，由已登录的 Claude Pro/Max 或 ChatGPT Official 执行。纯文本模型收到图片时先描述再转发。未登录对应账号时保持原样。接管必须已把 x-cc-switch-proxy 写进 live 配置（重新接管或重启代理），客户端只发 PROXY_MANAGED 时不会花钱。",
          })}
        </p>
        {!proxyRunning ? (
          <p className="text-xs text-muted-foreground mt-1">
            {t("proxy.sidecars.proxyStopped", {
              defaultValue: "代理未运行时仍可改设置，启动并完成接管后才会生效。",
            })}
          </p>
        ) : null}
      </div>

      <div className="space-y-3">
        <div className="flex items-center justify-between gap-3">
          <Label htmlFor="sidecar-web-search" className="text-sm">
            {t("proxy.sidecars.webSearch", { defaultValue: "Web Search" })}
          </Label>
          <Switch
            id="sidecar-web-search"
            checked={draft.webSearch.enabled}
            disabled={saving}
            onCheckedChange={(enabled) => {
              if (enabled && !draft.webSearch.enabled && !confirmEnable("webSearch")) {
                return;
              }
              patch(
                (current) => ({
                  ...current,
                  webSearch: { ...current.webSearch, enabled },
                }),
                true,
              );
            }}
          />
        </div>
        {draft.webSearch.enabled ? (
          <>
            <div className="space-y-1">
              <Label className="text-xs text-muted-foreground">
                {t("proxy.sidecars.webSearchBackend", {
                  defaultValue: "搜索后端",
                })}
              </Label>
              <Select
                value={draft.webSearch.backend}
                disabled={saving}
                onValueChange={(backend: SidecarBackend) =>
                  patch(
                    (current) => ({
                      ...current,
                      webSearch: { ...current.webSearch, backend },
                    }),
                    true,
                  )
                }
              >
                <SelectTrigger>
                  <SelectValue>
                    {backendLabel(draft.webSearch.backend)}
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
              value={draft.webSearch.model ?? ""}
              placeholder={t("proxy.sidecars.modelPlaceholder", {
                defaultValue: "留空则用后端默认模型",
              })}
              onChange={(model) =>
                patch(
                  (current) => ({
                    ...current,
                    webSearch: {
                      ...current.webSearch,
                      model: model || null,
                    },
                  }),
                  false,
                )
              }
              onCommit={() => {
                const current = draftRef.current;
                if (!current) return;
                void save({
                  ...current,
                  webSearch: {
                    ...current.webSearch,
                    model: current.webSearch.model?.trim() || null,
                  },
                });
              }}
            />
            <div className="grid grid-cols-2 gap-3">
              <NumberField
                id="sidecar-web-search-max"
                label={t("proxy.sidecars.maxSearches", {
                  defaultValue: "每轮最多搜索",
                })}
                value={draft.webSearch.maxSearchesPerTurn}
                min={1}
                max={20}
                onCommit={(maxSearchesPerTurn) =>
                  patch(
                    (current) => ({
                      ...current,
                      webSearch: { ...current.webSearch, maxSearchesPerTurn },
                    }),
                    true,
                  )
                }
              />
              <NumberField
                id="sidecar-web-search-timeout"
                label={t("proxy.sidecars.searchTimeoutMs", {
                  defaultValue: "搜索超时（毫秒）",
                })}
                value={draft.webSearch.timeoutMs}
                min={1}
                max={I32_MAX}
                onCommit={(timeoutMs) =>
                  patch(
                    (current) => ({
                      ...current,
                      webSearch: { ...current.webSearch, timeoutMs },
                    }),
                    true,
                  )
                }
              />
            </div>
          </>
        ) : null}

        <div className="flex items-center justify-between gap-3 pt-1">
          <Label htmlFor="sidecar-vision" className="text-sm">
            {t("proxy.sidecars.vision", { defaultValue: "Vision" })}
          </Label>
          <Switch
            id="sidecar-vision"
            checked={draft.vision.enabled}
            disabled={saving}
            onCheckedChange={(enabled) => {
              if (enabled && !draft.vision.enabled && !confirmEnable("vision")) {
                return;
              }
              patch(
                (current) => ({
                  ...current,
                  vision: { ...current.vision, enabled },
                }),
                true,
              );
            }}
          />
        </div>
        {draft.vision.enabled ? (
          <>
            <div className="space-y-1">
              <Label className="text-xs text-muted-foreground">
                {t("proxy.sidecars.visionBackend", {
                  defaultValue: "识图后端",
                })}
              </Label>
              <Select
                value={draft.vision.backend}
                disabled={saving}
                onValueChange={(backend: SidecarBackend) =>
                  patch(
                    (current) => ({
                      ...current,
                      vision: { ...current.vision, backend },
                    }),
                    true,
                  )
                }
              >
                <SelectTrigger>
                  <SelectValue>{backendLabel(draft.vision.backend)}</SelectValue>
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
              value={draft.vision.model ?? ""}
              placeholder={t("proxy.sidecars.modelPlaceholder", {
                defaultValue: "留空则用后端默认模型",
              })}
              onChange={(model) =>
                patch(
                  (current) => ({
                    ...current,
                    vision: {
                      ...current.vision,
                      model: model || null,
                    },
                  }),
                  false,
                )
              }
              onCommit={() => {
                const current = draftRef.current;
                if (!current) return;
                void save({
                  ...current,
                  vision: {
                    ...current.vision,
                    model: current.vision.model?.trim() || null,
                  },
                });
              }}
            />
            <div className="grid grid-cols-2 gap-3">
              <NumberField
                id="sidecar-vision-max"
                label={t("proxy.sidecars.maxDescriptions", {
                  defaultValue: "每轮最多描述",
                })}
                value={draft.vision.maxDescriptionsPerTurn}
                min={0}
                max={32}
                onCommit={(maxDescriptionsPerTurn) =>
                  patch(
                    (current) => ({
                      ...current,
                      vision: { ...current.vision, maxDescriptionsPerTurn },
                    }),
                    true,
                  )
                }
              />
              <NumberField
                id="sidecar-vision-timeout"
                label={t("proxy.sidecars.visionTimeoutMs", {
                  defaultValue: "识图超时（毫秒）",
                })}
                value={draft.vision.timeoutMs}
                min={1}
                max={I32_MAX}
                onCommit={(timeoutMs) =>
                  patch(
                    (current) => ({
                      ...current,
                      vision: { ...current.vision, timeoutMs },
                    }),
                    true,
                  )
                }
              />
            </div>
          </>
        ) : null}
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
  onChange,
  onCommit,
}: {
  id: string;
  label: string;
  value: string;
  placeholder?: string;
  disabled?: boolean;
  onChange: (value: string) => void;
  onCommit: () => void;
}) {
  return (
    <div className="space-y-1">
      <Label htmlFor={id} className="text-xs text-muted-foreground">
        {label}
      </Label>
      <Input
        id={id}
        value={value}
        placeholder={placeholder}
        disabled={disabled}
        onChange={(event) => onChange(event.target.value)}
        onBlur={onCommit}
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
  onChange,
  onCommit,
}: {
  id: string;
  label: string;
  value: number;
  min?: number;
  max?: number;
  disabled?: boolean;
  onChange?: (value: number) => void;
  onCommit: (value: number) => void;
}) {
  const [text, setText] = useState(String(value));
  useEffect(() => {
    setText(String(value));
  }, [value]);

  const clamp = (parsed: number) => {
    let rounded = Math.round(parsed);
    if (min !== undefined) rounded = Math.max(min, rounded);
    if (max !== undefined) rounded = Math.min(max, rounded);
    return rounded;
  };

  const commit = () => {
    if (text.trim() === "") {
      setText(String(value));
      return;
    }
    const parsed = Number(text);
    if (!Number.isFinite(parsed)) {
      setText(String(value));
      return;
    }
    const rounded = clamp(parsed);
    setText(String(rounded));
    onCommit(rounded);
  };

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
        onChange={(event) => {
          const next = event.target.value;
          setText(next);
          if (next.trim() === "") return;
          const parsed = Number(next);
          if (Number.isFinite(parsed)) {
            onChange?.(clamp(parsed));
          }
        }}
        onBlur={commit}
      />
    </div>
  );
}
