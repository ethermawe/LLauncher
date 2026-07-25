import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

export default function useDownload(onComplete) {
  const [downloading, setDownloading] = useState(false);
  const [paused, setPaused] = useState(false);
  const [resuming, setResuming] = useState(false);
  const [progress, setProgress] = useState(null);
  const [error, setError] = useState(null);
  // Whether the current stop should discard partial files (Cancel) or keep
  // them for a later resume (Pause).
  const discardRef = useRef(false);
  // Track whether the active session is an update (vs fresh install).
  const isUpdateRef = useRef(false);
  // Preserve last progress snapshot so the paused state shows where it stopped.
  const lastProgressRef = useRef(null);

  // On mount, check if a download was paused in a previous session (e.g. the
  // webview reloaded while paused).
  useEffect(() => {
    invoke('is_download_paused').then((wasPaused) => {
      if (wasPaused) setPaused(true);
    }).catch(() => {});
  }, []);

  useEffect(() => {
    const unlisteners = [];

    listen('download://resuming', (event) => {
      setResuming(true);
    }).then((u) => unlisteners.push(u));

    listen('download://progress', (event) => {
      setResuming(false);
      const p = { stage: 'downloading', ...event.payload };
      setProgress(p);
      lastProgressRef.current = p;
    }).then((u) => unlisteners.push(u));

    listen('download://file-complete', (event) => {
      setProgress((prev) => {
        const p = prev ? { ...prev, ...event.payload } : { stage: 'downloading', ...event.payload };
        lastProgressRef.current = p;
        return p;
      });
    }).then((u) => unlisteners.push(u));

    listen('download://verify-progress', (event) => {
      const p = { stage: 'verifying', ...event.payload };
      setProgress(p);
      lastProgressRef.current = p;
    }).then((u) => unlisteners.push(u));

    listen('download://extract-progress', (event) => {
      setProgress({ stage: 'extracting', ...event.payload });
    }).then((u) => unlisteners.push(u));

    listen('download://complete', (event) => {
      setResuming(false);
      setDownloading(false);
      setPaused(false);
      setProgress(null);
      lastProgressRef.current = null;
      if (onComplete) onComplete(event.payload.version);
    }).then((u) => unlisteners.push(u));

    listen('download://error', (event) => {
      setDownloading(false);
      // Cancellation/pause is user-initiated: partial files are kept and
      // the next start resumes via HTTP Range, so it is not an error.
      if (/cancelled/i.test(event.payload.message)) {
        // Don't clear progress — keep it for the paused state display.
        // paused flag is already set by pauseDownload().
      } else {
        setResuming(false);
        setError(event.payload.message);
        setPaused(false);
        lastProgressRef.current = null;
      }
    }).then((u) => unlisteners.push(u));

    // The smart-update delta path reports on its own channel; normalise it into
    // the same progress shape so the existing progress bar works.
    listen('update://progress', (event) => {
      const p = event.payload;
      const normalized = {
        stage: p.stage === 'downloading' ? 'downloading' : 'verifying',
        file_index: p.files_done,
        total_files: p.total_files,
        file_name: '',
        bytes_downloaded: p.bytes_done,
        bytes_total: p.bytes_total,
        speed_bps: p.speed_bps,
      };
      setProgress(normalized);
      lastProgressRef.current = normalized;
    }).then((u) => unlisteners.push(u));

    listen('update://complete', (event) => {
      setResuming(false);
      setDownloading(false);
      setPaused(false);
      setProgress(null);
      lastProgressRef.current = null;
      if (onComplete) onComplete(event.payload.version);
    }).then((u) => unlisteners.push(u));

    listen('update://error', (event) => {
      setDownloading(false);
      if (/cancelled/i.test(event.payload.message)) {
        // Keep progress for paused state.
      } else {
        setResuming(false);
        setError(event.payload.message);
        setPaused(false);
        lastProgressRef.current = null;
      }
    }).then((u) => unlisteners.push(u));

    return () => {
      unlisteners.forEach((u) => u());
    };
  }, [onComplete]);

  const startDownload = useCallback(async () => {
    setDownloading(true);
    setPaused(false);
    setResuming(false);
    setError(null);
    setProgress(null);
    discardRef.current = false;
    isUpdateRef.current = false;
    try {
      await invoke('start_download');
    } catch (e) {
      setDownloading(false);
      const message = typeof e === 'string' ? e : e.message || 'Download failed';
      if (/cancelled/i.test(message)) {
        // Pause or cancel — paused flag already set if it was a pause.
        if (discardRef.current) {
          setPaused(false);
          setProgress(null);
          lastProgressRef.current = null;
          await invoke('clear_download_cache').catch(() => {});
          discardRef.current = false;
        }
      } else {
        setError(message);
      }
    }
  }, []);

  // Update an existing install to the latest version. The backend chooses the
  // cheaper safe path (per-file VFS delta vs full packs); progress arrives on
  // either the update:// (delta) or download:// (packs) channel.
  const startUpdate = useCallback(async () => {
    setDownloading(true);
    setPaused(false);
    setResuming(false);
    setError(null);
    setProgress(null);
    discardRef.current = false;
    isUpdateRef.current = true;
    try {
      await invoke('start_update');
    } catch (e) {
      setDownloading(false);
      const message = typeof e === 'string' ? e : e.message || 'Update failed';
      if (/cancelled/i.test(message)) {
        if (discardRef.current) {
          setPaused(false);
          setProgress(null);
          lastProgressRef.current = null;
          await invoke('clear_download_cache').catch(() => {});
          discardRef.current = false;
        }
      } else {
        setError(message);
      }
    }
  }, []);

  // Pause: stop workers but keep partial files; clicking Resume continues.
  const pauseDownload = useCallback(async () => {
    discardRef.current = false;
    try {
      await invoke('pause_download');
      setPaused(true);
    } catch (e) {
      console.error('Failed to pause download:', e);
    }
  }, []);

  // Resume a paused download.
  const resumeDownload = useCallback(async () => {
    setDownloading(true);
    setPaused(false);
    setError(null);
    discardRef.current = false;
    try {
      if (isUpdateRef.current) {
        await invoke('start_update');
      } else {
        await invoke('start_download');
      }
    } catch (e) {
      setDownloading(false);
      const message = typeof e === 'string' ? e : e.message || 'Resume failed';
      if (/cancelled/i.test(message)) {
        if (discardRef.current) {
          setPaused(false);
          setProgress(null);
          lastProgressRef.current = null;
          await invoke('clear_download_cache').catch(() => {});
          discardRef.current = false;
        } else {
          setPaused(true);
        }
      } else {
        setError(message);
      }
    }
  }, []);

  // Cancel: stop and discard the partial download cache.
  const cancelDownload = useCallback(async () => {
    discardRef.current = true;
    try {
      await invoke('cancel_download');
      setPaused(false);
      setProgress(null);
      lastProgressRef.current = null;
    } catch (e) {
      console.error('Failed to cancel download:', e);
    }
  }, []);

  return { downloading, paused, resuming, progress, error, startDownload, startUpdate, pauseDownload, resumeDownload, cancelDownload };
}
