//! Assertions for the direct-child close and complete-generation contract.
use super::*;
use crate::mcp::test_support::Fixture;
use serde_json::json;
use std::sync::Arc;

fn stdio(transport: &Transport) -> &StdioTransport {
    let Transport::Stdio(s) = transport else {
        panic!("stdio fixture")
    };
    s
}

#[tokio::test]
async fn eof_cooperative_child_exits_normally_and_double_close_is_terminal() {
    let f = Fixture::new("eof");
    let t = Transport::new_stdio(&f.config).unwrap();
    f.wait("ready").await;
    let start = Instant::now();
    let (a, b) = tokio::join!(
        t.shutdown(CloseBudget::new()),
        t.shutdown(CloseBudget::new())
    );
    a.unwrap();
    b.unwrap();
    assert!(start.elapsed() < CLOSE_PHASE);
    assert!(f.has("eof"));
    assert!(!f.has("term"));
    assert!(stdio(&t).exit_success.load(Ordering::Relaxed));
    assert!(t.process_id().await.is_none());
    assert!(t.restart("fixture").await.unwrap_err().contains("closed"));
    assert_eq!(f.spawns(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn term_cooperative_child_gets_eof_grace_then_term_and_is_reaped() {
    let f = Fixture::new("term");
    let t = Transport::new_stdio(&f.config).unwrap();
    f.wait("ready").await;
    let start = Instant::now();
    t.shutdown(CloseBudget::new()).await.unwrap();
    assert!(start.elapsed() >= CLOSE_PHASE);
    assert!(start.elapsed() < CLOSE_PHASE * 2);
    assert!(f.has("eof") && f.has("term"));
    assert!(stdio(&t).exit_success.load(Ordering::Relaxed));
    assert!(t.process_id().await.is_none());
}

#[tokio::test]
async fn eof_and_term_ignoring_child_is_forced_and_reaped() {
    let f = Fixture::new("ignore");
    let t = Transport::new_stdio(&f.config).unwrap();
    f.wait("ready").await;
    let start = Instant::now();
    t.shutdown(CloseBudget::new()).await.unwrap();
    assert!(start.elapsed() >= CLOSE_PHASE * if cfg!(unix) { 2 } else { 1 });
    assert!(start.elapsed() < CLOSE_PHASE * if cfg!(unix) { 3 } else { 2 });
    assert!(f.has("eof"));
    assert!(!stdio(&t).exit_success.load(Ordering::Relaxed));
    assert!(t.process_id().await.is_none());
}

#[tokio::test]
async fn wait_failure_retains_retryable_child_and_blocks_restart() {
    let f = Fixture::new("term");
    let t = Transport::new_stdio(&f.config).unwrap();
    f.wait("ready").await;
    let pid = t.process_id().await;
    stdio(&t).failure.store(1, Ordering::Relaxed);
    assert!(
        t.shutdown(CloseBudget::new())
            .await
            .unwrap_err()
            .contains("wait error")
    );
    assert_eq!(t.process_id().await, pid);
    assert!(t.restart("fixture").await.is_err());
    assert_eq!(f.spawns(), 1);
    stdio(&t).failure.store(0, Ordering::Relaxed);
    t.shutdown(CloseBudget::new()).await.unwrap();
    assert!(t.process_id().await.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn signal_failure_returns_incomplete_and_retains_child_for_retry() {
    let f = Fixture::new("term");
    let t = Transport::new_stdio(&f.config).unwrap();
    f.wait("ready").await;
    let pid = t.process_id().await;
    stdio(&t).failure.store(2, Ordering::Relaxed);
    assert!(
        t.shutdown(CloseBudget::new())
            .await
            .unwrap_err()
            .contains("TERM error")
    );
    assert_eq!(t.process_id().await, pid);
    assert!(!f.has("term"));
    stdio(&t).failure.store(0, Ordering::Relaxed);
    t.shutdown(CloseBudget::new()).await.unwrap();
    assert!(t.process_id().await.is_none());
}

#[tokio::test]
async fn generation_lock_is_inside_the_close_budget_and_retry_keeps_ownership() {
    let f = Fixture::new("eof");
    let t = Transport::new_stdio(&f.config).unwrap();
    f.wait("ready").await;
    let lock = stdio(&t).generation.lock().await;
    let start = Instant::now();
    assert!(
        t.shutdown(CloseBudget::new())
            .await
            .unwrap_err()
            .contains("lock timed out")
    );
    assert!(start.elapsed() < CLOSE_PHASE * if cfg!(unix) { 4 } else { 3 });
    assert!(lock.as_ref().unwrap().child.id().is_some());
    assert!(stdio(&t).ensure_open().is_err());
    drop(lock);
    t.shutdown(CloseBudget::new()).await.unwrap();
    assert!(t.process_id().await.is_none());
}

#[tokio::test]
async fn close_cancellation_retains_generation_for_the_next_close() {
    let f = Fixture::new("term");
    let t = Transport::new_stdio(&f.config).unwrap();
    f.wait("ready").await;
    {
        let close = t.shutdown(CloseBudget::new());
        tokio::pin!(close);
        tokio::select! {
            result = &mut close => panic!("close returned before TERM grace: {result:?}"),
            _ = f.wait("eof") => {}
        }
    }
    assert!(t.process_id().await.is_some());
    assert!(stdio(&t).ensure_open().is_err());
    t.shutdown(CloseBudget::new()).await.unwrap();
    assert!(t.process_id().await.is_none());
}

#[tokio::test]
async fn close_interrupts_a_reader_before_waiting_for_its_generation_lock() {
    let f = Fixture::new("hang-init");
    let t = Arc::new(Transport::new_stdio(&f.config).unwrap());
    let peer = t.clone();
    let reader = tokio::spawn(async move {
        peer.send_request(
            "fixture",
            &json!({"id":1,"method":"initialize"}),
            1,
            &AtomicBool::new(false),
        )
        .await
    });
    f.wait("initialize").await;
    let start = Instant::now();
    t.shutdown(CloseBudget::new()).await.unwrap();
    assert!(start.elapsed() < CLOSE_PHASE);
    assert!(reader.await.unwrap().unwrap_err().contains("interrupted"));
    assert!(f.has("eof"));
    assert!(t.process_id().await.is_none());
}

#[tokio::test]
async fn close_during_restart_backoff_never_spawns_a_replacement() {
    let f = Fixture::new("eof");
    let t = Transport::new_stdio(&f.config).unwrap();
    f.wait("ready").await;
    let restart = t.restart("fixture");
    tokio::pin!(restart);
    tokio::select! {
        result = &mut restart => panic!("restart finished before backoff: {result:?}"),
        _ = f.wait("eof") => {}
    }
    t.begin_close();
    assert!(restart.await.is_err());
    t.shutdown(CloseBudget::new()).await.unwrap();
    assert_eq!(f.spawns(), 1);
}

#[tokio::test]
async fn cancelled_prepublication_restart_cannot_mix_child_and_pipes() {
    let f = Fixture::new("eof");
    let t = Transport::new_stdio(&f.config).unwrap();
    f.wait("ready").await;
    let lock = stdio(&t).generation.lock().await;
    {
        let restart = t.restart("fixture");
        tokio::pin!(restart);
        // Poll through admission up to the held generation lock, then cancel.
        assert!(futures::poll!(&mut restart).is_pending());
    }
    assert!(stdio(&t).ensure_open().is_err());
    assert_eq!(f.spawns(), 1);
    assert!(lock.as_ref().unwrap().stdin.is_some());
    drop(lock);
    t.shutdown(CloseBudget::new()).await.unwrap();
}

#[tokio::test]
async fn replacement_reaps_old_generation_and_publishes_a_complete_new_one() {
    let f = Fixture::new("eof");
    let t = Transport::new_stdio(&f.config).unwrap();
    f.wait("ready").await;
    let old = t.process_id().await.unwrap();
    let (a, _b) = tokio::join!(t.restart("fixture"), t.restart("fixture"));
    a.unwrap();
    assert_ne!(t.process_id().await.unwrap(), old);
    assert!(f.has("eof"));
    let response = t
        .send_request(
            "fixture",
            &json!({"id":1,"method":"initialize"}),
            1,
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
    assert_eq!(response["serverInfo"]["name"], "fixture");
    assert_eq!(f.spawns(), 2);
    t.shutdown(CloseBudget::new()).await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn wrapper_descendant_is_excluded_and_inherited_stdout_does_not_delay_direct_reap() {
    // Process-local test cleanup only. Production never becomes a subreaper,
    // signals descendants, or claims that wrappers contain their process trees.
    struct Subreaper(libc::c_int);
    impl Drop for Subreaper {
        fn drop(&mut self) {
            // SAFETY: scalar prctl argument; restore this test process's setting.
            unsafe {
                libc::prctl(libc::PR_SET_CHILD_SUBREAPER, self.0, 0, 0, 0);
            }
        }
    }
    let mut previous = 0;
    // SAFETY: GET writes to a valid int; SET changes only this test process.
    unsafe {
        assert_eq!(
            libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut previous, 0, 0, 0),
            0
        );
        assert_eq!(libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0), 0);
    }
    let _subreaper = Subreaper(previous);
    let f = Fixture::new("wrapper");
    let t = Transport::new_stdio(&f.config).unwrap();
    f.wait("descendant").await;
    let pid: libc::pid_t = std::fs::read_to_string(f.dir.path().join("descendant"))
        .unwrap()
        .parse()
        .unwrap();
    struct Descendant(libc::pid_t);
    impl Drop for Descendant {
        fn drop(&mut self) {
            // SAFETY: this exact fixture PID is retained until reaped here.
            unsafe {
                libc::kill(self.0, libc::SIGKILL);
                libc::waitpid(self.0, std::ptr::null_mut(), 0);
            }
        }
    }
    let _descendant = Descendant(pid);
    let start = Instant::now();
    t.shutdown(CloseBudget::new()).await.unwrap();
    assert!(start.elapsed() < CLOSE_PHASE);
    assert!(t.process_id().await.is_none());
    assert!(f.has("eof"));
    // SAFETY: signal 0 tests existence without delivering a signal.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "transport must not silently kill descendants"
    );
}
