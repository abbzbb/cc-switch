import { useCallback } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { useTranslation } from "react-i18next";
import { authApi } from "@/lib/api";
import { useManagedAuth } from "./useManagedAuth";

export function useAnthropicOauth() {
  const managed = useManagedAuth("anthropic_oauth");
  const queryClient = useQueryClient();
  const { t } = useTranslation();
  const queryKey = ["managed-auth-status", "anthropic_oauth"];

  const importMutation = useMutation({
    mutationFn: () => authApi.authImportLocal("anthropic_oauth"),
    onSuccess: async () => {
      toast.success(
        t("anthropicOauth.importSuccess", {
          defaultValue: "已从 Claude CLI 导入账号",
        }),
      );
      await managed.refetchStatus();
      await queryClient.invalidateQueries({ queryKey });
    },
    onError: (error) => {
      toast.error(error instanceof Error ? error.message : String(error));
    },
  });

  const importFromCli = useCallback(() => {
    importMutation.mutate();
  }, [importMutation]);

  return {
    ...managed,
    importFromCli,
    isImporting: importMutation.isPending,
  };
}
