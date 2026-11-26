//! ReAct `ToolLoop` with `LoopGuard` (max 16 iterations, duplicate tool+args >= 3 breaker)
//! and `ApprovalBroker` integration.

use crate::agent::approval::ApprovalBroker;
use crate::agent::classifier::CommandRiskClassifier;
use crate::events::{CoreEvent, EventBus, ReactorState, RiskTier};
use crate::providers::traits::{ChatMessage, LlmProvider, ProviderError, StreamEvent, ToolCall};
use crate::tools::{ToolContext, ToolRegistry, ToolResult};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

/// Default maximum ReAct loop iterations per turn.
pub const DEFAULT_MAX_ITERATIONS: usize = 16;
/// Default duplicate identical `(tool_name, args)` call threshold that trips [`LoopGuard`].
pub const DEFAULT_DUPLICATE_BREAKER: usize = 3;

/// Errors returned by [`ToolLoop`].
#[derive(Debug, Error)]
pub enum LoopError {
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("stream error from provider: {0}")]
    Stream(String),
    #[error("loop guard triggered: reached maximum {0} iterations")]
    MaxIterationsReached(usize),
    #[error("loop guard triggered: duplicate call to '{tool_name}' repeated {count} times")]
    DuplicateToolCall { tool_name: String, count: usize },
}

/// Guards the ReAct loop against runaway iterations and repeated identical tool calls.
#[derive(Debug, Clone)]
pub struct LoopGuard {
    max_iterations: usize,
    max_duplicate_calls: usize,
    iterations: usize,
    call_counts: HashMap<String, usize>,
}

impl LoopGuard {
    pub fn new(max_iterations: usize, max_duplicate_calls: usize) -> Self {
        Self {
            max_iterations,
            max_duplicate_calls,
            iterations: 0,
            call_counts: HashMap::new(),
        }
    }

    /// Advance to the next iteration, returning an error if `max_iterations` is exceeded.
    pub fn next_iteration(&mut self) -> Result<usize, LoopError> {
        self.iterations += 1;
        if self.iterations > self.max_iterations {
            return Err(LoopError::MaxIterationsReached(self.max_iterations));
        }
        Ok(self.iterations)
    }

    /// Record a `(tool_name, arguments)` invocation and trip if repeated `>= max_duplicate_calls`.
    pub fn record_tool_call(
        &mut self,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(), LoopError> {
        let key = Self::hash_call(tool_name, arguments);
        let count = self.call_counts.entry(key).or_insert(0);
        *count += 1;
        if *count >= self.max_duplicate_calls {
            return Err(LoopError::DuplicateToolCall {
                tool_name: tool_name.to_string(),
                count: *count,
            });
        }
        Ok(())
    }

    pub fn reset(&mut self) {
        self.iterations = 0;
        self.call_counts.clear();
    }

    fn hash_call(tool_name: &str, arguments: &serde_json::Value) -> String {
        let canonical = serde_json::to_string(arguments).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(tool_name.as_bytes());
        hasher.update(b"|");
        hasher.update(canonical.as_bytes());
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(digest.len() * 2);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for b in digest {
            hex.push(HEX[(b >> 4) as usize] as char);
            hex.push(HEX[(b & 0x0f) as usize] as char);
        }
        hex
    }
}

impl Default for LoopGuard {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_ITERATIONS, DEFAULT_DUPLICATE_BREAKER)
    }
}

/// Output produced after a completed [`ToolLoop::run`] execution.
#[derive(Debug, Clone)]
pub struct ToolLoopOutcome {
    pub final_text: String,
    pub iterations: usize,
    pub messages: Vec<ChatMessage>,
}

/// Executes a ReAct reasoning + tool execution loop over an [`LlmProvider`].
pub struct ToolLoop {
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    approval_broker: Arc<ApprovalBroker>,
    event_bus: EventBus,
    guard: LoopGuard,
}

impl ToolLoop {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        approval_broker: Arc<ApprovalBroker>,
        event_bus: EventBus,
    ) -> Self {
        Self {
            provider,
            tools,
            approval_broker,
            event_bus,
            guard: LoopGuard::default(),
        }
    }

    pub fn with_guard(mut self, guard: LoopGuard) -> Self {
        self.guard = guard;
        self
    }

    /// Run the ReAct loop starting from `initial_messages` until the LLM finishes without
    /// requesting tools or [`LoopGuard`] trips.
    pub async fn run(
        &mut self,
        initial_messages: Vec<ChatMessage>,
        model: &str,
        ctx: &ToolContext,
    ) -> Result<ToolLoopOutcome, LoopError> {
        self.guard.reset();
        let mut messages = initial_messages;
        let tool_defs = self.tools.definitions();

        loop {
            let iter = self.guard.next_iteration()?;
            self.event_bus.publish(CoreEvent::ReactorStateChanged {
                state: ReactorState::Executing,
            });

            let mut stream = self
                .provider
                .chat_stream(&messages, &tool_defs, model)
                .await?;

            let mut text_buf = String::new();
            let mut pending_calls: Vec<ToolCall> = Vec::new();

            while let Some(ev) = stream.recv().await {
                match ev {
                    StreamEvent::TextDelta(delta) => {
                        text_buf.push_str(&delta);
                        self.event_bus.publish(CoreEvent::TextDelta {
                            session_id: ctx.session_id.clone(),
                            delta,
                        });
                    }
                    StreamEvent::ToolCallComplete(tc) => {
                        pending_calls.push(tc);
                    }
                    StreamEvent::ToolProgress(msg) => {
                        self.event_bus.publish(CoreEvent::TerminalLine {
                            session_id: ctx.session_id.clone(),
                            stream: "tool".to_string(),
                            line: msg,
                        });
                    }
                    StreamEvent::Error(err) => {
                        self.event_bus.publish(CoreEvent::Error {
                            session_id: Some(ctx.session_id.clone()),
                            message: err.clone(),
                        });
                        self.event_bus.publish(CoreEvent::ReactorStateChanged {
                            state: ReactorState::Idle,
                        });
                        return Err(LoopError::Stream(err));
                    }
                    StreamEvent::Done => break,
                }
            }

            if pending_calls.is_empty() {
                messages.push(ChatMessage::assistant(text_buf.clone()));
                self.event_bus.publish(CoreEvent::ReactorStateChanged {
                    state: ReactorState::Idle,
                });
                return Ok(ToolLoopOutcome {
                    final_text: text_buf,
                    iterations: iter,
                    messages,
                });
            }

            messages.push(ChatMessage::assistant_tool_calls(
                text_buf,
                pending_calls.clone(),
            ));

            let mut call_iter = pending_calls.into_iter();
            while let Some(tc) = call_iter.next() {
                if let Err(loop_err) = self.guard.record_tool_call(&tc.name, &tc.arguments) {
                    self.event_bus.publish(CoreEvent::Error {
                        session_id: Some(ctx.session_id.clone()),
                        message: loop_err.to_string(),
                    });
                    self.event_bus.publish(CoreEvent::ReactorStateChanged {
                        state: ReactorState::Idle,
                    });
                    return Err(loop_err);
                }

                self.event_bus.publish(CoreEvent::ToolCallStarted {
                    session_id: ctx.session_id.clone(),
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                });

                let classification = CommandRiskClassifier::classify_tool_call(
                    &tc.name,
                    &tc.arguments,
                    &ctx.workspace_root,
                );

                let result = if classification.tier == RiskTier::RequiresApproval {
                    self.event_bus.publish(CoreEvent::ReactorStateChanged {
                        state: ReactorState::AwaitingApproval,
                    });
                    let decision = self
                        .approval_broker
                        .request_approval(
                            &ctx.session_id,
                            &tc.name,
                            &tc.arguments,
                            &classification.reason,
                            classification.tier,
                            ctx.full_auto_mode,
                        )
                        .await;
                    self.event_bus.publish(CoreEvent::ReactorStateChanged {
                        state: ReactorState::Executing,
                    });

                    if !decision.is_approved() {
                        let err_output = format!(
                            "Action '{}' was not approved ({decision:?}). User denied execution — task stopped.",
                            tc.name
                        );
                        let preview: String = err_output.chars().take(240).collect();
                        self.event_bus.publish(CoreEvent::ToolCallCompleted {
                            session_id: ctx.session_id.clone(),
                            tool_call_id: tc.id.clone(),
                            tool_name: tc.name.clone(),
                            success: false,
                            output_preview: preview,
                        });
                        messages.push(ChatMessage::tool_result(&tc.id, &tc.name, err_output));

                        for rem_tc in call_iter {
                            messages.push(ChatMessage::tool_result(
                                &rem_tc.id,
                                &rem_tc.name,
                                "Skipped because user denied permission.",
                            ));
                        }

                        self.approval_broker.deny_all_for_session(&ctx.session_id);

                        let stop_text = match decision {
                            crate::events::ApprovalDecision::TimedOut => format!(
                                "Approval request for `{}` timed out, so the task was cancelled.",
                                tc.name
                            ),
                            _ => format!(
                                "Permission to run `{}` was denied, so I have stopped this task. Let me know how you would like to proceed.",
                                tc.name
                            ),
                        };
                        self.event_bus.publish(CoreEvent::TextDelta {
                            session_id: ctx.session_id.clone(),
                            delta: stop_text.clone(),
                        });
                        messages.push(ChatMessage::assistant(stop_text.clone()));
                        self.event_bus.publish(CoreEvent::ReactorStateChanged {
                            state: ReactorState::Idle,
                        });
                        return Ok(ToolLoopOutcome {
                            final_text: stop_text,
                            iterations: iter,
                            messages,
                        });
                    } else {
                        self.execute_tool(&tc, ctx).await
                    }
                } else {
                    self.execute_tool(&tc, ctx).await
                };

                let preview: String = result.output.chars().take(240).collect();
                self.event_bus.publish(CoreEvent::ToolCallCompleted {
                    session_id: ctx.session_id.clone(),
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    success: result.success,
                    output_preview: preview,
                });

                messages.push(ChatMessage::tool_result(&tc.id, &tc.name, result.output));
            }
        }
    }

    async fn execute_tool(&self, tc: &ToolCall, ctx: &ToolContext) -> ToolResult {
        match self
            .tools
            .execute(&tc.name, tc.arguments.clone(), ctx)
            .await
        {
            Ok(res) => res,
            Err(err) => ToolResult::err(err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    struct MockProvider {
        turns: Mutex<VecDeque<Vec<StreamEvent>>>,
    }

    impl MockProvider {
        fn new(turns: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                turns: Mutex::new(VecDeque::from(turns)),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        async fn chat_stream(
            &self,
            _messages: &[ChatMessage],
            _tools: &[crate::providers::traits::ToolDefinition],
            _model: &str,
        ) -> Result<mpsc::Receiver<StreamEvent>, ProviderError> {
            let events = self
                .turns
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| vec![StreamEvent::TextDelta("done".to_string()), StreamEvent::Done]);
            let (tx, rx) = mpsc::channel(32);
            tokio::spawn(async move {
                for ev in events {
                    let _ = tx.send(ev).await;
                }
            });
            Ok(rx)
        }
    }

    fn make_ctx(dir: &TempDir, bus: EventBus) -> ToolContext {
        ToolContext {
            session_id: "sess-1".to_string(),
            workspace_root: dir.path().to_path_buf(),
            db: Database::open_in_memory().unwrap(),
            event_bus: bus,
            full_auto_mode: false,
            agy_effort: "medium".to_string(),
            active_model: "mock-model".to_string(),
            agy_model: None,
        }
    }

    #[test]
    fn loop_guard_trips_on_third_duplicate_and_max_iterations() {
        let mut guard = LoopGuard::default();
        let args = serde_json::json!({"path": "Cargo.toml"});
        assert!(guard.record_tool_call("fs_read", &args).is_ok());
        assert!(guard.record_tool_call("fs_read", &args).is_ok());
        let err = guard.record_tool_call("fs_read", &args).unwrap_err();
        assert!(matches!(err, LoopError::DuplicateToolCall { count: 3, .. }));

        let mut iter_guard = LoopGuard::new(2, 5);
        assert_eq!(iter_guard.next_iteration().unwrap(), 1);
        assert_eq!(iter_guard.next_iteration().unwrap(), 2);
        assert!(matches!(
            iter_guard.next_iteration().unwrap_err(),
            LoopError::MaxIterationsReached(2)
        ));
    }

    #[tokio::test]
    async fn tool_loop_executes_safe_tool_and_returns_final_answer() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "jarvis_content").unwrap();

        let provider = Arc::new(MockProvider::new(vec![
            vec![
                StreamEvent::ToolCallComplete(ToolCall {
                    id: "c1".to_string(),
                    name: "fs_read".to_string(),
                    arguments: serde_json::json!({"path": "hello.txt"}),
                }),
                StreamEvent::Done,
            ],
            vec![
                StreamEvent::TextDelta("File contains jarvis_content.".to_string()),
                StreamEvent::Done,
            ],
        ]));

        let bus = EventBus::new(64);
        let ctx = make_ctx(&dir, bus.clone());
        let broker = Arc::new(ApprovalBroker::new(bus.clone(), None));
        let registry = Arc::new(ToolRegistry::with_builtins());

        let mut loop_runner = ToolLoop::new(provider, registry, broker, bus);
        let outcome = loop_runner
            .run(vec![ChatMessage::user("read hello.txt")], "mock", &ctx)
            .await
            .unwrap();

        assert_eq!(outcome.iterations, 2);
        assert_eq!(outcome.final_text, "File contains jarvis_content.");
        assert!(outcome
            .messages
            .iter()
            .any(|m| m.content.contains("jarvis_content")));
    }

    #[tokio::test]
    async fn tool_loop_breaks_on_three_identical_calls() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hi").unwrap();

        let dup_turn = vec![
            StreamEvent::ToolCallComplete(ToolCall {
                id: "c".to_string(),
                name: "fs_read".to_string(),
                arguments: serde_json::json!({"path": "a.txt"}),
            }),
            StreamEvent::Done,
        ];
        let provider = Arc::new(MockProvider::new(vec![
            dup_turn.clone(),
            dup_turn.clone(),
            dup_turn,
        ]));

        let bus = EventBus::new(64);
        let ctx = make_ctx(&dir, bus.clone());
        let broker = Arc::new(ApprovalBroker::new(bus.clone(), None));
        let registry = Arc::new(ToolRegistry::with_builtins());

        let mut loop_runner = ToolLoop::new(provider, registry, broker, bus);
        let err = loop_runner
            .run(vec![ChatMessage::user("loop")], "mock", &ctx)
            .await
            .unwrap_err();

        assert!(matches!(err, LoopError::DuplicateToolCall { count: 3, .. }));
    }

    #[tokio::test]
    async fn denied_delegate_to_agy_stops_immediately_without_re_prompting() {
        let dir = TempDir::new().unwrap();
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                StreamEvent::ToolCallComplete(ToolCall {
                    id: "c1".to_string(),
                    name: "delegate_to_agy".to_string(),
                    arguments: serde_json::json!({"prompt": "fix bug"}),
                }),
                StreamEvent::Done,
            ],
            vec![
                StreamEvent::ToolCallComplete(ToolCall {
                    id: "c2".to_string(),
                    name: "delegate_to_agy".to_string(),
                    arguments: serde_json::json!({"prompt": "retry"}),
                }),
                StreamEvent::Done,
            ],
            vec![
                StreamEvent::ToolCallComplete(ToolCall {
                    id: "c3".to_string(),
                    name: "pty_terminal".to_string(),
                    arguments: serde_json::json!({"action": "open_agy", "prompt": "fix bug"}),
                }),
                StreamEvent::Done,
            ],
        ]));

        let bus = EventBus::new(64);
        let mut rx = bus.subscribe();
        let ctx = make_ctx(&dir, bus.clone());
        let broker = Arc::new(ApprovalBroker::new(bus.clone(), None));
        let registry = Arc::new(ToolRegistry::with_builtins());

        let mut loop_runner = ToolLoop::new(provider, registry, broker.clone(), bus.clone());
        let handle = tokio::spawn(async move {
            loop_runner.run(vec![ChatMessage::user("run agy")], "mock", &ctx).await
        });

        // Wait for ApprovalRequested event for delegate_to_agy
        let req = loop {
            match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await {
                Ok(Ok(CoreEvent::ApprovalRequested(r))) => {
                    assert_eq!(r.tool_name, "delegate_to_agy");
                    break r;
                }
                Ok(_) => {}
                Err(_) => panic!("timed out waiting for ApprovalRequested"),
            }
        };

        broker.resolve(&req.id, crate::events::ApprovalDecision::Denied);

        let outcome = handle.await.unwrap().unwrap();
        assert_eq!(outcome.iterations, 1);
        assert!(outcome.final_text.contains("Permission to run `delegate_to_agy` was denied"));
        assert!(broker.list_pending_requests().is_empty());

        // Verify no second ApprovalRequested event exists in rx
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        while let Ok(ev) = rx.try_recv() {
            if let CoreEvent::ApprovalRequested(r) = ev {
                panic!("unexpected second approval request: {:?}", r);
            }
        }
    }
}
