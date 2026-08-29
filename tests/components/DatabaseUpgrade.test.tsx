const exitMock = vi.hoisted(() => vi.fn(async () => undefined));

vi.mock("@tauri-apps/plugin-process", () => ({
  exit: exitMock,
}));

import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import * as eventApi from "@tauri-apps/api/event";
import { DatabaseUpgrade } from "@/components/DatabaseUpgrade";
import { server } from "../msw/server";
import { emitTauriEvent, getTauriListenerCount } from "../msw/tauriMocks";

const TAURI_ENDPOINT = "http://tauri.local";
const payload = {
  path: "/tmp/cc-switch.db",
  error: "database schema is newer",
  kind: "db_version_too_new",
  db_version: 42,
  supported_version: 41,
};

function handleCommand(
  command: string,
  resolver: () => Response | Promise<Response>,
) {
  server.use(http.post(`${TAURI_ENDPOINT}/${command}`, resolver));
}

describe("DatabaseUpgrade", () => {
  it("offers the available update and shows database version context", async () => {
    handleCommand("check_app_update_available", () =>
      HttpResponse.json("3.21.0"),
    );

    render(<DatabaseUpgrade payload={payload} />);

    expect(screen.getByText("正在检查可用更新…")).toBeInTheDocument();
    expect(await screen.findByText(/3\.21\.0/)).toBeInTheDocument();
    expect(screen.getByText(/v42/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "升级应用" })).toBeEnabled();
  });

  it("explains when the current app cannot upgrade the database", async () => {
    handleCommand("check_app_update_available", () => HttpResponse.json(null));
    let openedUrl: string | undefined;
    server.use(
      http.post(`${TAURI_ENDPOINT}/open_external`, async ({ request }) => {
        const body = (await request.json()) as { url?: string };
        openedUrl = body.url;
        return HttpResponse.json(null);
      }),
    );
    const user = userEvent.setup();

    render(<DatabaseUpgrade payload={payload} />);

    expect(await screen.findByText("升级也无法解决")).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "升级应用" }),
    ).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "打开发布页" }));
    await waitFor(() => {
      expect(openedUrl).toBe("https://github.com/abbzbb/cc-switch/releases");
    });
  });

  it("keeps the recovery action available when the update check fails", async () => {
    handleCommand("check_app_update_available", () =>
      HttpResponse.json({ message: "offline" }, { status: 503 }),
    );

    render(<DatabaseUpgrade payload={payload} />);

    expect(
      await screen.findByRole("button", { name: "升级应用" }),
    ).toBeEnabled();
  });

  it("reports download progress and removes its listener on unmount", async () => {
    handleCommand("check_app_update_available", () =>
      HttpResponse.json("3.21.0"),
    );
    let resolveInstall!: (response: Response) => void;
    const installResponse = new Promise<Response>((resolve) => {
      resolveInstall = resolve;
    });
    handleCommand("install_update_and_restart", () => installResponse);
    const user = userEvent.setup();
    const view = render(<DatabaseUpgrade payload={payload} />);

    await user.click(await screen.findByRole("button", { name: "升级应用" }));
    await waitFor(() => {
      expect(getTauriListenerCount("update-download-progress")).toBe(1);
    });

    act(() => {
      emitTauriEvent("update-download-progress", {
        downloaded: 5 * 1024 * 1024,
        total: 10 * 1024 * 1024,
      });
    });

    expect(await screen.findByText("50%")).toBeInTheDocument();
    expect(screen.getByText("5.0 MB / 10.0 MB")).toBeInTheDocument();

    view.unmount();
    expect(getTauriListenerCount("update-download-progress")).toBe(0);
    resolveInstall(HttpResponse.json(true));
  });

  it("cleans up a listener that resolves after unmount without installing", async () => {
    handleCommand("check_app_update_available", () =>
      HttpResponse.json("3.21.0"),
    );
    const installHandler = vi.fn(() => HttpResponse.json(true));
    handleCommand("install_update_and_restart", installHandler);
    let resolveListen!: (unlisten: () => void) => void;
    const deferredListen = new Promise<() => void>((resolve) => {
      resolveListen = resolve;
    });
    const delayedUnlisten = vi.fn();
    const listenSpy = vi
      .spyOn(eventApi, "listen")
      .mockReturnValueOnce(deferredListen);
    const user = userEvent.setup();
    const view = render(<DatabaseUpgrade payload={payload} />);

    await user.click(await screen.findByRole("button", { name: "升级应用" }));
    expect(listenSpy).toHaveBeenCalledWith(
      "update-download-progress",
      expect.any(Function),
    );

    view.unmount();
    await act(async () => {
      resolveListen(delayedUnlisten);
      await deferredListen;
    });

    expect(delayedUnlisten).toHaveBeenCalledOnce();
    expect(installHandler).not.toHaveBeenCalled();
    listenSpy.mockRestore();
  });

  it("handles the race where the update disappears before installation", async () => {
    handleCommand("check_app_update_available", () =>
      HttpResponse.json("3.21.0"),
    );
    handleCommand("install_update_and_restart", () => HttpResponse.json(false));
    const user = userEvent.setup();

    render(<DatabaseUpgrade payload={payload} />);

    await user.click(await screen.findByRole("button", { name: "升级应用" }));
    expect(await screen.findByText("升级也无法解决")).toBeInTheDocument();
    expect(getTauriListenerCount("update-download-progress")).toBe(0);
  });

  it("shows an install error and allows a retry", async () => {
    handleCommand("check_app_update_available", () =>
      HttpResponse.json("3.21.0"),
    );
    handleCommand("install_update_and_restart", () =>
      HttpResponse.json({ message: "signature rejected" }, { status: 500 }),
    );
    const user = userEvent.setup();

    render(<DatabaseUpgrade payload={payload} />);

    await user.click(await screen.findByRole("button", { name: "升级应用" }));
    expect(await screen.findByText("signature rejected")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "重试升级" })).toBeEnabled();
    expect(getTauriListenerCount("update-download-progress")).toBe(0);
  });
});
