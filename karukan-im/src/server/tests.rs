use serde_json::{Value, json};

use super::ImServer;
use crate::config::Settings;
use crate::config::settings::StrategyMode;
use crate::core::keycode::Keysym;

// XKB keysyms for common keys (u32 aliases for the JSON payloads below)
const XKB_KEY_K: u32 = Keysym::KEY_K.0;
const XKB_KEY_A: u32 = Keysym::KEY_A.0;
const XKB_KEY_LOWER_L: u32 = Keysym::KEY_L.0;
const XKB_KEY_RETURN: u32 = Keysym::RETURN.0;
const XKB_KEY_ESCAPE: u32 = Keysym::ESCAPE.0;
const XKB_KEY_SPACE: u32 = Keysym::SPACE.0;

fn test_server() -> ImServer {
    let mut server = ImServer::with_settings(Settings::default());
    // Disable live conversion (Ctrl+Shift+L) so the preedit stays as
    // hiragana; live conversion would require a loaded model.
    request(
        &mut server,
        json!({"jsonrpc":"2.0","id":0,"method":"process_key","params":{
            "keysym": XKB_KEY_LOWER_L,
            "modifiers": {"control": true, "shift": true}
        }}),
    );
    server
}

/// Send a request value and return the parsed response.
fn request(server: &mut ImServer, req: Value) -> Value {
    let line = serde_json::to_string(&req).unwrap();
    let resp = server.handle_line(&line).expect("expected a response");
    serde_json::from_str(&resp).unwrap()
}

fn press(server: &mut ImServer, keysym: u32) -> Value {
    request(
        server,
        json!({"jsonrpc":"2.0","id":1,"method":"process_key","params":{"keysym": keysym}}),
    )
}

/// Collect actions of a given type from a process_key response.
fn actions_of<'a>(resp: &'a Value, ty: &str) -> Vec<&'a Value> {
    resp["result"]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["type"] == ty)
        .collect()
}

#[test]
fn test_parse_error() {
    let mut server = test_server();
    let resp: Value = serde_json::from_str(&server.handle_line("not json").unwrap()).unwrap();
    assert_eq!(resp["error"]["code"], -32700);
    assert_eq!(resp["id"], Value::Null);
}

#[test]
fn test_method_not_found() {
    let mut server = test_server();
    let resp = request(
        &mut server,
        json!({"jsonrpc":"2.0","id":1,"method":"no_such_method"}),
    );
    assert_eq!(resp["error"]["code"], -32601);
    assert_eq!(resp["id"], 1);
}

#[test]
fn test_invalid_params() {
    let mut server = test_server();
    let resp = request(
        &mut server,
        json!({"jsonrpc":"2.0","id":2,"method":"process_key","params":{"keysym":"x"}}),
    );
    assert_eq!(resp["error"]["code"], -32602);
}

#[test]
fn test_notification_gets_no_response() {
    let mut server = test_server();
    let line = serde_json::to_string(
        &json!({"jsonrpc":"2.0","method":"process_key","params":{"keysym": XKB_KEY_K}}),
    )
    .unwrap();
    assert!(server.handle_line(&line).is_none());
    // The notification was still executed: 'k' is buffered, so 'a' yields か
    let resp = press(&mut server, XKB_KEY_A);
    let preedits = actions_of(&resp, "update_preedit");
    assert_eq!(preedits.last().unwrap()["text"], "か");
}

#[test]
fn test_typing_and_commit() {
    let mut server = test_server();

    let resp = press(&mut server, XKB_KEY_K);
    assert_eq!(resp["result"]["consumed"], true);
    let preedits = actions_of(&resp, "update_preedit");
    assert_eq!(preedits.last().unwrap()["text"], "k");

    let resp = press(&mut server, XKB_KEY_A);
    let preedits = actions_of(&resp, "update_preedit");
    let preedit = preedits.last().unwrap();
    assert_eq!(preedit["text"], "か");
    assert_eq!(preedit["caret"], 1);

    let resp = press(&mut server, XKB_KEY_RETURN);
    let commits = actions_of(&resp, "commit");
    assert_eq!(commits.last().unwrap()["text"], "か");
    assert!(actions_of(&resp, "update_preedit").is_empty());
}

#[test]
fn test_escape_cancels_composition() {
    let mut server = test_server();
    press(&mut server, XKB_KEY_K);
    press(&mut server, XKB_KEY_A);
    let resp = press(&mut server, XKB_KEY_ESCAPE);
    assert_eq!(resp["result"]["consumed"], true);
    let preedits = actions_of(&resp, "update_preedit");
    assert_eq!(preedits.last().unwrap()["text"], "");

    // Key after cancel is not consumed in Empty state unless printable;
    // Escape itself in Empty state passes through.
    let resp = press(&mut server, XKB_KEY_ESCAPE);
    assert_eq!(resp["result"]["consumed"], false);
}

#[test]
fn test_explicit_commit_method() {
    let mut server = test_server();
    press(&mut server, XKB_KEY_K);
    press(&mut server, XKB_KEY_A);
    let resp = request(
        &mut server,
        json!({"jsonrpc":"2.0","id":7,"method":"commit"}),
    );
    let commits = actions_of(&resp, "commit");
    assert_eq!(commits.last().unwrap()["text"], "か");
    assert!(actions_of(&resp, "update_preedit").is_empty());

    // Nothing left to commit afterwards
    let resp = request(
        &mut server,
        json!({"jsonrpc":"2.0","id":8,"method":"commit"}),
    );
    assert!(actions_of(&resp, "commit").is_empty());
    assert_eq!(
        actions_of(&resp, "update_preedit").last().unwrap()["text"],
        ""
    );
}

#[test]
fn test_select_candidate_waits_for_return_before_commit() {
    let mut server = test_server();
    press(&mut server, XKB_KEY_K);
    press(&mut server, XKB_KEY_A);

    // Space starts conversion; without a model the candidates come from
    // the hiragana/katakana fallback and the rewriter.
    let resp = press(&mut server, XKB_KEY_SPACE);
    let shows = actions_of(&resp, "show_candidates");
    let second_text = shows.last().unwrap()["candidates"][1]["text"].clone();

    let resp = request(
        &mut server,
        json!({"jsonrpc":"2.0","id":20,"method":"select_candidate","params":{"page_index":1}}),
    );
    assert_eq!(resp["result"]["consumed"], true);
    assert!(actions_of(&resp, "commit").is_empty());
    assert_eq!(
        actions_of(&resp, "update_preedit").last().unwrap()["text"],
        second_text
    );

    let resp = request(
        &mut server,
        json!({"jsonrpc":"2.0","id":21,"method":"status"}),
    );
    assert_eq!(resp["result"]["state"], "conversion");

    let resp = press(&mut server, XKB_KEY_RETURN);
    assert_eq!(
        actions_of(&resp, "commit").last().unwrap()["text"],
        second_text
    );
    let resp = request(
        &mut server,
        json!({"jsonrpc":"2.0","id":24,"method":"status"}),
    );
    assert_eq!(resp["result"]["state"], "empty");
}

#[test]
fn test_select_candidate_out_of_range() {
    let mut server = test_server();
    let resp = request(
        &mut server,
        json!({"jsonrpc":"2.0","id":22,"method":"select_candidate","params":{"page_index":9}}),
    );
    assert_eq!(resp["error"]["code"], -32602);
}

#[test]
fn test_select_candidate_without_candidates_not_consumed() {
    let mut server = test_server();
    let resp = request(
        &mut server,
        json!({"jsonrpc":"2.0","id":23,"method":"select_candidate","params":{"page_index":0}}),
    );
    assert_eq!(resp["result"]["consumed"], false);
}

#[test]
fn test_deferred_live_conversion_protocol_round_trip() {
    let mut server = test_server();
    // test_server disables live conversion; turn it back on without loading
    // a model so this checks transport/state behavior only.
    request(
        &mut server,
        json!({"jsonrpc":"2.0","id":30,"method":"process_key","params":{
            "keysym": XKB_KEY_LOWER_L,
            "modifiers": {"control": true, "shift": true}
        }}),
    );
    press(&mut server, XKB_KEY_A);

    let resp = request(
        &mut server,
        json!({"jsonrpc":"2.0","id":31,"method":"refresh_live_conversion","params":{}}),
    );
    assert_eq!(resp["result"]["consumed"], true);
    assert!(!actions_of(&resp, "update_preedit").is_empty());
}

#[test]
fn test_reset_clears_state() {
    let mut server = test_server();
    press(&mut server, XKB_KEY_K);
    let resp = request(
        &mut server,
        json!({"jsonrpc":"2.0","id":9,"method":"reset"}),
    );
    assert!(resp["error"].is_null());
    let resp = request(
        &mut server,
        json!({"jsonrpc":"2.0","id":10,"method":"status"}),
    );
    assert_eq!(resp["result"]["state"], "empty");
    assert_eq!(resp["result"]["initialized"], false);
}

#[test]
fn test_set_surrounding_text() {
    let mut server = test_server();
    let resp = request(
        &mut server,
        json!({"jsonrpc":"2.0","id":11,"method":"set_surrounding_text",
               "params":{"text":"こんにちは世界","cursor_pos":5}}),
    );
    assert!(resp["error"].is_null());
}

#[test]
fn test_status_before_init() {
    let mut server = test_server();
    let resp = request(
        &mut server,
        json!({"jsonrpc":"2.0","id":12,"method":"status"}),
    );
    assert_eq!(resp["result"]["initialized"], false);
    assert_eq!(resp["result"]["state"], "empty");
}

#[test]
fn init_returns_before_resources_and_first_kana_input_stays_responsive() {
    let mut settings = Settings::default();
    settings.conversion.strategy = StrategyMode::Main;
    settings.conversion.model = Some("not-a-real-model".to_string());
    settings.conversion.live_conversion = false;
    settings.learning.enabled = false;
    settings.dictionary_update.enabled = false;
    let mut server = ImServer::with_settings(settings);

    let started = std::time::Instant::now();
    let init = request(
        &mut server,
        json!({"jsonrpc":"2.0","id":40,"method":"init","params":{}}),
    );
    assert!(
        started.elapsed() < std::time::Duration::from_millis(250),
        "init must only start the worker, not wait for resources"
    );
    assert_eq!(init["result"]["model_name"], "initializing");

    press(&mut server, XKB_KEY_K);
    let typed = press(&mut server, XKB_KEY_A);
    assert_eq!(
        actions_of(&typed, "update_preedit").last().unwrap()["text"],
        "か"
    );
    assert_eq!(typed["result"]["process_key_ms"], 0);
}
