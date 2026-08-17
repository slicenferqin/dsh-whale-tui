//! Scripted demo turn through the real event pipeline: no runtime, no API key.
//! Events use the exact dsh-session wire shapes verified against a live
//! 0.1.0-rc.6 session log (block-start / deltas / block-end / tool result / usage).

use serde_json::json;

use crate::app::{App, RunState};

pub fn seed(app: &mut App) {
    let events = [
        json!({"type":"step/start","seq":1,"time":1000,"data":{"turn":1,"step":1}}),
        json!({"type":"user/message","seq":2,"time":1000,"data":{"turn":1,"step":1,"source":{"kind":"user"},"content":[{"type":"text","text":"查看这个仓库并修复失败的测试"}]}}),
        json!({"type":"assistant/chunk","seq":3,"time":1500,"data":{"turn":1,"step":1,"chunk":{"type":"block-start","index":0,"blockType":"reasoning"}}}),
        json!({"type":"assistant/chunk","seq":4,"time":1600,"data":{"turn":1,"step":1,"chunk":{"type":"reasoning-delta","index":0,"text":"先看一下仓库结构。"}}}),
        json!({"type":"assistant/chunk","seq":5,"time":1800,"data":{"turn":1,"step":1,"chunk":{"type":"block-end","index":0}}}),
        json!({"type":"assistant/chunk","seq":6,"time":1900,"data":{"turn":1,"step":1,"chunk":{"type":"block-start","index":1,"blockType":"text"}}}),
        json!({"type":"assistant/chunk","seq":7,"time":2000,"data":{"turn":1,"step":1,"chunk":{"type":"text-delta","index":1,"text":"失败测试在 src/lib.rs:42，我来修复：\n\n```rust\npub fn parse(input: &str) -> Result<u32, ParseError> {\n    // 空串以前会 panic\n    if input.is_empty() {\n        return Err(ParseError::Empty(\"input was blank\"));\n    }\n    input.trim().parse().map_err(ParseError::from)\n}\n```\n\n然后跑一遍：\n\n```bash\ncargo test --all && cargo clippy -- -D warnings\n```\n"}}}),
        json!({"type":"assistant/chunk","seq":8,"time":2100,"data":{"turn":1,"step":1,"chunk":{"type":"block-end","index":1}}}),
        json!({"type":"assistant/chunk","seq":9,"time":2200,"data":{"turn":1,"step":1,"chunk":{"type":"usage","usage":{"inputTokens":4458,"outputTokens":253,"cacheReadTokens":11008}}}}),
        json!({"type":"assistant/message","seq":10,"time":2200,"data":{"turn":1,"step":1,"usage":{"inputTokens":4458,"outputTokens":253,"cacheReadTokens":11008},"message":{"role":"assistant","content":[{"type":"text","text":"失败测试在 src/lib.rs:42，我来修复：\n\n```rust\npub fn parse(input: &str) -> Result<u32, ParseError> {\n    // 空串以前会 panic\n    if input.is_empty() {\n        return Err(ParseError::Empty(\"input was blank\"));\n    }\n    input.trim().parse().map_err(ParseError::from)\n}\n```\n\n然后跑一遍：\n\n```bash\ncargo test --all && cargo clippy -- -D warnings\n```\n"}]}}}),
        json!({"type":"tool/call","seq":11,"time":2201,"data":{"turn":1,"step":1,"callId":"demo-todo","name":"todo_write","arguments":"{\"todos\":[{\"content\":\"定位失败的测试\",\"status\":\"completed\"},{\"content\":\"修复 parse 的空串分支\",\"status\":\"in_progress\"},{\"content\":\"补一条回归测试\",\"status\":\"pending\"}]}"}}),
        json!({"type":"tool/call","seq":12,"time":2202,"data":{"turn":1,"step":1,"callId":"demo-edit","name":"edit","arguments":"{\"file_path\":\"src/lib.rs\"}"}}),
        json!({"type":"tool/result","seq":13,"time":2400,"data":{"turn":1,"step":1,"message":{"source":{"kind":"tool","callId":"demo-edit"},"role":"user","content":[{"type":"tool-result","toolCallId":"demo-edit","content":[{"type":"text","text":"@@ -40,6 +40,9 @@ pub fn parse(input: &str) -> Result<u32, ParseError> {\n     let trimmed = input.trim();\n-    trimmed.parse().unwrap()\n+    if trimmed.is_empty() {\n+        return Err(ParseError::Empty(\"input was blank\"));\n+    }\n+    trimmed.parse().map_err(ParseError::from)\n }"}]}]}}}),
        json!({"type":"tool/call","seq":14,"time":2500,"data":{"turn":1,"step":1,"callId":"demo-call","name":"bash","arguments":"{\"command\":\"cargo test --all\"}"}}),
        json!({"type":"tool/result","seq":15,"time":3011,"data":{"turn":1,"step":1,"message":{"source":{"kind":"tool","callId":"demo-call"},"role":"user","content":[{"type":"tool-result","toolCallId":"demo-call","content":[{"type":"text","text":"   Compiling demo v0.1.0\n    Finished test profile in 4.21s\n     Running unittests src/lib.rs\n\nrunning 8 tests\ntest parse::empty_input_is_an_error ... ok\ntest parse::trims_whitespace ... ok\ntest parse::rejects_letters ... ok\ntest parse::accepts_zero ... ok\ntest parse::accepts_max ... ok\ntest parse::overflow_is_an_error ... ok\ntest parse::negative_is_an_error ... ok\ntest parse::roundtrips ... ok\n\ntest result: ok. 8 passed; 0 failed; 0 ignored"}]}]}}}),
        json!({"type":"step/end","seq":16,"time":3011,"data":{"turn":1,"step":1}}),
        json!({"type":"turn/end","seq":17,"time":3011,"data":{"turn":1,"reason":{"kind":"completed"}}}),
    ];
    for e in events {
        app.transcript.apply(&e);
    }
    app.state = RunState::Idle;
    app.status = "demo seeded".into();
}
