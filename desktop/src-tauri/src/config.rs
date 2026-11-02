pub const LOG_FILENAME_PREFIX: &str = "log";
pub const LOG_FILENAME_SUFFIX: &str = ".txt";
/// `meeting_detect` is its own crate, so `vibe=` alone kept its every line, warnings included,
/// out of the log: a detector that saw nothing and one that could not look were indistinguishable.
pub const DEFAULT_LOG_DIRECTIVE: &str = "vibe=DEBUG,meeting_detect=DEBUG";
pub const STORE_FILENAME: &str = "app_config.json";
pub const DOCUMENTS_SUBFOLDER: &str = "Vibe";
