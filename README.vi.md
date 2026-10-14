<div align="center">
  <img src="docs/assets/matrix-banner.svg" alt="HighVi — mưa mã xanh" width="100%" />

  <p>
    <a href="README.md">English</a> ·
    <strong>Tiếng Việt</strong> ·
    <a href="README.ja.md">日本語</a>
  </p>

  <p>
    <a href="https://tauri.app/"><img src="https://img.shields.io/badge/Tauri-v2-00ff41?style=flat-square&logo=tauri&logoColor=00ff41&labelColor=000000" alt="Tauri v2" /></a>
    <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-2024-00ff41?style=flat-square&logo=rust&logoColor=00ff41&labelColor=000000" alt="Rust 2024" /></a>
    <img src="https://img.shields.io/badge/Ubuntu-desktop-00ff41?style=flat-square&logo=ubuntu&logoColor=00ff41&labelColor=000000" alt="Ubuntu desktop" />
    <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-00ff41?style=flat-square&labelColor=000000" alt="Giấy phép MIT" /></a>
  </p>

  <p><code>&gt; tỉnh dậy đi... video đang chờ bạn_</code></p>
</div>

## 🟩 HighVi là gì?

Nhiều video trên mạng không phải là một file lớn. Chúng được cắt thành **rất nhiều mảnh nhỏ**, và có một danh sách nhỏ gọi là **playlist** (file có đuôi `.m3u8`) ghi lại chỗ của từng mảnh. Bạn cứ hình dung nó như một **tấm bản đồ kho báu**.

HighVi đọc tấm bản đồ đó, đi gom từng mảnh rồi ghép lại thành **một file MP4** để bạn xem ở đâu cũng được.

<div align="center">
  <img src="docs/assets/screenshot-vi.png" alt="Cửa sổ HighVi với giao diện Matrix màu xanh" width="720" />
  <br />
  <sub>Cửa sổ HighVi (dữ liệu chỉ để minh họa).</sub>
</div>

## 🕹️ Hai cách dùng

| Chế độ | Bạn dán vào | Dùng khi |
| --- | --- | --- |
| **Link trang video** | Link của trang có video | Trang web mà `yt-dlp` đã biết |
| **Link playlist (.m3u8)** | Link playlist **và** trang bạn tìm thấy nó | Bạn đã có sẵn link `.m3u8` |

HighVi nói được **English, Tiếng Việt và 日本語**. Bấm nút ở góc trên để chọn, HighVi sẽ nhớ lựa chọn của bạn.

**Sao chế độ “Link playlist” lại hỏi thêm trang web?**
Có những trang chỉ đưa bản đồ kho báu khi bạn nói mình đến từ đâu, giống như phải trình vé ở cửa. Lời nhắn đó gọi là **Referer**. HighVi chỉ nói địa chỉ "cổng chính" của trang (ví dụ `https://example.com/`). HighVi không bao giờ gửi phần còn lại của link, cookie hay tài khoản đăng nhập của bạn.

**Tìm link playlist ở đâu?** Trên trình duyệt, bấm <kbd>F12</kbd>, mở tab **Network**, bật video lên rồi gõ `m3u8` vào ô lọc. Chỉ làm vậy với video bạn được phép tải nhé.

## ✅ HighVi làm gì

1. Bạn dán link rồi bấm **Tải MP4**.
2. HighVi kiểm tra đó là link `https://`, rồi bật một **lớp bảo vệ mạng** riêng: mọi request mà `yt-dlp` gửi đi (trang, redirect, playlist, từng mảnh, khóa) đều phải tới địa chỉ internet công khai. Máy trong mạng nhà hay mạng công ty luôn bị chặn.
3. `yt-dlp` tìm tấm bản đồ kho báu, HighVi chọn hình đẹp nhất (và phần tiếng nếu tách riêng).
4. Các mảnh được tải về và ghép thành một file MP4 ngay trên máy bạn. Bạn thấy tiến độ, tốc độ, thời gian còn lại, và có thể bấm **Hủy** bất cứ lúc nào.
5. Video xong nằm trong **`~/Downloads/HighVi`**, đặt theo tên video. Nếu trùng tên, HighVi thêm ` (2)`. Không bao giờ ghi đè. Bấm **Mở thư mục** để xem.

## 🛑 Những điều HighVi không bao giờ làm

- Phá DRM, tường phí hay chuyện đăng nhập.
- Dùng cookie hay phiên đăng nhập trên trình duyệt của bạn.
- Giả làm trình duyệt để lách qua chặn bot.
- Chạy plugin hay lệnh phụ từ cấu hình `yt-dlp` của bạn.

> [!IMPORTANT]
> Chỉ tải video của bạn hoặc video bạn được phép tải.

## 🚀 Bắt đầu

Bạn cần **Ubuntu**, **Rust**, **Node.js**, **`yt-dlp`** và **FFmpeg**.

Cài các thư viện hệ thống mà Tauri cần:

```bash
sudo apt update
sudo apt install build-essential curl wget file libxdo-dev libssl-dev \
  libayatana-appindicator3-dev librsvg2-dev libwebkit2gtk-4.1-dev ffmpeg
```

Cài `yt-dlp` bằng trình quản lý gói hoặc từ [dự án chính thức](https://github.com/yt-dlp/yt-dlp), và nhớ cập nhật thường xuyên. Sau đó kiểm tra lại:

```bash
yt-dlp --version && ffmpeg -version && node --version && cargo --version
```

Chạy ứng dụng:

```bash
npm install
npm run dev     # mở HighVi
npm run build   # đóng gói .deb và AppImage
```

## 🧯 Khi có lỗi

| Mã lỗi | Nghĩa là gì | Thử làm gì |
| --- | --- | --- |
| `access_denied` / `host_firewall_block` | Trang web **từ chối** (HTTP 403). | Nếu bạn dán link `.m3u8` vào ô link trang video, hãy chuyển sang **Link playlist** và thêm trang chứa nó. Xem lại trang đã đúng chưa, hoặc lấy link playlist mới. Nếu trang bắt đăng nhập bằng trình duyệt thì HighVi đành chịu. |
| `unsupported_page` | `yt-dlp` không biết trang này, hoặc trình phát video được dựng bằng JavaScript. | Chọn **Link playlist** nếu bạn có link playlist. |
| `unsupported_playlist` | Link này không phải playlist mà HighVi đọc được. | Kiểm tra đó đúng là link `.m3u8`. |
| `media_not_found` | Video không còn nữa (HTTP 404/410). | Link có thể đã hết hạn, hãy lấy link mới. |
| `drm_not_supported` | Video bị khóa bằng DRM. | HighVi sẽ không mở khóa. |
| `authentication_required` | Phải đăng nhập mới xem được. | HighVi không dùng tài khoản của bạn. |
| `network_timeout` | Trang web trả lời quá lâu. | Thử lại sau nhé. |
| `unsafe_network_target` | Video định kết nối tới máy trong mạng nhà hay mạng công ty. | HighVi cố ý chặn lại. |
| `live_not_supported` | Đây là phát trực tiếp. | HighVi chỉ tải video đã phát xong. |
| `missing_dependency` | Chưa cài `yt-dlp` hoặc FFmpeg. | Cài vào rồi thử lại. |

## 🧠 Dành cho lập trình viên

```bash
HIGHVI_REQUIRE_E2E=1 cargo test --workspace --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo fmt --all -- --check
node --check ui/app.js && node --check ui/i18n.js && node --check ui/rain.js
node scripts/check-ui.mjs   # translation parity
```

```text
crates/highvi-core/   Lớp bảo vệ mạng (egress proxy), tìm video, chọn format, job tải, ghép MP4, lưu file an toàn
                      (unit test + test end-to-end với yt-dlp/FFmpeg thật và server HTTPS cục bộ)
src-tauri/            Lệnh Tauri: bắt đầu/hủy job, sự kiện tiến độ, mở thư mục
ui/                   Cửa sổ màu xanh: index.html, styles.css, app.js, i18n.js (English · Tiếng Việt · 日本語), rain.js
docs/assets/          Banner và ảnh chụp màn hình cho README
```

Cách HighVi giữ an toàn cho bạn và các giới hạn hiện tại được viết trong [SECURITY.md](SECURITY.md). Nói gọn: mọi request mà công cụ của HighVi gửi đi đều phải qua lớp bảo vệ chỉ cho phép địa chỉ HTTPS công khai, và FFmpeg không bao giờ ra mạng.

## 📜 Giấy phép

[MIT](LICENSE)

<div align="center"><sub><code>không có cái thìa nào cả — chỉ có các mảnh video_</code></sub></div>
