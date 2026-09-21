#!/usr/bin/env bash
# SC-12：离线端到端冒烟。
#
# 验证的是**安装后的应用**能启动、服务就绪、对话走完一轮——而不是
# `target/release/` 里的裸二进制。双击能用才算数。
#
# 全程不碰真实 Kiro：上游是本地回环替身。这条脚本必须能在一台没有凭据、
# 没有外网的机器上跑完，否则它在 CI 上就是个摆设。
#
# 用法：
#   bash scripts/desktop-smoke.sh <已安装应用的可执行文件路径>
# 例：
#   bash scripts/desktop-smoke.sh "/Applications/kiro.rs.app/Contents/MacOS/kiro-rs-desktop"
set -euo pipefail

APP="${1:?用法: desktop-smoke.sh <已安装应用的可执行文件路径>}"
[ -x "$APP" ] || { echo "FAIL: $APP 不可执行"; exit 1; }

WORK="$(mktemp -d)"
UPSTREAM_PORT=18771
APP_PORT=18772
PIDS=()

cleanup() {
  for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT

step() { printf '  %s\n' "$*"; }
fail() { echo "FAIL: $*"; exit 1; }

# --noproxy "*"：CI 与公司网络常设 http_proxy，而这里的目标全是回环。
# 不加的话 curl 会把 127.0.0.1 的请求也送去代理，表现成「服务没起来」。
echo "离线冒烟 · 工作目录 $WORK"

# ---- 1. 起替身上游 ----
python3 "$(dirname "$0")/fake_upstream.py" --port "$UPSTREAM_PORT" --out "$WORK/upstream.log" &
PIDS+=($!)
for _ in $(seq 1 40); do
  nc -z 127.0.0.1 "$UPSTREAM_PORT" 2>/dev/null && break
  sleep 0.1
done
nc -z 127.0.0.1 "$UPSTREAM_PORT" 2>/dev/null || fail "替身上游没起来"
step "替身上游就绪（:$UPSTREAM_PORT）"

# ---- 2. 准备一个已初始化的数据目录 ----
#
# 配置里直接给出 adminApiKey = 已初始化，跳过首次认领——那条路由
# F3/F4 的测试覆盖，冒烟要验的是对话链路。
cat > "$WORK/config.json" <<JSON
{
  "host": "127.0.0.1",
  "port": $APP_PORT,
  "apiKey": "sk-kiro-rs-smoke",
  "adminApiKey": "smoke-admin-password"
}
JSON
echo '[]' > "$WORK/credentials.json"

# 网关配置：一个指向替身的 Anthropic 上游。
# allowPrivateNetwork 必须开——网关默认拒绝回环地址（SSRF 防御），
# 而这里的回环正是我们要的。
# 注意：文件里存的是 GatewayConfig **本身**，不带 revision/config 包装。
# 套错一层不会报错——所有字段都有 serde(default)，于是静默变成空配置，
# 请求悄悄退回 Kiro 路。第一版 fixture 就是这么错的，表现是
# 「所有凭据均已禁用（0/0）」，与网关毫无关系的一句话。
cat > "$WORK/gateway.json" <<JSON
{
  "upstreams": [{
    "id": "smoke", "name": "smoke", "kind": "anthropic", "enabled": true,
    "weight": 10, "baseUrl": "http://127.0.0.1:$UPSTREAM_PORT",
    "apiKey": "unused", "hasApiKey": true, "allowPrivateNetwork": true
  }],
  "models": [{
    "id": "smoke-model",
    "bindings": [{
      "id": "b1", "upstreamId": "smoke", "upstreamModel": "smoke-model",
      "enabled": true, "priorityTier": 1, "weight": 10,
      "contextWindow": 200000, "maxOutputTokens": 8192,
      "billingUnit": "USD",
      "costPrices": {"currency": "USD", "input": "0.000003", "output": "0.000015", "cacheRead": "0.000001", "cacheWrite": "0.000004"},
      "sellPrices": {"currency": "USD", "input": "0.000006", "output": "0.000030", "cacheRead": "0.000002", "cacheWrite": "0.000008"}
    }]
  }]
}
JSON

# ---- 3. 起应用（headless：只起代理，不开窗口）----
# NO_PROXY：替身上游在回环上，而 reqwest 开着 system-proxy——CI 与公司
# 网络常设 http_proxy，不排除回环的话网关会把本地请求送去代理，表现成
# 「every eligible route failed」。
#
# 这不是冒烟在绕开问题：真实部署里把上游配在回环或内网时同样需要
# NO_PROXY，那是标准做法。（顺带记一笔：gateway 的 build_client 没有
# 对 allowPrivateNetwork 的上游自动绕过代理，值得单独看一眼——不在
# 本特性范围内。）
KIRO_DATA_DIR="$WORK" KIRO_HEADLESS=1 \
  NO_PROXY="127.0.0.1,localhost" no_proxy="127.0.0.1,localhost" \
  "$APP" > "$WORK/app.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 100); do
  nc -z 127.0.0.1 "$APP_PORT" 2>/dev/null && break
  sleep 0.1
done
nc -z 127.0.0.1 "$APP_PORT" 2>/dev/null || {
  echo "--- app.log ---"; tail -20 "$WORK/app.log"
  fail "应用没在 10 秒内监听 :$APP_PORT"
}
step "应用就绪（:$APP_PORT）"

BASE="http://127.0.0.1:$APP_PORT"

# ---- 4. 给系统 Key 立一个 USD 账户 ----
#
# 网关按钱计费的路线要求 Key 先有对应币种的账户，没有就 permission_error。
# 这是账本的正确行为（记不了账就不能收费），不是需要绕开的障碍——
# 冒烟照着真实流程做一遍。
curl -sS --noproxy "*" -X PUT "$BASE/api/admin/client-keys/0/budgets" \
  -H "x-api-key: smoke-admin-password" \
  -H 'content-type: application/json' \
  -d '{"unit":"USD","limit":"100","enforcement":"hard","maxInFlight":4,"maxPending":8}' \
  > /dev/null || fail "无法为系统 Key 立户"
step "系统 Key 已立 USD 账户"

# ---- 5. 走完一轮对话 ----
RESP="$(curl -sS --noproxy "*" --max-time 20 -N "$BASE/v1/messages" \
  -H "x-api-key: sk-kiro-rs-smoke" \
  -H 'content-type: application/json' \
  -d '{"model":"smoke-model","max_tokens":64,"stream":true,
       "messages":[{"role":"user","content":"ping"}]}')" || fail "请求失败"

grep -q 'content_block_delta' <<<"$RESP" || { echo "$RESP" | head -5; fail "没有文本增量"; }
# 替身输出真正的 UTF-8（ensure_ascii=False），所以这里检查的是
# 「多字节字节序列原样穿过了整条链路」，而不是「\uXXXX 转义原样穿过」。
grep -q '离线冒烟通过'          <<<"$RESP" || fail "多字节文本没有完整回传"
grep -q 'message_stop'          <<<"$RESP" || fail "流没有正常收尾"
step "对话走完一轮，文本完整"

# ---- 6. 用量确实一路传到了客户端（SC-9 的链路证据）----
grep -q 'output_tokens' <<<"$RESP" || fail "用量没有随流回传"
step "用量随流回传"

# ---- 7. 全程没碰真实上游 ----
grep -q 'POST' "$WORK/upstream.log" || fail "替身上游没收到请求——流量去哪了？"
step "上游流量全部落在替身上"

# ---- 8. 管理面仍然要鉴权（冒烟顺手守一道）----
code="$(curl -sS --noproxy "*" -o /dev/null -w '%{http_code}' "$BASE/api/admin/credentials")"
[ "$code" = "401" ] || fail "未认证访问 admin 得到 $code，应为 401"
step "管理面仍然要鉴权"

echo "PASS: 离线冒烟通过"
