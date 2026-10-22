//! Real --print success/error exits with disposable storage, MCP, and model stub.
use eidetica::{Instance, NewAccount, backend::database::Sqlite};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn print_normal_and_bridge_error_exits_explicitly_close_mcp_before_return() {
    for fail in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let connection = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("eidetica.db").display()
        );
        let backend = Sqlite::connect(&connection).await.unwrap();
        let (owner, user) =
            Instance::create_backend(Box::new(backend), NewAccount::passwordless("fixture"))
                .await
                .unwrap();
        // Only the CLI opens this direct-owner store while it runs.
        drop(user);
        drop(owner);
        let model = MockServer::start().await;
        let response = if fail {
            ResponseTemplate::new(400).set_body_json(
                json!({"error":{"message":"fixture model error", "type":"invalid_request_error"}}),
            )
        } else {
            ResponseTemplate::new(200).set_body_json(json!({"choices":[{"message":{"role":"assistant","content":"fixture reply"}, "finish_reason":"stop"}]}))
        };
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(response)
            .expect(1)
            .mount(&model)
            .await;
        Mock::given(method("GET")).and(path("/v1/models")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"object":"list", "data":[{"id":"stub", "object":"model", "created":0, "owned_by":"fixture"}]}))).mount(&model).await;
        let config = json!({
            "state_dir":dir.path().join("state"), "execution":"executor",
            "eidetica":{"connection":connection, "login":{"username":"fixture", "passwordless":true}},
            "backends":[{"name":"stub", "type":"openaicompatible", "api_base":format!("{}/v1",model.uri()), "api_key":"not-a-real-key", "models":[{"name":"stub"}]}],
            "agents":[{"name":"chaz", "model":"stub", "system_prompt":"fixture"}], "default_agents":["chaz"],
            "mcp_servers":[{"name":"fixture", "command":"python3", "args":["-u","-c",MCP, dir.path()], "startup_timeout_secs":3}]
        });
        let config_path = dir.path().join("config.yaml");
        std::fs::write(&config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
        let binary = std::env::var_os("NEXTEST_BIN_EXE_chaz")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_chaz").into());
        let stderr_path = dir.path().join("stderr");
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            tokio::process::Command::new(binary)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env(
                    "LD_LIBRARY_PATH",
                    std::env::var_os("LD_LIBRARY_PATH").unwrap_or_default(),
                )
                // reqwest still constructs a TLS client for loopback HTTP;
                // preserve the Nix test shell's explicit certificate bundle.
                .envs(std::env::var_os("SSL_CERT_FILE").map(|v| ("SSL_CERT_FILE", v)))
                .env("HOME", dir.path())
                .env("XDG_CONFIG_HOME", dir.path())
                .arg("--config")
                .arg(config_path)
                .args(["--print", "fixture prompt"])
                .kill_on_drop(true)
                .stdout(std::process::Stdio::piped())
                .stderr(std::fs::File::create(&stderr_path).unwrap())
                .spawn()
                .unwrap()
                .wait_with_output(),
        )
        .await;
        let log = std::fs::read_dir(dir.path().join("state"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("chaz-cli"))
            .map(|e| std::fs::read_to_string(e.path()).unwrap())
            .collect::<String>();
        let stderr = std::fs::read_to_string(stderr_path).unwrap();
        let output = result
            .unwrap_or_else(|_| panic!("one-shot fixture timed out: {stderr}\n{log}"))
            .unwrap();
        assert!(
            log.contains("MCP server ready"),
            "test requires a Running child, not startup-timeout cleanup: {log}"
        );
        assert!(
            log.contains("Runtime cleanup complete"),
            "explicit awaited cleanup did not finish on fail={fail}: {log}"
        );
        assert_eq!(output.status.success(), !fail, "{stderr}");
        if fail {
            assert!(stderr.contains("fixture model error"), "{stderr}");
        } else {
            assert!(String::from_utf8_lossy(&output.stdout).contains("fixture reply"));
        }
        assert!(
            dir.path().join("eof").exists(),
            "explicit EOF cleanup missing on fail={fail}: {stderr}"
        );
        let pid = std::fs::read_to_string(dir.path().join("pid")).unwrap();
        #[cfg(target_os = "linux")]
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "direct child was not reaped"
        );
        #[cfg(not(target_os = "linux"))]
        let _ = pid;
        model.verify().await;
    }
}

const MCP: &str = r#"
import json, os, pathlib, signal, sys
root = pathlib.Path(sys.argv[1])
signal.alarm(25)
(root / 'pid').write_text(str(os.getpid()))
for line in sys.stdin:
    msg = json.loads(line)
    if msg.get('method') == 'initialize':
        result = {'protocolVersion':'2025-11-25','capabilities':{'tools':{}}}
    elif msg.get('method') == 'tools/list':
        result = {'tools':[]}
    else:
        continue
    print(json.dumps({'jsonrpc':'2.0','id':msg['id'],'result':result}), flush=True)
(root / 'eof').write_text('closed')
"#;
