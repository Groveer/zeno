-- zeno configuration
-- Place at ~/.config/zeno/init.lua
--
-- Documentation: https://github.com/user/zeno#configuration
--
-- You can split config into modules and require() them:
--   local utils = require 'utils'  -- loads ~/.config/zeno/lua/utils.lua
-- require() is sandboxed: only files under ~/.config/zeno/lua/ are allowed.
-- Path traversal (e.g. require '../etc/passwd') is blocked.

local zn = require("zeno")

-- ═══════════════════════════════════════════════
-- Providers
-- ═══════════════════════════════════════════════

-- Anthropic (default)
zn.provider("anthropic", {
	api_key = "ANTHROPIC_API_KEY",  -- auto-detected as env var name
  base_url = "https://api.anthropic.com",
  default_model = "claude-sonnet-4-20250514",
})

-- OpenAI compatible
zn.provider("openai", {
	api_key = "OPENAI_API_KEY",  -- auto-detected as env var name
  base_url = "https://api.openai.com/v1",
  default_model = "gpt-4o",
})

-- Custom OpenAI-compatible endpoint (e.g. Groveer, DeepSeek, local Ollama)
-- zn.provider("groveer", {
--   api_key = "GROVEER_API_KEY",  -- auto-detected as env var name
--   base_url = "https://cpa.groveer.com/v1",
--   default_model = "glm-5.1",
-- })
--
-- ═══════════════════════════════════════════════
-- Tools
-- ═══════════════════════════════════════════════
--
-- Available tools and their defaults:
--   bash        = true   (shell command execution)
--   file_read   = true   (read file contents)
--   file_write  = true   (create/overwrite files)
--   file_edit   = true   (patch/find-replace in files)
--   glob        = true   (find files by name pattern)
--   grep        = true   (search file contents)
--   web_search  = true   (web search queries)
--   web_fetch   = false  (fetch URL content — disabled by default)
--
-- Disable unwanted tools:
-- zn.tool("web_fetch", false)

-- Set environment variables injected into every bash command execution:
-- zn.bash_env({
--   NODE_ENV = "development",
--   DOCKER_HOST = "unix:///var/run/docker.sock",
-- })

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
-- Usage flow (LLM sees these 4 meta-tools automatically):
--   1. mcp_list_servers   — see configured servers & status
--   2. mcp_list_tools     — discover tools on a server (triggers connection)
--   3. mcp_describe_tool  — get a tool's parameter schema
--   4. mcp_call_tool      — execute a tool with arguments
--
-- ── Style 1: Bulk table (recommended for multiple servers) ──────
--
-- zn.mcp_servers({
--   ["filesystem"] = { command = { "npx", "-y", "@modelcontextprotocol/server-filesystem", "/home/user/documents" } },
--   ["github"]     = { command = { "npx", "-y", "@modelcontextprotocol/server-github" } },
--   ["git"]        = { command = { "npx", "-y", "@modelcontextprotocol/server-git", "--repository", "." } },
--   ["postgres"]   = { command = { "npx", "-y", "@modelcontextprotocol/server-postgres", "postgresql://localhost/mydb" } },
--   ["sqlite"]     = { command = { "npx", "-y", "mcp-server-sqlite", "--db-path", "/path/to/database.db" } },
--   ["fetch"]      = { command = { "npx", "-y", "@modelcontextprotocol/server-fetch" } },
-- })
--
-- ── Style 2: Individual calls (also works, can mix with bulk) ──
--
-- zn.mcp_server("filesystem", {
--   command = { "npx", "-y", "@modelcontextprotocol/server-filesystem", "/home/user/documents" },
-- })
--
-- ── HTTP Transport ─────────────────────────────
--
-- Remote MCP server via HTTP:
-- zn.mcp_server("remote-api", {
--   url = "http://localhost:3000",
-- })
--
-- HTTP with custom headers (API key, Bearer token, etc.):
-- zn.mcp_server("remote-api", {
--   url = "https://api.example.com/mcp",
--   headers = {
--     ["Authorization"] = "Bearer sk-your-token-here",
--     ["X-API-Key"] = "your-api-key",
--   },
-- })
--
-- GitLab MCP:
-- zn.mcp_server("gitlab", {
--   url = "https://gitlab.com/api/v4/mcp",
--   headers = {
--     ["PRIVATE-TOKEN"] = "glpat-xxxxxxxxxxxx",
--   },
-- })

-- ═══════════════════════════════════════════════
-- Auxiliary models (cheaper models for specific tasks)
-- ═══════════════════════════════════════════════
--
-- Each task can override provider, model, url, api_key, timeout, extra_body, max_tokens, temperature.
-- provider = "auto" → try active provider first, then fallback to others.
-- model = "" → inherit from the resolved provider or main model.
-- url = nil → use the resolved provider's base_url.
-- api_key = nil → use the resolved provider's api_key.

zn.auxiliary("compression", {
  provider = "auto",
  model = "", -- "" = inherit from main model
  timeout = 30,
})

zn.auxiliary("vision", {
  provider = "auto",
  model = "",
  timeout = 30,
})

zn.auxiliary("web_fetch", {
  provider = "auto",
  model = "",
  timeout = 60,
})

zn.auxiliary("title_generation", {
  provider = "auto",
  model = "",
  timeout = 30,
  max_tokens = 256, -- title is short, save tokens
})

zn.auxiliary("session_search", {
  provider = "auto",
  model = "",
  timeout = 30,
  max_tokens = 1024,
})

-- ── Examples: custom endpoint/credentials for a specific task ──
--
-- Use a different OpenAI-compatible endpoint:
-- zn.auxiliary("compression", {
--   model = "gpt-4o-mini",
--   url = "https://api.openai.com/v1",
--   api_key = "OPENAI_API_KEY",
-- })
--
-- Use a local Ollama instance:
-- zn.auxiliary("compression", {
--   model = "qwen2.5:7b",
--   url = "http://localhost:11434/v1",
-- })
--
-- Use a proxy/reverse-proxy (api_key inherited from active provider):
-- zn.auxiliary("compression", {
--   url = "https://my-proxy.example.com/v1",
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
  -- Kimi / Moonshot
  ["kimi"] = 131072,
  ["moonshot"] = 131072,
  -- StepFun
  ["stepfun"] = 262144,
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
-- this fraction of the model's context window (0.0-1.0, default: 0.33).
-- Set to 0 to disable auto-compaction entirely.
zn.compact_threshold(0.33)

-- ═══════════════════════════════════════════════
-- Paths
-- ═══════════════════════════════════════════════

zn.plugins_dir("~/.config/zeno/plugins")

-- ═══════════════════════════════════════════════
-- Memory
-- ═══════════════════════════════════════════════
-- MEMORY.md is stored globally at ~/.config/zeno/memory/MEMORY.md
-- USER.md is stored globally at ~/.config/zeno/USER.md

zn.memory_char_limit(4000)  -- MEMORY.md character limit (default: 4000)
zn.user_char_limit(2500)    -- USER.md character limit (default: 2500)

-- ── External Memory Providers (Lua script-based) ──────────
--
-- External providers run alongside the built-in MEMORY.md/USER.md store.
-- Only ONE external provider can be active at a time.
-- Configure via script file or inline Lua code.
--
-- zn.memory_provider("mem0", { script = "memory_providers/mem0.lua" })
-- zn.memory_provider("custom", { script = [[inline code]], inline = true })
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
--   on_memory_write   = function(action, target, content)  -- mirror built-in writes
--   shutdown          = function()
--
-- Lifecycle hooks (optional, called by the engine at key events):
--   on_turn_start     = function(turn_number, message)
--   on_session_end    = function(messages_json)          -- session exit/timeout
--   on_session_switch = function(new_id, parent_id, reset) -- /resume, /reset etc.
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
--     on_memory_write = function(action, target, content)
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
--     -- Session ID rotation: /resume, /branch, /reset, context compression
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

-- Or set individually:
-- zn.identity("You are a data engineer specializing in ETL pipelines.")
-- zn.guidelines("- Validate all inputs.\n- Prefer SQL for data queries.")

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
-- if string.find(zn.cwd(), "/home/user/Develop/") then
--   zn.trusted_paths({"/home/user/Develop/"})
-- end

-- Use different provider by hostname
-- if zn.hostname() == "work-laptop" then
--   zn.set_provider("openai")
-- end

-- Use different model by environment variable
-- local model_override = zn.env("ZENO_MODEL")
-- if model_override then
--   zn.set_model(model_override)
-- end

-- ═══════════════════════════════════════════════
-- Finalize
-- ═══════════════════════════════════════════════

return zn.config()
