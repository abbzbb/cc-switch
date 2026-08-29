import crossFetch, {
  Headers as CrossFetchHeaders,
  Request as CrossFetchRequest,
  Response as CrossFetchResponse,
} from "cross-fetch";
import { vi } from "vitest";
import { server } from "./server";

const TAURI_ENDPOINT = "http://tauri.local";

globalThis.fetch = crossFetch as typeof fetch;
globalThis.Headers = CrossFetchHeaders as typeof Headers;
globalThis.Request = CrossFetchRequest as typeof Request;
globalThis.Response = CrossFetchResponse as typeof Response;

vi.mock("@tauri-apps/api/core", () => ({
  invoke: async (command: string, payload: Record<string, unknown> = {}) => {
    const response = await fetch(`${TAURI_ENDPOINT}/${command}`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
      },
      body: JSON.stringify(payload ?? {}),
    });

    if (!response.ok) {
      const text = await response.text();
      if (!text) throw new Error(`Invoke failed for ${command}`);
      let payload: unknown = text;
      try {
        payload = JSON.parse(text);
      } catch {
        // Legacy string errors remain strings, matching Tauri invoke.
      }
      throw payload;
    }

    const text = await response.text();
    if (!text) return undefined;
    try {
      return JSON.parse(text);
    } catch {
      return text;
    }
  },
}));

const listeners = new Map<string, Set<(event: { payload: unknown }) => void>>();
let syntheticDeeplinkId = 0;

const ensureListenerSet = (event: string) => {
  if (!listeners.has(event)) {
    listeners.set(event, new Set());
  }
  return listeners.get(event)!;
};

export const emitTauriEvent = (event: string, payload: unknown) => {
  const normalizedEvent =
    event === "deeplink-import" || event === "deeplink-error"
      ? "deeplink-inbox"
      : event;
  const normalizedPayload =
    event === "deeplink-import" || event === "deeplink-error"
      ? {
          id: String(++syntheticDeeplinkId),
          type: event === "deeplink-import" ? "import" : "error",
          payload,
        }
      : payload;
  const handlers = listeners.get(normalizedEvent);
  handlers?.forEach((handler) => handler({ payload: normalizedPayload }));
};

export const getTauriListenerCount = (event: string) =>
  listeners.get(event)?.size ?? 0;

vi.mock("@tauri-apps/api/event", () => ({
  listen: async (
    event: string,
    handler: (event: { payload: unknown }) => void,
  ) => {
    const set = ensureListenerSet(event);
    set.add(handler);
    return () => {
      set.delete(handler);
    };
  },
}));

// Ensure the MSW server is referenced so tree shaking doesn't remove imports
void server;

vi.mock("@tauri-apps/api/path", () => ({
  homeDir: async () => "/home/mock",
  join: async (...segments: string[]) => segments.join("/"),
}));
