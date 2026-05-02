# zeno (zn)

> Rust 编写的终端 AI Coding 助手，基于 OpenHarness 核心架构精简重写。

## 1. 项目目标

一个单二进制、毫秒启动、类型安全的终端 coding 助手，提供：

- 全屏 TUI 交互（ratatui，流式渲染）
- 斜杠命令（/help /compact /cost /goal 等）
- 本地 tool 执行（bash / file / grep / glob 等）
- MCP 协议支持（官方 rmcp crate）
- Lua 插件扩展（mlua，沙箱化）
- 全 Lua 配置（Neovim 风格）
- 多 LLM Provider（Anthropic / OpenAI / 自定义）

**不做的事**：CLI 非交互模式、后台运行、Docker 沙箱、多 Agent 协作、IM 集成、动态插件加载。

## 2. 架构总览

```
┌──────────────────────────────────────┐
│ ratatui TUI │
│ 输入区 / 流式输出 / tool 结果渲染 │
│ 斜杠命令 dispatch (CommandAction) │
├──────────────────────────────────────┤
│       Engine (核心对话循环)           │
│  query loop → API client → tool exec │
│  SSE stream → event → TUI render    │
├──────────────┬───────────────────────┤
│  Tool Registry (静态注册)            │
│  bash / read / write / ... │
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
│ config(mlua+serde) / auth(keyring) │
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
| `session_search` | 历史会话搜索摘要 | gemini-3-flash |

**zeno 采用简化版设计**：

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
zeno 只保留 2 级降级：主 provider → 备选 provider → 跳过。

**402 自动降级**（来自 Hermes 的关键设计）：当辅助 provider 返回 HTTP 402（余额耗尽）时，
自动重试 chain 中的下一个 provider，对用户透明。

## 3. 目录结构

```
zeno/
├── Cargo.toml
├── DESIGN.md
├── config.example.lua
└── src/
    ├── main.rs # 入口 + 斜杠命令 dispatch
    ├── config/
    │   ├── mod.rs
    │   ├── settings.rs # serde 反序列化配置 struct（Lua/YAML 共用）
    │   ├── loader.rs # Lua VM 初始化 + zeno 模块注册 + serde 转换
    │   ├── paths.rs # XDG 规范路径 + 日志清理
    │   └── model_context.rs # 模型上下文窗口映射
 ├── api/
 │ ├── mod.rs
 │ ├── client.rs # trait: SupportsStreamingMessages
 │ ├── anthropic.rs # Anthropic Messages API
 │ ├── openai.rs # OpenAI Chat API
 │ ├── sse.rs # SSE 流解析 (eventsource-stream)
 │ ├── types.rs # Message, ToolUse, ContentBlock, StreamEvent 等
 │ └── retry.rs # API 重试 + 指数退避 (429/500/502/503/529)
 ├── engine/
 │ ├── mod.rs
 │ ├── query.rs # 核心 tool-aware 对话循环 + auto-continue
 │ ├── query_engine.rs # 对话历史 + 状态管理 + HookExecutor
 │ ├── messages.rs # ConversationEntry / ConversationHistory
 │ ├── stream_events.rs # re-export: StreamEvent → api/types
 │ ├── cost_tracker.rs # token 用量追踪
 │ ├── compact.rs # 四级压缩: micro/collapse/full/reactive + PTL retry
 │ ├── carryover.rs # Tool Carryover 工作记忆 + TaskFocusState
 │ └── tui_events.rs # UiEvent enum (TUI 事件流)
 ├── tools/
 │ ├── mod.rs
 │ ├── base.rs # Tool trait + ToolRegistry + ToolContext
 │ ├── bash.rs # subprocess + rtk 自动路由
 │ ├── read.rs
 │ ├── write.rs
 │ ├── edit.rs # 9 策略模糊匹配 find-and-replace
 │ ├── glob.rs
 │ ├── grep.rs
 │ ├── web_search.rs
 │ ├── web_fetch.rs # html2text 提取网页
 │ ├── mcp.rs # MCP tool 代理（stub，待 rmcp 接入）
 │ ├── config_tool.rs
 │ ├── ask_user.rs
 │ ├── skill_list.rs # 按分类/tag 列出 skill 摘要（Tier 1）
 │ └── skill_view.rs # 加载完整 skill 内容（Tier 2）
 ├── plugin/
 │ ├── mod.rs
 │ ├── bridge.rs # mlua 桥接层（Phase 5）
 │ └── sandbox.rs # Lua 沙箱配置（Phase 5）
 ├── mcp/
 │ ├── mod.rs
 │ └── manager.rs # rmcp client 管理（stub，待接入）
 ├── auxiliary/ # 辅助模型路由（参考 Hermes auxiliary_client.py）
 │ ├── mod.rs
 │ ├── router.rs # 任务枚举 + provider 解析 + 别名规范化
 │ ├── client.rs # call_auxiliary() + 多种自动重试 + response 验证
 │ ├── cache.rs # HTTP 客户端缓存（避免每次重建连接）
 │ ├── compressor.rs # 对话历史压缩 + 标题生成
 │ ├── vision.rs # Vision 独立路由 + 图片编码 + 混合消息
 │ └── web_extract.rs # 网页内容提取摘要
 ├── auth/
 │ ├── mod.rs
 │ └── key_store.rs # API key 解析 (env/config) — keyring 待集成
 ├── memory/
 │ ├── mod.rs
 │ └── store.rs # markdown 文件读写
 ├── permissions/
 │ ├── mod.rs
 │ └── checker.rs # 权限模式: allow/deny/ask
 ├── skills/
 │ ├── mod.rs
 │ ├── types.rs # SkillDefinition + CategoryInfo 数据结构
 │ ├── loader.rs # 分类目录扫描 + frontmatter 解析
 │ ├── registry.rs # SkillRegistry（分类索引 + 按需检索）
 │ └── index_cache.rs # 磁盘快照缓存（快速冷启动）
 ├── prompts/
 │ ├── mod.rs
 │ ├── system_prompt.rs # system prompt 构建（含 Tier 0 分类索引 + 智能预加载）
 │ ├── context.rs # 运行时上下文注入
 │ └── claudemd.rs # 解析 CLAUDE.md / AGENTS.md
 ├── hooks/
 │ ├── mod.rs
 │ ├── types.rs # HookEvent (PreToolUse/PostToolUse/Notification)
 │ └── executor.rs # HookExecutor (异步回调注册)
 ├── ui/
 │ ├── mod.rs
 │ ├── app.rs # ratatui App 状态机
 │ ├── input.rs # 输入框（多行 + 斜杠命令 + 路径补全弹窗）
 │ ├── output.rs # 输出区管理
 │ ├── markdown.rs # pulldown-cmark + syntect 自定义渲染器
 │ ├── theme.rs # 颜色主题
 │ └── status_bar.rs # 底部状态栏
 └── utils/
 └── mod.rs # 工具函数（shell 封装已在 tools/bash.rs 内）
```

## 4. 核心数据结构

### 4.1 配置 (config/settings.rs + config/loader.rs)

**首选：Lua 配置**（Neovim 风格，参考 WezTerm）：

```lua
-- ~/.config/zeno/init.lua（完整示例见 config.example.lua）
local zn = require 'zeno'

zn.provider("anthropic", {
  api_key_env = "ANTHROPIC_API_KEY",
  base_url = "https://api.anthropic.com",
  default_model = "claude-sonnet-4-20250514",
})
zn.set_provider("anthropic")

zn.tool("web_fetch", false)
zn.bash_env({ NODE_ENV = "development" })
zn.mcp_server("context7", { command = { "npx", "-y", "@upstreamapi/context7" } })
zn.auxiliary("vision", { provider = "auto", model = "", timeout = 30 })
zn.web_search({ provider = "brave", api_key_env = "BRAVE_API_KEY" })
zn.model_context({ ["claude"] = 200000, ["gpt-4"] = 128000 })
zn.permissions("ask")
zn.max_turns(200)
zn.max_tokens(0)  -- 0 = auto

-- 条件配置
if string.find(zn.cwd(), "/home/guo/Develop/") then
  zn.permissions("allow")
end

return zn.config()
```

**配置文件布局**：

```
~/.config/zeno/
├── init.lua          # 主配置（入口）
├── lua/              # 用户自定义模块（require 搜索路径）
│   └── zeno/
│       └── providers.lua  # 示例：provider 辅助模块
├── plugins/ # Lua 插件（Phase 5，独立于配置）
└── config.yaml # 旧 YAML 格式（仅 /migrate 迁移时读取，运行时不再使用）

**加载逻辑**：`init.lua` 存在 → 用 Lua 加载；不存在 → 使用默认值并提示用户创建 `init.lua`。

**旧 YAML 格式**（`/migrate` 命令可自动转换为 `init.lua`，运行时不读 YAML）。

对应的 Rust struct（精简展示核心字段，实际以 `config/settings.rs` 为准）：

```rust
pub struct Settings {
    pub providers: HashMap<String, ProviderConfig>,
    pub active_provider: String,
    pub model: String,
    pub tools: ToolsConfig,
    pub mcp: McpConfig,
    pub permissions: PermissionMode,
    pub max_turns: u32,
    pub max_tokens: u32,
    pub model_contexts: HashMap<String, u32>,  // 模型名前缀 → 上下文窗口大小
    pub theme: String,
    pub plugins: PluginConfig,
    pub memory: MemoryConfig,
    pub auxiliary: AuxiliaryConfig,
    pub llm: LlmConfig,              // max_retries 等 LLM 相关配置
    pub log_retention_days: u64,     // 日志保留天数
}

pub struct ProviderConfig {
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    pub base_url: String,
    pub default_model: String,
    pub max_output_tokens: Option<u32>,  // 可选：覆盖顶层 max_tokens
}

pub struct AuxiliaryConfig {
    pub compression: AuxiliaryTaskConfig,  // timeout 30s
    pub vision: AuxiliaryTaskConfig,       // timeout 30s
    pub web_extract: AuxiliaryTaskConfig,  // timeout 60s
}

pub struct AuxiliaryTaskConfig {
    pub provider: String,       // "auto" | provider name
    pub model: String,          // 空 = 用 provider 的 default_model
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub timeout: f64,
}

pub struct LlmConfig {
    pub max_retries: u32,  // 默认 3
}

pub enum PermissionMode { Allow, Deny, Ask }  // 默认 Ask
```

### 4.2 Tool trait (tools/base.rs)

每个 tool 实现 `Tool` trait，向 LLM 暴露 JSON Schema，接收参数执行并返回文本结果。

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> serde_json::Value;
    async fn execute(&self, arguments: serde_json::Value, ctx: &ToolContext) -> Result<String, ToolError>;
    fn is_read_only(&self, input: &Value) -> bool { false }   // 权限系统用：判断是否无副作用
    fn validate_input(&self, arguments: &Value) -> Result<(), ToolError> { ... }  // 输入校验
}

/// ToolContext：每次 tool 调用传入的上下文
pub struct ToolContext {
    pub cwd: PathBuf,
    pub ask_sender: Option<mpsc::UnboundedSender<UiEvent>>,  // ask_user 工具的 TUI 通信通道
}
```

`ToolRegistry` 提供 `register()` / `schemas()` / `execute()` 三个核心方法。

### 4.3 API Client trait (api/client.rs)

所有 provider（Anthropic / OpenAI / Custom）实现此 trait，返回 SSE 事件流。

```rust
#[async_trait]
pub trait SupportsStreamingMessages: Send + Sync {
    async fn stream_messages(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],          // api/types.rs 中定义的 Message
        tools: &[serde_json::Value],
        max_tokens: Option<u32>,       // None = 由 provider 自行决定默认值
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>>, ApiError>;
}

/// SSE 流事件
pub enum StreamEvent {
    TextDelta(String),
    ToolUseStart { id: String, name: String, input_json: Option<String> },
    ToolUseDelta { id: String, delta_json: String },
    UsageUpdate { input_tokens: u64, output_tokens: u64 },  // Anthropic message_start 中的提前 token 统计
    MessageComplete { stop_reason: StopReason, usage: Usage },
    Error(String),
}
```

### 4.4 核心对话循环 (engine/query.rs)

```
loop:
 1. 组装 messages（system + history + user + carryover context）
 2. Auto-compact: token 估算超阈值 → 触发压缩（micro/collapse/full）
 3. 调用 api_client.stream_messages()（含重试+指数退避）
 4. 消费 StreamEvent:
 - TextDelta → 推送到 TUI 渲染
 - ToolUseStart/Delta → 累积 tool input，发 ToolStart 事件
 - MessageComplete → 记录 usage，break
 5. 如果有 tool_use，执行 tool（支持并发），将结果追加到 messages
 - 执行后更新 carryover（记录 read/written/done）
 6. 如果 stop_reason == "end_turn"，结束
 7. 如果 stop_reason == "tool_use"，goto 1
 8. 如果空响应/无 tool_use → 检查 TaskFocusState:
 - 有活跃 goal 且未完成 → 注入 continuation prompt，goto 1
 - 无 goal → 正常结束
 9. 如果 turn >= max_turns，结束
```

**关键改进**（相比初版设计）：
- **Auto-continue**：空响应时不直接 break，通过 `TaskFocusState` 判断目标是否完成
- **Carryover 注入**：每轮将 `carryover.to_context_text()` 追加到 user message
- **PTL retry**：API 返回 "prompt too long" 时自动触发 reactive compact 后重试
- **并发 tool 执行**：单 tool 顺序执行，多 tool 并发执行

### 4.5 Lua 插件接口 (plugin/bridge.rs + plugin/sandbox.rs)

Lua 插件系统（Phase 5，尚未实现）。设计目标：

- `plugin/bridge.rs`：mlua 加载 `.lua` 文件，注册 `zeno.register_tool()` API，插件可定义自定义 tool
- `plugin/sandbox.rs`：沙箱化配置，限制 os/io 操作（配置 VM 与插件 VM 独立）

插件通过 `config.plugins.dir` 目录自动扫描加载。

### 4.6 辅助模型路由 (auxiliary/)

六个子模块职责：

- **router.rs**：定义 `AuxiliaryTask` 枚举（Compression/Vision/WebExtract/TitleGeneration/SessionSearch），
  提供 `resolve_provider()` 解析 provider/model，`build_provider_chain()` 构建降级链，
  `normalize_provider()` provider 别名规范化（"claude"→"anthropic"、"codex"→"openai-codex"等），
  `model_omits_temperature()` 判断模型是否不应发送 temperature 参数（如 Kimi）。
  TitleGeneration 和 SessionSearch 有独立配置。
- **client.rs**：`call_auxiliary()` 统一调用接口，OpenAI 兼容 API 非流式调用。
  支持多种自动重试：402(支付) / 401/403(认证) / 429/5xx(连接) 自动降级到下一个 provider；
  temperature 不被支持时自动去掉重试；max_tokens 不被支持时改用 max_completion_tokens 重试。
  `validate_response()` 检查返回结构合法性。返回 `AuxiliaryResult`。
- **cache.rs**：HTTP 客户端缓存（按 provider+base_url 缓存 `reqwest::Client`），
  避免每次辅助调用重建连接池。支持 stale 过期清理和 provider 级别驱逐。
- **compressor.rs**：`compress_history()` 调用 Compression 辅助模型生成对话摘要；
  `generate_title()` 调用 TitleGeneration 生成会话标题（接受 user+assistant 交换）。
- **vision.rs**：Vision 任务独立路由（过滤到支持多模态的 provider），
  图片 base64 data URL 编码，`VisionMessage` 支持文本+图片混合内容，
  `ImageInputMode` 决定图片以 native 还是 text 模式传递。
- **web_extract.rs**：`extract_web_content()` 调用 WebExtract 辅助模型提取网页摘要，
  `extract_html()` 先将 HTML 转为纯文本再提取。

**路由逻辑**：provider == "auto" 时走 provider chain（active_provider → 其他已配置 provider），
每个候选遇到 402/401/403/429/5xx 时自动跳到下一个。非 "auto" 时直接使用指定 provider。
Vision 任务的 auto 模式额外过滤到已知支持多模态的 provider 列表。

### 4.7 Skill 数据结构 (skills/types.rs)

```rust
/// Skill 分类信息 — 用于 Tier 0 分类索引
#[derive(Debug, Clone)]
pub struct CategoryInfo {
    /// 分类描述（来自 DESCRIPTION.md）
    pub description: String,
    /// 该分类下所有 skill 名称
    pub skill_names: Vec<String>,
}

/// 加载后的 skill 定义
#[derive(Debug, Clone)]
pub struct SkillDefinition {
    /// 唯一 skill 名称（frontmatter name 或目录名）
    pub name: String,
    /// 短描述（≤120 chars，截断到第一句）
    pub description: String,
    /// 完整 SKILL.md 内容
    pub content: String,
    /// 来源："bundled" | "user" | "project"
    pub source: String,
    /// SKILL.md 绝对路径
    pub path: Option<String>,
    // --- 分类与检索 ---
    /// 所属分类（从目录层级推导），如 "software-development"
    pub category: String,
    /// 标签（frontmatter metadata.tags）
    pub tags: Vec<String>,
    /// 关联 skill（frontmatter metadata.related_skills）
    pub related_skills: Vec<String>,
    // --- 条件过滤 ---
    /// 声明需要的 tool 名称，仅当这些 tool 全部可用时才在 Tier 1 展露
    pub requires_tools: Vec<String>,
    /// 平台限制（空 = 全平台），如 ["linux", "macos"]
    pub platforms: Vec<String>,
}

/// Skill 注册表 — 支持分类索引 + 按需检索
pub struct SkillRegistry {
    skills: Vec<SkillDefinition>,
    /// 分类 → CategoryInfo 索引
    categories: IndexMap<String, CategoryInfo>,
}

impl SkillRegistry {
    pub fn register(&mut self, skill: SkillDefinition) { ... }
    pub fn get(&self, name: &str) -> Option<&SkillDefinition> { ... }
    pub fn get_insensitive(&self, name: &str) -> Option<&SkillDefinition> { ... }

    /// Tier 0: 返回分类索引（用于 system prompt 注入）
    pub fn categories(&self) -> &IndexMap<String, CategoryInfo> { ... }

    /// Tier 1: 按分类列出 skill 摘要
    pub fn list_by_category(&self, category: &str) -> Vec<&SkillDefinition> { ... }

    /// Tier 1: 按 tag 检索 skill
    pub fn list_by_tag(&self, tag: &str) -> Vec<&SkillDefinition> { ... }

    /// 条件过滤：返回 requires_tools 与 available_tools 匹配的 skill
    pub fn filter_by_tools(&self, available_tools: &HashSet<String>) -> Vec<&SkillDefinition> { ... }

    /// 平台过滤：排除与当前 OS 不兼容的 skill
    pub fn filter_by_platform(&self, os: &str) -> Vec<&SkillDefinition> { ... }
}
```

#### Skill 目录结构

```text
skills/
├── DESCRIPTION.md              # 根级分类总览（可选）
├── software-development/
│   ├── DESCRIPTION.md          # 分类描述: "Coding, debugging, testing"
│   ├── coding-principles/
│   │   └── SKILL.md
│   ├── tdd/
│   │   └── SKILL.md
│   └── systematic-debugging/
│       └── SKILL.md
├── research/
│   ├── DESCRIPTION.md
│   ├── arxiv/
│   │   └── SKILL.md
│   └── llm-wiki/
│       └── SKILL.md
└── devops/
    ├── DESCRIPTION.md
    └── webhook-subscriptions/
        └── SKILL.md
```

SKILL.md frontmatter 格式（agentskills.io 兼容）：

```yaml
---
name: coding-principles
description: Behavioral guidelines to reduce common LLM coding mistakes.
version: 1.0.0
platforms: []            # 空 = 全平台
metadata:
  hermes:
    tags: [coding, best-practices, simplicity, tdd]
    related_skills: [test-driven-development, writing-plans]
    requires_tools: []    # 空 = 无条件，始终展露
---
```

### 4.8 输入补全弹窗 (ui/input.rs)

输入框支持两种补全模式，优先级从高到低：

**1. 斜杠命令补全（Command Completion）**

- 触发条件：输入以 `/` 开头且光标在行尾
- 匹配方式：在 `COMMANDS` 常量中做前缀匹配
- 确认行为：替换整行文本

**2. 路径补全（Path Completion）**

- 触发条件：光标前的 token 包含 `/`、以 `.` 开头、或以 `~` 开头
- 匹配方式：用 `std::fs::read_dir` 扫描文件系统
- 特性：
  - 支持 `~` 展开（使用 `dirs::home_dir()`）
  - 目录自动追加 `/` 后缀
  - 隐藏文件（`.` 开头）仅在前缀也以 `.` 开头时显示
  - 弹窗宽度根据最长匹配项动态调整
- 确认行为：仅替换路径 token（不替换整行），光标移动到替换文本末尾

**数据结构：**

```rust
enum CompletionType {
    Command,
    Path { prefix: String, start: usize },
}

struct CompletionPopup {
    matches: Vec<String>,
    selected: usize,
    scroll: usize,
    completion_type: CompletionType,
}
```

**交互方式：**

| 按键 | 行为 |
|------|------|
| Tab | 打开补全弹窗 / 向下循环选择 |
| Shift+Tab | 打开补全弹窗 / 向上循环选择 |
| ↑/↓ | 在弹窗中导航 |
| Enter | 确认选择并提交 |
| Esc | 关闭弹窗 |
| 继续输入 | 关闭弹窗，继续编辑 |

**优先级规则：** 输入以 `/` 开头时优先匹配斜杠命令；无命令匹配时才尝试路径补全。

## 5. 用法

```bash
# 安装
cargo install --path .

# 启动 TUI
zn

# TUI 内斜杠命令
/help
/model
/config
/compact
/cost
/clear
/memory
/mcp
/tools
/goal <text>
/goal clear
/goal pause
/goal resume
/exit
```

## 6. 依赖清单

以 `Cargo.toml` 为准，核心依赖分组：

| 类别 | 依赖 | 用途 |
|------|------|------|
| 异步 | tokio, tokio-util, futures | 异步运行时 + stream 工具 |
| HTTP | reqwest, eventsource-stream | API 请求 + SSE 流 |
| 序列化 | serde, serde_json, serde_yaml | 数据序列化（serde_yaml 用于 skill frontmatter） |
| Lua | mlua (lua54, vendored, serialize, send) | 配置加载 + 插件 |
| TUI | ratatui, crossterm, syntect, pulldown-cmark, unicode-width | 全屏终端 UI + markdown 渲染 + CJK 宽度 |
| Tool | walkdir, regex, which, urlencoding, html2text, base64 | 各 tool 实现 |
| 错误 | anyhow, thiserror | 错误处理 |
| 日志 | tracing, tracing-subscriber, tracing-appender | 结构化日志 + 文件输出 |
| 其他 | dirs, hostname, rand, async-trait, indexmap | XDG 目录 / 主机名 / 随机 / trait / 有序 map |

**待引入**（未实现 Phase）：rmcp（MCP）、keyring（OS keychain）、notify（热重载）、git2（可选 Git 集成）。

## 7. 实现路线图

### Phase 1：骨架 + 对话（2 周） ✅

- [x] 项目结构搭建（目录 + mod 声明）
- [x] `config/` — Lua 配置加载（mlua + serde，旧 YAML 已移除）
- [x] `config/loader.rs` — Lua 配置加载（mlua + serde）
- [x] `api/` — Anthropic client（reqwest + SSE 流解析）
- [x] `engine/query.rs` — 基础对话循环（无 tool）
- [x] `main.rs` — TUI 入口，斜杠命令 dispatch
- [x] 流式输出到终端（非 TUI，先 println）

**验证**：`zn` 启动 TUI，输入对话能流式输出回答。

### Phase 2：Tool 系统（2 周） ✅

- [x] `tools/base.rs` — Tool trait + ToolRegistry
- [x] `tools/bash.rs` — subprocess 执行（含 rtk 路由）
- [x] `tools/read.rs` / `write.rs` / `edit.rs`（9 策略模糊匹配）
- [x] `tools/glob.rs` / `tools/grep.rs` / `tools/web_search.rs` / `tools/web_fetch.rs`
- [x] `permissions/checker.rs` — ask 模式下的用户确认
- [x] engine 集成 tool 循环（tool_use → execute → 追加结果 → 继续）
- [x] `api/types.rs` — 完善的消息/工具类型定义

**验证**：TUI 内输入"在当前目录创建 main.rs 并写入快排代码"能成功读写文件。

### Phase 3：ratatui TUI（2 周） ✅

- [x] `ui/app.rs` — ratatui 应用状态机
- [x] `ui/input.rs` — 多行输入框（含斜杠命令 + 路径补全弹窗）
- [x] `ui/output.rs` — markdown 渲染（pulldown-cmark + syntect 代码高亮）
- [x] `ui/status_bar.rs` — 模型名 / token 用量 / 权限状态
- [x] `cli/commands.rs` — 斜杠命令 dispatch（已内联到 main.rs，CommandAction 模式）
- [x] 流式输出 → TUI 实时渲染

**验证**：`zn` 进入全屏交互模式，对话 + 工具执行 + 流式渲染完整闭环。

### Phase 4：完善基础设施 + 辅助模型（2.5 周） 🔶

- [x] `auth/key_store.rs` — API key 解析（env + config）— keyring 集成待做
- [x] `memory/store.rs` — markdown 记忆读写
- [x] `prompts/` — system prompt 构建 + CLAUDE.md 解析
- [ ] `mcp/manager.rs` — rmcp server 管理 + tool 代理（框架在，rmcp 未接入）
- [x] `api/openai.rs` — OpenAI provider 支持
- [x] `engine/cost_tracker.rs` — token 用量统计
- [x] `auxiliary/router.rs` — 按任务分派 provider/model，provider chain 自动探测，别名规范化，temperature 模型适配
- [x] `auxiliary/client.rs` — `call_auxiliary()` 统一接口，402/401/403/429/5xx 自动降级重试，temperature/max_tokens 兼容重试，response 验证
- [x] `auxiliary/cache.rs` — HTTP 客户端缓存，stale 过期清理，provider 级别驱逐
- [x] `auxiliary/compressor.rs` — 对话历史压缩 + 标题生成（参考 Hermes context_compressor.py + title_generator.py）
- [x] `auxiliary/vision.rs` — Vision 独立路由（过滤到多模态 provider），图片 base64 编码，混合消息支持
- [x] `auxiliary/web_extract.rs` — 网页内容提取摘要（参考 Hermes web_extract task）
- [x] `/compact` 命令 — 调用辅助模型压缩历史
- [x] `skills/index_cache.rs` — 磁盘快照缓存（冷启动加速）

### Phase 5：Lua 插件系统（1.5 周）

- [ ] `plugin/bridge.rs` — mlua 加载 + 沙箱化
- [ ] `plugin/sandbox.rs` — 限制 os/io 操作
- [ ] zeno shell / file / env safe API 暴露给 Lua
- [ ] `config` 中 plugins.dir 自动扫描加载
- [ ] 示例插件：`.lua` 文件定义自定义 tool

### Phase 6：打磨发布（1-2 周）

- [ ] 错误处理完善（API 超时 / 网络断开 / tool 失败）
- [ ] 配置热加载（notify 监视 init.lua）
- [ ] `--version` 输出（简单的 version 常量）
- [ ] README.md
- [ ] `cargo install` 测试

**总工期估算：11-14 周**（含辅助模型路由，比原计划多 1 周）

## 8. 风险矩阵

| 风险 | 概率 | 影响 | 应对 |
|------|------|------|------|
| SSE 流解析边界 case（Anthropic 格式变更） | 中 | 高 | 用 Python 版抓真实日志写回归测试 |
| Anthropic/OpenAI 无官方 Rust SDK | — | — | 已实现：reqwest 直接封装，含重试+指数退避 |
| ratatui 复杂交互难实现（vim mode 等） | 中 | 低 | MVP 不做 vim mode，后续按需加 |
| mlua vendored 编译慢 | 低 | 低 | 可换非 vendored 模式 |
| Tool schema 与 LLM 预期不匹配 | 中 | 高 | serde_json 严格 schema 定义 + 集成测试覆盖 |
| 辅助 provider chain 全部 402 导致压缩/视觉任务失败 | 低 | 中 | graceful degradation：跳过辅助任务，主流程不阻塞 |
| 不同 provider API 格式差异（max_tokens vs max_completion_tokens 等） | 中 | 中 | config/model_context.rs 按模型名映射上下文窗口 |
| Skill 分类索引导致 LLM 检索精度下降 | 中 | 中 | 智能预加载 + tag 跨分类检索兜底（详见 §10） |
| rmcp crate API 不稳定（0.x 版本） | 中 | 中 | 隔离在 mcp/manager.rs，不扩散到核心代码 |
| CJK token 估算偏差导致 compact 过早/过晚 | 低 | 中 | 使用 CJK-aware 估算（ASCII ÷4, CJK ÷2） |
| Auto-continue continuation prompt 被 LLM 忽略 | 中 | 低 | 最多重试 3 次，超限后正常结束 |

## 9. rtk 集成设计（Token 优化）

### 9.1 背景

[rtk](https://github.com/rtk-ai/rtk)（Rust Token Killer）是一个 CLI 命令代理，通过拦截系统命令输出并压缩，为 LLM 节省 60-90% 的 token 消耗。

| 维度 | 数据 |
|------|------|
| 语言 | 100% Rust（与 zeno 同语言） |
| 规模 | ~59K 行（30K cmds 过滤器 + 8K hooks + 8K core） |
| 许可证 | MIT |
| 性能 | 单线程、零 async、<10ms 启动 |

### 9.2 集成方案选择

| 方案 | 优点 | 缺点 |
|------|------|------|
| **A. 源码集成**（rtk crate 作为依赖） | 调用最方便 | +59K 行代码 + 15+ crate；编译时间暴增；Clap 冲突 |
| B. 库提取（提取 core/filter 作为独立 crate） | 只引入 ~2K 行核心 | rtk 未提供 library crate，需 fork 维护 |
| **C. 外部二进制依赖（推荐）** | 零代码侵入，用户可选装 | 需要用户额外安装 rtk |

**选择方案 C**：zeno 通过 bash tool 自动检测 rtk 并路由命令，无需引入任何额外依赖。

### 9.3 实现方式

`tools/bash.rs` 中的 rtk 路由逻辑（`maybe_rtk_route()`）：

1. 检查 `use_rtk` 配置开关和 PATH 中的 rtk 二进制
2. 跳过复合命令（含 `|`、`&&`、`||`）
3. 调用 `rtk rewrite <cmd>` 作为权威判断——返回 exit code 3 表示可重写
4. 若 rtk rewrite 成功，执行重写后的命令；否则 fallback 到原始命令

代码中保留了一份命令前缀白名单（`RTK_SUPPORTED_PREFIXES`），用于快速预筛，
实际路由以 `rtk rewrite` 子命令的返回结果为准。

### 9.4 配置

```lua
-- init.lua 中
zn.tool("rtk", true)   -- 启用 rtk 路由（默认 true）
zn.tool("rtk", false)  -- 禁用
```

配置存储在 `ToolsConfig.use_rtk: bool` 中。

### 9.5 用户侧使用

```bash
# 安装 rtk（可选）
cargo install --git https://github.com/rtk-ai/rtk

# zeno 自动检测并路由，无需额外配置
zn "运行测试并分析结果"   # 内部执行 "rtk cargo test" 而非 "cargo test"

# 查看节省了多少 token（zeno 可在 /cost 命令中显示）
/cost
```

### 9.6 后续扩展

- 给 rtk 提 PR 添加 `--agent zeno` hook 模板（`rtk init --agent zeno`）
- `/cost` 命令集成 rtk 的 SQLite 统计数据（`rtk gain`）
- 项目级 `.rtk.toml` 配置与 zeno 的 `init.lua` 联动

## 10. 三层渐进式 Skill 体系

### 10.1 问题

当前 zeno 的 skill 注入方式（`skills_block()`）将**所有 skill 的 name + description 全量塞入 system prompt**。
当 skill 数量增长到 60+ 时，skill 列表占用 ~1500 token；200+ 时膨胀到 ~5000 token，且 LLM 需从冗长列表中
猜测应该加载哪个 skill，检索精度也下降。

Hermes Agent 的做法（分类目录 + 条件过滤 + 磁盘快照）有改进，但最终仍把所有匹配 skill 的摘要全量注入 system
prompt，并未解决 skill 规模增长时 prompt 膨胀的根本问题。

### 10.2 设计目标

- **skill 数量增长时，system prompt 中 skill 部分基本不增长**（只与分类数有关）
- **检索精度**：LLM 通过"分类→skill"两步缩小范围，而非从全量列表猜名字
- **渐进式加载**：system prompt 只放最精简的索引，详细内容按需加载
- **兼容 agentskills.io 格式**：frontmatter 结构与 Hermes/Alex 目录兼容

### 10.3 三层架构

```text
┌─────────────────────────────────────────────────────────────┐
│ Tier 0: 分类索引 (常驻 system prompt, 极小)               │
│ 只有分类名 + 分类描述 + skill 数量                        │
│ ~8 行 × ~30 chars = ~200 token                             │
│                                                             │
│   - software-development (8 skills) — Coding, debugging... │
│   - research (4 skills) — Academic search, wiki...         │
│   - devops (3 skills) — Infrastructure, deploy...          │
│   - wayland (6 skills) — Compositor debugging...           │
│   ...                                                       │
├─────────────────────────────────────────────────────────────┤
│ Tier 1: Skill 摘要 (按需, 通过 skill_list tool 获取)      │
│ 某分类下所有 skill 的 name + description                  │
│ ~5-10 行 × ~100 chars = ~300 token/分类                   │
│                                                             │
│   调用: skill_list(category="wayland")                      │
│   返回:                                                     │
│   - wayland-compositor-debug: Common bugs and debugging... │
│   - wayland-shortcut-architecture: 快捷键处理架构...       │
│   - wayland-input-method-debug: IME support debug...       │
│   ...                                                       │
├─────────────────────────────────────────────────────────────┤
│ Tier 2: 完整内容 (按需, 通过 skill_view tool 获取)        │
│ 完整 SKILL.md 内容 + 关联文件                             │
│ 通过 tool result 注入对话上下文（不是 system prompt）     │
│                                                             │
│   调用: skill_view("wayland-shortcut-architecture")         │
│   返回: 完整 SKILL.md 内容 + linked files                  │
└─────────────────────────────────────────────────────────────┘
```

### 10.4 System Prompt 注入策略

`prompts/system_prompt.rs` 中的 `skills_block()` 负责构建 system prompt 中的 skill 部分：

- **Tier 0 分类索引**：遍历 `SkillRegistry.categories()`（IndexMap），输出分类名 + skill 数量 + 分类描述
- **智能预加载**：对声明了 `requires_tools` 的 skill，当所依赖 tool 全部可用时，
  自动追加 Active Skills 摘要（Tier 1），避免 LLM 额外调用 `skill_list`
- **builtin 分类特殊处理**：builtin category 下的 skill 内容完整注入 system prompt（核心行为准则）

### 10.5 Token 消耗对比

| 方案 | system prompt 内容 | 60 skills 时 | 200 skills 时 |
|------|--------------------|-------------|--------------|
| 当前 zeno（全量列表） | N × name + description | ~1500 token | ~5000 token |
| Hermes（分类全量列表） | N × name + description + 分类头 | ~2000 token | ~6000 token |
| **新方案 Tier 0** | K × category + count + desc | ~200 token | ~300 token |
| 新方案 + 智能预加载 | Tier 0 + 条件匹配 skill 摘要 | ~400 token | ~600 token |

分类索引从 1500 token 降到 ~200 token，**节省 87%**。即使加智能预加载，
200 个 skill 时也仅 ~600 token，比全量列表方案节省 88%。

### 10.6 Tool 接口

#### skill_list（Tier 1 检索）

`tools/skill_list.rs`：接收 `category` 或 `tag` 参数，返回对应 skill 摘要列表。
- `(Some(cat), _)` → 该分类下所有 skill 的 name + description
- `(_, Some(tag))` → 跨分类按 tag 检索
- `(None, None)` → 所有分类 + 各分类 skill 数量

#### skill_view（Tier 2 完整内容）

`tools/skill_view.rs`：接收 `name`（必填）和 `file_path`（可选），返回完整 SKILL.md 内容。
`file_path` 可加载 skill 目录下的关联文件（如 `references/api.md`）。

### 10.7 完整工作流示例

```text
用户: "帮我调试一个 Wayland compositor 的快捷键问题"
│
┌─▼───────────────────────────────────────────────────┐
│ System Prompt: Tier 0 分类索引                      │
│ - software-development (8 skills) — Coding...       │
│ - wayland (6 skills) — Wayland compositor...        │
│ - research (4 skills) — ...                         │
│                                                     │
│ Active Skills (requires_tools 匹配):               │
│ (无，因为 wayland skill 不声明 requires_tools)      │
└─┬───────────────────────────────────────────────────┘
  │ LLM 看到 wayland 分类相关
  ▼
调用: skill_list(category="wayland")
│
┌─▼───────────────────────────────────────────────────┐
│ Tool Result: Tier 1 摘要                           │
│ - wayland-compositor-debug: Common bugs and...      │
│ - wayland-shortcut-architecture: 快捷键架构...      │
│ - wayland-input-method-debug: IME support...        │
│ - wayland-output-scale-animation: Scale 与动画...   │
│ - wayland-signal-debug: Signal emission...          │
│ - wayland-blur-renderer-design: 模糊效果渲染...     │
└─┬───────────────────────────────────────────────────┘
  │ LLM 识别"快捷键"相关
  ▼
调用: skill_view("wayland-shortcut-architecture")
│
┌─▼───────────────────────────────────────────────────┐
│ Tool Result: Tier 2 完整内容                       │
│ (完整 SKILL.md，通过 tool result 注入对话上下文)   │
│ # Wayland Compositor 快捷键架构设计                 │
│ ...                                                 │
└─────────────────────────────────────────────────────┘
```

### 10.8 磁盘快照缓存

参考 Hermes 的 `.skills_prompt_snapshot.json`，zeno 在首次扫描后缓存 skill 元数据
到 `~/.config/zeno/.skills_cache.json`，后续启动直接读缓存，跳过文件扫描。

缓存结构包含：version（格式版本）、manifest（每个 .md 文件的 mtime + size 用于检测过期）、
序列化的 skill 列表和分类描述。manifest 中任一文件不匹配则回退全量扫描。

缓存命中时，skill 加载从 O(N) 文件 I/O 降为 1 次文件读取。

### 10.9 需要修改/新增的文件

| 文件 | 改动 | 说明 |
|------|------|------|
| `src/skills/types.rs` | 修改 | SkillDefinition 增加 category/tags/requires_tools/platforms；新增 CategoryInfo |
| `src/skills/loader.rs` | 修改 | 加载时从目录结构推导 category；解析 DESCRIPTION.md；解析 frontmatter 新字段 |
| `src/skills/registry.rs` | 修改 | 增加 categories IndexMap；添加 list_by_category/list_by_tag/filter_by_tools |
| `src/skills/index_cache.rs` | 新增 | 磁盘快照缓存 |
| `src/skills/mod.rs` | 修改 | 增加 pub mod index_cache |
| `src/prompts/system_prompt.rs` | 修改 | skills_block() 改为 Tier 0 分类索引 + 智能预加载 |
| `src/tools/skill_list.rs` | 新增 | skill_list tool |
| `src/tools/skill_view.rs` | 新增 | skill_view tool |
| `src/tools/mod.rs` | 修改 | 注册新 tool |

### 10.10 风险

| 风险 | 概率 | 影响 | 应对 |
|------|------|------|------|
| LLM 不知道该查哪个分类 | 中 | 中 | 智能预加载：requires_tools 匹配的 skill 自动展露 Tier 1 |
| 分类粒度不合理导致 skill 分散 | 中 | 低 | 支持跨分类 tag 检索作为兜底 |
| 缓存与文件系统不同步 | 低 | 低 | manifest (mtime+size) 校验，不一致时回退全量扫描 |
| frontmatter 格式不兼容（非 agentskills.io） | 中 | 低 | 解析失败时 fallback 到目录名作为 category，空 tags |
| skill_list 调用增加一轮 tool 往返 | 中 | 低 | 智能预加载减少常见场景的额外调用；分类索引足够精准时 LLM 可直接 skill_view |

---

## 11. Lua 配置系统

### 11.1 设计目标

参考 Neovim + WezTerm 的全 Lua 配置模式，替代 YAML 成为首选配置方式：

- **条件配置**：按 OS / 目录 / 环境变量动态切换 provider、权限等
- **模块化**：`require()` 加载自定义模块，配置可拆分复用
- **即时验证**：加载后验证配置完整性，拼写错误立即报告
- **向后兼容**：`/migrate` 命令可将旧 `config.yaml` 自动转换为 `init.lua`
- **安全隔离**：配置 VM 与插件 VM 独立，配置 VM 沙箱化（无 io/os/debug/ffi）

### 11.2 技术选型

| 维度 | 决策 | 理由 |
|------|------|------|
| Lua 版本 | Lua 5.4（`lua54` feature） | 比 LuaJIT 编译快，mlua 原生支持，无需 C 编译链 |
| Lua→Rust 转换 | mlua 内置 serde（`LuaSerdeExt::from_value`） | zeno 配置项 ~30 个，无需 WezTerm 的 wezterm-dynamic 中间层 |
| API 风格 | 模块函数模式（`zn.provider()` / `zn.set_model()`） | 比 WezTerm builder 模式更简洁，与 Neovim `vim.opt` 风格一致 |
| 验证策略 | 加载后验证（非赋值时验证） | 30 个配置项不需要 `__newindex` 即时验证，后验证代码更少 |
| 沙箱 | `TABLE|STRING|MATH|UTF8|COROUTINE|PACKAGE` + 移除 `dofile`/`loadfile` + 自定义 `require` 白名单 | 配置 VM 不需要文件 IO，插件 VM 另开实例放开权限 |

### 11.3 配置加载流水线

```
init.lua → Lua VM (safe libs only) → 执行脚本 → 返回 mlua::Value
    → lua.from_value::<Settings>() → validate() → Settings struct

无 init.lua → Settings::default() + 提示用户创建配置
```

### 11.4 `zeno` Lua 模块 API

```lua
local zn = require 'zeno'

-- Provider
zn.provider(name, { base_url=, api_key_env=, api_key=, default_model=, max_output_tokens= })
zn.set_provider(name)
zn.set_model(name)

-- Tools
zn.tool(name, enabled)          -- zn.tool("web_fetch", false)
zn.bash_env({ KEY = "value" })  -- 注入 bash 命令的环境变量

-- Web Search
zn.web_search({ provider=, url=, api_key_env=, api_key= })
-- provider: "searxng"(默认) | "brave" | "tavily" | "duckduckgo"

-- MCP
zn.mcp_server(name, { command= } | { url= })

-- Auxiliary
zn.auxiliary(task, { provider=, model=, base_url=, api_key=, timeout=, extra_body={}, max_tokens=, temperature= })
-- task: "compression" | "vision" | "web_extract" | "title_generation" | "session_search"

-- 模型上下文窗口（按前缀匹配，最长前缀优先）
zn.model_context({ ["claude"] = 200000, ["gpt-4"] = 128000, ... })

-- 全局设置
zn.permissions("ask" | "allow" | "deny")
zn.max_turns(n)
zn.max_tokens(n)                 -- 0 = auto
zn.theme(name)
zn.plugins_dir(path)
zn.memory_dir(path)
zn.memory_global(enabled)        -- true = XDG data dir, false = cwd-relative
zn.memory_char_limit(n)          -- MEMORY.md 字符限制（默认 4000）
zn.user_char_limit(n)            -- USER.md 字符限制（默认 2500）
zn.memory_provider(name, opts)   -- Lua 配置的外部 memory 提供商（见 memory/lua_provider.rs）
zn.log_retention_days(n)
zn.llm_max_retries(n)           -- LLM 空响应/瞬态错误重试次数（默认 3）

-- 环境查询（只读，供条件配置使用）
zn.cwd()                         -- 返回当前工作目录
zn.env(name)                     -- 返回环境变量值（nil 如果未设置）
zn.os()                          -- "linux" | "macos" | "windows"
zn.hostname()                    -- 主机名

-- 结束
zn.config()                      -- 构建并返回最终配置 table
```

### 11.5 Rust 实现 (config/loader.rs)

采用 **registry overrides** 模式：`zn.*` API 将用户配置写入 Lua registry 中的 overrides table，
`build_settings()` 从 `Settings::default()` 出发逐字段合并 overrides。未设置的字段保留默认值。

加载流水线：

1. `init.lua` 不存在 → 返回 `Settings::default()`
2. 创建沙箱化 Lua VM（`TABLE|STRING|MATH|UTF8|COROUTINE|PACKAGE`）
3. 移除 `dofile`/`loadfile`，设置 `package.path` 为 `~/.config/zeno/lua/`
4. 注册 `zeno` 模块（`zn.provider()` 等 API 写入 overrides registry）
5. 执行 `init.lua`，收集所有 `zn.*` 调用的覆盖值
6. `build_settings()` 从 defaults + overrides 构建 `Settings`
7. `validate()` 验证配置完整性，错误报告精确行号

### 11.6 Sandbox 策略

| 场景 | 加载的标准库 | 额外限制 |
|------|-------------|---------|
| 配置 VM | `TABLE|STRING|MATH|UTF8|COROUTINE|PACKAGE` | 移除 `dofile`/`loadfile`，自定义 `require` 白名单。⚠️ `ALL_SAFE` 包含 io/os，不可使用 |
| 插件 VM (Phase 5) | `ALL`（含 io/os） | zeno safe API 封装，无直接 ffi |

配置 VM 生命周期短（加载后释放），插件 VM 长驻。两者不共享状态。

### 11.7 错误处理

所有错误通过 anyhow context 链报告，用户看到精确位置：

```
Error: converting Lua config to Settings
Caused by: missing field `base_url` for provider 'groveer'
Location: ~/.config/zeno/init.lua:5
```

### 11.8 迁移支持

- `/migrate` 斜杠命令：将旧 `config.yaml` 转换为 `init.lua`（仅迁移工具，运行时不读 YAML）
- 两者可共存，`init.lua` 优先

### 11.9 实现路线

| Phase | 内容 | 工时 | 状态 |
|-------|------|------|------|
| P1 | `config/loader.rs`：Lua VM + zeno 模块 + serde 转换 + 验证 | 3-4 天 | ✅ |
| P2 | 自定义 `require` + `lua/` 模块路径 + `zn.env()`/`zn.os()`/`zn.cwd()`/`zn.hostname()` | 1-2 天 | ✅ |
| P3 | `/migrate` 命令（YAML → Lua 自动转换） | 1 天 | — |
| P4 | 热重载（notify crate + `/config-reload` 命令） | 2-3 天 | — |

### 11.10 风险

| 风险 | 概率 | 影响 | 应对 |
|------|------|------|------|
| mlua vendored 编译时间增加 | 高 | 中 | lua54 比 luajit 编译快；CI 缓存 |
| serde nil/null 语义陷阱 | 中 | 中 | `Option<T>` 用 `nil` 表示 None，文档说明 |
| 用户不熟悉 Lua | 中 | 低 | 丰富示例 + /migrate 从旧 YAML 迁移 |
| 沙箱不够安全 | 低 | 中 | 显式安全库组合 + 移除危险全局函数 + 自定义 require（⚠️ ALL_SAFE 含 io/os） |
