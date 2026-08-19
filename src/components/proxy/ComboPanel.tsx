import { useMemo, useState } from "react";
import { Loader2, Plus, Trash2 } from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  useDeleteModelCombo,
  useModelCombos,
  useUpsertModelCombo,
} from "@/lib/query/proxy";
import type { ComboStrategy, ModelCombo } from "@/types/proxy";
import { useProvidersQuery } from "@/lib/query/queries";
import { getAppLabel } from "@/config/appConfig";
import {
  formatComboTargets,
  isReservedComboId,
  isValidComboId,
  normalizeComboId,
  parseComboTargets,
  resolveComboHop,
} from "@/utils/combo";
import { assignRoutingSlugs, providersInAssignOrder } from "@/utils/routingSlug";
import { extractErrorMessage } from "@/utils/errorUtils";

export function ComboPanel() {
  const { t } = useTranslation();
  const { data, isLoading, isError, refetch } = useModelCombos();
  const combos = data ?? [];
  const codexProviders = useProvidersQuery("codex");
  const claudeProviders = useProvidersQuery("claude");
  const desktopProviders = useProvidersQuery("claude-desktop");
  const upsert = useUpsertModelCombo();
  const remove = useDeleteModelCombo();
  const [id, setId] = useState("");
  const [targetsText, setTargetsText] = useState("");
  const [strategy, setStrategy] = useState<ComboStrategy>("failover");
  const [editingId, setEditingId] = useState<string | null>(null);

  const comboId = normalizeComboId(id);
  const canonical = useMemo(
    () => (comboId ? `combo/${comboId}` : "combo/{id}"),
    [comboId],
  );
  const parsedTargets = useMemo(
    () => parseComboTargets(targetsText),
    [targetsText],
  );
  const resolveApps = useMemo(
    () => [
      {
        appId: "codex" as const,
        providers: providersInAssignOrder(
          Object.values(codexProviders.data?.providers ?? {}),
        ),
      },
      {
        appId: "claude" as const,
        providers: providersInAssignOrder(
          Object.values(claudeProviders.data?.providers ?? {}),
        ),
      },
      {
        appId: "claude-desktop" as const,
        providers: providersInAssignOrder(
          Object.values(desktopProviders.data?.providers ?? {}),
        ),
      },
    ],
    [
      claudeProviders.data?.providers,
      codexProviders.data?.providers,
      desktopProviders.data?.providers,
    ],
  );

  const resetForm = () => {
    setId("");
    setTargetsText("");
    setStrategy("failover");
    setEditingId(null);
  };

  const loadCombo = (combo: ModelCombo) => {
    setEditingId(combo.id);
    setId(combo.id);
    setTargetsText(formatComboTargets(combo.targets ?? []));
    setStrategy(combo.strategy ?? "failover");
  };

  const strategyLabel = (value: ComboStrategy | undefined) =>
    value === "round-robin"
      ? t("proxy.combos.strategyRoundRobin", {
          defaultValue: "round-robin",
        })
      : t("proxy.combos.strategyFailover", { defaultValue: "failover" });

  const handleSave = async () => {
    if (!comboId) {
      toast.error(
        t("proxy.combos.idRequired", { defaultValue: "请填写 Combo id" }),
      );
      return;
    }
    if (isReservedComboId(comboId) || !isValidComboId(comboId)) {
      toast.error(
        t("proxy.combos.idInvalid", {
          defaultValue:
            "Combo id 须以字母或数字开头，可含 . _ -，最长 64，且不能是 combo",
        }),
      );
      return;
    }
    const parsed = parseComboTargets(targetsText);
    if (!parsed.ok) {
      const spec = parsed.error.spec;
      const message =
        parsed.error.kind === "nested_combo"
          ? t("proxy.combos.nestedCombo", {
              spec,
              defaultValue: "目标不能再指向 combo/…：{{spec}}",
            })
          : parsed.error.kind === "invalid_weight"
            ? t("proxy.combos.invalidWeight", {
                spec,
                defaultValue: "权重须为 1–10000：{{spec}}",
              })
            : parsed.error.kind === "duplicate_target"
              ? t("proxy.combos.duplicateTarget", {
                  spec,
                  defaultValue: "重复目标：{{spec}}",
                })
              : t("proxy.combos.invalidTarget", {
                  spec,
                  defaultValue: "目标须为 provider/model[:weight]：{{spec}}",
                });
      toast.error(message);
      return;
    }
    if (parsed.targets.length === 0) {
      toast.error(
        t("proxy.combos.targetsRequired", {
          defaultValue: "请至少填写一个 provider/model 目标",
        }),
      );
      return;
    }
    const idTaken = combos.some(
      (combo) =>
        combo.id.toLowerCase() === comboId.toLowerCase() &&
        combo.id.toLowerCase() !== editingId?.toLowerCase(),
    );
    if (idTaken) {
      toast.error(
        t("proxy.combos.idExists", {
          id: comboId,
          defaultValue: "Combo id {{id}} 已存在",
        }),
      );
      return;
    }
    const reservedSlugs = new Set(
      resolveApps.flatMap((app) => [
        ...assignRoutingSlugs(app.providers).values(),
      ]),
    );
    if (reservedSlugs.has(comboId.toLowerCase())) {
      toast.error(
        t("proxy.combos.idCollidesSlug", {
          id: comboId,
          defaultValue: "Combo id {{id}} 与供应商路由 slug 冲突",
        }),
      );
      return;
    }
    const renaming =
      Boolean(editingId) &&
      editingId!.toLowerCase() !== comboId.toLowerCase();
    const stickySource = combos.find(
      (combo) =>
        combo.id.toLowerCase() ===
        (editingId ?? comboId).toLowerCase(),
    );
    try {
      await upsert.mutateAsync({
        combo: {
          id: comboId,
          targets: parsed.targets,
          strategy,
          stickyLimit: stickySource?.stickyLimit ?? 1,
        },
        previousId: renaming && editingId ? editingId : undefined,
      });
      toast.success(t("proxy.combos.saved", { defaultValue: "Combo 已保存" }), {
        closeButton: true,
      });
      resetForm();
    } catch (error) {
      toast.error(
        extractErrorMessage(error) ||
          t("proxy.combos.saveFailed", { defaultValue: "保存 Combo 失败" }),
      );
    }
  };

  const handleDelete = async (rawId: string) => {
    if (
      !window.confirm(
        t("proxy.combos.deleteConfirm", {
          id: rawId,
          defaultValue: "确定删除 combo/{{id}}？",
        }),
      )
    ) {
      return;
    }
    try {
      await remove.mutateAsync(rawId);
      if (editingId === rawId) {
        resetForm();
      }
      toast.success(
        t("proxy.combos.deleted", { defaultValue: "Combo 已删除" }),
        { closeButton: true },
      );
    } catch (error) {
      toast.error(
        extractErrorMessage(error) ||
          t("proxy.combos.deleteFailed", { defaultValue: "删除 Combo 失败" }),
      );
    }
  };

  return (
    <div className="rounded-xl border border-border bg-muted/30 p-4 space-y-3">
      <div>
        <p className="text-xs font-medium">
          {t("proxy.combos.title", { defaultValue: "Combo 虚拟模型" })}
        </p>
        <p className="text-xs text-muted-foreground mt-1">
          {t("proxy.combos.description", {
            defaultValue:
              "请求 combo/{id} 会按目标列表转发。failover 按顺序尝试；round-robin 按权重选择第一跳，失败仍会继续后面的目标。Claude 选择器里是 anthropic/combo/{id}。Combo 由所有目录应用共享。",
          })}
        </p>
      </div>

      {isError ? (
        <div className="flex items-center justify-between gap-2">
          <p className="text-xs text-destructive">
            {t("proxy.combos.loadFailed", {
              defaultValue: "无法加载 Combo 列表",
            })}
          </p>
          <Button size="sm" variant="outline" onClick={() => void refetch()}>
            {t("common.retry", { defaultValue: "重试" })}
          </Button>
        </div>
      ) : null}
      {isLoading && combos.length === 0 ? (
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <Loader2 className="h-3.5 w-3.5 animate-spin" />
          {t("common.loading", { defaultValue: "加载中…" })}
        </div>
      ) : combos.length === 0 && !isError ? (
        <p className="text-xs text-muted-foreground">
          {t("proxy.combos.empty", {
            defaultValue: "还没有 Combo。用 kimi/k2 这种目标加一条。",
          })}
        </p>
      ) : combos.length === 0 ? null : (
        <div className="space-y-2">
          {combos.map((combo) => (
            <div
              key={combo.id}
              className="flex items-start justify-between gap-2 rounded-md border border-border bg-background/70 px-3 py-2"
            >
              <button
                type="button"
                className="min-w-0 text-left"
                onClick={() => loadCombo(combo)}
              >
                <p className="text-sm font-medium truncate">combo/{combo.id}</p>
                <p className="text-xs text-muted-foreground truncate">
                  {strategyLabel(combo.strategy)} ·{" "}
                  {formatComboTargets(combo.targets ?? []).replace(/\n/g, ", ")}
                </p>
              </button>
              <Button
                size="icon"
                variant="ghost"
                className="h-8 w-8 shrink-0"
                onClick={() => handleDelete(combo.id)}
                disabled={remove.isPending}
              >
                <Trash2 className="h-4 w-4" />
              </Button>
            </div>
          ))}
        </div>
      )}

      <div className="grid gap-3 sm:grid-cols-2">
        <div className="space-y-1.5">
          <Label className="text-xs">
            {t("proxy.combos.id", { defaultValue: "Combo id" })}
          </Label>
          <Input
            value={id}
            onChange={(event) => setId(event.target.value)}
            placeholder="main"
          />
          <p className="text-[11px] text-muted-foreground">{canonical}</p>
        </div>
        <div className="space-y-1.5">
          <Label className="text-xs">
            {t("proxy.combos.strategy", { defaultValue: "策略" })}
          </Label>
          <Select
            value={strategy}
            onValueChange={(value) => setStrategy(value as ComboStrategy)}
          >
            <SelectTrigger>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="failover">
                {strategyLabel("failover")}
              </SelectItem>
              <SelectItem value="round-robin">
                {strategyLabel("round-robin")}
              </SelectItem>
            </SelectContent>
          </Select>
        </div>
      </div>
      <div className="space-y-1.5">
        <Label className="text-xs">
          {t("proxy.combos.targets", {
            defaultValue: "目标（每行一个 provider/model[:weight]）",
          })}
        </Label>
        <Textarea
          value={targetsText}
          onChange={(event) => setTargetsText(event.target.value)}
          placeholder={"kimi/k2\ndeepseek/deepseek-v4:1"}
          rows={3}
        />
        {targetsText.trim() && !parsedTargets.ok ? (
          <p className="text-[11px] text-destructive">
            {parsedTargets.error.kind === "nested_combo"
              ? t("proxy.combos.nestedCombo", {
                  spec: parsedTargets.error.spec,
                  defaultValue: "目标不能再指向 combo/…：{{spec}}",
                })
              : parsedTargets.error.kind === "invalid_weight"
                ? t("proxy.combos.invalidWeight", {
                    spec: parsedTargets.error.spec,
                    defaultValue: "权重须为 1–10000：{{spec}}",
                  })
                : parsedTargets.error.kind === "duplicate_target"
                  ? t("proxy.combos.duplicateTarget", {
                      spec: parsedTargets.error.spec,
                      defaultValue: "重复目标：{{spec}}",
                    })
                  : t("proxy.combos.invalidTarget", {
                      spec: parsedTargets.error.spec,
                      defaultValue: "目标须为 provider/model[:weight]：{{spec}}",
                    })}
          </p>
        ) : null}
        {parsedTargets.ok && parsedTargets.targets.length > 0 ? (
          <div className="space-y-1.5">
            {parsedTargets.targets.map((target, index) => {
              const route = `${target.provider}/${target.model}`;
              const hops = resolveApps.map((app) => ({
                appId: app.appId,
                ...resolveComboHop(target, app.providers),
              }));
              const anyMatched = hops.some((hop) => hop.matched);
              const canResolveHops = resolveApps.some(
                (app) => app.providers.length > 0,
              );
              return (
                <div key={`${route}-${index}`} className="space-y-0.5">
                  <p className="text-[11px] text-muted-foreground">
                    {route}
                    {target.weight && target.weight !== 1
                      ? `:${target.weight}`
                      : ""}
                  </p>
                  <p className="text-[11px] text-muted-foreground">
                    {hops
                      .map((hop) =>
                        hop.matched
                          ? t("proxy.combos.hopMatched", {
                              app: getAppLabel(hop.appId),
                              slug: hop.assignedSlug ?? hop.providerId,
                              defaultValue: "{{app}}：{{slug}}",
                            })
                          : t("proxy.combos.hopUnmatched", {
                              app: getAppLabel(hop.appId),
                              defaultValue: "{{app}}：未匹配",
                            }),
                      )
                      .join(" · ")}
                  </p>
                  {!anyMatched && canResolveHops ? (
                    <p className="text-[11px] text-amber-600 dark:text-amber-500">
                      {t("proxy.combos.hopDropped", {
                        route,
                        defaultValue:
                          "请求时会跳过 {{route}}：没有目录应用的卡匹配这个 slug 或 id。",
                      })}
                    </p>
                  ) : null}
                </div>
              );
            })}
          </div>
        ) : null}
      </div>
      <div className="flex gap-2">
        <Button
          size="sm"
          onClick={handleSave}
          disabled={upsert.isPending || remove.isPending}
        >
          {upsert.isPending ? (
            <Loader2 className="h-4 w-4 animate-spin" />
          ) : (
            <Plus className="h-4 w-4" />
          )}
          {editingId
            ? t("proxy.combos.update", { defaultValue: "更新" })
            : t("proxy.combos.add", { defaultValue: "添加" })}
        </Button>
        {editingId ? (
          <Button size="sm" variant="outline" onClick={resetForm}>
            {t("common.cancel", { defaultValue: "取消" })}
          </Button>
        ) : null}
      </div>
    </div>
  );
}
