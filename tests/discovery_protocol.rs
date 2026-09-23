//! Wire-level contract for opt-in discovery mode.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

#[test]
fn discovery_hides_domain_names_and_dispatches_only_allowed_tools() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_appstore-mcp"))
        .env_remove("ASC_ISSUER_ID")
        .env_remove("ASC_KEY_ID")
        .env_remove("ASC_PRIVATE_KEY")
        .env_remove("ASC_PRIVATE_KEY_PATH")
        .env("ASC_TOOL_DISCOVERY", "1")
        .env("ASC_TOOLS", "users")
        .env("ASC_READ_ONLY", "1")
        .env("ASC_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start server");
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());

    fn request(input: &mut impl Write, output: &mut impl BufRead, value: Value) -> Value {
        writeln!(input, "{value}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        assert!(
            output.read_line(&mut line).unwrap() > 0,
            "server closed stdout"
        );
        serde_json::from_str(&line).unwrap()
    }

    let init = request(
        &mut input,
        &mut output,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26", "capabilities": {},
                       "clientInfo": {"name": "discovery-test", "version": "1"}}
        }),
    );
    assert!(init.get("result").is_some(), "{init}");
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    input.flush().unwrap();

    let listed = request(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["call_discovered_tool", "get_tool_details", "search_tools"]
    );

    let search = request(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","id":3,
        "method":"tools/call","params":{"name":"search_tools","arguments":{"query":"users"}}}),
    );
    let matches: Value =
        serde_json::from_str(search["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(matches["matches"][0]["name"], "list_users");
    assert_eq!(matches["totalMatches"], 1);

    let direct = request(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","id":4,
        "method":"tools/call","params":{"name":"list_users","arguments":{}}}),
    );
    assert_eq!(direct["error"]["message"], "tool not found");

    let withheld = request(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","id":5,
        "method":"tools/call","params":{"name":"call_discovered_tool",
        "arguments":{"name":"remove_user","arguments":{"user_id":"x"}}}}),
    );
    assert_eq!(withheld["error"]["message"], "tool not available");

    let dispatched = request(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","id":6,
        "method":"tools/call","params":{"name":"call_discovered_tool",
        "arguments":{"name":"list_users","arguments":{}}}}),
    );
    assert!(
        dispatched["error"]["message"]
            .as_str()
            .unwrap()
            .contains("credentials are not configured"),
        "{dispatched}"
    );

    drop(input);
    assert!(child.wait().unwrap().success());
}
