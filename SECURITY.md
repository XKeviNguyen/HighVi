# Security model

## Trust boundaries

- **Webview → Rust:** nguồn (`page` hoặc `directHls`), URL và filename luôn được kiểm tra lại trong command Rust; UI không phải security boundary. UI chỉ nhận URL đã rút gọn (`https://host/…/file?…`), không nhận URL playlist đầy đủ có thể chứa token.
- **Network → Rust:** JSON và progress của `yt-dlp` là dữ liệu không tin cậy. Metadata có giới hạn kích thước; progress chỉ nhận prefix/field cố định; stderr thô không được trả về UI; lỗi chỉ trả về error code và thông điệp cố định.
- **Rust → process:** `tokio::process::Command` truyền từng argument riêng, có `--` trước URL và không gọi shell. Mỗi lượt chạy dùng `--ignore-config`, tắt plugin, cookie và `--exec` để không kế thừa hành vi từ môi trường người dùng. Biến môi trường bị xoá trừ `PATH`, `HOME`, locale, `TMPDIR` và proxy; luôn đặt `YTDLP_NO_PLUGINS=1`. `Referer` được truyền bằng `--add-headers Referer:<url>` như một argument riêng, lấy từ `Url` đã parse nên không chứa CR/LF.
- **Referer:** chỉ là metadata về trang nguồn, không phải credential. Theo mặc định trình duyệt (`strict-origin-when-cross-origin`), playlist khác origin chỉ nhận origin của trang. Chế độ trang không gửi Referer khi tải trang, và không bao giờ dùng chính playlist làm Referer.
- **Rust → filesystem:** người dùng chỉ chọn basename; ký tự template `%`/`$` bị từ chối; thư mục đích cố định là `Downloads/HighVi`. Mỗi job dùng thư mục riêng có marker, tắt overwrite/post-overwrite rồi publish trên Linux bằng `RENAME_NOREPLACE`. Chỉ thư mục job có prefix + marker hợp lệ và cũ hơn 7 ngày mới được tự dọn; symlink không được theo.

## Explicit non-goals

- Không giải mã hoặc vượt DRM.
- Không lấy cookie/token từ trình duyệt và không vượt đăng nhập, paywall hoặc quyền truy cập.
- Không bảo đảm chặn SSRF qua mọi redirect/segment/`EXT-X-KEY` do `yt-dlp` tự tải (hoặc FFmpeg khi `yt-dlp` fallback); xem giới hạn trong README.
- Không vượt Cloudflare/anti-bot, không giả lập trình duyệt (`--impersonate`) và không chạy plugin bên thứ ba.
- Không ghi URL ký số, stderr thô hay secrets vào log/file.

## Report a problem

Không gửi URL có token, cookie, credential hoặc nội dung riêng tư trong báo cáo lỗi. Chỉ gửi error code và bước tái hiện đã làm sạch.
