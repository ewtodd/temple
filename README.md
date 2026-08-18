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
persistent coding assistant that runs on your workstation as a single
daemon under its own service account and talks to GPU-hosted models
served by llama-swap on son-of-anton. The TUI connects to the local
daemon; Signal works through the same instance (shared number).
<!---->
The names `renco` and `temple` are a reference to the character Renco from the novel *Temple* by Matthew Reilly.
<!---->
```
┌─────────────────────────────────────────────────────────────┐
│  temple --daemon (single service, own account)               │
│  • Full agent: loop, sessions, session log, cron            │
│  • Local tool execution (fs confined to the session cwd)    │
│  • WebSocket API on 127.0.0.1 + pubkey auth                 │
│  • Per-user session isolation (pubkey owner)                │
│  • Memory bridge to Open WebUI (write-through + recall)     │
│  • Signal presence (shared number)                          │
└─────────────────────────────────────────────────────────────┘
         ▲ WebSocket (127.0.0.1, pubkey auth)
    (TUI clients: e-play / e-work — isolated sessions)
         │                        ▲
         ▼                        │ Signal (phone)
    son-of-anton llama-swap → models
    (deepseek, qwen, gemma)
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
### Agent daemon (`temple --daemon`)
Runs as a single systemd SYSTEM service under its own account (boot-starting, no login).
It hosts the complete agent: loop, session log, tools (executed
in-process, confined to the session working directory), memory bridge,
cron, the WebSocket front on 127.0.0.1, and the Signal presence.
Session isolation is per authenticated TUI client: the pubkey's owner
file names the session owner, so e-play's TUI never sees e-work's
sessions (both live in the same DB). Signal replies label the session
owner.
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
native library is x86-only). The agent daemon connects to its socket and
handles all Signal sessions — DMs per sender, group chats shared.
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
# configuration.nix — the agent daemon on the workstation
imports = [ inputs.temple.nixosModules.temple-daemon ];
#
services.temple-daemon = {
  enable = true;
  serviceUser = "temple";
  modelEndpoints = {
    "qwen3.6-27b" = "http://10.0.0.5:8080/v1";
    "qwen3.6-35b-a3b" = "http://10.0.0.5:8080/v1";
  };
  defaultModel = "qwen3.6-35b-a3b";
  # Shared Signal number (signal-cli on mu).
  signal = {
    enable = true;
    socketAddr = "10.0.0.2:7583";
  };
  openWebUI = {
    enable = true;
    baseUrl = "http://10.0.0.6:8081";
  };
  environmentFile = "/run/agenix/temple-env";  # OPENWEBUI_API_KEY=...
  allowedDirs = [ "/etc/nixos" "/home" ];
  supplementaryGroups = [ "nixconfig" ];
  readWritePaths = [ "/etc/nixos" ];
  gitSafeDirectories = [ "/etc/nixos" ];
  authorizedKeys = {
    e-play = [ "ssh-ed25519 AAAAC3Nz..." ];
    e-work = [ "ssh-ed25519 AAAAC3Nz..." ];
  };
};
```
<!---->
The module creates a hardened systemd service (`temple --daemon --config
/etc/temple/temple-daemon.toml`, boot-starting, no login), the service
account, per-owner pubkey files under `/etc/temple/keys/` (the key file
name is the session owner), and state under `/var/lib/temple/`. Secrets
come from agenix via `environmentFile`. The standalone `temple-server`
binary still exists for the older single-host topology.
<!---->
## License
<!---->
MIT
