# zeno (zn)

> **Rust 编写的终端 AI Coding 助手** — 单二进制、毫秒启动、全屏 TUI 交互。

zeno 是一个面向开发者的终端 AI 编程助手，受 [Hermes Agent](https://github.com/NousResearch/Hermes) 架构启发，用 Rust 从零实现。它不是一个 CLI 包装器，而是一个完整的 **tool-aware 对话引擎**，能在终端中直接读写文件、执行命令、搜索网络、调用 MCP 服务，并通过流式 TUI 实时展示结果。

---

## ✨ 核心特性

### ⚡ 毫秒启动 · 单二进制

纯 Rust 实现，编译为单个静态链接二进制文件。无 Node.js 运行时、无 Python 解释器、无 Docker 依赖。启动即用，零等待。

```bash
$ cargo install --path .
$ zeno    # 瞬间进入全屏 TUI
```

### 🖥️ 全屏 TUI 交互

基于 [ratatui](https://github.com/ratatui/ratatui) 构建的终端 UI，提供类 IDE 的交互体验：

- **流式渲染** — LLM 输出逐 token 实时渲染，Markdown + 代码语法高亮（syntect）
- **多行输入** — 支持多行编辑、斜杠命令补全、文件路径补全弹窗（Tab 触发）
- **斜杠命令** — `/help` `/cost` `/compact` `/goal` `/resume` `/search` 等
- **状态栏** — 实时显示模型名、token 用量、权限状态
- **鼠标支持** — 滚轮滚动、Shift+拖拽选择复制

### 🔧 丰富的 Tool 系统

LLM 可直接调用的内置工具，覆盖开发全流程：

| 工具                                         | 能力                                                                    |
| -------------------------------------------- | ----------------------------------------------------------------------- |
| `bash`                                       | 执行 shell 命令，自动检测 [rtk](https://github.com/rtk-ai/rtk) 压缩输出 |
| `read` / `write` / `edit`                    | 文件读写 + **9 策略模糊匹配** find-and-replace                          |
| `glob` / `grep`                              | 文件查找 + 内容搜索                                                     |
| `web_search` / `web_fetch`                   | 网络搜索（SearXNG / Brave / Tavily / DuckDuckGo）+ 网页提取             |
| `ask_user`                                   | 向用户提问获取澄清                                                      |
| `todo`                                       | 任务规划与进度追踪                                                      |
| `delegate_task`                              | **子代理并行执行**独立子任务                                            |
| `memory`                                     | 持久化记忆读写（MEMORY.md / USER.md）                                   |
| `skill_list` / `skill_view` / `skill_manage` | Skill 检索与管理                                                        |

### 🔌 MCP 协议支持

原生支持 [Model Context Protocol](https://modelcontextprotocol.io)，通过 [rmcp](https://crates.io/crates/rmcp) crate 实现：

- **惰性加载** — 配置任意数量的 MCP 服务器，零启动开销，按需连接
- **双传输** — 支持 stdio 子进程和 HTTP Streamable 传输
- **自动发现** — LLM 通过 `mcp_list_servers` → `mcp_list_tools` → `mcp_call_tool` 三步使用
- **自定义 HTTP 头** — 支持 API Key、Bearer Token 等认证

```lua
zn.mcp_servers({
  ["filesystem"] = { command = { "npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp" } },
  ["github"]     = { command = { "npx", "-y", "@modelcontextprotocol/server-github" } },
  ["remote-api"] = { url = "https://api.example.com/mcp", headers = { ["Authorization"] = "Bearer sk-xxx" } },
})
```

### 🧠 智能上下文管理

对话历史过长时自动压缩，永不丢失关键信息：

- **4 级压缩**：Micro（零成本截断旧 tool 结果）→ Collapse（确定性截断）→ Full（辅助 LLM 摘要）→ Reactive（API 返回"prompt too long"时自动触发）
- **Tool Carryover** — 跨轮次工作记忆：追踪已读文件、已修改工件、已完成工作，压缩后 LLM 仍能"记住"关键事实
- **PTL 重试** — 压缩请求本身过长时自动截断最旧轮次并重试（最多 3 次）
- **Auto-continue** — 空响应时通过 `TaskFocusState` 判断目标是否完成，自动注入 continuation prompt（最多 3 次）

### 🎯 辅助模型路由

受 Hermes Agent 启发，**不同任务用不同模型**，省钱省时间：

| 任务               | 用途                     |
| ------------------ | ------------------------ |
| `compression`      | 对话历史压缩为摘要       |
| `vision`           | 图像分析（截图/CAPTCHA） |
| `web_extract`      | 网页内容提取摘要         |
| `title_generation` | 会话标题生成             |
| `session_search`   | 历史会话搜索摘要         |
| `delegation`       | 子代理模型路由           |

**自动降级链**：主 provider → 备选 provider → 跳过（不阻塞主流程）。402/401/403/429/5xx 自动重试下一个 provider，对用户透明。支持 temperature/max_tokens 兼容重试。

### 📚 三层渐进式 Skill 体系

Skill 数量增长时 system prompt 不膨胀：

```
Tier 0: 分类索引 (常驻 system prompt, ~200 token)
  └─ software-development (8 skills) — Coding, debugging...
  └─ research (4 skills) — Academic search...

Tier 1: Skill 摘要 (按需, skill_list tool)
  └─ 某分类下所有 skill 的 name + description

Tier 2: 完整内容 (按需, skill_view tool)
  └─ 完整 SKILL.md 注入对话上下文
```

- 200 个 skill 时 system prompt 仅 ~600 token（全量列表方案 ~5000 token），**节省 88%**
- 磁盘快照缓存实现毫秒级冷启动
- 支持条件过滤（`requires_tools` / `platforms`）和跨分类 tag 检索
- **skill_manage tool** — LLM 可直接创建、编辑、删除 skill，将成功经验沉淀为可复用知识

### ⚙️ 全 Lua 配置（Neovim 风格）

```lua
-- ~/.config/zeno/init.lua
local zn = require 'zeno'

zn.provider("anthropic", {
  api_key = "ANTHROPIC_API_KEY",
  base_url = "https://api.anthropic.com",
  default_model = "claude-sonnet-4-20250514",
})
zn.set_provider("anthropic")

-- 条件配置：按目录动态切换权限
if string.find(zn.cwd(), "/home/guo/Develop/") then
  zn.permissions("allow")
end

-- 模块化：require 加载自定义模块
local my_providers = require 'zeno.providers'

return zn.config()
```

- 支持条件配置（按 OS / 目录 / 环境变量）
- 沙箱化 VM（无 io/os/debug/ffi），`require` 限制在配置目录内
- 向后兼容：`/migrate` 命令自动转换旧 YAML 配置

### 🔐 智能权限系统

三层权限模式，兼顾安全与效率：

| 模式          | 行为                                                                     |
| ------------- | ------------------------------------------------------------------------ |
| `allow`       | 自动放行所有操作                                                         |
| `ask`（默认） | 只读工具自动放行；CWD 内文件操作自动放行；破坏性命令（rm/sudo/dd）需确认 |
| `deny`        | 禁止所有写操作                                                           |

- Git 仓库 CWD 自动信任（可回滚）
- `/tmp` 和 `/var/tmp` 始终自动放行
- 路径规范化防止符号链接逃逸
- 所有决策结构化日志记录，支持审计

### 🪝 Lua Hook 系统

在关键生命周期点注入 Lua 回调，拦截、转换、观察：

```lua
zn.hook("pre_tool_use", function(ctx)
  if ctx.tool_name == "bash" and string.find(ctx.tool_input.command, "rm -rf /") then
    return { block = "Dangerous command detected" }
  end
end)

zn.hook("user_message", function(ctx)
  return { modified_input = "[CWD: " .. ctx.cwd .. "]\n" .. ctx.input }
end)
```

支持 7 个事件：`pre_tool_use` / `post_tool_use` / `session_start` / `session_end` / `pre_llm_call` / `post_llm_call` / `user_message`

Hook 沙箱化（无 io/os.execute），错误不会崩溃 agent。

### 🧩 外部 Memory Provider

通过 Lua 脚本接入任意记忆后端（Mem0、Honcho、自定义 API 等）：

```lua
zn.memory_provider("mem0", { script = "memory_providers/mem0.lua" })
```

Provider 生命周期钩子：`initialize` / `prefetch` / `queue_prefetch` / `sync_turn` / `on_memory_write` / `on_session_end` / `on_session_switch` / `on_pre_compress` / `shutdown`

支持暴露自定义 tool schema 给 LLM 调用。

### 💾 会话持久化

- 输入历史自动保存（最多 2000 条）
- `/resume` 恢复上次会话（对话历史 + 输出）
- `/resume N` 恢复指定编号的会话
- `/search [query]` 按主题搜索历史会话
- 会话标题自动生成（辅助模型）

### 📦 rtk 集成

可选集成 [rtk](https://github.com/rtk-ai/rtk)（Rust Token Killer），自动压缩 bash 命令输出，节省 60-90% token：

```lua
zn.tool("rtk", true)   -- 启用 rtk 路由（默认 true）
```

- 通过 `rtk rewrite` 子命令作为权威判断
- 跳过复合命令（管道/链式）
- rtk 失败时自动 fallback 到原始命令

---

## 🚀 快速开始

```bash
# 1. 安装
cargo install --path .

# 2. 配置
cp config.example.lua ~/.config/zeno/init.lua
# 编辑 init.lua，填入 API Key

# 3. 安装 rtk
cargo install --git https://github.com/rtk-ai/rtk

# 4. 启动
zeno
```

### 斜杠命令

| 命令              | 用途                  |
| ----------------- | --------------------- |
| `/help`           | 显示帮助              |
| `/model [name]`   | 查看/切换模型         |
| `/cost`           | 查看 token 用量       |
| `/compact`        | 手动压缩对话历史      |
| `/clear`          | 清空对话历史          |
| `/goal <text>`    | 设置自动续写目标      |
| `/resume [N]`     | 恢复会话              |
| `/search [query]` | 搜索历史会话          |
| `/tools`          | 列出内置工具          |
| `/mcp`            | 列出 MCP 服务器和工具 |
| `/skills`         | 列出已加载 Skill      |
| `/memory`         | 查看记忆文件          |
| `/hooks`          | 列出已注册 Hook       |
| `/exit`           | 退出                  |

---

## 🏗️ 架构概览

```
┌──────────────────────────────────────┐
│         ratatui TUI                  │
│  输入区 / 流式输出 / Tool 结果渲染    │
│  斜杠命令 dispatch                   │
├──────────────────────────────────────┤
│     Engine (核心对话循环)             │
│  query loop → API client → tool exec │
│  SSE stream → event → TUI render     │
├──────────────┬───────────────────────┤
│  Tool Registry (静态注册)            │
│  bash / read / write / edit / ...    │
│  glob / grep / web_search / ...      │
├──────────────────────────────────────┤
│  API Clients (reqwest + SSE)         │
│  Anthropic / OpenAI / Custom         │
├──────────────────────────────────────┤
│  Auxiliary Router (辅助模型路由)      │
│  按任务分派 provider/model           │
│  402 自动降级 · provider chain       │
├──────────────────────────────────────┤
│  Infrastructure                      │
│ config(mlua) / memory(md files)      │
│  mcp(rmcp) / permissions / hooks     │
│  skills / cost_tracker               │
└──────────────────────────────────────┘
```

---

## 📦 技术栈

| 类别    | 技术                                           |
| ------- | ---------------------------------------------- |
| 语言    | Rust (edition 2024)                            |
| 异步    | tokio + futures                                |
| TUI     | ratatui + crossterm + syntect + pulldown-cmark |
| LLM API | reqwest + eventsource-stream (SSE)             |
| 配置    | mlua (Lua 5.4, vendored)                       |
| MCP     | rmcp (stdio + HTTP Streamable)                 |
| 序列化  | serde + serde_json + serde_yaml                |
| 日志    | tracing + tracing-subscriber (JSON)            |

---

## 🗺️ 路线图

- **Lua 插件系统**（Phase 5）— 通过 `.lua` 文件定义自定义 tool，沙箱化执行
- **配置热加载** — `notify` 监视 `init.lua` 变更自动重载
- **OS keychain 集成** — `keyring` crate 安全存储 API Key

---

## 📄 许可

MIT

