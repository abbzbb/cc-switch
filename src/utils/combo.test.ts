import { describe, expect, it } from "vitest";
import {
  buildComboConfigOptions,
  clampStickyLimit,
  groupComboConfigOptions,
  isReservedComboId,
  isValidComboId,
  normalizeComboId,
  parseComboTargets,
  providerUpstreamModelIds,
  resolveComboHop,
} from "./combo";

describe("normalizeComboId", () => {
  it("strips combo/ and anthropic/combo/ prefixes", () => {
    expect(normalizeComboId("combo/main")).toBe("main");
    expect(normalizeComboId("COMBO/main")).toBe("main");
    expect(normalizeComboId("anthropic/combo/main")).toBe("main");
    expect(normalizeComboId("main")).toBe("main");
  });
});

describe("isValidComboId", () => {
  it("accepts slug-safe ids without peeling prefixes", () => {
    expect(isValidComboId("main")).toBe(true);
    expect(isValidComboId("a_b-c.1")).toBe(true);
    expect(isValidComboId("combo/main")).toBe(false);
  });

  it("rejects reserved, empty, slash, and overlong ids", () => {
    expect(isValidComboId("combo")).toBe(true);
    expect(isReservedComboId("combo")).toBe(true);
    expect(isValidComboId("combo/combo")).toBe(false);
    expect(isValidComboId("")).toBe(false);
    expect(isValidComboId("combo/main/extra")).toBe(false);
    expect(isValidComboId("a".repeat(65))).toBe(false);
    expect(isReservedComboId("Combo")).toBe(true);
  });
});

describe("parseComboTargets", () => {
  it("parses provider/model and optional weight", () => {
    expect(parseComboTargets("kimi/k2\ndeepseek/deepseek-v4:2")).toEqual({
      ok: true,
      targets: [
        { provider: "kimi", model: "k2" },
        { provider: "deepseek", model: "deepseek-v4", weight: 2 },
      ],
    });
  });

  it("parses inner slashes and rejects weight 0", () => {
    expect(parseComboTargets("kimi/org/model")).toEqual({
      ok: true,
      targets: [{ provider: "kimi", model: "org/model" }],
    });
    expect(parseComboTargets("kimi/k2:0")).toEqual({
      ok: false,
      error: { kind: "invalid_weight", spec: "kimi/k2:0" },
    });
  });

  it("rejects missing slash, nested combo, duplicates, and out-of-range weight", () => {
    expect(parseComboTargets("kimi")).toEqual({
      ok: false,
      error: { kind: "invalid_target", spec: "kimi" },
    });
    expect(parseComboTargets("combo/main")).toEqual({
      ok: false,
      error: { kind: "nested_combo", spec: "combo/main" },
    });
    expect(parseComboTargets("kimi/k2\nkimi/k2")).toEqual({
      ok: false,
      error: { kind: "duplicate_target", spec: "kimi/k2" },
    });
    expect(parseComboTargets("kimi/k2:10001")).toEqual({
      ok: false,
      error: { kind: "invalid_weight", spec: "kimi/k2:10001" },
    });
  });
});

describe("resolveComboHop", () => {
  it("matches assigned slug or provider id and skips unknown hops", () => {
    const kimiId = "2c0f1a6e-9b11-4d22-8c33-abcdef123456";
    const providers = [
      { id: kimiId, name: "Kimi Coding" },
      { id: "deepseek", name: "DeepSeek" },
    ];
    expect(
      resolveComboHop({ provider: "kimi-coding", model: "k2" }, providers),
    ).toMatchObject({
      matched: true,
      assignedSlug: "kimi-coding",
      providerId: kimiId,
    });
    expect(
      resolveComboHop({ provider: "deepseek", model: "v4" }, providers),
    ).toMatchObject({ matched: true, providerId: "deepseek" });
    expect(
      resolveComboHop({ provider: "kimi", model: "k2" }, providers),
    ).toEqual({ matched: false });
  });
});

describe("clampStickyLimit", () => {
  it("clamps to the Rust 1–100 window", () => {
    expect(clampStickyLimit(1)).toBe(1);
    expect(clampStickyLimit(100)).toBe(100);
    expect(clampStickyLimit(0)).toBe(1);
    expect(clampStickyLimit(101)).toBe(100);
    expect(clampStickyLimit(1.5)).toBe(1);
  });
});

describe("providerUpstreamModelIds", () => {
  it("reads catalog, env, toml, and desktop routes", () => {
    expect(
      providerUpstreamModelIds({
        settingsConfig: {
          modelCatalog: { models: [{ model: "k2" }, { model: "k2" }] },
          model: "extra",
          env: { ANTHROPIC_MODEL: "sonnet" },
          config: 'model = "grok-4.5"\n',
        },
        meta: {
          claudeDesktopModelRoutes: { opus: { model: "opus" } },
        },
      }),
    ).toEqual(["k2", "extra", "sonnet", "grok-4.5", "opus"]);
  });
});

describe("buildComboConfigOptions", () => {
  it("groups configs then models and keeps provider id as the value", () => {
    const options = buildComboConfigOptions([
      {
        appId: "codex",
        providers: [
          {
            id: "kimi",
            name: "Kimi",
            settingsConfig: { modelCatalog: { models: [{ model: "k2" }] } },
          },
          {
            id: "grok",
            name: "Grok",
            settingsConfig: {
              modelCatalog: { models: [{ model: "grok-4.6" }] },
            },
          },
        ],
      },
      {
        appId: "claude",
        providers: [
          {
            id: "kimi-claude",
            name: "Kimi",
            settingsConfig: { env: { ANTHROPIC_MODEL: "k2" } },
          },
        ],
      },
    ]);
    expect(options).toEqual([
      {
        value: "kimi",
        slug: "kimi",
        label: "kimi · Kimi",
        appId: "codex",
        models: ["k2"],
      },
      {
        value: "grok",
        slug: "grok",
        label: "grok · Grok",
        appId: "codex",
        models: ["grok-4.6"],
      },
      {
        value: "kimi-claude",
        slug: "kimi-claude",
        label: "kimi-claude · Kimi",
        appId: "claude",
        models: ["k2"],
      },
    ]);
    expect(
      groupComboConfigOptions(options).map((group) => group.appId),
    ).toEqual(["codex", "claude"]);
  });
});
