//! Race-free approval broker bridging agent tool execution and UI/channel confirmation.

use crate::db::Database;
use crate::events::{ApprovalDecision, ApprovalRequest, CoreEvent, EventBus, RiskTier};
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::oneshot;
use uuid::Uuid;

struct PendingEntry {
    request: ApprovalRequest,
    tx: Option<oneshot::Sender<ApprovalDecision>>,
}

struct ApprovalCleanupGuard<'a> {
    broker: &'a ApprovalBroker,
    id: String,
    active: bool,
}

impl<'a> ApprovalCleanupGuard<'a> {
    fn new(broker: &'a ApprovalBroker, id: String) -> Self {
        Self {
            broker,
            id,
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for ApprovalCleanupGuard<'_> {
    fn drop(&mut self) {
        if self.active && self.broker.take_and_remove(&self.id).is_some() {
            self.broker
                .record_resolution(&self.id, ApprovalDecision::Denied);
        }
    }
}

/// Thread-safe, race-free approval coordinator.
///
/// Each pending approval stores `Option<oneshot::Sender<ApprovalDecision>>` inside
/// a `std::sync::Mutex<HashMap<String, PendingEntry>>` so double-resolve or concurrent
/// timeout/resolve races can never double-fire or panic.
pub struct ApprovalBroker {
    pending: Mutex<HashMap<String, PendingEntry>>,
    event_bus: EventBus,
    db: Option<Database>,
    timeout: Duration,
}

impl ApprovalBroker {
    pub fn new(event_bus: EventBus, db: Option<Database>) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            event_bus,
            db,
            timeout: Duration::from_secs(120),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Returns `true` for tools that MUST NEVER be auto-approved, even in `FullAuto` mode.
    pub fn requires_explicit_even_in_full_auto(tool_name: &str) -> bool {
        tool_name == "gmail_send_confirmed"
    }

    /// Request approval for an action.
    ///
    /// - `RiskTier::SafeReadOnly` is immediately approved.
    /// - When `full_auto_mode == true`, actions are auto-approved EXCEPT `gmail_send_confirmed`.
    /// - Otherwise, broadcasts `CoreEvent::ApprovalRequested` and waits for `resolve()` or timeout.
    pub async fn request_approval(
        &self,
        session_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
        reason: &str,
        risk_tier: RiskTier,
        full_auto_mode: bool,
    ) -> ApprovalDecision {
        if risk_tier == RiskTier::SafeReadOnly {
            return ApprovalDecision::Approved;
        }

        if full_auto_mode && !Self::requires_explicit_even_in_full_auto(tool_name) {
            return ApprovalDecision::Approved;
        }

        let id = Uuid::new_v4().to_string();
        let args_json = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());
        let (tx, rx) = oneshot::channel();
        let req_obj = ApprovalRequest {
            id: id.clone(),
            session_id: session_id.to_string(),
            tool_name: tool_name.to_string(),
            command_or_args: args_json.clone(),
            reason: reason.to_string(),
            risk_tier,
            created_at: Utc::now(),
        };

        {
            let mut guard = self.pending.lock().expect("approval broker mutex poisoned");
            guard.insert(
                id.clone(),
                PendingEntry {
                    request: req_obj.clone(),
                    tx: Some(tx),
                },
            );
        }

        if let Some(db) = &self.db {
            let _ = db.create_pending_approval(&id, session_id, tool_name, &args_json, reason);
        }

        self.event_bus
            .publish(CoreEvent::ApprovalRequested(req_obj));

        let mut guard = ApprovalCleanupGuard::new(self, id.clone());

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(decision)) => {
                guard.disarm();
                decision
            }
            Ok(Err(_)) => {
                guard.disarm();
                // Sender was dropped unexpectedly; clean up and deny.
                self.cleanup_without_sender(&id, ApprovalDecision::Denied);
                ApprovalDecision::Denied
            }
            Err(_) => {
                guard.disarm();
                // Timeout elapsed; atomically claim if not yet resolved.
                if self.take_and_remove(&id).is_some() {
                    self.record_resolution(&id, ApprovalDecision::TimedOut);
                    ApprovalDecision::TimedOut
                } else {
                    // Resolved concurrently just as timeout fired; deny conservatively if lost.
                    ApprovalDecision::Denied
                }
            }
        }
    }

    /// Resolve a pending approval request.
    ///
    /// Returns `true` if this call claimed and resolved the entry, or `false` if the ID
    /// was unknown or already resolved (race-free double-resolve guard).
    pub fn resolve(&self, id: &str, decision: ApprovalDecision) -> bool {
        let Some(tx) = self.take_and_remove(id) else {
            return false;
        };
        let _ = tx.send(decision);
        self.record_resolution(id, decision);
        true
    }

    /// Return IDs currently awaiting resolution.
    pub fn pending_ids(&self) -> Vec<String> {
        let guard = self.pending.lock().expect("approval broker mutex poisoned");
        guard.keys().cloned().collect()
    }

    /// Return all pending approval requests sorted by creation time.
    pub fn list_pending_requests(&self) -> Vec<ApprovalRequest> {
        let guard = self.pending.lock().expect("approval broker mutex poisoned");
        let mut reqs: Vec<ApprovalRequest> = guard.values().map(|entry| entry.request.clone()).collect();
        reqs.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        reqs
    }

    fn take_and_remove(&self, id: &str) -> Option<oneshot::Sender<ApprovalDecision>> {
        let mut guard = self.pending.lock().expect("approval broker mutex poisoned");
        let tx = guard.get_mut(id).and_then(|entry| entry.tx.take());
        if tx.is_some() {
            guard.remove(id);
        }
        tx
    }

    fn cleanup_without_sender(&self, id: &str, decision: ApprovalDecision) {
        {
            let mut guard = self.pending.lock().expect("approval broker mutex poisoned");
            guard.remove(id);
        }
        self.record_resolution(id, decision);
    }

    fn record_resolution(&self, id: &str, decision: ApprovalDecision) {
        if let Some(db) = &self.db {
            let decision_str = match decision {
                ApprovalDecision::Approved => "approved",
                ApprovalDecision::Denied => "denied",
                ApprovalDecision::TimedOut => "timed_out",
            };
            let _ = db.resolve_pending_approval(id, "resolved", decision_str);
        }
        self.event_bus.publish(CoreEvent::ApprovalResolved {
            id: id.to_string(),
            decision,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn approval_broker_list_pending_requests_tracks_lifecycle() {
        let bus = EventBus::new(32);
        let broker = Arc::new(ApprovalBroker::new(bus, None));

        assert!(broker.list_pending_requests().is_empty());

        let broker_req = Arc::clone(&broker);
        let handle = tokio::spawn(async move {
            broker_req
                .request_approval(
                    "s1",
                    "terminal",
                    &serde_json::json!({"command": "echo test"}),
                    "test",
                    RiskTier::RequiresApproval,
                    false,
                )
                .await
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        let pending = broker.list_pending_requests();
        assert_eq!(pending.len(), 1);
        let req_id = pending[0].id.clone();
        assert_eq!(pending[0].tool_name, "terminal");

        assert!(broker.resolve(&req_id, ApprovalDecision::Approved));
        let decision = handle.await.unwrap();
        assert_eq!(decision, ApprovalDecision::Approved);
        assert!(broker.list_pending_requests().is_empty());
    }

    #[tokio::test]
    async fn race_free_double_resolve_only_succeeds_once() {
        let bus = EventBus::new(32);
        let mut rx = bus.subscribe();
        let db = Database::open_in_memory().unwrap();
        let broker = Arc::new(ApprovalBroker::new(bus, Some(db)));

        let broker_req = Arc::clone(&broker);
        let handle = tokio::spawn(async move {
            broker_req
                .request_approval(
                    "s1",
                    "terminal",
                    &serde_json::json!({"command": "Remove-Item a.txt"}),
                    "destructive",
                    RiskTier::RequiresApproval,
                    false,
                )
                .await
        });

        let req_id = loop {
            match rx.recv().await.unwrap() {
                CoreEvent::ApprovalRequested(req) => break req.id,
                _ => continue,
            }
        };

        let b1 = Arc::clone(&broker);
        let b2 = Arc::clone(&broker);
        let id1 = req_id.clone();
        let id2 = req_id.clone();

        let (r1, r2) = tokio::join!(
            tokio::spawn(async move { b1.resolve(&id1, ApprovalDecision::Approved) }),
            tokio::spawn(async move { b2.resolve(&id2, ApprovalDecision::Denied) }),
        );

        let first = r1.unwrap();
        let second = r2.unwrap();
        assert_ne!(first, second, "exactly one concurrent resolve must win");
        assert!(broker.pending_ids().is_empty());

        let decision = handle.await.unwrap();
        assert!(matches!(
            decision,
            ApprovalDecision::Approved | ApprovalDecision::Denied
        ));
    }

    #[tokio::test]
    async fn gmail_send_confirmed_requires_approval_even_in_full_auto() {
        let bus = EventBus::new(32);
        let mut rx = bus.subscribe();
        let broker = Arc::new(ApprovalBroker::new(bus, None).with_timeout(Duration::from_secs(5)));

        // Normal mutating tool auto-approves in full_auto_mode = true
        let normal = broker
            .request_approval(
                "s1",
                "terminal",
                &serde_json::json!({"command": "rm foo"}),
                "destructive",
                RiskTier::RequiresApproval,
                true,
            )
            .await;
        assert_eq!(normal, ApprovalDecision::Approved);

        // gmail_send_confirmed MUST still prompt even in full_auto_mode = true
        let broker_gmail = Arc::clone(&broker);
        let gmail_handle = tokio::spawn(async move {
            broker_gmail
                .request_approval(
                    "s1",
                    "gmail_send_confirmed",
                    &serde_json::json!({"to": "boss@example.com", "subject": "Hi"}),
                    "send email",
                    RiskTier::RequiresApproval,
                    true,
                )
                .await
        });

        let req_id = loop {
            match rx.recv().await.unwrap() {
                CoreEvent::ApprovalRequested(req) => {
                    assert_eq!(req.tool_name, "gmail_send_confirmed");
                    break req.id;
                }
                _ => continue,
            }
        };

        assert!(broker.resolve(&req_id, ApprovalDecision::Denied));
        assert_eq!(gmail_handle.await.unwrap(), ApprovalDecision::Denied);
    }

    #[tokio::test]
    async fn timeout_marks_approval_timed_out() {
        let bus = EventBus::new(16);
        let broker = ApprovalBroker::new(bus, None).with_timeout(Duration::from_millis(40));
        let decision = broker
            .request_approval(
                "s1",
                "terminal",
                &serde_json::json!({"command": "rm x"}),
                "destructive",
                RiskTier::RequiresApproval,
                false,
            )
            .await;
        assert_eq!(decision, ApprovalDecision::TimedOut);
        assert!(broker.pending_ids().is_empty());
    }

    #[tokio::test]
    async fn aborted_approval_future_cleans_up_pending_and_broadcasts_denied() {
        let bus = EventBus::new(32);
        let mut rx = bus.subscribe();
        let db = Database::open_in_memory().unwrap();
        let broker = Arc::new(ApprovalBroker::new(bus, Some(db.clone())));

        let broker_req = Arc::clone(&broker);
        let handle = tokio::spawn(async move {
            broker_req
                .request_approval(
                    "s_abort",
                    "terminal",
                    &serde_json::json!({"command": "Remove-Item a.txt"}),
                    "destructive",
                    RiskTier::RequiresApproval,
                    false,
                )
                .await
        });

        let req_id = loop {
            match rx.recv().await.unwrap() {
                CoreEvent::ApprovalRequested(req) => break req.id,
                _ => continue,
            }
        };

        assert_eq!(broker.pending_ids(), vec![req_id.clone()]);
        handle.abort();
        let _ = handle.await;

        assert!(broker.pending_ids().is_empty());
        let resolved = loop {
            match tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                CoreEvent::ApprovalResolved { id, decision } if id == req_id => break decision,
                _ => continue,
            }
        };
        assert_eq!(resolved, ApprovalDecision::Denied);
        assert!(db.list_pending_approvals().unwrap().is_empty());
    }
}
