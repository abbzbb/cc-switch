import type { ComboTarget } from "@/types/proxy";
import {
  aliasInnerSlashes,
  assignRoutingSlugs,
  type RoutingSlugInput,
} from "@/utils/routingSlug";

export const COMBO_NAMESPACE = "combo";
const COMBO_PREFIX = "combo/";
const CLAUDE_COMBO_PREFIX = "anthropic/combo/";
const MAX_COMBO_ID_LEN = 64;
const MIN_COMBO_WEIGHT = 1;
const MAX_COMBO_WEIGHT = 10_000;

type ComboParseErrorKind =
  | "invalid_target"
  | "nested_combo"
  | "invalid_weight"
  | "duplicate_target";

type ComboParseError = {
  kind: ComboParseErrorKind;
  spec: string;
};

export type ComboParseResult =
  | { ok: true; targets: ComboTarget[] }
  | { ok: false; error: ComboParseError };

/** Strip a displayed `combo/` or `anthropic/combo/` prefix. Keep the bare id. */
export function normalizeComboId(raw: string): string {
  let id = raw.trim();
  const lower = id.toLowerCase();
  if (lower.startsWith(CLAUDE_COMBO_PREFIX)) {
    id = id.slice(CLAUDE_COMBO_PREFIX.length);
  } else if (lower.startsWith(COMBO_PREFIX)) {
    id = id.slice(COMBO_PREFIX.length);
  }
  return id.trim();
}

export function isReservedComboId(id: string): boolean {
  return normalizeComboId(id).toLowerCase() === COMBO_NAMESPACE;
}

/** Same grammar as Rust `is_valid_combo_id` (reserved `combo` is separate). */
export function isValidComboId(id: string): boolean {
  const trimmed = id.trim();
  if (!trimmed || trimmed.length > MAX_COMBO_ID_LEN) {
    return false;
  }
  return /^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(trimmed);
}

function splitOptionalWeight(
  spec: string,
): { route: string; weight?: number; error?: ComboParseError } {
  const lastColon = spec.lastIndexOf(":");
  if (lastColon <= 0) {
    return { route: spec };
  }
  const right = spec.slice(lastColon + 1);
  if (!/^\d+$/.test(right)) {
    return { route: spec };
  }
  const weight = Number(right);
  const left = spec.slice(0, lastColon);
  if (!left) {
    return { route: spec };
  }
  if (
    !Number.isInteger(weight) ||
    weight < MIN_COMBO_WEIGHT ||
    weight > MAX_COMBO_WEIGHT
  ) {
    return {
      route: spec,
      error: { kind: "invalid_weight", spec },
    };
  }
  return { route: left, weight };
}

export function parseComboTargets(text: string): ComboParseResult {
  const targets: ComboTarget[] = [];
  const seen = new Set<string>();
  const lines = text
    .split(/[\n,]+/)
    .map((line) => line.trim())
    .filter(Boolean);

  for (const spec of lines) {
    const split = splitOptionalWeight(spec);
    if (split.error) {
      return { ok: false, error: split.error };
    }
    const route = split.route.trim();
    const slash = route.indexOf("/");
    if (slash <= 0 || slash === route.length - 1) {
      return { ok: false, error: { kind: "invalid_target", spec } };
    }
    const provider = route.slice(0, slash).trim();
    const model = route.slice(slash + 1).trim();
    if (!provider || !model) {
      return { ok: false, error: { kind: "invalid_target", spec } };
    }
    if (provider.toLowerCase() === COMBO_NAMESPACE) {
      return { ok: false, error: { kind: "nested_combo", spec } };
    }
    const key = `${provider.toLowerCase()}\0${aliasInnerSlashes(model).toLowerCase()}`;
    if (seen.has(key)) {
      return { ok: false, error: { kind: "duplicate_target", spec } };
    }
    seen.add(key);
    targets.push({
      provider,
      model,
      ...(split.weight && split.weight !== 1 ? { weight: split.weight } : {}),
    });
  }

  return { ok: true, targets };
}

type ComboHopResolve = {
  matched: boolean;
  assignedSlug?: string;
  providerId?: string;
  providerName?: string;
};

/** Same match as Rust `resolve_combo_targets`: assigned slug or provider id. */
export function resolveComboHop(
  target: ComboTarget,
  providers: RoutingSlugInput[],
): ComboHopResolve {
  const slugs = assignRoutingSlugs(providers);
  const want = target.provider.trim().toLowerCase();
  const provider = providers.find((item) => {
    const id = item.id?.trim() ?? "";
    if (!id) return false;
    const slug = slugs.get(id);
    return slug === want || id.toLowerCase() === want;
  });
  if (!provider?.id) {
    return { matched: false };
  }
  return {
    matched: true,
    assignedSlug: slugs.get(provider.id),
    providerId: provider.id,
    providerName: provider.name,
  };
}

export function formatComboTargets(targets: ComboTarget[]): string {
  return targets
    .map((target) => {
      const route = `${target.provider}/${target.model}`;
      return target.weight && target.weight !== 1
        ? `${route}:${target.weight}`
        : route;
    })
    .join("\n");
}
