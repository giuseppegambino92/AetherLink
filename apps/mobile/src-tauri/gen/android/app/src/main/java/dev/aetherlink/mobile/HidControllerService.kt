package dev.aetherlink.mobile

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat

/** Keeps the Bluetooth HID controller alive while the app is in the background. */
class HidControllerService : Service() {
  companion object {
    private const val CHANNEL = "aetherlink-hid"
    private const val NOTIFICATION_ID = 0xAE2

    fun start(context: Context) {
      ContextCompat.startForegroundService(context, Intent(context, HidControllerService::class.java))
    }

    fun stop(context: Context) {
      context.stopService(Intent(context, HidControllerService::class.java))
    }
  }

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    val nm = getSystemService(NotificationManager::class.java)
    if (Build.VERSION.SDK_INT >= 26) {
      nm.createNotificationChannel(NotificationChannel(CHANNEL, getString(R.string.hid_channel), NotificationManager.IMPORTANCE_LOW))
    }
    val n: Notification = NotificationCompat.Builder(this, CHANNEL)
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle(getString(R.string.hid_notification_title))
      .setContentText(getString(R.string.hid_notification_text))
      .setOngoing(true)
      .build()
    try {
      if (Build.VERSION.SDK_INT >= 29) {
        startForeground(NOTIFICATION_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
      } else {
        startForeground(NOTIFICATION_ID, n)
      }
      HidController.start(this, NativeBridge.nativeHidDescriptor())
    } catch (e: Exception) {
      Log.w("AetherLink", "HID controller failed", e)
      stopSelf()
    }
    return START_NOT_STICKY
  }

  override fun onDestroy() {
    HidController.stop(this)
    super.onDestroy()
  }
}
