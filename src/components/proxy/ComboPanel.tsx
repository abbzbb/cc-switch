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
import type { ComboStrategy, ComboTarget, ModelCombo } from "@/types/proxy";
import { extractErrorMessage } from "@/utils/errorUtils";

function formatTargets(targets: ComboTarget[]): string {
  return targets
    .map((target) => {
      const route = `${target.provider}/${target.model}`;
      return target.weight && target.weight !== 1
        ? `${route}:${target.weight}`
        : route;
    })
    .join("\n");
}

function parseTargets(text: string): ComboTarget[] {
  return text
    .split(/[\n,]+/)
    .map((line) => line.trim())
    .filter(Boolean)
    .map((spec) => {
      const weightMatch = spec.match(/:(\d+)$/);
      const weight =
        weightMatch && Number(weightMatch[1]) >= 1
          ? Number(weightMatch[1])
          : undefined;
      const route = weightMatch ? spec.slice(0, spec.lastIndexOf(":")) : spec;
      const slash = route.indexOf("/");
      if (slash <= 0 || slash === route.length - 1) {
        throw new Error(spec);
      }
      return {
        provider: route.slice(0, slash).trim(),
        model: route.slice(slash + 1).trim(),
        ...(weight ? { weight } : {}),
      };
    });
}

export function ComboPanel() {
  const { t } = useTranslation();
  const { data: combos = [], isLoading } = useModelCombos();
  const upsert = useUpsertModelCombo();
  const remove = useDeleteModelCombo();
  const [id, setId] = useState("");
  const [targetsText, setTargetsText] = useState("");
  const [strategy, setStrategy] = useState<ComboStrategy>("failover");
  const [editingId, setEditingId] = useState<string | null>(null);

  const canonical = useMemo(() => {
    const trimmed = id.trim();
    return trimmed ? `combo/${trimmed}` : "combo/{id}";
  }, [id]);

  const resetForm = () => {
    setId("");
    setTargetsText("");
    setStrategy("failover");
    setEditingId(null);
  };

  const loadCombo = (combo: ModelCombo) => {
    setEditingId(combo.id);
    setId(combo.id);
    setTargetsText(formatTargets(combo.targets ?? []));
    setStrategy(combo.strategy ?? "failover");
  };

  const handleSave = async () => {
    try {
      const targets = parseTargets(targetsText);
      if (!id.trim()) {
        toast.error(
          t("proxy.combos.idRequired", { defaultValue: "请填写 Combo id" }),
        );
        return;
      }
      if (targets.length === 0) {
        toast.error(
          t("proxy.combos.targetsRequired", {
            defaultValue: "请至少填写一个 provider/model 目标",
          }),
        );
        return;
      }
      await upsert.mutateAsync({
        id: id.trim(),
        targets,
        strategy,
        stickyLimit: 1,
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

  const handleDelete = async (comboId: string) => {
    try {
      await remove.mutateAsync(comboId);
      if (editingId === comboId) {
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
              "请求 combo/{id} 会按目标列表转发。failover 按顺序尝试；round-robin 按权重选择第一跳，失败仍会继续后面的目标。",
          })}
        </p>
      </div>

      {isLoading ? (
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <Loader2 className="h-3.5 w-3.5 animate-spin" />
          {t("common.loading", { defaultValue: "加载中…" })}
        </div>
      ) : combos.length === 0 ? (
        <p className="text-xs text-muted-foreground">
          {t("proxy.combos.empty", {
            defaultValue: "还没有 Combo。用 kimi/k2 这种目标加一条。",
          })}
        </p>
      ) : (
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
                  {combo.strategy === "round-robin"
                    ? "round-robin"
                    : "failover"}{" "}
                  · {formatTargets(combo.targets ?? []).replace(/\n/g, ", ")}
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
              <SelectItem value="failover">failover</SelectItem>
              <SelectItem value="round-robin">round-robin</SelectItem>
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
      </div>
      <div className="flex gap-2">
        <Button size="sm" onClick={handleSave} disabled={upsert.isPending}>
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
