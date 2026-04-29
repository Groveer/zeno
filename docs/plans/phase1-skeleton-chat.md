# Phase 1: 骨架 + 对话 实现计划

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** 搭建 rcode 项目骨架，实现从配置加载到流式对话的完整最小闭环。

**Architecture:** clap CLI → Settings 加载 → Anthropic API Client（reqwest + SSE）→ 流式输出到终端（println）。不做 TUI，不做 tool，纯文本交互。

**Tech Stack:** Rust 2024 edition, tokio, reqwest, eventsource-stream, serde/serde_yaml, clap

**Verification:** `rc "用 Rust 写 hello world"` 能流式输出回答。

---

### Task 1: 初始化 Cargo.toml 依赖

**Objective:** 配置所有 Phase 1 需要的 crate 依赖

**Files:**
- Modify: `Cargo.toml`

**Step 1: 写入依赖**

```toml
[package]
name = "rcode"
version = "0.1.0"
edition = "2024"

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

# 错误处理
anyhow = "1"
thiserror = "2"

# 日志
tracing = "0.1"
tracing-subscriber = "0.3"

# 其他
dirs = "6"
```

**Step 2: 验证编译**

Run: `cargo check`
Expected: 编译通过（可能需要下载依赖）

**Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: add Phase 1 dependencies"
```

---

### Task 2: 搭建目录结构和 mod 声明

**Objective:** 创建 DESIGN.md 中定义的 Phase 1 涉及的所有目录和模块声明文件

**Files:**
- Create: `src/cli/mod.rs`, `src/cli/commands.rs`
- Create: `src/config/mod.rs`, `src/config/settings.rs`, `src/config/paths.rs`
- Create: `src/api/mod.rs`, `src/api/client.rs`, `src/api/anthropic.rs`, `src/api/sse.rs`, `src/api/types.rs`
- Create: `src/engine/mod.rs`, `src/engine/query.rs`, `src/engine/messages.rs`, `src/engine/stream_events.rs`
- Modify: `src/main.rs`

**Step 1: 创建所有目录和空 mod 文件**

每个 mod.rs 只包含 `pub mod submodule;` 声明。其他 .rs 文件暂为空（只有必要的 struct/trait 存根，确保 `cargo check` 通过）。

**Step 2: 更新 main.rs**

```rust
mod cli;
mod config;
mod api;
mod engine;

fn main() {
    println!("Hello, world!");
}
```

**Step 3: 验证编译**

Run: `cargo check`
Expected: 编译通过

**Step 4: Commit**

```bash
git add -A
git commit -m "chore: scaffold project directory structure"
```

---

### Task 3: 配置系统 — Settings struct + YAML 加载

**Objective:** 实现 Settings 反序列化、XDG 路径解析、默认配置生成

**Files:**
- Modify: `src/config/settings.rs`
- Modify: `src/config/paths.rs`
- Create: `config.example.yaml`

**Step 1: 实现 Settings struct（完整 serde derive）**

从 DESIGN.md 第 4.1 节复制所有 struct 定义：
- `Settings`（顶层配置）
- `ProviderConfig`
- `ToolsConfig`
- `McpConfig`
- `PluginConfig`
- `MemoryConfig`
- `AuxiliaryConfig` / `AuxiliaryTaskConfig`
- `PermissionMode`（+ Display + FromStr）

所有 struct 需要 `Default` impl（except ProviderConfig）。

**Step 2: 实现路径解析 (paths.rs)**

```rust
use dirs;

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap().join("rcode")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.yaml")
}

pub fn ensure_config_dir() -> anyhow::Result<PathBuf> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
```

**Step 3: 实现配置加载逻辑**

```rust
pub fn load() -> anyhow::Result<Settings> {
    let path = config_path();
    if !path.exists() {
        return Ok(Settings::default());
    }
    let content = std::fs::read_to_string(&path)?;
    let settings: Settings = serde_yaml::from_str(&content)?;
    Ok(settings)
}

pub fn resolve_api_key(provider: &ProviderConfig) -> anyhow::Result<String> {
    if let Some(ref key) = provider.api_key {
        return Ok(key.clone());
    }
    if let Some(ref env_var) = provider.api_key_env {
        std::env::var(env_var)
            .map_err(|_| anyhow::anyhow!("Environment variable {} not set", env_var))
    } else {
        anyhow::bail!("No api_key or api_key_env configured")
    }
}
```

**Step 4: 创建 config.example.yaml**

从 DESIGN.md 第 4.1 节复制 YAML 示例。

**Step 5: 验证编译**

Run: `cargo check`
Expected: PASS

**Step 6: Commit**

```bash
git add -A
git commit -m "feat(config): Settings struct with YAML deserialization and XDG paths"
```

---

### Task 4: API 类型定义 — Message, ToolUse, ContentBlock, StreamEvent

**Objective:** 定义 LLM 对话中所有核心数据类型

**Files:**
- Modify: `src/api/types.rs`
- Modify: `src/engine/messages.rs`
- Modify: `src/engine/stream_events.rs`

**Step 1: 定义 api/types.rs**

核心类型：
- `Role` enum: `User`, `Assistant`
- `ContentBlock` enum: `Text(String)`, `ToolUse { id, name, input }`, `ToolResult { id, content, is_error }`
- `Message { role, content: Vec<ContentBlock> }`
- `StopReason` enum: `EndTurn`, `ToolUse`, `MaxTokens`
- `Usage { input_tokens, output_tokens }`
- `ApiError` (thiserror)

**Step 2: 定义 engine/messages.rs**

- `ConversationMessage` — 对话历史条目（区分 user/assistant/system）
- `ConversationHistory` — 对话历史管理（push、trim、format for API）

**Step 3: 定义 engine/stream_events.rs**

从 DESIGN.md 4.3 节复制 `StreamEvent` enum：
- `TextDelta(String)`
- `ToolUseStart { id, name, input_json }`
- `ToolUseDelta { id, delta_json }`
- `MessageComplete { stop_reason, usage }`
- `Error(String)`

**Step 4: 验证编译**

Run: `cargo check`

**Step 5: Commit**

```bash
git add -A
git commit -m "feat(api): define core message types and stream events"
```

---

### Task 5: Anthropic API Client — SSE 流式请求

**Objective:** 实现 Anthropic Messages API 的流式调用，返回 `Stream<Item = Result<StreamEvent>>`

**Files:**
- Modify: `src/api/client.rs` — `SupportsStreamingMessages` trait
- Modify: `src/api/anthropic.rs` — Anthropic 实现
- Modify: `src/api/sse.rs` — SSE 流解析
- Create: `tests/api_smoke_test.rs` (可选，集成测试)

**Step 1: 定义 trait (client.rs)**

```rust
#[async_trait]
pub trait SupportsStreamingMessages: Send + Sync {
    async fn stream_messages(
        &self,
        model: &str,
        system: &str,
        messages: &[api::types::Message],
        tools: &[serde_json::Value],
        max_tokens: u32,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>, ApiError>;
}
```

**Step 2: 实现 SSE 解析 (sse.rs)**

使用 `eventsource-stream` crate：
1. 接收 reqwest Response 的 byte stream
2. 解析 SSE `event:` + `data:` 行
3. 根据 `event` 类型分发：`message_start`, `content_block_start`, `content_block_delta`, `message_delta`, `message_stop`
4. 将 `data` JSON 反序列化为对应 Anthropic event struct
5. 转换为 `StreamEvent` 输出

**Step 3: 实现 Anthropic client (anthropic.rs)**

```rust
pub struct AnthropicClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl AnthropicClient {
    pub fn new(api_key: String, base_url: String) -> Self { ... }
}
```

POST 到 `/v1/messages`，请求体：
```json
{
  "model": "...",
  "max_tokens": 4096,
  "system": "...",
  "messages": [...],
  "stream": true
}
```

Headers: `x-api-key`, `anthropic-version: 2023-06-01`, `content-type: application/json`

**Step 4: 验证编译**

Run: `cargo check`

**Step 5: Commit**

```bash
git add -A
git commit -m "feat(api): implement Anthropic streaming client with SSE parsing"
```

---

### Task 6: 核心对话循环 — Engine query

**Objective:** 实现 tool-aware 的对话循环（Phase 1 版本无 tool，纯文本）

**Files:**
- Modify: `src/engine/query.rs`

**Step 1: 实现基础对话循环**

```rust
pub struct QueryEngine {
    client: Box<dyn SupportsStreamingMessages>,
    model: String,
    system_prompt: String,
    history: ConversationHistory,
    max_turns: u32,
    max_tokens: u32,
}

impl QueryEngine {
    pub async fn query(&mut self, user_input: &str) -> anyhow::Result<QueryResult> {
        self.history.push_user(user_input);

        let mut turn = 0;
        loop {
            turn += 1;
            if turn > self.max_turns {
                break;
            }

            let messages = self.history.to_api_messages();
            let mut stream = self.client.stream_messages(
                &self.model,
                &self.system_prompt,
                &messages,
                &[],
                self.max_tokens,
            ).await?;

            let mut assistant_text = String::new();
            let mut tool_uses = Vec::new();

            while let Some(event) = stream.next().await {
                match event? {
                    StreamEvent::TextDelta(delta) => {
                        print!("{}", delta);
                        std::io::Write::flush(&mut std::io::stdout())?;
                        assistant_text.push_str(&delta);
                    }
                    StreamEvent::ToolUseStart { .. } => { /* Phase 1 ignore */ }
                    StreamEvent::ToolUseDelta { .. } => { /* Phase 1 ignore */ }
                    StreamEvent::MessageComplete { stop_reason, usage } => {
                        // 记录 usage
                    }
                    StreamEvent::Error(e) => {
                        anyhow::bail!("Stream error: {}", e);
                    }
                }
            }

            self.history.push_assistant(&assistant_text);
            break; // Phase 1: no tool loop, single turn
        }

        Ok(QueryResult {})
    }
}
```

**Step 2: 验证编译**

Run: `cargo check`

**Step 3: Commit**

```bash
git add -A
git commit -m "feat(engine): implement basic query loop with streaming output"
```

---

### Task 7: CLI 集成 — clap + main.rs 串联

**Objective:** 实现完整的 CLI 入口：加载配置 → 构建 client → 运行 query

**Files:**
- Modify: `src/cli/commands.rs`
- Modify: `src/main.rs`

**Step 1: 定义 CLI args (clap derive)**

```rust
#[derive(Parser)]
#[command(name = "rc", version, about = "Rust AI Coding Assistant")]
struct Cli {
    /// User prompt (non-interactive mode)
    prompt: Option<String>,

    /// Override provider
    #[arg(long)]
    provider: Option<String>,

    /// Override model
    #[arg(long)]
    model: Option<String>,

    /// Max turns
    #[arg(long, default_value = "8")]
    max_turns: u32,
}
```

**Step 2: 实现 main.rs**

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::init();

    let cli = Cli::parse();
    let settings = config::settings::load()?;

    let provider_name = cli.provider.as_deref().unwrap_or(&settings.active_provider);
    let model = cli.model.as_deref().unwrap_or(&settings.model);

    let provider_config = settings.providers.get(provider_name)
        .ok_or_else(|| anyhow::anyhow!("Provider '{}' not found", provider_name))?;
    let api_key = config::settings::resolve_api_key(provider_config)?;

    let client = api::anthropic::AnthropicClient::new(
        api_key,
        provider_config.base_url.clone(),
    );

    let mut engine = engine::query::QueryEngine::new(
        Box::new(client),
        model.to_string(),
        String::new(), // system prompt
        engine::messages::ConversationHistory::new(),
        cli.max_turns,
        settings.max_tokens,
    );

    match cli.prompt {
        Some(prompt) => {
            // 非交互模式
            engine.query(&prompt).await?;
            println!(); // trailing newline
        }
        None => {
            // 交互模式（Phase 1: 简单的 readline 循环）
            println!("rcode v{} — type /exit to quit", env!("CARGO_PKG_VERSION"));
            loop {
                print!("> ");
                std::io::Write::flush(&mut std::io::stdout())?;
                let mut input = String::new();
                if std::io::stdin().read_line(&mut input)? == 0 {
                    break;
                }
                let input = input.trim();
                if input.is_empty() { continue; }
                if input == "/exit" || input == "/quit" { break; }
                engine.query(input).await?;
                println!();
            }
        }
    }

    Ok(())
}
```

**Step 3: 验证编译**

Run: `cargo build`
Expected: 编译成功，生成 `target/debug/rcode` (即 `rc`)

**Step 4: 冒烟测试**

```bash
# 需要设置 API key
export ANTHROPIC_API_KEY="sk-ant-..."
./target/debug/rc "用 Rust 写 hello world"
```

Expected: 流式输出回答

**Step 5: Commit**

```bash
git add -A
git commit -m "feat(cli): integrate config, API client, and engine for end-to-end streaming chat"
```

---

## 后续阶段（本次不实现）

- Phase 2: Tool 系统（Tool trait + bash/file/glob/grep + engine tool loop）
- Phase 3: ratatui TUI
- Phase 4: OpenAI provider / MCP / auxiliary router / memory
- Phase 5: Lua 插件
- Phase 6: 打磨发布
