```
   █████████╗███████╗███╗   ███╗██████╗ ██╗     ███████╗
    ╚══██╔══╝██╔════╝████╗ ████║██╔══██╗██║     ██╔════╝
       ██║   █████╗  ██╔████╔██║██████╔╝██║     █████╗
       ██║   ██╔══╝  ██║╚██╔╝██║██╔═══╝ ██║     ██╔══╝
       ██║   ███████╗██║ ╚═╝ ██║██║     ███████╗███████╗
       ╚═╝   ╚══════╝╚═╝     ╚═╝╚═╝     ╚══════╝╚══════╝
C A R R Y   T H E   S P I R I T   O F   T H E   P E O P L E
```
<!---->
# temple — renco's agent harness
<!---->
**WARNING: Experimental local-AI agent harness. Not production software.**
It runs LLM agents that execute shell commands and read/write files on your
system. Use at your own risk. Do not expose to untrusted networks.
<!---->
**TO DO:** Audit the code myself to clean up small bugs and finally make it "production" ready - at least for me.
<!---->
---
<!---->
## What is this?
<!---->
`temple` is an always-on agent harness. **renco** is the agent — a
persistent coding assistant that runs on your workstation(s) as a
per-user daemon and talks to GPU-hosted models served by llama-swap on
son-of-anton. The TUI connects to the local daemon; Signal works through
the daemon that owns the shared number.
<!---->
The names `renco` and `temple` are a reference to the character Renco from the novel *Temple* by Matthew Reilly.
<!---->
```
┌─────────────────────────────────────────────────────────────┐
│  temple --daemon (per user, e.g. e-play on e-desktop)        │
│  • Full agent: loop, sessions, session log, cron            │
│  • Local tool execution (fs confined to the session cwd)    │
│  • WebSocket API on 127.0.0.1 + pubkey auth                 │
│  • Memory bridge to Open WebUI (write-through + recall)     │
│  • Signal presence (shared number, one owning daemon)       │
└─────────────────────────────────────────────────────────────┘
         ▲ WebSocket (127.0.0.1, pubkey auth)
    (TUI client)
         │
         ▼
    son-of-anton llama-swap → models
    (deepseek, qwen, gemma, bge-m3 embeddings)
```
<!---->
## Quick start
<!---->
```bash
nix build                    # binaries in result/bin/
result/bin/temple --daemon --config /etc/temple/daemon.toml  # full agent daemon
result/bin/temple            # TUI client (connects to 127.0.0.1:42123 by default)
```
<!---->
## Client commands
<!---->
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
<!---->
The permission mode is always visible in the status bar as `DEFAULT`,
`ASK`, `YOLO`, or `LOCKED`. The client also enforces its own consent
layer: writes outside the working directory and non-safe shell commands
prompt locally with y/N, regardless of the server's mode.
<!---->
## Architecture
<!---->
### Auth — pubkey only
Clients authenticate via SSH public key (ed25519).
The TUI auto-discovers
`~/.ssh/id_ed25519.pub` and sends it on session open; the daemon verifies
it against the key files in its `authorized_keys_dir` (per-user keyed).
Token-based auth is retained as a legacy fallback for Signal registration
only (one-time use, 5-minute expiry, never stored to disk).
<!---->
### Per-user agent daemon (`temple --daemon`)
Runs as a systemd SYSTEM service per user (boot-starting, no login).
Each daemon hosts the complete agent: loop, session log, tools (executed
in-process, confined to the session working directory), memory bridge,
cron, and the WebSocket front on 127.0.0.1. The TUI connects to the local
daemon. Exactly one daemon (the Signal owner) connects to the shared
signal-cli socket.
<!---->
### Tool execution
All tools execute in the agent process:
fs tools (`read_file`, `write_file`, `list_dir`, `edit_file`,
`execute_command`) are confined to the session cwd (reads may touch
/tmp), and the permission modes gate risky operations.
Web tools (fetch, searxng, nixos, arxiv, context7) are direct HTTP
calls; `save_memory`/`recall_memory` go through the Open WebUI bridge.
<!---->
### Router
Heuristic classifier determines model routing.
Simple → researcher model.
Medium → default model.
Complex → planner→executor→reviewer pipeline.
Critical → direct.
Routing is per request — every message re-classifies;
`/model NAME` pins a model for the session, `/model auto`
re-enables routing.
Signal sessions auto-use the
router model.
<!---->
### Signal bot
Two-way via signal-cli JSON-RPC daemon on server-mu (x86_64 — signal-cli's
native library is x86-only). The Signal-owning daemon (one per deployment —
the shared number) connects to its socket and handles all Signal sessions.
Read receipts, typing bubbles, and "still
consulting the oracle..." status updates.
<!---->
### Queue
One agent loop at a time across all sessions (Signal, TUI).
Queued requests
dequeue by user priority (configurable per-token).
Non-preemptive.
<!---->
### Sessions
Persisted in SQLite.
Owned by the authenticated user (e.g. "ethan").
`--continue` resumes the most recent session in the same directory.
Sessions persist across connections — resume from TUI, continue from
Signal, pick up from another host.
<!---->
### Cron
03:00 skills extraction, 04:00 smart `nix flake update` (GitHub compare
API, reverts if risky), Sunday 05:00 personality self-update.
<!---->
## NixOS module
<!---->
```nix
# flake.nix
inputs.temple.url = "github:ewtodd/temple";
#
# configuration.nix — per-user agent daemons on the workstation
imports = [ inputs.temple.nixosModules.temple-daemon ];
#
services.temple-daemon = {
  enable = true;
  userDaemons = [ "e-play" "e-work" ];
  modelEndpoints = {
    "qwen3.6-27b" = "http://10.0.0.5:8080/v1";
    "qwen3.6-35b-a3b" = "http://10.0.0.5:8080/v1";
  };
  defaultModel = "qwen3.6-35b-a3b";
  # e-play owns the shared Signal number (signal-cli on mu).
  signal = {
    enable = true;
    owner = "e-play";
    socketAddr = "10.0.0.2:7583";
  };
  openWebUI = {
    enable = true;
    baseUrl = "http://10.0.0.6:8081";
  };
  environmentFile = "/run/agenix/temple-env";  # OPENWEBUI_API_KEY=...
  allowedDirs = [ "/etc/nixos" "/home" ];
  authorizedKeys.e-play = [ "ssh-ed25519 AAAAC3Nz..." ];
};
```
<!---->
The module creates a hardened systemd service per user (`temple --daemon
--config /etc/temple/daemon-<user>.toml`, boot-starting, no login), per-user
state under `/var/lib/temple/<user>/`, and pubkey auth files under
`/etc/temple/keys/`. Secrets come from agenix via `environmentFile`.
The standalone `temple-server` binary (older topology: a single shared
server) still exists for one-host deployments.
<!---->
## License
<!---->
MIT
