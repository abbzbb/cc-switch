import { useTranslation } from "react-i18next";
import { FormLabel } from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { preferredRoutingSlug, routedModelPreview } from "@/utils/routingSlug";

interface RoutingSlugFieldsProps {
  providerId?: string;
  providerName: string;
  routingSlug: string;
  onRoutingSlugChange: (value: string) => void;
  routingCatalog: boolean;
  onRoutingCatalogChange: (value: boolean) => void;
  previewModels: Array<string | undefined | null>;
}

export function RoutingSlugFields({
  providerId,
  providerName,
  routingSlug,
  onRoutingSlugChange,
  routingCatalog,
  onRoutingCatalogChange,
  previewModels,
}: RoutingSlugFieldsProps) {
  const { t } = useTranslation();
  const effectiveSlug = preferredRoutingSlug({
    id: providerId,
    name: providerName,
    meta: { routingSlug: routingSlug.trim() || undefined },
  });
  const previews = routedModelPreview(effectiveSlug, previewModels);

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
          placeholder={effectiveSlug}
          autoComplete="off"
        />
        <p className="text-xs leading-relaxed text-muted-foreground">
          {t("providerForm.routingSlugHint", {
            defaultValue:
              "请求用 {slug}/{model} 选中这张卡。留空则用供应商 id 或名称生成。改名不会改已保存的覆盖值。",
            slug: effectiveSlug,
          })}
        </p>
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
                "关闭后，这张卡的模型不会出现在 Codex / Claude Desktop 的合并列表里，但仍可用 slug 前缀直接请求。",
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
