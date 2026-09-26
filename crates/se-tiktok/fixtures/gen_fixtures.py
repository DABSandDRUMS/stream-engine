#!/usr/bin/env python3
"""Regenerate the binary fixtures from the upstream schema, independently of our Rust types.

The frames are encoded by `protoc --encode` against the community-maintained .proto files, so
the tests catch a wrong field number in `src/proto.rs` (encoding and decoding with our own
prost structs could not). Usage:

    git clone https://github.com/isaackogan/TikTok-Webcast-Protobuf
    python3 gen_fixtures.py TikTok-Webcast-Protobuf/src/slim/v3

Fixtures were generated from commit cf7bcd49 (2026-07-22). The payloads are synthetic
(invented users and ids): no real captured frames are published in the test data of
TikTok-Live-Connector, TikTokLive or TikTok-Webcast-Protobuf.
"""

import gzip
import os
import subprocess
import sys

MSG = "webcast/model/message/messages.proto"
IM = "webcast/synthetic_proto.proto"
SHARED = "webcast/shared/message.proto"
ROOM = 7140000000000000001


def encode(root, proto, type_name, text):
    return subprocess.run(
        ["protoc", "-I", root, f"--encode={type_name}", proto],
        input=text.encode(), capture_output=True, check=True,
    ).stdout


def esc(b):
    return '"' + "".join(f"\\{c:03o}" for c in b) + '"'


def common(method, msg_id, key=None):
    dt = f' display_text {{ key: "{key}" }}' if key else ""
    return f'common {{ method: "{method}" msg_id: {msg_id} room_id: {ROOM} create_time: 1758835200000{dt} }}'


def user(uid, nick, handle, extra=""):
    return f'id: {uid} nickname: "{nick}" display_id: "{handle}" {extra}'


def event(root, method, msg_id, body, history=False):
    payload = encode(root, MSG, f"webcast.model.message.{method}", f"{common(method, msg_id)} {body}")
    hist = " is_history: true" if history else ""
    return f'messages {{ method: "{method}" payload: {esc(payload)} msg_id: {msg_id}{hist} }}'


def social(root, msg_id, key, body):
    method = "WebcastSocialMessage"
    payload = encode(root, MSG, f"webcast.model.message.{method}", f"{common(method, msg_id, key)} {body}")
    return f'messages {{ method: "{method}" payload: {esc(payload)} msg_id: {msg_id} }}'


def fetch_result(root, messages, extra):
    return encode(root, SHARED, "webcast.shared.message.ProtoMessageFetchResult", " ".join(messages) + " " + extra)


def push_frame(root, log_id, result):
    gz = gzip.compress(result, mtime=0)
    text = f'log_id: {log_id} headers {{ key: "compress_type" value: "gzip" }} payload_encoding: "pb" payload_type: "msg" payload: {esc(gz)}'
    return encode(root, IM, "webcast.im.WebcastPushFrame", text)


def main():
    root = sys.argv[1]
    out = os.path.dirname(os.path.abspath(__file__))
    fan = user(6800000000000000001, "Drum Fan 🥁", "drumfan42", "follow_info { follow_status: 1 }")
    mod = user(6800000000000000002, "Snare Queen", "snarequeen", "user_attr { is_admin: true }")
    rose = 'gift { id: 5655 type: 1 diamond_count: 1 name: "Rose" }'
    lion = 'gift { id: 6369 type: 2 diamond_count: 29999 name: "Lion" }'
    msgs = [
        event(root, "WebcastChatMessage", 7400000000000000001,
              f'user {{ {fan} }} content: "hello from tiktok\\n" user_identity {{ is_follower_of_anchor: true is_subscriber_of_anchor: true }}'),
        event(root, "WebcastGiftMessage", 7400000000000000002,
              f'gift_id: 6369 repeat_count: 1 repeat_end: 1 group_id: 2001 user {{ {mod} }} {lion}'),
        event(root, "WebcastGiftMessage", 7400000000000000003,
              f'gift_id: 5655 repeat_count: 1 repeat_end: 0 group_id: 1001 user {{ {fan} }} {rose}'),
        event(root, "WebcastGiftMessage", 7400000000000000004,
              f'gift_id: 5655 repeat_count: 2 repeat_end: 0 group_id: 1001 user {{ {fan} }} {rose}'),
        event(root, "WebcastLikeMessage", 7400000000000000005, f"count: 15 total: 12000 user {{ {fan} }}"),
        social(root, 7400000000000000006, "pm_main_follow_message_viewer_2", f"user {{ {mod} }} action: 1"),
        social(root, 7400000000000000007, "pm_mt_guidance_share", f"user {{ {fan} }} action: 3"),
        event(root, "WebcastMemberMessage", 7400000000000000008, f"user {{ {mod} }} action: 1 member_count: 57"),
        event(root, "WebcastSubNotifyMessage", 7400000000000000009, f"user {{ {fan} }} sub_month: 3"),
        event(root, "WebcastGiftMessage", 7400000000000000010,
              f'gift_id: 5655 repeat_count: 5 repeat_end: 1 group_id: 1001 user {{ {fan} }} {rose}'),
        event(root, "WebcastLikeMessage", 7400000000000000011, f"count: 10 total: 12010 user {{ {mod} }}"),
        event(root, "WebcastRoomUserSeqMessage", 7400000000000000012, "total: 1234 total_user: 5678"),
        'messages { method: "WebcastLinkMicBattle" payload: "\\377\\377\\377" msg_id: 7400000000000000013 }',
        event(root, "WebcastChatMessage", 7400000000000000014, f'user {{ {fan} }} content: "old history line"', history=True),
    ]
    events = fetch_result(root, msgs, 'cursor: "t-1758835200000_r-1_d-1_u-1" internal_ext: "fetch_time:1758835200000|start:0|seq:42" need_ack: true')
    end = fetch_result(root, [event(root, "WebcastControlMessage", 7400000000000000099, "action: 3")], 'cursor: "c2" need_ack: false')
    sign = fetch_result(
        root,
        [event(root, "WebcastRoomUserSeqMessage", 7400000000000000100, "total: 99")],
        'cursor: "t-1758835200000_r-1" internal_ext: "fetch_time:1758835200000|seq:1" fetch_interval: 1000 '
        'push_server: "wss://webcast-ws.tiktok.com/webcast/im/ws_proxy/ws_reuse_supplement/" '
        'route_params { key: "wrss" value: "Zm9vYmFy" } is_first: true',
    )
    files = {
        "push_frame_events.bin": push_frame(root, 7400000000000000123, events),
        "push_frame_end.bin": push_frame(root, 7400000000000000124, end),
        "sign_response.bin": sign,
    }
    for name, data in files.items():
        with open(os.path.join(out, name), "wb") as f:
            f.write(data)
        print(f"{name}: {len(data)} bytes")


if __name__ == "__main__":
    main()
