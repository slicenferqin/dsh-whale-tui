//! Scripted demo turn through the real event pipeline: no runtime, no API key.
//! Also the basis for --dump-frame style UI self-verification (see
//! docs/02-openma-teardown.md section 9).

use serde_json::json;

use crate::app::{App, RunState};

pub fn seed(app: &mut App) {
    let events = [
        json!({"type":"user/message","data":{"content":[{"type":"text","text":"查看这个仓库并修复失败的测试"}]}}),
        json!({"type":"assistant/chunk","data":{"text":"我先"}}),
        json!({"type":"assistant/chunk","data":{"text":"看一下仓库结构。"}}),
        json!({"type":"tool/call","data":{"name":"bash","input":{"command":"ls -la && git status"}}}),
        json!({"type":"tool/result","data":{"status":"ok","result":"(输出略) 12 files, 2 modified"}}),
        json!({"type":"assistant/chunk","data":{"text":"失败测试在 src/lib.rs:42，根因是断言写反了。"}}),
        json!({"type":"tool/call","data":{"name":"edit","input":{"path":"src/lib.rs","hunks":2}}}),
        json!({"type":"tool/result","data":{"status":"ok","result":"+2/-2"}}),
        json!({"type":"assistant/chunk","data":{"text":"修复完成，测试应当通过 ✓"}}),
        json!({"type":"assistant/message","data":{"text":"修复完成，测试应当通过 ✓"}}),
    ];
    for e in events {
        app.transcript.apply(&e);
    }
    app.state = RunState::Idle;
    app.status = "demo seeded".into();
}
