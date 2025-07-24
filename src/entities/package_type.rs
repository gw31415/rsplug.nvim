use std::{
    borrow::{Borrow, Cow},
    collections::BTreeSet,
    ops::BitAndAssign,
};

/// Startプラグインとするか、Optプラグインとするか
#[derive(PartialEq, Eq, Clone, Hash)]
pub enum PackageType {
    /// Startプラグイン。起動時に読み込まれる。
    Start,
    /// Optプラグイン。読み込みのタイミングがある。
    Opt(BTreeSet<LoadEvent>),
}

impl PartialOrd for PackageType {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PackageType {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if let PackageType::Start = self
            && let PackageType::Start = other
        {
            return std::cmp::Ordering::Equal;
        }
        if let PackageType::Opt(l_opt) = self
            && let PackageType::Opt(r_opt) = other
        {
            let len_cmp = l_opt.len().cmp(&r_opt.len());
            if len_cmp != std::cmp::Ordering::Equal {
                return len_cmp;
            }

            return l_opt.iter().cmp(r_opt.iter());
        }

        if let PackageType::Start = self {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        }
    }
}

impl<'a> From<&'a PackageType> for Cow<'a, PackageType> {
    fn from(val: &'a PackageType) -> Self {
        Cow::Borrowed(val)
    }
}

impl From<PackageType> for Cow<'_, PackageType> {
    fn from(value: PackageType) -> Self {
        Cow::Owned(value)
    }
}

impl<'a, Rhs: Into<Cow<'a, PackageType>>> BitAndAssign<Rhs> for PackageType {
    fn bitand_assign(&mut self, rhs: Rhs) {
        let rhs: Cow<'a, PackageType> = rhs.into();
        if let PackageType::Opt(events) = self {
            if let PackageType::Opt(events_rhs) = rhs.borrow() {
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
    /// 手動で packadd
    Autocmd(String),
}
