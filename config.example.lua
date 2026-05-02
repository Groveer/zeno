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
  api_key_env = "ANTHROPIC_API_KEY",
  base_url = "https://api.anthropic.com",
  default_model = "claude-sonnet-4-20250514",
})

-- OpenAI compatible
zn.provider("openai", {
  api_key_env = "OPENAI_API_KEY",
  base_url = "https://api.openai.com/v1",
  default_model = "gpt-4o",
})

-- Custom OpenAI-compatible endpoint (e.g. Groveer, DeepSeek, local Ollama)
-- zn.provider("groveer", {
--   api_key_env = "GROVEER_API_KEY",
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
-- zn.web_search({ provider = "brave", api_key_env = "BRAVE_API_KEY" })
-- zn.web_search({ provider = "tavily", api_key_env = "TAVILY_API_KEY" })
-- zn.web_search({ provider = "duckduckgo" })

-- ═══════════════════════════════════════════════
-- MCP Servers
-- ═══════════════════════════════════════════════

-- zn.mcp_server("context7", {
--   command = { "npx", "-y", "@upstreamapi/context7" },
-- })

-- zn.mcp_server("my-server", {
--   url = "http://localhost:3000",
-- })

-- ═══════════════════════════════════════════════
-- Auxiliary models (cheaper models for specific tasks)
-- ═══════════════════════════════════════════════

zn.auxiliary("compression", {
  provider = "auto",
  model = "", -- "" = inherit from main model
  timeout = 30,
})

zn.auxiliary("vision", {
  provider = "auto",
  model = "", -- "" = inherit from main model
  timeout = 30,
})

zn.auxiliary("web_extract", {
  provider = "auto",
  model = "", -- "" = inherit from main model
  timeout = 60,
})

zn.auxiliary("title_generation", {
  provider = "auto",
  model = "",
  timeout = 30,
})

zn.auxiliary("session_search", {
  provider = "auto",
  model = "",
  timeout = 30,
})

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

-- External memory providers (Lua script-based)
-- zn.memory_provider("mem0", { script = "mem0.lua" })  -- external script
-- zn.memory_provider("custom", { script = [[inline code]], inline = true })  -- inline

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
