# HighVi

HighVi là MVP desktop cho Ubuntu: dán URL trang video được phép truy cập, để `yt-dlp` tìm playlist HLS (`.m3u8`), rồi tải và remux thành MP4 với đúng `Referer`.

## Phạm vi MVP

- Giao diện Tauri v2 bằng HTML/CSS/JavaScript thuần; Rust là backend và nơi giữ toàn bộ kiểm tra an toàn.
- Chỉ nhận URL HTTPS công khai; chặn hostname/IP local, private và reserved trước khi gọi công cụ ngoài.
- Gọi `yt-dlp` bằng danh sách đối số, không dùng shell; tên file không thể chứa đường dẫn.
- Bỏ qua toàn bộ config/plugin/cookie/`--exec` bên ngoài của `yt-dlp`; hành vi app không phụ thuộc config cá nhân.
- Tìm HLS, từ chối media được đánh dấu DRM, làm mới playlist trước khi tải, hiển thị tiến độ có cấu trúc.
- Mỗi lần chỉ chạy một download. File được tải vào thư mục job riêng rồi publish atomically, không thay thế, vào `~/Downloads/HighVi`; file đã có không bị ghi đè.
- Không nhập cookie, không sao chép phiên trình duyệt, không vượt đăng nhập/DRM/kiểm soát truy cập.

## Chạy trên Ubuntu

Máy hiện đã có Rust, Node.js, `yt-dlp` và FFmpeg. Tauri còn cần thư viện WebKitGTK hệ thống:

```bash
sudo apt update
sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
```

Sau đó, trong thư mục dự án:

```bash
npm install
npm run dev
```

Tạo gói `.deb` và AppImage:

```bash
npm run build
```

Chạy riêng kiểm thử lõi Rust (không cần WebKitGTK):

```bash
cargo test -p highvi-core
```

## Luồng xử lý

1. Rust xác thực URL trang và DNS, từ chối đích mạng không công khai.
2. Rust chạy `yt-dlp --dump-single-json --no-playlist --skip-download -- <URL>` trong tối đa 60 giây.
3. Rust parse JSON có giới hạn, chọn manifest HLS không DRM và kiểm tra URL/DNS của playlist.
4. Khi người dùng bấm tải, Rust lặp lại bước khám phá để tránh URL HLS đã hết hạn hoặc dữ liệu webview bị sửa.
5. Rust chạy `yt-dlp` với `--referer`, output template cố định và progress template; giao diện chỉ hiển thị sự kiện tiến độ đã parse.
6. Trên Linux, file hoàn tất được đổi tên atomically bằng `RENAME_NOREPLACE`; nếu tên đã tồn tại, file cũ không bị thay thế. Job HighVi bị gián đoạn được giữ để phục hồi và tự dọn sau 7 ngày.

## Tauri v2 hay Dioxus?

Tauri v2 hợp với MVP này vì UI nhỏ, backend Rust rõ ràng và có thể dùng webview có sẵn của Ubuntu. Dioxus Desktop cũng dùng webview và không làm mất yêu cầu phụ thuộc WebKitGTK, trong khi sẽ thêm một lớp UI Rust chưa cần thiết.

Tauri hỗ trợ desktop/mobile về mặt framework, nhưng chính chức năng này phụ thuộc vào tiến trình `yt-dlp` + FFmpeg. Trình duyệt web không được phép spawn tiến trình; Android/iOS cần cách đóng gói, sandbox, lưu file và chính sách phân phối riêng. Vì vậy bản đầu chỉ cam kết Ubuntu desktop; phần lõi Rust được tách thành crate để có thể tái sử dụng sau.

## Giới hạn an toàn đã biết

Kiểm tra DNS được thực hiện cho URL ban đầu và playlist đã trích xuất. Tuy nhiên `yt-dlp` có thể tự theo redirect hoặc tải các segment con; MVP chưa có proxy mạng riêng để ép chính sách IP trên từng request. Không nên dán URL từ nguồn không tin cậy. Một bản hardening sau có thể chạy mọi request qua proxy lọc địa chỉ hoặc sandbox mạng.

HighVi không khẳng định người dùng có quyền tải bất kỳ nội dung cụ thể nào. Trách nhiệm quyền sử dụng thuộc về người dùng.

## License

HighVi is available under the [MIT License](LICENSE).
