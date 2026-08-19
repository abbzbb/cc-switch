import { AlertTriangle, Settings2, User } from "lucide-react";
import { useTranslation } from "react-i18next";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

export type ManagedOAuthAccountOption = {
  id: string;
  login: string;
  requires_reauth?: boolean;
};

interface ManagedOAuthAccountSelectProps {
  accounts: ManagedOAuthAccountOption[];
  selectedAccountId?: string | null;
  onAccountSelect: (accountId: string | null) => void;
  onManageAccounts?: () => void;
  selectLabel: string;
  placeholder: string;
  noneOptionLabel: string;
  expiredLabel: string;
}

export function ManagedOAuthAccountSelect({
  accounts,
  selectedAccountId,
  onAccountSelect,
  onManageAccounts,
  selectLabel,
  placeholder,
  noneOptionLabel,
  expiredLabel,
}: ManagedOAuthAccountSelectProps) {
  const { t } = useTranslation();
  const selected = accounts.find((account) => account.id === selectedAccountId);

  return (
    <div className="space-y-2">
      <div className="flex flex-col gap-2 sm:flex-row sm:items-end">
        <div className="min-w-0 flex-1 space-y-2">
          <Label className="text-sm text-muted-foreground">{selectLabel}</Label>
          <Select
            value={selectedAccountId || "none"}
            onValueChange={(value) =>
              onAccountSelect(value === "none" ? null : value)
            }
          >
            <SelectTrigger>
              <SelectValue placeholder={placeholder} />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="none">{noneOptionLabel}</SelectItem>
              {accounts.map((account) => (
                <SelectItem
                  key={account.id}
                  value={account.id}
                  disabled={account.requires_reauth}
                >
                  <span className="flex items-center gap-2">
                    {account.requires_reauth ? (
                      <AlertTriangle className="h-4 w-4 text-amber-500" />
                    ) : (
                      <User className="h-4 w-4 text-muted-foreground" />
                    )}
                    {account.login}
                    {account.requires_reauth ? (
                      <span className="text-xs text-amber-600">
                        ({expiredLabel})
                      </span>
                    ) : null}
                  </span>
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        {onManageAccounts ? (
          <Button
            type="button"
            variant="outline"
            onClick={onManageAccounts}
            className="h-9 shrink-0"
          >
            <Settings2 className="h-4 w-4" />
            {t("copilot.manageAccounts", "管理账号")}
          </Button>
        ) : null}
      </div>
      {selected?.requires_reauth ? (
        <p className="text-xs text-amber-600 dark:text-amber-500">
          {expiredLabel}
        </p>
      ) : null}
    </div>
  );
}
