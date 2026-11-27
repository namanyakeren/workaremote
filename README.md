# Worka Remote

Remote desktop self-hosted ala AnyDesk: agent Rust di mesin tujuan, viewer di
browser, dan relay yang bisa dipanggil dari central panel perusahaan.

```
┌──────────────┐   WSS (outbound)   ┌──────────────┐   WSS   ┌──────────────────┐
│  agent       │ ─────────────────▶ │   relay      │ ◀────── │  browser viewer  │
│  Windows 7+  │ ◀───────────────── │  (Rust/axum) │ ──────▶ │  /v#sid=..&token │
│  Linux X11   │  input, tile JPEG  │              │         └──────────────────┘
└──────────────┘                    └──────┬───────┘
                                           │ REST + Bearer key
                                    ┌──────▼───────┐
                                    │ central panel│  window.open(url)
                                    └──────────────┘
```

Agent selalu **mendial keluar**, jadi tidak perlu port forwarding atau IP publik
di mesin tujuan. Browser tidak butuh plugin, WebRTC, atau STUN/TURN.

## Isi repo

| Path | Isi |
|---|---|
| `proto/` | format pesan (JSON kontrol + binary tile JPEG) |
| `agent/` | program di mesin tujuan: capture, input, dialog izin, password |
| `server/` | relay + API panel + viewer HTML (satu binary) |
| `install/` | skrip pemasangan Windows dan Linux |
| `scripts/smoke.py` | uji ujung-ke-ujung tanpa browser |

## Build

Butuh **Rust 1.77.1** (sudah dipin di `rust-toolchain.toml`). rustc 1.78 ke atas
membuang dukungan Windows 7, jadi jangan naikkan versi tanpa melepas target Win7.

```bash
cargo build --release
# hasil: target/release/worka-server, target/release/worka-agent
```

Windows 7/8/10/11:

```powershell
rustup target add x86_64-pc-windows-gnu   # atau -msvc dengan Visual Studio Build Tools
cargo build --release --target x86_64-pc-windows-gnu -p worka-agent
```

Untuk mesin 32-bit pakai target `i686-pc-windows-msvc` / `i686-pc-windows-gnu`.

## Menjalankan relay

```bash
WORKA_LISTEN=0.0.0.0:8080 \
WORKA_PANEL_KEY=$(openssl rand -hex 32) \
WORKA_ENROLL=$(openssl rand -hex 16) \
WORKA_PUBLIC_URL=https://remote.perusahaan.com \
WORKA_STATE=/var/lib/worka/worka-state.json \
./worka-server
```

| Env | Arti |
|---|---|
| `WORKA_LISTEN` | alamat bind, default `0.0.0.0:8080` |
| `WORKA_PANEL_KEY` | Bearer token untuk API panel. Kosong = API dimatikan |
| `WORKA_ENROLL` | token yang harus dikirim agent saat mendaftar. Kosong = siapa pun boleh daftar |
| `WORKA_PUBLIC_URL` | URL publik, dipakai untuk menyusun link viewer |
| `WORKA_STATE` | file JSON berisi daftar agent + secret-nya |

TLS diterminasi di reverse proxy. Contoh nginx:

```nginx
server {
    listen 443 ssl http2;
    server_name remote.perusahaan.com;
    ssl_certificate     /etc/letsencrypt/live/remote.perusahaan.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/remote.perusahaan.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_read_timeout 3600s;   # sesi remote panjang
        proxy_buffering off;        # penting: tile harus langsung lewat
    }
}
```

## Memasang agent

Windows (PowerShell, tanpa admin, autostart per user):

```powershell
powershell -ExecutionPolicy Bypass -File .\windows-install.ps1 `
    -Server https://remote.perusahaan.com -Token <WORKA_ENROLL>
```

Linux (X11):

```bash
sudo ./linux-install.sh https://remote.perusahaan.com <WORKA_ENROLL>
```

Setelah itu `worka-agent id` menampilkan 9 digit ID mesin. Untuk unattended:

```bash
worka-agent set-password      # simpan hash argon2, koneksi langsung masuk
worka-agent clear-password    # kembali ke mode konfirmasi terima/tolak
```

Tanpa password, setiap koneksi memunculkan dialog Terima/Tolak di mesin tujuan
(MessageBox di Windows, zenity/kdialog di Linux).

## Integrasi central panel

Panel cukup memanggil satu endpoint lalu membuka tab baru.

```js
const res = await fetch('https://remote.perusahaan.com/api/v1/sessions', {
  method: 'POST',
  headers: {
    'Authorization': 'Bearer ' + PANEL_KEY,
    'Content-Type': 'application/json',
  },
  body: JSON.stringify({ agent_id: '123456789', password: 'opsional' }),
});
const { url } = await res.json();
window.open(url, '_blank', 'noopener');
```

Endpoint lain:

```
GET  /api/v1/health                 -> {"ok":true}
GET  /api/v1/agents                 -> daftar mesin + status online
POST /api/v1/sessions               -> {"session_id","token","url"}
```

Contoh isi `GET /api/v1/agents`:

```json
{"agents":[{"agent_id":"123456789","hostname":"KASIR-01","os":"windows x86_64",
            "version":"0.1.0","last_seen":1795756469,"online":true}]}
```

Token viewer sekali pakai dan kedaluwarsa 5 menit; link ditaruh di fragment
(`#sid=..&token=..`) supaya tidak ikut terkirim ke server log atau header
`Referer`.

## Alur satu sesi

1. Panel `POST /api/v1/sessions` → dapat `url`.
2. Browser buka `url`, relay kirim `session_request` ke agent.
3. Agent cek password (argon2). Cocok → terima. Belum diset → dialog Terima/Tolak.
   Password salah → viewer diminta memasukkan password.
4. Relay menyambungkan keduanya. Agent kirim layout monitor, lalu hanya tile yang
   berubah (256×256, JPEG q72, maksimal 480 KB per frame).
5. Input browser (mouse, wheel, keyboard) diteruskan apa adanya ke agent.

## Keamanan

- Semua trafik lewat TLS (WSS). Tidak ada port yang dibuka di mesin tujuan.
- Password unattended disimpan sebagai hash argon2id di mesin itu sendiri,
  tidak pernah disimpan di server.
- Agent pertama kali mendaftar memakai `WORKA_ENROLL`; server menyimpan secret
  40 karakter per mesin dan menolak ID yang sudah dipakai mesin lain.
- Token viewer single-use, TTL 5 menit, terikat ke satu sesi.
- API panel butuh Bearer key; tanpa `WORKA_PANEL_KEY` API mati total.

Yang **tidak** ada: enkripsi end-to-end. Relay melihat tile dan input. Kalau itu
masalah, jalankan relay di jaringan/perusahaan sendiri (memang itu desainnya).

## Fitur 1.9.0 dan batasan

Tersedia pada jalur yang didukung:

- Clipboard teks dua arah (browser harus mengizinkan Clipboard API; Ctrl+V
  diteruskan sebagai teks saat event paste tersedia).
- Upload file viewer ke folder Downloads/Worka Remote pada agent. Unduhan dari
  mesin tujuan belum tersedia di viewer.
- Multi-monitor: pilihan Semua monitor tersedia bila lebih dari satu monitor.
- OpenH264 encoding tersedia dengan fitur `h264`; decoder WebCodecs belum
  divalidasi runtime, sehingga JPEG tetap jalur default yang teruji.
- Audio sistem Linux melalui `parec`, `pw-record`, atau `arecord`.
- Ctrl+W/T/N: Keyboard Lock saat layar penuh, jika didukung browser.

Belum didukung / ada prasyarat:

- **Windows UAC/login dan Ctrl+Alt+Del** belum tersedia. Perintah
  `worka-agent service` sengaja menolak operasi; service SYSTEM + helper aktif
  per sesi belum diimplementasikan. `SendSAS` tidak diekspos sampai itu ada.
- **Wayland** memerlukan `grim` dan `ydotool` (beserta daemon). Capture adalah
  satu virtual screen; input tool invocation belum diverifikasi lintas compositor.
- **Audio Windows** belum tersedia; tombol Audio akan menunjukkan unsupported.
- **WebRTC** belum digunakan: H.264 dibawa lewat relay WebSocket.
- Build Windows tanpa MinGW C++ atau MSVC gunakan `--no-default-features` dan
  tetap memakai JPEG. H.264 memerlukan toolchain C++ yang sesuai target.
- Browser dapat tetap menangkap `Ctrl+W/T/N`; Keyboard Lock tidak didukung oleh
  semua browser/key combinations.

## Uji

```bash
cargo test --workspace                       # unit test + uji relay
cargo build --release --workspace           # H.264 OpenH264 aktif
cargo build --release -p worka-agent --no-default-features # fallback JPEG
cargo test -p worka-agent -- --ignored       # butuh display X11 asli
python3 scripts/smoke.py http://127.0.0.1:8080 PANEL_KEY 123456789 password
```

`smoke.py` membuka sesi sungguhan, menerima tile, dan menyimpan
`/tmp/worka-frame.png` — cara tercepat memastikan agent di mesin lain benar-benar
mengirim gambar.
