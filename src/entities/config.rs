use std::{iter::Sum, ops::AddAssign};

use serde::Deserialize;
use serde_with::{OneOrMany, serde_as};

use crate::UnitSource;

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
    pub on_event: Vec<String>,
    #[serde(flatten)]
    pub script: SetupScript,
}

/// プラグインのセットアップに用いるスクリプト群
#[derive(Deserialize, Clone, Default)]
pub struct SetupScript {
    /// プラグイン読み込み直後に実行される Lua スクリプト
    pub lua_source: Option<String>,
}
