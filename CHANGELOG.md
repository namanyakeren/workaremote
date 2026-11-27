# Changelog

## 1.9.0

Sembilan fitur baru di atas 0.1.0.

1. **Secure desktop Windows.** Belum selesai; service + helper per-sesi masih
   diperlukan dan tidak diklaim tersedia di rilis ini.
2. **Ctrl+Alt+Del.** Belum tersedia sampai service SYSTEM dan pengujian Windows.
3. **Wayland.** Capture menggunakan `grim`, input menggunakan `ydotool`/`wtype`;
   butuh daemon ydotool dan saat ini menangkap virtual screen sebagai satu output.
4. **Clipboard dua arah.** Teks tersinkron otomatis, dengan tombol matikan.
5. **Transfer file.** Unggah lewat drag & drop / pemilih file, unduh dengan
   mengetik path di mesin tujuan. Ada progres dan batas ukuran.
6. **Audio sistem Linux.** Capture PCM 48 kHz mono melalui utilitas audio host;
   Windows belum didukung.
7. **Multi-monitor satu tampilan.** Opsi "Semua monitor" menggabungkan seluruh
   desktop virtual jadi satu gambar, di samping pilihan per monitor.
8. **H.264.** Encoder openh264 + decoder WebCodecs di browser, dengan fallback
   otomatis ke JPEG kalau browser atau mesin tidak mendukung.
9. **Ctrl+W / Ctrl+T / Ctrl+N.** Keyboard Lock API saat layar penuh, pada browser
   yang mengizinkan penguncian shortcut tersebut.

Perubahan lain:

- Versi 1.9.0 pada workspace dan health endpoint.
- Protokol menambahkan frame biner untuk file, audio, dan video.
- Catatan: helper secure-desktop Windows, audio Windows, serta verifikasi runtime
  H.264/WebCodecs belum selesai; lihat batasan README.

## 0.1.0

Rilis pertama: capture JPEG per tile, input mouse/keyboard, unattended password,
konfirmasi terima/tolak, viewer browser, API panel, installer Windows dan Linux.
