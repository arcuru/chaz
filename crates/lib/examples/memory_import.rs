//! Standalone trusted native consumer: public Eidetica authentication and typed
//! import data only. No Chaz Server, ToolContext, agent/bank DBs or model runtime.
use anyhow::{Result, ensure};
use chaz_core::extension::memory_import::{
    MemoryImportScope, MemoryImportSnapshot, NativeMemoryImport,
};
use eidetica::Instance;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    scope: MemoryImportScope,
    snapshot: MemoryImportSnapshot,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    ensure!(
        args.len() == 4,
        "usage: memory_import ENDPOINT ACCOUNT REQUEST.json (MEMORY_IMPORT_PASSWORD required)"
    );
    let request: Request = serde_json::from_slice(&std::fs::read(&args[3])?)?;
    let instance = Instance::connect(&args[1]).await?;
    let mut user = instance
        .login_account(&args[2], &std::env::var("MEMORY_IMPORT_PASSWORD")?)
        .await?;
    let mut importer = NativeMemoryImport::bind(&mut user, request.scope).await?;
    let changes = importer.replace_snapshot(&request.snapshot).await?;
    println!(
        "inserted={} updated={} removed={} state_changed={}",
        changes.inserted, changes.updated, changes.removed, changes.state_changed
    );
    Ok(())
}
