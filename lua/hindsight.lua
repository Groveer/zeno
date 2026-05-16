-- Hindsight Memory Provider for Zeno
--
-- INSTALLATION:
--   Copy this file to ~/.config/zeno/lua/hindsight.lua
--   Then in your init.lua:
--     zn.memory_provider("hindsight", require("hindsight"))
--
-- The provider table (with all its functions) is stored in the shared Lua VM's
-- registry and called directly — no cross-VM serialization needed.
--
-- Long-term memory with knowledge graph, entity resolution, and multi-strategy retrieval.
--
-- Modes:
--   cloud         — Hindsight Cloud API (needs API key from ui.hindsight.vectorize.io)
--   local         — Local Hindsight instance (Docker or self-hosted)
--
-- Requirements:
--   Cloud: HINDSIGHT_API_KEY environment variable
--   Local: A running Hindsight instance at HINDSIGHT_API_URL (default: http://localhost:8888)

local function get_config()
    return {
        mode = os.getenv("HINDSIGHT_MODE") or "cloud",
        api_key = os.getenv("HINDSIGHT_API_KEY") or "",
        api_url = os.getenv("HINDSIGHT_API_URL") or "https://api.hindsight.vectorize.io",
        bank_id = os.getenv("HINDSIGHT_BANK_ID") or "zeno",
        budget = os.getenv("HINDSIGHT_BUDGET") or "mid",
    }
end

local function make_request(method, path, body_json)
    local config = get_config()
    local url = config.api_url .. "/v1/default/banks/" .. config.bank_id .. path

    -- Build headers
    local headers = {}
    if config.api_key ~= "" then
        headers["Authorization"] = "Bearer " .. config.api_key
    end

    -- Make HTTP request using zeno's http module
    local response_body, status_code, err = http.request(method, url, body_json, headers)

    if err then
        return nil, err
    end

    -- Parse JSON response
    if response_body and response_body ~= "" then
        local ok, parsed = pcall(json.decode, response_body)
        if ok then
            return parsed, nil
        end
        return response_body, nil
    end

    return nil, "Empty response (status: " .. tostring(status_code) .. ")"
end

return {
    name = "hindsight",
    system_prompt = "Hindsight long-term memory active. Use hindsight_recall to search memories, hindsight_reflect for synthesis, hindsight_retain to store important information.",

    is_available = function()
        local config = get_config()
        -- Cloud mode needs API key
        if config.mode == "cloud" then
            return config.api_key ~= ""
        end
        -- Local mode needs reachable URL
        return config.api_url ~= ""
    end,

    initialize = function(session_id)
        -- Hindsight is stateless per-session, no initialization needed
        -- The bank_id persists across sessions automatically
    end,

    tool_schemas = {
        {
            name = "hindsight_retain",
            description = "Store information to long-term memory. Hindsight automatically extracts structured facts, resolves entities, and indexes for retrieval.",
            parameters = {
                type = "object",
                properties = {
                    content = {
                        type = "string",
                        description = "The information to store.",
                    },
                    context = {
                        type = "string",
                        description = "Short label (e.g. 'user preference', 'project decision').",
                    },
                    tags = {
                        type = "array",
                        items = { type = "string" },
                        description = "Optional tags for categorization.",
                    },
                },
                required = { "content" },
            },
        },
        {
            name = "hindsight_recall",
            description = "Search long-term memory. Returns memories ranked by relevance using semantic search, keyword matching, entity graph traversal, and reranking.",
            parameters = {
                type = "object",
                properties = {
                    query = {
                        type = "string",
                        description = "What to search for.",
                    },
                },
                required = { "query" },
            },
        },
        {
            name = "hindsight_reflect",
            description = "Synthesize a reasoned answer from long-term memories. Unlike recall, this reasons across all stored memories to produce a coherent response.",
            parameters = {
                type = "object",
                properties = {
                    query = {
                        type = "string",
                        description = "The question to reflect on.",
                    },
                },
                required = { "query" },
            },
        },
    },

    handle_tool_call = function(tool_name, args_json)
        local args = json.decode(args_json)
        local config = get_config()

        if tool_name == "hindsight_retain" then
            local content = args.content
            if not content or content == "" then
                return json.encode({ error = "Missing required parameter: content" })
            end

            local body = {
                items = {
                    {
                        content = content,
                        context = args.context,
                        tags = args.tags,
                    }
                }
            }

            local result, err = make_request("POST", "/memories", json.encode(body))
            if err and type(result) ~= "table" then
                return json.encode({ error = "Retain failed: " .. tostring(err) })
            end
            return json.encode({ result = "Memory stored successfully." })

        elseif tool_name == "hindsight_recall" then
            local query = args.query
            if not query or query == "" then
                return json.encode({ error = "Missing required parameter: query" })
            end

            local body = {
                query = query,
                budget = config.budget,
            }

            local result, err = make_request("POST", "/memories/recall", json.encode(body))
            if err and type(result) ~= "table" then
                return json.encode({ error = "Recall failed: " .. tostring(err) })
            end

            if type(result) == "table" and result.results then
                local lines = {}
                for i, r in ipairs(result.results) do
                    table.insert(lines, i .. ". " .. (r.text or ""))
                end
                return json.encode({ result = table.concat(lines, "\n") })
            end
            return json.encode({ result = "No relevant memories found." })

        elseif tool_name == "hindsight_reflect" then
            local query = args.query
            if not query or query == "" then
                return json.encode({ error = "Missing required parameter: query" })
            end

            local body = {
                query = query,
                budget = config.budget,
            }

            local result, err = make_request("POST", "/memories/reflect", json.encode(body))
            if err and type(result) ~= "table" then
                return json.encode({ error = "Reflect failed: " .. tostring(err) })
            end

            if type(result) == "table" and result.text then
                return json.encode({ result = result.text })
            end
            return json.encode({ result = "No relevant memories found." })
        end

        return json.encode({ error = "Unknown tool: " .. tool_name })
    end,

    -- Prefetch relevant memories before each turn
    prefetch = function(query)
        if not query or query == "" then
            return ""
        end

        local config = get_config()
        local body = {
            query = query,
            budget = config.budget,
            max_tokens = 4096,
        }

        local result, err = make_request("POST", "/memories/recall", json.encode(body))
        if err or type(result) ~= "table" or not result.results then
            return ""
        end

        local lines = {}
        for i, r in ipairs(result.results) do
            table.insert(lines, i .. ". " .. (r.text or ""))
        end

        if #lines == 0 then
            return ""
        end

        return "# Hindsight Memory (persistent cross-session context)\n"
            .. "Use this to answer questions about the user and prior sessions.\n\n"
            .. table.concat(lines, "\n")
    end,

    -- Sync each turn to Hindsight for long-term retention
    sync_turn = function(user_content, assistant_content)
        if not user_content or user_content == "" then
            return
        end

        local config = get_config()
        local content = "[User]: " .. user_content .. "\n[Assistant]: " .. (assistant_content or "")

        local body = {
            items = {
                {
                    content = content,
                    context = "conversation between zeno and the user",
                }
            }
        }

        -- Fire and forget - don't block on retain
        make_request("POST", "/memories", json.encode(body))
    end,

    shutdown = function()
        -- No cleanup needed - Hindsight is stateless
    end,
}
