//! Disposable subprocess fixtures; no configured servers or model backends.
use crate::config::McpServerConfig;
use std::path::Path;
use std::time::Duration;

pub(crate) struct Fixture {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) config: McpServerConfig,
}

impl Fixture {
    pub(crate) fn new(mode: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = McpServerConfig {
            name: "fixture".into(),
            command: "python3".into(),
            args: Some(vec![
                "-u".into(),
                "-c".into(),
                SCRIPT.into(),
                mode.into(),
                dir.path().display().to_string(),
            ]),
            env: None,
            url: None,
            default_policy: None,
            startup_timeout_secs: 1,
        };
        Self { dir, config }
    }

    pub(crate) async fn wait(&self, marker: &str) {
        wait_file(&self.dir.path().join(marker)).await;
    }

    pub(crate) fn has(&self, marker: &str) -> bool {
        self.dir.path().join(marker).exists()
    }

    pub(crate) fn spawns(&self) -> usize {
        std::fs::read_to_string(self.dir.path().join("spawns"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}

pub(crate) async fn wait_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fixture marker missing: {}", path.display()));
}

const SCRIPT: &str = r#"
import json, os, pathlib, signal, sys, time
mode, root = sys.argv[1], pathlib.Path(sys.argv[2])
def mark(name): (root / name).write_text(str(os.getpid()))
def term(*_):
    mark('term')
    sys.exit(0)
signal.alarm(30)
signal.signal(signal.SIGTERM, signal.SIG_IGN if mode == 'ignore' else term)
with (root / 'spawns').open('a') as f: f.write(str(os.getpid()) + '\n')
mark('ready')
if mode == 'wrapper':
    import subprocess
    descendant = subprocess.Popen([sys.executable, '-c', 'import signal,time; signal.alarm(10); time.sleep(5)'], stdin=sys.stdin, stdout=sys.stdout)
    (root / 'descendant').write_text(str(descendant.pid))
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get('method', '')
    if method == 'initialize':
        mark('initialize')
        if mode == 'hang-init': continue
        if mode == 'restart-hang-init' and len((root / 'spawns').read_text().splitlines()) > 1:
            mark('reinitialize')
            continue
        result = {'serverInfo': {'name':'fixture'}, 'protocolVersion':'2025-11-25', 'capabilities':{'tools':{}}}
    elif method == 'tools/list':
        mark('discovery')
        if mode == 'hang-discovery': continue
        result = {'tools':[{'name':'echo', 'description':'fixture', 'inputSchema':{'type':'object'}}]}
    elif method == 'tools/call':
        mark('call')
        if mode in ('crash', 'restart-hang-init'): sys.exit(1)
        result = {'content':[{'type':'text', 'text':'ok'}]}
    elif 'id' in msg: result = {}
    else: continue
    print(json.dumps({'jsonrpc':'2.0', 'id':msg['id'], 'result':result}), flush=True)
mark('eof')
if mode in ('term', 'ignore'):
    while True: time.sleep(0.1)
"#;
