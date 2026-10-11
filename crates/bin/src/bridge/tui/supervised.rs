//! Shared boundary for owned backend work. Only the terminal owner restores it.
use futures::FutureExt;
use std::{future::Future, panic::AssertUnwindSafe, time::Duration};

pub(super) const READ_TIMEOUT: Duration = Duration::from_secs(10);
tokio::task_local! { static BACKGROUND: (); }

/// Panic hooks run before catch_unwind. Background failures are logged, not
/// printed into (or allowed to restore) an otherwise healthy live terminal.
pub(super) fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if BACKGROUND.try_with(|_| ()).is_ok() {
            tracing::error!("TUI backend worker panicked: {info}");
        } else {
            previous(info);
        }
    }));
}

pub(super) async fn work<T>(future: impl Future<Output = Result<T, String>>) -> Result<T, String> {
    BACKGROUND
        .scope((), async {
            AssertUnwindSafe(future)
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err("backend worker panicked".into()))
        })
        .await
}

pub(super) async fn read<T: Send + 'static>(
    future: impl Future<Output = Result<T, String>> + Send + 'static,
) -> Result<T, String> {
    join_read(Task(tokio::spawn(work(future)))).await
}

pub(super) async fn join_read<T>(mut task: Task<Result<T, String>>) -> Result<T, String> {
    match tokio::time::timeout(READ_TIMEOUT, &mut task.0).await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => Err(format!("backend read worker cancelled or panicked: {e}")),
        Err(_) => Err("backend read timed out; cached data is stale".into()),
    }
}

pub(super) struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        super::restore_terminal();
    }
}

#[derive(Debug)]
pub(super) struct Task<T>(pub tokio::task::JoinHandle<T>);
impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn timeout_panic_and_cancellation_are_errors_and_subsequent_reads_recover() {
        assert!(
            read::<()>(std::future::pending())
                .await
                .unwrap_err()
                .contains("timed out")
        );
        assert!(
            read::<()>(async { panic!("fixture panic") })
                .await
                .unwrap_err()
                .contains("panicked")
        );
        let task = Task(tokio::spawn(std::future::pending::<Result<(), String>>()));
        task.0.abort();
        assert!(join_read(task).await.unwrap_err().contains("cancelled"));
        assert_eq!(read(async { Ok::<_, String>(7) }).await.unwrap(), 7);
    }
}

#[cfg(test)]
#[test]
#[ignore = "requires the controlling PTY supplied by just tui-pty"]
fn background_panic_keeps_the_live_terminal_owned() {
    install_panic_hook();
    let owner = TerminalGuard;
    let mut terminal = super::init_terminal().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        assert!(
            read::<()>(async { panic!("PTY worker fixture") })
                .await
                .is_err()
        );
        assert!(
            crossterm::terminal::is_raw_mode_enabled().unwrap(),
            "background panic restored a live terminal"
        );
        assert!(
            read::<()>(std::future::pending())
                .await
                .unwrap_err()
                .contains("timed out")
        );
        assert!(
            crossterm::terminal::is_raw_mode_enabled().unwrap(),
            "timeout restored a live terminal"
        );
    });
    terminal
        .draw(|f| {
            f.render_widget(
                ratatui::widgets::Paragraph::new("LIVE-AFTER-WORKER-PANIC-AND-TIMEOUT"),
                f.area(),
            )
        })
        .unwrap();
    drop(terminal);
    drop(owner);
    assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
}
