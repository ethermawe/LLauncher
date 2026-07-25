use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tauri::Emitter;

pub struct FileProgress {
    pub bytes: AtomicU64,
    pub active: AtomicBool,
    pub verifying: AtomicBool,
}

use crate::api::types::{DownloadComplete, DownloadError, ResumeInfo};
use crate::error::AppError;

pub async fn start_download(
    app: tauri::AppHandle,
    client: reqwest::Client,
    download_active: Arc<AtomicBool>,
    download_dir: &str,
    game_dir: &str,
    current_version: &str,
    speed_limit: u64,
    max_concurrent: u32,
) -> Result<String, AppError> {
    download_active.store(true, Ordering::SeqCst);

    let download_path = Path::new(download_dir);
    let game_path = Path::new(game_dir);

    let mut version_info =
        crate::api::client::get_latest_game_version(&client, current_version).await?;

    // If API returned no packs (e.g. version already matches latest but game files are missing),
    // retry with empty version to request a full download
    if version_info.pkg.packs.is_empty() && !current_version.is_empty() {
        version_info =
            crate::api::client::get_latest_game_version(&client, "").await?;
    }

    if version_info.pkg.packs.is_empty() {
        download_active.store(false, Ordering::SeqCst);
        let msg = "No download packages available from server".to_string();
        app.emit("download://error", DownloadError { message: msg.clone() }).ok();
        return Err(AppError::Api(msg));
    }

    let version = version_info.version.clone();
    let packs = &version_info.pkg.packs;
    let total_files = packs.len();

    // Calculate total size for aggregate progress
    let total_size: u64 = packs
        .iter()
        .map(|p| p.package_size.parse::<u64>().unwrap_or(0))
        .sum();

    // Refuse early when the download clearly cannot fit, instead of failing
    // 40 GB in. Already-downloaded pack files do not need space again.
    let existing_parts: u64 = packs
        .iter()
        .filter_map(|p| {
            let name = p.url.split('/').last().unwrap_or("unknown");
            std::fs::metadata(download_path.join(name)).ok().map(|m| m.len())
        })
        .sum();
    let needed = total_size.saturating_sub(existing_parts);
    std::fs::create_dir_all(download_path)?;

    // Tell the frontend how much data already exists on disk so it can show
    // a distinct "verifying downloaded files" phase instead of making disk
    // reads look like internet downloads.
    if existing_parts > 0 {
        app.emit(
            "download://resuming",
            ResumeInfo {
                bytes_existing: existing_parts,
                bytes_total: total_size,
            },
        )
        .ok();
    }

    if let Some(available) = crate::config::paths::available_space(download_path) {
        if available < needed {
            download_active.store(false, Ordering::SeqCst);
            let err = AppError::DiskSpace {
                path: download_dir.to_string(),
                needed_mib: needed / (1024 * 1024),
                available_mib: available / (1024 * 1024),
            };
            app.emit("download://error", DownloadError { message: err.to_string() }).ok();
            return Err(err);
        }
    }

    let file_progress: Arc<Vec<Arc<FileProgress>>> = Arc::new(
        (0..total_files).map(|_| Arc::new(FileProgress {
            bytes: AtomicU64::new(0),
            active: AtomicBool::new(false),
            verifying: AtomicBool::new(false),
        })).collect()
    );

    let monitor_active = download_active.clone();
    let monitor_progress = file_progress.clone();
    let monitor_app = app.clone();
    let packs_clone = packs.clone();
    let ts = total_size;

    tokio::spawn(async move {
        let mut history: std::collections::VecDeque<(std::time::Instant, u64)> = std::collections::VecDeque::new();
        
        while monitor_active.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            
            let mut total_bytes = 0;
            let mut active_file_idx = None;
            let mut any_verifying = false;
            
            for (i, fp) in monitor_progress.iter().enumerate() {
                total_bytes += fp.bytes.load(Ordering::Relaxed);
                if fp.active.load(Ordering::Relaxed) {
                    if active_file_idx.is_none() {
                        active_file_idx = Some(i);
                    }
                    if fp.verifying.load(Ordering::Relaxed) {
                        any_verifying = true;
                    }
                }
            }
            
            let now = std::time::Instant::now();
            let speed_bps = if any_verifying {
                history.clear();
                0
            } else {
                history.push_back((now, total_bytes));
                while history.front().map_or(false, |&(t, _)| now.duration_since(t).as_secs_f64() > 3.0) {
                    history.pop_front();
                }
                
                if let Some(&(old_t, old_bytes)) = history.front() {
                    let dt = now.duration_since(old_t).as_secs_f64();
                    if dt > 0.5 {
                        (total_bytes.saturating_sub(old_bytes) as f64 / dt) as u64
                    } else {
                        0
                    }
                } else {
                    0
                }
            };
            
            if let Some(idx) = active_file_idx {
                let file_name = packs_clone[idx].url.split('/').last().unwrap_or("unknown").to_string();
                monitor_app.emit(
                    if any_verifying { "download://verify-progress" } else { "download://progress" },
                    crate::api::types::DownloadProgress {
                        file_index: idx,
                        total_files,
                        file_name,
                        bytes_downloaded: total_bytes,
                        bytes_total: ts,
                        speed_bps,
                    }
                ).ok();
            }
        }
    });

    let per_worker_limit = if speed_limit > 0 {
        (speed_limit / max_concurrent as u64).max(1024)
    } else {
        0
    };

    // Download packs concurrently
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent as usize));
    let mut handles = Vec::new();

    for (i, pack) in packs.iter().enumerate() {
        if !download_active.load(Ordering::SeqCst) {
            break;
        }

        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AppError::Cancelled)?;

        let app2 = app.clone();
        let client2 = client.clone();
        let pack2 = pack.clone();
        let file_name = pack.url.split('/').last().unwrap_or("unknown").to_string();
        let dest = download_path.join(&file_name);
        let active2 = download_active.clone();
        let fp2 = file_progress[i].clone();

        let handle = tokio::spawn(async move {
            let _permit = permit;
            let mut retries = 0;
            loop {
                let res = crate::download::worker::download_file(
                    &app2, &client2, &pack2, &dest, i, total_files, &active2, &fp2, per_worker_limit,
                ).await;
                
                match res {
                    Ok(()) => break Ok(()),
                    Err(e) => {
                        if !active2.load(Ordering::SeqCst) {
                            break Err(AppError::Cancelled);
                        }
                        if matches!(e, AppError::Http(_) | AppError::Io(_)) {
                            if retries < 3 {
                                retries += 1;
                                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                continue;
                            }
                        }
                        break Err(e);
                    }
                }
            }
        });

        handles.push(handle);
    }

    // Wait for all downloads
    for handle in handles {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                download_active.store(false, Ordering::SeqCst);
                app.emit(
                    "download://error",
                    DownloadError {
                        message: e.to_string(),
                    },
                )
                .ok();
                return Err(e);
            }
            Err(e) => {
                download_active.store(false, Ordering::SeqCst);
                let msg = format!("Download task failed: {}", e);
                app.emit("download://error", DownloadError { message: msg.clone() })
                    .ok();
                return Err(AppError::Api(msg));
            }
        }
    }

    // Cancellation may have broken out of the spawn loop after the in-flight
    // workers finished cleanly; never proceed to extraction in that case.
    if !download_active.load(Ordering::SeqCst) {
        app.emit(
            "download://error",
            DownloadError {
                message: AppError::Cancelled.to_string(),
            },
        )
        .ok();
        return Err(AppError::Cancelled);
    }

    let parts: Vec<PathBuf> = packs
        .iter()
        .map(|p| download_path.join(p.url.split('/').last().unwrap_or("unknown")))
        .collect();

    let marker = crate::game::state::incomplete_marker(game_path);
    std::fs::write(&marker, b"extracting").ok();

    let extract_app = app.clone();
    let game_path_owned = game_path.to_path_buf();
    let extract_parts = parts.clone();
    let extract_result = tokio::task::spawn_blocking(move || {
        crate::download::extract::extract_split_zip(
            &extract_app,
            &extract_parts,
            &game_path_owned,
            total_size,
        )
    })
    .await;

    let extract_outcome = match extract_result {
        Ok(inner) => inner,
        Err(e) => Err(AppError::Api(format!("Extraction task failed: {}", e))),
    };

    if let Err(e) = extract_outcome {
        download_active.store(false, Ordering::SeqCst);
        app.emit(
            "download://error",
            DownloadError {
                message: e.to_string(),
            },
        )
        .ok();
        return Err(e);
    }

    std::fs::remove_file(&marker).ok();

    // The pack archives are no longer needed once extracted; leaving them
    // around wastes tens of GB and forces a pointless re-verify on the next
    // repair/update pass.
    for part in &parts {
        std::fs::remove_file(part).ok();
    }

    download_active.store(false, Ordering::SeqCst);

    app.emit("download://complete", DownloadComplete { version: version.clone() }).ok();

    Ok(version)
}
