import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { useBaseUrlState } from "@/components/providers/forms/hooks/useBaseUrlState";

describe("useBaseUrlState", () => {
  it("writes base URL into the latest JSON instead of a stale snapshot", () => {
    let config = JSON.stringify({
      env: {
        ANTHROPIC_AUTH_TOKEN: "sk-keep",
        ANTHROPIC_BASE_URL: "https://old.example",
      },
    });
    const onSettingsConfigChange = vi.fn((next: string) => {
      config = next;
    });

    const { result, rerender } = renderHook(
      ({ settingsConfig }: { settingsConfig: string }) =>
        useBaseUrlState({
          appType: "claude",
          category: "custom",
          settingsConfig,
          onSettingsConfigChange,
        }),
      { initialProps: { settingsConfig: config } },
    );

    const withExtra = JSON.stringify({
      env: {
        ANTHROPIC_AUTH_TOKEN: "sk-keep",
        ANTHROPIC_BASE_URL: "https://old.example",
        ANTHROPIC_MODEL: "kept-model",
      },
    });
    rerender({ settingsConfig: withExtra });

    act(() => {
      result.current.handleClaudeBaseUrlChange("https://new.example");
    });

    const lastCall = onSettingsConfigChange.mock.calls.at(-1)?.[0];
    expect(lastCall).toEqual(expect.any(String));
    const written = JSON.parse(lastCall as string);
    expect(written.env.ANTHROPIC_BASE_URL).toBe("https://new.example");
    expect(written.env.ANTHROPIC_AUTH_TOKEN).toBe("sk-keep");
    expect(written.env.ANTHROPIC_MODEL).toBe("kept-model");
  });
});
