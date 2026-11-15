package dev.aetherlink.mobile

import android.Manifest
import android.app.Activity
import android.bluetooth.BluetoothAdapter
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.media.projection.MediaProjectionManager
import android.net.wifi.WifiManager
import android.os.Build
import android.provider.Settings
import android.text.TextUtils
import android.webkit.WebView
import androidx.activity.result.ActivityResult
import androidx.appcompat.app.AppCompatActivity
import app.tauri.PermissionState
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.Permission
import app.tauri.annotation.PermissionCallback
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@InvokeArg
class StartShareArgs {
  var audio: Boolean = true
}

@InvokeArg
class PermissionKindsArgs {
  var kinds: Array<String> = arrayOf()
}

@InvokeArg
class HidConnectArgs {
  var address: String = ""
}

/**
 * Platform plugin behind the Rust `mobile_*` commands (`apps/mobile/src-tauri/src/mobile.rs`).
 * Rejections carry stable i18n keys (`errors.*`) that the UI localizes.
 */
@TauriPlugin(
  permissions = [
    Permission(strings = [Manifest.permission.POST_NOTIFICATIONS], alias = "notifications"),
    Permission(strings = [Manifest.permission.RECORD_AUDIO], alias = "audio"),
    Permission(strings = [Manifest.permission.BLUETOOTH_CONNECT, Manifest.permission.BLUETOOTH_ADVERTISE], alias = "bluetooth"),
  ],
)
class AetherPlugin(private val activity: Activity) : Plugin(activity) {
  private var multicastLock: WifiManager.MulticastLock? = null
  private var pendingAudio = true

  override fun load(webView: WebView) {
    NativeBridge.init(activity)
    // mDNS (LAN discovery) needs a multicast lock on Android.
    try {
      val wifi = activity.applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
      multicastLock = wifi.createMulticastLock("aetherlink-mdns").apply {
        setReferenceCounted(false)
        acquire()
      }
    } catch (_: Exception) {
    }
  }

  override fun onDestroy(activity: AppCompatActivity) {
    multicastLock?.release()
    super.onDestroy(activity)
  }

  // ------------------------------------------------------------ screen sharing

  @Command
  fun startScreenShare(invoke: Invoke) {
    if (ScreenShareService.instance != null) {
      invoke.resolve(JSObject().put("started", true))
      return
    }
    pendingAudio = invoke.parseArgs(StartShareArgs::class.java).audio
    val mpm = activity.getSystemService(MediaProjectionManager::class.java)
    startActivityForResult(invoke, mpm.createScreenCaptureIntent(), "onProjectionResult")
  }

  @ActivityCallback
  fun onProjectionResult(invoke: Invoke, result: ActivityResult) {
    val data = result.data
    if (result.resultCode != Activity.RESULT_OK || data == null) {
      invoke.reject("errors.projectionDenied")
      return
    }
    ScreenShareService.start(activity, result.resultCode, data, pendingAudio)
    invoke.resolve(JSObject().put("started", true))
  }

  @Command
  fun stopScreenShare(invoke: Invoke) {
    ScreenShareService.stop(activity)
    invoke.resolve(JSObject().put("stopped", true))
  }

  @Command
  fun hostStatus(invoke: Invoke) {
    val s = ScreenShareService.instance
    val o = JSObject()
    o.put("serviceRunning", s != null)
    o.put("encoder", s?.codecName)
    o.put("audio", s?.audioActive == true)
    o.put("viewers", s?.viewers ?: 0)
    invoke.resolve(o)
  }

  // ------------------------------------------------------------ remote control of this phone

  private fun accessibilityEnabled(): Boolean {
    val expected = ComponentName(activity, AetherAccessibilityService::class.java).flattenToString()
    val enabled = Settings.Secure.getString(activity.contentResolver, Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES) ?: return false
    val splitter = TextUtils.SimpleStringSplitter(':')
    splitter.setString(enabled)
    return splitter.any { it.equals(expected, ignoreCase = true) }
  }

  @Command
  fun inputStatus(invoke: Invoke) {
    val o = JSObject()
    o.put("accessibility", if (accessibilityEnabled()) "enabled" else "disabled")
    o.put("accessibilityConnected", AetherAccessibilityService.instance != null)
    o.put("shizuku", ShizukuInput.state())
    o.put("backend", InputRouter.backend())
    invoke.resolve(o)
  }

  @Command
  fun openAccessibilitySettings(invoke: Invoke) {
    val i = Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
    activity.startActivity(i)
    invoke.resolve(JSObject().put("opened", true))
  }

  @Command
  fun requestShizuku(invoke: Invoke) {
    invoke.resolve(JSObject().put("shizuku", ShizukuInput.requestPermission()))
  }

  // ------------------------------------------------------------ runtime permissions

  private fun aliasNeeded(alias: String): Boolean = when (alias) {
    "notifications" -> Build.VERSION.SDK_INT >= 33
    "bluetooth" -> Build.VERSION.SDK_INT >= 31
    "audio" -> true
    else -> false
  }

  @Command
  fun requestRuntimePermissions(invoke: Invoke) {
    val kinds = invoke.parseArgs(PermissionKindsArgs::class.java).kinds
    val aliases = kinds.filter { aliasNeeded(it) && getPermissionState(it) != PermissionState.GRANTED }
    if (aliases.isEmpty()) {
      permissionsResult(invoke)
    } else {
      requestPermissionForAliases(aliases.toTypedArray(), invoke, "permissionsResult")
    }
  }

  @PermissionCallback
  fun permissionsResult(invoke: Invoke) {
    val o = JSObject()
    for (alias in listOf("notifications", "audio", "bluetooth")) {
      val granted = !aliasNeeded(alias) || getPermissionState(alias) == PermissionState.GRANTED
      o.put(alias, if (granted) "granted" else "denied")
    }
    invoke.resolve(o)
  }

  // ------------------------------------------------------------ Bluetooth HID controller

  @Command
  fun hidStatus(invoke: Invoke) {
    invoke.resolve(HidController.status(activity))
  }

  @Command
  fun hidStart(invoke: Invoke) {
    if (!HidController.supported(activity)) {
      invoke.reject("errors.unsupported")
      return
    }
    if (Build.VERSION.SDK_INT >= 31 && getPermissionState("bluetooth") != PermissionState.GRANTED) {
      invoke.reject("errors.bluetoothPermission")
      return
    }
    if (activity.getSystemService(android.bluetooth.BluetoothManager::class.java)?.adapter?.isEnabled != true) {
      invoke.reject("errors.bluetoothUnavailable")
      return
    }
    HidControllerService.start(activity)
    // Let the iPhone/Mac find this phone for pairing.
    try {
      activity.startActivity(
        Intent(BluetoothAdapter.ACTION_REQUEST_DISCOVERABLE).putExtra(BluetoothAdapter.EXTRA_DISCOVERABLE_DURATION, 120),
      )
    } catch (_: Exception) {
    }
    invoke.resolve(HidController.status(activity))
  }

  @Command
  fun hidStop(invoke: Invoke) {
    HidControllerService.stop(activity)
    invoke.resolve(HidController.status(activity))
  }

  @Command
  fun hidConnect(invoke: Invoke) {
    val address = invoke.parseArgs(HidConnectArgs::class.java).address
    if (!HidController.connect(activity, address)) {
      invoke.reject("errors.hidNotConnected")
      return
    }
    invoke.resolve(HidController.status(activity))
  }
}
