use std::{
    borrow::{Borrow, Cow},
    cmp::Ordering,
    collections::BTreeSet,
    ops::BitAndAssign,
};

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
        if let LazyType::Start = self
            && let LazyType::Start = other
        {
            return Ordering::Equal;
        }
        if let LazyType::Opt(l_opt) = self
            && let LazyType::Opt(r_opt) = other
        {
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
    Autocmd(String),
}
