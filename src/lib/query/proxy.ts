import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { proxyApi } from "@/lib/api/proxy";
import { toast } from "sonner";
import { useTranslation } from "react-i18next";
import type {
  GlobalProxyConfig,
  AppProxyConfig,
  ProxyTakeoverStatus,
} from "@/types/proxy";
import { getAppLabel } from "@/config/appConfig";
import { extractErrorMessage } from "@/utils/errorUtils";

export const proxyKeys = {
  status: ["proxyStatus"] as const,
  takeoverStatus: ["proxyTakeoverStatus"] as const,
  globalConfig: ["globalProxyConfig"] as const,
  appConfig: (appType: string) => ["appProxyConfig", appType] as const,
  combos: ["modelCombos"] as const,
  sidecars: ["sidecarSettings"] as const,
};

// ========== 代理服务器状态 Hooks ==========

/**
 * 获取代理服务器状态
 */
export function useProxyStatusQuery() {
  return useQuery({
    queryKey: proxyKeys.status,
    queryFn: () => proxyApi.getProxyStatus(),
    // Running: 2s. Stopped: 5s so tray-started transitions are observed
    // without requiring window focus (refetchOnWindowFocus is not enough).
    refetchInterval: (query) => (query.state.data?.running ? 2000 : 5000),
    // 保持之前的数据，避免闪烁
    placeholderData: (previousData) => previousData,
  });
}

/**
 * 获取各应用接管状态
 */
export function useProxyTakeoverStatus(poll = true) {
  return useQuery({
    queryKey: proxyKeys.takeoverStatus,
    queryFn: () => proxyApi.getProxyTakeoverStatus(),
    // Fast poll in the proxy panel; modest poll elsewhere so a tray
    // takeover change is visible even if the event listener is late.
    refetchInterval: poll ? 2000 : 5000,
    placeholderData: (previousData: ProxyTakeoverStatus | undefined) =>
      previousData,
  });
}

// ========== 代理服务器控制 Hooks ==========

/**
 * 设置应用接管状态
 */
export function useSetProxyTakeoverForApp() {
  const queryClient = useQueryClient();
  const { t } = useTranslation();

  return useMutation({
    mutationFn: ({ appType, enabled }: { appType: string; enabled: boolean }) =>
      proxyApi.setProxyTakeoverForApp(appType, enabled),
    onSuccess: (_data, variables) => {
      queryClient.invalidateQueries({ queryKey: proxyKeys.takeoverStatus });
      queryClient.invalidateQueries({ queryKey: proxyKeys.status });
      queryClient.invalidateQueries({
        queryKey: proxyKeys.appConfig(variables.appType),
      });
      const appLabel = getAppLabel(variables.appType);
      toast.success(
        variables.enabled
          ? t("proxy.takeover.enabled", {
              app: appLabel,
              defaultValue: `已接管 ${appLabel} 配置（请求将走本地代理）`,
            })
          : t("proxy.takeover.disabled", {
              app: appLabel,
              defaultValue: `已恢复 ${appLabel} 配置`,
            }),
        { closeButton: true },
      );
    },
  });
}

// ========== v3+ 全局/应用级配置 Hooks ==========

/**
 * 获取全局代理配置
 */
export function useGlobalProxyConfig() {
  return useQuery({
    queryKey: proxyKeys.globalConfig,
    queryFn: () => proxyApi.getGlobalProxyConfig(),
  });
}

/**
 * 更新全局代理配置
 */
export function useUpdateGlobalProxyConfig() {
  const queryClient = useQueryClient();
  const { t } = useTranslation();

  return useMutation({
    mutationFn: (config: GlobalProxyConfig) =>
      proxyApi.updateGlobalProxyConfig(config),
    onSuccess: () => {
      toast.success(t("proxy.settings.toast.saved"), { closeButton: true });
      queryClient.invalidateQueries({ queryKey: proxyKeys.globalConfig });
      queryClient.invalidateQueries({ queryKey: proxyKeys.status });
    },
    onError: (error: unknown) => {
      toast.error(
        t("proxy.settings.toast.saveFailed", {
          error: extractErrorMessage(error),
        }),
      );
    },
  });
}

/**
 * 获取指定应用的代理配置
 */
export function useAppProxyConfig(appType: string) {
  return useQuery({
    queryKey: proxyKeys.appConfig(appType),
    queryFn: () => proxyApi.getProxyConfigForApp(appType),
    enabled: !!appType,
  });
}

/**
 * 更新指定应用的代理配置
 */
export function useUpdateAppProxyConfig() {
  const queryClient = useQueryClient();
  const { t } = useTranslation();

  return useMutation({
    mutationFn: (config: AppProxyConfig) =>
      proxyApi.updateProxyConfigForApp(config),
    onSuccess: (_, variables) => {
      toast.success(t("proxy.settings.toast.saved"), { closeButton: true });
      queryClient.invalidateQueries({
        queryKey: proxyKeys.appConfig(variables.appType),
      });
      queryClient.invalidateQueries({
        queryKey: ["autoFailoverEnabled", variables.appType],
      });
      queryClient.invalidateQueries({ queryKey: ["circuitBreakerConfig"] });
      queryClient.invalidateQueries({ queryKey: proxyKeys.status });
    },
    onError: (error: unknown) => {
      toast.error(
        t("proxy.settings.toast.saveFailed", {
          error: extractErrorMessage(error),
        }),
      );
    },
  });
}

export function useModelCombos() {
  return useQuery({
    queryKey: proxyKeys.combos,
    queryFn: () => proxyApi.listModelCombos(),
  });
}

export function useUpsertModelCombo() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({
      combo,
      previousId,
    }: {
      combo: import("@/types/proxy").ModelCombo;
      previousId?: string;
    }) => proxyApi.upsertModelCombo(combo, previousId),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: proxyKeys.combos });
    },
  });
}

export function useDeleteModelCombo() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => proxyApi.deleteModelCombo(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: proxyKeys.combos });
    },
  });
}

export function useSetProviderRoutingCatalog() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({
      app,
      id,
      enabled,
    }: {
      app: string;
      id: string;
      enabled: boolean;
    }) => proxyApi.setProviderRoutingCatalog(app, id, enabled),
    onSuccess: (_data, variables) => {
      queryClient.invalidateQueries({ queryKey: ["providers", variables.app] });
    },
  });
}

export function useSidecarSettings() {
  return useQuery({
    queryKey: proxyKeys.sidecars,
    queryFn: () => proxyApi.getSidecarSettings(),
  });
}

export function useUpdateSidecarSettings() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (settings: import("@/types/proxy").SidecarSettings) =>
      proxyApi.updateSidecarSettings(settings),
    onMutate: async (settings) => {
      await queryClient.cancelQueries({ queryKey: proxyKeys.sidecars });
      const previous = queryClient.getQueryData(proxyKeys.sidecars);
      queryClient.setQueryData(proxyKeys.sidecars, settings);
      return { previous };
    },
    onError: (_error, _settings, context) => {
      if (context?.previous !== undefined) {
        queryClient.setQueryData(proxyKeys.sidecars, context.previous);
      }
    },
    onSuccess: (settings) => {
      queryClient.setQueryData(proxyKeys.sidecars, settings);
    },
  });
}
