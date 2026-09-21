use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};

/// Directory next to a worktree that holds generated files awaiting deletion.
pub fn trash_dir(worktree: &Path) -> Option<PathBuf> {
    Some(worktree.parent()?.join(".wt-trash"))
}

/// Move the configured paths into `trash` so removal does not wait for large
/// generated trees. Paths on another filesystem are deleted in place.
pub fn trash_paths(root: &Path, paths: &[PathBuf], trash: &Path) -> Result<()> {
    let roots = removal_roots(root, paths)?;
    if roots.is_empty() {
        return Ok(());
    }
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_nanos());
    let batch = trash.join(format!(
        "{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&batch).with_context(|| format!("create {}", batch.display()))?;
    for (index, path) in roots.iter().enumerate() {
        let source = root.join(path);
        match fs::rename(&source, batch.join(index.to_string())) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
                remove_roots(root, std::slice::from_ref(path))?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("remove {}", source.display()));
            }
        }
    }
    Ok(())
}

/// Delete trashed files in a detached process that outlives this command.
pub fn purge_in_background(trash: &Path) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut command = Command::new(exe);
    command
        .arg("purge-trash")
        .arg(trash)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    // Leftovers are retried by the next removal, so a failed start is harmless.
    let _ = command.spawn();
}

/// Delete everything in `trash`. Concurrent purges may race, so errors only
/// leave entries for a later purge.
pub fn purge(trash: &Path) {
    // Never empty an arbitrary directory passed to the hidden command.
    if trash.file_name() != Some(".wt-trash".as_ref()) {
        return;
    }
    let Ok(entries) = fs::read_dir(trash) else {
        return;
    };
    let names = entries
        .filter_map(|entry| Some(PathBuf::from(entry.ok()?.file_name())))
        .collect::<Vec<_>>();
    let _ = remove_paths(trash, &names);
    let _ = fs::remove_dir(trash);
}

/// Remove only the configured paths, without following directory symlinks.
pub fn remove_paths(root: &Path, paths: &[PathBuf]) -> Result<()> {
    let roots = removal_roots(root, paths)?;
    remove_roots(root, &roots)
}

/// Deduplicated top-level paths, refusing any that sit below a symlink.
fn removal_roots(root: &Path, paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut paths = paths.to_vec();
    paths.sort();
    paths.dedup();
    let mut roots: Vec<PathBuf> = Vec::new();
    for path in paths {
        if !roots.iter().any(|parent| path.starts_with(parent)) {
            // A configured path may have acquired a symlink ancestor since add.
            for parent in path
                .ancestors()
                .skip(1)
                .filter(|p| !p.as_os_str().is_empty())
            {
                let ancestor = root.join(parent);
                match fs::symlink_metadata(&ancestor) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        bail!(
                            "cleanup path has a symlink ancestor: {}",
                            ancestor.display()
                        );
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("inspect {}", ancestor.display()));
                    }
                }
            }
            roots.push(path);
        }
    }
    Ok(roots)
}

fn remove_roots(root: &Path, roots: &[PathBuf]) -> Result<()> {
    let mut tasks = Vec::new();
    let mut parents = Vec::new();
    for path in roots {
        partition(&root.join(path), 2, &mut tasks, &mut parents)?;
    }
    let next = AtomicUsize::new(0);
    thread::scope(|scope| {
        let workers = (0..tasks.len().min(4))
            .map(|_| {
                scope.spawn(|| -> Result<()> {
                    while let Some(path) = tasks.get(next.fetch_add(1, Ordering::Relaxed)) {
                        remove_one(path)?;
                    }
                    Ok(())
                })
            })
            .collect::<Vec<_>>();
        // Join every worker before returning an error or removing parent directories.
        let results = workers
            .into_iter()
            .map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("cleanup worker panicked")))
            })
            .collect::<Vec<_>>();
        results.into_iter().collect::<Result<Vec<_>>>()
    })?;
    for parent in parents {
        fs::remove_dir(&parent).with_context(|| format!("remove {}", parent.display()))?;
    }
    Ok(())
}

fn partition(
    path: &Path,
    depth: usize,
    tasks: &mut Vec<PathBuf>,
    parents: &mut Vec<PathBuf>,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if depth == 0 || !metadata.is_dir() || metadata.file_type().is_symlink() {
        tasks.push(path.to_owned());
    } else {
        // Two levels expose package subtrees beneath node_modules/.pnpm without
        // walking every dependency file before deletion starts.
        for entry in fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
            let entry = entry.with_context(|| format!("read {}", path.display()))?;
            partition(&entry.path(), depth - 1, tasks, parents)?;
        }
        parents.push(path.to_owned());
    }
    Ok(())
}

fn remove_one(path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    let result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.with_context(|| format!("remove {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_nested_overlapping_and_missing_paths() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("ösalkdfjöalsk/.pnpm/package/lib");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("index.js"), "cached").unwrap();
        fs::write(temp.path().join("keep"), "keep").unwrap();
        remove_paths(
            temp.path(),
            &[
                "ösalkdfjöalsk/.pnpm".into(),
                "missing".into(),
                "ösalkdfjöalsk".into(),
                "ösalkdfjöalsk".into(),
            ],
        )
        .unwrap();
        assert!(!temp.path().join("ösalkdfjöalsk").exists());
        assert_eq!(
            fs::read_to_string(temp.path().join("keep")).unwrap(),
            "keep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unlinks_symlinks_at_every_partition_depth_without_touching_targets() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep"), "keep").unwrap();
        fs::create_dir_all(temp.path().join("generated/one/two/three")).unwrap();
        for parent in ["", "generated", "generated/one", "generated/one/two/three"] {
            symlink(outside.path(), temp.path().join(parent).join("link")).unwrap();
        }
        symlink("missing", temp.path().join("generated/dangling")).unwrap();
        remove_paths(temp.path(), &["link".into(), "generated".into()]).unwrap();
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
        assert_eq!(
            fs::read_to_string(outside.path().join("keep")).unwrap(),
            "keep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_ancestors_before_deleting_anything() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(outside.path().join("cache")).unwrap();
        fs::write(temp.path().join("first"), "keep").unwrap();
        std::os::unix::fs::symlink(outside.path(), temp.path().join("link")).unwrap();
        let error = remove_paths(temp.path(), &["first".into(), "link/cache".into()]).unwrap_err();
        assert!(error.to_string().contains("symlink ancestor"));
        assert!(temp.path().join("first").exists());
        assert!(outside.path().join("cache").exists());
    }

    #[test]
    #[ignore = "manual filesystem benchmark: cargo test --release cleanup::tests::benchmark -- --ignored --nocapture"]
    fn benchmark() {
        use std::time::Instant;
        let temp = tempfile::tempdir().unwrap();
        for round in 0..3 {
            for parallel in if round % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let root = temp.path().join("node_modules");
                for package in 0..200 {
                    let dir =
                        root.join(format!(".pnpm/package-{package}/node_modules/package/lib"));
                    fs::create_dir_all(&dir).unwrap();
                    for file in 0..100 {
                        fs::write(dir.join(format!("file-{file}.js")), "export default 1;\n")
                            .unwrap();
                    }
                }
                let started = Instant::now();
                if parallel {
                    remove_paths(temp.path(), &["node_modules".into()]).unwrap();
                } else {
                    fs::remove_dir_all(&root).unwrap();
                }
                eprintln!(
                    "round {} {}: {:.3}s (20,000 files)",
                    round + 1,
                    if parallel { "parallel" } else { "serial" },
                    started.elapsed().as_secs_f64()
                );
                assert!(!root.exists());
            }
        }
    }
}
