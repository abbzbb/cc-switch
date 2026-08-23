import type { QueryClient } from "@tanstack/react-query";
import { proxyKeys } from "./proxy";

/**
 * Refresh caches that import / remote-config download can rewrite.
 * Keep this list in one place so App and WebDAV/S3 download cannot drift.
 *
 * Never call bare invalidateQueries() — that refetches every query and
 * re-hydrates dirty settings forms.
 */
export function invalidateAfterImport(queryClient: QueryClient) {
  return Promise.all([
    queryClient.invalidateQueries({ queryKey: ["providers"] }),
    queryClient.invalidateQueries({ queryKey: ["settings"] }),
    queryClient.invalidateQueries({ queryKey: ["mcp"] }),
    queryClient.invalidateQueries({ queryKey: ["skills"] }),
    queryClient.invalidateQueries({ queryKey: ["profiles"] }),
    queryClient.invalidateQueries({ queryKey: proxyKeys.status }),
    queryClient.invalidateQueries({ queryKey: proxyKeys.takeoverStatus }),
    queryClient.invalidateQueries({ queryKey: proxyKeys.combos }),
    queryClient.invalidateQueries({ queryKey: proxyKeys.sidecars }),
    queryClient.invalidateQueries({ queryKey: proxyKeys.globalConfig }),
    queryClient.invalidateQueries({ queryKey: ["sessions"] }),
    queryClient.invalidateQueries({ queryKey: ["sessionMessages"] }),
    queryClient.invalidateQueries({ queryKey: ["globalProxyUrl"] }),
    queryClient.invalidateQueries({ queryKey: ["usage"] }),
    queryClient.invalidateQueries({ queryKey: ["pi"] }),
    queryClient.invalidateQueries({ queryKey: ["openclaw"] }),
    queryClient.invalidateQueries({ queryKey: ["hermes"] }),
    queryClient.invalidateQueries({
      queryKey: ["opencodeLiveProviderIds"],
    }),
    queryClient.invalidateQueries({
      queryKey: ["providers", "claude-desktop"],
    }),
    queryClient.invalidateQueries({
      queryKey: ["omo", "current-provider-id"],
    }),
    queryClient.invalidateQueries({
      queryKey: ["omo-slim", "current-provider-id"],
    }),
    queryClient.invalidateQueries({ queryKey: ["omo"] }),
    queryClient.invalidateQueries({ queryKey: ["omo-slim"] }),
    queryClient.invalidateQueries({ queryKey: ["autoFailoverEnabled"] }),
    queryClient.invalidateQueries({ queryKey: ["failoverQueue"] }),
    queryClient.invalidateQueries({ queryKey: ["circuitBreakerConfig"] }),
    queryClient.invalidateQueries({ queryKey: ["claudeDesktopStatus"] }),
    queryClient.invalidateQueries({ queryKey: ["subscription"] }),
  ]);
}
