//! Scripted demo turn through the real event pipeline: no runtime, no API key.
//! Events use the exact dsh-session wire shapes verified against a live
//! 0.1.0-rc.6 session log (block-start / text-delta / block-end / usage).

use serde_json::json;

use crate::app::{App, RunState};

pub fn seed(app: &mut App) {
    let events = [
        json!({"type":"user/message","seq":1,"data":{"source":{"kind":"user"},"content":[{"type":"text","text":"查看这个仓库并修复失败的测试"}]}}),
        json!({"type":"assistant/chunk","seq":2,"data":{"chunk":{"type":"block-start","index":0,"blockType":"reasoning"}}}),
        json!({"type":"assistant/chunk","seq":3,"data":{"chunk":{"type":"text-delta","index":0,"text":"先看一下仓库结构。"}}}),
        json!({"type":"assistant/chunk","seq":4,"data":{"chunk":{"type":"block-end","index":0}}}),
        json!({"type":"assistant/chunk","seq":5,"data":{"chunk":{"type":"block-start","index":1,"blockType":"text"}}}),
        json!({"type":"assistant/chunk","seq":6,"data":{"chunk":{"type":"text-delta","index":1,"text":"失败测试在 src/lib.rs:42，我来修复。"}}}),
        json!({"type":"assistant/chunk","seq":7,"data":{"chunk":{"type":"block-end","index":1}}}),
        json!({"type":"tool/call","seq":8,"data":{"name":"bash","arguments":"{\"command\":\"cargo test\"}"}}),
        json!({"type":"tool/result","seq":9,"data":{"status":"ok","result":"tests passed (2 suites)"}}),
        json!({"type":"assistant/chunk","seq":10,"data":{"chunk":{"type":"usage","usage":{"inputTokens":12010,"outputTokens":6,"cacheReadTokens":7552}}}}),
        json!({"type":"assistant/message","seq":11,"data":{"message":{"role":"assistant","content":[{"type":"text","text":"修复完成，测试应当通过 ✓"}]}}}),
    ];
    for e in events {
        app.transcript.apply(&e);
    }
    app.state = RunState::Idle;
    app.status = "demo seeded".into();
}
