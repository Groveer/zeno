-- zeno configuration
-- Place at ~/.config/zeno/init.lua
--
-- You can split config into modules and require() them:
--   local utils = require 'utils'  -- loads ~/.config/zeno/lua/utils.lua
-- require() is sandboxed: only files under ~/.config/zeno/lua/ are allowed.
-- Path traversal (e.g. require '../etc/passwd') is blocked.

local zn = require("zeno")

-- ═══════════════════════════════════════════════
-- Providers
-- ═══════════════════════════════════════════════

-- Provider names are arbitrary — the `api_type` field determines which
-- API format to use. Default is "openai" (Chat Completions).
--
-- Supported api_type values:
--   "openai" (default)    → POST /v1/chat/completions, Bearer auth
--   "openai-responses"    → POST /v1/responses, Bearer auth
--   "anthropic"           → POST /v1/messages, x-api-key auth
--
-- When omitted, api_type defaults to "openai".

zn.provider("openai", {
  api_key = "OPENAI_API_KEY",
  base_url = "https://api.openai.com/v1",
  default_model = "gpt-4o",
  -- api_type defaults to "openai", no need to set
})
zn.set_provider("openai")
zn.set_model("gpt-5.5")

-- ═══════════════════════════════════════════════
-- Role (Identity & Persona)
-- ═══════════════════════════════════════════════
-- Customize the agent's identity and behavioral principles.
-- All fields are optional — unset ones use the built-in defaults.
-- Note: Tools and Skills guidance is always included and cannot be overridden.

-- Bulk configuration:
-- zn.role({
--   identity = "You are Alice, a senior Rust engineer.",
--   guidelines = "- Always write tests first.\n- Prefer zero-cost abstractions.",
-- })

-- Guidelines now supports external file references. Use a table of entries
-- where each entry is either a plain string or a {text, file_path} pair:
--   { "prefix text", zn.config_dir .. "rules.md" }
-- Both the prefix text and the file content are combined into the final guidelines.

-- zn.role({
--   identity = "You are Alice, a senior Rust engineer.",
--   guidelines = {
--     "- Always write tests first.",
--     { "- Company Rust style:", zn.config_dir .. "rust-style.md" },
--   },
-- })

-- Or use the individual setter:
-- zn.identity("You are a data engineer specializing in ETL pipelines.")
-- zn.guidelines({
--   "- Validate all inputs.",
--   { "- Team conventions:", zn.config_dir .. "team-rules.md" },
-- })

-- ═══════════════════════════════════════════════
-- Named Identities (Multiple Personas)
-- ═══════════════════════════════════════════════
-- Define named identities that can be switched at runtime via:
--   • /identity <name>  (TUI command)
--   • ZENO_IDENTITY=<name>  (environment variable)
--
-- Each identity overrides the base role's `identity` and `guidelines`
-- when activated.  All fields are optional — unset ones fall back to
-- the base role config or built-in defaults.
--
-- Identities also support the multi-entry guidelines format with file references:
--
-- zn.def_identity("dev", {
--   identity = "You are a senior Rust engineer. You write clean, idiomatic code.",
--   guidelines = {
--     "- Always write tests first.",
--     { "- Project conventions:", zn.config_dir .. "rust-style.md" },
--   },
-- })
--
-- zn.def_identity("ops", {
--   identity = "You are a DevOps engineer specializing in Kubernetes and CI/CD.",
--   guidelines = {
--     "- Always check resource limits.",
--     { "- Company runbooks:", zn.config_dir .. "ops-runbook.md" },
--   },
-- })
--
-- zn.def_identity("pm", {
--   identity = "You are a product manager. You focus on user stories and acceptance criteria.",
--   guidelines = "- Write clear, testable requirements.\n- Prioritize by business impact.",
-- })
--
-- Optionally set a default active identity on startup:
-- zn.set_identity("dev")

-- ═══════════════════════════════════════════════
-- Auxiliary models (cheaper models for specific tasks)
-- ═══════════════════════════════════════════════
--
-- Each task can override provider, model, url, api_key, extra_body, max_tokens, temperature, timeout.
-- timeout = 0 → no timeout (wait indefinitely).
-- provider = "auto" → try active provider first, then fallback to others.
-- model = "" → inherit from the resolved provider or main model.
-- url = nil → use the resolved provider's base_url.
-- api_key = nil → use the resolved provider's api_key.

zn.auxiliaries({
  -- Full fields reference (compression shows all available fields):
  compression = {
    provider = "auto",
    model = "",
    url = nil,
    api_key = nil,
    timeout = 30,    -- seconds; 0 = no timeout
    max_tokens = 0,
    temperature = nil,
  },
  vision = { provider = "auto", model = "", timeout = 30 },
  web_fetch = { provider = "auto", model = "", timeout = 60, max_tokens = 2048 },
  title_generation = { provider = "auto", model = "", timeout = 30, max_tokens = 256 },
  session_search = { provider = "auto", model = "", timeout = 30, max_tokens = 1024 },
  delegation = { provider = "auto", model = "", timeout = 60 },
})

-- ═══════════════════════════════════════════════
-- Tools
-- ═══════════════════════════════════════════════
--
-- Available tools and their defaults:
--   bash        = true   (shell command execution)
--   read        = true   (read file contents)
--   write       = true   (create/overwrite files)
--   edit        = true   (patch/find-replace in files)
--   glob        = true   (find files by name pattern)
--   grep        = true   (search file contents)
--   web_search  = true   (web search queries)
--   web_fetch   = true   (fetch URL content)

-- Disable tools (booleans), skip dirs, bash env — all in one table:
zn.tools({
  -- web_fetch = false,
  skip_dirs = {
    ".turbo",
    "build",
    "target",
  }, -- extra dirs to skip in glob/grep
  bash_env = {
    LC_ALL = "C",
  }, -- env vars for every bash call
})

-- ═══════════════════════════════════════════════
-- Execution Policy (Command-level Permissions)
-- ═══════════════════════════════════════════════
--
-- Fine-grained rule-based authorization for bash commands.
-- Each rule matches the command text via prefix matching (or regex when
-- `is_regex = true`). Rules are evaluated first-match-wins.
--
-- Actions:
--   Auto  — auto-allowed, no permission prompt
--   Ask   — requires user confirmation in "ask" permission mode
--   Deny  — blocked unconditionally, even in "allow" mode
--
-- Built-in rules cover ~90 common commands (ls, cat, git status → Auto;
-- rm, sudo, dd → Ask; rm -rf / → Deny). User rules are evaluated first,
-- so they can override any built-in rule.
--
-- Use `is_regex = true` when `^` anchors are needed for full command match:
--   zn.exec_policy({ pattern = "^cargo publish$", action = "ask",
--                    reason = "Confirm publishing to crates.io",
--                    is_regex = true })
--
-- Plain prefix matching (no is_regex) — just type the command start:
--   zn.exec_policy({ pattern = "cargo test", action = "auto",
--                    reason = "Running tests is safe" })
--   zn.exec_policy({ pattern = "git push", action = "ask",
--                    reason = "Confirm pushes to remote" })
--   zn.exec_policy({ pattern = "git checkout", action = "ask",
--                    reason = "Confirm checkout" })
--   zn.exec_policy({ pattern = "sudo rm", action = "deny",
--                    reason = "No sudo rm allowed" })
--
-- ⚠️  DON'T add ^ without is_regex=true — ^ is treated as a literal char!
--     Wrong:  { pattern = "^git push", action = "ask" }   -- matches "^git push" literally
--     Right:  { pattern = "git push", action = "ask" }    -- matches "git push" naturally

-- ═══════════════════════════════════════════════
-- Sandbox (Secure Command Execution)
-- ═══════════════════════════════════════════════
--
-- Sandbox provides process-level isolation for bash commands using
-- Bubblewrap (bwrap) on Linux. Commands are wrapped with filesystem
-- and network restrictions.
--
-- Modes:
--   "none" (default)      — no isolation, commands run normally
--   "workspace_write"     — root filesystem read-only, cwd + /tmp writable
--   "strict"              — only explicitly allowed paths, network disabled
--
-- Additional paths that should be writable/readable when sandboxed:
--   writable_paths = { "/data" }
--   readable_paths = { "/usr/local/share" }
--
-- zn.sandbox({ mode = "workspace_write" })
-- zn.sandbox({ mode = "strict", writable_paths = { "/workspace" },
--              readable_paths = { "/data/reference" } })

-- ═══════════════════════════════════════════════
-- Web Search
-- ═══════════════════════════════════════════════
--
-- Customize the web search backend. Default is SearXNG (no API key).
-- Supported providers:
--   "searxng"     — SearXNG meta-search (default, no key needed)
--   "brave"       — Brave Search API
--   "tavily"      — Tavily Search API
--   "duckduckgo"  — DuckDuckGo Lite (no key needed)
--
-- zn.web_search({ provider = "searxng", url = "http://localhost:8888" })
-- zn.web_search({ provider = "brave", api_key = "BRAVE_API_KEY" })  -- auto-detected as env var name
-- zn.web_search({ provider = "tavily", api_key = "TAVILY_API_KEY" })
-- zn.web_search({ provider = "duckduckgo" })
-- zn.web_search({ provider = "brave", api_key = "BSA-xxxx-yyyy" })  -- literal key (not UPPER_SNAKE_CASE)

-- ═══════════════════════════════════════════════
-- MCP Servers (lazy-loaded — zero startup cost)
-- ═══════════════════════════════════════════════
--
-- MCP servers are connected on-demand: the server process starts only
-- when the LLM first calls mcp_list_tools or mcp_call_tool on it.
-- This keeps zeno startup instant regardless of how many servers are configured.
--
-- Each server can have a `description` and optional `tags` to help the LLM
-- decide which server to activate — no blind activation needed.
-- `description` is shown in mcp_list_servers output before any connection.
-- `tags` provide additional semantic hints for routing decisions.
--
-- ── Local commands (stdio transport) ──────────────
--
-- zn.mcp_servers({
--   ["git"] = {
--     command = { "uvx", "mcp-server-git" },
--     description = "Git repository interaction and automation. This server provides tools to read, search, and manipulate Git repositories via Large Language Models.",
--     tags = { "git", "code" },
--   },
-- })
--
-- ── HTTP transport (remote servers) ───────────────
--
-- zn.mcp_servers({
--   -- Simple HTTP (no auth):
--   ["local-api"] = { url = "http://localhost:3000", description = "Local development API." },
--   -- With custom headers:
--   ["context7"] = {
--     url = "https://mcp.context7.com/mcp",
--     headers = {
--       CONTEXT7_API_KEY = "CONTEXT7_API_KEY",
--     },
--     description = "Library/framework documentation lookup. Use for any programming library docs, API references, or framework guides.",
--     tags = { "docs", "library", "api" },
--   },
--   ["jina-mcp-server"] = {
--     url = "https://mcp.jina.ai/v1",
--     headers = {
--       Authorization = "Bearer jina_5381569fbb2f4245ad419dd6ec1da251qrLOOcaydPLGF_yAJBgieFIXQ0QZ",
--     },
--     description = "A suite of URL-to-markdown, web search, image search, and embeddings/reranker tools.",
--     tags = { "web", "search", "reranker" },
--   },
-- })

-- ═══════════════════════════════════════════════
-- Model Context Windows
-- ═══════════════════════════════════════════════
--
-- Define context window sizes for model families.
-- zeno uses longest-prefix match: "claude-opus-4-6" is more specific
-- than "claude", so it takes priority when both are defined.
-- When no prefix matches, DEFAULT_CONTEXT_WINDOW (128000) is used.

zn.model_context({
  -- Anthropic Claude
  ["claude-opus-4-7"] = 1000000,
  ["claude-opus-4.7"] = 1000000,
  ["claude-opus-4-6"] = 1000000,
  ["claude-sonnet-4-6"] = 1000000,
  ["claude-opus-4.6"] = 1000000,
  ["claude-sonnet-4.6"] = 1000000,
  ["claude"] = 200000,
  -- OpenAI GPT
  ["gpt-5.5"] = 1050000,
  ["gpt-5.4-nano"] = 400000,
  ["gpt-5.4-mini"] = 400000,
  ["gpt-5.4"] = 1050000,
  ["gpt-5.1-chat"] = 128000,
  ["gpt-5"] = 400000,
  ["gpt-4.1"] = 1047576,
  ["gpt-4"] = 128000,
  -- Google Gemini
  ["gemini"] = 1048576,
  -- DeepSeek
  ["deepseek-v4"] = 1000000,
  ["deepseek-chat"] = 1000000,
  ["deepseek-reasoner"] = 1000000,
  ["deepseek"] = 128000,
  -- Meta Llama
  ["llama"] = 131072,
  -- Qwen
  ["qwen3-coder-plus"] = 1000000,
  ["qwen3-coder"] = 262144,
  ["qwen"] = 131072,
  -- GLM / Z.AI
  ["glm-5"] = 202752,
  ["glm"] = 202752,
  -- xAI Grok
  ["grok-4"] = 262144,
  ["grok-3"] = 131072,
  ["grok"] = 131072,
  -- MiniMax
  ["minimax"] = 204800,
  ["mimo"] = 1024 * 1024,
})

-- ═══════════════════════════════════════════════
-- Permissions & Limits
-- ═══════════════════════════════════════════════

zn.permissions("ask") -- "allow" | "deny" | "ask"
-- Determines behavior for files *outside* trusted paths.

-- Trusted paths: files under these directories are always allowed,
-- bypassing both the CWD boundary check and permission prompts.
-- Useful for declaring development directories you fully trust.
-- zn.trusted_paths({"/home/user/Develop/", "/home/user/work/"})

zn.max_turns(200)
zn.max_tokens(0) -- 0 = auto (derived from model context window)
zn.theme("default") -- "default" | "dark" | "light"
zn.log_retention_days(7)
zn.llm_max_retries(3) -- retry on empty response or transient error (default: 3)

-- Auto-compact: compress conversation history when estimated tokens exceed
-- this fraction of the model's context window (0.0-1.0, default: 0.5).
-- Set to 0 to disable auto-compaction entirely.
zn.compact_threshold(0.5)

-- ═══════════════════════════════════════════════
-- Engine Behavior
-- ═══════════════════════════════════════════════
-- Fine-tune the query engine's timeouts and auto-continue limits.
-- All values use sensible defaults — uncomment to customize.

-- zn.engine({
--   max_auto_continue = 3,         -- max retries when LLM stops without tool use
--   stream_timeout_secs = 120,     -- stream idle timeout (no event for N seconds), 0 = no timeout
--   collapse_char_limit = 2400,  -- text blocks larger than this get head/tail collapsed
--   collapse_head_chars = 900,   -- chars to keep from the beginning
--   collapse_tail_chars = 500,   -- chars to keep from the end
-- })

-- ═══════════════════════════════════════════════
-- Delegation (Sub-agents)
-- ═══════════════════════════════════════════════
-- Control sub-agent behavior when spawned via the delegate_task tool.
-- All values use sensible defaults — uncomment to customize.

-- zn.delegation({
--   max_concurrent_children = 3,   -- max parallel sub-agents
--   max_turns = 30,                -- max tool-calling turns per sub-agent
--   max_auto_continue = 2,         -- auto-continue retries for empty responses
-- })

-- ═══════════════════════════════════════════════
-- Safe Paths (Permission Bypass)
-- ═══════════════════════════════════════════════
-- Extra directories that are always allowed, bypassing permission prompts.
-- Built-in safe paths: /tmp/, /var/tmp/.
-- zn.safe_paths({ "/home/user/sandbox/", "/data/cache/" })

-- ═══════════════════════════════════════════════
-- Skills (Background Review & Curator)
-- ═══════════════════════════════════════════════
-- Zeno can automatically maintain its skill library by:
--   1. Reviewing conversations after every N turns to extract learnings
--   2. Periodically consolidating narrow skills into broader class-level skills
--   3. Automatically archiving skills that haven't been used in a while
--
-- All settings below use sensible defaults — uncomment to customize.

-- zn.skills({
--   -- Background review: after every 10 turns, review the conversation
--   -- and create/update skills based on learnings.
--   background_review_enabled = true,    -- false to disable
--   review_interval_turns = 10,          -- 0 to disable
--
--   -- Curator: automatic lifecycle maintenance when idle.
--   curator_enabled = true,              -- false to disable
--   curator_interval_hours = 168,        -- every 7 days
--   stale_after_days = 30,               -- 30 days unused → stale
--   archive_after_days = 90,             -- 90 days unused → archived
-- })

-- ═══════════════════════════════════════════════
-- Memory
-- ═══════════════════════════════════════════════
-- MEMORY.md is stored globally at ~/.config/zeno/memory/MEMORY.md
-- USER.md is stored globally at ~/.config/zeno/USER.md

zn.memory_char_limit(2200) -- MEMORY.md character limit (default: 2200)
zn.user_char_limit(1375) -- USER.md character limit (default: 1375)

-- ── External Memory Providers (Lua script-based) ──────────
--
-- External providers run alongside the built-in MEMORY.md/USER.md store.
-- Only ONE external provider can be active at a time.
-- Configure via require() which returns a provider table:
--
-- zn.memory_provider("hindsight", require("hindsight"))
--
-- ── Memory Provider Lifecycle Hooks ────────────────────────
--
-- A Lua memory provider script returns a table implementing these hooks:
--
-- Required:
--   name              = string   -- e.g. "mem0", "honcho"
--   is_available      = function() → bool     -- check config, no network
--   initialize        = function(session_id)  -- connect, warm up
--
-- Core (optional):
--   system_prompt     = string   -- static text for system prompt
--   tool_schemas      = table    -- array of OpenAI function-call schemas
--   handle_tool_call  = function(tool_name, args_json) → json_string
--   prefetch          = function(query) → string  -- recall before each turn
--   queue_prefetch    = function(query)            -- background prefetch for next turn
--   sync_turn         = function(user_content, assistant_content)
--   on_memory_change   = function(action, target, content)  -- mirror built-in writes
--   shutdown          = function()
--
-- Lifecycle hooks (optional, called by the engine at key events):
--   on_turn_start     = function(turn_number, message)
--   on_session_end    = function(messages_json)          -- session exit/timeout
--   on_session_switch = function(new_id, parent_id, reset) -- /restore, /reset etc.
--   on_pre_compress   = function(messages_json) → string -- before context compression
--
-- ── Example: Full-featured memory provider ─────────────────
--
-- zn.memory_provider("example", { script = [[
--   local turn_count = 0
--   return {
--     name = "example",
--     system_prompt = "External memory provider active.",
--
--     is_available = function()
--       return os.getenv("EXAMPLE_API_KEY") ~= nil
--     end,
--
--     initialize = function(session_id)
--       -- Connect to backend, create session, etc.
--       turn_count = 0
--     end,
--
--     -- Tool schemas exposed to the LLM
--     tool_schemas = {
--       {
--         name = "memory_search",
--         description = "Search persistent memory by meaning.",
--         parameters = {
--           type = "object",
--           properties = {
--             query = { type = "string", description = "What to search for." },
--           },
--           required = { "query" },
--         },
--       },
--     },
--
--     handle_tool_call = function(tool_name, args_json)
--       local args = json.decode(args_json)
--       if tool_name == "memory_search" then
--         -- Search your backend for relevant memories
--         return json.encode({ success = true, results = {} })
--       end
--       return json.encode({ error = "unknown tool" })
--     end,
--
--     -- Pre-turn recall: fetch relevant context from your backend
--     prefetch = function(query)
--       return ""  -- return relevant text, or "" for nothing
--     end,
--
--     -- Background prefetch for the next turn (non-blocking)
--     queue_prefetch = function(query)
--       -- Kick off async search, cache results for next prefetch()
--     end,
--
--     -- Persist a completed turn to the backend
--     sync_turn = function(user_content, assistant_content)
--       -- Send the turn pair to your memory backend
--     end,
--
--     -- Mirror built-in memory writes to your backend
--     on_memory_change = function(action, target, content)
--       -- action is "add", "replace", or "remove"
--       -- target is "memory" or "user"
--     end,
--
--     -- Per-turn notification with turn number
--     on_turn_start = function(turn_number, message)
--       turn_count = turn_number
--     end,
--
--     -- End-of-session: extract facts, flush buffers, etc.
--     on_session_end = function(messages_json)
--       local messages = json.decode(messages_json)
--       -- Extract key facts from the conversation and persist them
--       -- Flush any pending writes or background tasks
--     end,
--
--     -- Session ID rotation: /restore, /branch, /reset, context compression
--     on_session_switch = function(new_id, parent_id, reset)
--       -- Update internal session tracking
--       -- If reset == true, flush per-session buffers
--     end,
--
--     -- Before context compression: extract insights from messages about to be lost
--     on_pre_compress = function(messages_json)
--       local messages = json.decode(messages_json)
--       -- Extract and return key insights to include in the compression summary
--       -- Return "" to skip
--       return ""
--     end,
--
--     shutdown = function()
--       -- Flush queues, close connections
--     end,
--   }
-- ]])
--
-- ── Hindsight Memory Provider ───────────────────────
--
-- Hindsight is a long-term memory backend with knowledge graph, entity
-- resolution, and multi-strategy retrieval. Supports cloud and local modes.
--
-- Setup:
--   1. Get API key from https://ui.hindsight.vectorize.io (cloud mode)
--   2. Set environment variables (see below)
--   3. Uncomment the zn.memory_provider line below
--
-- Environment variables:
--   HINDSIGHT_API_KEY    — API key for cloud mode (required for cloud)
--   HINDSIGHT_MODE       — "cloud" (default) or "local"
--   HINDSIGHT_API_URL    — API endpoint (default: https://api.hindsight.vectorize.io)
--   HINDSIGHT_BANK_ID    — Memory bank name (default: "zeno")
--   HINDSIGHT_BUDGET     — Recall thoroughness: "low", "mid" (default), "high"
--
-- The provider script is at lua/hindsight.lua (project example).
-- Copy it to ~/.config/zeno/lua/hindsight.lua, then use require() to load it.

-- zn.memory_provider("hindsight", require("hindsight"))

-- ═══════════════════════════════════════════════
-- Hooks (Lua callbacks at lifecycle points)
-- ═══════════════════════════════════════════════
-- Hooks let you run Lua code at key lifecycle events to block dangerous
-- operations, inject project context, transform user input, log API usage,
-- and more.  Hook errors are logged but never crash the agent.
--
-- The hook VM has: table, string, math, utf8, coroutine, os.getenv, json
-- There is NO io, os.execute, dofile, or print — hooks are sandboxed.
--
-- Event                     Can return
-- ────────────────────────────────────────────────────────
-- pre_tool_use              { block = "reason" } — prevent tool execution
-- post_tool_use             (observe-only)
-- session_start             (observe-only)
-- session_end               (observe-only)
-- pre_llm_call              { inject_context = "text" } — append to system prompt
-- post_llm_call             (observe-only)
-- user_message              { modified_input = "text" } — rewrite user input
--
-- Context fields by event:
--   pre_tool_use / post_tool_use:   tool_name, tool_input (Lua table), cwd
--                                    post_tool_use adds: tool_output, tool_is_error
--   pre_llm_call / post_llm_call:   model, turn, cwd
--                                    pre_llm_call adds: message_count
--                                    post_llm_call adds: input_tokens, output_tokens, total_tokens, stop_reason
--   session_start / session_end:    cwd, model, provider
--   user_message:                   input, cwd

-- ── Block dangerous shell commands ──────────────────
-- zn.hook("pre_tool_use", function(ctx)
--   if ctx.tool_name == "bash" then
--     local cmd = ctx.tool_input.command or ""
--     if cmd:find("rm %-rf /") or cmd:find("sudo rm") then
--       return { block = "Refusing to run destructive command: " .. cmd }
--     end
--     if cmd:find("git push %-%-force") then
--       return { block = "Refusing to force-push. Use --force-with-lease instead." }
--     end
--   end
-- end)

-- ── Block writes outside the project tree ───────────
-- zn.hook("pre_tool_use", function(ctx)
--   if ctx.tool_name == "write" or ctx.tool_name == "edit" then
--     local path = ctx.tool_input.path or ""
--     -- Only allow writes under cwd
--     if not path:find("^" .. ctx.cwd) and not path:find("^~/") then
--       return { block = "Write outside project tree blocked." }
--     end
--   end
-- end)

-- ── Block tool use at night (conditional) ────────────
-- zn.hook("pre_tool_use", function(ctx)
--   if ctx.tool_name == "write" and os.getenv("ALLOW_NIGHT_WRITES") ~= "1" then
--     -- Hour check requires timestamps; note os.date is NOT available.
--     -- You could record start time in session_start and compare later.
--   end
-- end)

-- ── Inject static project context ───────────────────
-- zn.hook("pre_llm_call", function(ctx)
--   return { inject_context = "Project: zeno | Language: Rust | Always add tests." }
-- end)

-- ── Inject context conditionally by model ────────────
-- zn.hook("pre_llm_call", function(ctx)
--   if ctx.model:find("^claude") then
--     return { inject_context = "Use Anthropic-style XML tool calling." }
--   end
--   -- return nil → no injection
-- end)

-- ── Transform user input ────────────────────────────
-- zn.hook("user_message", function(ctx)
--   -- Automatically prepend task context
--   return { modified_input = "[Task] " .. ctx.input }
-- end)

-- ── Session boundary hooks ──────────────────────────
-- zn.hook("session_start", function(ctx)
--   -- Can log to external service via coroutine resume or just observe.
--   -- The ctx table contains cwd, model, provider.
-- end)
-- zn.hook("session_end", function(ctx) end)

-- ═══════════════════════════════════════════════
-- Conditional configuration (examples)
-- ═══════════════════════════════════════════════

-- Auto-trust specific development directories
-- if string.find(zn.cwd, "/home/user/Develop/") then
--   zn.trusted_paths({"/home/user/Develop/"})
-- end

-- Use different provider by hostname
-- if zn.hostname == "work-laptop" then
--   zn.set_provider("openai")
-- end

-- Use different model by environment variable
-- local model_override = zn.env("ZENO_MODEL")
-- if model_override then
--   zn.set_model(model_override)
-- end

-- Locate zeno's own config/data/cache directories (process-lifetime constants)
-- local zb_config = zn.config_dir   -- e.g. ~/.config/zeno/
-- local zb_data   = zn.data_dir     -- e.g. ~/.local/share/zeno/
-- local zb_cache  = zn.cache_dir    -- e.g. ~/.cache/zeno/

-- ═══════════════════════════════════════════════
-- Finalize
-- ═══════════════════════════════════════════════

return zn.config()
