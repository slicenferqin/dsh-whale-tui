#!/usr/bin/env bash
# 渲染冒烟：--dump-frame 输出确定性演示帧，走完整绘制管道
# （主题量化 → transcript 渲染/换行 → 布局 → 缓冲），不依赖运行时与 API key。
# 任何一环崩了（panic、布局错位、关键区域消失）这里都会立刻红。
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=target/debug/dsh-whale-tui
cargo build --quiet

frame="$("$BIN" --dump-frame 100x30)"

check() { # check <描述> <固定字符串>
    if ! grep -qF -- "$2" <<<"$frame"; then
        echo "SMOKE FAIL: $1（找不到 '$2'）" >&2
        echo "$frame" >&2
        exit 1
    fi
    echo "ok: $1"
}

check "状态栏模型名" "deepseek-v4-flash"
check "scrollback 工具块" "cargo test --all"
check "scrollback 折叠计数" "… 10 more lines"
check "composer 边框" "╭"
check "composer 提示符" "›"
check "快捷键提示行" "Ctrl+P:commands"
# 小尺寸帧也不能崩（布局 clamp 路径）。12 行高时 composer 只剩边框，
# 提示符行被裁掉是已知的极小屏行为，这里只断言边框在。
small="$("$BIN" --dump-frame 40x12)"
grep -qF "╭" <<<"$small" || { echo "SMOKE FAIL: 40x12 小帧缺 composer 边框" >&2; exit 1; }
echo "ok: 40x12 小帧渲染"

echo "SMOKE PASS"
