use anyhow::Result;
use codex_core::config::TokenBudgetConfig;
use codex_features::Feature;
use codex_protocol::protocol::HookEventName;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_exec_command_call;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::ev_tool_search_call;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::stdio_server_bin;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_mcp_server;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::fs;
use test_case::test_case;

const CANCELLED: &str = "The requested context reset was cancelled";

fn local_recovery() -> TestCodexBuilder {
    let notes_server = stdio_server_bin().expect("local notes test server");
    test_codex()
        .with_model("test-gpt-5.1-codex")
        .with_pre_build_hook(|home| {
            let script = home.join("checkpoint.py");
            fs::write(
                &script,
                "import json, pathlib, sys\njson.load(sys.stdin)\npathlib.Path(__file__).with_suffix('.ran').touch()\nprint(json.dumps({'continue': True}))\n",
            )
            .expect("write checkpoint hook");
            fs::write(
                home.join("hooks.json"),
                json!({"hooks": {"PreCompact": [{"hooks": [{
                    "type": "command", "command": format!("python3 {}", script.display())
                }]}]}}).to_string(),
            )
            .expect("write checkpoint configuration");
        })
        .with_config(move |config| {
            trust_discovered_hooks(config);
            let hook = codex_hooks::list_hooks(codex_hooks::HooksConfig {
                feature_enabled: true,
                config_layer_stack: Some(config.config_layer_stack.clone()),
                ..Default::default()
            })
            .hooks
            .into_iter()
            .find(|hook| hook.event_name == HookEventName::PreCompact)
            .expect("checkpoint hook is discoverable");
            config
                .features
                .enable(Feature::TokenBudget)
                .expect("enable local token-budget mode");
            config.model_context_window = Some(50_000);
            config.model_auto_compact_token_limit = Some(9_000);
            config.token_budget = Some(TokenBudgetConfig {
                local_recovery_hook: Some(hook.key),
                ..Default::default()
            });
            let mut servers = config.mcp_servers.get().clone();
            servers.insert(
                "notes".to_string(),
                serde_json::from_value(json!({"command": notes_server}))
                    .expect("valid notes server configuration"),
            );
            config
                .mcp_servers
                .set(servers)
                .expect("register local notes server");
        })
}

fn window(request: &ResponsesRequest) -> String {
    request
        .message_input_texts("developer")
        .iter()
        .flat_map(|text| text.lines())
        .find_map(|line| line.strip_prefix("Current context window id: "))
        .expect("context window is present")
        .to_string()
}

#[derive(Clone, Copy)]
enum ResetOrder {
    First,
    Last,
}

#[test_case(ResetOrder::First, 100; "reset before failure")]
#[test_case(ResetOrder::Last, 100; "failure before reset")]
#[test_case(ResetOrder::First, 9_500; "failure at automatic threshold")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_command_cancels_only_its_batch_reset(order: ResetOrder, tokens: i64) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let reset = ev_function_call("reset", "new_context", "{}");
    let fail = ev_exec_command_call("failed", "echo failed-command-output; exit 7");
    let calls = match order {
        ResetOrder::First => [reset, fail],
        ResetOrder::Last => [fail, reset],
    };
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("r1"),
                calls[0].clone(),
                calls[1].clone(),
                ev_completed_with_tokens("r1", tokens),
            ]),
            sse(vec![
                ev_response_created("r2"),
                ev_function_call("retry-reset", "new_context", "{}"),
                ev_completed("r2"),
            ]),
            sse(vec![
                ev_response_created("r3"),
                ev_assistant_message("done", "done"),
                ev_completed("r3"),
            ]),
        ],
    )
    .await;
    let test = local_recovery().build_with_auto_env(&server).await?;
    test.submit_turn("Preserve this original instruction while resolving the failed command.")
        .await?;
    test.codex.shutdown_and_wait().await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(window(&requests[0]), window(&requests[1]));
    assert!(requests[1].body_contains_text(CANCELLED));
    assert!(requests[1].body_contains_text("Preserve this original instruction"));
    assert!(
        requests[1]
            .function_call_output("failed")
            .to_string()
            .contains("failed-command-output")
    );
    assert_ne!(
        window(&requests[1]),
        window(&requests[2]),
        "a handled failure must not poison the next step"
    );
    assert!(!requests[2].body_contains_text(CANCELLED));
    assert!(test.codex_home_path().join("checkpoint.ran").exists());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_retry_clears_the_failed_batch_request_and_outcome() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("incomplete"),
                ev_function_call("reset", "new_context", "{}"),
                ev_exec_command_call("failed", "echo retained-before-retry; exit 7"),
            ]),
            sse(vec![
                ev_response_created("retry"),
                ev_function_call("retry-reset", "new_context", "{}"),
                ev_completed("retry"),
            ]),
            sse(vec![
                ev_response_created("done"),
                ev_assistant_message("done-message", "done"),
                ev_completed("done"),
            ]),
        ],
    )
    .await;
    let test = local_recovery()
        .with_config(|config| config.model_provider.stream_max_retries = Some(1))
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("Retain partial results when retrying the model stream.")
        .await?;
    test.codex.shutdown_and_wait().await?;
    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(window(&requests[0]), window(&requests[1]));
    assert!(requests[1].body_contains_text(CANCELLED));
    assert!(
        requests[1]
            .function_call_output("failed")
            .to_string()
            .contains("retained-before-retry")
    );
    assert_ne!(window(&requests[1]), window(&requests[2]));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stock_token_budget_keeps_existing_failed_sibling_behavior() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("r1"),
                ev_function_call("reset", "new_context", "{}"),
                ev_exec_command_call("failed", "exit 7"),
                ev_completed("r1"),
            ]),
            sse(vec![
                ev_response_created("r2"),
                ev_assistant_message("done", "done"),
                ev_completed("r2"),
            ]),
        ],
    )
    .await;
    let test = local_recovery()
        .with_config(|config| {
            config
                .token_budget
                .as_mut()
                .expect("local recovery configures token-budget mode")
                .local_recovery_hook = None;
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("Use the existing token-budget behavior.")
        .await?;
    test.codex.shutdown_and_wait().await?;
    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    assert_ne!(window(&requests[0]), window(&requests[1]));
    assert!(!requests[1].body_contains_text(CANCELLED));
    Ok(())
}

#[test_case("text((await tools.exec_command({cmd: 'exit 7'})).exit_code);"; "nonzero command")]
#[test_case("try { await tools.exec_command({}); } catch(error) { text('caught failed tool'); }"; "caught nested error")]
#[test_case("try { await tools.exec_command('bad argument type'); } catch(error) { text('caught rejected payload'); }"; "caught nested admission error")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_code_mode_cell_cannot_hide_failed_nested_tools(code: &str) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let code = format!("await tools.new_context({{}}); {code} text('outer cell succeeded');");
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("r1"),
                ev_custom_tool_call("cell", "exec", &code),
                ev_completed("r1"),
            ]),
            sse(vec![
                ev_response_created("r2"),
                ev_assistant_message("done", "done"),
                ev_completed("r2"),
            ]),
        ],
    )
    .await;
    let test = local_recovery()
        .with_config(|config| {
            config
                .features
                .enable(Feature::CodeMode)
                .expect("enable code mode");
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("Keep failed nested tool results available.")
        .await?;
    test.codex.shutdown_and_wait().await?;
    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].body_json()["tools"]
            .to_string()
            .contains("new_context")
    );
    assert_eq!(window(&requests[0]), window(&requests[1]));
    assert!(
        requests[1]
            .custom_tool_call_output("cell")
            .to_string()
            .contains("outer cell succeeded")
    );
    assert!(requests[1].body_contains_text(CANCELLED));
    assert!(!test.codex_home_path().join("checkpoint.ran").exists());
    Ok(())
}

#[test_case(r#"{"decision":"block","reason":"rejected tool result"}"#; "block result")]
#[test_case(r#"{"continue":false,"stopReason":"rejected tool result"}"#; "stop feedback")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_tool_use_rejection_cancels_reset(output: &'static str) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("r1"),
                ev_function_call("reset", "new_context", "{}"),
                ev_exec_command_call("command", "echo successful-command"),
                ev_completed("r1"),
            ]),
            sse(vec![
                ev_response_created("r2"),
                ev_assistant_message("done", "done"),
                ev_completed("r2"),
            ]),
        ],
    )
    .await;
    let test = local_recovery()
        .with_pre_build_hook(move |home| {
            let script = home.join("post-tool.py");
            fs::write(
                &script,
                format!("import json, sys\njson.load(sys.stdin)\nprint({output:?})\n"),
            )
            .expect("write post-tool hook");
            let path = home.join("hooks.json");
            let mut hooks: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).expect("read checkpoint configuration"))
                    .expect("valid checkpoint configuration");
            hooks["hooks"]["PostToolUse"] = json!([{"matcher": "^Bash$", "hooks": [{
                "type": "command", "command": format!("python3 {}", script.display())
            }]}]);
            fs::write(path, hooks.to_string()).expect("write post-tool configuration");
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("Keep rejected tool results in the original window.")
        .await?;
    test.codex.shutdown_and_wait().await?;
    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(window(&requests[0]), window(&requests[1]));
    assert!(
        requests[1]
            .function_call_output("command")
            .to_string()
            .contains("rejected tool result")
    );
    assert!(requests[1].body_contains_text(CANCELLED));
    assert!(!test.codex_home_path().join("checkpoint.ran").exists());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_is_error_cancels_reset() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let binary = stdio_server_bin()?;
    let test = local_recovery()
        .with_config(move |config| {
            let mut servers = config.mcp_servers.get().clone();
            servers.insert(
                "rmcp".to_string(),
                serde_json::from_value(json!({
                    "command": binary,
                    "env": {"MCP_TEST_ENABLE_NODE_REPL_JS": "1"},
                    "supports_parallel_tool_calls": true
                }))
                .expect("valid error-producing MCP server configuration"),
            );
            config
                .mcp_servers
                .set(servers)
                .expect("register error-producing MCP server");
        })
        .build_with_auto_env(&server)
        .await?;
    wait_for_mcp_server(&test.codex, "rmcp").await?;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("discover"),
                ev_tool_search_call(
                    "discover-js",
                    &json!({"query": "Run JavaScript in the test Node REPL"}),
                ),
                ev_completed("discover"),
            ]),
            sse(vec![
                ev_response_created("r1"),
                ev_function_call("reset", "new_context", "{}"),
                ev_function_call_with_namespace(
                    "failed-mcp",
                    "mcp__rmcp",
                    "js",
                    r#"{"code":"nodeRepl.fail()"}"#,
                ),
                ev_completed("r1"),
            ]),
            sse(vec![
                ev_response_created("r2"),
                ev_assistant_message("done", "done"),
                ev_completed("r2"),
            ]),
        ],
    )
    .await;
    test.submit_turn("Keep the failed MCP result before starting a new window.")
        .await?;
    test.codex.shutdown_and_wait().await?;
    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[1]
            .tool_search_output("discover-js")
            .to_string()
            .contains("\"js\"")
    );
    assert_eq!(window(&requests[1]), window(&requests[2]));
    let failed_output = requests[2].function_call_output("failed-mcp");
    assert!(
        failed_output
            .to_string()
            .contains("guardian-hidden-failed-result"),
        "actual MCP output: {failed_output}"
    );
    assert!(requests[2].body_contains_text(CANCELLED));
    assert!(!test.codex_home_path().join("checkpoint.ran").exists());
    Ok(())
}
