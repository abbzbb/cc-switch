import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { DeepLinkImportDialog } from "@/components/DeepLinkImportDialog";
import { emitTauriEvent } from "../msw/tauriMocks";

const deeplinkMocks = vi.hoisted(() => ({
  importFromDeeplink: vi.fn(),
  mergeDeeplinkConfig: vi.fn(async (request: unknown) => request),
  listenerReady: vi.fn<() => Promise<unknown[]>>(async () => []),
  ack: vi.fn(async () => true),
}));

vi.mock("@/lib/api/deeplink", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/api/deeplink")>();
  return {
    ...actual,
    deeplinkApi: {
      ...actual.deeplinkApi,
      importFromDeeplink: deeplinkMocks.importFromDeeplink,
      mergeDeeplinkConfig: deeplinkMocks.mergeDeeplinkConfig,
      listenerReady: deeplinkMocks.listenerReady,
      ack: deeplinkMocks.ack,
    },
  };
});

vi.mock("@/components/ui/dialog", () => ({
  Dialog: ({
    open,
    onOpenChange,
    children,
  }: {
    open?: boolean;
    onOpenChange?: (open: boolean) => void;
    children: React.ReactNode;
  }) => (
    <div>
      {open ? children : null}
      <button
        type="button"
        data-testid="dialog-request-close"
        onClick={() => onOpenChange?.(false)}
      >
        request-close
      </button>
    </div>
  ),
  DialogContent: ({ children }: { children: React.ReactNode }) => (
    <div>{children}</div>
  ),
  DialogHeader: ({ children }: { children: React.ReactNode }) => (
    <div>{children}</div>
  ),
  DialogTitle: ({ children }: { children: React.ReactNode }) => (
    <h1>{children}</h1>
  ),
  DialogDescription: ({ children }: { children: React.ReactNode }) => (
    <p>{children}</p>
  ),
  DialogFooter: ({ children }: { children: React.ReactNode }) => (
    <div>{children}</div>
  ),
}));

const Wrapper = ({ children }: { children: React.ReactNode }) => (
  <QueryClientProvider client={new QueryClient()}>
    {children}
  </QueryClientProvider>
);

describe("DeepLinkImportDialog", () => {
  beforeEach(() => {
    deeplinkMocks.importFromDeeplink.mockReset();
    deeplinkMocks.mergeDeeplinkConfig.mockReset();
    deeplinkMocks.mergeDeeplinkConfig.mockImplementation(
      async (request: unknown) => request,
    );
    deeplinkMocks.listenerReady.mockReset();
    deeplinkMocks.listenerReady.mockResolvedValue([]);
    deeplinkMocks.ack.mockReset();
    deeplinkMocks.ack.mockResolvedValue(true);
  });

  it("loads startup inbox items in order without replacing the active dialog", async () => {
    deeplinkMocks.listenerReady.mockResolvedValue([
      {
        id: "100",
        type: "import",
        payload: {
          version: "v1",
          resource: "provider",
          app: "claude",
          name: "First Provider",
          endpoint: "https://first.example.com",
        },
      },
      {
        id: "101",
        type: "import",
        payload: {
          version: "v1",
          resource: "provider",
          app: "claude",
          name: "Second Provider",
          endpoint: "https://second.example.com",
        },
      },
    ]);

    render(<DeepLinkImportDialog />, { wrapper: Wrapper });

    expect(await screen.findByText("First Provider")).toBeInTheDocument();
    expect(screen.queryByText("Second Provider")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "common.cancel" }));

    await waitFor(() => expect(deeplinkMocks.ack).toHaveBeenCalledWith("100"));
    expect(await screen.findByText("Second Provider")).toBeInTheDocument();
  });

  it("does not advance when cancelling cannot acknowledge the active item", async () => {
    deeplinkMocks.listenerReady.mockResolvedValue([
      {
        id: "200",
        type: "import",
        payload: {
          version: "v1",
          resource: "provider",
          app: "claude",
          name: "Retry Cancel Provider",
        },
      },
      {
        id: "201",
        type: "import",
        payload: {
          version: "v1",
          resource: "provider",
          app: "claude",
          name: "Blocked Provider",
        },
      },
    ]);
    deeplinkMocks.ack.mockRejectedValueOnce(new Error("transport failed"));

    render(<DeepLinkImportDialog />, { wrapper: Wrapper });
    expect(
      await screen.findByText("Retry Cancel Provider"),
    ).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "common.cancel" }));
    await waitFor(() => expect(deeplinkMocks.ack).toHaveBeenCalledWith("200"));
    expect(screen.getByText("Retry Cancel Provider")).toBeInTheDocument();
    expect(screen.queryByText("Blocked Provider")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "common.cancel" }));
    expect(await screen.findByText("Blocked Provider")).toBeInTheDocument();
  });

  it("retries only acknowledgement after an import already succeeded", async () => {
    deeplinkMocks.importFromDeeplink.mockResolvedValue({
      type: "provider",
      id: "imported-provider",
    });
    deeplinkMocks.ack.mockRejectedValueOnce(new Error("transport failed"));

    render(<DeepLinkImportDialog />, { wrapper: Wrapper });
    act(() => {
      emitTauriEvent("deeplink-inbox", {
        id: "300",
        type: "import",
        payload: {
          version: "v1",
          resource: "provider",
          app: "claude",
          name: "Imported Once",
        },
      });
    });

    fireEvent.click(
      await screen.findByRole("button", { name: "deeplink.import" }),
    );
    const retry = await screen.findByRole("button", { name: "common.retry" });
    expect(deeplinkMocks.importFromDeeplink).toHaveBeenCalledTimes(1);

    fireEvent.click(retry);
    await waitFor(() => expect(deeplinkMocks.ack).toHaveBeenCalledTimes(2));
    expect(deeplinkMocks.importFromDeeplink).toHaveBeenCalledTimes(1);
    await waitFor(() =>
      expect(screen.queryByText("Imported Once")).not.toBeInTheDocument(),
    );
  });

  it("queues an arriving event behind the active dialog", async () => {
    render(<DeepLinkImportDialog />, { wrapper: Wrapper });

    act(() => {
      emitTauriEvent("deeplink-import", {
        version: "v1",
        resource: "provider",
        app: "claude",
        name: "Active Provider",
        endpoint: "https://active.example.com",
      });
      emitTauriEvent("deeplink-import", {
        version: "v1",
        resource: "provider",
        app: "claude",
        name: "Queued Provider",
        endpoint: "https://queued.example.com",
      });
    });

    expect(await screen.findByText("Active Provider")).toBeInTheDocument();
    expect(screen.queryByText("Queued Provider")).not.toBeInTheDocument();
  });

  it("renders masked usage access token and user id for provider imports", async () => {
    render(<DeepLinkImportDialog />, { wrapper: Wrapper });

    act(() => {
      emitTauriEvent("deeplink-import", {
        version: "v1",
        resource: "provider",
        app: "claude",
        name: "Test Provider",
        homepage: "https://example.com",
        endpoint: "https://api.example.com",
        apiKey: "sk-provider-key",
        usageEnabled: true,
        usageScript: btoa("console.log('usage');"),
        usageApiKey: "sk-usage-key",
        usageBaseUrl: "https://usage.example.com",
        usageAccessToken: "pat-secret-token",
        usageUserId: "user-12345",
        usageAutoInterval: 60,
      });
    });

    await waitFor(() => {
      expect(screen.getByText("用量访问令牌")).toBeInTheDocument();
    });

    expect(screen.getByText("用量用户 ID")).toBeInTheDocument();
    expect(screen.getByText("user-12345")).toBeInTheDocument();
    // Masked: first 4 chars + 12 stars
    expect(screen.getByText("pat-************")).toBeInTheDocument();
  });

  it("shows usage credentials even when the deeplink carries no usageScript", async () => {
    // 后端 build_provider_meta 在任一 usage 字段存在时即持久化（含 access_token
    // 与 user_id）。若对话框只在 usageScript 存在时开门，这条链接会把凭据静默
    // 写进供应商配置。撤销门槛 widening（恢复只按 usageScript 开门）本测试即失败。
    render(<DeepLinkImportDialog />, { wrapper: Wrapper });

    act(() => {
      emitTauriEvent("deeplink-import", {
        version: "v1",
        resource: "provider",
        app: "claude",
        name: "Token Only Provider",
        homepage: "https://example.com",
        endpoint: "https://api.example.com",
        apiKey: "sk-provider-key",
        usageAccessToken: "pat-secret-token",
        usageUserId: "user-12345",
      });
    });

    await waitFor(() => {
      expect(screen.getByText("用量访问令牌")).toBeInTheDocument();
    });

    expect(screen.getByText("pat-************")).toBeInTheDocument();
    expect(screen.getByText("用量用户 ID")).toBeInTheDocument();
    expect(screen.getByText("user-12345")).toBeInTheDocument();
    // 没有脚本就不应渲染脚本执行警告与脚本代码区
    expect(
      screen.queryByText(
        "这是一段 JavaScript 代码，启用后会在查询用量时执行。请确认来源可信后再导入。",
      ),
    ).not.toBeInTheDocument();
    expect(screen.queryByText("脚本代码")).not.toBeInTheDocument();
  });

  it("keeps the dialog open and lists MCP import failures", async () => {
    deeplinkMocks.importFromDeeplink.mockResolvedValue({
      type: "mcp",
      importedCount: 1,
      importedIds: ["ok-server"],
      failed: [{ id: "bad-server", error: "command missing" }],
    });

    render(<DeepLinkImportDialog />, { wrapper: Wrapper });

    act(() => {
      emitTauriEvent("deeplink-import", {
        version: "v1",
        resource: "mcp",
        apps: "claude",
        config: btoa(JSON.stringify({ mcpServers: { "ok-server": {} } })),
      });
    });

    await waitFor(() => {
      expect(
        screen.getByRole("button", { name: "deeplink.import" }),
      ).toBeInTheDocument();
    });

    fireEvent.click(screen.getByRole("button", { name: "deeplink.import" }));

    await waitFor(() => {
      expect(screen.getByText("bad-server")).toBeInTheDocument();
    });
    expect(screen.getByText(/command missing/)).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "deeplink.import" }),
    ).toBeInTheDocument();

    fireEvent.click(screen.getByTestId("dialog-request-close"));
    expect(screen.getByText("bad-server")).toBeInTheDocument();
  });

  it("disables import when an MCP deeplink is missing config or apps", async () => {
    render(<DeepLinkImportDialog />, { wrapper: Wrapper });

    act(() => {
      emitTauriEvent("deeplink-import", {
        version: "v1",
        resource: "mcp",
        apps: "claude",
      });
    });

    const importButton = await screen.findByRole("button", {
      name: "deeplink.import",
    });
    expect(importButton).toBeDisabled();
    fireEvent.click(importButton);
    expect(deeplinkMocks.importFromDeeplink).not.toHaveBeenCalled();
  });

  it("ignores ESC close while an MCP import is in flight", async () => {
    let resolveImport: (value: unknown) => void = () => {};
    deeplinkMocks.importFromDeeplink.mockReturnValue(
      new Promise((resolve) => {
        resolveImport = resolve;
      }),
    );

    render(<DeepLinkImportDialog />, { wrapper: Wrapper });

    act(() => {
      emitTauriEvent("deeplink-import", {
        version: "v1",
        resource: "mcp",
        apps: "claude",
        config: btoa(JSON.stringify({ mcpServers: { "ok-server": {} } })),
      });
    });

    fireEvent.click(
      await screen.findByRole("button", { name: "deeplink.import" }),
    );
    await waitFor(() => {
      expect(
        screen.getByRole("button", { name: "deeplink.importing" }),
      ).toBeInTheDocument();
    });

    fireEvent.click(screen.getByTestId("dialog-request-close"));
    expect(
      screen.getByRole("button", { name: "deeplink.importing" }),
    ).toBeInTheDocument();

    await act(async () => {
      resolveImport({
        type: "mcp",
        importedCount: 1,
        importedIds: ["ok-server"],
        failed: [],
      });
    });
    await waitFor(() =>
      expect(deeplinkMocks.ack).toHaveBeenCalledWith(expect.any(String)),
    );
  });

  it("validates required provider fields before importing", async () => {
    render(<DeepLinkImportDialog />, { wrapper: Wrapper });

    act(() => {
      emitTauriEvent("deeplink-import", {
        version: "v1",
        resource: "provider",
        homepage: "https://example.com",
      });
    });

    await waitFor(() => {
      expect(
        screen.getByRole("button", { name: "deeplink.import" }),
      ).toBeInTheDocument();
    });

    fireEvent.click(screen.getByRole("button", { name: "deeplink.import" }));

    await waitFor(() => {
      expect(deeplinkMocks.importFromDeeplink).not.toHaveBeenCalled();
    });
  });

  it("shows the full prompt content instead of a truncated preview", async () => {
    const content = `line-one\n${"x".repeat(520)}\nline-end`;
    render(<DeepLinkImportDialog />, { wrapper: Wrapper });

    act(() => {
      emitTauriEvent("deeplink-import", {
        version: "v1",
        resource: "prompt",
        app: "claude",
        name: "Long Prompt",
        content: btoa(content),
      });
    });

    await waitFor(() => {
      expect(screen.getByText(/line-one/)).toBeInTheDocument();
    });
    expect(screen.getByText(/line-end/)).toBeInTheDocument();
    expect(screen.queryByText(/\.\.\.$/)).not.toBeInTheDocument();
  });
});
