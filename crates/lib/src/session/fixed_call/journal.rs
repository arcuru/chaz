//! A private, single-binding journal on the serving peer. A controlled replay
//! republishes saved bytes; it is never a new invocation. Locking only covers
//! users of this journal on this machine, not replicated provider elections.

use super::{Answer, Binding, ID, MAX_RECORD_BYTES};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Event {
    Start(ID),
    Save { request: ID, answer: Answer },
}

pub(super) enum State {
    Empty,
    Started,
    Saved(Answer),
}

pub(super) fn provision(path: &Path, binding: &Binding) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("journal needs a directory")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    append(&mut file, binding)?;
    // Keep the name durable too, before the generation becomes usable.
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub(super) struct Journal {
    file: File,
    events: Vec<Event>,
}

impl Journal {
    pub(super) fn open(path: &Path, binding: &Binding) -> Result<Self> {
        // Do not create on open, not even if the operator still has its key.
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .context("original fixed-call journal is unavailable")?;
        ensure!(file.metadata()?.is_file(), "fixed journal is not a file");
        file.try_lock()
            .context("cannot exclusively observe fixed journal")?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_RECORD_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_RECORD_BYTES && bytes.ends_with(b"\n"),
            "incomplete fixed journal; cannot prove prior state"
        );
        let mut lines = std::str::from_utf8(&bytes)?.lines();
        let recorded: Binding =
            serde_json::from_str(lines.next().context("missing fixed journal binding")?)?;
        ensure!(&recorded == binding, "wrong fixed journal binding");
        let events = lines.map(serde_json::from_str).collect::<Result<_, _>>()?;
        Ok(Self { file, events })
    }

    pub(super) fn state(&self, id: &ID) -> Result<State> {
        match self.events.as_slice() {
            [] => Ok(State::Empty),
            [Event::Start(request)] if request == id => Ok(State::Started),
            [Event::Start(start), Event::Save { request, answer }]
                if start == id && request == id =>
            {
                answer.validate()?;
                Ok(State::Saved(answer.clone()))
            }
            _ => anyhow::bail!("fixed journal does not describe this exact call"),
        }
    }

    pub(super) fn start(&mut self, id: &ID) -> Result<()> {
        append(&mut self.file, &Event::Start(id.clone()))
    }

    pub(super) fn save(&mut self, id: &ID, answer: &Answer) -> Result<()> {
        append(
            &mut self.file,
            &Event::Save {
                request: id.clone(),
                answer: answer.clone(),
            },
        )
    }
}

fn append(file: &mut File, value: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    file.write_all(&line)?;
    file.sync_all()?;
    Ok(())
}
