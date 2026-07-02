use std::{
    cmp::Ordering,
    collections::BTreeSet,
    iter::Sum,
    ops::{Add, AddAssign, Deref},
    path::Path,
    sync::Arc,
};

use crate::rsplug::util::hash;
use sailfish::runtime::Render;

/// 固定されたプラグインのID(表示や書き込み用)。
/// インストールが済んだ後に使用するのが望ましい。未インストールの PluginID は変更される可能性があるため。
#[derive(Hash, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct PluginIDStr([u8; 32]);

impl Render for PluginIDStr {
    fn render(&self, b: &mut sailfish::runtime::Buffer) -> Result<(), sailfish::RenderError> {
        b.push_str(self);
        Ok(())
    }
}

impl From<PluginIDStr> for Box<[u8]> {
    fn from(val: PluginIDStr) -> Self {
        val.0.into()
    }
}

impl AsRef<str> for PluginIDStr {
    fn as_ref(&self) -> &str {
        self as &str
    }
}

impl AsRef<Path> for PluginIDStr {
    fn as_ref(&self) -> &Path {
        (self as &str).as_ref()
    }
}

impl Deref for PluginIDStr {
    type Target = str;

    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { str::from_utf8_unchecked(&self.0) }
    }
}

impl From<PluginIDStr> for Arc<str> {
    fn from(val: PluginIDStr) -> Self {
        Arc::from(unsafe { str::from_utf8_unchecked(&val.0) })
    }
}

impl std::fmt::Display for PluginIDStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (self as &str).fmt(f)
    }
}

/// パッケージID。ディレクトリ名として使用される。
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct PluginID(pub(super) BTreeSet<[u8; 16]>);

impl Ord for PluginID {
    fn cmp(&self, other: &Self) -> Ordering {
        let cmp = self.0.len().cmp(&other.0.len());
        if let Ordering::Equal = cmp {
            for (a, b) in self.0.iter().zip(other.0.iter()) {
                let cmp = a.cmp(b);
                if !cmp.is_eq() {
                    return cmp;
                }
            }
        }
        cmp
    }
}

impl PartialOrd for PluginID {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PluginID {
    /// (内部用) [`std::hash::Hash`] 実装から生成する。
    ///
    /// 新しいID入力は `#[derive(Hash)]` した小さな入力型で表し、こちらを使う。
    /// どのフィールドがIDに影響するかを型定義へ集約できるため、呼び出し側で
    /// バイト列を手で連結する必要がなくなる。
    pub(super) fn from_hash<T: std::hash::Hash + ?Sized>(value: &T) -> Self {
        Self(BTreeSet::from([hash::digest_hash(value)]))
    }

    /// 文字列に変換
    pub fn as_str(&self) -> PluginIDStr {
        let PluginID(inner) = self;
        PluginIDStr(hash::to_hex_bytes(hash::digest_hash(inner)))
    }
}

impl Add for PluginID {
    type Output = Self;
    fn add(mut self, rhs: Self) -> Self::Output {
        self += rhs;
        self
    }
}

impl Sum for PluginID {
    fn sum<I: Iterator<Item = Self>>(mut iter: I) -> Self {
        let mut id0 = iter
            .next()
            .expect("PluginID's Sum Implementation requires at least one element");
        for id in iter {
            id0 += id;
        }
        id0
    }
}

impl AddAssign for PluginID {
    fn add_assign(&mut self, rhs: Self) {
        self.0.extend(rhs.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Hash)]
    struct GeneratedFileId<'a> {
        path: &'a str,
        data: &'a [u8],
    }

    #[test]
    fn plugin_id_can_be_derived_from_hash_input_structs() {
        let left = PluginID::from_hash(&GeneratedFileId {
            path: "plugin/generated.lua",
            data: b"vim.g.generated = true",
        });
        let right = PluginID::from_hash(&GeneratedFileId {
            path: "plugin/generated.lua",
            data: b"vim.g.generated = true",
        });

        assert_eq!(left, right);
    }

    #[test]
    fn derived_plugin_id_tracks_each_hashed_field() {
        let baseline = PluginID::from_hash(&GeneratedFileId {
            path: "plugin/generated.lua",
            data: b"vim.g.generated = true",
        });
        let different_path = PluginID::from_hash(&GeneratedFileId {
            path: "plugin/other.lua",
            data: b"vim.g.generated = true",
        });
        let different_data = PluginID::from_hash(&GeneratedFileId {
            path: "plugin/generated.lua",
            data: b"vim.g.generated = false",
        });

        assert_ne!(baseline, different_path);
        assert_ne!(baseline, different_data);
    }
}
