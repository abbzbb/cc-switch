import React from "react";
import { Loader2 } from "lucide-react";
import type { ProviderMeta } from "@/types";
import {
  useManagedOauthQuota,
  useManagedOauthQuotaByAccountId,
  type ManagedQuotaProvider,
} from "@/lib/query/subscription";
import { SubscriptionQuotaView } from "@/components/SubscriptionQuotaFooter";

interface ManagedOauthAccountQuotaProps {
  provider: ManagedQuotaProvider;
  accountId: string;
}

export const ManagedOauthAccountQuota: React.FC<
  ManagedOauthAccountQuotaProps
> = ({ provider, accountId }) => {
  const {
    data: quota,
    isFetching: loading,
    refetch,
  } = useManagedOauthQuotaByAccountId(provider, accountId, {
    enabled: true,
    autoQuery: false,
  });

  if (loading && !quota) {
    return (
      <div className="mt-3 flex items-center justify-center rounded-xl border border-border-default bg-card py-5 shadow-sm">
        <Loader2 className="h-4 w-4 animate-spin text-muted-foreground" />
      </div>
    );
  }

  return (
    <SubscriptionQuotaView
      quota={quota}
      loading={loading}
      refetch={refetch}
      appIdForExpiredHint={provider}
      inline={false}
    />
  );
};

interface ManagedOauthQuotaFooterProps {
  provider: ManagedQuotaProvider;
  meta?: ProviderMeta;
  inline?: boolean;
  isCurrent?: boolean;
  autoQueryInterval?: number;
}

export const ManagedOauthQuotaFooter: React.FC<
  ManagedOauthQuotaFooterProps
> = ({
  provider,
  meta,
  inline = false,
  isCurrent = false,
  autoQueryInterval = 5,
}) => {
  const {
    data: quota,
    isFetching: loading,
    refetch,
  } = useManagedOauthQuota(provider, meta, {
    enabled: true,
    autoQuery: isCurrent && autoQueryInterval > 0,
    autoQueryIntervalMinutes: autoQueryInterval,
  });

  return (
    <SubscriptionQuotaView
      quota={quota}
      loading={loading}
      refetch={refetch}
      appIdForExpiredHint={provider}
      inline={inline}
    />
  );
};
