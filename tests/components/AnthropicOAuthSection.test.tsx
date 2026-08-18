import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { AnthropicOAuthSection } from "@/components/providers/forms/AnthropicOAuthSection";

const mocks = vi.hoisted(() => ({
  useAnthropicOauth: vi.fn(),
}));

vi.mock("@/components/providers/forms/hooks/useAnthropicOauth", () => ({
  useAnthropicOauth: mocks.useAnthropicOauth,
}));

describe("AnthropicOAuthSection", () => {
  const addAccount = vi.fn();
  const importFromCli = vi.fn();

  beforeEach(() => {
    addAccount.mockReset();
    importFromCli.mockReset();
    mocks.useAnthropicOauth.mockReturnValue({
      accounts: [],
      defaultAccountId: null,
      hasAnyAccount: false,
      isAuthenticated: false,
      pollingState: "idle",
      deviceCode: null,
      error: null,
      isPolling: false,
      isAddingAccount: false,
      isRemovingAccount: false,
      isSettingDefaultAccount: false,
      addAccount,
      cancelAuth: vi.fn(),
      removeAccount: vi.fn(),
      setDefaultAccount: vi.fn(),
      logout: vi.fn(),
      importFromCli,
      isImporting: false,
    });
  });

  it("offers browser login without showing a device code", async () => {
    const user = userEvent.setup();
    render(<AnthropicOAuthSection />);

    await user.click(
      screen.getByRole("button", {
        name: /使用浏览器登录|Sign in with browser/i,
      }),
    );
    expect(addAccount).toHaveBeenCalled();
    expect(screen.queryByText("BROWSER")).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", {
        name: /从 Claude CLI 导入|Import from Claude CLI/i,
      }),
    ).toBeInTheDocument();
  });

  it("shows a browser waiting state instead of a user code", () => {
    mocks.useAnthropicOauth.mockReturnValue({
      ...mocks.useAnthropicOauth(),
      pollingState: "polling",
      isPolling: true,
      isAddingAccount: true,
      deviceCode: {
        device_code: "pending-1",
        user_code: "BROWSER",
        verification_uri: "https://claude.ai/oauth/authorize?code_challenge=x",
        expires_in: 600,
        interval: 2,
      },
    });
    render(<AnthropicOAuthSection />);
    expect(
      screen.getByText(/等待浏览器授权|Waiting for browser/i),
    ).toBeInTheDocument();
    expect(screen.queryByText("BROWSER")).not.toBeInTheDocument();
  });
});
