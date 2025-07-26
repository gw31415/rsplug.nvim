use std::path::PathBuf;

/// グローバルな設定
#[derive(Clone)]
pub struct Config {
    /// キャッシュ先
    pub cachepath: PathBuf,
    /// Vim の 'packpath' に設定されるディレクトリ
    pub packpath: PathBuf,
    /// マージに関する設定
    pub install: InstallConfig,
}

impl Default for Config {
    fn default() -> Self {
        let homedir = std::env::home_dir().unwrap();
        let cachedir = homedir.join(".cache");
        let appdir = cachedir.join("rsplug");
        Config {
            cachepath: appdir.clone(),
            packpath: appdir,
            install: Default::default(),
        }
    }
}

/// インストールに関する設定
#[derive(Clone)]
pub struct InstallConfig {
    // インストールを無視するファイル名パターン (Regexパターン)
    pub ignore: Vec<String>,
}

impl Default for InstallConfig {
    fn default() -> Self {
        InstallConfig {
            ignore: vec![
                r"^README\.md$".to_string(),
                r"^LICENSE$".to_string(),
                r"^LICENSE\.txt$".to_string(),
                r"^LICENSE\.md$".to_string(),
                r"^COPYING$".to_string(),
                r"^COPYING\.txt$".to_string(),
                r"^\.gitignore$".to_string(),
                r"^\.tool-versions$".to_string(),
                r"^\.vscode$".to_string(),
                r"^deno\.json$".to_string(),
                r"^deno\.lock$".to_string(),
                r"^deno\.jsonc$".to_string(),
                r"^\.gitmessage$".to_string(),
                r"^\.gitattributes$".to_string(),
                r"^\.github$".to_string(),
            ],
        }
    }
}
