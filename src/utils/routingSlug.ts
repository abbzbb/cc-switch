/** OpenCodex-style `{slug}/{model}` helpers. Keep in sync with Rust `model_routing`. */

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

export function preferredRoutingSlug(input: {
  id?: string;
  name?: string;
  meta?: { routingSlug?: string };
}): string {
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

export function routedModelPreview(
  slug: string,
  models: Array<string | undefined | null>,
  limit = 4,
): string[] {
  const seen = new Set<string>();
  const previews: string[] = [];
  for (const model of models) {
    const trimmed = model?.trim();
    if (!trimmed) continue;
    const id = `${slug}/${aliasInnerSlashes(trimmed)}`;
    if (seen.has(id)) continue;
    seen.add(id);
    previews.push(id);
    if (previews.length >= limit) break;
  }
  return previews;
}
