//! Provision only the disposable store used by the local PTY test.
use eidetica::{Instance, NewUser, backend::database::Sqlite};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("database path required");
    let backend = Sqlite::connect(&format!("sqlite://{path}?mode=rwc")).await?;
    let (instance, user) =
        Instance::create_backend(Box::new(backend), NewUser::passwordless("pty-test")).await?;
    drop(user);
    drop(instance);
    Ok(())
}
