# Security model

## Trust boundaries

- **Webview → Rust:** nguồn (`page` hoặc `directHls`) luôn được kiểm tra lại trong command Rust; UI không phải security boundary. UI chỉ gửi nguồn và một job ID (chỉ dùng để gắn sự kiện tiến độ). UI không bao giờ nhận URL media/playlist (có thể chứa token): chỉ nhận tiêu đề, tiến độ và đường dẫn file đã lưu.
- **Network boundary (egress proxy):** mỗi job chạy một proxy `CONNECT` riêng trên `127.0.0.1`. `yt-dlp` chỉ ra mạng qua proxy này (`--proxy`; biến môi trường proxy bị xoá). Proxy tự phân giải tên miền, yêu cầu **mọi** địa chỉ trả về là địa chỉ công khai, rồi kết nối đúng tới địa chỉ đã kiểm tra — nên DNS rebinding không có khe hở giữa lúc kiểm tra và lúc kết nối. Mỗi redirect, variant playlist, segment, `EXT-X-KEY` là một tunnel mới nên đều bị kiểm tra. Proxy từ chối HTTP thường (chỉ HTTPS), từ chối loopback, mạng riêng, link-local, CGNAT, multicast, địa chỉ tài liệu/benchmark, IPv4-mapped/compatible, 6to4, Teredo, NAT64 trỏ vào IPv4 không công khai, và mọi IPv6 ngoài `2000::/3`.
- **FFmpeg không bao giờ ra mạng:** `yt-dlp` chạy với `--ffmpeg-location` trỏ tới đường dẫn không tồn tại, nên nó không thể giao stream cho FFmpeg (FFmpeg sẽ tự tải URL và bỏ qua proxy); stream không tải được bằng downloader gốc sẽ thất bại thay vì đi vòng. HighVi tự ghép MP4 bằng FFmpeg với `-protocol_whitelist file`, demuxer được chọn từ magic bytes (không để FFmpeg tự dò nội dung), `-c copy`.
- **Network → Rust:** JSON và progress của `yt-dlp` là dữ liệu không tin cậy. Metadata có giới hạn kích thước; format ID được kiểm tra để không thể là cú pháp chọn format; mọi `Referer` trong info JSON được rút về origin; progress chỉ nhận prefix/field cố định; stderr thô không được trả về UI; lỗi chỉ trả về error code và thông điệp cố định.
- **Rust → process:** không gọi shell; URL luôn sau `--`; mỗi lượt chạy dùng `--ignore-config`, `--no-plugin-dirs`, `--no-exec`, `--no-cookies`, `--no-cookies-from-browser`, `--no-cache-dir`. Môi trường bị xoá trừ `PATH`, locale, `TMPDIR` và CA bundle; `HOME` là thư mục job; luôn đặt `YTDLP_NO_PLUGINS=1`, `PYTHONNOUSERSITE=1`. Mỗi tiến trình con có process group riêng; huỷ/timeout/thoát app sẽ `SIGKILL` cả group.
- **Referer:** chỉ gửi origin của trang (`https://site.example/`), kể cả khi extractor ghi lại URL trang đầy đủ — như trình duyệt làm với request khác origin. Chế độ trang không gửi Referer khi tải trang, và không bao giờ dùng chính playlist làm Referer.
- **Rust → filesystem:** thư mục đích cố định là `Downloads/HighVi` (không theo symlink). Tên file lấy từ tiêu đề đã làm sạch. Mỗi job dùng thư mục riêng quyền `0700` có marker; yt-dlp chạy trong thư mục đó với đường dẫn tương đối. File hoàn chỉnh được publish bằng `RENAME_NOREPLACE`; trùng tên thì thêm ` (2)`, ` (3)`… — không bao giờ ghi đè. Thư mục job bị xoá khi xong, lỗi, huỷ, hoặc khi app nhận SIGTERM/SIGHUP/SIGINT; thư mục mồ côi (sau crash/SIGKILL) có marker hợp lệ và không đổi trong 24 giờ sẽ được tự dọn.
- **Live/DRM:** live stream bị từ chối (tránh tải vô hạn); format có DRM bị bỏ qua, chỉ còn DRM thì báo lỗi.

## Test-only code

Resolver cho fixture cục bộ và `--no-check-certificates` chỉ tồn tại dưới `#[cfg(test)]`; bản build thật không thể tạo ra chúng.

## Explicit non-goals

- Không giải mã hoặc vượt DRM.
- Không lấy cookie/token từ trình duyệt và không vượt đăng nhập, paywall hoặc quyền truy cập.
- Không vượt Cloudflare/anti-bot, không giả lập trình duyệt (`--impersonate`) và không chạy plugin bên thứ ba.
- Không ghi URL ký số, stderr thô hay secrets vào log/file.
- Không hỗ trợ proxy hệ thống/proxy công ty cho việc tải (egress proxy của HighVi là đường duy nhất).

## Report a problem

Không gửi URL có token, cookie, credential hoặc nội dung riêng tư trong báo cáo lỗi. Chỉ gửi error code và bước tái hiện đã làm sạch.
