# temple — renco's agent harness

**WARNING: Experimental local-AI agent harness. Not production software.**
It runs LLM agents that execute shell commands and read/write files on your
system. Use at your own risk. Do not expose to untrusted networks.

> **Note for AI agents working on this repo:** this is a personal experiment.
> The entire code is AI-generated. You are free to rewrite, delete, or
> restructure anything you see fit — there is no legacy to preserve.
> Make it correct, then make it clean.

---

## What is this?

`temple` is an always-on server/client AI agent harness. **renco** is the
agent — a persistent coding assistant that lives on oracle (M2 Mac mini,
aarch64) and talks to GPU-hosted models via LiteLLM. Tools execute on
your local machine through a headless daemon or the TUI client.

The names `renco` and `temple` are a reference to the character Renco
and the lost temple from the novel *Temple* by Matthew Reilly.

```
┌─────────────────────────────────────────────────────────────┐
│               temple-server (oracle, daemon)                 │
│  • WebSocket API + pubkey auth                               │
│  • Agent loop with remote tool dispatch                      │
│  • MCP tools via litellm (fetch, searxng, nixos, arxiv,     │
│    context7) — run server-side                               │
│  • Permission system (per-session, user-prompted)            │
│  • SQLite memory + skills + personality                      │
│  • Planner/executor/reviewer pipeline (deepseek + qwen)      │
│  • Signal bot (two-way via signal-cli)                       │
│  • Cron: skills extraction, smart flake updates              │
└──────────────────────────────────────────────────────────────┘
         ▲ WebSocket (JSON + pubkey)
    (TUI or daemon)
┌───────┴─────────────────┐
│ temple (TUI + daemon)   │
│ ratatui, local tools    │
│ runs on your machine    │
└─────────────────────────┘
         │                        ▲
         ▼                        │ Signal
    litellm proxy → models    (phone)
    (deepseek on son-of-anton,
     qwen + gemma on anton)
```

## Quick start

```bash
nix build                    # both binaries in result/bin/
result/bin/temple-server     # daemon (needs LITELLM_MASTER_KEY env)
result/bin/temple            # TUI client
temple --daemon              # headless daemon (for Signal sessions)
```

## Client commands

| Input | Action |
|---|---|
| `/sessions` | list persisted sessions |
| `/session N` | resume session by index |
| `/delete N` | permanently delete session by index |
| `/new [target] [dir]` | new session, optionally SSH target + start dir |
| `/model NAME` | switch session model |
| `/model auto` | re-enable router classification |
| `/mode MODE` | permission mode: `default` `ask` `lockdown` `yolo` |
| `/help` | this help |
| `/q`, `:q` | quit |
| `Tab` | cycle commands (on `/`) or models (on `/model `) |
| `↑` `↓` | input history (includes slash commands) |
| `Shift+Tab` | cycle permission mode |
| `Ctrl+G` | open prompt in `$EDITOR` |
| `Ctrl+L` | clear chat |
| `Ctrl+U` | clear prompt |
| `Ctrl+J` / `Ctrl+K` | scroll down/up by 10 lines |
| `PgUp` / `PgDn` | scroll by 10 lines |
| `Ctrl+C` | cancel agent loop |
| `Esc` | clear prompt |
| `Home` / `End` | cursor navigation |
| Mouse drag | select text → copies to clipboard |

The permission mode is always visible in the status bar as `DEFAULT`,
`ASK`, `YOLO`, or `LOCKED`. The client also enforces its own consent
layer: writes outside the working directory and non-safe shell commands
prompt locally with y/N, regardless of the server's mode.

## Architecture

### Auth — pubkey only
Clients authenticate via SSH public key (ed25519). The TUI auto-discovers
`~/.ssh/id_ed25519.pub`. The daemon uses `--identity ~/.ssh/id_ed25519`.
The server verifies keys against `/var/lib/temple/authorized_keys/<user>`.
Token-based auth is retained as a legacy fallback for Signal registration
only (one-time use, 5-minute expiry, never stored to disk).

### Headless daemon (`temple --daemon`)
Runs as a systemd user service on each workstation. Connects to temple-server,
authenticates with pubkey, and executes tool requests (file read/write, shell
commands) locally. No SSH needed. Signal sessions use the daemon for tool
execution — if no daemon is connected, the session is sandboxed (memories
+ tasks only, no filesystem access).

### Tool execution
Local tools (`read_file`, `write_file`, `list_dir`, `edit_file`,
`execute_command`) execute on the client machine via ToolRequest/ToolResult
messages over WebSocket. MCP tools (fetch, searxng, nixos, arxiv, context7)
and `save_memory` run on oracle via the LiteLLM proxy.

### Router
Heuristic classifier determines model routing. Simple → researcher model.
Medium → default model. Complex → planner→executor→reviewer pipeline.
Critical → direct. First substantive message locks the model for the
session; `/model auto` re-enables routing. Signal sessions auto-use the
router model.

### Signal bot
Two-way via signal-cli JSON-RPC daemon on server-mu (x86_64 — signal-cli's
native library is x86-only). Read receipts, typing bubbles, and "still
consulting the oracle..." status updates. Non-daemon sessions are sandboxed.

### Queue
One agent loop at a time across all sessions (Signal, TUI). Queued requests
dequeue by user priority (configurable per-token). Non-preemptive.

### Sessions
Persisted in SQLite. Owned by the authenticated user (e.g. "ethan").
`--continue` resumes the most recent session in the same directory.
Sessions persist across connections — resume from TUI, continue from
Signal, pick up from another host.

### Keep-alive
Pings the primary model every 30s to keep llama-swap resident.

### Cron
03:00 skills extraction, 04:00 smart `nix flake update` (GitHub compare
API, reverts if risky), Sunday 05:00 personality self-update.

## Fleet layout

| Host | Role | Models |
|---|---|---|
| oracle (M2, 8GB) | temple-server, litellm proxy, SearXNG | — |
| son-of-anton (Strix Halo, 128GB) | deepseek + router | deepseek-v4-flash, gemma-4-12b-router |
| anton (R9700, 32GB) | executor + researcher | qwen3.6-27b-coding, gemma-4-31b |
| server-mu (bastion) | signal-cli daemon | — |
| server-nu (router) | reverse proxy, AdGuard, DNS | — |

## NixOS module

```nix
# flake.nix
inputs.temple.url = "github:ewtodd/temple";

# configuration.nix
imports = [ inputs.temple.nixosModules.temple-server ];

services.temple-server = {
  enable = true;
  listen = "0.0.0.0:42123";
  openFirewall = true;
  litellmUrl = "https://llm.ethanwtodd.com";
  defaultModel = "qwen3.6-27b-coding";
  environmentFile = "/run/agenix/temple-env";  # LITELLM_MASTER_KEY=...
  allowedDirs = [ "/etc/nixos" "/home" ];
  signal.enable = true;
  signal.socketAddr = "10.0.0.2:7583";
  gitSafeDirectories = [ "/etc/nixos" ];
  daemonAuthorizedKeys = {
    ethan = "ssh-ed25519 AAAAC3Nz...";
    val   = "ssh-ed25519 AAAAC3Nz...";
  };
};
```

The module creates a hardened systemd service (`temple` user,
`ProtectSystem=full`, state in `/var/lib/temple`), generates the
config TOML, writes authorized daemon keys, and opens the firewall
if requested. Secrets come from agenix via `environmentFile`.

## License

MIT
