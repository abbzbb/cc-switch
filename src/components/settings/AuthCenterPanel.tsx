import { useEffect, useRef } from "react";
import { Github, ShieldCheck } from "lucide-react";
import { useTranslation } from "react-i18next";
import { Badge } from "@/components/ui/badge";
import { CodexIcon } from "@/components/BrandIcons";
import { CopilotAuthSection } from "@/components/providers/forms/CopilotAuthSection";
import { CodexOAuthSection } from "@/components/providers/forms/CodexOAuthSection";
import type { ManagedAuthProvider } from "@/lib/api";
import { XaiOAuthSection } from "@/components/providers/forms/XaiOAuthSection";
import { KimiOAuthSection } from "@/components/providers/forms/KimiOAuthSection";
import { AnthropicOAuthSection } from "@/components/providers/forms/AnthropicOAuthSection";
import { ProviderIcon } from "@/components/ProviderIcon";

interface AuthCenterPanelProps {
  authScrollTarget?: ManagedAuthProvider | null;
}

export function AuthCenterPanel({ authScrollTarget }: AuthCenterPanelProps) {
  const { t } = useTranslation();
  const copilotSectionRef = useRef<HTMLElement | null>(null);
  const codexOauthSectionRef = useRef<HTMLElement | null>(null);
  const xaiOauthSectionRef = useRef<HTMLElement | null>(null);
  const kimiOauthSectionRef = useRef<HTMLElement | null>(null);
  const anthropicOauthSectionRef = useRef<HTMLElement | null>(null);

  useEffect(() => {
    if (!authScrollTarget) return;

    const sectionRef =
      authScrollTarget === "github_copilot"
        ? copilotSectionRef
        : authScrollTarget === "codex_oauth"
          ? codexOauthSectionRef
          : authScrollTarget === "xai_oauth"
            ? xaiOauthSectionRef
            : authScrollTarget === "kimi_oauth"
              ? kimiOauthSectionRef
              : anthropicOauthSectionRef;

    const frame = requestAnimationFrame(() => {
      const prefersReducedMotion = window.matchMedia(
        "(prefers-reduced-motion: reduce)",
      ).matches;

      sectionRef.current?.scrollIntoView({
        behavior: prefersReducedMotion ? "auto" : "smooth",
        block: "start",
      });
    });

    return () => cancelAnimationFrame(frame);
  }, [authScrollTarget]);

  return (
    <div className="space-y-6">
      <section className="rounded-xl border border-border/60 bg-card/60 p-6">
        <div className="flex items-start justify-between gap-4">
          <div className="space-y-2">
            <div className="flex items-center gap-2">
              <ShieldCheck className="h-5 w-5 text-primary" />
              <h3 className="text-base font-semibold">
                {t("settings.authCenter.title", {
                  defaultValue: "OAuth 认证中心",
                })}
              </h3>
            </div>
            <p className="text-sm text-muted-foreground">
              {t("settings.authCenter.description", {
                defaultValue:
                  "在 Claude Code 中使用您的其他订阅，请注意合规风险。",
              })}
            </p>
          </div>
          <Badge variant="secondary">
            {t("settings.authCenter.beta", { defaultValue: "Beta" })}
          </Badge>
        </div>
      </section>

      <section
        ref={copilotSectionRef}
        className="scroll-mt-4 rounded-xl border border-border/60 bg-card/60 p-6"
      >
        <div className="mb-4 flex items-center gap-3">
          <div className="flex h-10 w-10 items-center justify-center rounded-xl bg-muted">
            <Github className="h-5 w-5" />
          </div>
          <div>
            <h4 className="font-medium">GitHub Copilot</h4>
            <p className="text-sm text-muted-foreground">
              {t("settings.authCenter.copilotDescription", {
                defaultValue: "管理 GitHub Copilot 账号",
              })}
            </p>
          </div>
        </div>

        <CopilotAuthSection />
      </section>

      <section
        ref={codexOauthSectionRef}
        className="scroll-mt-4 rounded-xl border border-border/60 bg-card/60 p-6"
      >
        <div className="mb-4 flex items-center gap-3">
          <div className="flex h-10 w-10 items-center justify-center rounded-xl bg-muted">
            <CodexIcon size={20} />
          </div>
          <div>
            <h4 className="font-medium">ChatGPT (Codex OAuth)</h4>
            <p className="text-sm text-muted-foreground">
              {t("settings.authCenter.codexOauthDescription", {
                defaultValue: "管理 ChatGPT 账号",
              })}
            </p>
          </div>
        </div>

        <CodexOAuthSection showAccountQuota />
      </section>

      <section
        ref={xaiOauthSectionRef}
        className="scroll-mt-4 rounded-xl border border-border/60 bg-card/60 p-6"
      >
        <div className="mb-4 flex items-center gap-3">
          <div className="flex h-10 w-10 items-center justify-center rounded-xl bg-muted">
            <ProviderIcon icon="xai" name="xAI" size={20} />
          </div>
          <div>
            <h4 className="font-medium">xAI (Grok OAuth)</h4>
            <p className="text-sm text-muted-foreground">
              {t("settings.authCenter.xaiOauthDescription", {
                defaultValue: "管理 xAI / Grok 账号",
              })}
            </p>
          </div>
        </div>

        <XaiOAuthSection />
      </section>

      <section
        ref={kimiOauthSectionRef}
        className="scroll-mt-4 rounded-xl border border-border/60 bg-card/60 p-6"
      >
        <div className="mb-4 flex items-center gap-3">
          <div className="flex h-10 w-10 items-center justify-center rounded-xl bg-muted">
            <ProviderIcon icon="kimi" name="Kimi" size={20} />
          </div>
          <div>
            <h4 className="font-medium">Kimi For Coding</h4>
            <p className="text-sm text-muted-foreground">
              {t("settings.authCenter.kimiOauthDescription", {
                defaultValue: "管理 Kimi For Coding 账号",
              })}
            </p>
          </div>
        </div>

        <KimiOAuthSection showAccountQuota />
      </section>

      <section
        ref={anthropicOauthSectionRef}
        className="scroll-mt-4 rounded-xl border border-border/60 bg-card/60 p-6"
      >
        <div className="mb-4 flex items-center gap-3">
          <div className="flex h-10 w-10 items-center justify-center rounded-xl bg-muted">
            <ProviderIcon icon="anthropic" name="Claude" size={20} />
          </div>
          <div>
            <h4 className="font-medium">Claude Pro/Max</h4>
            <p className="text-sm text-muted-foreground">
              {t("settings.authCenter.anthropicOauthDescription", {
                defaultValue: "用浏览器登录或从 Claude CLI 导入 Pro/Max 账号",
              })}
            </p>
          </div>
        </div>

        <AnthropicOAuthSection showAccountQuota />
      </section>
    </div>
  );
}
