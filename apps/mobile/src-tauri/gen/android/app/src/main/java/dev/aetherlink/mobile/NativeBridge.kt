package dev.aetherlink.mobile

import android.content.Context
import android.os.Build
import android.provider.Settings
import android.util.Log
import androidx.annotation.Keep
import java.nio.ByteBuffer

/**
 * JNI bridge to the Rust engine (`apps/mobile/src-tauri/src/android.rs`).
 *
 * Kotlin → Rust: the `native*` externals (encoded frames, PCM, display metrics).
 * Rust → Kotlin: the `@Keep` instance methods below, called on this object through a JNI
 * global reference taken in [nativeInit].
 */
@Keep
object NativeBridge {
  private const val TAG = "AetherLink"

  @Volatile
  var appContext: Context? = null
    private set

  @Volatile
  private var initialized = false

  init {
    // Already loaded by the Tauri activity; loading again is a no-op (and makes services safe).
    System.loadLibrary("aetherlink_mobile_lib")
  }

  @Synchronized
  fun init(context: Context) {
    appContext = context.applicationContext
    if (initialized) return
    initialized = true
    nativeInit()
    nativeSetDefaultDeviceName(deviceName(context))
    ShizukuInput.init(context.applicationContext)
  }

  private fun deviceName(context: Context): String {
    val user = try {
      Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME)
    } catch (_: Exception) {
      null
    }
    if (!user.isNullOrBlank()) return user
    val maker = Build.MANUFACTURER.replaceFirstChar { it.uppercase() }
    return if (Build.MODEL.startsWith(maker, ignoreCase = true)) Build.MODEL else "$maker ${Build.MODEL}"
  }

  // ------------------------------------------------------------ Kotlin → Rust
  external fun nativeInit()
  external fun nativeSetDisplay(width: Int, height: Int, scale: Float, refreshHz: Float)
  external fun nativeVideoConfig(hevc: Boolean, width: Int, height: Int, fps: Int)
  external fun nativeVideoCodecConfig(buf: ByteBuffer, offset: Int, len: Int)
  external fun nativeVideoFrame(buf: ByteBuffer, offset: Int, len: Int, ptsUs: Long, keyframe: Boolean)
  external fun nativeAudio(buf: ByteBuffer, len: Int, sampleRate: Int, channels: Int, isFloat: Boolean, ptsUs: Long)
  external fun nativeAudioWanted(): Boolean
  external fun nativeHostStopped()
  external fun nativeSetDefaultDeviceName(name: String)
  external fun nativeHidDescriptor(): ByteArray

  // ------------------------------------------------------------ Rust → Kotlin
  @Keep
  fun requestKeyframe() {
    ScreenShareService.instance?.requestKeyframe()
  }

  @Keep
  fun viewersChanged(viewers: Int) {
    Log.i(TAG, "viewers: $viewers")
    ScreenShareService.instance?.onViewersChanged(viewers)
  }

  @Keep
  fun inputReady(): Boolean = InputRouter.ready()

  /** See `encode()` in android.rs for the meaning of the arguments per `kind`. */
  @Keep
  fun inject(kind: Int, x: Float, y: Float, a: Float, b: Float, code: Int, meta: Int, text: String?): Boolean =
    try {
      InputRouter.inject(kind, x, y, a, b, code, meta, text)
    } catch (e: Exception) {
      Log.w(TAG, "inject failed", e)
      false
    }

  @Keep
  fun hidSendReport(id: Int, data: ByteArray): Boolean = HidController.sendReport(id, data)
}
