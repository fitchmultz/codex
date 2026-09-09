use std::sync::Arc;

use crate::compact::InitialContextInjection;
use crate::context::world_state::WorldState;
use crate::hook_runtime::PostCompactHookOutcome;
use crate::hook_runtime::PreCompactHookOutcome;
use crate::hook_runtime::run_post_compact_hooks;
use crate::hook_runtime::run_pre_compact_hooks;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use codex_analytics::CompactionTrigger;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::WarningEvent;
use tokio_util::sync::CancellationToken;

// Leave room for the surrounding context metadata within the native 10K-token item budget.
const MAX_LOCAL_RECOVERY_HINT_BYTES: usize = 32_000;

pub(crate) fn validated_local_recovery_hint(
    result: &CallToolResult,
) -> Result<String, &'static str> {
    if result.is_error == Some(true) {
        return Err("Recovery notes could not be read. Check the notes server and retry.");
    }
    let parts = result
        .content
        .iter()
        .filter_map(|content| content.get("text").and_then(serde_json::Value::as_str))
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>();
    let bytes = parts.iter().map(|part| part.len()).sum::<usize>() + parts.len().saturating_sub(1);
    if parts.is_empty() {
        Err("Recovery notes are empty. Check the notes server and retry.")
    } else if bytes > MAX_LOCAL_RECOVERY_HINT_BYTES {
        Err(
            "Recovery notes exceed the 32,000-byte limit. Shorten the checkpoint and keep full details in history.",
        )
    } else {
        Ok(parts.join("\n"))
    }
}

/// Runs token-budget manual compaction as a normal compaction lifecycle.
///
/// Token-budget compaction skips model/server summarization and installs a fresh context window
/// instead. It is still modeled as compaction so compact hooks and `ContextCompaction` turn items
/// observe the same lifecycle as local or remote compaction.
pub(crate) async fn run_manual_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) -> CodexResult<()> {
    let start_event = EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_context.sub_id.clone(),
        trace_id: turn_context.trace_id.clone(),
        started_at: turn_context.turn_timing_state.started_at_unix_secs().await,
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.mode(),
    });
    sess.send_event(&turn_context, start_event).await;

    // Manual compaction runs outside run_turn, so it captures its own current step.
    let step_context = sess
        .capture_step_context(Arc::clone(&turn_context), &CancellationToken::new())
        .await?;
    let world_state = Arc::new(sess.build_world_state_for_step(&step_context).await?);
    run_compact_task_inner(&sess, &step_context, world_state, CompactionTrigger::Manual).await
}

/// Runs token-budget inline auto-compaction as a normal compaction lifecycle.
///
/// Token-budget compaction skips model/server summarization and installs a fresh context window
/// instead. It is still modeled as compaction so compact hooks and `ContextCompaction` turn items
/// observe the same lifecycle as local or remote compaction.
pub(crate) async fn run_inline_auto_compact_task(
    sess: Arc<Session>,
    step_context: Arc<StepContext>,
    initial_context_injection: InitialContextInjection,
) -> CodexResult<()> {
    let world_state = match initial_context_injection {
        InitialContextInjection::BeforeLastUserMessage { world_state, .. } => world_state,
        InitialContextInjection::DoNotInject => {
            Arc::new(sess.build_world_state_for_step(&step_context).await?)
        }
    };
    run_compact_task_inner(&sess, &step_context, world_state, CompactionTrigger::Auto).await
}

async fn run_compact_task_inner(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    world_state: Arc<WorldState>,
    trigger: CompactionTrigger,
) -> CodexResult<()> {
    let turn_context = &step_context.turn;
    let pre_compact_outcome = run_pre_compact_hooks(sess, turn_context, trigger).await;
    match pre_compact_outcome {
        PreCompactHookOutcome::Continue => {}
        PreCompactHookOutcome::Stopped => return Err(CodexErr::TurnAborted),
    }

    let recovery_hint = if step_context
        .token_budget
        .as_ref()
        .is_some_and(|budget| budget.local_recovery_hook.is_some())
    {
        let result = sess
            .services
            .mcp_runtime
            .latest_call_tool(
                "notes",
                "thread_hint",
                /*environment_id*/ None,
                /*arguments*/ None,
                Some(serde_json::json!({ "threadId": sess.thread_id().to_string() })),
                /*requested_timeout*/ None,
                /*wait_for_server*/ true,
            )
            .await;
        let hint = match result {
            Ok(result) => validated_local_recovery_hint(&result),
            Err(error) => {
                tracing::warn!(%error, "required local recovery hint failed");
                Err("Recovery notes are unavailable. Check the notes server and retry.")
            }
        };
        match hint {
            Ok(text) => Some(text),
            Err(message) => {
                sess.send_event(
                    turn_context,
                    EventMsg::Warning(WarningEvent {
                        message: format!("{message} Context was not reset."),
                    }),
                )
                .await;
                return Err(CodexErr::TurnAborted);
            }
        }
    } else {
        None
    };
    let compaction_item = TurnItem::ContextCompaction(ContextCompactionItem::new());
    sess.emit_turn_item_started(turn_context, &compaction_item)
        .await;
    sess.start_new_context_window(step_context, world_state, recovery_hint)
        .await;
    sess.emit_turn_item_completed(turn_context, compaction_item)
        .await;

    let post_compact_outcome = run_post_compact_hooks(sess, turn_context, trigger).await;
    if let PostCompactHookOutcome::Stopped = post_compact_outcome {
        return Err(CodexErr::TurnAborted);
    }

    Ok(())
}
