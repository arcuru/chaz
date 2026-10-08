use super::*;
use crate::test_support::{
    MockBackend, empty_secrets, fresh_session, permissive_security, tool_context,
};
use crate::tool::{Tool, ToolDescriptor, ToolError, ToolPolicy, ToolRegistry};
use serde_json::Value;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore};

// Represents observation plus a child holding capacity after the observation
// finishes. The blocker lives outside the tool future, like another turn.
struct WaitTool {
    semaphore: Arc<Semaphore>,
    blocker: Arc<Mutex<Option<OwnedSemaphorePermit>>>,
    entered: Arc<Notify>,
    finish: Arc<Notify>,
    fail: bool,
}

impl Tool for WaitTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "job_wait".into(),
            description: "test observation".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    fn execute<'a>(
        &'a self,
        _: Value,
        _: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let permit = self
                .semaphore
                .clone()
                .try_acquire_owned()
                .expect("job observation must yield its turn permit");
            *self.blocker.lock().await = Some(permit);
            self.entered.notify_one();
            self.finish.notified().await;
            if self.fail {
                Err("observation failed".into())
            } else {
                Ok("observed".into())
            }
        })
    }
}

struct EffectTool {
    semaphore: Arc<Semaphore>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl Tool for EffectTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "effect".into(),
            description: "test effect".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    fn execute<'a>(
        &'a self,
        _: Value,
        _: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            assert_eq!(
                self.semaphore.available_permits(),
                0,
                "same-batch effect must reacquire"
            );
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("effect ran".into())
        })
    }
}

#[tokio::test]
async fn job_wait_reacquires_after_success_error_and_policy_timeout() {
    for mode in [
        "success",
        "error",
        "timeout",
        "abort-observation",
        "abort-reacquire",
        "closed",
        "claim-loss",
    ] {
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let blocker = Arc::new(Mutex::new(None));
        let entered = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools = Arc::new(ToolRegistry::new());
        tools.register(WaitTool {
            semaphore: semaphore.clone(),
            blocker: blocker.clone(),
            entered: entered.clone(),
            finish: finish.clone(),
            fail: mode == "error",
        });
        tools.register(EffectTool {
            semaphore: semaphore.clone(),
            calls: effects.clone(),
        });
        let (instance, session) = fresh_session().await;
        let db = session.lock().await.database().clone();
        let claim = if mode == "claim-loss" {
            assert!(
                crate::session::jobs::claim_job(&db, "peer", "agent", "owner")
                    .await
                    .unwrap()
            );
            Some((db.clone(), "owner".into()))
        } else {
            None
        };
        let ctx = tool_context(session, tools);
        let mock = Arc::new(MockBackend::new());
        mock.push_tool_calls([
            ("wait".into(), "job_wait".into(), "{}".into()),
            ("effect".into(), "effect".into(), "{}".into()),
        ]);
        mock.push_text("finished");
        let backend = BackendManager::with_mock(mock.clone(), empty_secrets().await);
        let policies = ToolPolicyRegistry::new(std::collections::HashMap::from([(
            "job_wait".into(),
            ToolPolicy {
                timeout: 1,
                ..Default::default()
            },
        )]));
        let capacity = ExecutionCapacity::new(semaphore.clone(), permit, claim);
        let task = tokio::spawn(async move {
            // Keep the in-memory instance alive for the complete runtime.
            let _instance = instance;
            execute_with_recorder(
                None,
                vec![RuntimeMessage::User("test".into())],
                &backend,
                &permissive_security(),
                &ctx,
                &policies,
                None,
                None,
                Some(capacity),
                ModelCallScope::default(),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        if mode == "abort-observation" {
            task.abort();
            assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        } else {
            if mode == "claim-loss" {
                use eidetica::store::DocStore;
                let txn = db.new_transaction().await.unwrap();
                let mut owner = crate::session::jobs::read_job_owner(&db)
                    .await
                    .unwrap()
                    .unwrap();
                owner.incarnation = "rival".into();
                txn.get_store::<DocStore>("job_owner")
                    .await
                    .unwrap()
                    .set_string("v1", serde_json::to_string(&owner).unwrap())
                    .await
                    .unwrap();
                txn.commit().await.unwrap();
            }
            if mode != "timeout" {
                finish.notify_one();
            }
            // Exceed the tool's policy timeout while capacity stays held by
            // the child: neither reacquisition nor another tool may time out
            // into model continuation without a permit.
            tokio::time::sleep(Duration::from_millis(1100)).await;
            assert_eq!(
                mock.recorded_calls().len(),
                1,
                "{mode}: model cannot continue before acquisition"
            );
            assert_eq!(
                effects.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "{mode}"
            );
            if mode == "abort-reacquire" {
                task.abort();
                assert!(matches!(task.await, Err(error) if error.is_cancelled()));
            } else {
                if mode == "closed" {
                    semaphore.close();
                }
                blocker.lock().await.take();
                let result = tokio::time::timeout(Duration::from_secs(5), task)
                    .await
                    .unwrap()
                    .unwrap();
                if mode == "closed" || mode == "claim-loss" {
                    assert!(result.is_err(), "{mode}: runtime must stop");
                    assert_eq!(mock.recorded_calls().len(), 1);
                    assert_eq!(effects.load(std::sync::atomic::Ordering::SeqCst), 0);
                } else {
                    assert_eq!(result.unwrap().body, "finished");
                    assert_eq!(effects.load(std::sync::atomic::Ordering::SeqCst), 1);
                    let expected = match mode {
                        "error" => "Tool error: observation failed",
                        "timeout" => "Tool timed out after 1 seconds",
                        _ => "observed",
                    };
                    assert!(mock.recorded_calls()[1].messages.iter().any(|message| matches!(message,
                        RuntimeMessage::ToolResult { call_id, content } if call_id == "wait" && content.contains(expected))), "{mode}");
                }
            }
        }
        blocker.lock().await.take();
        // Acquiring the entire capacity after cancellation/return detects
        // both leaked permits and cancelled acquisition queue entries.
        if mode != "closed" {
            let all =
                tokio::time::timeout(Duration::from_secs(5), semaphore.clone().acquire_owned())
                    .await
                    .unwrap()
                    .unwrap();
            drop(all);
            assert_eq!(
                semaphore.available_permits(),
                1,
                "{mode}: no leaked capacity"
            );
        }
    }
}
