/** OpenCodex-style `{slug}/{model}` helpers. Keep in sync with Rust `model_routing`. */

const RESERVED_ROUTING_SLUG = "combo";
export const CLAUDE_GATEWAY_MODEL_PREFIX = "anthropic/";

export function isReservedRoutingSlug(slug: string): boolean {
  return slug.trim().toLowerCase() === RESERVED_ROUTING_SLUG;
}

function slugify(raw: string): string {
  let out = "";
  let lastHyphen = false;
  for (const ch of raw) {
    let mapped: string;
    if (ch >= "A" && ch <= "Z") {
      mapped = ch.toLowerCase();
    } else if (
      (ch >= "a" && ch <= "z") ||
      (ch >= "0" && ch <= "9") ||
      ch === "." ||
      ch === "_"
    ) {
      mapped = ch;
    } else {
      mapped = "-";
    }
    if (mapped === "-") {
      if (out && !lastHyphen) {
        out += "-";
        lastHyphen = true;
      }
    } else {
      out += mapped;
      lastHyphen = false;
    }
  }
  while (out.endsWith("-")) {
    out = out.slice(0, -1);
  }
  if (out && !/^[a-z0-9]/i.test(out[0] ?? "")) {
    out = `p${out}`;
  }
  return out || "provider";
}

function isValidSlug(value: string): boolean {
  if (!value) return false;
  return /^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(value);
}

function isUuidLike(value: string): boolean {
  const trimmed = value.trim();
  return (
    trimmed.length === 36 &&
    trimmed[8] === "-" &&
    trimmed[13] === "-" &&
    trimmed[18] === "-" &&
    trimmed[23] === "-" &&
    /^[0-9a-fA-F-]+$/.test(trimmed)
  );
}

export function aliasInnerSlashes(model: string): string {
  return model.split("/").join("-");
}

export type RoutingSlugInput = {
  id?: string;
  name?: string;
  sortIndex?: number;
  createdAt?: number;
  meta?: { routingSlug?: string };
};

/** Same order as the provider DAO: sort_index, created_at, id. */
function compareProvidersForAssign(
  left: RoutingSlugInput,
  right: RoutingSlugInput,
): number {
  const indexA = left.sortIndex ?? Number.MAX_SAFE_INTEGER;
  const indexB = right.sortIndex ?? Number.MAX_SAFE_INTEGER;
  if (indexA !== indexB) return indexA - indexB;
  const timeA = left.createdAt ?? 0;
  const timeB = right.createdAt ?? 0;
  if (timeA !== timeB) return timeA - timeB;
  return (left.id ?? "").localeCompare(right.id ?? "");
}

export function providersInAssignOrder<T extends RoutingSlugInput>(
  providers: T[],
): T[] {
  return [...providers].sort(compareProvidersForAssign);
}

export function preferredRoutingSlug(input: RoutingSlugInput): string {
  const override = input.meta?.routingSlug?.trim();
  if (override) {
    return slugify(override);
  }
  const id = input.id?.trim() ?? "";
  if (isValidSlug(id) && !isUuidLike(id)) {
    return id.toLowerCase();
  }
  const fromName = slugify(input.name ?? "");
  return fromName || slugify(id);
}

function sanitizeIdSuffix(id: string): string {
  const compact = [...id]
    .filter((ch) => /[A-Za-z0-9]/.test(ch))
    .slice(0, 8)
    .join("")
    .toLowerCase();
  return compact || "id";
}

/**
 * Unique slugs across a provider set. Same order and suffix rules as Rust
 * `assign_routing_slugs`: sort by sortIndex / createdAt / id, then explicit
 * routingSlug first, then collisions get `-{8-char id}`.
 */
export function assignRoutingSlugs(
  providers: RoutingSlugInput[],
): Map<string, string> {
  const used = new Set<string>();
  const assigned = new Map<string, string>();
  const withOverride: RoutingSlugInput[] = [];
  const withoutOverride: RoutingSlugInput[] = [];
  for (const provider of providersInAssignOrder(providers)) {
    if (provider.meta?.routingSlug?.trim()) {
      withOverride.push(provider);
    } else {
      withoutOverride.push(provider);
    }
  }

  for (const provider of [...withOverride, ...withoutOverride]) {
    const id = provider.id?.trim() ?? "";
    if (!id) continue;
    let slug = preferredRoutingSlug(provider) || "provider";
    if (used.has(slug)) {
      const suffix = sanitizeIdSuffix(id);
      slug = `${slug}-${suffix}`;
      let n = 2;
      while (used.has(slug)) {
        slug = `${preferredRoutingSlug(provider)}-${suffix}-${n}`;
        n += 1;
      }
    }
    used.add(slug);
    assigned.set(id, slug);
  }
  return assigned;
}

export type AssignedSlugForForm = {
  assigned: string;
  preferred: string;
  collided: boolean;
};

/**
 * Live assignment keeps the edited card in list order. A new card keeps its
 * preferred slug; collision is “preferred already used”.
 */
export function assignedSlugForForm(input: {
  providers: RoutingSlugInput[];
  draft: RoutingSlugInput;
  editingId?: string;
}): AssignedSlugForForm {
  const preferred = preferredRoutingSlug(input.draft);
  const editingId = input.editingId?.trim();
  if (editingId) {
    const list = providersInAssignOrder(input.providers);
    const index = list.findIndex(
      (provider) => provider.id?.trim() === editingId,
    );
    const next: RoutingSlugInput = {
      ...input.draft,
      id: editingId,
    };
    if (index >= 0) {
      list[index] = { ...list[index], ...next };
    } else {
      list.push(next);
    }
    const assigned = assignRoutingSlugs(list).get(editingId) ?? preferred;
    return {
      assigned,
      preferred,
      collided: assigned !== preferred,
    };
  }

  const used = new Set(
    assignRoutingSlugs(providersInAssignOrder(input.providers)).values(),
  );
  return {
    assigned: preferred,
    preferred,
    collided: used.has(preferred),
  };
}

export function routedModelPreview(
  slug: string,
  models: Array<string | undefined | null>,
  limit = 4,
  prefix = "",
): string[] {
  const seen = new Set<string>();
  const previews: string[] = [];
  for (const model of models) {
    const trimmed = model?.trim();
    if (!trimmed) continue;
    const id = `${prefix}${slug}/${aliasInnerSlashes(trimmed)}`;
    if (seen.has(id)) continue;
    seen.add(id);
    previews.push(id);
    if (previews.length >= limit) break;
  }
  return previews;
}
