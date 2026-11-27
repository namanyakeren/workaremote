#!/usr/bin/env python3
"""Uji ujung-ke-ujung tanpa browser.

Membuka sesi lewat API panel, menyambung sebagai viewer, lalu menyimpan satu
frame ke PNG. Kalau perintah ini menghasilkan gambar desktop, berarti agent,
relay, dan protokol tile semuanya sehat.

    pip install websockets pillow
    python3 scripts/smoke.py http://127.0.0.1:8080 PANEL_KEY 123456789 [password]
"""
import asyncio
import io
import json
import struct
import sys
import urllib.request

import websockets
from PIL import Image


async def main() -> int:
    base = (sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8080").rstrip("/")
    key = sys.argv[2]
    agent_id = sys.argv[3]
    password = sys.argv[4] if len(sys.argv) > 4 else None

    body = {"agent_id": agent_id}
    if password:
        body["password"] = password
    req = urllib.request.Request(
        base + "/api/v1/sessions",
        data=json.dumps(body).encode(),
        headers={"Authorization": "Bearer " + key, "Content-Type": "application/json"},
    )
    created = json.load(urllib.request.urlopen(req))
    print("sesi:", created["session_id"])

    url = created["url"].replace("https://", "wss://").replace("http://", "ws://")
    host, frag = url.split("/v#", 1)
    q = dict(p.split("=", 1) for p in frag.split("&"))

    canvas = None
    size = None
    async with websockets.connect(
        f"{host}/ws/viewer?sid={q['sid']}&token={q['token']}", max_size=None
    ) as ws:
        while True:
            msg = await asyncio.wait_for(ws.recv(), timeout=45)
            if isinstance(msg, str):
                ev = json.loads(msg)
                print("event:", ev["t"], ev.get("reason", ""))
                if ev["t"] == "need_password":
                    print("ditolak: password salah atau belum diberikan")
                    return 1
                if ev["t"] in ("denied", "end"):
                    return 1
                if ev["t"] == "screen":
                    mons = ev["monitors"]
                    m = next((x for x in mons if x["primary"]), mons[0])
                    size = (m["w"], m["h"])
                    canvas = Image.new("RGB", size, "black")
                    print("layar:", size, "monitor:", [x["name"] for x in mons])
                continue

            kind = msg[0]
            if kind != 1:
                continue
            count = struct.unpack_from("<H", msg, 1)[0]
            off = 3
            for _ in range(count):
                x, y, w, h, ln = struct.unpack_from("<HHHHI", msg, off)
                off += 12
                if canvas is not None and ln > 0:
                    tile = Image.open(io.BytesIO(msg[off : off + ln])).convert("RGB")
                    canvas.paste(tile, (x, y))
                off += ln
            if canvas is not None and count:
                canvas.save("/tmp/worka-frame.png")
                print("frame tersimpan di /tmp/worka-frame.png")
                return 0
    return 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
