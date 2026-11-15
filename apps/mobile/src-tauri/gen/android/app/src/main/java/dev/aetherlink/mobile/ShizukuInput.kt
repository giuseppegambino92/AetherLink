package dev.aetherlink.mobile

import android.content.ComponentName
import android.content.Context
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.os.IBinder
import android.util.Log
import rikka.shizuku.Shizuku

/**
 * Optional privileged input path through [Shizuku](https://shizuku.rikka.app/): when the
 * Shizuku service runs and the user granted AetherLink access, [InputUserService] is bound
 * and injects real input events. Otherwise [InputRouter] falls back to the accessibility
 * service.
 */
object ShizukuInput {
  private const val TAG = "AetherLink"
  private const val REQUEST_CODE = 0xAE7

  @Volatile
  private var service: IInputService? = null
  private var bound = false
  private lateinit var appContext: Context

  private val args by lazy {
    Shizuku.UserServiceArgs(ComponentName(appContext.packageName, InputUserService::class.java.name))
      .daemon(false)
      .processNameSuffix("input")
      .debuggable(BuildConfig.DEBUG)
      .version(1)
  }

  private val connection = object : ServiceConnection {
    override fun onServiceConnected(name: ComponentName?, binder: IBinder?) {
      if (binder != null && binder.pingBinder()) {
        service = IInputService.Stub.asInterface(binder)
        Log.i(TAG, "Shizuku input service connected")
      }
    }

    override fun onServiceDisconnected(name: ComponentName?) {
      service = null
    }
  }

  private val permissionListener = Shizuku.OnRequestPermissionResultListener { code, result ->
    if (code == REQUEST_CODE && result == PackageManager.PERMISSION_GRANTED) bind()
  }

  private val binderListener = Shizuku.OnBinderReceivedListener { if (granted()) bind() }

  private val binderDeadListener = Shizuku.OnBinderDeadListener {
    service = null
    bound = false
  }

  fun init(context: Context) {
    appContext = context.applicationContext
    try {
      Shizuku.addRequestPermissionResultListener(permissionListener)
      Shizuku.addBinderReceivedListenerSticky(binderListener)
      Shizuku.addBinderDeadListener(binderDeadListener)
    } catch (e: Throwable) {
      Log.w(TAG, "Shizuku unavailable", e)
    }
  }

  private fun running(): Boolean = try {
    Shizuku.pingBinder()
  } catch (_: Throwable) {
    false
  }

  private fun granted(): Boolean = try {
    running() && !Shizuku.isPreV11() && Shizuku.checkSelfPermission() == PackageManager.PERMISSION_GRANTED
  } catch (_: Throwable) {
    false
  }

  /** `unavailable` (Shizuku not running) · `noPermission` · `ready` (service bound). */
  fun state(): String = when {
    !running() -> "unavailable"
    !granted() -> "noPermission"
    service != null -> "ready"
    else -> "starting"
  }

  fun requestPermission(): String {
    if (!running()) return "unavailable"
    if (granted()) bind() else try {
      Shizuku.requestPermission(REQUEST_CODE)
    } catch (e: Throwable) {
      Log.w(TAG, "Shizuku permission request failed", e)
    }
    return state()
  }

  @Synchronized
  private fun bind() {
    if (bound) return
    try {
      Shizuku.bindUserService(args, connection)
      bound = true
    } catch (e: Throwable) {
      Log.w(TAG, "cannot bind Shizuku user service", e)
    }
  }

  fun service(): IInputService? = service
}
