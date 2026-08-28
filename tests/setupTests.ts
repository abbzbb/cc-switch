import "@testing-library/jest-dom";
import { afterAll, afterEach, beforeAll, vi } from "vitest";
import { cleanup } from "@testing-library/react";
import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import { server } from "./msw/server";
import { resetProviderState } from "./msw/state";
import "./msw/tauriMocks";

const unhandledRequests: string[] = [];

vi.mock("@tauri-apps/plugin-log", () => ({
  error: vi.fn(),
  warn: vi.fn(),
  info: vi.fn(),
  debug: vi.fn(),
  trace: vi.fn(),
}));

vi.mock("@tauri-apps/api/window", () => {
  const currentWindow = {
    close: vi.fn(async () => undefined),
    isMaximized: vi.fn(async () => false),
    minimize: vi.fn(async () => undefined),
    onFocusChanged: vi.fn(async () => vi.fn()),
    onResized: vi.fn(async () => vi.fn()),
    setDecorations: vi.fn(async () => undefined),
    toggleMaximize: vi.fn(async () => undefined),
  };

  return {
    getCurrentWindow: () => currentWindow,
  };
});

beforeAll(async () => {
  server.listen({
    onUnhandledRequest(request, print) {
      unhandledRequests.push(`${request.method} ${request.url}`);
      print.error();
    },
  });
  await i18n.use(initReactI18next).init({
    lng: "zh",
    fallbackLng: "zh",
    resources: {
      zh: { translation: {} },
      en: { translation: {} },
    },
    interpolation: {
      escapeValue: false,
    },
  });
});

afterEach(() => {
  cleanup();
  resetProviderState();
  server.resetHandlers();
  vi.clearAllMocks();

  const requests = unhandledRequests.splice(0);
  if (requests.length > 0) {
    throw new Error(`Unhandled MSW requests:\n${requests.join("\n")}`);
  }
});

afterAll(() => {
  server.close();
});
