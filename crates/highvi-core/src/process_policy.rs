/// Arguments that make each yt-dlp invocation independent from ambient user
/// configuration. Keep these first in the command line.
pub const YT_DLP_HERMETIC_ARGS: &[&str] = &[
    "--ignore-config",
    "--no-plugin-dirs",
    "--no-exec",
    "--no-cookies",
    "--no-cookies-from-browser",
    "--color",
    "never",
];

/// Environment variables passed through to yt-dlp; everything else is cleared
/// so ambient Python or yt-dlp settings cannot change its behaviour. Proxy
/// variables are kept so HighVi works behind a system proxy.
pub const YT_DLP_ENV_PASSTHROUGH: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "XDG_CACHE_HOME",
    // Custom CA bundles, e.g. behind a TLS-inspecting corporate proxy.
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "REQUESTS_CA_BUNDLE",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "all_proxy",
    "ALL_PROXY",
];

/// Set on every yt-dlp process. `YTDLP_NO_PLUGINS` repeats `--no-plugin-dirs`
/// as defence in depth.
pub const YT_DLP_FIXED_ENV: &[(&str, &str)] = &[("YTDLP_NO_PLUGINS", "1")];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_policy_disables_ambient_code_and_session_inputs() {
        for required in [
            "--ignore-config",
            "--no-plugin-dirs",
            "--no-exec",
            "--no-cookies",
            "--no-cookies-from-browser",
        ] {
            assert!(YT_DLP_HERMETIC_ARGS.contains(&required));
        }
    }

    #[test]
    fn environment_policy_drops_python_and_config_overrides() {
        for dropped in [
            "PYTHONPATH",
            "PYTHONSTARTUP",
            "PYTHONHOME",
            "XDG_CONFIG_HOME",
            "YTDLP_PLUGINS",
        ] {
            assert!(!YT_DLP_ENV_PASSTHROUGH.contains(&dropped), "{dropped}");
        }
        assert!(YT_DLP_FIXED_ENV.contains(&("YTDLP_NO_PLUGINS", "1")));
    }
}
