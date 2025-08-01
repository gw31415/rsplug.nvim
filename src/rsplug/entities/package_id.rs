use std::{
    cmp::Ordering,
    collections::BTreeSet,
    mem::MaybeUninit,
    ops::{Add, AddAssign, Deref},
    path::Path,
    sync::Arc,
};

use itertools::Itertools;
use sailfish::runtime::Render;
use xxhash_rust::xxh3::xxh3_128;

/// 固定されたパッケージID(表示や書き込み用)。
/// インストールが済んだ後に使用するのが望ましい。未インストールの PackageID は変更される可能性があるため。
#[derive(Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageIDStr([u8; 32]);

impl Render for PackageIDStr {
    fn render(&self, b: &mut sailfish::runtime::Buffer) -> Result<(), sailfish::RenderError> {
        b.push_str(self);
        Ok(())
    }
}

impl From<PackageIDStr> for Box<[u8]> {
    fn from(val: PackageIDStr) -> Self {
        val.0.into()
    }
}

impl AsRef<str> for PackageIDStr {
    fn as_ref(&self) -> &str {
        self as &str
    }
}

impl AsRef<Path> for PackageIDStr {
    fn as_ref(&self) -> &Path {
        (self as &str).as_ref()
    }
}

impl Deref for PackageIDStr {
    type Target = str;

    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { str::from_utf8_unchecked(&self.0) }
    }
}

impl From<PackageIDStr> for Arc<str> {
    fn from(val: PackageIDStr) -> Self {
        Arc::from(unsafe { str::from_utf8_unchecked(&val.0) })
    }
}

impl std::fmt::Display for PackageIDStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (self as &str).fmt(f)
    }
}

/// パッケージID。ディレクトリ名として使用される。
#[derive(Hash, PartialEq, Eq)]
pub struct PackageID(pub(super) BTreeSet<[u8; 16]>);

impl Ord for PackageID {
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

impl PartialOrd for PackageID {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PackageID {
    /// (内部用) 任意のデータからハッシュ利用し生成する。
    pub(super) fn new(data: impl AsRef<[u8]>) -> Self {
        Self(BTreeSet::from([u128::to_ne_bytes(xxh3_128(data.as_ref()))]))
    }
    /// 文字列に変換
    pub fn as_str(&self) -> PackageIDStr {
        const TABLE: &[u8; 16] = b"0123456789abcdef";
        let PackageID(inner) = self;
        let bytes = inner.iter().flat_map(ToOwned::to_owned).collect_vec();
        let hash: [u8; 16] = xxh3_128(&bytes).to_ne_bytes();
        let mut res = const { [MaybeUninit::<u8>::uninit(); 32] };
        for (i, b) in hash.iter().enumerate() {
            let i = i << 1;
            unsafe {
                res.get_mut(i)
                    .unwrap_unchecked()
                    .write(TABLE[(b / 16u8) as usize]);
                res.get_mut(i + 1)
                    .unwrap_unchecked()
                    .write(TABLE[(b % 16u8) as usize]);
            }
        }
        PackageIDStr(unsafe { std::mem::transmute::<[MaybeUninit<u8>; 32], [u8; 32]>(res) })
    }
}

impl Add for PackageID {
    type Output = Self;
    fn add(mut self, rhs: Self) -> Self::Output {
        self += rhs;
        self
    }
}

impl AddAssign for PackageID {
    fn add_assign(&mut self, rhs: Self) {
        self.0.extend(rhs.0);
    }
}
