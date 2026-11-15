package dev.aetherlink.mobile

import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothHidDevice
import android.bluetooth.BluetoothHidDeviceAppSdpSettings
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.content.Context
import android.os.Build
import android.util.Log
import androidx.annotation.RequiresApi
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import java.util.concurrent.Executors

/**
 * Phone-as-controller: registers this phone as a Bluetooth **HID keyboard + mouse**
 * (`BluetoothHidDevice`, Android 9+) so it can drive an iPhone/iPad (with AssistiveTouch
 * for the pointer) or a Mac. The report descriptor and every report come from Rust
 * (`aether_input::hid_report::REPORT_DESCRIPTOR` / `HidReportBuilder`), so the wire format
 * is the one the desktop BLE/USB HID paths use.
 */
@SuppressLint("MissingPermission") // BLUETOOTH_CONNECT is requested by the UI before use
object HidController {
  private const val TAG = "AetherLink"
  private val executor = Executors.newSingleThreadExecutor()

  @Volatile
  private var hid: BluetoothHidDevice? = null

  @Volatile
  private var host: BluetoothDevice? = null

  @Volatile
  var registered = false
    private set

  private var pendingDescriptor: ByteArray? = null

  private fun adapter(ctx: Context): BluetoothAdapter? = ctx.getSystemService(BluetoothManager::class.java)?.adapter

  fun supported(ctx: Context): Boolean = Build.VERSION.SDK_INT >= 28 && adapter(ctx) != null

  @RequiresApi(28)
  private val callback = object : BluetoothHidDevice.Callback() {
    override fun onAppStatusChanged(pluggedDevice: BluetoothDevice?, registered: Boolean) {
      this@HidController.registered = registered
      Log.i(TAG, "HID app registered=$registered")
      if (registered && pluggedDevice != null) hid?.connect(pluggedDevice)
    }

    override fun onConnectionStateChanged(device: BluetoothDevice?, state: Int) {
      Log.i(TAG, "HID connection ${device?.address} state=$state")
      when (state) {
        BluetoothProfile.STATE_CONNECTED -> host = device
        BluetoothProfile.STATE_DISCONNECTED -> if (host?.address == device?.address) host = null
      }
    }
  }

  fun start(ctx: Context, descriptor: ByteArray) {
    if (Build.VERSION.SDK_INT < 28) throw IllegalStateException("errors.unsupported")
    val adapter = adapter(ctx) ?: throw IllegalStateException("errors.bluetoothUnavailable")
    if (!adapter.isEnabled) throw IllegalStateException("errors.bluetoothUnavailable")
    pendingDescriptor = descriptor
    hid?.let {
      register(ctx, it)
      return
    }
    adapter.getProfileProxy(ctx.applicationContext, object : BluetoothProfile.ServiceListener {
      override fun onServiceConnected(profile: Int, proxy: BluetoothProfile) {
        if (profile == BluetoothProfile.HID_DEVICE && Build.VERSION.SDK_INT >= 28) {
          val p = proxy as BluetoothHidDevice
          hid = p
          register(ctx, p)
        }
      }

      override fun onServiceDisconnected(profile: Int) {
        hid = null
        registered = false
        host = null
      }
    }, BluetoothProfile.HID_DEVICE)
  }

  @RequiresApi(28)
  private fun register(ctx: Context, p: BluetoothHidDevice) {
    val desc = pendingDescriptor ?: return
    if (registered) return
    val sdp = BluetoothHidDeviceAppSdpSettings(
      ctx.getString(R.string.hid_sdp_name),
      ctx.getString(R.string.hid_sdp_description),
      "AetherLink",
      BluetoothHidDevice.SUBCLASS1_COMBO,
      desc,
    )
    val ok = p.registerApp(sdp, null, null, executor, callback)
    Log.i(TAG, "HID registerApp -> $ok")
  }

  fun stop(ctx: Context) {
    if (Build.VERSION.SDK_INT < 28) return
    try {
      host?.let { hid?.disconnect(it) }
      hid?.unregisterApp()
    } catch (e: Exception) {
      Log.w(TAG, "HID stop", e)
    }
    adapter(ctx)?.closeProfileProxy(BluetoothProfile.HID_DEVICE, hid)
    hid = null
    host = null
    registered = false
  }

  fun connect(ctx: Context, address: String): Boolean {
    if (Build.VERSION.SDK_INT < 28) return false
    val device = adapter(ctx)?.getRemoteDevice(address) ?: return false
    return hid?.connect(device) ?: false
  }

  fun sendReport(id: Int, data: ByteArray): Boolean {
    if (Build.VERSION.SDK_INT < 28) return false
    val h = hid ?: return false
    val dev = host ?: return false
    return try {
      h.sendReport(dev, id, data)
    } catch (e: SecurityException) {
      false
    }
  }

  fun status(ctx: Context): JSObject {
    val o = JSObject()
    o.put("supported", supported(ctx))
    o.put("enabled", adapter(ctx)?.isEnabled == true)
    o.put("registered", registered)
    o.put("connected", host != null)
    host?.let {
      val h = JSObject()
      h.put("address", it.address)
      h.put("name", safeName(it))
      o.put("host", h)
    }
    val bonded = JSArray()
    try {
      adapter(ctx)?.bondedDevices?.forEach {
        val d = JSObject()
        d.put("address", it.address)
        d.put("name", safeName(it))
        bonded.put(d)
      }
    } catch (_: SecurityException) {
    }
    o.put("bonded", bonded)
    return o
  }

  private fun safeName(d: BluetoothDevice): String = try {
    d.name ?: d.address
  } catch (_: SecurityException) {
    d.address
  }
}
