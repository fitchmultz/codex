# Local Posthorse recovery

This personal-fork patch adds an opt-in local recovery policy to Codex's existing token-budget context reset. It does not enable the account-gated remote history/notes extension, change account eligibility, or replace the desktop app.

The companion adapter and isolated runtime tests live in [pi-posthorse](https://github.com/fitchmultz/pi-posthorse/tree/main/adapters/codex-posthorse). The adapter supplies durable notes, original transcript history, and a checkpoint hook. The runtime continues to own context windows, hook execution, and tool execution.

## Opt-in

`features.token_budget.local_recovery_hook` selects the stable key of a trusted, synchronous `PreCompact` hook, as returned by the app-server's `hooks/list` method. The adapter's test harness discovers the key and creates its own isolated configuration. It does not modify the normal Codex home.

The setting requires token-budget mode and `use_history_notes_extension = false`. An empty key or combination with the remote extension is rejected. Without this setting, the existing token-budget behavior remains in place.

With local recovery selected:

- Manual compaction uses the existing summarization path.
- `new_context` requests a reset after its complete tool batch succeeds. A failed command, MCP error, rejected result, or interrupted sampling attempt cancels that request and preserves the current context. A failed explicit request cannot immediately trigger an automatic reset at the same threshold decision. A later successful request can reset normally.
- Code mode can discover and call `new_context`. Nested tool failures count even when the script catches them.
- Before an automatic or explicit reset, Codex flushes the transcript and requires the selected hook to finish successfully. A missing, disabled, untrusted, unmatched, asynchronous, failed, or timed-out hook stops the reset.
- Codex reads `notes.thread_hint` before changing windows. Missing, failed, or empty recovery text stops the reset. The validated text is used in the new window without a second read.
- Recovery hints have a 32,000-byte UTF-8 limit. Invalid initial hints are omitted with a warning and a pointer to original notes/history; they do not replace saved history. Full records remain available through the adapter.

Failed required recovery leaves the task stopped with an explanation, not a successful reset. Repair the hook, storage, or notes server before retrying. This patch does not promise power-loss durability or infer that a previous external action succeeded.

## Verification and use

The native integration cases are in `codex-rs/core/tests/suite/token_budget_local_recovery.rs`, alongside the manual-summarization case in `token_budget.rs`. Run them with the repository's `just test` runner. The companion adapter exercises the actual app-server, hooks, notes server, and matching code-mode host with scripted local model replies.

Build the CLI and code-mode host from the same revision. Keep tests in a separate `CODEX_HOME`; do not replace the daily desktop runtime based only on a successful build. Controlled tests do not establish real-model behavior, a 500,000-token workload, or desktop compatibility. The PR's verification results state which checks have actually run.
