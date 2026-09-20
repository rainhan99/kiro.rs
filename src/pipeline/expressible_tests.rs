use super::*;
use serde_json::json;

fn request(messages: Value) -> MessagesRequest {
    serde_json::from_value(json!({
        "model": "claude-sonnet-4", "max_tokens": 100, "messages": messages
    }))
    .unwrap()
}

fn roles(payload: &MessagesRequest) -> Vec<&str> {
    payload.messages.iter().map(|m| m.role.as_str()).collect()
}

// ---------- 这一组是真正缺的那一层：0.9.0 能跑的形状，现在还要能跑 ----------

/// 未知角色的消息：0.9.0 由转换器整条静默跳过，请求照样跑通。
/// 现在同样跑通，但**说得出丢了什么**。
#[test]
fn a_message_with_an_unknown_role_is_dropped_and_named() {
    let mut payload = request(json!([
        {"role": "system", "content": "be terse"},
        {"role": "user", "content": "hi"}
    ]));
    let removed = make_expressible(&mut payload, UnexpressibleStrategy::Drop).unwrap();

    assert_eq!(roles(&payload), vec!["user"], "未知角色那条被去掉");
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].path, "messages[0]");
    assert!(
        removed[0].reason.contains("\"system\""),
        "必须说出实际收到的角色：{}",
        removed[0].reason
    );
    assert!(
        !removed[0].reason.contains("be terse"),
        "原因里不得出现内容：{}",
        removed[0].reason
    );
}

/// 末尾 assistant（prefill）：0.9.0 截断到最后一条 user 继续，请求跑通。
///
/// Kiro 只能从一条 user 轮次往下生成，这个开头它用不上；所以它和未知角色、
/// 未知内容块是同一类，归同一个设置管。
#[test]
fn a_trailing_assistant_prefill_is_truncated_and_named() {
    let mut payload = request(json!([
        {"role": "user", "content": "write a title"},
        {"role": "assistant", "content": "Title:"}
    ]));
    let removed = make_expressible(&mut payload, UnexpressibleStrategy::Drop).unwrap();

    assert_eq!(roles(&payload), vec!["user"], "截断到最后一条 user");
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].path, "messages[1]");
    assert!(
        removed[0].reason.contains("prefill"),
        "{}",
        removed[0].reason
    );
    assert!(
        !removed[0].reason.contains("Title:"),
        "原因里不得出现内容：{}",
        removed[0].reason
    );
}

/// 连续多条末尾 assistant 一并截掉，而不是只去掉最后一条。
#[test]
fn several_trailing_assistant_turns_are_all_truncated() {
    let mut payload = request(json!([
        {"role": "user", "content": "q"},
        {"role": "assistant", "content": "a1"},
        {"role": "assistant", "content": "a2"}
    ]));
    let removed = make_expressible(&mut payload, UnexpressibleStrategy::Drop).unwrap();
    assert_eq!(roles(&payload), vec!["user"]);
    assert_eq!(removed.len(), 2);
}

/// 未知内容块：0.9.0 不会把它送出去，请求照跑。
#[test]
fn an_unknown_content_block_is_dropped_and_named() {
    let mut payload = request(json!([{
        "role": "user",
        "content": [
            {"type": "text", "text": "look"},
            {"type": "video", "url": "https://example.test/v.mp4"}
        ]
    }]));
    let removed = make_expressible(&mut payload, UnexpressibleStrategy::Drop).unwrap();

    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].path, "messages[0].content[1]");
    assert!(
        removed[0].reason.contains("\"video\""),
        "{}",
        removed[0].reason
    );
    let blocks = payload.messages[0].content.as_array().unwrap();
    assert_eq!(blocks.len(), 1, "只该去掉那一个块");
    assert_eq!(blocks[0]["text"], "look", "可表达的内容一个不动");
}

/// URL 图片：Kiro 只收内联 base64。丢弃并说明**实际是什么形式**。
#[test]
fn a_url_image_is_dropped_with_its_source_type_named() {
    let mut payload = request(json!([{
        "role": "user",
        "content": [{"type": "image", "source": {"type": "url", "url": "https://x.test/a.png"}}]
    }]));
    let removed = make_expressible(&mut payload, UnexpressibleStrategy::Drop).unwrap();
    assert_eq!(removed.len(), 1);
    assert!(
        removed[0].reason.contains("\"url\"") && removed[0].reason.contains("base64"),
        "要说清实际是什么形式、Kiro 要什么：{}",
        removed[0].reason
    );
    assert!(payload.messages.is_empty(), "块删光的消息不该留成空轮次");
}

/// 常规形状一个都不能动。
#[test]
fn an_ordinary_conversation_is_left_exactly_as_it_is() {
    let original = json!([
        {"role": "user", "content": "hello"},
        {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
        {"role": "user", "content": [
            {"type": "text", "text": "and this"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
        ]}
    ]);
    let mut payload = request(original.clone());
    let before = serde_json::to_value(&payload.messages).unwrap();

    let removed = make_expressible(&mut payload, UnexpressibleStrategy::Drop).unwrap();
    assert!(removed.is_empty(), "常规请求不该有任何删除：{removed:?}");
    assert_eq!(serde_json::to_value(&payload.messages).unwrap(), before);
}

/// 工具往返是常规形状，不得被当成表达不了。
#[test]
fn a_tool_round_trip_is_expressible() {
    let mut payload = request(json!([
        {"role": "user", "content": "run it"},
        {"role": "assistant", "content": [
            {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"cmd": "ls"}}
        ]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "a.txt"}
        ]}
    ]));
    let removed = make_expressible(&mut payload, UnexpressibleStrategy::Drop).unwrap();
    assert!(removed.is_empty(), "{removed:?}");
    assert_eq!(roles(&payload), vec!["user", "assistant", "user"]);
}

// ---------- refuse ----------

/// 选了 `refuse` 就拒绝，且**请求原样不动**，错误里带位置、实际值与角色序列。
#[test]
fn refusing_names_what_it_saw_and_changes_nothing() {
    let mut payload = request(json!([
        {"role": "system", "content": "x"},
        {"role": "user", "content": "hi"}
    ]));
    let before = serde_json::to_value(&payload.messages).unwrap();

    let Err(error) = make_expressible(&mut payload, UnexpressibleStrategy::Refuse) else {
        panic!("选了 refuse 就该拒绝");
    };
    let rendered = format!("{error:#}");
    assert!(rendered.contains("messages[0]"), "{rendered}");
    assert!(
        rendered.contains("\"system\""),
        "要说出实际角色：{rendered}"
    );
    assert!(
        rendered.contains("Roles received: [system, user]"),
        "{rendered}"
    );
    assert!(
        rendered.contains("requestPipeline.unexpressible"),
        "要给出改法：{rendered}"
    );
    assert_eq!(
        serde_json::to_value(&payload.messages).unwrap(),
        before,
        "拒绝的请求不得被改动"
    );
}

/// 多项问题时告诉运维一共有几项，而不是只报第一项就完事。
#[test]
fn refusing_says_how_many_problems_there_are() {
    let mut payload = request(json!([
        {"role": "system", "content": "x"},
        {"role": "tool", "content": "y"},
        {"role": "user", "content": "hi"}
    ]));
    let Err(error) = make_expressible(&mut payload, UnexpressibleStrategy::Refuse) else {
        panic!("应拒绝");
    };
    assert!(format!("{error:#}").contains("and 1 more"), "{error:#}");
}

/// 默认是 `drop`：拒绝并不能把内容保住，却会让请求失败。
#[test]
fn the_default_is_drop() {
    assert_eq!(
        UnexpressibleStrategy::default(),
        UnexpressibleStrategy::Drop
    );
}

/// 长会话的角色序列要折叠，不能把整段历史铺开。
#[test]
fn a_long_role_sequence_is_folded() {
    let mut messages = Vec::new();
    for _ in 0..10 {
        messages.push(json!({"role": "user", "content": "q"}));
        messages.push(json!({"role": "assistant", "content": "a"}));
    }
    let payload = request(json!(messages));
    let rendered = role_sequence(&payload);
    assert!(rendered.contains("…12 more…"), "{rendered}");
}
