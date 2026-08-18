import { describe, expect, it } from "vitest";
import {
  aliasInnerSlashes,
  preferredRoutingSlug,
  routedModelPreview,
} from "./routingSlug";

describe("preferredRoutingSlug", () => {
  it("uses a stable id when it is slug-safe", () => {
    expect(preferredRoutingSlug({ id: "deepseek", name: "DeepSeek" })).toBe(
      "deepseek",
    );
  });

  it("slugifies the display name for UUID ids", () => {
    expect(
      preferredRoutingSlug({
        id: "2c0f1a6e-9b11-4d22-8c33-abcdef123456",
        name: "Kimi Coding",
      }),
    ).toBe("kimi-coding");
  });

  it("prefers an explicit routingSlug override", () => {
    expect(
      preferredRoutingSlug({
        id: "deepseek",
        name: "DeepSeek",
        meta: { routingSlug: "ds" },
      }),
    ).toBe("ds");
  });
});

describe("routedModelPreview", () => {
  it("aliases inner slashes and de-duplicates", () => {
    expect(routedModelPreview("kimi", ["k2", "org/model", "k2", ""])).toEqual([
      "kimi/k2",
      "kimi/org-model",
    ]);
  });
});

describe("aliasInnerSlashes", () => {
  it("replaces every slash", () => {
    expect(aliasInnerSlashes("org/team/model")).toBe("org-team-model");
  });
});
