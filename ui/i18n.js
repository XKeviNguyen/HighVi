// UI text in English, Vietnamese and Japanese. Rust sends stable error codes;
// they are translated here, and unknown codes fall back to Rust's English text.
// Keep the wording simple enough for a child to follow.
const I18N = (() => {
  const messages = {
    en: {
      "lang.label": "Language",
      "hero.eyebrow": "HLS downloader · Ubuntu",
      "hero.subtitle": "Paste a link, and HighVi turns the video into one MP4 file.",
      notice:
        "Only download videos you own or are allowed to download. HighVi never breaks logins, DRM, or any other lock.",
      "mode.legend": "Where is the video?",
      "mode.page": "Video page link",
      "mode.direct": "Playlist link (.m3u8)",
      "page.label": "Video page link",
      "page.hint":
        "Works on sites yt-dlp knows. Sites that build their video player with JavaScript may not work.",
      "playlist.label": "Playlist link (.m3u8)",
      "referer.label": "Page where you found it",
      "direct.hint":
        "HighVi only tells the site its front-door address (like https://example.com/). It never sends the page path, cookies, or logins.",
      "discover.button": "Find video",
      "discover.busy": "Searching…",
      "discover.hint": "Only public https:// links. Home and office network addresses are blocked.",
      "result.eyebrow": "Found it",
      "result.playlist": "Playlist",
      "result.referer": "Referer",
      "result.refererNone": "Not sent",
      "filename.label": "Save as",
      "download.button": "Download MP4",
      "download.busy": "Downloading…",
      "download.hint":
        "Saved in Downloads/HighVi. HighVi never writes over a file that's already there.",
      "progress.aria": "Download progress",
      "progress.preparing": "Getting ready…",
      "progress.working": "Working…",
      "progress.recheck": "Checking the playlist again…",
      "progress.checking": "Checking the page and getting a fresh playlist…",
      "progress.downloading": "Downloading the pieces…",
      "progress.processing": "Gluing the pieces into an MP4…",
      "progress.finished": "Done!",
      "progress.speed": "Speed: {value}",
      "progress.eta": "Time left: {value}",
      "error.title": "Couldn't finish",
      "success.title": "All done!",
      "error.unknown": "Something went wrong, and HighVi doesn't know why.",
      "error.invalid_page_url":
        "That doesn't look like a web page link. Use a full https:// link with no username or password in it.",
      "error.invalid_playlist_url":
        "That isn't a playlist link. It has to start with https:// and end with .m3u8.",
      "error.invalid_referer_url":
        "Type the web page where you found the playlist (an https:// link), not the playlist itself.",
      "error.unsafe_network_target":
        "HighVi won't connect to computers on your home or office network.",
      "error.network_lookup_failed":
        "HighVi couldn't find that website. Check the spelling and your internet connection.",
      "error.discovery_timeout": "Looking for the video took too long (over 60 seconds). Try again.",
      "error.extractor_output_too_large":
        "The page sent back too much information for HighVi to handle safely.",
      "error.invalid_extractor_output": "yt-dlp sent back something HighVi couldn't read.",
      "error.hls_not_found": "No playlist was found on this page.",
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
        "yt-dlp doesn't know this website, or its player is built with JavaScript. If you have the playlist link, use “Playlist link”.",
      "error.unsupported_playlist":
        "yt-dlp couldn't read this as a playlist. Make sure it's the .m3u8 link itself and that it hasn't expired.",
      "error.no_formats": "There was no video that could be downloaded.",
      "error.network_timeout": "The website took too long to answer. Try again later.",
      "error.yt_dlp_failed":
        "yt-dlp couldn't handle this video. Check the link, and make sure yt-dlp is up to date.",
      "error.missing_dependency": "yt-dlp isn't installed, or HighVi can't find it.",
      "error.process_start_failed": "HighVi couldn't start the downloader.",
      "error.process_output_failed": "HighVi lost track of the download's progress.",
      "error.download_already_active":
        "Another download is still running. Please wait for it to finish.",
      "error.download_directory_unavailable": "HighVi can't use its folder in Downloads.",
      "error.output_exists":
        "There's already a file with that name in Downloads/HighVi. Pick another name.",
      "error.output_missing": "yt-dlp finished, but the MP4 file wasn't made.",
      "error.output_publish_failed":
        "The video finished, but HighVi couldn't move it into its folder.",
      "error.invalid_filename":
        "Use a name with 1–120 characters and none of these: / \\ : * ? \" < > | % $",
    },

    vi: {
      "lang.label": "Ngôn ngữ",
      "hero.eyebrow": "Trình tải HLS · Ubuntu",
      "hero.subtitle": "Dán link vào là HighVi gom video lại thành một file MP4.",
      notice:
        "Chỉ tải video của bạn hoặc video bạn được phép tải. HighVi không bao giờ phá đăng nhập, DRM hay bất kỳ ổ khóa nào.",
      "mode.legend": "Video nằm ở đâu?",
      "mode.page": "Link trang video",
      "mode.direct": "Link playlist (.m3u8)",
      "page.label": "Link trang video",
      "page.hint":
        "Dùng được với các trang mà yt-dlp biết. Trang dựng trình phát bằng JavaScript thì có thể không được.",
      "playlist.label": "Link playlist (.m3u8)",
      "referer.label": "Trang bạn tìm thấy playlist",
      "direct.hint":
        "HighVi chỉ cho trang web biết địa chỉ cổng chính (như https://example.com/). Không gửi đường dẫn trang, cookie hay tài khoản.",
      "discover.button": "Tìm video",
      "discover.busy": "Đang tìm…",
      "discover.hint": "Chỉ nhận link https:// công khai. Địa chỉ mạng trong nhà hay công ty sẽ bị chặn.",
      "result.eyebrow": "Tìm thấy rồi",
      "result.playlist": "Playlist",
      "result.referer": "Referer",
      "result.refererNone": "Không gửi",
      "filename.label": "Lưu với tên",
      "download.button": "Tải MP4",
      "download.busy": "Đang tải…",
      "download.hint":
        "File sẽ nằm trong Downloads/HighVi. HighVi không bao giờ ghi đè lên file đã có.",
      "progress.aria": "Tiến độ tải",
      "progress.preparing": "Đang chuẩn bị…",
      "progress.working": "Đang làm…",
      "progress.recheck": "Đang kiểm tra lại playlist…",
      "progress.checking": "Đang xem lại trang và lấy playlist mới…",
      "progress.downloading": "Đang tải từng mảnh…",
      "progress.processing": "Đang ghép các mảnh thành MP4…",
      "progress.finished": "Xong rồi!",
      "progress.speed": "Tốc độ: {value}",
      "progress.eta": "Còn khoảng: {value}",
      "error.title": "Chưa xong được",
      "success.title": "Tải xong rồi!",
      "error.unknown": "Có lỗi gì đó mà HighVi không rõ nguyên nhân.",
      "error.invalid_page_url":
        "Link này không giống link trang web. Hãy dùng link https:// đầy đủ, không chứa tên đăng nhập hay mật khẩu.",
      "error.invalid_playlist_url":
        "Đây không phải link playlist. Link phải bắt đầu bằng https:// và kết thúc bằng .m3u8.",
      "error.invalid_referer_url":
        "Hãy nhập trang web nơi bạn tìm thấy playlist (một link https://), chứ không phải chính playlist.",
      "error.unsafe_network_target": "HighVi không kết nối tới máy trong mạng nhà hay mạng công ty.",
      "error.network_lookup_failed":
        "HighVi không tìm thấy trang web này. Kiểm tra lại chính tả và kết nối mạng nhé.",
      "error.discovery_timeout": "Tìm video lâu quá (hơn 60 giây). Thử lại nhé.",
      "error.extractor_output_too_large":
        "Trang web gửi về quá nhiều thông tin, HighVi không xử lý an toàn được.",
      "error.invalid_extractor_output": "yt-dlp gửi về thứ mà HighVi không đọc được.",
      "error.hls_not_found": "Không tìm thấy playlist nào trên trang này.",
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
        "yt-dlp không biết trang này, hoặc trình phát được dựng bằng JavaScript. Nếu có link playlist, hãy chọn “Link playlist”.",
      "error.unsupported_playlist":
        "yt-dlp không đọc được link này như một playlist. Kiểm tra đó đúng là link .m3u8 và chưa hết hạn.",
      "error.no_formats": "Không có video nào tải được.",
      "error.network_timeout": "Trang web trả lời lâu quá. Thử lại sau nhé.",
      "error.yt_dlp_failed":
        "yt-dlp không xử lý được video này. Kiểm tra lại link và cập nhật yt-dlp nhé.",
      "error.missing_dependency": "Chưa cài yt-dlp, hoặc HighVi không tìm thấy nó.",
      "error.process_start_failed": "HighVi không khởi động được trình tải.",
      "error.process_output_failed": "HighVi không theo dõi được tiến độ tải.",
      "error.download_already_active": "Đang có một video khác tải dở. Chờ nó xong đã nhé.",
      "error.download_directory_unavailable": "HighVi không dùng được thư mục của nó trong Downloads.",
      "error.output_exists": "Trong Downloads/HighVi đã có file trùng tên. Hãy đặt tên khác.",
      "error.output_missing": "yt-dlp đã chạy xong nhưng không tạo ra file MP4.",
      "error.output_publish_failed":
        "Video đã tải xong nhưng HighVi không chuyển được vào thư mục.",
      "error.invalid_filename":
        "Tên file dài 1–120 ký tự và không có các ký tự: / \\ : * ? \" < > | % $",
    },

    ja: {
      "lang.label": "言語",
      "hero.eyebrow": "HLS ダウンローダー · Ubuntu",
      "hero.subtitle": "リンクを入れるだけで、HighVi が動画をひとつの MP4 にまとめます。",
      notice:
        "自分の動画か、ダウンロードしてよい動画だけにしてください。HighVi はログインや DRM などのカギを、ぜったいにこわしません。",
      "mode.legend": "動画はどこにある？",
      "mode.page": "動画ページのリンク",
      "mode.direct": "プレイリストのリンク（.m3u8）",
      "page.label": "動画ページのリンク",
      "page.hint":
        "yt-dlp が知っているサイトで使えます。JavaScript でプレーヤーを作っているサイトでは、うまくいかないことがあります。",
      "playlist.label": "プレイリストのリンク（.m3u8）",
      "referer.label": "プレイリストを見つけたページ",
      "direct.hint":
        "HighVi がサイトに伝えるのは「玄関の住所」（たとえば https://example.com/）だけです。ページのくわしい場所、クッキー、ログイン情報は送りません。",
      "discover.button": "動画をさがす",
      "discover.busy": "さがしています…",
      "discover.hint":
        "公開されている https:// のリンクだけ使えます。家や会社のネットワークの住所はブロックします。",
      "result.eyebrow": "見つかりました",
      "result.playlist": "プレイリスト",
      "result.referer": "Referer（リファラー）",
      "result.refererNone": "送りません",
      "filename.label": "保存する名前",
      "download.button": "MP4 をダウンロード",
      "download.busy": "ダウンロード中…",
      "download.hint":
        "ファイルは Downloads/HighVi に入ります。同じ名前のファイルがあっても上書きしません。",
      "progress.aria": "ダウンロードの進みぐあい",
      "progress.preparing": "準備しています…",
      "progress.working": "作業中…",
      "progress.recheck": "プレイリストをもう一度たしかめています…",
      "progress.checking": "ページを見なおして、新しいプレイリストをもらっています…",
      "progress.downloading": "かけらをダウンロードしています…",
      "progress.processing": "かけらをつなげて MP4 にしています…",
      "progress.finished": "できました！",
      "progress.speed": "速さ：{value}",
      "progress.eta": "あと：{value}",
      "error.title": "うまくいきませんでした",
      "success.title": "ダウンロードできました！",
      "error.unknown": "何かがうまくいきませんでしたが、原因がわかりません。",
      "error.invalid_page_url":
        "Web ページのリンクではないようです。ユーザー名やパスワードが入っていない、https:// から始まるリンクを使ってください。",
      "error.invalid_playlist_url":
        "プレイリストのリンクではありません。https:// で始まり、.m3u8 で終わるリンクにしてください。",
      "error.invalid_referer_url":
        "プレイリストそのものではなく、それを見つけた Web ページ（https:// のリンク）を入れてください。",
      "error.unsafe_network_target":
        "HighVi は、家や会社のネットワークの中のコンピューターにはつなぎません。",
      "error.network_lookup_failed":
        "そのサイトが見つかりませんでした。つづりとインターネットの接続をたしかめてください。",
      "error.discovery_timeout":
        "動画さがしに時間がかかりすぎました（60秒以上）。もう一度ためしてください。",
      "error.extractor_output_too_large":
        "ページから届いた情報が多すぎて、安全にあつかえません。",
      "error.invalid_extractor_output": "yt-dlp から、HighVi が読めない返事が届きました。",
      "error.hls_not_found": "このページにはプレイリストが見つかりませんでした。",
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
        "yt-dlp がこのサイトを知らないか、プレーヤーが JavaScript で作られています。プレイリストのリンクがあれば「プレイリストのリンク」を使ってください。",
      "error.unsupported_playlist":
        "yt-dlp がプレイリストとして読めませんでした。.m3u8 のリンクそのものか、期限が切れていないかたしかめてください。",
      "error.no_formats": "ダウンロードできる動画がありませんでした。",
      "error.network_timeout":
        "サイトの返事がおそすぎました。しばらくしてから、もう一度ためしてください。",
      "error.yt_dlp_failed":
        "yt-dlp がこの動画をあつかえませんでした。リンクをたしかめて、yt-dlp を新しくしてください。",
      "error.missing_dependency": "yt-dlp が入っていないか、HighVi が見つけられません。",
      "error.process_start_failed": "HighVi がダウンローダーを動かせませんでした。",
      "error.process_output_failed": "HighVi がダウンロードの進みぐあいを読めなくなりました。",
      "error.download_already_active":
        "ほかのダウンロードがまだ動いています。終わるまで待ってください。",
      "error.download_directory_unavailable": "HighVi が Downloads の中のフォルダーを使えません。",
      "error.output_exists":
        "Downloads/HighVi に同じ名前のファイルがもうあります。べつの名前にしてください。",
      "error.output_missing": "yt-dlp は終わりましたが、MP4 ファイルができませんでした。",
      "error.output_publish_failed": "動画はできましたが、HighVi がフォルダーに移せませんでした。",
      "error.invalid_filename":
        "名前は 1〜120 文字にして、次の文字は使わないでください：/ \\ : * ? \" < > | % $",
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
    get language() {
      return language;
    },
  };
})();
