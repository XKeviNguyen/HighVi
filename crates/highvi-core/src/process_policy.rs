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
}
