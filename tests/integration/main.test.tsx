const mocks = vi.hoisted(() => ({
  app: vi.fn(() => null),
  createRoot: vi.fn(),
  databaseUpgrade: vi.fn((_props: { payload: unknown }) => null),
  exit: vi.fn(async () => undefined),
  initializeWindowActivity: vi.fn(),
  installGlobalErrorHandlers: vi.fn(),
  invoke: vi.fn(),
  listen: vi.fn(async () => vi.fn()),
  message: vi.fn(async (_message: string) => undefined),
  renderRoot: vi.fn(),
  reportFrontendError: vi.fn(),
  syncModelsDevPricingOnStartup: vi.fn(),
}));

vi.mock("react-dom/client", () => ({
  default: {
    createRoot: mocks.createRoot,
  },
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: mocks.listen }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ message: mocks.message }));
vi.mock("@tauri-apps/plugin-process", () => ({ exit: mocks.exit }));

vi.mock("@/App", () => ({ default: mocks.app }));
vi.mock("@/components/DatabaseUpgrade", () => ({
  DatabaseUpgrade: mocks.databaseUpgrade,
}));
vi.mock("@/components/FrontendErrorBoundary", () => ({
  FrontendErrorBoundary: ({ children }: { children: React.ReactNode }) =>
    children,
}));
vi.mock("@/components/theme-provider", () => ({
  ThemeProvider: ({ children }: { children: React.ReactNode }) => children,
}));
vi.mock("@/components/ui/sonner", () => ({ Toaster: () => null }));
vi.mock("@/contexts/UpdateContext", () => ({
  UpdateProvider: ({ children }: { children: React.ReactNode }) => children,
}));
vi.mock("@/lib/frontendLogger", () => ({
  installGlobalErrorHandlers: mocks.installGlobalErrorHandlers,
  reportFrontendError: mocks.reportFrontendError,
}));
vi.mock("@/lib/modelsDevAutoSync", () => ({
  MODELS_DEV_SYNC_CONFIG_QUERY_KEY: ["models-dev-sync-config"],
  syncModelsDevPricingOnStartup: mocks.syncModelsDevPricingOnStartup,
}));
vi.mock("@/lib/windowActivity", () => ({
  initializeWindowActivity: mocks.initializeWindowActivity,
}));

import { render, waitFor } from "@testing-library/react";

describe("application bootstrap", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.clearAllMocks();
    document.body.innerHTML = '<div id="root"></div>';
    mocks.createRoot.mockReturnValue({ render: mocks.renderRoot });
    mocks.listen.mockResolvedValue(vi.fn());
    mocks.syncModelsDevPricingOnStartup.mockResolvedValue({ skipped: true });
  });

  it("routes a future-schema database to the recovery screen only", async () => {
    const initError = {
      kind: "db_version_too_new",
      path: "/tmp/cc-switch.db",
      error: "database schema is newer",
      db_version: 42,
      supported_version: 41,
    };
    mocks.invoke.mockResolvedValue(initError);

    await import("@/main");

    await waitFor(() => expect(mocks.renderRoot).toHaveBeenCalledOnce());
    render(mocks.renderRoot.mock.calls[0][0]);

    const recoveryProps = mocks.databaseUpgrade.mock.calls.at(0)?.[0];
    expect(recoveryProps?.payload).toEqual(initError);
    expect(mocks.app).not.toHaveBeenCalled();
    expect(mocks.initializeWindowActivity).not.toHaveBeenCalled();
    expect(mocks.syncModelsDevPricingOnStartup).not.toHaveBeenCalled();
  });

  it("fails closed when a persisted app config directory is unavailable", async () => {
    const initError = {
      kind: "app_config_dir_unavailable",
      path: "app_paths.json",
      error: "CC Switch 配置目录不存在: /mnt/offline/cc-switch",
    };
    mocks.invoke.mockResolvedValue(initError);

    await import("@/main");

    await waitFor(() => expect(mocks.message).toHaveBeenCalledOnce());
    expect(mocks.message.mock.calls[0][0]).toContain("/mnt/offline/cc-switch");
    expect(mocks.exit).toHaveBeenCalledWith(1);
    expect(mocks.renderRoot).not.toHaveBeenCalled();
    expect(mocks.app).not.toHaveBeenCalled();
    expect(mocks.initializeWindowActivity).not.toHaveBeenCalled();
  });

  it("starts the normal application when initialization succeeds", async () => {
    mocks.invoke.mockResolvedValue(null);

    await import("@/main");

    await waitFor(() => expect(mocks.renderRoot).toHaveBeenCalledOnce());
    render(mocks.renderRoot.mock.calls[0][0]);

    expect(mocks.app).toHaveBeenCalled();
    expect(mocks.databaseUpgrade).not.toHaveBeenCalled();
    expect(mocks.initializeWindowActivity).toHaveBeenCalledOnce();
    expect(mocks.syncModelsDevPricingOnStartup).toHaveBeenCalledOnce();
  });
});
