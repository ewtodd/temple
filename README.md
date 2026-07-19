# temple — renco's agent harness

**WARNING: Experimental local-AI agent harness. Not production software.**
It runs LLM agents that execute shell commands and read/write files on your
system. Use at your own risk. Do not expose to untrusted networks. Do not use
this — it's an experiment in local AI for exactly one person's hardware fleet.

---

## What is this?

`temple` is an always-on, server/client AI agent harness. **renco** is the
agent — a persistent coding assistant that lives on your own hardware and
talks to models you host yourself.

```
┌─────────────────────────────────────────────────────────┐
│                  temple-server (daemon)                  │
│  • WebSocket API for clients                             │
│  • Agent loop with real tool use (files, shell, MCP)     │
│  • MCP tools via litellm (fetch, searxng, nixos, arxiv,  │
│    context7)                                             │
│  • Permission system (per-session, user-prompted)        │
│  • SQLite memory + skills + personality                  │
│  • Warm-model keep-alive (llamaswap never unloads)       │
│  • Cron: skills extraction, smart flake updates          │
│  • ntfy two-way, Nextcloud calendar                      │
└──────────────────────────────────────────────────────────┘
        ▲ WebSocket (JSON protocol)
        │
┌───────┴────────┐  ┌────────────────┐
│ temple (TUI)   │  │ temple (TUI)   │
│ iocraft, fast  │  │ another host   │
└────────────────┘  └────────────────┘
        │
        ▼
   litellm proxy → models on your GPUs
   (DeepSeek on Strix Halo, Qwen on Radeon, Gemma on 4090…)
```

## Quick start

```bash
nix build                    # both binaries in result/bin/
result/bin/temple-server     # daemon (needs LITELLM_API_KEY or LITELLM_MASTER_KEY)
result/bin/temple            # TUI client, another terminal
```

## Client commands

| Input | Action |
|---|---|
| `/models` | list models from litellm |
| `/model NAME` | switch session model |
| `/mode MODE` | permission mode: `default` `ask` `lockdown` `yolo` |
| `/help` | help |
| `:q` | quit |
| `Ctrl+G` | open prompt in `$EDITOR` |
| `Ctrl+L` | clear chat |
| `Ctrl+U/D`, `PgUp/PgDn` | scroll |
| `↑` `↓` | scroll one line |

Each assistant reply gets a stats footer:

```
renco › Done — fixed the borrow error in main.rs.
       ⏱ qwen3.6-27b-coding · 41.2 tok/s · 12.4k ctx · 12.4k+890 tok · 21.6s
```

## NixOS module

```nix
# flake.nix
inputs.temple.url = "github:ewtodd/temple";

# configuration.nix
imports = [ inputs.temple.nixosModules.temple-server ];

services.temple-server = {
  enable = true;
  listen = "0.0.0.0:42123";
  openFirewall = true;               # LAN clients connect directly
  litellmUrl = "https://llm.example.com";
  defaultModel = "deepseek-v4-flash-high";
  environmentFile = "/run/agenix/litellm-master-key";  # LITELLM_API_KEY=... or LITELLM_MASTER_KEY=...
  allowedDirs = [ "/etc/nixos" "/home" ];
  ntfy.enable = true;                # optional two-way channel
};
```

The module generates a complete config.toml from these options, runs the
daemon as a hardened systemd service (dedicated `temple` user,
`ProtectSystem=full`, state in `/var/lib/temple`), and opens the firewall port
if requested.

**Secret note:** `environmentFile` just needs `LITELLM_API_KEY=sk-...` or
`LITELLM_MASTER_KEY=sk-...` on one line. Any existing litellm master-key
secret file works. Make sure the `temple` user can read it (agenix
`mode`/`group`).

## Shell completions

Installed by the nix package for bash, zsh, fish. Manually:

```bash
temple --generate-completions bash
temple-server --generate-completions zsh
```

## Architecture notes

- **Server**: Rust, tokio, WebSocket JSON protocol. One daemon serves many
  clients. Sessions hold conversation history; tools run with per-session
  permission scopes.
- **Tools**: local (`read_file`, `write_file`, `list_dir`, `execute_command`)
  run with permission checks; everything else routes through litellm's MCP
  endpoints (`/v1/mcp/tools` for discovery, `/mcp/{server}` JSON-RPC for
  execution).
- **Keep-alive**: the daemon pings the primary model every 30s so llamaswap
  keeps it resident — no cold-start latency when you actually need it.
- **Cron**: 03:00 daily skills extraction → `skills.md`; 04:00 smart
  `nix flake update` (checks llama.cpp commits for regression keywords via
  GitHub compare API, reverts flake.lock if risky); Sunday 05:00 maintenance.

## License

MIT
