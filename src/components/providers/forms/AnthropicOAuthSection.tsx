import React from "react";
import { useTranslation } from "react-i18next";
import {
  AlertTriangle,
  Download,
  ExternalLink,
  Globe,
  Loader2,
  LogOut,
  User,
  X,
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { ManagedOauthAccountQuota } from "@/components/ManagedOauthQuota";
import { useAnthropicOauth } from "./hooks/useAnthropicOauth";

interface AnthropicOAuthSectionProps {
  className?: string;
  selectedAccountId?: string | null;
  onAccountSelect?: (accountId: string | null) => void;
  showAccountQuota?: boolean;
  pollStatus?: boolean;
}

export const AnthropicOAuthSection: React.FC<AnthropicOAuthSectionProps> = ({
  className,
  selectedAccountId,
  onAccountSelect,
  showAccountQuota = false,
  pollStatus = true,
}) => {
  const { t } = useTranslation();
  const {
    accounts,
    defaultAccountId,
    hasAnyAccount,
    isAuthenticated,
    pollingState,
    deviceCode,
    error,
    isPolling,
    isAddingAccount,
    isRemovingAccount,
    isSettingDefaultAccount,
    addAccount,
    cancelAuth,
    removeAccount,
    setDefaultAccount,
    logout,
    importFromCli,
    isImporting,
  } = useAnthropicOauth({ pollStatus });

  const usableAccounts = accounts.filter((account) => !account.requires_reauth);

  const remove = (accountId: string, event: React.MouseEvent) => {
    event.preventDefault();
    event.stopPropagation();
    removeAccount(accountId);
    if (selectedAccountId === accountId) onAccountSelect?.(null);
  };

  return (
    <div className={`space-y-4 ${className ?? ""}`}>
      <div className="flex items-center justify-between">
        <Label>{t("anthropicOauth.authStatus", "Claude Pro/Max OAuth")}</Label>
        <Badge
          variant={isAuthenticated ? "default" : "secondary"}
          className={
            isAuthenticated
              ? "bg-green-500 hover:bg-green-600"
              : hasAnyAccount
                ? "border-amber-500 text-amber-600"
                : ""
          }
        >
          {isAuthenticated
            ? t("anthropicOauth.accountCount", {
                count: usableAccounts.length,
                defaultValue: `${usableAccounts.length} 个可用账号`,
              })
            : hasAnyAccount
              ? t("anthropicOauth.reauthRequired", "需要重新登录")
              : t("anthropicOauth.notAuthenticated", "未认证")}
        </Badge>
      </div>

      <p className="text-sm text-muted-foreground">
        {t("anthropicOauth.importHint", {
          defaultValue:
            "用浏览器登录 Claude Pro/Max，或从当前 Claude CLI 导入。切换 CLI 账号后再导入即可添加第二个账号。",
        })}
      </p>

      {accounts.length > 0 && onAccountSelect && (
        <div className="space-y-2">
          <Label className="text-sm text-muted-foreground">
            {t("anthropicOauth.selectAccount", "选择账号")}
          </Label>
          <Select
            value={selectedAccountId || "none"}
            onValueChange={(value) =>
              onAccountSelect(value === "none" ? null : value)
            }
          >
            <SelectTrigger>
              <SelectValue
                placeholder={t(
                  "anthropicOauth.selectAccountPlaceholder",
                  "选择 Claude 账号",
                )}
              />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="none">
                {t("anthropicOauth.useDefaultAccount", "使用默认账号")}
              </SelectItem>
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
                    {account.requires_reauth && (
                      <span className="text-xs text-amber-600">
                        ({t("anthropicOauth.expired", "凭据已失效")})
                      </span>
                    )}
                  </span>
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      )}

      {hasAnyAccount && (
        <div className="space-y-2">
          <Label className="text-sm text-muted-foreground">
            {t("anthropicOauth.accounts", "Claude 账号")}
          </Label>
          <div className="space-y-1">
            {accounts.map((account) => (
              <div
                key={account.id}
                className="space-y-2 rounded-md border bg-muted/30 p-2"
              >
                <div className="flex items-center justify-between gap-2">
                  <div className="flex min-w-0 items-center gap-2">
                    {account.requires_reauth ? (
                      <AlertTriangle className="h-5 w-5 shrink-0 text-amber-500" />
                    ) : (
                      <User className="h-5 w-5 shrink-0 text-muted-foreground" />
                    )}
                    <span className="truncate text-sm font-medium">
                      {account.login}
                    </span>
                    {defaultAccountId === account.id && (
                      <Badge variant="secondary" className="text-xs">
                        {t("anthropicOauth.defaultAccount", "默认")}
                      </Badge>
                    )}
                    {account.requires_reauth && (
                      <Badge
                        variant="outline"
                        className="border-amber-500 text-xs text-amber-600"
                      >
                        {t("anthropicOauth.expired", "凭据已失效")}
                      </Badge>
                    )}
                  </div>
                  <div className="flex items-center gap-1">
                    {!account.requires_reauth &&
                      defaultAccountId !== account.id && (
                        <Button
                          type="button"
                          variant="ghost"
                          size="sm"
                          className="h-7 px-2 text-xs"
                          disabled={isSettingDefaultAccount}
                          onClick={() => setDefaultAccount(account.id)}
                        >
                          {t("anthropicOauth.setAsDefault", "设为默认")}
                        </Button>
                      )}
                    <Button
                      type="button"
                      variant="ghost"
                      size="icon"
                      className="h-7 w-7 text-muted-foreground hover:text-red-500"
                      disabled={isRemovingAccount}
                      onClick={(event) => remove(account.id, event)}
                      title={t("anthropicOauth.removeAccount", "移除账号")}
                    >
                      <X className="h-4 w-4" />
                    </Button>
                  </div>
                </div>
                {showAccountQuota && !account.requires_reauth && (
                  <ManagedOauthAccountQuota
                    provider="anthropic_oauth"
                    accountId={account.id}
                  />
                )}
              </div>
            ))}
          </div>
        </div>
      )}

      {pollingState === "idle" && (
        <Button
          type="button"
          variant="outline"
          className="w-full"
          disabled={isAddingAccount}
          onClick={addAccount}
        >
          <Globe className="mr-2 h-4 w-4" />
          {hasAnyAccount
            ? t("anthropicOauth.loginAnother", "使用浏览器添加账号")
            : t("anthropicOauth.login", "使用浏览器登录")}
        </Button>
      )}

      {isPolling && (
        <div className="space-y-3 rounded-lg border bg-muted/50 p-4">
          <div className="flex items-center justify-center gap-2 text-sm text-muted-foreground">
            <Loader2 className="h-4 w-4 animate-spin" />
            {t("anthropicOauth.waitingForAuth", "等待浏览器授权中…")}
          </div>
          {deviceCode?.verification_uri && (
            <div className="text-center">
              <a
                href={deviceCode.verification_uri}
                target="_blank"
                rel="noopener noreferrer"
                className="inline-flex items-center gap-1 text-sm text-blue-500 hover:underline break-all"
              >
                {t("anthropicOauth.openBrowser", "打开登录页")}
                <ExternalLink className="h-3 w-3" />
              </a>
            </div>
          )}
          <div className="text-center">
            <Button
              type="button"
              variant="ghost"
              size="sm"
              onClick={cancelAuth}
            >
              {t("common.cancel", "取消")}
            </Button>
          </div>
        </div>
      )}

      {pollingState === "error" && error && (
        <div className="space-y-2">
          <p className="text-sm text-red-500">{error}</p>
          <div className="flex gap-2">
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={addAccount}
            >
              {t("anthropicOauth.retry", "重试")}
            </Button>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              onClick={cancelAuth}
            >
              {t("common.cancel", "取消")}
            </Button>
          </div>
        </div>
      )}

      {pollingState === "idle" && (
        <Button
          type="button"
          variant="outline"
          className="w-full"
          disabled={isImporting}
          onClick={importFromCli}
        >
          <Download className="mr-2 h-4 w-4" />
          {hasAnyAccount
            ? t("anthropicOauth.importAnother", "再次从 Claude CLI 导入")
            : t("anthropicOauth.import", "从 Claude CLI 导入")}
        </Button>
      )}

      {error && pollingState !== "error" && (
        <p className="text-sm text-red-500">{error}</p>
      )}

      {hasAnyAccount && accounts.length > 1 && (
        <Button
          type="button"
          variant="outline"
          className="w-full text-red-500 hover:text-red-600"
          onClick={logout}
        >
          <LogOut className="mr-2 h-4 w-4" />
          {t("anthropicOauth.logoutAll", "移除所有 Claude 账号")}
        </Button>
      )}
    </div>
  );
};

export default AnthropicOAuthSection;
