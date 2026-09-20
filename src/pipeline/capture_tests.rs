use super::*;
use serde_json::json;

fn sample() -> Value {
    json!({
        "model": "claude-opus-5",
        "max_tokens": 64,
        "stream": true,
        "system": "你是一个助手",
        "messages": [
            {"role": "system", "content": "be terse"},
            {"role": "user", "content": "这是一段很敏感的提示词"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Title:"},
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"cmd": "ls -la"}}
            ]}
        ]
    })
}

fn enabled() -> CaptureConfig {
    CaptureConfig {
        enabled: true,
        ..CaptureConfig::default()
    }
}

/// 默认关。没人开它就什么都不该留下——抓包是排查工具，不是默认行为。
#[test]
fn nothing_is_captured_until_it_is_switched_on() {
    assert!(!CaptureConfig::default().enabled);
    let store = CaptureStore::new();
    store.record(&CaptureConfig::default(), None, &sample());
    assert!(store.recent().is_empty());
}

/// 默认只抓形状：**一个字的提示词都不留**，但结构一目了然。
#[test]
fn the_default_keeps_the_shape_and_none_of_the_text() {
    let store = CaptureStore::new();
    store.record(&enabled(), Some("t-1".into()), &sample());

    let entry = store.recent().into_iter().next().unwrap();
    let rendered = serde_json::to_string(&entry).unwrap();
    for secret in [
        "这是一段很敏感的提示词",
        "你是一个助手",
        "be terse",
        "ls -la",
        "Title:",
    ] {
        assert!(!rendered.contains(secret), "抓到了内容：{secret}");
    }

    // 而结构全在。
    assert_eq!(entry.model, "claude-opus-5");
    assert!(entry.stream);
    assert_eq!(entry.roles, vec!["system", "user", "assistant"]);
    assert_eq!(
        entry.block_types,
        vec![
            ("<string>".to_string(), 2),
            ("text".to_string(), 1),
            ("tool_use".to_string(), 1)
        ]
    );
    // 字段名与数值保留——max_tokens / stream 正是要看的。
    assert_eq!(entry.body["max_tokens"], 64);
    assert_eq!(entry.body["messages"][1]["content"], "<str:11>");
    assert_eq!(entry.body["messages"][2]["content"][1]["name"], "<str:4>");
    assert_eq!(entry.trace_id.as_deref(), Some("t-1"));
}

/// 明确关掉脱敏才留原文。
#[test]
fn turning_redaction_off_keeps_the_literal_text() {
    let store = CaptureStore::new();
    store.record(
        &CaptureConfig {
            redact_text: false,
            ..enabled()
        },
        None,
        &sample(),
    );
    let entry = store.recent().into_iter().next().unwrap();
    assert_eq!(
        entry.body["messages"][1]["content"],
        "这是一段很敏感的提示词"
    );
}

/// 有界：超出条数丢最旧的，最新在前。
#[test]
fn the_buffer_is_bounded_and_newest_first() {
    let store = CaptureStore::new();
    let config = CaptureConfig {
        max_requests: 3,
        ..enabled()
    };
    for i in 0..6 {
        let mut body = sample();
        body["model"] = json!(format!("m{i}"));
        store.record(&config, None, &body);
    }
    let models: Vec<String> = store.recent().into_iter().map(|e| e.model).collect();
    assert_eq!(models, vec!["m5", "m4", "m3"], "只留最近三条，最新在前");
}

/// 单条太大时标记为截断，而不是把它整个塞进去。
#[test]
fn an_oversized_entry_is_marked_rather_than_kept() {
    let store = CaptureStore::new();
    store.record(
        &CaptureConfig {
            max_bytes: 32,
            ..enabled()
        },
        None,
        &sample(),
    );
    let entry = store.recent().into_iter().next().unwrap();
    assert!(entry.truncated);
    assert_eq!(entry.body, Value::Null, "截断时不保留半截 body");
    // 但形状摘要仍在——那正是最常用的部分。
    assert_eq!(entry.roles, vec!["system", "user", "assistant"]);
}

/// 缺字段不该让抓包炸掉：它是排查工具，遇到畸形请求时最需要它还活着。
#[test]
fn a_malformed_body_is_still_captured() {
    let store = CaptureStore::new();
    store.record(&enabled(), None, &json!({"messages": "not-an-array"}));
    let entry = store.recent().into_iter().next().unwrap();
    assert_eq!(entry.model, "<missing>");
    assert!(entry.roles.is_empty());
    assert!(entry.block_types.is_empty());
}

/// 清空。
#[test]
fn clearing_removes_everything() {
    let store = CaptureStore::new();
    store.record(&enabled(), None, &sample());
    store.clear();
    assert!(store.recent().is_empty());
}

/// 抓包是**排查工具**，不是日志系统：忘了关也不该慢慢吃光内存。
///
/// 这条钉住上限真的生效——把条数和单条大小都设小，塞很多大请求，占用不随之增长。
#[test]
fn leaving_it_on_cannot_grow_without_bound() {
    let store = CaptureStore::new();
    let config = CaptureConfig {
        max_requests: 5,
        max_bytes: 4096,
        ..enabled()
    };
    for i in 0..500 {
        let mut body = sample();
        // 每条都塞一段很长的内容。
        body["messages"][1]["content"] = json!("x".repeat(50_000));
        body["model"] = json!(format!("m{i}"));
        store.record(&config, None, &body);
    }
    let entries = store.recent();
    assert_eq!(entries.len(), 5, "条数上限必须生效");
    let bytes = serde_json::to_vec(&entries).unwrap().len();
    assert!(
        bytes < 64 * 1024,
        "五条的总占用不该随请求大小无限增长，实得 {bytes} 字节"
    );
}

/// 脱敏必须覆盖**任意深度**：嵌套在工具入参、tool_result 里的文本同样是内容。
#[test]
fn redaction_reaches_every_depth() {
    let store = CaptureStore::new();
    store.record(
        &enabled(),
        None,
        &json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": "深层的敏感内容"}
                ]}
            ]}]
        }),
    );
    let rendered = serde_json::to_string(&store.recent()[0]).unwrap();
    assert!(
        !rendered.contains("深层的敏感内容"),
        "深层文本没被脱敏：{rendered}"
    );
    assert!(rendered.contains("<str:7>"), "但长度要留下：{rendered}");
}
