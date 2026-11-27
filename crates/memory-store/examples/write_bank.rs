//! Minimal external writer. Run with a single-key bundle provisioned for the bank.

#[cfg(unix)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use std::{fs::File, io, path::Path};

    use anyhow::{Context, ensure};
    use chaz_memory_store::{MemoryUpsert, apply_batch};
    use eidetica::{
        ID,
        account::{AccountSigner, CredentialBundle},
    };
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Batch {
        upserts: Vec<MemoryUpsert>,
        deletes: Vec<String>,
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let [socket, bank_id, bundle_file, batch_file] = args.as_slice() else {
        anyhow::bail!(
            "usage: write_bank SOCKET_PATH BANK_ID BUNDLE_JSON BATCH_JSON < password-file"
        );
    };
    ensure!(
        Path::new(socket).is_absolute(),
        "expected an absolute Unix socket path"
    );
    let bank_id = ID::parse(bank_id)?;
    let bundle: CredentialBundle = serde_json::from_reader(File::open(bundle_file)?)?;
    let batch: Batch = serde_json::from_reader(File::open(batch_file)?)?;
    // Use a protected secret file/descriptor, not an argument or a Chaz login.
    let password = io::read_to_string(io::stdin())?;
    ensure!(
        !password.is_empty(),
        "expected the bundle password on stdin"
    );
    let keys = bundle
        .unlock_for(&bank_id, &password)
        .context("unlock bank writer bundle")?;
    let [key_id] = keys
        .key_ids()
        .try_into()
        .ok()
        .context("expected one bank writer key")?;
    let signer = AccountSigner::new(bank_id.clone(), keys.signer(&key_id)?);
    let connection = signer.connect(format!("unix://{socket}")).await?;
    let bank = signer.open_database(&connection, &bank_id).await?;
    ensure!(
        bank.current_permission().await?.can_write(),
        "bank Write grant required"
    );
    let changes = apply_batch(&bank, &batch.upserts, &batch.deletes).await?;
    println!(
        "inserted={} updated={} removed={}",
        changes.inserted, changes.updated, changes.removed
    );
    Ok(())
}

#[cfg(not(unix))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("write_bank requires a Unix service socket");
}
