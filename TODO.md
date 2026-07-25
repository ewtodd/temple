# TODO — remaining work
<!---->
## Performance (prompt cache & context management)
<!---->
- [x] Cache hit/miss tracking in Usage struct — parse `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` from provider stream chunks, accumulate session-wide, expose in ChatStats
- [x] Build system prompt once per session — pin it and the tool list for the session lifetime so the prefix cache stays hot across turns. Dynamic content (memories, date, skills) appends to the user turn instead
- [x] Tool-result snip/prune — when context nears the window, shorten stale tool results (keep head+tail), then elide entirely with one-liner markers. Both are zero-cost (no summarizer API call)
- [x] Summary compaction — for long sessions: call a small summarizer model to fold the older middle of the session into a `<compaction-summary>` block, keeping a fixed tail budget of recent turns verbatim
- [x] Token estimator — rough heuristic (~4 bytes/token for English) to decide snip/prune before sending oversized requests
- [x] Retry improvements — body-drain deadline on error reads (prevent 502 storms from wedging the retry loop), auth-retry gating (don't hammer dead keys)
<!---->
## Future ideas
<!---->
- [ ] PDF/image OCR extraction for uploaded documents
- [ ] Streaming tool output in TUI (incremental display as tool runs)
- [ ] Voice input via Signal voice notes
- [ ] Multi-user collaborative sessions
- [ ] Web UI: mobile-responsive layout
- [ ] Plugin marketplace or user-contributed cron jobs
- [ ] OAuth2/SSO authentication
- [ ] Scheduled message delivery (send later)
- [ ] Agent-to-agent handoff (delegate subtasks to specialized sub-agents)

(End of file - total 11 lines)
