
use std::{iter::Sum, ops::AddAssign};

use serde::Deserialize;
use serde_with::{OneOrMany, serde_as};

impl<T: IntoIterator<Item = Config>> From<T> for Config {
    fn from(value: T) -> Self {
        value.into_iter().sum()
    }
}

/// 設定ファイルの構造体
#[serde_as]
#[derive(Deserialize)]
pub struct Config {
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
    pub repo: String,
    #[serde(default)]
    pub start: bool,
    #[serde_as(as = "OneOrMany<_>")]
    #[serde(default)]
    pub on_event: Vec<String>,
    pub rev: Option<String>,
}
