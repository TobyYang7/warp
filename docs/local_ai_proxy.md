# Local Warp Modifications

This document describes all modifications made to the Warp OSS codebase to enable:
1. Local AI proxy with DeepSeek integration
2. Local Claude Code integration (`/agent` → `claude`)
3. Skip login / offline mode
4. Custom appearance (app name "Warp")

## Architecture

```
┌──────────────┐     HTTP (localhost:18080)     ┌──────────────────┐
│  Warp App    │ ──────────────────────────────> │  Local Proxy     │
│  (warp-oss)  │                                 │  (axum server)   │
└──────┬───────┘                                 └────────┬─────────┘
       │                                                   │
  ┌────┴──────┐                    ┌───────────────────────┼───────────────┐
  │ /agent    │                    │  /ai/multi-agent      │  /graphql/v2  │
  │ ───>      │                    │  ─────────> DeepSeek  │  ───> stub    │
  │ claude    │                    │                       │               │
  └───────────┘                    │  /client/login        │  /api/v1/*    │
                                   │  ───> 200 OK          │  ───> "{}"    │
                                   └───────────────────────┴───────────────┘
```

## Modified Files

### 1. `app/src/bin/oss.rs` — OSS Entry Point

**What changed:**
- Added `mod local_ai_proxy;` module declaration
- After `ChannelState::set()`, loads config from `~/.warp/setting.json`
- If DeepSeek API key is configured, starts the local proxy in a separate tokio runtime
- The proxy port overrides `ChannelState::server_root_url()` to `http://127.0.0.1:<port>`
- The proxy runtime is kept alive in a background thread until app exit

### 2. `app/src/bin/local_ai_proxy.rs` — New Local Proxy Module

A complete local HTTP server (using axum) that emulates the Warp server API.

**Endpoints:**
| Route | Behavior |
|-------|----------|
| `POST /ai/multi-agent` | Decodes protobuf request, extracts user query, calls DeepSeek chat completions API, streams back ResponseEvent SSE events |
| `POST /ai/passive-suggestions` | Returns empty SSE stream |
| `POST /client/login` | Returns 200 OK (no-op) |
| `POST /graphql/v2` | Routes by `op` query param: `getUser` → stub user with DeepSeek model, `getUserSettings` → stub, others → `{"data":{}}` |
| `GET \| POST /api/v1/*` | Returns `{}` |
| All other paths | Returns 200 OK empty |

**SSE Protocol:**
- Each event: `data: "<base64url-encoded protobuf>"`
- Protobuf messages: `warp_multi_agent_api::ResponseEvent`
- Sequence: `StreamInit` → `ClientActions(CreateTask)` → `ClientActions(AddMessagesToTask)` → `StreamFinished`

**DeepSeek Integration:**
- Uses OpenAI-compatible API format: `POST {base_url}/chat/completions`
- Streams response (SSE), collects full text, sends as single `AgentOutput` message

### 3. `app/src/server/server_api/auth.rs` — Auth Bypass

**What changed:**
- When `skip_login` feature is enabled, `access_token()` returns a mock bearer token instead of failing
- This allows authenticated HTTP requests to be sent to the local proxy
- The mock token is never validated by the real server since the proxy handles all requests

### 4. `app/src/terminal/input/slash_commands/mod.rs` — Claude Code Integration

**What changed:**
- The `/agent` slash command handler now checks for `~/.warp/setting.json`
- If `claude_code_path` is configured (e.g., `"claude"` or a full path):
  - `/agent <query>` → runs `claude "<query>"` directly in the terminal via `try_execute_command`
  - Skips the Warp cloud agent system entirely
- If no config file or no `claude_code_path`, falls back to the default Warp agent view

### 5. `script/run_oss_local.sh` — New Build/Run Script

One-click script to build, bundle, sign, and run Warp OSS with all local modifications.

### 6. `~/.warp/setting.json` — Configuration File

**Location:** `$HOME/.warp/setting.json`

```json
{
  "ai_provider": "deepseek",
  "deepseek_api_key": "sk-...",
  "deepseek_model": "deepseek-v4-flash",
  "deepseek_base_url": "https://api.deepseek.com/v1",
  "claude_code_path": "claude",
  "appearance": {
    "app_name": "Warp",
    "hide_login_screen": true
  }
}
```

**Fields:**
| Field | Required | Description |
|-------|----------|-------------|
| `deepseek_api_key` | For DeepSeek | Your DeepSeek API key |
| `deepseek_model` | Optional | Model name (default: deepseek-v4-flash) |
| `deepseek_base_url` | Optional | API base URL (default: https://api.deepseek.com/v1) |
| `claude_code_path` | For Claude Code | Path to claude binary (`"claude"` for PATH lookup) |
| `appearance.hide_login_screen` | Optional | Skip login screen when true |

## Build & Run

### Quick Script
```bash
./script/run_oss_local.sh
```

### Manual Build
```bash
cd app && cargo bundle --bin warp-oss --features "gui,skip_login"
# Then follow post-bundle steps in script/run_oss_local.sh
```

### Features Used
- `gui` — Enables the terminal GUI (required)
- `skip_login` — Skips Firebase auth / login screen, creates mock user

## Known Limitations

1. **No streaming to UI**: DeepSeek response is collected fully before sending
2. **GraphQL stubs are minimal**: Only essential queries have stubs
3. **No real auth**: Mock bearer token used; cloud features (Drive, sharing) won't work
4. **Cloud agent UI still visible**: Agent panel may show but `/agent` redirects to Claude Code
