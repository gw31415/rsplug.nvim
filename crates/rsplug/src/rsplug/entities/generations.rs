//! 保持されている世代の一覧と切り替え。
//!
//! 各世代の loader は `generations/<世代ID>.lua` にあり、`init.lua` はその1つへの
//! symlink。切り替えは symlink の付け替えと registry の先頭移動だけで、パッケージは
//! コピーしない。

use super::*;

/// registry に残っている世代1件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationInfo {
    pub id: String,
    /// `init.lua` が現在この世代を指している。
    pub current: bool,
    /// 世代が参照するパッケージ数。
    pub packages: usize,
    /// loader の最終更新時刻（publish または切り替えの時刻）。
    pub published: Option<std::time::SystemTime>,
    /// loader と全パッケージが揃っていて起動できる。
    pub bootable: bool,
}

async fn current_generation(packpath: &Path) -> Option<String> {
    let target = tokio::fs::read_link(packpath.join("init.lua")).await.ok()?;
    target.file_stem()?.to_str().map(str::to_owned)
}

async fn read_manifest(gen_root: &Path, id: &str) -> Option<GenerationManifest> {
    let path = gen_root.join("generations").join(id).with_extension("json");
    let content = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&content).ok()
}

async fn generation_info(
    packpath: &Path,
    gen_root: &Path,
    id: &str,
    manifest: &GenerationManifest,
    current: Option<&str>,
) -> GenerationInfo {
    let loader = packpath.join("generations").join(id).with_extension("lua");
    let published = tokio::fs::metadata(&loader)
        .await
        .ok()
        .and_then(|metadata| metadata.modified().ok());
    let mut bootable = tokio::fs::metadata(&loader)
        .await
        .is_ok_and(|metadata| metadata.is_file());
    for entry in &manifest.entries {
        if !bootable {
            break;
        }
        bootable = tokio::fs::metadata(gen_root.join(entry))
            .await
            .is_ok_and(|metadata| metadata.is_dir());
    }
    GenerationInfo {
        id: id.to_string(),
        current: current == Some(id),
        packages: manifest.entries.len(),
        published,
        bootable,
    }
}

/// 保持されている世代を新しい順に返す。
pub async fn list_generations(packpath: &Path) -> io::Result<Vec<GenerationInfo>> {
    let gen_root = packpath.join("pack").join("_gen");
    let current = current_generation(packpath).await;
    let mut generations = Vec::new();
    for id in read_generation_registry(&gen_root).await {
        if !is_generation_name(&id) {
            continue;
        }
        if let Some(manifest) = read_manifest(&gen_root, &id).await {
            generations.push(
                generation_info(packpath, &gen_root, &id, &manifest, current.as_deref()).await,
            );
        }
    }
    Ok(generations)
}

/// `query`（世代IDまたはその一意な先頭部分）に一致する保持世代へ `init.lua` を切り替える。
///
/// 切り替えた世代は registry の先頭に移り、以後の publish でも保持される。設定から
/// 生成される世代へ戻すには通常どおり `rsplug` を実行する。
pub async fn switch_generation(packpath: &Path, query: &str) -> io::Result<GenerationInfo> {
    let query = query.to_ascii_lowercase();
    if query.is_empty() || query.len() > 32 || !query.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid generation ID {query:?}: expected up to 32 hexadecimal characters"),
        ));
    }
    let gen_root = packpath.join("pack").join("_gen");
    tokio::fs::create_dir_all(&gen_root).await?;
    #[cfg(unix)]
    let _install_lock = acquire_install_lock(&gen_root).await?;

    let candidates: Vec<String> = read_generation_registry(&gen_root)
        .await
        .into_iter()
        .filter(|id| is_generation_name(id) && id.starts_with(&query))
        .collect();
    let id = match candidates.as_slice() {
        [id] => id.clone(),
        [] => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "no retained generation matches {query:?}; see `rsplug --list-generations`"
                ),
            ));
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "generation ID {query:?} is ambiguous: {}",
                    candidates.join(", ")
                ),
            ));
        }
    };
    let Some(manifest) = read_manifest(&gen_root, &id).await else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("generation {id} has no readable manifest"),
        ));
    };

    let loader = packpath.join("generations").join(&id).with_extension("lua");
    // 世代 loader を現在のテンプレートで描き直す。古い rsplug が書いた loader は
    // RSPLUG_GENERATION を誤解釈するため、切り替えを機に更新しておく。plan を持たない
    // 古い manifest は control ID が分からないので既存の loader をそのまま使う。
    if let Some(plan) = &manifest.plan {
        let tmp = loader.with_extension(format!(
            "lua.tmp-{}",
            STAGING_NONCE.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        tokio::fs::write(&tmp, render_init(&plan.control_ids)).await?;
        tokio::fs::rename(&tmp, &loader).await?;
    }

    let info = generation_info(packpath, &gen_root, &id, &manifest, Some(&id)).await;
    if !info.bootable {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("generation {id} is missing packages and cannot be booted"),
        ));
    }
    point_init_to(packpath, &loader).await?;
    write_generation_registry(&gen_root, &id).await?;
    Ok(info)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const OLD: &str = "11111111111111111111111111111111";
    const NEW: &str = "22222222222222222222222222222222";
    const PRUNED: &str = "33333333333333333333333333333333";

    /// 2世代ぶんの公開済みレイアウトを作り、init.lua は NEW を指す。
    async fn fixture(packpath: &Path) {
        let gen_root = packpath.join("pack").join("_gen");
        tokio::fs::create_dir_all(gen_root.join("generations"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(packpath.join("generations"))
            .await
            .unwrap();
        for (id, control) in [
            (OLD, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            (NEW, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        ] {
            let entry = format!("opt/{control}");
            tokio::fs::create_dir_all(gen_root.join(&entry))
                .await
                .unwrap();
            let manifest = GenerationManifest {
                version: 2,
                entries: vec![entry.clone()],
                generation_id: id.to_string(),
                plan: Some(GenerationPlan {
                    schema: GenerationPlan::SCHEMA,
                    merge_abi: GenerationPlan::MERGE_ABI,
                    entries: vec![entry],
                    control_ids: vec![control.to_string()],
                    runtime: RuntimeManifest::default(),
                }),
                runtime: RuntimeManifest::default(),
            };
            tokio::fs::write(
                gen_root.join("generations").join(id).with_extension("json"),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .await
            .unwrap();
            tokio::fs::write(
                packpath.join("generations").join(id).with_extension("lua"),
                b"-- stale loader\n",
            )
            .await
            .unwrap();
        }
        write_generation_registry(&gen_root, OLD).await.unwrap();
        write_generation_registry(&gen_root, NEW).await.unwrap();
        point_init_to(
            packpath,
            &packpath.join("generations").join(NEW).with_extension("lua"),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn lists_retained_generations_newest_first_with_current_marked() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path()).await;
        let generations = list_generations(tmp.path()).await.unwrap();
        let ids: Vec<_> = generations.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, [NEW, OLD]);
        assert!(generations[0].current && !generations[1].current);
        assert!(generations.iter().all(|g| g.bootable && g.packages == 1));
    }

    #[tokio::test]
    async fn switch_by_prefix_repoints_init_and_rerenders_loader() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path()).await;
        let info = switch_generation(tmp.path(), "1111").await.unwrap();
        assert_eq!(info.id, OLD);
        assert!(info.current);

        let loader = tmp
            .path()
            .join("generations")
            .join(OLD)
            .with_extension("lua");
        assert_eq!(
            tokio::fs::canonicalize(tmp.path().join("init.lua"))
                .await
                .unwrap(),
            tokio::fs::canonicalize(&loader).await.unwrap()
        );
        let content = tokio::fs::read(&loader).await.unwrap();
        assert_eq!(content, render_init(&["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]));

        let ids: Vec<_> = list_generations(tmp.path())
            .await
            .unwrap()
            .into_iter()
            .map(|g| (g.id, g.current))
            .collect();
        assert_eq!(ids, [(OLD.to_string(), true), (NEW.to_string(), false)]);
    }

    #[tokio::test]
    async fn switch_rejects_unknown_ambiguous_and_broken_generations() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path()).await;
        let err = |e: io::Error| e.to_string();

        assert!(
            err(switch_generation(tmp.path(), "4").await.unwrap_err())
                .contains("no retained generation")
        );
        assert!(
            err(switch_generation(tmp.path(), "../x").await.unwrap_err())
                .contains("invalid generation ID")
        );

        // OLD と同じ先頭を持つ世代を registry に足すと、先頭一致が曖昧になる。
        let gen_root = tmp.path().join("pack").join("_gen");
        let twin = "11111111111111111111111111111112";
        tokio::fs::copy(
            gen_root
                .join("generations")
                .join(OLD)
                .with_extension("json"),
            gen_root
                .join("generations")
                .join(twin)
                .with_extension("json"),
        )
        .await
        .unwrap();
        write_generation_registry(&gen_root, twin).await.unwrap();
        assert!(
            err(switch_generation(tmp.path(), "1111").await.unwrap_err()).contains("ambiguous")
        );

        // パッケージが消えた世代へは切り替えず、init.lua も動かさない。
        tokio::fs::remove_dir_all(gen_root.join("opt/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))
            .await
            .unwrap();
        assert!(
            err(switch_generation(tmp.path(), OLD).await.unwrap_err()).contains("cannot be booted")
        );
        assert_eq!(
            tokio::fs::read_link(tmp.path().join("init.lua"))
                .await
                .unwrap()
                .file_stem()
                .unwrap(),
            NEW
        );
    }

    #[tokio::test]
    async fn registry_keeps_the_newest_retain_generations() {
        let tmp = tempfile::tempdir().unwrap();
        let gen_root = tmp.path().join("pack").join("_gen");
        tokio::fs::create_dir_all(gen_root.join("generations"))
            .await
            .unwrap();
        let ids: Vec<String> = (0..RETAIN_GENERATIONS + 2)
            .map(|i| format!("{i:032x}"))
            .collect();
        for id in &ids {
            write_generation_registry(&gen_root, id).await.unwrap();
        }
        let registry = read_generation_registry(&gen_root).await;
        assert_eq!(RETAIN_GENERATIONS, 64);
        assert_eq!(registry.len(), RETAIN_GENERATIONS);
        assert_eq!(registry.first(), ids.last());
        assert!(!registry.contains(&ids[0]) && !registry.contains(&ids[1]));
    }

    #[tokio::test]
    async fn prune_drops_metadata_of_unregistered_generations() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path()).await;
        let gen_root = tmp.path().join("pack").join("_gen");
        let json = gen_root
            .join("generations")
            .join(PRUNED)
            .with_extension("json");
        let lua = tmp
            .path()
            .join("generations")
            .join(PRUNED)
            .with_extension("lua");
        tokio::fs::write(&json, b"{}").await.unwrap();
        tokio::fs::write(&lua, b"").await.unwrap();

        prune_unregistered_generations(&gen_root, tmp.path()).await;

        assert!(!json.exists() && !lua.exists());
        for id in [OLD, NEW] {
            assert!(
                gen_root
                    .join("generations")
                    .join(id)
                    .with_extension("json")
                    .exists()
            );
            assert!(
                tmp.path()
                    .join("generations")
                    .join(id)
                    .with_extension("lua")
                    .exists()
            );
        }
        assert!(gen_root.join("generations").join("registry.json").exists());
    }
}
