//! Replayable orchestration tests.
//!
//! The real chat loop runs against a scripted Responses server on loopback: each model reply is a
//! recorded stream, the tools are stand-ins, and the test checks what the loop did with them
//! (what it sent back to the model, what it showed, what the ledger recorded). No network, no
//! account, and the same result every run.

use std::{
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{Value, json};

use crate::{
    chat::{RunOptions, respond_with_options},
    ledger::{Entry, Event, Ledger},
};

fn sse(events: &[Value]) -> String {
    events.iter().map(|event| format!("data: {event}\n\n")).collect()
}

fn completed(output: Value) -> Value {
    json!({"type": "response.completed", "response": {"output": output, "usage": {"input_tokens": 100, "output_tokens": 20}}})
}

/// A model reply that only talks.
fn says(text: &str) -> String {
    sse(&[
        json!({"type": "response.output_text.delta", "delta": text}),
        completed(json!([{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}])),
    ])
}

/// A model reply that calls one tool.
fn calls(call_id: &str, name: &str, arguments: Value) -> String {
    sse(&[completed(json!([{"type": "function_call", "call_id": call_id, "name": name, "arguments": arguments.to_string()}]))])
}

fn fails(message: &str) -> String {
    sse(&[json!({"type": "response.failed", "response": {"error": {"message": message}}})])
}

/// Serves one scripted reply per request, in order, and records every request body.
fn serve(replies: Vec<String>) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let url = format!("http://{}/responses", listener.local_addr().unwrap());
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&bodies);
    std::thread::spawn(move || {
        for reply in replies {
            let Ok((mut stream, _)) = listener.accept() else { return };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let mut data = Vec::new();
            let mut chunk = [0_u8; 8192];
            let (head_end, length) = loop {
                let Ok(read) = stream.read(&mut chunk) else { return };
                if read == 0 {
                    return;
                }
                data.extend_from_slice(&chunk[..read]);
                if let Some(at) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&data[..at]).to_ascii_lowercase();
                    let length = head.lines().find_map(|line| line.strip_prefix("content-length:")?.trim().parse::<usize>().ok()).unwrap_or(0);
                    break (at + 4, length);
                }
            };
            while data.len() < head_end + length {
                let Ok(read) = stream.read(&mut chunk) else { return };
                if read == 0 {
                    break;
                }
                data.extend_from_slice(&chunk[..read]);
            }
            if let Ok(body) = serde_json::from_slice::<Value>(&data[head_end..]) {
                seen.lock().unwrap().push(body);
            }
            // "ERR:<json>" scripts a refusal; anything else is a stream.
            let (status, kind, payload) = match reply.strip_prefix("ERR:") {
                Some(body) => ("400 Bad Request", "application/json", body),
                None => ("200 OK", "text/event-stream", reply.as_str()),
            };
            let response = format!("HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len());
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (url, bodies)
}

struct Replay {
    answer: Result<String, String>,
    /// Every request the loop sent to the model.
    requests: Vec<Value>,
    /// What the loop showed the user.
    shown: String,
    ledger: Vec<Entry>,
    /// Tool names the loop asked to run, in order.
    ran: Vec<String>,
    /// Whether the loop told the caller fast mode was rejected for this run.
    fast_unavailable: bool,
}

impl Replay {
    fn input_of(&self, request: usize) -> String {
        self.requests[request]["input"].to_string()
    }
}

fn replay(replies: Vec<String>, message: &str, tool: impl FnMut(&str, &Value) -> Result<Value, String>) -> Replay {
    replay_with(replies, message, false, tool)
}

fn replay_with(replies: Vec<String>, message: &str, fast: bool, mut tool: impl FnMut(&str, &Value) -> Result<Value, String>) -> Replay {
    let (url, bodies) = serve(replies);
    let dir = std::env::temp_dir().join(format!("rhea-replay-{}-{:?}", std::process::id(), std::thread::current().id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path: PathBuf = dir.join("ledger.jsonl");
    let options = RunOptions { endpoint: Some(url), ledger: Some(Ledger::at(Some(path.clone()), "main")), fast, ..Default::default() };
    let shown = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&shown);
    let ran = Arc::new(Mutex::new(Vec::new()));
    let ran_log = Arc::clone(&ran);
    let fast_unavailable = Arc::new(Mutex::new(false));
    let fast_unavailable_sink = Arc::clone(&fast_unavailable);
    let answer = respond_with_options(
        "test-token",
        None,
        "test-model",
        "low",
        200_000,
        "",
        &[],
        message,
        &[],
        move |name, arguments| {
            ran_log.lock().unwrap().push(name.to_owned());
            tool(name, arguments)
        },
        options,
        move |event| {
            if let Some(delta) = event.delta {
                sink.lock().unwrap().push_str(&delta);
            }
            if event.fast_unavailable {
                *fast_unavailable_sink.lock().unwrap() = true;
            }
        },
    );
    let requests = bodies.lock().unwrap().clone();
    let ledger = std::fs::read_to_string(&path).unwrap_or_default().lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
    let shown = shown.lock().unwrap().clone();
    let ran = ran.lock().unwrap().clone();
    let fast_unavailable = *fast_unavailable.lock().unwrap();
    Replay { answer, requests, shown, ledger, ran, fast_unavailable }
}

fn ends_with(replay: &Replay, status: &str) -> bool {
    replay.ledger.iter().any(|entry| matches!(&entry.event, Event::End { status: s, .. } if s == status))
}

#[test]
fn a_plain_answer_is_one_request_and_is_recorded() {
    let run = replay(vec![says("Hello there.")], "say hello", |_, _| unreachable!("no tools expected"));
    assert_eq!(run.answer.as_deref(), Ok("Hello there."));
    assert_eq!(run.requests.len(), 1);
    assert!(run.input_of(0).contains("say hello"));
    assert_eq!(run.requests[0]["stream"], json!(true));
    assert_eq!(run.requests[0]["store"], json!(false));
    assert!(matches!(run.ledger.first().map(|e| &e.event), Some(Event::Start { request, .. }) if request == "say hello"));
    assert!(ends_with(&run, "completed"));
}

#[test]
fn fast_mode_asks_for_priority_processing_and_normal_mode_does_not() {
    let fast = replay_with(vec![says("quick")], "hello", true, |_, _| unreachable!());
    assert_eq!(fast.answer.as_deref(), Ok("quick"));
    assert_eq!(fast.requests[0]["service_tier"], json!("priority"));
    assert!(!fast.fast_unavailable, "the service accepted it without complaint");
    let normal = replay(vec![says("steady")], "hello", |_, _| unreachable!());
    assert!(normal.requests[0].get("service_tier").is_none());
}

#[test]
fn a_backend_that_refuses_fast_mode_gets_the_same_request_at_normal_speed() {
    let refusal = r#"ERR:{"error":{"message":"Unknown parameter: 'service_tier'."}}"#.to_owned();
    let run = replay_with(vec![refusal, says("done anyway")], "hello", true, |_, _| unreachable!());
    assert_eq!(run.answer.as_deref(), Ok("done anyway"), "the run is not failed by the refused option");
    assert_eq!(run.requests.len(), 2);
    assert_eq!(run.requests[0]["service_tier"], json!("priority"));
    assert!(run.requests[1].get("service_tier").is_none(), "retried without it");
    assert!(run.fast_unavailable, "the caller is told so it does not count this run as fast");
    // Any other refusal still fails the run.
    let other = replay_with(vec![r#"ERR:{"error":{"message":"quota exceeded"}}"#.to_owned()], "hello", true, |_, _| unreachable!());
    assert!(other.answer.is_err());
    assert!(!other.fast_unavailable, "the run simply failed, fast mode was never actually rejected");
}

#[test]
fn a_tool_result_goes_back_to_the_model_before_the_answer() {
    let run = replay(
        vec![calls("call-1", "probe_tool", json!({"q": 1})), says("The probe said 42.")],
        "run the probe",
        |name, arguments| {
            assert_eq!((name, arguments["q"].as_i64()), ("probe_tool", Some(1)));
            Ok(json!({"value": 42}))
        },
    );
    assert_eq!(run.answer.as_deref(), Ok("The probe said 42."));
    assert_eq!(run.requests.len(), 2);
    assert_eq!(run.ran, ["probe_tool"]);
    let second = run.input_of(1);
    assert!(second.contains("function_call_output") && second.contains("call-1") && second.contains("42"), "{second}");
    let tool = run.ledger.iter().find_map(|entry| match &entry.event {
        Event::Tool { name, ok, .. } => Some((name.clone(), *ok)),
        _ => None,
    });
    assert_eq!(tool, Some(("probe_tool".into(), true)));
}

#[test]
fn a_failed_tool_is_reported_to_the_model_not_hidden() {
    let run = replay(vec![calls("call-1", "probe_tool", json!({})), says("It failed, so I stopped.")], "run the probe", |_, _| Err("disk on fire".into()));
    assert!(run.answer.is_ok());
    let second = run.input_of(1);
    assert!(second.contains("disk on fire") && second.contains("\\\"ok\\\":false"), "{second}");
    assert!(run.ledger.iter().any(|entry| matches!(&entry.event, Event::Tool { ok: false, .. })));
}

#[test]
fn research_with_too_little_evidence_is_sent_back_then_accepted_once_proven() {
    let page = |host: &str, players: u32| Ok(json!({"text": format!("https://{host}/games The game has {players} active players right now")}));
    let mut fetched = 0;
    let run = replay(
        vec![
            says("I'll research that for you."),
            calls("c1", "browser__extract", json!({"url": "a"})),
            calls("c2", "browser__extract", json!({"url": "b"})),
            says("Two games qualify: A (5,000) and B (9,000)."),
        ],
        "search the web for games with more than 1K active players",
        |_, _| {
            fetched += 1;
            if fetched == 1 { page("a.com", 5000) } else { page("b.com", 9000) }
        },
    );
    assert_eq!(run.answer.as_deref(), Ok("Two games qualify: A (5,000) and B (9,000)."));
    assert_eq!(run.requests.len(), 4);
    let retry = run.input_of(1);
    assert!(retry.contains("Completion gate") && retry.contains("0 of the 2 needed"), "{retry}");
    assert!(!run.shown.contains("I'll research"), "a promise is never shown as an answer: {}", run.shown);
    let gates: Vec<String> = run
        .ledger
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Gate { decision, .. } => Some(decision.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(gates, ["retry", "accept"]);
    let sources: Vec<String> = run
        .ledger
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Evidence { source, .. } => Some(source.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(sources, ["a.com", "b.com"]);
}

#[test]
fn research_that_never_finds_evidence_ends_with_an_honest_blocker() {
    let run = replay(vec![says("Sure, researching."), says("Here are some games I remember.")], "search the web for games with more than 1K active players", |_, _| {
        unreachable!("the model never called a tool")
    });
    let answer = run.answer.unwrap();
    assert!(answer.contains("couldn’t verify") && answer.contains("0 of the 2 needed"), "{answer}");
    assert!(!answer.contains("games I remember"));
    assert_eq!(run.requests.len(), 2);
}

#[test]
fn the_same_page_fetched_twice_does_not_satisfy_a_two_source_request() {
    let same = || Ok(json!({"text": "https://a.com/games The game has 5000 active players right now"}));
    let run = replay(
        vec![calls("c1", "browser__extract", json!({})), calls("c2", "browser__extract", json!({})), says("Done."), says("Still done.")],
        "search the web for games with more than 1K active players",
        |_, _| same(),
    );
    let answer = run.answer.unwrap();
    assert!(answer.contains("1 of the 2 needed") && answer.contains("a.com"), "{answer}");
}

#[test]
fn a_model_error_ends_the_run_as_failed_in_the_ledger() {
    let run = replay(vec![fails("overloaded")], "say hello", |_, _| unreachable!());
    assert!(run.answer.is_err());
    assert!(ends_with(&run, "failed"));
    assert!(run.ledger.iter().any(|entry| matches!(&entry.event, Event::End { error, .. } if !error.is_empty())));
}

#[test]
fn a_tool_the_run_may_not_use_is_refused_without_running_it() {
    let (url, _bodies) = serve(vec![calls("c1", "ask_user", json!({})), says("ok")]);
    let options = RunOptions { endpoint: Some(url), hidden_tools: vec!["ask_user".into()], ..Default::default() };
    let ran = Arc::new(Mutex::new(0));
    let counter = Arc::clone(&ran);
    let answer = respond_with_options(
        "t", None, "m", "low", 200_000, "", &[], "do it", &[],
        move |_, _| {
            *counter.lock().unwrap() += 1;
            Ok(json!({}))
        },
        options,
        |_| {},
    );
    assert_eq!(answer.as_deref(), Ok("ok"));
    assert_eq!(*ran.lock().unwrap(), 0, "a hidden tool must never reach the executor");
}
