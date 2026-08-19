import { describe, expect, it } from "vitest";
import {
  aliasInnerSlashes,
  assignedSlugForForm,
  assignRoutingSlugs,
  CLAUDE_GATEWAY_MODEL_PREFIX,
  isReservedRoutingSlug,
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

  it("prefixes Claude gateway catalog ids", () => {
    expect(
      routedModelPreview("kimi", ["k2"], 4, CLAUDE_GATEWAY_MODEL_PREFIX),
    ).toEqual(["anthropic/kimi/k2"]);
  });
});

describe("aliasInnerSlashes", () => {
  it("replaces every slash", () => {
    expect(aliasInnerSlashes("org/team/model")).toBe("org-team-model");
  });
});

describe("assignRoutingSlugs", () => {
  it("suffixes a colliding preferred slug with the id", () => {
    const first = "2c0f1a6e-9b11-4d22-8c33-abcdef123456";
    const second = "aaaaaaaa-9b11-4d22-8c33-abcdef123456";
    const assigned = assignRoutingSlugs([
      { id: first, name: "DeepSeek" },
      { id: second, name: "DeepSeek" },
    ]);
    expect(assigned.get(first)).toBe("deepseek");
    expect(assigned.get(second)).toBe("deepseek-aaaaaaaa");
  });

  it("lets an explicit override win, then suffixes the other card", () => {
    const other = "bbbbbbbb-9b11-4d22-8c33-abcdef123456";
    const assigned = assignRoutingSlugs([
      { id: "kimi", name: "Kimi", meta: { routingSlug: "kimi" } },
      { id: other, name: "Kimi" },
    ]);
    expect(assigned.get("kimi")).toBe("kimi");
    expect(assigned.get(other)).toBe("kimi-bbbbbbbb");
  });

  it("uses -n when two colliding ids share an 8-char suffix", () => {
    const first = "2c0f1a6e-9b11-4d22-8c33-abcdef123456";
    const second = "aabbccdd-1111-4d22-8c33-abcdef123456";
    const third = "aabbccdd-2222-4d22-8c33-abcdef123456";
    const assigned = assignRoutingSlugs([
      { id: first, name: "Same" },
      { id: second, name: "Same" },
      { id: third, name: "Same" },
    ]);
    expect(assigned.get(first)).toBe("same");
    expect(assigned.get(second)).toBe("same-aabbccdd");
    expect(assigned.get(third)).toBe("same-aabbccdd-2");
  });

  it("sorts by sortIndex, not caller array order", () => {
    const later = {
      id: "bbbbbbbb-9b11-4d22-8c33-abcdef123456",
      name: "Same",
      sortIndex: 1,
    };
    const earlier = {
      id: "aaaaaaaa-9b11-4d22-8c33-abcdef123456",
      name: "Same",
      sortIndex: 0,
    };
    const assigned = assignRoutingSlugs([later, earlier]);
    expect(assigned.get(earlier.id)).toBe("same");
    expect(assigned.get(later.id)).toBe("same-bbbbbbbb");
  });
});

describe("assignedSlugForForm", () => {
  const first = "2c0f1a6e-9b11-4d22-8c33-abcdef123456";
  const second = "aaaaaaaa-9b11-4d22-8c33-abcdef123456";
  const providers = [
    { id: first, name: "DeepSeek", sortIndex: 0 },
    { id: second, name: "DeepSeek", sortIndex: 1 },
  ];

  it("keeps the edited card in list order so the live winner stays first", () => {
    const live = assignedSlugForForm({
      providers,
      draft: { id: first, name: "DeepSeek" },
      editingId: first,
    });
    expect(live.assigned).toBe("deepseek");
    expect(live.collided).toBe(false);

    const loser = assignedSlugForForm({
      providers,
      draft: { id: second, name: "DeepSeek" },
      editingId: second,
    });
    expect(loser.assigned).toBe("deepseek-aaaaaaaa");
    expect(loser.collided).toBe(true);
  });

  it("does not invent a draft suffix for a new card", () => {
    const created = assignedSlugForForm({
      providers,
      draft: { name: "DeepSeek" },
    });
    expect(created.assigned).toBe("deepseek");
    expect(created.collided).toBe(true);
  });
});

describe("isReservedRoutingSlug", () => {
  it("reserves combo regardless of case", () => {
    expect(isReservedRoutingSlug("combo")).toBe(true);
    expect(isReservedRoutingSlug("Combo")).toBe(true);
    expect(isReservedRoutingSlug("kimi")).toBe(false);
  });
});
