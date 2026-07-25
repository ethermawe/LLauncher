use futures_util::StreamExt;
use md5::{Digest, Md5};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::Emitter;
use tokio::io::{AsyncWriteExt, BufWriter};

use crate::api::types::PackFile;
use crate::error::AppError;

pub async fn download_file(
    app: &tauri::AppHandle,
    client: &reqwest::Client,
    pack: &PackFile,
    dest: &Path,
    file_index: usize,
    total_files: usize,
    cancel_flag: &Arc<AtomicBool>,
    progress: &Arc<crate::download::manager::FileProgress>,
    speed_limit: u64,
) -> Result<(), AppError> {
    let file_name = pack
        .url
        .split('/')
        .last()
        .unwrap_or("unknown")
        .to_string();

    let expected_size: u64 = pack.package_size.parse().unwrap_or(0);
    let existing_size = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);

    let mut resume_from: u64 = 0;
    let mut hasher = Md5::new();

    struct ActiveGuard<'a> { progress: &'a Arc<crate::download::manager::FileProgress> }
    impl<'a> Drop for ActiveGuard<'a> { fn drop(&mut self) { self.progress.active.store(false, Ordering::Relaxed); } }
    let _guard = ActiveGuard { progress };
    progress.active.store(true, Ordering::Relaxed);

    if existing_size > 0 && (expected_size == 0 || existing_size >= expected_size) {
        progress.verifying.store(true, Ordering::Relaxed);
        let dest_owned = dest.to_path_buf();
        let expected_md5 = pack.md5.clone();
        let cancel2 = cancel_flag.clone();
        let prog2 = progress.clone();
        
        let verify_result = tokio::task::spawn_blocking(move || {
            let mut verified_local: u64 = 0;
            let is_valid = crate::download::verify::verify_md5_with_progress(
                &dest_owned,
                &expected_md5,
                |n| {
                    if !cancel2.load(Ordering::SeqCst) {
                        return Err(AppError::Cancelled);
                    }
                    verified_local += n;
                    prog2.bytes.store(verified_local, Ordering::Relaxed);
                    Ok(())
                },
            )?;
            Ok::<(bool, u64), AppError>((is_valid, verified_local))
        }).await.map_err(|e| AppError::Api(format!("verify task panicked: {}", e)))??;

        let (is_valid, _verified_local) = verify_result;

        if is_valid {
            progress.bytes.store(expected_size, Ordering::Relaxed);
            progress.verifying.store(false, Ordering::Relaxed);
            app.emit("download://file-complete", crate::api::types::DownloadFileComplete { file_index, total_files, file_name }).ok();
            return Ok(());
        }

        std::fs::remove_file(dest).ok();
        progress.bytes.store(0, Ordering::Relaxed);
    } else if existing_size > 0 && existing_size < expected_size {
        progress.verifying.store(true, Ordering::Relaxed);
        let dest_owned = dest.to_path_buf();
        let cancel2 = cancel_flag.clone();
        let prog2 = progress.clone();

        hasher = tokio::task::spawn_blocking(move || -> Result<Md5, AppError> {
            use std::io::Read;
            let mut file = std::fs::File::open(&dest_owned)?;
            let mut hasher = Md5::new();
            let mut buf = vec![0u8; 4 * 1024 * 1024];
            let mut verified_local = 0;
            loop {
                if !cancel2.load(Ordering::SeqCst) {
                    return Err(AppError::Cancelled);
                }
                let n = file.read(&mut buf)?;
                if n == 0 { break; }
                hasher.update(&buf[..n]);
                verified_local += n as u64;
                prog2.bytes.store(verified_local, Ordering::Relaxed);
            }
            Ok(hasher)
        }).await.map_err(|e| AppError::Api(format!("resume hash task panicked: {}", e)))??;

        resume_from = existing_size;
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    progress.verifying.store(false, Ordering::Relaxed);

    let mut request = client.get(&pack.url);
    if resume_from > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={}-", resume_from));
    }
    let response = request.send().await?.error_for_status()?;

    let file = if resume_from > 0 && response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(dest)
            .await
            .map_err(AppError::Io)?
    } else {
        if resume_from > 0 {
            resume_from = 0;
            progress.bytes.store(0, Ordering::Relaxed);
            hasher = Md5::new();
        }
        tokio::fs::File::create(dest).await.map_err(AppError::Io)?
    };

    let mut stream = response.bytes_stream();
    let mut writer = BufWriter::with_capacity(4 * 1024 * 1024, file);

    let mut rate_bytes: u64 = 0;
    let mut rate_start = Instant::now();
    let mut current_bytes = resume_from;

    while let Some(chunk) = stream.next().await {
        if !cancel_flag.load(Ordering::SeqCst) {
            return Err(AppError::Cancelled);
        }

        let chunk = chunk.map_err(AppError::Http)?;
        writer.write_all(&chunk).await.map_err(AppError::Io)?;
        hasher.update(&chunk);

        current_bytes += chunk.len() as u64;
        progress.bytes.store(current_bytes, Ordering::Relaxed);
        
        let chunk_len = chunk.len() as u64;
        if speed_limit > 0 {
            rate_bytes += chunk_len;
            let expected = Duration::from_secs_f64(rate_bytes as f64 / speed_limit as f64);
            let elapsed = rate_start.elapsed();
            if expected > elapsed {
                tokio::time::sleep(expected - elapsed).await;
            }
            if rate_start.elapsed().as_secs() >= 2 {
                rate_bytes = 0;
                rate_start = Instant::now();
            }
        }
    }

    writer.flush().await.map_err(AppError::Io)?;

    let actual_md5 = format!("{:x}", hasher.finalize());
    if actual_md5 != pack.md5 {
        return Err(AppError::Md5Mismatch {
            expected: pack.md5.clone(),
            actual: actual_md5,
        });
    }

    progress.bytes.store(expected_size, Ordering::Relaxed);

    app.emit(
        "download://file-complete",
        crate::api::types::DownloadFileComplete {
            file_index,
            total_files,
            file_name,
        },
    )
    .ok();

    Ok(())
}
