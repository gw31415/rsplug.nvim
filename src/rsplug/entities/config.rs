use std::{iter::Sum, ops::AddAssign, path::Path, str::FromStr, sync::Arc};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::Deserialize;
use serde_with::{DeserializeFromStr, OneOrMany, serde_as};

use super::*;

impl<T: IntoIterator<Item = Config>> From<T> for Config {
    fn from(value: T) -> Self {
        value.into_iter().sum()
    }
}

/// 設定ファイルの構造体
#[serde_as]
#[derive(Deserialize)]
pub struct Config {
    #[serde(default)]
    pub(super) plugins: Vec<Plugin>,
}

impl AddAssign for Config {
    fn add_assign(&mut self, rhs: Self) {
        self.plugins.extend(rhs.plugins);
    }
}

impl Sum for Config {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        let mut res = Config {
            plugins: Default::default(),
        };
        for plugin in iter {
            res += plugin;
        }
        res
    }
}

#[serde_as]
#[derive(Deserialize)]
pub(super) struct Plugin {
    pub repo: UnitSource,
    #[serde(default)]
    pub start: bool,
    #[serde_as(as = "OneOrMany<_>")]
    #[serde(default)]
    pub on_event: Vec<Autocmd>,
    #[serde_as(as = "OneOrMany<_>")]
    #[serde(default)]
    pub on_cmd: Vec<UserCmd>,
    #[serde(flatten)]
    pub script: SetupScript,
    #[serde(flatten)]
    pub merge: MergeConfig,
}

/// プラグインのセットアップに用いるスクリプト群
#[derive(Deserialize, Clone, Default)]
pub struct SetupScript {
    /// プラグイン読み込み直後に実行される Lua スクリプト
    pub lua_source: Option<String>,
}

/// プラグインのセットアップに用いるスクリプト群
#[derive(Deserialize)]
pub struct MergeConfig {
    #[serde(default = "default_ignore")]
    pub ignore: FileSpecifier,
}

fn default_ignore() -> FileSpecifier {
    FileSpecifier::from_str(include_str!("../../../templates/ignore.gitignore")).unwrap()
}

/// Gitignore形式のファイル指定子
#[derive(DeserializeFromStr)]
pub struct FileSpecifier(Arc<Gitignore>);

impl FileSpecifier {
    pub fn matched(&self, filepath: impl AsRef<Path>) -> bool {
        self.0.matched(filepath.as_ref(), false).is_ignore()
    }
}

impl FromStr for FileSpecifier {
    type Err = ignore::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut builder = GitignoreBuilder::new("");
        for line in s.lines() {
            builder.add_line(None, line)?;
        }
        Ok(FileSpecifier(builder.build()?.into()))
    }
}
