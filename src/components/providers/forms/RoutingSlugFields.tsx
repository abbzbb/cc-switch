import { useMemo } from "react";
import { useTranslation } from "react-i18next";
import { FormLabel } from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { useProvidersQuery } from "@/lib/query/queries";
import type { AppId } from "@/lib/api";
import {
  assignedSlugForForm,
  CLAUDE_GATEWAY_MODEL_PREFIX,
  isReservedRoutingSlug,
  providersInAssignOrder,
  routedModelPreview,
} from "@/utils/routingSlug";

interface RoutingSlugFieldsProps {
  appId: AppId;
  providerId?: string;
  providerName: string;
  routingSlug: string;
  onRoutingSlugChange: (value: string) => void;
  routingCatalog: boolean;
  onRoutingCatalogChange: (value: boolean) => void;
  previewModels: Array<string | undefined | null>;
}

export function RoutingSlugFields({
  appId,
  providerId,
  providerName,
  routingSlug,
  onRoutingSlugChange,
  routingCatalog,
  onRoutingCatalogChange,
  previewModels,
}: RoutingSlugFieldsProps) {
  const { t } = useTranslation();
  const { data } = useProvidersQuery(appId);
  const formSlug = useMemo(() => {
    const draft = {
      id: providerId,
      name: providerName,
      meta: { routingSlug: routingSlug.trim() || undefined },
    };
    return assignedSlugForForm({
      providers: providersInAssignOrder(Object.values(data?.providers ?? {})),
      draft,
      editingId: providerId?.trim() || undefined,
    });
  }, [data?.providers, providerId, providerName, routingSlug]);
  const assignedSlug = formSlug.assigned;
  const reserved = isReservedRoutingSlug(assignedSlug);
  const collided = formSlug.collided;
  const previews = routedModelPreview(
    assignedSlug,
    previewModels,
    4,
    appId === "claude" ? CLAUDE_GATEWAY_MODEL_PREFIX : "",
  );

  return (
    <div className="space-y-3 border-t border-border-default pt-3">
      <div className="space-y-1.5">
        <FormLabel htmlFor="provider-routing-slug">
          {t("providerForm.routingSlug", {
            defaultValue: "路由 slug",
          })}
        </FormLabel>
        <Input
          id="provider-routing-slug"
          value={routingSlug}
          onChange={(event) => onRoutingSlugChange(event.target.value)}
          placeholder={assignedSlug}
          autoComplete="off"
        />
        <p className="text-xs leading-relaxed text-muted-foreground">
          {t("providerForm.routingSlugHint", {
            defaultValue:
              "请求用 {{slug}}/{model} 选中这张卡。留空则用供应商 id 或名称生成。建议保存覆盖值，避免以后改名把旧会话打到别的卡。",
            slug: assignedSlug,
          })}
        </p>
        {reserved ? (
          <p className="text-xs text-destructive">
            {t("providerForm.routingSlugReserved", {
              defaultValue:
                "combo 是 Combo 虚拟模型的保留前缀。只要存在任意 Combo，combo/{model} 就不会钉到这张卡。",
            })}
          </p>
        ) : null}
        {collided ? (
          <p className="text-xs text-amber-600 dark:text-amber-500">
            {providerId
              ? t("providerForm.routingSlugCollision", {
                  slug: assignedSlug,
                  defaultValue:
                    "与其它卡冲突，实际路由是 {{slug}}。请改覆盖值以免请求打到别的卡。",
                })
              : t("providerForm.routingSlugCollisionNew", {
                  slug: assignedSlug,
                  defaultValue:
                    "与已有卡的路由 {{slug}} 冲突。保存后才会加真实后缀，请改覆盖值以免打到别的卡。",
                })}
          </p>
        ) : null}
      </div>

      <div className="flex items-center justify-between gap-4">
        <div className="space-y-1">
          <FormLabel>
            {t("providerForm.routingCatalog", {
              defaultValue: "参与路由目录",
            })}
          </FormLabel>
          <p className="text-xs leading-relaxed text-muted-foreground">
            {t("providerForm.routingCatalogHint", {
              defaultValue:
                "关闭后，这张卡的模型不会出现在 Claude / Codex / Claude Desktop 的合并列表里，但仍可用 slug 前缀直接请求。",
            })}
          </p>
        </div>
        <Switch
          checked={routingCatalog}
          onCheckedChange={onRoutingCatalogChange}
          aria-label={t("providerForm.routingCatalog", {
            defaultValue: "参与路由目录",
          })}
        />
      </div>

      {previews.length > 0 && (
        <p className="text-xs text-muted-foreground">
          {t("providerForm.routingPreview", {
            defaultValue: "将写入目录：{{models}}",
            models: previews.join("、"),
          })}
        </p>
      )}
    </div>
  );
}
