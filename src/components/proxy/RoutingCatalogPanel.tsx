import { useMemo } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { useProvidersQuery } from "@/lib/query/queries";
import { useSetProviderRoutingCatalog } from "@/lib/query/proxy";
import { getAppLabel } from "@/config/appConfig";
import { providerUpstreamModelIds } from "@/utils/combo";
import {
  aliasInnerSlashes,
  assignRoutingSlugs,
  providersInAssignOrder,
} from "@/utils/routingSlug";
import { extractErrorMessage } from "@/utils/errorUtils";
import type { Provider } from "@/types";
import type { ProxyTakeoverStatus } from "@/types/proxy";

const ROUTING_CATALOG_APPS = ["codex", "claude", "claude-desktop"] as const;
type RoutingCatalogApp = (typeof ROUTING_CATALOG_APPS)[number];

function isCatalogEnabled(provider: Provider): boolean {
  return provider.meta?.routingCatalog !== false;
}

const CATALOG_MODEL_PREVIEW = 6;

function CatalogModelList({
  slug,
  provider,
}: {
  slug: string;
  provider: Provider;
}) {
  const { t } = useTranslation();
  const models = providerUpstreamModelIds(provider);
  if (models.length === 0) {
    return (
      <p className="text-[11px] text-muted-foreground/80">
        {t("proxy.routingCards.noModels", {
          defaultValue: "还没有可写入目录的模型",
        })}
      </p>
    );
  }
  const shown = models.slice(0, CATALOG_MODEL_PREVIEW);
  const extra = models.length - shown.length;
  return (
    <ul className="mt-1 space-y-0.5">
      {shown.map((model) => (
        <li
          key={model}
          className="truncate font-mono text-[11px] text-muted-foreground"
        >
          {slug}/{aliasInnerSlashes(model)}
        </li>
      ))}
      {extra > 0 ? (
        <li className="text-[11px] text-muted-foreground/80">
          {t("proxy.routingCards.moreModels", {
            count: extra,
            defaultValue: "另有 {{count}} 个模型",
          })}
        </li>
      ) : null}
    </ul>
  );
}

function AppCatalogGroup({
  appId,
  takeoverOn,
}: {
  appId: RoutingCatalogApp;
  takeoverOn: boolean;
}) {
  const { t } = useTranslation();
  const { data, isError, isPending, refetch } = useProvidersQuery(appId);
  const setCatalog = useSetProviderRoutingCatalog();

  const providers = useMemo(
    () => providersInAssignOrder(Object.values(data?.providers ?? {})),
    [data?.providers],
  );
  const assignedSlugs = useMemo(
    () => assignRoutingSlugs(providers),
    [providers],
  );
  const currentId = data?.currentProviderId ?? "";

  if (isPending && !data) {
    return null;
  }

  if (isError && providers.length === 0) {
    return (
      <div className="flex items-center justify-between gap-2 px-1">
        <span className="text-xs text-destructive">
          {t("proxy.routingCards.loadFailed", {
            app: getAppLabel(appId),
            defaultValue: "无法加载 {{app}} 的路由目录",
          })}
        </span>
        <Button size="sm" variant="outline" onClick={() => void refetch()}>
          {t("common.retry", { defaultValue: "重试" })}
        </Button>
      </div>
    );
  }

  if (providers.length === 0) {
    return null;
  }

  const handleToggle = async (provider: Provider, enabled: boolean) => {
    try {
      await setCatalog.mutateAsync({
        app: appId,
        id: provider.id,
        enabled,
      });
      toast.success(
        t("proxy.routingCards.updated", {
          defaultValue: "已更新参与路由的配置",
        }),
        { closeButton: true },
      );
      if (appId === "codex" && takeoverOn) {
        toast.message(
          t("proxy.routingCards.codexRestart", {
            defaultValue: "完全退出并重启 Codex 后，/model 才会读到新目录",
          }),
        );
      }
    } catch (error) {
      toast.error(
        t("proxy.routingCards.failed", {
          detail: extractErrorMessage(error),
          defaultValue: "更新参与路由的配置失败: {{detail}}",
        }),
      );
    }
  };

  return (
    <div className="space-y-2">
      <div className="flex items-center gap-2 px-1">
        <span className="text-xs font-semibold text-foreground/80">
          {getAppLabel(appId)}
        </span>
        <div className="flex-1 h-px bg-border/50" />
        <span className="text-[11px] text-muted-foreground">
          {t("proxy.routingCards.enabledCount", {
            enabled: providers.filter(isCatalogEnabled).length,
            total: providers.length,
            defaultValue: "{{enabled}} / {{total}} 已加入",
          })}
        </span>
      </div>
      <div className="space-y-1.5">
        {providers.map((provider) => {
          const slug =
            assignedSlugs.get(provider.id) ?? provider.id.toLowerCase();
          const enabled = isCatalogEnabled(provider);
          const isCurrent = provider.id === currentId;
          const pending =
            setCatalog.isPending &&
            setCatalog.variables?.app === appId &&
            setCatalog.variables?.id === provider.id;
          return (
            <div
              key={provider.id}
              className="flex items-center justify-between gap-3 rounded-md border border-border bg-background/60 px-3 py-2"
            >
              <div className="min-w-0 space-y-0.5">
                <div className="flex items-center gap-2 min-w-0">
                  <span className="text-sm font-medium truncate">
                    {provider.name}
                  </span>
                  {isCurrent && (
                    <span className="shrink-0 text-[10px] px-1.5 py-0.5 rounded bg-primary/15 text-primary">
                      {t("proxy.routingCards.homeCurrent", {
                        defaultValue: "当前接管",
                      })}
                    </span>
                  )}
                </div>
                <p className="text-[11px] text-muted-foreground truncate">
                  {t("proxy.routingCards.slug", {
                    slug,
                    defaultValue: "路由 {{slug}}",
                  })}
                </p>
                <CatalogModelList slug={slug} provider={provider} />
              </div>
              <Switch
                checked={enabled}
                disabled={pending}
                onCheckedChange={(checked) => handleToggle(provider, checked)}
                aria-label={t("providerForm.routingCatalog", {
                  defaultValue: "参与路由目录",
                })}
              />
            </div>
          );
        })}
      </div>
    </div>
  );
}

interface RoutingCatalogPanelProps {
  takeoverByApp?: ProxyTakeoverStatus;
}

export function RoutingCatalogPanel({
  takeoverByApp,
}: RoutingCatalogPanelProps) {
  const { t } = useTranslation();

  return (
    <div className="rounded-xl border border-border bg-card/50 p-4 space-y-3">
      <div className="space-y-1">
        <p className="text-sm font-medium">
          {t("proxy.routingCards.title", {
            defaultValue: "参与路由的配置",
          })}
        </p>
        <p className="text-xs text-muted-foreground">
          {t("proxy.routingCards.description", {
            defaultValue:
              "勾选要出现在选择器里的供应商配置。平台开关只决定要不要接管该应用；这里决定合并目录里出现哪几张卡。关闭后仍可用 slug 前缀请求。Gemini / Grok 没有合并目录，继续用当前配置 + 故障转移队列。新卡默认加入。使用中的徽章表示接管当前卡，不是上次 pin/combo 请求。",
          })}
        </p>
      </div>
      <div className="space-y-4">
        {ROUTING_CATALOG_APPS.map((appId) => (
          <AppCatalogGroup
            key={appId}
            appId={appId}
            takeoverOn={takeoverByApp?.[appId] ?? false}
          />
        ))}
      </div>
    </div>
  );
}
