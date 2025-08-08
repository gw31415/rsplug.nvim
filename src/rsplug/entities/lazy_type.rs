use core::fmt;
use std::{
    borrow::{Borrow, Cow},
    cmp::Ordering,
    collections::BTreeSet,
    ops::BitAndAssign,
    str::FromStr,
    sync::Arc,
};

use once_cell::sync::Lazy;
use regex::Regex;
use sailfish::runtime::Render;
use serde_with::DeserializeFromStr;

/// Startプラグインとするか、Optプラグインとするか
#[derive(PartialEq, Eq, Clone, Hash)]
pub enum LazyType {
    /// Startプラグイン。起動時に読み込まれる。
    Start,
    /// Optプラグイン。読み込みのタイミングがある。
    Opt(BTreeSet<LoadEvent>),
}

impl LazyType {
    #[inline]
    /// Startプラグインかどうかを判定する。
    pub fn is_start(&self) -> bool {
        matches!(self, LazyType::Start)
    }
}

impl PartialOrd for LazyType {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LazyType {
    fn cmp(&self, other: &Self) -> Ordering {
        if let (LazyType::Start, LazyType::Start) = (self, other) {
            return Ordering::Equal;
        }
        if let (LazyType::Opt(l_opt), LazyType::Opt(r_opt)) = (self, other) {
            let len_cmp = l_opt.len().cmp(&r_opt.len());
            if len_cmp != Ordering::Equal {
                return len_cmp;
            }

            return l_opt.iter().cmp(r_opt.iter());
        }

        if let LazyType::Start = self {
            Ordering::Less
        } else {
            Ordering::Greater
        }
    }
}

impl<'a> From<&'a LazyType> for Cow<'a, LazyType> {
    fn from(val: &'a LazyType) -> Self {
        Cow::Borrowed(val)
    }
}

impl From<LazyType> for Cow<'_, LazyType> {
    fn from(value: LazyType) -> Self {
        Cow::Owned(value)
    }
}

impl<'a, Rhs: Into<Cow<'a, LazyType>>> BitAndAssign<Rhs> for LazyType {
    fn bitand_assign(&mut self, rhs: Rhs) {
        let rhs: Cow<'a, LazyType> = rhs.into();
        if let LazyType::Opt(events) = self {
            if let LazyType::Opt(events_rhs) = rhs.borrow() {
                events.extend(events_rhs.clone());
            } else {
                *self = rhs.into_owned();
            }
        }
    }
}

/// Optプラグインの読み込みイベントを表す。
#[derive(Hash, Clone, PartialOrd, Ord, PartialEq, Eq)]
pub enum LoadEvent {
    /// Vim の自動コマンドイベント。
    Autocmd(Autocmd),
    /// Vimのユーザーコマンド
    UserCmd(UserCmd),
    /// 起動ファイルタイプ
    FileType(FileType),
}

/// Vimの自動コマンドの文字列を表す型。
#[derive(Hash, Clone, PartialOrd, Ord, PartialEq, Eq, DeserializeFromStr)]
pub struct Autocmd(Arc<String>);

impl FromStr for Autocmd {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        static AUTOCMD_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[^\p{C}\p{Z}*]+$").unwrap());
        if AUTOCMD_REGEX.is_match(s) {
            Ok(Autocmd(Arc::new(s.to_string())))
        } else {
            Err("Autocmd must not contain control characters, spaces, or asterisks")
        }
    }
}

impl Render for Autocmd {
    fn render(&self, b: &mut sailfish::runtime::Buffer) -> Result<(), sailfish::RenderError> {
        self.0.render(b)
    }
}

/// Vimのユーザーコマンドの文字列を表す型。
#[derive(Hash, Clone, PartialOrd, Ord, PartialEq, Eq, DeserializeFromStr)]
pub struct UserCmd(Arc<String>);

impl FromStr for UserCmd {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut chars = s.chars();
        if s.is_empty() {
            return Err("UserCmd must not be empty");
        }
        if !chars.next().unwrap().is_ascii_uppercase() {
            return Err("User command must start with an ascii uppercase letter");
        }

        if chars.all(|c| c.is_ascii_alphabetic()) {
            Ok(UserCmd(Arc::new(s.to_string())))
        } else {
            Err("UserCmd must consist of ascii alphabetic letters only")
        }
    }
}

impl Render for UserCmd {
    fn render(&self, b: &mut sailfish::runtime::Buffer) -> Result<(), sailfish::RenderError> {
        self.0.render(b)
    }
}

/// Vimのユーザーコマンドの文字列を表す型。
#[derive(Hash, Clone, PartialOrd, Ord, PartialEq, Eq, DeserializeFromStr)]
pub struct FileType(Arc<String>);

impl FromStr for FileType {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        {
            Ok(FileType(Arc::new(s.to_string())))
        } else {
            Err(
                "FileType must consist of ascii alphanumeric characters, underscores, hyphens, or dots",
            )
        }
    }
}

impl Render for FileType {
    fn render(&self, b: &mut sailfish::runtime::Buffer) -> Result<(), sailfish::RenderError> {
        self.0.render(b)
    }
}

impl fmt::Display for FileType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
