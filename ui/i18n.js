// UI text in English, Vietnamese and Japanese. Rust sends stable error codes;
// they are translated here, and unknown codes fall back to Rust's English text.
// Keep the wording simple enough for a child to follow.
const I18N = (() => {
  const messages = {
    en: {
      "lang.label": "Language",
      "hero.eyebrow": "Video downloader · Linux",
      "hero.subtitle": "Paste a link, and HighVi turns the video into one MP4 file.",
      notice:
        "Only download videos you own or are allowed to download. HighVi never breaks logins, DRM, or any other lock.",
      "mode.legend": "Where is the video?",
      "mode.page": "Video page link",
      "mode.direct": "Playlist link (.m3u8)",
      "page.label": "Video page link",
      "page.hint":
        "Paste the page that plays the video. HighVi asks yt-dlp first, then watches the page's own player in a private browser.",
      "playlist.label": "Playlist link (.m3u8)",
      "referer.label": "Page where you found it",
      "direct.hint":
        "HighVi only tells the site its front-door address (like https://example.com/). It never sends the page path, cookies, or logins.",
      "download.button": "Download MP4",
      "download.busy": "Working…",
      "download.hint":
        "Saved in Downloads/HighVi. If the name is taken, HighVi adds a number. It never writes over a file.",
      "network.hint":
        "Only public https:// links. Home and office network addresses are always blocked, even if the video points to them later.",
      "deps.title": "Missing tools",
      "deps.missing": "HighVi needs {tools} to download videos. Install it, then restart HighVi.",
      "deps.hint": "On Ubuntu: sudo apt install yt-dlp ffmpeg",
      "cancel.button": "Cancel",
      "cancel.busy": "Stopping…",
      "progress.aria": "Download progress",
      "progress.working": "Working…",
      "progress.discovering": "Looking for the video…",
      "progress.browsing": "Opening the page in a private browser to find the video…",
      "deps.browserMissing":
        "Optional: install Google Chrome or Chromium so HighVi can find videos on pages whose player is built with JavaScript.",
      "error.browser_unavailable": "This page needs Google Chrome or Chromium to find the video. Install one, then try again.",
      "error.browser_failed": "The private browser HighVi uses to find videos stopped unexpectedly. Try again.",
      "error.no_media_found":
        "HighVi opened the page in a private browser, but no video showed up. The video may need a sign-in, a special button, or use a format HighVi doesn't support.",
      "progress.downloadingVideo": "Downloading the picture…",
      "progress.downloadingAudio": "Downloading the sound…",
      "progress.merging": "Putting it together as an MP4…",
      "progress.saving": "Saving…",
      "progress.title": "Found: {title}",
      "progress.speed": "Speed: {value}",
      "progress.eta": "Time left: {value}",
      "cancelled.title": "Stopped. Nothing was saved.",
      "error.title": "Couldn't finish",
      "success.title": "All done!",
      "success.open": "Open folder",
      "error.unknown": "Something went wrong, and HighVi doesn't know why.",
      "error.invalid_page_url":
        "That doesn't look like a web page link. Use a full https:// link with no username or password in it.",
      "error.invalid_playlist_url":
        "That isn't a playlist link. It has to start with https:// and end with .m3u8.",
      "error.invalid_referer_url":
        "Type the web page where you found the playlist (an https:// link), not the playlist itself.",
      "error.unsafe_network_target":
        "This video tried to reach a computer on a home or office network. HighVi blocked it.",
      "error.insecure_media_url":
        "Part of this video uses plain http://, which isn't safe. HighVi only downloads over https://.",
      "error.network_lookup_failed":
        "HighVi couldn't find that website. Check the spelling and your internet connection.",
      "error.discovery_timeout": "Looking for the video took too long (over 60 seconds). Try again.",
      "error.extractor_output_too_large":
        "The page sent back too much information for HighVi to handle safely.",
      "error.invalid_extractor_output": "yt-dlp sent back something HighVi couldn't read.",
      "error.hls_not_found": "No video that HighVi can download was found on this page.",
      "error.live_not_supported": "This is a live stream. HighVi only downloads finished videos.",
      "error.drm_not_supported": "This video is locked with DRM. HighVi won't unlock it.",
      "error.authentication_required":
        "You have to sign in to watch this. HighVi doesn't use your logins.",
      "error.access_denied.page":
        "The website said no (HTTP 403). It may block download apps.",
      "error.access_denied.bare":
        "The website said no (HTTP 403). This playlist may only work from the page it came from. Switch to “Playlist link” and add that page.",
      "error.access_denied.direct":
        "The website said no (HTTP 403). Check that this is the page where you found the playlist, or copy a fresh playlist link.",
      "error.host_firewall_block.page":
        "The website's guard (Cloudflare) blocked HighVi (HTTP 403). HighVi doesn't use browser logins or sneak past bot protection.",
      "error.host_firewall_block.bare":
        "The website's guard (Cloudflare) blocked HighVi (HTTP 403). This playlist may only work from the page it came from. Switch to “Playlist link” and add that page.",
      "error.host_firewall_block.direct":
        "The website's guard (Cloudflare) blocked HighVi (HTTP 403). Check that this is the page where you found the playlist, or copy a fresh one. HighVi doesn't sneak past bot protection.",
      "error.media_not_found":
        "The video isn't there anymore (HTTP 404/410). The link may have expired, so copy a fresh one.",
      "error.unsupported_page":
        "yt-dlp doesn't know this website and found no video in it. If you have the playlist link, use “Playlist link”.",
      "error.unsupported_playlist":
        "yt-dlp couldn't read this as a playlist. Make sure it's the .m3u8 link itself and that it hasn't expired.",
      "error.no_formats": "There was no video that could be downloaded.",
      "error.network_timeout": "The website took too long to answer. Try again later.",
      "error.yt_dlp_failed":
        "yt-dlp couldn't handle this video. Check the link, and make sure yt-dlp is up to date.",
      "error.missing_dependency":
        "A tool HighVi needs (yt-dlp or FFmpeg) isn't installed, or HighVi can't find it.",
      "error.process_start_failed": "HighVi couldn't start one of its tools.",
      "error.process_output_failed": "HighVi lost track of the download's progress.",
      "error.proxy_failed": "HighVi couldn't start its network guard, so it didn't download anything.",
      "error.download_already_active":
        "Another download is still running. Please wait for it to finish.",
      "error.download_directory_unavailable": "HighVi can't use its folder in Downloads.",
      "error.output_missing": "The download finished, but the video file wasn't there.",
      "error.unsupported_container":
        "The video came in a format HighVi can't safely turn into an MP4.",
      "error.mux_failed":
        "HighVi couldn't put the picture and sound together into an MP4, so nothing was saved.",
      "error.output_publish_failed":
        "The video finished, but HighVi couldn't move it into its folder.",
      "error.open_folder_failed": "HighVi couldn't open the folder.",
      "error.playlist_access_denied":
        "The page's player found the video, but the video server said no (HTTP 403). HighVi doesn't use your logins or get around locks.",
      "error.media_access_denied":
        "The website showed HighVi the video, but then refused to send the video file (HTTP 403).",
      "error.rate_limited": "The website says HighVi asked too often (HTTP 429). Wait a while, then try again.",
      "error.page_unavailable": "The page didn't open in HighVi's private browser. Check the link.",
      "error.media_discovery_timeout":
        "The page took too long to open in HighVi's private browser, so no video was found. Try again later.",
      "hint.updateYtDlp.minimum":
        "Your yt-dlp is {installed}. This needs {minimum} or newer: websites change, and old versions stop working. Update yt-dlp, then try again.",
      "hint.updateYtDlp.stale":
        "Your yt-dlp ({installed}) is more than 90 days old. Websites change, and old versions stop working. Update yt-dlp, then try again.",
      "diagnostics.copy": "Copy diagnostics",
      "diagnostics.copied": "Copied.",
      "diagnostics.copyFailed": "Couldn't copy. Select the text below and copy it.",
      "diagnostics.show": "Show diagnostics (no links, cookies or passwords)",
      "deps.outdated":
        "Your yt-dlp ({installed}) is older than {recommended}, the version HighVi is tested with. Some sites, like YouTube, won't work.",
      "deps.updateHint": "To update on Ubuntu: sudo apt install pipx, then pipx install yt-dlp, then restart HighVi.",
    },

    vi: {
      "lang.label": "Ngôn ngữ",
      "hero.eyebrow": "Trình tải video · Linux",
      "hero.subtitle": "Dán link vào là HighVi gom video lại thành một file MP4.",
      notice:
        "Chỉ tải video của bạn hoặc video bạn được phép tải. HighVi không bao giờ phá đăng nhập, DRM hay bất kỳ ổ khóa nào.",
      "mode.legend": "Video nằm ở đâu?",
      "mode.page": "Link trang video",
      "mode.direct": "Link playlist (.m3u8)",
      "page.label": "Link trang video",
      "page.hint": "Dán trang đang phát video. HighVi hỏi yt-dlp trước, rồi xem trình phát của chính trang đó trong một trình duyệt riêng.",
      "playlist.label": "Link playlist (.m3u8)",
      "referer.label": "Trang bạn tìm thấy playlist",
      "direct.hint":
        "HighVi chỉ cho trang web biết địa chỉ cổng chính (như https://example.com/). Không gửi đường dẫn trang, cookie hay tài khoản.",
      "download.button": "Tải MP4",
      "download.busy": "Đang làm…",
      "download.hint":
        "File nằm trong Downloads/HighVi. Nếu trùng tên, HighVi thêm số vào sau. Không bao giờ ghi đè lên file đã có.",
      "network.hint":
        "Chỉ nhận link https:// công khai. Địa chỉ mạng trong nhà hay công ty luôn bị chặn, kể cả khi video trỏ tới đó về sau.",
      "deps.title": "Thiếu công cụ",
      "deps.missing": "HighVi cần {tools} để tải video. Hãy cài vào rồi mở lại HighVi.",
      "deps.hint": "Trên Ubuntu: sudo apt install yt-dlp ffmpeg",
      "cancel.button": "Hủy",
      "cancel.busy": "Đang dừng…",
      "progress.aria": "Tiến độ tải",
      "progress.working": "Đang làm…",
      "progress.discovering": "Đang tìm video…",
      "progress.browsing": "Đang mở trang trong một trình duyệt riêng để tìm video…",
      "deps.browserMissing":
        "Không bắt buộc: cài Google Chrome hoặc Chromium để HighVi tìm được video trên các trang có trình phát dựng bằng JavaScript.",
      "error.browser_unavailable": "Trang này cần Google Chrome hoặc Chromium để tìm video. Hãy cài một cái rồi thử lại.",
      "error.browser_failed": "Trình duyệt riêng mà HighVi dùng để tìm video bị dừng bất ngờ. Thử lại nhé.",
      "error.no_media_found":
        "HighVi đã mở trang trong trình duyệt riêng nhưng không thấy video nào. Có thể video cần đăng nhập, cần bấm một nút đặc biệt, hoặc dùng định dạng HighVi chưa hỗ trợ.",
      "progress.downloadingVideo": "Đang tải phần hình…",
      "progress.downloadingAudio": "Đang tải phần tiếng…",
      "progress.merging": "Đang ghép lại thành MP4…",
      "progress.saving": "Đang lưu…",
      "progress.title": "Đã tìm thấy: {title}",
      "progress.speed": "Tốc độ: {value}",
      "progress.eta": "Còn khoảng: {value}",
      "cancelled.title": "Đã dừng. Không có gì được lưu.",
      "error.title": "Chưa xong được",
      "success.title": "Tải xong rồi!",
      "success.open": "Mở thư mục",
      "error.unknown": "Có lỗi gì đó mà HighVi không rõ nguyên nhân.",
      "error.invalid_page_url":
        "Link này không giống link trang web. Hãy dùng link https:// đầy đủ, không chứa tên đăng nhập hay mật khẩu.",
      "error.invalid_playlist_url":
        "Đây không phải link playlist. Link phải bắt đầu bằng https:// và kết thúc bằng .m3u8.",
      "error.invalid_referer_url":
        "Hãy nhập trang web nơi bạn tìm thấy playlist (một link https://), chứ không phải chính playlist.",
      "error.unsafe_network_target":
        "Video này định kết nối tới một máy trong mạng nhà hay mạng công ty. HighVi đã chặn lại.",
      "error.insecure_media_url":
        "Một phần video dùng http:// không an toàn. HighVi chỉ tải qua https://.",
      "error.network_lookup_failed":
        "HighVi không tìm thấy trang web này. Kiểm tra lại chính tả và kết nối mạng nhé.",
      "error.discovery_timeout": "Tìm video lâu quá (hơn 60 giây). Thử lại nhé.",
      "error.extractor_output_too_large":
        "Trang web gửi về quá nhiều thông tin, HighVi không xử lý an toàn được.",
      "error.invalid_extractor_output": "yt-dlp gửi về thứ mà HighVi không đọc được.",
      "error.hls_not_found": "Không tìm thấy video nào HighVi tải được trên trang này.",
      "error.live_not_supported": "Đây là phát trực tiếp. HighVi chỉ tải video đã phát xong.",
      "error.drm_not_supported": "Video này bị khóa bằng DRM. HighVi sẽ không mở khóa.",
      "error.authentication_required":
        "Phải đăng nhập mới xem được video này. HighVi không dùng tài khoản của bạn.",
      "error.access_denied.page":
        "Trang web từ chối (HTTP 403). Có thể trang chặn các ứng dụng tải video.",
      "error.access_denied.bare":
        "Trang web từ chối (HTTP 403). Playlist này có thể chỉ chạy từ trang chứa nó. Hãy chuyển sang “Link playlist” và thêm trang đó vào.",
      "error.access_denied.direct":
        "Trang web từ chối (HTTP 403). Xem lại trang có đúng là nơi bạn tìm thấy playlist không, hoặc lấy link playlist mới.",
      "error.host_firewall_block.page":
        "Lớp bảo vệ của trang (Cloudflare) đã chặn HighVi (HTTP 403). HighVi không dùng đăng nhập trình duyệt và không lách qua chặn bot.",
      "error.host_firewall_block.bare":
        "Lớp bảo vệ của trang (Cloudflare) đã chặn HighVi (HTTP 403). Playlist này có thể chỉ chạy từ trang chứa nó. Hãy chuyển sang “Link playlist” và thêm trang đó vào.",
      "error.host_firewall_block.direct":
        "Lớp bảo vệ của trang (Cloudflare) đã chặn HighVi (HTTP 403). Xem lại trang có đúng là nơi bạn tìm thấy playlist không, hoặc lấy link mới. HighVi không lách qua chặn bot.",
      "error.media_not_found":
        "Video không còn ở đó nữa (HTTP 404/410). Có thể link đã hết hạn, hãy lấy link mới.",
      "error.unsupported_page":
        "yt-dlp không biết trang này và không tìm thấy video nào. Nếu có link playlist, hãy chọn “Link playlist”.",
      "error.unsupported_playlist":
        "yt-dlp không đọc được link này như một playlist. Kiểm tra đó đúng là link .m3u8 và chưa hết hạn.",
      "error.no_formats": "Không có video nào tải được.",
      "error.network_timeout": "Trang web trả lời lâu quá. Thử lại sau nhé.",
      "error.yt_dlp_failed":
        "yt-dlp không xử lý được video này. Kiểm tra lại link và cập nhật yt-dlp nhé.",
      "error.missing_dependency":
        "Thiếu công cụ HighVi cần (yt-dlp hoặc FFmpeg), hoặc HighVi không tìm thấy nó.",
      "error.process_start_failed": "HighVi không khởi động được một công cụ của nó.",
      "error.process_output_failed": "HighVi không theo dõi được tiến độ tải.",
      "error.proxy_failed": "HighVi không bật được lớp bảo vệ mạng nên chưa tải gì cả.",
      "error.download_already_active": "Đang có một video khác tải dở. Chờ nó xong đã nhé.",
      "error.download_directory_unavailable": "HighVi không dùng được thư mục của nó trong Downloads.",
      "error.output_missing": "Đã tải xong nhưng không thấy file video đâu.",
      "error.unsupported_container":
        "Video ở định dạng mà HighVi không chuyển sang MP4 một cách an toàn được.",
      "error.mux_failed": "HighVi không ghép được hình và tiếng thành MP4, nên chưa lưu gì cả.",
      "error.output_publish_failed":
        "Video đã tải xong nhưng HighVi không chuyển được vào thư mục.",
      "error.open_folder_failed": "HighVi không mở được thư mục.",
      "error.playlist_access_denied":
        "Trình phát của trang đã tìm thấy video, nhưng máy chủ video từ chối (HTTP 403). HighVi không dùng tài khoản của bạn và không phá khóa.",
      "error.media_access_denied":
        "Trang web cho HighVi xem thông tin video, nhưng lại không chịu gửi file video (HTTP 403).",
      "error.rate_limited": "Trang web bảo HighVi hỏi quá nhiều lần (HTTP 429). Đợi một lúc rồi thử lại.",
      "error.page_unavailable": "Trang không mở được trong trình duyệt riêng của HighVi. Hãy kiểm tra lại link.",
      "error.media_discovery_timeout":
        "Trang mở quá lâu trong trình duyệt riêng của HighVi nên chưa tìm được video. Thử lại sau nhé.",
      "hint.updateYtDlp.minimum":
        "yt-dlp của bạn là bản {installed}. Cần bản {minimum} hoặc mới hơn: trang web hay thay đổi, bản cũ sẽ ngừng chạy. Hãy cập nhật yt-dlp rồi thử lại.",
      "hint.updateYtDlp.stale":
        "yt-dlp của bạn ({installed}) đã cũ hơn 90 ngày. Trang web hay thay đổi, bản cũ sẽ ngừng chạy. Hãy cập nhật yt-dlp rồi thử lại.",
      "diagnostics.copy": "Sao chép thông tin chẩn đoán",
      "diagnostics.copied": "Đã sao chép.",
      "diagnostics.copyFailed": "Không sao chép được. Hãy bôi đen đoạn chữ bên dưới rồi sao chép.",
      "diagnostics.show": "Xem thông tin chẩn đoán (không có link, cookie hay mật khẩu)",
      "deps.outdated":
        "yt-dlp của bạn ({installed}) cũ hơn bản {recommended} mà HighVi đã thử. Một số trang, như YouTube, sẽ không tải được.",
      "deps.updateHint": "Cập nhật trên Ubuntu: sudo apt install pipx, rồi pipx install yt-dlp, rồi mở lại HighVi.",
    },

    ja: {
      "lang.label": "言語",
      "hero.eyebrow": "動画ダウンローダー · Linux",
      "hero.subtitle": "リンクを入れるだけで、HighVi が動画をひとつの MP4 にまとめます。",
      notice:
        "自分の動画か、ダウンロードしてよい動画だけにしてください。HighVi はログインや DRM などのカギを、ぜったいにこわしません。",
      "mode.legend": "動画はどこにある？",
      "mode.page": "動画ページのリンク",
      "mode.direct": "プレイリストのリンク（.m3u8）",
      "page.label": "動画ページのリンク",
      "page.hint": "動画が再生されるページを入れてください。HighVi はまず yt-dlp にきき、つぎに専用のブラウザーでページのプレーヤーを見ます。",
      "playlist.label": "プレイリストのリンク（.m3u8）",
      "referer.label": "プレイリストを見つけたページ",
      "direct.hint":
        "HighVi がサイトに伝えるのは「玄関の住所」（たとえば https://example.com/）だけです。ページのくわしい場所、クッキー、ログイン情報は送りません。",
      "download.button": "MP4 をダウンロード",
      "download.busy": "作業中…",
      "download.hint":
        "ファイルは Downloads/HighVi に入ります。同じ名前があるときは番号をつけます。上書きはしません。",
      "network.hint":
        "公開されている https:// のリンクだけ使えます。家や会社のネットワークの住所は、あとから動画がそこを指しても、いつもブロックします。",
      "deps.title": "道具が足りません",
      "deps.missing": "動画をダウンロードするには {tools} が必要です。入れてから、HighVi を開きなおしてください。",
      "deps.hint": "Ubuntu では：sudo apt install yt-dlp ffmpeg",
      "cancel.button": "キャンセル",
      "cancel.busy": "止めています…",
      "progress.aria": "ダウンロードの進みぐあい",
      "progress.working": "作業中…",
      "progress.discovering": "動画をさがしています…",
      "progress.browsing": "動画をさがすために、専用のブラウザーでページを開いています…",
      "deps.browserMissing":
        "なくても使えます：Google Chrome か Chromium を入れると、JavaScript で作られたプレーヤーのページでも動画をさがせます。",
      "error.browser_unavailable": "このページで動画をさがすには Google Chrome か Chromium が必要です。入れてから、もう一度ためしてください。",
      "error.browser_failed": "動画さがし用のブラウザーが、とちゅうで止まってしまいました。もう一度ためしてください。",
      "error.no_media_found":
        "専用のブラウザーでページを開きましたが、動画は出てきませんでした。ログインや特別なボタンが必要か、HighVi がまだあつかえない形式かもしれません。",
      "progress.downloadingVideo": "映像をダウンロードしています…",
      "progress.downloadingAudio": "音声をダウンロードしています…",
      "progress.merging": "MP4 にまとめています…",
      "progress.saving": "保存しています…",
      "progress.title": "見つかりました：{title}",
      "progress.speed": "速さ：{value}",
      "progress.eta": "あと：{value}",
      "cancelled.title": "止めました。何も保存していません。",
      "error.title": "うまくいきませんでした",
      "success.title": "ダウンロードできました！",
      "success.open": "フォルダーを開く",
      "error.unknown": "何かがうまくいきませんでしたが、原因がわかりません。",
      "error.invalid_page_url":
        "Web ページのリンクではないようです。ユーザー名やパスワードが入っていない、https:// から始まるリンクを使ってください。",
      "error.invalid_playlist_url":
        "プレイリストのリンクではありません。https:// で始まり、.m3u8 で終わるリンクにしてください。",
      "error.invalid_referer_url":
        "プレイリストそのものではなく、それを見つけた Web ページ（https:// のリンク）を入れてください。",
      "error.unsafe_network_target":
        "この動画は、家や会社のネットワークの中のコンピューターにつなごうとしました。HighVi がブロックしました。",
      "error.insecure_media_url":
        "動画の一部が安全でない http:// を使っています。HighVi は https:// でしかダウンロードしません。",
      "error.network_lookup_failed":
        "そのサイトが見つかりませんでした。つづりとインターネットの接続をたしかめてください。",
      "error.discovery_timeout":
        "動画さがしに時間がかかりすぎました（60秒以上）。もう一度ためしてください。",
      "error.extractor_output_too_large":
        "ページから届いた情報が多すぎて、安全にあつかえません。",
      "error.invalid_extractor_output": "yt-dlp から、HighVi が読めない返事が届きました。",
      "error.hls_not_found": "このページには、HighVi がダウンロードできる動画が見つかりませんでした。",
      "error.live_not_supported": "これはライブ配信です。HighVi は配信が終わった動画だけダウンロードします。",
      "error.drm_not_supported":
        "この動画には DRM のカギがかかっています。HighVi はカギを開けません。",
      "error.authentication_required":
        "この動画はログインしないと見られません。HighVi はあなたのログインを使いません。",
      "error.access_denied.page":
        "サイトにことわられました（HTTP 403）。ダウンロード用のアプリをブロックしているのかもしれません。",
      "error.access_denied.bare":
        "サイトにことわられました（HTTP 403）。このプレイリストは、のせているページからしか使えないのかもしれません。「プレイリストのリンク」に切りかえて、そのページも入れてください。",
      "error.access_denied.direct":
        "サイトにことわられました（HTTP 403）。プレイリストを見つけたページが合っているか見なおすか、新しいプレイリストのリンクをコピーしてください。",
      "error.host_firewall_block.page":
        "サイトの見はり番（Cloudflare）にブロックされました（HTTP 403）。HighVi はブラウザのログインを使わず、ボット対策もすりぬけません。",
      "error.host_firewall_block.bare":
        "サイトの見はり番（Cloudflare）にブロックされました（HTTP 403）。このプレイリストは、のせているページからしか使えないのかもしれません。「プレイリストのリンク」に切りかえて、そのページも入れてください。",
      "error.host_firewall_block.direct":
        "サイトの見はり番（Cloudflare）にブロックされました（HTTP 403）。見つけたページが合っているか見なおすか、新しいプレイリストをコピーしてください。HighVi はボット対策をすりぬけません。",
      "error.media_not_found":
        "動画がもうありません（HTTP 404/410）。リンクの期限が切れたのかもしれません。新しいリンクをコピーしてください。",
      "error.unsupported_page":
        "yt-dlp がこのサイトを知らず、動画も見つかりませんでした。プレイリストのリンクがあれば「プレイリストのリンク」を使ってください。",
      "error.unsupported_playlist":
        "yt-dlp がプレイリストとして読めませんでした。.m3u8 のリンクそのものか、期限が切れていないかたしかめてください。",
      "error.no_formats": "ダウンロードできる動画がありませんでした。",
      "error.network_timeout":
        "サイトの返事がおそすぎました。しばらくしてから、もう一度ためしてください。",
      "error.yt_dlp_failed":
        "yt-dlp がこの動画をあつかえませんでした。リンクをたしかめて、yt-dlp を新しくしてください。",
      "error.missing_dependency":
        "HighVi に必要な道具（yt-dlp か FFmpeg）が入っていないか、見つけられません。",
      "error.process_start_failed": "HighVi が道具のひとつを動かせませんでした。",
      "error.process_output_failed": "HighVi がダウンロードの進みぐあいを読めなくなりました。",
      "error.proxy_failed": "HighVi がネットワークの見はりを始められなかったので、何もダウンロードしていません。",
      "error.download_already_active":
        "ほかのダウンロードがまだ動いています。終わるまで待ってください。",
      "error.download_directory_unavailable": "HighVi が Downloads の中のフォルダーを使えません。",
      "error.output_missing": "ダウンロードは終わりましたが、動画ファイルが見つかりませんでした。",
      "error.unsupported_container":
        "この動画は、HighVi が安全に MP4 にできない形式でした。",
      "error.mux_failed": "映像と音声を MP4 にまとめられなかったので、何も保存していません。",
      "error.output_publish_failed": "動画はできましたが、HighVi がフォルダーに移せませんでした。",
      "error.open_folder_failed": "フォルダーを開けませんでした。",
      "error.playlist_access_denied":
        "ページのプレーヤーは動画を見つけましたが、動画のサーバーにことわられました（HTTP 403）。HighVi はログイン情報を使わず、ロックもやぶりません。",
      "error.media_access_denied":
        "サイトは動画の情報を見せてくれましたが、動画ファイルを送ることはことわられました（HTTP 403）。",
      "error.rate_limited": "アクセスが多すぎるとサイトに言われました（HTTP 429）。しばらく待ってから、もう一度ためしてください。",
      "error.page_unavailable": "HighVi の専用ブラウザーでページを開けませんでした。リンクを確かめてください。",
      "error.media_discovery_timeout":
        "HighVi の専用ブラウザーでページを開くのに時間がかかりすぎて、動画を見つけられませんでした。あとでもう一度ためしてください。",
      "hint.updateYtDlp.minimum":
        "お使いの yt-dlp は {installed} です。{minimum} 以降が必要です。サイトは変わるので、古いバージョンは動かなくなります。yt-dlp を更新してから、もう一度ためしてください。",
      "hint.updateYtDlp.stale":
        "お使いの yt-dlp（{installed}）は 90 日以上前のものです。サイトは変わるので、古いバージョンは動かなくなります。yt-dlp を更新してから、もう一度ためしてください。",
      "diagnostics.copy": "診断情報をコピー",
      "diagnostics.copied": "コピーしました。",
      "diagnostics.copyFailed": "コピーできませんでした。下の文字を選んでコピーしてください。",
      "diagnostics.show": "診断情報を見る（リンク・クッキー・パスワードはふくみません）",
      "deps.outdated":
        "お使いの yt-dlp（{installed}）は、HighVi がテストした {recommended} より古いです。YouTube など、いくつかのサイトは使えません。",
      "deps.updateHint": "Ubuntu での更新：sudo apt install pipx のあと pipx install yt-dlp を実行し、HighVi を開きなおしてください。",
    },
  };

  const supported = Object.keys(messages);
  const storageKey = "highvi.language";

  function detect() {
    try {
      const saved = window.localStorage.getItem(storageKey);
      if (supported.includes(saved)) {
        return saved;
      }
    } catch {
      // Storage can be unavailable; fall back to the system language.
    }
    for (const tag of navigator.languages || [navigator.language || ""]) {
      const base = String(tag).slice(0, 2).toLowerCase();
      if (supported.includes(base)) {
        return base;
      }
    }
    return "en";
  }

  let language = detect();

  function has(key) {
    return key in messages[language] || key in messages.en;
  }

  function t(key, params = {}) {
    const text = messages[language][key] ?? messages.en[key] ?? key;
    return text.replace(/\{(\w+)\}/g, (_, name) => String(params[name] ?? ""));
  }

  function apply() {
    document.documentElement.lang = language;
    for (const element of document.querySelectorAll("[data-i18n]")) {
      element.textContent = t(element.dataset.i18n);
    }
    for (const element of document.querySelectorAll("[data-i18n-aria-label]")) {
      element.setAttribute("aria-label", t(element.dataset.i18nAriaLabel));
    }
    for (const button of document.querySelectorAll("[data-lang]")) {
      button.setAttribute("aria-pressed", String(button.dataset.lang === language));
    }
  }

  function set(next) {
    if (!supported.includes(next) || next === language) {
      return;
    }
    language = next;
    try {
      window.localStorage.setItem(storageKey, next);
    } catch {
      // The choice still applies for this session.
    }
    apply();
    document.dispatchEvent(new CustomEvent("highvi:language"));
  }

  return {
    t,
    has,
    apply,
    set,
    // For the catalogue parity check (scripts/check-ui.mjs).
    catalog: messages,
    get language() {
      return language;
    },
  };
})();
