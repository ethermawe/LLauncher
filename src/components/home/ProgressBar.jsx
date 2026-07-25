import { formatSize, formatSpeed, formatEta } from '../../utils/format';
import { useTranslation } from '../../i18n';
import './ProgressBar.css';

export default function ProgressBar({ progress, paused, resuming, onPause, onResume, onCancel }) {
  const { t } = useTranslation();
  if (!progress) return null;

  const extracting = progress.stage === 'extracting';
  const verifying = progress.stage === 'verifying';

  const percent = extracting
    ? progress.percent ?? 0
    : progress.bytes_total > 0
      ? Math.round((progress.bytes_downloaded / progress.bytes_total) * 100)
      : 0;

  const done = extracting ? progress.bytes_processed : progress.bytes_downloaded;
  const total = progress.bytes_total;
  const speed = paused ? 0 : progress.speed_bps;
  const eta = formatEta(total - done, speed);

  const stageClass = extracting
    ? 'progress-bar--extracting'
    : verifying
      ? 'progress-bar--verifying'
      : paused
        ? 'progress-bar--paused'
        : '';

  return (
    <div className={`progress-bar ${stageClass}`}>
      <div className="progress-bar__track">
        <div
          className="progress-bar__fill"
          style={{ width: `${percent}%` }}
        />
      </div>
      <div className="progress-bar__info">
        <span>
          {formatSize(done)} / {formatSize(total)}
          {' — '}{percent}%
        </span>
        <span>
          {paused
            ? t('progress.pausedLabel')
            : resuming && verifying
              ? t('progress.resumeVerify')
              : formatSpeed(speed)}
          {!paused && !(resuming && verifying) && eta ? ` — ${eta}` : ''}
        </span>
      </div>
      <div className="progress-bar__file">
        {extracting ? (
          <span className="progress-bar__stage">{t('progress.extractLabel')}</span>
        ) : (
          <>
            <span className="progress-bar__file-name">
              {verifying
                ? resuming
                  ? t('progress.resumeVerifyFile', {
                      current: progress.file_index + 1,
                      total: progress.total_files,
                    })
                  : t('progress.verifyLabel', {
                      current: progress.file_index + 1,
                      total: progress.total_files,
                      name: progress.file_name,
                    })
                : paused
                  ? t('progress.pausedFile', {
                      current: progress.file_index + 1,
                      total: progress.total_files,
                    })
                  : t('progress.fileLabel', {
                      current: progress.file_index + 1,
                      total: progress.total_files,
                      name: progress.file_name,
                    })}
            </span>
            <div className="progress-bar__actions">
              {paused ? (
                onResume && (
                  <button className="progress-bar__resume" onClick={onResume}>
                    {t('progress.resume')}
                  </button>
                )
              ) : (
                onPause && (
                  <button className="progress-bar__pause" onClick={onPause}>
                    {t('progress.pause')}
                  </button>
                )
              )}
              {onCancel && (
                <button className="progress-bar__cancel" onClick={onCancel}>
                  {t('common.cancel')}
                </button>
              )}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
