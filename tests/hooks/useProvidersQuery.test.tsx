import type { ReactNode } from "react";
import { QueryClientProvider } from "@tanstack/react-query";
import { renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  keepPreviousProvidersIfSameApp,
  useProvidersQuery,
} from "@/lib/query/queries";
import type { ProvidersQueryData } from "@/lib/query/queries";
import { createTestQueryClient } from "../utils/testQueryClient";

const apiMocks = vi.hoisted(() => ({
  getAll: vi.fn(),
  getCurrent: vi.fn(),
}));

vi.mock("@/lib/api", () => ({
  providersApi: {
    getAll: (...args: unknown[]) => apiMocks.getAll(...args),
    getCurrent: (...args: unknown[]) => apiMocks.getCurrent(...args),
  },
  settingsApi: {},
  usageApi: {},
  sessionsApi: {},
}));

const claudeData: ProvidersQueryData = {
  providers: {
    "claude-1": {
      id: "claude-1",
      name: "Claude",
      settingsConfig: {},
    },
  },
  currentProviderId: "claude-1",
};

const codexData: ProvidersQueryData = {
  providers: {
    "codex-1": {
      id: "codex-1",
      name: "Codex",
      settingsConfig: {},
    },
  },
  currentProviderId: "codex-1",
};

describe("keepPreviousProvidersIfSameApp", () => {
  it("keeps previous data when the last query was for the same app", () => {
    expect(
      keepPreviousProvidersIfSameApp(
        claudeData,
        { queryKey: ["providers", "claude"] },
        "claude",
      ),
    ).toBe(claudeData);
  });

  it("drops previous data when switching apps so the old list cannot be acted on", () => {
    expect(
      keepPreviousProvidersIfSameApp(
        claudeData,
        { queryKey: ["providers", "claude"] },
        "codex",
      ),
    ).toBeUndefined();
    expect(
      keepPreviousProvidersIfSameApp(
        codexData,
        { queryKey: ["providers", "codex"] },
        "claude",
      ),
    ).toBeUndefined();
  });

  it("returns undefined when there is no previous query", () => {
    expect(
      keepPreviousProvidersIfSameApp(claudeData, undefined, "claude"),
    ).toBeUndefined();
  });
});

describe("useProvidersQuery getCurrent failure", () => {
  beforeEach(() => {
    apiMocks.getAll.mockReset();
    apiMocks.getCurrent.mockReset();
  });

  it("keeps the previous current provider id when getCurrent fails", async () => {
    apiMocks.getAll.mockResolvedValue(claudeData.providers);
    apiMocks.getCurrent
      .mockResolvedValueOnce("claude-1")
      .mockRejectedValueOnce(new Error("current unavailable"));

    const queryClient = createTestQueryClient();
    const wrapper = ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
    );

    const { result } = renderHook(() => useProvidersQuery("claude"), {
      wrapper,
    });

    await waitFor(() => {
      expect(result.current.data?.currentProviderId).toBe("claude-1");
    });

    await result.current.refetch();

    await waitFor(() => {
      expect(apiMocks.getCurrent).toHaveBeenCalledTimes(2);
    });
    expect(result.current.data?.currentProviderId).toBe("claude-1");
    expect(result.current.isError).toBe(false);
  });
});
