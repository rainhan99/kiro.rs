#!/usr/bin/env python3
"""离线冒烟用的上游替身：说 Anthropic Messages 协议，回一段 SSE。

**不碰网络、不碰真实 Kiro。** SC-12 的整个意义就在这里：冒烟必须能在
一台没有凭据、没有外网的机器上跑完。

为什么走网关而不是 Kiro 路：Kiro 的端点 URL 是写死在
`src/kiro/endpoint/` 里的，没有配置项能把它指到别处；而网关的上游
`base_url` 本来就是配置出来的，还有 `allowPrivateNetwork` 开关专门
用于这种回环场景。用既有的配置面比为测试开一个后门好。

这条路覆盖的是：鉴权 → 网关准入 → 上游调用 → SSE 回传 → 用量落账。
不覆盖 Kiro 的转换器与 event-stream 解码器——那两块有自己的 1200+ 单测。
"""
import argparse
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


def sse(event: str, data: dict) -> bytes:
    return f"event: {event}\ndata: {json.dumps(data, ensure_ascii=False)}\n\n".encode()


class Handler(BaseHTTPRequestHandler):
    log_path = None

    def do_POST(self):  # noqa: N802
        length = int(self.headers.get("content-length", 0))
        body = self.rfile.read(length)
        if Handler.log_path:
            with open(Handler.log_path, "a") as f:
                f.write(f"POST {self.path} {len(body)}\n")

        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()

        for chunk in [
            sse("message_start", {
                "type": "message_start",
                "message": {
                    "id": "msg_smoke", "type": "message", "role": "assistant",
                    "model": "smoke-model", "content": [], "stop_reason": None,
                    "usage": {"input_tokens": 12, "output_tokens": 0},
                },
            }),
            sse("content_block_start", {
                "type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": ""},
            }),
            # 故意用多字节字符：要验证分片缝合，而不只是「有字出来」。
            sse("content_block_delta", {
                "type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": "离线冒烟通过"},
            }),
            sse("content_block_stop", {"type": "content_block_stop", "index": 0}),
            sse("message_delta", {
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 4},
            }),
            sse("message_stop", {"type": "message_stop"}),
        ]:
            self.wfile.write(chunk)
            self.wfile.flush()

    def log_message(self, *_args):
        pass


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()

    Handler.log_path = args.out
    open(args.out, "w").close()
    HTTPServer(("127.0.0.1", args.port), Handler).serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())
