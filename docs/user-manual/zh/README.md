# CC Switch 用户手册

> Claude Code / Claude Desktop / Codex / Gemini CLI / OpenCode / OpenClaw / Hermes 全方位辅助工具

## 目录结构

```
📚 CC Switch 用户手册
│
├── 1. 快速入门
│   ├── 1.1 软件介绍
│   ├── 1.2 安装指南
│   ├── 1.3 界面概览
│   ├── 1.4 快速上手
│   └── 1.5 个性化配置
│
├── 2. 供应商管理
│   ├── 2.1 添加供应商
│   ├── 2.2 切换供应商
│   ├── 2.3 编辑供应商
│   ├── 2.4 排序与复制
│   ├── 2.5 用量查询
│   └── 2.6 Claude Desktop
│
├── 3. 扩展功能
│   ├── 3.1 MCP 服务器管理
│   ├── 3.2 Prompts 提示词管理
│   ├── 3.3 Skills 技能管理
│   ├── 3.4 会话管理器
│   └── 3.5 工作区文件与每日记忆
│
├── 4. 代理与高可用
│   ├── 4.1 代理服务
│   ├── 4.2 应用接管
│   ├── 4.3 故障转移
│   ├── 4.4 用量统计
│   ├── 4.5 模型检查
│   ├── 4.6 按供应商选择模型
│   ├── 4.7 Combo 虚拟模型
│   └── 4.8 Web Search / Vision Sidecar
│
└── 5. 常见问题
    ├── 5.1 配置文件说明
    ├── 5.2 FAQ
    ├── 5.3 深度链接协议
    └── 5.4 环境变量冲突
```

## 文件列表

### 1. 快速入门

| 文件 | 内容 |
|------|------|
| [1.1-introduction.md](./1-getting-started/1.1-introduction.md) | 软件介绍、核心功能、支持平台 |
| [1.2-installation.md](./1-getting-started/1.2-installation.md) | Windows/macOS/Linux 安装指南 |
| [1.3-interface.md](./1-getting-started/1.3-interface.md) | 界面布局、导航栏、供应商卡片说明 |
| [1.4-quickstart.md](./1-getting-started/1.4-quickstart.md) | 5 分钟快速上手教程 |
| [1.5-settings.md](./1-getting-started/1.5-settings.md) | 语言、主题、目录、云同步配置 |

### 2. 供应商管理

| 文件 | 内容 |
|------|------|
| [2.1-add.md](./2-providers/2.1-add.md) | 使用预设、自定义配置、统一供应商 |
| [2.2-switch.md](./2-providers/2.2-switch.md) | 主界面切换、托盘切换、生效方式 |
| [2.3-edit.md](./2-providers/2.3-edit.md) | 编辑配置、修改 API Key、回填机制 |
| [2.4-sort-duplicate.md](./2-providers/2.4-sort-duplicate.md) | 拖拽排序、复制供应商、删除 |
| [2.5-usage-query.md](./2-providers/2.5-usage-query.md) | 用量查询、剩余额度、多套餐显示 |
| [2.6-claude-desktop.md](./2-providers/2.6-claude-desktop.md) | Claude Desktop 第三方供应商、直连与模型映射 |

### 3. 扩展功能

| 文件 | 内容 |
|------|------|
| [3.1-mcp.md](./3-extensions/3.1-mcp.md) | MCP 协议、添加服务器、应用绑定 |
| [3.2-prompts.md](./3-extensions/3.2-prompts.md) | 创建预设、激活切换、智能回填 |
| [3.3-skills.md](./3-extensions/3.3-skills.md) | 发现技能、安装卸载、仓库管理 |
| [3.4-sessions.md](./3-extensions/3.4-sessions.md) | 会话浏览、搜索过滤、恢复与删除 |
| [3.5-workspace.md](./3-extensions/3.5-workspace.md) | OpenClaw 工作区文件、每日记忆 |

### 4. 代理与高可用

| 文件 | 内容 |
|------|------|
| [4.1-service.md](./4-proxy/4.1-service.md) | 启动代理、配置项、运行状态 |
| [4.2-routing.md](./4-proxy/4.2-routing.md) | 应用接管、配置修改、状态指示 |
| [4.3-failover.md](./4-proxy/4.3-failover.md) | 故障转移队列、熔断器、健康状态 |
| [4.4-usage.md](./4-proxy/4.4-usage.md) | 用量统计、趋势图表、定价配置 |
| [4.5-model-test.md](./4-proxy/4.5-model-test.md) | 模型检查、健康检测、延迟测试 |
| [4.6-model-routing.md](./4-proxy/4.6-model-routing.md) | `{slug}/{model}` 钉卡、账号池、路由 slug |
| [4.7-combo.md](./4-proxy/4.7-combo.md) | `combo/{id}` 故障转移与加权轮询 |
| [4.8-sidecar.md](./4-proxy/4.8-sidecar.md) | hosted 搜索与纯文本识图 Sidecar |

### 5. 常见问题

| 文件 | 内容 |
|------|------|
| [5.1-config-files.md](./5-faq/5.1-config-files.md) | CC Switch 存储、CLI 配置文件格式 |
| [5.2-questions.md](./5-faq/5.2-questions.md) | 常见问题解答 |
| [5.3-deeplink.md](./5-faq/5.3-deeplink.md) | 深度链接协议、生成和使用方法 |
| [5.4-env-conflict.md](./5-faq/5.4-env-conflict.md) | 环境变量冲突检测与处理 |

## 快速链接

- **新用户**：从 [1.1 软件介绍](./1-getting-started/1.1-introduction.md) 开始
- **安装问题**：查看 [1.2 安装指南](./1-getting-started/1.2-installation.md)
- **配置供应商**：查看 [2.1 添加供应商](./2-providers/2.1-add.md)
- **使用 Claude Desktop**：查看 [2.6 Claude Desktop](./2-providers/2.6-claude-desktop.md)
- **使用代理**：查看 [4.1 代理服务](./4-proxy/4.1-service.md)
- **按供应商选模型 / Combo / Sidecar**：查看 [4.6](./4-proxy/4.6-model-routing.md)、[4.7](./4-proxy/4.7-combo.md)、[4.8](./4-proxy/4.8-sidecar.md)
- **遇到问题**：查看 [5.2 FAQ](./5-faq/5.2-questions.md)

## 版本信息

- 文档版本：v3.20.0
- 最后更新：2026-08-19
- 适用于 CC Switch v3.20.0+

### v3.20.0 亮点

- **`{slug}/{model}` 钉卡**：接管 Codex / Claude 后，选择器合并各卡模型；带前缀的请求钉到对应卡 — 详见 [4.6 按供应商选择模型](./4-proxy/4.6-model-routing.md)
- **官方族账号池**：多张 ChatGPT Official / Kimi For Coding / Claude Pro/Max 卡时，未加前缀的官方 id 选闲号并粘会话
- **Combo 虚拟模型**：`combo/{id}` 按 failover 或加权轮询跨卡 — 详见 [4.7 Combo](./4-proxy/4.7-combo.md)
- **Kimi / Claude Pro/Max 托管登录**：认证中心设备码或浏览器 PKCE / 导入 CLI，token 不写进卡片 — 详见 [1.5](./1-getting-started/1.5-settings.md)、[2.1](./2-providers/2.1-add.md)
- **Web Search / Vision Sidecar**：非官方模型的 hosted 搜索和纯文本识图，交给已登录的 Claude Pro/Max 或 ChatGPT Official — 详见 [4.8](./4-proxy/4.8-sidecar.md)

### v3.16.0 亮点

- **Codex Chat Completions 路由**：DeepSeek、Kimi、GLM、MiniMax 等仅支持 Chat 协议的供应商可通过 Codex 使用 — 详见 [2.1 添加供应商](./2-providers/2.1-add.md)
- **托管 CLI 工具生命周期**：在设置 / 关于页安装、升级、全部升级并诊断 Claude / Codex / Gemini / OpenCode / OpenClaw / Hermes — 详见 [1.5 个性化配置](./1-getting-started/1.5-settings.md)
- **供应商与模型矩阵刷新**：新增合作方预设，刷新默认模型与计费矩阵，Claude Opus 默认升级到 4.8，适用场景下 GPT 默认升级到 5.5
- **路由支持徽章**：Claude Code / Codex 供应商卡片会标明是否支持 Local Routing，便于选择可代理的供应商
- **Codex OAuth 实时模型发现**：ChatGPT Codex 类供应商按需从 ChatGPT 后端拉取最新模型列表
- **用量看板筛选驱动 Hero**：展示缓存归一化后的真实总 token 与缓存命中率，并跟随日期 / 供应商 / 模型筛选实时更新 — 详见 [4.4 用量统计](./4-proxy/4.4-usage.md)
- **轻量模式**：退出到托盘时销毁主窗口，空闲占用接近零 — 详见 [1.5 个性化配置](./1-getting-started/1.5-settings.md)
- **配额与余额展示**：官方订阅类（Claude/Codex/Gemini/Copilot/Codex OAuth）自动展示剩余额度；Token Plan 和第三方余额通过内置模板一键启用 — 详见 [2.5 用量查询](./2-providers/2.5-usage-query.md)
- **Codex OAuth 反向代理**：用 ChatGPT 账号在 Claude Code 中复用 Codex 服务 — 详见 [2.1 添加供应商](./2-providers/2.1-add.md)
- **托盘按应用分级菜单**：Claude / Codex / Gemini 独立子菜单，标题展示当前供应商与可用用量摘要 — 详见 [2.2 切换供应商](./2-providers/2.2-switch.md)
- **Skills 发现与批量更新**：SHA-256 更新检测、批量更新、skills.sh 公共注册表搜索 — 详见 [3.3 Skills 技能管理](./3-extensions/3.3-skills.md)
- **完整 URL 端点模式**：高级选项支持将 base_url 视作完整上游端点 — 详见 [2.1 添加供应商](./2-providers/2.1-add.md)
- **OpenCode / OpenClaw / Hermes 流式检测覆盖**：Stream Check 面板覆盖 Claude / Codex / Gemini / OpenCode / OpenClaw / Hermes — 详见 [4.5 模型检查](./4-proxy/4.5-model-test.md)

## 贡献

欢迎提交 Issue 或 PR 改进文档：

- [GitHub Issues](https://github.com/farion1231/cc-switch/issues)
- [GitHub Repository](https://github.com/farion1231/cc-switch)
