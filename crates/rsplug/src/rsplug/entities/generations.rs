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
    /// この世代で起動中の Neovim の数（生きているリースの数）。
    pub in_use: usize,
}

/// 起動中の Neovim が `pack/_gen/leases/<世代ID>/<pid>.json` に置くリース。
/// 世代の loader が起動時に書き、`VimLeavePre` で消す。
#[derive(Deserialize)]
struct Lease {
    pid: i64,
    #[serde(default)]
    host: String,
}

/// 書き込み途中で読めないリースを、壊れたものとみなして消すまでの猶予。
const UNREADABLE_LEASE_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

#[cfg(unix)]
fn hostname() -> String {
    let mut buf = [0u8; 256];
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return String::new();
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

/// 別ホストのリースと `kill(pid, 0)` が `ESRCH` 以外を返す pid は生存とみなす。
/// PID の再利用では余分に保持するだけで、使用中の世代を消す方向には倒れない。
#[cfg(unix)]
fn lease_is_live(lease: &Lease, host: &str) -> bool {
    if !lease.host.is_empty() && lease.host != host {
        return true;
    }
    let Ok(pid) = libc::pid_t::try_from(lease.pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// 生きているリースを世代ごとに数える。死んだ pid のリースと空になったディレクトリは
/// ここで消す。リースは unix でのみ書かれる。
pub(super) async fn live_leases(gen_root: &Path) -> BTreeMap<String, usize> {
    let mut live = BTreeMap::new();
    #[cfg(unix)]
    {
        let host = hostname();
        let Ok(mut generations) = tokio::fs::read_dir(gen_root.join("leases")).await else {
            return live;
        };
        while let Ok(Some(generation)) = generations.next_entry().await {
            let Some(id) = generation.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(mut leases) = tokio::fs::read_dir(generation.path()).await else {
                continue;
            };
            let mut count = 0;
            while let Ok(Some(entry)) = leases.next_entry().await {
                let path = entry.path();
                let live = match tokio::fs::read(&path).await {
                    Ok(content) => match serde_json::from_slice::<Lease>(&content) {
                        Ok(lease) => lease_is_live(&lease, &host),
                        Err(_) => entry
                            .metadata()
                            .await
                            .ok()
                            .and_then(|metadata| metadata.modified().ok())
                            .and_then(|modified| modified.elapsed().ok())
                            .is_some_and(|age| age < UNREADABLE_LEASE_GRACE),
                    },
                    Err(_) => continue,
                };
                if live {
                    count += 1;
                } else {
                    tokio::fs::remove_file(&path).await.ok();
                }
            }
            if count == 0 {
                tokio::fs::remove_dir(generation.path()).await.ok();
            } else if is_generation_name(&id) {
                live.insert(id, count);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = gen_root;
    live
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
    in_use: usize,
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
        in_use,
    }
}

/// 保持されている世代を新しい順に返す。registry から外れていても起動中の Neovim が
/// 使っている世代は保持されているので、registry の後ろに足す。
pub async fn list_generations(packpath: &Path) -> io::Result<Vec<GenerationInfo>> {
    let gen_root = packpath.join("pack").join("_gen");
    let current = current_generation(packpath).await;
    let mut leases = live_leases(&gen_root).await;
    let registry = read_generation_registry(&gen_root).await;
    let mut ids: Vec<String> = registry
        .into_iter()
        .filter(|id| is_generation_name(id))
        .collect();
    for id in leases.keys() {
        if !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    let mut generations = Vec::new();
    for id in ids {
        if let Some(manifest) = read_manifest(&gen_root, &id).await {
            let in_use = leases.remove(&id).unwrap_or(0);
            generations.push(
                generation_info(
                    packpath,
                    &gen_root,
                    &id,
                    &manifest,
                    current.as_deref(),
                    in_use,
                )
                .await,
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

    let in_use = live_leases(&gen_root).await.remove(&id).unwrap_or(0);
    let info = generation_info(packpath, &gen_root, &id, &manifest, Some(&id), in_use).await;
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

    async fn write_lease(gen_root: &Path, id: &str, pid: u32, host: &str) -> PathBuf {
        let dir = gen_root.join("leases").join(id);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join(format!("{pid}.json"));
        tokio::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({ "pid": pid, "host": host, "started": 0 }))
                .unwrap(),
        )
        .await
        .unwrap();
        path
    }

    /// 終了済みの子プロセスの pid。すぐには再利用されない。
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[tokio::test]
    async fn live_leases_count_running_pids_and_drop_dead_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let gen_root = tmp.path();
        let host = hostname();
        write_lease(gen_root, OLD, std::process::id(), &host).await;
        let dead = write_lease(gen_root, NEW, dead_pid(), &host).await;
        // 別ホストの pid は確かめられないので生存とみなす。
        write_lease(gen_root, PRUNED, dead_pid(), "elsewhere").await;

        let live = live_leases(gen_root).await;
        assert_eq!(
            live.into_iter().collect::<Vec<_>>(),
            [(OLD.to_string(), 1), (PRUNED.to_string(), 1)]
        );
        assert!(!dead.exists() && !dead.parent().unwrap().exists());
    }

    #[tokio::test]
    async fn leased_generations_outside_the_registry_are_kept_and_listed() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path()).await;
        let gen_root = tmp.path().join("pack").join("_gen");
        // PRUNED は registry に無いが、起動中の Neovim が使っている。
        tokio::fs::copy(
            gen_root
                .join("generations")
                .join(OLD)
                .with_extension("json"),
            gen_root
                .join("generations")
                .join(PRUNED)
                .with_extension("json"),
        )
        .await
        .unwrap();
        let lua = tmp
            .path()
            .join("generations")
            .join(PRUNED)
            .with_extension("lua");
        tokio::fs::write(&lua, b"").await.unwrap();
        write_lease(&gen_root, PRUNED, std::process::id(), &hostname()).await;

        prune_unregistered_generations(&gen_root, tmp.path()).await;
        assert!(lua.exists());

        let generations = list_generations(tmp.path()).await.unwrap();
        let ids: Vec<_> = generations
            .iter()
            .map(|g| (g.id.as_str(), g.in_use))
            .collect();
        assert_eq!(ids, [(NEW, 0), (OLD, 0), (PRUNED, 1)]);
    }
}
