# Security model

## Trust boundaries

- **Webview → Rust:** URL và filename luôn được kiểm tra lại trong command Rust; UI không phải security boundary.
- **Network → Rust:** JSON và progress của `yt-dlp` là dữ liệu không tin cậy. Metadata có giới hạn kích thước; progress chỉ nhận prefix/field cố định; stderr thô không được trả về UI.
- **Rust → process:** `tokio::process::Command` truyền từng argument riêng, có `--` trước URL và không gọi shell. Mỗi lượt chạy dùng `--ignore-config`, tắt plugin, cookie và `--exec` để không kế thừa hành vi từ môi trường người dùng.
- **Rust → filesystem:** người dùng chỉ chọn basename; ký tự template `%`/`$` bị từ chối; thư mục đích cố định là `Downloads/HighVi`. Mỗi job dùng thư mục riêng có marker, tắt overwrite/post-overwrite rồi publish trên Linux bằng `RENAME_NOREPLACE`. Chỉ thư mục job có prefix + marker hợp lệ và cũ hơn 7 ngày mới được tự dọn; symlink không được theo.

## Explicit non-goals

- Không giải mã hoặc vượt DRM.
- Không lấy cookie/token từ trình duyệt và không vượt đăng nhập, paywall hoặc quyền truy cập.
- Không bảo đảm chặn SSRF qua mọi redirect/segment do `yt-dlp` tự tải; xem giới hạn trong README.
- Không ghi URL ký số, stderr thô hay secrets vào log/file.

## Report a problem

Không gửi URL có token, cookie, credential hoặc nội dung riêng tư trong báo cáo lỗi. Chỉ gửi error code và bước tái hiện đã làm sạch.
