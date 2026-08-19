# CC Switch 代理功能使用指南

本地 HTTP 代理统一转发 Claude Code、Codex、Gemini CLI 的请求。v3.20.0 起，接管后还可以按供应商钉模型、用 Combo 跨卡、用已登录的官方账号做搜索/识图。**不需要**单独安装或启动 OpenCodex / `ocx`。

完整手册：[用户手册 · 代理与高可用](../user-manual/zh/README.md)。

## 功能一览

- **统一入口** — CLI 打到 `127.0.0.1:15721`，由代理转发
- **按应用接管** — Claude / Codex / Gemini 可分别开关
- **`{slug}/{model}` 钉卡** — 选择器合并目录，带前缀则固定那张卡
- **账号池** — 多张 ChatGPT Official / Kimi For Coding / Claude Pro/Max 时，未加前缀的官方 id 选闲号
- **Combo** — `combo/{id}` 故障转移或加权轮询
- **Sidecar** — 非官方模型的 hosted 搜索、纯文本识图
- **应用级故障转移** — 未加前缀时按队列换整张卡
- **配置保护** — 停止代理时恢复原始 CLI 配置

## 第一次怎么用（10 分钟）

### 1. 准备至少一张卡

- **中转 / API Key**：首页 **+**，选预设，填 Key。
- **Kimi / Claude 订阅**：设置 → **OAuth 认证中心**登录，再添加 `Kimi For Coding (OAuth)` 或 `Claude Pro/Max (OAuth)` 并绑定账号。Token 不写进卡片。步骤见 [2.1](../user-manual/zh/2-providers/2.1-add.md#托管-oauth-供应商kimi--claude-promax)。

编辑卡时建议填好 **路由 slug**（例如 `kimi`），并确认 **参与路由目录** 为开。

### 2. 启动代理并接管

主界面点 **Proxy**（或顶部代理开关）→ **启动代理**。默认 `127.0.0.1:15721`。

打开要走代理的应用开关：**Claude** / **Codex** / **Gemini**。CC Switch 会改 CLI 配置并备份原文。

### 3. 按目标选模型

| 你想要的 | 怎么写 |
|----------|--------|
| 钉死某张卡的某个模型 | Codex：`codex -m "kimi/k2"`；Claude：`claude --model "anthropic/kimi/k2"` |
| 多张官方卡，新对话去闲号 | 不要加 slug，继续用 `gpt-5.5` / `kimi-for-coding` / `claude-*` |
| 几张卡之间自动换或分担 | 代理面板建 Combo，然后 `codex -m "combo/main"` |
| 中转模型要搜网页 / 看图 | 认证中心登录 Claude 或 ChatGPT，打开面板里的 Sidecar |

规则摘要（细节以手册为准）：

- 规范 id：`{routing_slug}/{upstream_model}`。上游 id 里的 `/` 在目录里写成 `-`，两种写法都认。
- 第一段不是已配置 slug 时回落到当前供应商，不是 400。
- 关闭「参与路由目录」后选择器不列出该卡，仍可用前缀请求。代理面板的「参与路由的配置」可按卡勾选，不必一张张打开编辑页。
- Codex 选择器读的是 `cc-switch-model-catalog.json`：映射表 + toml 模型 + 上次 `/v1/models`。不是实时镜像；刷新后需重启 Codex。
- `{slug}/model` 钉卡，不走跨卡故障转移；Official ChatGPT 同一请求内不跨号重试。
- 账号池：新会话选已知用量最低的号（未知不当 0%）；粘会话；401/403/429 或最热窗口 ≥ 80% 且有更闲号时，**下一轮**换号。

完整说明：[4.6 按供应商选择模型](../user-manual/zh/4-proxy/4.6-model-routing.md)。

### 4. 建一条 Combo（可选）

代理面板 → **Combo 虚拟模型**：

1. id 填 `main`（请求即 `combo/main`）。
2. 策略：`failover`（按行尝试）或 `round-robin`（按权重选第一跳，失败仍继续）。
3. 目标每行一个，例如：

```
kimi/k2
deepseek/deepseek-v4:2
```

```bash
codex -m "combo/main" "解释这段代码"
claude --model "anthropic/combo/main"
```

未知 `combo/foo` → 400；目标都解析不到 → 503。供应商 slug 不要叫 `combo`。见 [4.7](../user-manual/zh/4-proxy/4.7-combo.md)。

### 5. 打开 Sidecar（可选）

代理面板 → **Web Search / Vision Sidecar**。先登录 Claude Pro/Max 或 ChatGPT Official。

- 非官方、非 Anthropic OAuth 卡上的 hosted `web_search` 会改成函数调用，由 sidecar 执行。
- 纯文本模型收到图片时先描述再转发。
- 官方 ChatGPT / Anthropic OAuth 卡不拦截自己的 hosted 搜索。没登录则不改写。

见 [4.8](../user-manual/zh/4-proxy/4.8-sidecar.md)。

### 6. 停止代理

点 **停止代理**。服务关闭，CLI 配置恢复备份。

## 应用级故障转移

和 Combo **不是同一条路径**：

| | 应用级故障转移 | Combo |
|--|----------------|-------|
| 开关 | 代理面板 / 设置里的「自动故障转移」 | Combo 自己的策略 |
| 触发 | 未加前缀的请求失败 | 请求 `combo/{id}` |
| 单位 | 换整张供应商卡 | 换目标列表里的 `slug/model` |
| 钉选 `{slug}/model` | 不走队列 | 不是 Combo 请求则无关 |

应用级：至少两张卡 → 启动代理并接管 → 配队列 → 打开自动故障转移。熔断、健康色见 [4.3](../user-manual/zh/4-proxy/4.3-failover.md)。

## 代理配置

| 参数 | 默认值 | 说明 |
|------|--------|------|
| 监听地址 | 127.0.0.1 | 仅本机；`0.0.0.0` 允许局域网 |
| 监听端口 | 15721 | 与 CLI 指向的端口一致 |
| 最大重试 | 3 | 单跳失败重试 |
| 请求超时 | 120 秒 | 单个请求 |
| 启用日志 | 是 | 高频场景可关以减负 |

改地址/端口须先停代理再保存、再启动。

## 常见问题

**端口被占用**：关掉占用 15721 的程序，或在代理配置里改端口。

**接管后 CLI 不能用**：代理是否在跑、接管开关是否开、供应商 Key/OAuth 是否有效、本机能否访问 `127.0.0.1`。

**`kimi/k2` 没钉到 Kimi 卡**：slug 是否就是 `kimi`；未知前缀会回落当前卡。

**配额**：ChatGPT / Kimi OAuth / Claude OAuth 在认证中心和卡片底部自动显示。托盘不为 Kimi / Anthropic 展示配额。API Key 版 Token Plan 仍要手动开用量查询。

**macOS 打不开（本仓库 Releases）**：构建可能未经公证。Finder 中右键应用 → 打开。

更多问答：[5.2 FAQ](../user-manual/zh/5-faq/5.2-questions.md)。

## 接管会改哪些文件

| 应用 | 配置文件 | 改动 |
|------|----------|------|
| Claude | `~/.claude/settings.json` | `ANTHROPIC_BASE_URL` 指向代理 |
| Codex | `~/.codex/config.toml` | `base_url` 指向代理 `/v1` |
| Gemini | `~/.gemini/.env` | `GOOGLE_GEMINI_BASE_URL` 指向代理 |

原文备份在 CC Switch 数据库，停止代理时恢复。

## 相关文档

- [4.1 代理服务](../user-manual/zh/4-proxy/4.1-service.md)
- [4.2 应用接管](../user-manual/zh/4-proxy/4.2-routing.md)
- [4.6 按供应商选择模型](../user-manual/zh/4-proxy/4.6-model-routing.md)
- [4.7 Combo](../user-manual/zh/4-proxy/4.7-combo.md)
- [4.8 Sidecar](../user-manual/zh/4-proxy/4.8-sidecar.md)
- [1.5 OAuth 认证中心](../user-manual/zh/1-getting-started/1.5-settings.md)
