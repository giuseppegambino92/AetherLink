package dev.aetherlink.mobile

import android.Manifest
import android.annotation.SuppressLint
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.util.Log
import android.view.Display
import android.view.WindowManager
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import java.nio.ByteBuffer
import kotlin.math.max
import kotlin.math.roundToInt

/**
 * "Share this device": MediaProjection → VirtualDisplay → MediaCodec low-latency surface
 * encoder (H.265, falling back to H.264) → Annex-B access units into Rust
 * ([NativeBridge.nativeVideoFrame]) → `aether_core::mobile::MobileHost`.
 * System audio (Android 10+) via `AudioPlaybackCapture` → [NativeBridge.nativeAudio].
 *
 * Runs as a `mediaProjection` foreground service (required since Android 10; on Android 14
 * the service must be in the foreground *before* `getMediaProjection`).
 */
class ScreenShareService : Service() {
  companion object {
    private const val TAG = "AetherLink"
    private const val CHANNEL = "aetherlink-share"
    private const val NOTIFICATION_ID = 0xAE1
    const val EXTRA_RESULT_CODE = "resultCode"
    const val EXTRA_DATA = "data"
    const val EXTRA_AUDIO = "audio"
    const val ACTION_STOP = "dev.aetherlink.mobile.STOP_SHARE"
    private const val MAX_LONG_SIDE = 1920
    private const val FPS = 60

    @Volatile
    var instance: ScreenShareService? = null
      private set

    fun start(context: Context, resultCode: Int, data: Intent, audio: Boolean) {
      val i = Intent(context, ScreenShareService::class.java)
        .putExtra(EXTRA_RESULT_CODE, resultCode)
        .putExtra(EXTRA_DATA, data)
        .putExtra(EXTRA_AUDIO, audio)
      ContextCompat.startForegroundService(context, i)
    }

    fun stop(context: Context) {
      context.stopService(Intent(context, ScreenShareService::class.java))
    }
  }

  private var projection: MediaProjection? = null
  private var display: VirtualDisplay? = null
  private var codec: MediaCodec? = null
  private var drain: Thread? = null
  private var audioThread: Thread? = null
  private var audioRecord: AudioRecord? = null

  @Volatile
  private var running = false

  @Volatile
  var codecName: String? = null
    private set

  @Volatile
  var audioActive = false
    private set

  @Volatile
  var viewers = 0
    private set

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    if (intent?.action == ACTION_STOP) {
      stopSelf()
      return START_NOT_STICKY
    }
    startInForeground()
    val code = intent?.getIntExtra(EXTRA_RESULT_CODE, 0) ?: 0
    @Suppress("DEPRECATION")
    val data: Intent? = if (Build.VERSION.SDK_INT >= 33) intent?.getParcelableExtra(EXTRA_DATA, Intent::class.java) else intent?.getParcelableExtra(EXTRA_DATA)
    if (data == null || running) return START_NOT_STICKY
    instance = this
    NativeBridge.init(this)
    try {
      startProjection(code, data, intent?.getBooleanExtra(EXTRA_AUDIO, true) ?: true)
    } catch (e: Exception) {
      Log.e(TAG, "screen share failed", e)
      stopSelf()
    }
    return START_NOT_STICKY
  }

  private fun startInForeground() {
    val nm = getSystemService(NotificationManager::class.java)
    if (Build.VERSION.SDK_INT >= 26) {
      nm.createNotificationChannel(NotificationChannel(CHANNEL, getString(R.string.share_channel), NotificationManager.IMPORTANCE_LOW))
    }
    val stop = PendingIntent.getService(
      this, 1, Intent(this, ScreenShareService::class.java).setAction(ACTION_STOP),
      PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )
    val open = packageManager.getLaunchIntentForPackage(packageName)?.let {
      PendingIntent.getActivity(this, 2, it, PendingIntent.FLAG_IMMUTABLE)
    }
    val n: Notification = NotificationCompat.Builder(this, CHANNEL)
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle(getString(R.string.share_notification_title))
      .setContentText(getString(R.string.share_notification_text))
      .setOngoing(true)
      .setContentIntent(open)
      .addAction(0, getString(R.string.share_stop), stop)
      .build()
    if (Build.VERSION.SDK_INT >= 29) {
      startForeground(NOTIFICATION_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
    } else {
      startForeground(NOTIFICATION_ID, n)
    }
  }

  private data class Size(val w: Int, val h: Int, val dpi: Int, val refresh: Float, val density: Float)

  private fun realSize(): Size {
    val wm = getSystemService(WindowManager::class.java)
    val dm = resources.displayMetrics
    val (w, h) = if (Build.VERSION.SDK_INT >= 30) {
      val b = wm.maximumWindowMetrics.bounds
      b.width() to b.height()
    } else {
      @Suppress("DEPRECATION")
      val d = android.util.DisplayMetrics().also { wm.defaultDisplay.getRealMetrics(it) }
      d.widthPixels to d.heightPixels
    }
    val refresh = getSystemService(DisplayManager::class.java).getDisplay(Display.DEFAULT_DISPLAY)?.refreshRate ?: 60f
    return Size(w, h, dm.densityDpi, refresh, dm.density)
  }

  private fun startProjection(resultCode: Int, data: Intent, wantAudio: Boolean) {
    val mpm = getSystemService(MediaProjectionManager::class.java)
    val proj = mpm.getMediaProjection(resultCode, data) ?: throw IllegalStateException("no projection")
    projection = proj
    proj.registerCallback(object : MediaProjection.Callback() {
      override fun onStop() {
        stopSelf()
      }
    }, null)

    val real = realSize()
    NativeBridge.nativeSetDisplay(real.w, real.h, real.density, real.refresh)
    // Encode at most 1080p-class resolution (long side ≤ 1920), even dimensions.
    val k = minOf(1.0, MAX_LONG_SIDE.toDouble() / max(real.w, real.h))
    val w = ((real.w * k).roundToInt() / 16) * 16
    val h = ((real.h * k).roundToInt() / 16) * 16

    val (enc, hevc) = createEncoder(w, h)
    codec = enc
    codecName = if (hevc) "hevc" else "h264"
    NativeBridge.nativeVideoConfig(hevc, w, h, FPS)
    val surface = enc.createInputSurface()
    enc.start()
    running = true
    display = proj.createVirtualDisplay(
      "AetherLink", w, h, real.dpi, DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR, surface, null, null,
    )
    drain = Thread({ drainLoop(enc) }, "aether-encoder").also { it.start() }
    if (wantAudio) startAudio(proj)
    Log.i(TAG, "sharing ${real.w}x${real.h} as ${w}x$h $codecName")
  }

  /**
   * HEVC when an encoder exists for the size, else H.264. Each codec is tried first with the
   * low-latency format, then low-latency without `KEY_PREPEND_HEADER_TO_SYNC_FRAMES` (Google's
   * `c2.android.avc.encoder` rejects it with BAD_VALUE; harmless since `MobileHost` prepends
   * the cached SPS/PPS to keyframes anyway), then a conservative one (some vendor encoders
   * also reject `KEY_LATENCY`, `KEY_PRIORITY` or VBR).
   */
  private fun createEncoder(w: Int, h: Int): Pair<MediaCodec, Boolean> {
    for (mime in listOf(MediaFormat.MIMETYPE_VIDEO_HEVC, MediaFormat.MIMETYPE_VIDEO_AVC)) {
      val name = findEncoder(mime, w, h) ?: continue
      for (attempt in 0..2) {
        val lowLatency = attempt < 2
        val fmt = MediaFormat.createVideoFormat(mime, w, h).apply {
          setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
          setInteger(MediaFormat.KEY_BIT_RATE, (w.toLong() * h * FPS / 10).coerceIn(2_000_000, 16_000_000).toInt())
          setInteger(MediaFormat.KEY_FRAME_RATE, FPS)
          setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2)
          // Keep a static screen alive (and recoverable) for new viewers.
          setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, 100_000)
          if (lowLatency) {
            setInteger(MediaFormat.KEY_PRIORITY, 0) // realtime
            if (attempt == 0 && Build.VERSION.SDK_INT >= 29) setInteger(MediaFormat.KEY_PREPEND_HEADER_TO_SYNC_FRAMES, 1)
            if (Build.VERSION.SDK_INT >= 30) {
              setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
              setInteger(MediaFormat.KEY_LATENCY, 1)
            }
            setInteger(MediaFormat.KEY_BITRATE_MODE, MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_VBR)
          }
        }
        var c: MediaCodec? = null
        try {
          c = MediaCodec.createByCodecName(name)
          c.configure(fmt, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
          Log.i(TAG, "encoder $name $mime ${w}x$h lowLatency=$lowLatency prependHeaders=${attempt == 0}")
          return c to (mime == MediaFormat.MIMETYPE_VIDEO_HEVC)
        } catch (e: Exception) {
          c?.release()
          Log.w(TAG, "encoder $name rejected $mime ${w}x$h (attempt $attempt): $e")
        }
      }
    }
    throw IllegalStateException("no H.265/H.264 encoder")
  }

  private fun findEncoder(mime: String, w: Int, h: Int): String? {
    val infos = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
    val candidates = infos.filter { it.isEncoder && it.supportedTypes.any { t -> t.equals(mime, true) } }
    val hw = candidates.sortedBy { if (Build.VERSION.SDK_INT >= 29 && it.isHardwareAccelerated) 0 else 1 }
    return hw.firstOrNull {
      try {
        it.getCapabilitiesForType(mime).videoCapabilities?.isSizeSupported(w, h) == true
      } catch (_: Exception) {
        false
      }
    }?.name
  }

  private fun drainLoop(c: MediaCodec) {
    val info = MediaCodec.BufferInfo()
    while (running) {
      val idx = try {
        c.dequeueOutputBuffer(info, 20_000)
      } catch (_: IllegalStateException) {
        break
      }
      if (idx < 0) continue
      val buf: ByteBuffer? = c.getOutputBuffer(idx)
      if (buf != null && info.size > 0) {
        if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
          NativeBridge.nativeVideoCodecConfig(buf, info.offset, info.size)
        } else {
          val key = info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0
          NativeBridge.nativeVideoFrame(buf, info.offset, info.size, info.presentationTimeUs, key)
        }
      }
      try {
        c.releaseOutputBuffer(idx, false)
      } catch (_: IllegalStateException) {
        break
      }
      if (info.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) break
    }
  }

  fun requestKeyframe() {
    try {
      codec?.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) })
    } catch (_: Exception) {
    }
  }

  fun onViewersChanged(n: Int) {
    viewers = n
    if (n > 0) requestKeyframe()
  }

  @SuppressLint("MissingPermission")
  private fun startAudio(proj: MediaProjection) {
    if (Build.VERSION.SDK_INT < 29) return
    if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
      Log.i(TAG, "RECORD_AUDIO not granted: sharing without audio")
      return
    }
    val rate = 48_000
    val cfg = AudioPlaybackCaptureConfiguration.Builder(proj)
      .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
      .addMatchingUsage(AudioAttributes.USAGE_GAME)
      .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
      .build()
    val format = AudioFormat.Builder()
      .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
      .setSampleRate(rate)
      .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
      .build()
    val minBuf = AudioRecord.getMinBufferSize(rate, AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_16BIT)
    val rec = try {
      AudioRecord.Builder().setAudioFormat(format).setBufferSizeInBytes(max(minBuf, 3840 * 4)).setAudioPlaybackCaptureConfig(cfg).build()
    } catch (e: Exception) {
      Log.w(TAG, "AudioPlaybackCapture unavailable", e)
      return
    }
    audioRecord = rec
    rec.startRecording()
    audioActive = true
    audioThread = Thread({
      // 20 ms of 48 kHz stereo s16.
      val buf = ByteBuffer.allocateDirect(3840)
      val t0 = System.nanoTime()
      while (running) {
        buf.clear()
        val n = rec.read(buf, buf.capacity(), AudioRecord.READ_BLOCKING)
        if (n <= 0) continue
        if (NativeBridge.nativeAudioWanted()) {
          NativeBridge.nativeAudio(buf, n, rate, 2, false, (System.nanoTime() - t0) / 1000)
        }
      }
    }, "aether-audio").also { it.start() }
  }

  override fun onDestroy() {
    running = false
    instance = null
    try {
      drain?.join(500)
      audioThread?.join(500)
    } catch (_: InterruptedException) {
    }
    try {
      audioRecord?.stop()
    } catch (_: Exception) {
    }
    audioRecord?.release()
    audioActive = false
    display?.release()
    try {
      codec?.stop()
    } catch (_: Exception) {
    }
    codec?.release()
    projection?.stop()
    NativeBridge.nativeHostStopped()
    super.onDestroy()
  }
}
