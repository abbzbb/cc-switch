import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClientProvider } from "@tanstack/react-query";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ProviderForm } from "@/components/providers/forms/ProviderForm";
import { createTestQueryClient } from "../utils/testQueryClient";

const toastMocks = vi.hoisted(() => ({
  error: vi.fn(),
}));

vi.mock("sonner", () => ({
  toast: {
    error: toastMocks.error,
    success: vi.fn(),
  },
}));

vi.mock("@/components/providers/forms/CodexConfigEditor", () => ({
  default: ({
    onAuthChange,
    authError,
  }: {
    onAuthChange: (value: string) => void;
    authError: string;
  }) => (
    <div>
      <button type="button" onClick={() => onAuthChange("{not-json")}>
        inject-invalid-auth
      </button>
      <output data-testid="codex-auth-error">{authError}</output>
    </div>
  ),
}));

vi.mock("@/components/providers/forms/ProviderAdvancedConfig", () => ({
  ProviderAdvancedConfig: () => <div />,
}));

vi.mock("@/components/providers/forms/hooks", async (importOriginal) => {
  const actual =
    await importOriginal<typeof import("@/components/providers/forms/hooks")>();
  return {
    ...actual,
    useCopilotAuth: () => ({
      isAuthenticated: false,
      isStatusSuccess: true,
      isStatusError: false,
      accounts: [],
    }),
    useCodexOauth: () => ({
      isAuthenticated: false,
      isStatusSuccess: true,
      isStatusError: false,
      defaultAccountId: null,
      accounts: [],
    }),
    useXaiOauth: () => ({
      isAuthenticated: false,
      accounts: [],
    }),
    useCommonConfigSnippet: () => ({
      useCommonConfig: false,
      commonConfigSnippet: "",
      commonConfigError: null,
      isLoading: false,
      isExtracting: false,
      handleCommonConfigToggle: vi.fn(),
      handleCommonConfigSnippetChange: vi.fn(),
      handleExtract: vi.fn(),
    }),
    useCodexCommonConfig: () => ({
      useCommonConfig: false,
      commonConfigSnippet: "",
      commonConfigError: null,
      handleCommonConfigToggle: vi.fn(),
      handleCommonConfigSnippetChange: vi.fn(),
      isExtracting: false,
      handleExtract: vi.fn(),
      clearCommonConfigError: vi.fn(),
    }),
    useGeminiCommonConfig: () => ({
      useCommonConfig: false,
      commonConfigSnippet: "",
      commonConfigError: null,
      handleCommonConfigToggle: vi.fn(),
      handleCommonConfigSnippetChange: vi.fn(),
      isExtracting: false,
      handleExtract: vi.fn(),
      clearCommonConfigError: vi.fn(),
    }),
  };
});

vi.mock("@/lib/query", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/query")>();
  return {
    ...actual,
    useSettingsQuery: () => ({
      data: { commonConfigConfirmed: true },
    }),
    useProvidersQuery: () => ({
      data: { providers: {}, currentProviderId: "" },
    }),
  };
});

const hermesLiveIds = vi.hoisted(() => ({ current: [] as string[] }));

vi.mock("@/hooks/useHermes", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/hooks/useHermes")>();
  return {
    ...actual,
    useHermesLiveProviderIds: () => ({
      data: hermesLiveIds.current,
      isLoading: false,
    }),
  };
});

describe("ProviderForm Codex/Gemini parse errors", () => {
  it("disables save and does not submit when Codex auth JSON is invalid", async () => {
    const onSubmit = vi.fn();
    const queryClient = createTestQueryClient();
    render(
      <QueryClientProvider client={queryClient}>
        <ProviderForm
          appId="codex"
          submitLabel="save-provider"
          onSubmit={onSubmit}
          onCancel={vi.fn()}
          initialData={{
            name: "Custom Codex",
            category: "custom",
            settingsConfig: {
              auth: { OPENAI_API_KEY: "sk-ok" },
              config: 'model_provider = "custom"\n',
            },
          }}
        />
      </QueryClientProvider>,
    );

    fireEvent.click(screen.getByRole("button", { name: "inject-invalid-auth" }));
    expect(screen.getByTestId("codex-auth-error").textContent).toMatch(
      /Invalid JSON/i,
    );

    const saveButton = screen.getByRole("button", { name: "save-provider" });
    expect(saveButton).toBeDisabled();

    fireEvent.submit(saveButton.closest("form")!);
    await waitFor(() => {
      expect(onSubmit).not.toHaveBeenCalled();
    });
  });
});

describe("ProviderForm Hermes provider keys", () => {
  beforeEach(() => {
    hermesLiveIds.current = [];
    toastMocks.error.mockReset();
  });

  it("seeds and submits underscore keys like kimi_coding", async () => {
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    const queryClient = createTestQueryClient();
    render(
      <QueryClientProvider client={queryClient}>
        <ProviderForm
          appId="hermes"
          providerId="kimi_coding"
          submitLabel="save-provider"
          onSubmit={onSubmit}
          onCancel={vi.fn()}
          initialData={{
            name: "Kimi For Coding",
            category: "cn_official",
            settingsConfig: {
              name: "kimi_coding",
              base_url: "https://api.kimi.com/coding/",
              api_key: "sk-test",
            },
          }}
        />
      </QueryClientProvider>,
    );

    const keyInput = document.getElementById("hermes-key");
    expect(keyInput).toHaveValue("kimi_coding");

    fireEvent.submit(screen.getByRole("button", { name: "save-provider" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    expect(onSubmit.mock.calls[0][0].providerKey).toBe("kimi_coding");
    expect(toastMocks.error).not.toHaveBeenCalledWith(
      expect.stringMatching(/providerKeyInvalid|Invalid/i),
    );
  });

  it("skips format rejection for locked live keys that already exist", async () => {
    hermesLiveIds.current = ["kimi_coding"];
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    const queryClient = createTestQueryClient();
    render(
      <QueryClientProvider client={queryClient}>
        <ProviderForm
          appId="hermes"
          providerId="kimi_coding"
          submitLabel="save-provider"
          onSubmit={onSubmit}
          onCancel={vi.fn()}
          initialData={{
            name: "Kimi For Coding",
            category: "cn_official",
            settingsConfig: {
              name: "kimi_coding",
              base_url: "https://api.kimi.com/coding/",
              api_key: "sk-test",
            },
          }}
        />
      </QueryClientProvider>,
    );

    const keyInput = document.getElementById("hermes-key");
    expect(keyInput).toHaveValue("kimi_coding");
    expect(keyInput).toBeDisabled();

    fireEvent.submit(screen.getByRole("button", { name: "save-provider" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    expect(onSubmit.mock.calls[0][0].providerKey).toBe("kimi_coding");
  });
});
