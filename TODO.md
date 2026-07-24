# TODO — remaining work
<!---->
## DB improvement 
- [ ] handle uploaded binary files (PDF, images) with content extraction
- [ ] documents should be searchable via fulltext (FTS5) in webui and for renco
## Web UI improvements
<!---->
- [ ] Support image attachments in chat (drag-and-drop, paste)
- [ ] Dark/light theme toggle
- [ ] Keyboard shortcuts
- [ ] Desktop notifications for responses
<!---->
## TUI improvements
<!---->
- [ ] Mouse-based scroll gesture support
- [ ] Multi-line selection copy to system clipboard
<!---->
## MCP tools
<!---->
- [ ] `image-viewer` for models that are not image capable
- [ ] Search engine rate-limit handling and retries
<!---->
## Plugin / cron system
<!---->
- [ ] Workout tracking plugin (`workout.md` state file, daily check-in)
- [ ] Calendar-aware cron (Nextcloud CalDAV integration)
- [ ] Cron job history and notification on completion
<!---->
## Code indexing
<!---->
- [ ] Tree-sitter integration for language-aware code search
- [ ] File-watcher for automatic reindexing
<!---->
## Misc
<!---->
- [ ] Remove dead local-llama code in agent.rs (set_local_endpoint, router_model field)
- [ ] Session export (JSON/Markdown)
- [ ] Multi-agent collaboration (planner→executor→reviewer pipeline already exists)
