# rcode (rc)

> Rust 编写的终端 AI Coding 助手，基于 OpenHarness 核心架构精简重写。

## 1. 项目目标

一个单二进制、毫秒启动、类型安全的终端 coding 助手，提供：

- 流式对话交互（SSE streaming）
- 本地 tool 执行（bash / file / grep / glob 等）
- MCP 协议支持（官方 rmcp crate）
- Lua 插件扩展（mlua，沙箱化）
- 单文件配置（YAML）
- 多 LLM Provider（Anthropic / OpenAI / 自定义）

**不做的事**：后台运行、Docker 沙箱、多 Agent 协作、IM 集成、动态插件加载。

## 2. 架构总览

```
┌──────────────────────────────────────┐
│          ratatui TUI                  │
│  输入区 / 流式输出 / tool 结果渲染     │
├──────────────────────────────────────┤
│          CLI 层 (clap)               │
│  /help /model /config /compact ...   │
├──────────────────────────────────────┤
│       Engine (核心对话循环)           │
│  query loop → API client → tool exec │
│  SSE stream → event → TUI render    │
├──────────────┬───────────────────────┤
│  Tool Registry (静态注册)            │
│  bash / file_read / file_write / ... │
│              │                       │
│  ┌───────────┴──────────┐           │
│  │  Lua Plugin Bridge   │           │
│  │  (mlua)              │           │
│  │  .lua 文件定义 tool  │           │
│  └──────────────────────┘           │
├──────────────────────────────────────┤
│  API Clients (reqwest + SSE)        │
│  Anthropic / OpenAI / Custom        │
├──────────────────────────────────────┤
│  Auxiliary Router (辅助模型路由)      │
│  按任务分派 provider/model           │
│  402 自动降级 · provider chain       │
├──────────────────────────────────────┤
│  Infrastructure                     │
│  config(serde) / auth(keyring)      │
│  memory(md files) / mcp(rmcp)       │
│  permissions / cost_tracker         │
└──────────────────────────────────────┘
```

### 2.1 辅助模型路由（参考 Hermes Agent `auxiliary_client.py`）

灵感来自 Hermes Agent 的 `auxiliary_client.py`（3681 行），核心思想：**不同任务用不同的模型，省钱省时间**。

Hermes 的辅助任务分为：

| 任务 | 用途 | 典型模型 |
|------|------|---------|
| `compression` | 长对话历史压缩为摘要 | gemini-3-flash / claude-haiku |
| `vision` | 图像分析（截图/CAPTCHA） | gemini-3.1-flash-lite / glm-5v-turbo |
| `web_extract` | 网页内容提取摘要 | gemini-3-flash |
| `title_generation` | 会话标题生成 | gemini-3-flash |

**rcode 采用简化版设计**：

```
任务 ──→ 读取 auxiliary.{task} 配置
       ├── 有配置 → 直接使用指定 provider/model
       └── 无配置/auto → 走 provider chain 自动探测
            ├── 1. active_provider（主模型 provider）
            ├── 2. OpenRouter（OPENROUTER_API_KEY）
            ├── 3. Custom endpoint（config 中第二个 provider）
            └── 4. None → 跳过辅助任务（不阻塞主流程）
```

**与 Hermes 的差异**：不做 Codex OAuth / Nous Portal / 庞大的 provider alias 表。
rcode 只保留 2 级降级：主 provider → 备选 provider → 跳过。

**402 自动降级**（来自 Hermes 的关键设计）：当辅助 provider 返回 HTTP 402（余额耗尽）时，
自动重试 chain 中的下一个 provider，对用户透明。

## 3. 目录结构

```
rcode/
├── Cargo.toml
├── DESIGN.md
├── config.example.yaml
└── src/
    ├── main.rs                  # 入口，clap 解析
    ├── cli/
    │   ├── mod.rs
    │   └── commands.rs          # 斜杠命令（静态注册）
    ├── config/
    │   ├── mod.rs
    │   ├── settings.rs          # serde 反序列化配置
    │   └── paths.rs             # XDG 规范路径
    ├── api/
    │   ├── mod.rs
    │   ├── client.rs            # trait: SupportsStreamingMessages
    │   ├── anthropic.rs         # Anthropic Messages API
    │   ├── openai.rs            # OpenAI Chat API
    │   ├── sse.rs               # SSE 流解析 (eventsource-stream)
    │   └── types.rs             # Message, ToolUse, ContentBlock 等
    ├── engine/
    │   ├── mod.rs
    │   ├── query.rs             # 核心 tool-aware 对话循环
    │   ├── query_engine.rs      # 对话历史 + 状态管理
    │   ├── messages.rs          # ConversationMessage 定义
    │   ├── stream_events.rs     # StreamEvent enum (给 TUI 消费)
    │   └── cost_tracker.rs      # token 用量追踪
    ├── tools/
    │   ├── mod.rs
    │   ├── registry.rs          # ToolRegistry，静态注册
    │   ├── base.rs              # Tool trait + ToolExecutionContext
    │   ├── bash.rs
    │   ├── file_read.rs
    │   ├── file_write.rs
    │   ├── file_edit.rs         # find-and-replace
    │   ├── glob.rs
    │   ├── grep.rs
    │   ├── web_search.rs
    │   ├── web_fetch.rs
    │   ├── mcp.rs               # MCP tool 代理
    │   ├── config_tool.rs
    │   └── ask_user.rs
    ├── plugin/
    │   ├── mod.rs
    │   ├── bridge.rs            # mlua 桥接层
    │   └── sandbox.rs           # Lua 沙箱配置
    ├── mcp/
    │   ├── mod.rs
    │   └── manager.rs           # rmcp client 管理
    ├── auxiliary/                # 辅助模型路由（参考 Hermes auxiliary_client.py）
    │   ├── mod.rs
    │   ├── router.rs            # 按任务分派 provider/model 的路由器
    │   ├── client.rs            # 辅助 LLM 调用（同步/异步统一接口）
    │   └── compressor.rs        # 对话历史压缩（调用辅助模型生成摘要）
    ├── auth/
    │   ├── mod.rs
    │   └── key_store.rs         # API key 读写 (keyring crate)
    ├── memory/
    │   ├── mod.rs
    │   └── store.rs             # markdown 文件读写
    ├── permissions/
    │   ├── mod.rs
    │   └── checker.rs           # 权限模式: allow/deny/ask
    ├── prompts/
    │   ├── mod.rs
    │   ├── system_prompt.rs     # system prompt 构建
    │   ├── context.rs           # 运行时上下文注入
    │   └── claudemd.rs          # 解析 CLAUDE.md / AGENTS.md
    ├── ui/
    │   ├── mod.rs
    │   ├── app.rs               # ratatui App 状态机
    │   ├── input.rs             # 输入框（支持多行）
    │   ├── output.rs            # markdown 渲染输出
    │   ├── theme.rs             # 颜色主题
    │   └── status_bar.rs        # 底部状态栏
    └── utils/
        ├── mod.rs
        └── shell.rs             # subprocess 封装
```

## 4. 核心数据结构

### 4.1 配置 (config/settings.rs)

```yaml
# config.example.yaml — 存放于 ~/.config/rcode/config.yaml
providers:
  anthropic:
    api_key_env: ANTHROPIC_API_KEY   # 或直接 api_key: "sk-..."
    base_url: https://api.anthropic.com
    default_model: claude-sonnet-4-20250514
  openai:
    api_key_env: OPENAI_API_KEY
    base_url: https://api.openai.com/v1
    default_model: gpt-4o

active_provider: anthropic
model: claude-sonnet-4-20250514

tools:
  bash: true
  file_read: true
  file_write: true
  file_edit: true
  glob: true
  grep: true
  web_search: true
  web_fetch: false

mcp:
  servers:
    example:
      command: ["npx", "-y", "some-mcp-server"]
      # or: url: "http://localhost:3000"

permissions: ask          # allow | deny | ask
max_turns: 8
max_tokens: 4096

theme: default             # default | dark | light

plugins:
  dir: ~/.config/rcode/plugins
  # 自动加载目录下所有 .lua 文件

memory:
  dir: .rcode/memory       # 项目级，相对 cwd

# 辅助模型配置 — 不同任务可用不同 provider/model，省钱省时间
# provider: "auto" | 主 provider 名 | 自定义 OpenAI 兼容端点
# model: 模型名（空字符串 = 用该 provider 的 default_model）
# base_url / api_key: 覆盖 provider 默认值（仅该任务生效）
auxiliary:
  compression:             # 长对话历史压缩为摘要
    provider: auto
    model: ""              # 空 = auto 选择便宜快速的模型
    timeout: 30
  vision:                  # 图像分析
    provider: auto
    model: ""
    timeout: 30
  web_extract:             # 网页内容提取
    provider: auto
    model: ""
    timeout: 60
```

对应的 Rust struct:

```rust
#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    pub providers: HashMap<String, ProviderConfig>,
    pub active_provider: String,
    pub model: String,
    pub tools: ToolsConfig,
    pub mcp: McpConfig,
    pub permissions: PermissionMode,
    pub max_turns: u32,
    pub max_tokens: u32,
    pub theme: String,
    pub plugins: PluginConfig,
    pub memory: MemoryConfig,
    pub auxiliary: AuxiliaryConfig,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ProviderConfig {
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    pub base_url: String,
    pub default_model: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AuxiliaryConfig {
    pub compression: AuxiliaryTaskConfig,
    pub vision: AuxiliaryTaskConfig,
    pub web_extract: AuxiliaryTaskConfig,
}

impl Default for AuxiliaryConfig {
    fn default() -> Self {
        Self {
            compression: AuxiliaryTaskConfig::default_with_timeout(30.0),
            vision: AuxiliaryTaskConfig::default_with_timeout(30.0),
            web_extract: AuxiliaryTaskConfig::default_with_timeout(60.0),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AuxiliaryTaskConfig {
    pub provider: String,       // "auto" | provider name
    pub model: String,          // 空 = 用 provider 的 default_model
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub timeout: f64,
}

impl Default for AuxiliaryTaskConfig {
    fn default() -> Self {
        Self {
            provider: "auto".into(),
            model: String::new(),
            base_url: None,
            api_key: None,
            timeout: 30.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionMode {
    Allow,
    Deny,
    Ask,
}
```

### 4.2 Tool trait (tools/base.rs)

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    /// 工具名称，对应 LLM function calling 的 name
    fn name(&self) -> &str;

    /// JSON Schema 描述（传给 LLM 的 parameters）
    fn schema(&self) -> serde_json::Value;

    /// 执行工具，返回文本结果
    async fn execute(
        &self,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<String, ToolError>;
}

pub struct ToolContext {
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn register(&mut self, tool: Box<dyn Tool>) { ... }
    pub fn schemas(&self) -> Vec<serde_json::Value> { ... }
    pub async fn execute(&self, name: &str, args: Value, ctx: &ToolContext) -> Result<String, ToolError> { ... }
}
```

### 4.3 API Client trait (api/client.rs)

```rust
#[async_trait]
pub trait SupportsStreamingMessages: Send + Sync {
    async fn stream_messages(
        &self,
        model: &str,
        system: &str,
        messages: &[ConversationMessage],
        tools: &[serde_json::Value],
        max_tokens: u32,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>, ApiError>;
}

pub enum StreamEvent {
    TextDelta(String),
    ToolUseStart { id: String, name: String, input_json: String },
    ToolUseDelta { id: String, delta_json: String },
    MessageComplete { stop_reason: StopReason, usage: Usage },
    Error(String),
}
```

### 4.4 核心对话循环 (engine/query.rs)

```
loop:
  1. 组装 messages（system + history + user）
  2. 调用 api_client.stream_messages()
  3. 消费 StreamEvent:
     - TextDelta → 推送到 TUI 渲染
     - ToolUseStart/Delta → 累积 tool input
     - MessageComplete → 记录 usage，break
  4. 如果有 tool_use，执行 tool，将结果追加到 messages，goto 1
  5. 如果 stop_reason == "end_turn"，结束
  6. 如果 turn >= max_turns，结束
```

### 4.5 Lua 插件接口 (plugin/bridge.rs)

```rust
pub struct LuaPlugin {
    name: String,
    lua: Lua,
}

impl LuaPlugin {
    pub fn load(path: &Path) -> Result<Self, PluginError> {
        let lua = Lua::new();
        // 沙箱化：限制 os/io 库
        // 注册 rcode 提供的 safe API
        lua.sandbox()?;
        lua.load_file(path).exec()?;
        Ok(Self { name, lua })
    }
}

// .lua 插件示例:
// rcode.register_tool({
//     name = "docker_ps",
//     description = "List running containers",
//     parameters = { type = "object", properties = {} },
//     execute = function(args)
//         local output = rcode.shell("docker ps --format {{.Names}}")
//         return output
//     end,
// })
```

### 4.6 辅助模型路由 (auxiliary/router.rs + client.rs)

```rust
/// 辅助任务类型
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub enum AuxiliaryTask {
    Compression,    // 对话历史压缩
    Vision,         // 图像分析
    WebExtract,     // 网页内容提取
    TitleGeneration,// 会话标题
}

/// 辅助模型调用结果
pub struct AuxiliaryResult {
    pub content: String,
    pub provider_used: String,
    pub model_used: String,
    pub cached: bool,
}

/// 统一的辅助 LLM 调用接口
/// 设计参考 Hermes 的 call_llm()，但大幅简化
pub async fn call_auxiliary(
    settings: &Settings,
    task: AuxiliaryTask,
    messages: Vec<ChatMessage>,
) -> Result<AuxiliaryResult, AuxiliaryError> {
    // 1. 读取 auxiliary.{task} 配置
    let task_config = settings.auxiliary.get(task);

    // 2. 如果 provider == "auto"，走 provider chain 自动探测
    if task_config.provider == "auto" {
        for candidate in build_provider_chain(settings, task) {
            match try_call(candidate, &messages, &task_config).await {
                Ok(result) => return Ok(result),
                Err(AuxiliaryError::PaymentRequired) => {
                    tracing::warn!(
                        "Auxiliary provider {} returned 402, trying next...",
                        candidate.provider
                    );
                    continue;  // 402 自动降级
                }
                Err(e) => return Err(e),
            }
        }
        return Err(AuxiliaryError::NoProviderAvailable(task));
    }

    // 3. 有明确配置 → 直接使用
    try_call(ResolvedProvider::from_config(&task_config, settings), &messages, &task_config).await
}

/// 构建 provider 探测链
fn build_provider_chain(settings: &Settings, task: AuxiliaryTask) -> Vec<ResolvedProvider> {
    let mut chain = Vec::new();

    // 1. 主 provider（使用其 default_model 或配置中指定的便宜模型）
    if let Some(main) = settings.providers.get(&settings.active_provider) {
        chain.push(ResolvedProvider::from_main_provider(main));
    }

    // 2. 其他已配置的 provider（按配置顺序尝试）
    for (name, provider) in &settings.providers {
        if *name != settings.active_provider {
            chain.push(ResolvedProvider::from_provider(name, provider));
        }
    }

    chain
}
```

## 5. CLI 用法

```bash
# 安装
cargo install --path .

# 直接对话
rc "帮我写一个快速排序"

# 交互模式（TUI）
rc

# 指定 provider / model
rc --provider openai --model gpt-4o

# 读取管道输入
cat main.rs | rc "review 这段代码"

# 斜杠命令（交互模式内）
/help
/model claude-sonnet-4-20250514
/config show
/compact
/cost
/memory
/mcp
/clear
```

## 6. 依赖清单

```toml
[dependencies]
# 异步运行时
tokio = { version = "1", features = ["full"] }

# HTTP + SSE
reqwest = { version = "0.12", features = ["stream", "json"] }
eventsource-stream = "0.2"
futures = "0.3"

# 序列化
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"

# CLI
clap = { version = "4", features = ["derive"] }

# TUI
ratatui = "0.29"
crossterm = "0.28"
syntect = "5"           # 代码语法高亮

# Lua 插件
mlua = { version = "0.10", features = ["luajit52", "vendored"] }

# MCP
rmcp = "0.16"

# 文件系统
notify = "7"            # 文件监视（后续可选）
walkdir = "2"           # glob 实现

# Git
git2 = { version = "0.19", optional = true }

# 密钥存储
keyring = "3"

# 错误处理
anyhow = "1"
thiserror = "2"

# 日志
tracing = "0.1"
tracing-subscriber = "0.3"

# 其他
uuid = { version = "1", features = ["v4"] }
chrono = "0.4"
dirs = "6"              # XDG 目录
regex = "1"
base64 = "0.22"
```

## 7. 实现路线图

### Phase 1：骨架 + 对话（2 周）

- [ ] 项目结构搭建（目录 + mod 声明）
- [ ] `config/` — YAML 配置加载（serde_yaml）
- [ ] `api/` — Anthropic client（reqwest + SSE 流解析）
- [ ] `engine/query.rs` — 基础对话循环（无 tool）
- [ ] `main.rs` — clap CLI，`rc "prompt"` 直接对话能跑通
- [ ] 流式输出到终端（非 TUI，先 println）

**验证**：`rc "用 Rust 写 hello world"` 能流式输出回答。

### Phase 2：Tool 系统（2 周）

- [ ] `tools/base.rs` — Tool trait + ToolRegistry
- [ ] `tools/bash.rs` — subprocess 执行
- [ ] `tools/file_read.rs` / `file_write.rs` / `file_edit.rs`
- [ ] `tools/glob.rs` / `tools/grep.rs`
- [ ] `permissions/checker.rs` — ask 模式下的用户确认
- [ ] engine 集成 tool 循环（tool_use → execute → 追加结果 → 继续）
- [ ] `api/types.rs` — 完善的消息/工具类型定义

**验证**：`rc "在当前目录创建 main.rs 并写入快排代码"` 能成功读写文件。

### Phase 3：ratatui TUI（2 周）

- [ ] `ui/app.rs` — ratatui 应用状态机
- [ ] `ui/input.rs` — 多行输入框
- [ ] `ui/output.rs` — markdown 渲染（syntect 代码高亮）
- [ ] `ui/status_bar.rs` — 模型名 / token 用量 / 权限状态
- [ ] `cli/commands.rs` — 斜杠命令解析 + 执行
- [ ] 流式输出 → TUI 实时渲染

**验证**：`rc` 进入全屏交互模式，对话 + 工具执行 + 流式渲染完整闭环。

### Phase 4：完善基础设施 + 辅助模型（2.5 周）

- [ ] `auth/key_store.rs` — keyring 存储 API key
- [ ] `memory/store.rs` — markdown 记忆读写
- [ ] `prompts/` — system prompt 构建 + CLAUDE.md 解析
- [ ] `mcp/manager.rs` — rmcp server 管理 + tool 代理
- [ ] `api/openai.rs` — OpenAI provider 支持
- [ ] `engine/cost_tracker.rs` — token 用量统计
- [ ] `auxiliary/router.rs` — 按任务分派 provider/model，provider chain 自动探测
- [ ] `auxiliary/client.rs` — `call_llm()` 统一接口，402 自动降级重试
- [ ] `auxiliary/compressor.rs` — 对话历史压缩（参考 Hermes context_compressor.py）
- [ ] `/compact` 命令 — 调用辅助模型压缩历史

### Phase 5：Lua 插件系统（1.5 周）

- [ ] `plugin/bridge.rs` — mlua 加载 + 沙箱化
- [ ] `plugin/sandbox.rs` — 限制 os/io 操作
- [ ] rcode shell / file / env safe API 暴露给 Lua
- [ ] `config` 中 plugins.dir 自动扫描加载
- [ ] 示例插件：`.lua` 文件定义自定义 tool

### Phase 6：打磨发布（1-2 周）

- [ ] 错误处理完善（API 超时 / 网络断开 / tool 失败）
- [ ] 配置热加载（notify 监视 config.yaml）
- [ ] `--help` / `--version` 完善
- [ ] README.md
- [ ] `cargo install` 测试

**总工期估算：11-14 周**（含辅助模型路由，比原计划多 1 周）

## 8. 风险矩阵

| 风险 | 概率 | 影响 | 应对 |
|------|------|------|------|
| SSE 流解析边界 case（Anthropic 格式变更） | 中 | 高 | 用 Python 版抓真实日志写回归测试 |
| Anthropic/OpenAI 无官方 Rust SDK | — | — | 已计划：reqwest 直接封装，不依赖第三方 SDK |
| ratatui 复杂交互难实现（vim mode 等） | 中 | 低 | MVP 不做 vim mode，后续按需加 |
| mlua vendored 编译慢 | 低 | 低 | 可换非 vendored 模式 |
| Tool schema 与 LLM 预期不匹配 | 中 | 高 | serde_json 严格 schema 定义 + 集成测试覆盖 |
| 辅助 provider chain 全部 402 导致压缩/视觉任务失败 | 低 | 中 | graceful degradation：跳过辅助任务，主流程不阻塞，日志警告即可 |
| 不同 provider 的 API 格式差异（max_tokens vs max_completion_tokens 等） | 中 | 中 | 在 ResolvedProvider 中按 provider 归一化参数名（参考 Hermes `_build_call_kwargs`） |

## 9. rtk 集成设计（Token 优化）

### 9.1 背景

[rtk](https://github.com/rtk-ai/rtk)（Rust Token Killer）是一个 CLI 命令代理，通过拦截系统命令输出并压缩，为 LLM 节省 60-90% 的 token 消耗。

| 维度 | 数据 |
|------|------|
| 语言 | 100% Rust（与 rcode 同语言） |
| 规模 | ~59K 行（30K cmds 过滤器 + 8K hooks + 8K core） |
| 许可证 | MIT |
| 性能 | 单线程、零 async、<10ms 启动 |

### 9.2 集成方案选择

| 方案 | 优点 | 缺点 |
|------|------|------|
| **A. 源码集成**（rtk crate 作为依赖） | 调用最方便 | +59K 行代码 + 15+ crate；编译时间暴增；Clap 冲突 |
| B. 库提取（提取 core/filter 作为独立 crate） | 只引入 ~2K 行核心 | rtk 未提供 library crate，需 fork 维护 |
| **C. 外部二进制依赖（推荐）** | 零代码侵入，用户可选装 | 需要用户额外安装 rtk |

**选择方案 C**：rcode 通过 bash tool 自动检测 rtk 并路由命令，无需引入任何额外依赖。

### 9.3 实现方式

在 `tools/bash.rs` 中添加 rtk 自动路由：

```rust
// tools/bash.rs

use std::process::Command;

/// rtk 支持的命令前缀白名单
const RTK_SUPPORTED_PREFIXES: &[&str] = &[
    "git ", "gh ", "cargo ", "npm ", "pnpm ", "npx ",
    "pytest ", "ruff ", "mypy ", "jest ", "vitest ",
    "tsc ", "next ", "ls ", "tree ", "cat ", "grep ",
    "docker ", "kubectl ", "aws ", "go ", "gcc ",
];

impl BashTool {
    /// 检测 rtk 是否可用，且命令在支持列表中
    fn maybe_rtk_route(&self, cmd: &str) -> Option<Vec<String>> {
        // 1. 检查配置是否启用
        if !self.use_rtk {
            return None;
        }
        // 2. 检查 PATH 中是否有 rtk 二进制
        if which::which("rtk").is_err() {
            return None;
        }
        // 3. 命令是否在 rtk 支持列表中
        RTK_SUPPORTED_PREFIXES.iter()
            .find(|prefix| cmd.starts_with(*prefix))
            .map(|_| cmd.split_whitespace().collect())
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let cmd = args["command"].as_str().unwrap();

        // 尝试通过 rtk 代理执行（获得压缩输出）
        if let Some(rtk_parts) = self.maybe_rtk_route(cmd) {
            let output = Command::new("rtk")
                .args(&rtk_parts)
                .output()
                .await?;
            if output.status.success() {
                return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
            }
            // rtk 失败时 fallback 到原始命令
            tracing::debug!("rtk proxy failed, falling back to raw command");
        }

        // 正常执行
        let output = Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .output()
            .await?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}
```

### 9.4 配置

```yaml
# config.yaml 中新增
tools:
  bash:
    use_rtk: auto        # auto | always | never
                          # auto: 检测到 rtk 时自动路由
                          # always: 强制使用 rtk（不可用时报错）
                          # never: 禁用 rtk 路由
```

### 9.5 用户侧使用

```bash
# 安装 rtk（可选）
cargo install --git https://github.com/rtk-ai/rtk

# rcode 自动检测并路由，无需额外配置
rc "运行测试并分析结果"   # 内部执行 "rtk cargo test" 而非 "cargo test"

# 查看节省了多少 token（rcode 可在 /cost 命令中显示）
/cost
```

### 9.6 后续扩展

- 给 rtk 提 PR 添加 `--agent rcode` hook 模板（`rtk init --agent rcode`）
- `/cost` 命令集成 rtk 的 SQLite 统计数据（`rtk gain`）
- 项目级 `.rtk.toml` 配置与 rcode 的 `config.yaml` 联动


