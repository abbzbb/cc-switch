import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { useApiKeyState } from "@/components/providers/forms/hooks/useApiKeyState";

describe("useApiKeyState", () => {
  it("shows and creates Claude API key for uncategorized edit providers", () => {
    const onConfigChange = vi.fn();
    const initialConfig = JSON.stringify({ env: {} }, null, 2);

    const { result } = renderHook(() =>
      useApiKeyState({
        initialConfig,
        onConfigChange,
        selectedPresetId: null,
        category: undefined,
        appType: "claude",
      }),
    );

    expect(result.current.showApiKey(initialConfig, true)).toBe(true);

    act(() => {
      result.current.handleApiKeyChange("sk-test");
    });

    const updated = JSON.parse(onConfigChange.mock.calls.at(-1)?.[0]);
    expect(updated.env.ANTHROPIC_AUTH_TOKEN).toBe("sk-test");
  });

  it("keeps official and cloud provider edit behavior conservative", () => {
    const initialConfig = JSON.stringify({ env: {} }, null, 2);
    const officialConfigChange = vi.fn();

    const official = renderHook(() =>
      useApiKeyState({
        initialConfig,
        onConfigChange: officialConfigChange,
        selectedPresetId: null,
        category: "official",
        appType: "claude",
      }),
    );
    expect(official.result.current.showApiKey(initialConfig, true)).toBe(false);
    act(() => {
      official.result.current.handleApiKeyChange("sk-official");
    });
    expect(officialConfigChange).toHaveBeenLastCalledWith(initialConfig);

    const cloudProviderConfigChange = vi.fn();
    const cloudProvider = renderHook(() =>
      useApiKeyState({
        initialConfig,
        onConfigChange: cloudProviderConfigChange,
        selectedPresetId: null,
        category: "cloud_provider",
        appType: "claude",
      }),
    );
    expect(cloudProvider.result.current.showApiKey(initialConfig, true)).toBe(
      false,
    );
    act(() => {
      cloudProvider.result.current.handleApiKeyChange("sk-cloud");
    });
    expect(cloudProviderConfigChange).toHaveBeenLastCalledWith(initialConfig);
  });

  it("writes API key into the latest config without depending on apiKey in the sync effect", () => {
    const onConfigChange = vi.fn();
    let config = JSON.stringify(
      { env: { ANTHROPIC_AUTH_TOKEN: "old", ANTHROPIC_BASE_URL: "https://x" } },
      null,
      2,
    );

    const { result, rerender } = renderHook(
      ({ initialConfig }: { initialConfig: string }) =>
        useApiKeyState({
          initialConfig,
          onConfigChange: (next) => {
            config = next;
            onConfigChange(next);
          },
          selectedPresetId: null,
          category: "custom",
          appType: "claude",
        }),
      { initialProps: { initialConfig: config } },
    );

    expect(result.current.apiKey).toBe("old");

    act(() => {
      result.current.handleApiKeyChange("sk-new");
    });

    const written = JSON.parse(onConfigChange.mock.calls.at(-1)?.[0]);
    expect(written.env.ANTHROPIC_AUTH_TOKEN).toBe("sk-new");
    expect(written.env.ANTHROPIC_BASE_URL).toBe("https://x");

    rerender({ initialConfig: config });
    expect(result.current.apiKey).toBe("sk-new");
  });

  it("syncs the input from external JSON without looping on apiKey", () => {
    const onConfigChange = vi.fn();
    const first = JSON.stringify({ env: { ANTHROPIC_AUTH_TOKEN: "a" } });
    const { result, rerender } = renderHook(
      ({ initialConfig }: { initialConfig: string }) =>
        useApiKeyState({
          initialConfig,
          onConfigChange,
          selectedPresetId: null,
          category: "custom",
          appType: "claude",
        }),
      { initialProps: { initialConfig: first } },
    );

    const second = JSON.stringify({
      env: { ANTHROPIC_AUTH_TOKEN: "b", extra: true },
    });
    rerender({ initialConfig: second });
    expect(result.current.apiKey).toBe("b");
    expect(onConfigChange).not.toHaveBeenCalled();
  });
});
