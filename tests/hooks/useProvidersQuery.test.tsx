import { describe, expect, it } from "vitest";
import { keepPreviousProvidersIfSameApp } from "@/lib/query/queries";
import type { ProvidersQueryData } from "@/lib/query/queries";

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
