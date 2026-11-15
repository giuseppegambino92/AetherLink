package dev.aetherlink.mobile

import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent

/**
 * Routes remote input from Rust (`aether_input::android`) to the best available backend:
 * Shizuku ([ShizukuInput], real `MotionEvent`/`KeyEvent` injection) when ready, otherwise
 * [AetherAccessibilityService] (gestures + global actions). Global actions always prefer the
 * accessibility service because `performGlobalAction` is the reliable way to open
 * Recents / Notifications.
 */
object InputRouter {
  private const val LINE_PX = 48f
  private val main = Handler(Looper.getMainLooper())

  @Volatile
  private var downTime = 0L

  fun ready(): Boolean = ShizukuInput.service() != null || AetherAccessibilityService.instance != null

  /** Which backend handles pointer/key events right now (`shizuku` · `accessibility` · `none`). */
  fun backend(): String = when {
    ShizukuInput.service() != null -> "shizuku"
    AetherAccessibilityService.instance != null -> "accessibility"
    else -> "none"
  }

  fun inject(kind: Int, x: Float, y: Float, a: Float, b: Float, code: Int, meta: Int, text: String?): Boolean {
    val sh = ShizukuInput.service()
    val acc = AetherAccessibilityService.instance
    if (sh == null && acc == null) return false
    return when (kind) {
      0 -> if (sh != null) touch(sh, MotionEvent.ACTION_DOWN, x, y, start = true) else acc!!.pointerDown(x, y)
      1 -> if (sh != null) touch(sh, MotionEvent.ACTION_MOVE, x, y) else acc!!.pointerMove(x, y)
      2 -> if (sh != null) touch(sh, MotionEvent.ACTION_UP, x, y) else acc!!.pointerUp(x, y)
      3 -> if (acc != null) acc.longPress(x, y) else {
        touch(sh!!, MotionEvent.ACTION_DOWN, x, y, start = true)
        main.postDelayed({ touch(sh, MotionEvent.ACTION_UP, x, y) }, 650)
        true
      }
      4 -> if (sh != null) {
        sh.injectMotion(MotionEvent.ACTION_SCROLL, x, y, 0, InputDevice.SOURCE_MOUSE, -b / LINE_PX, a / LINE_PX, 0)
      } else acc!!.scroll(x, y, a, b)
      5 -> acc?.pinch(x, y, a) ?: false
      6 -> if (sh != null && code == 0) {
        val action = when (meta) {
          0 -> MotionEvent.ACTION_DOWN
          1 -> MotionEvent.ACTION_MOVE
          2 -> MotionEvent.ACTION_UP
          else -> MotionEvent.ACTION_CANCEL
        }
        touch(sh, action, x, y, start = meta == 0)
      } else acc?.touch(code, x, y, meta) ?: false
      7 -> if (sh != null) {
        sh.injectKey(if (a > 0.5f) KeyEvent.ACTION_DOWN else KeyEvent.ACTION_UP, code, meta)
      } else acc!!.key(code, a > 0.5f)
      8 -> if (sh != null) sh.injectText(text ?: "") else acc!!.typeText(text ?: "")
      9 -> acc?.performGlobalAction(code) ?: globalViaKeys(sh!!, code)
      10 -> {
        if (sh != null && downTime != 0L) touch(sh, MotionEvent.ACTION_CANCEL, x, y)
        acc?.releaseAll()
        true
      }
      else -> false
    }
  }

  private fun touch(sh: IInputService, action: Int, x: Float, y: Float, start: Boolean = false): Boolean {
    if (start || downTime == 0L) downTime = SystemClock.uptimeMillis()
    val ok = sh.injectMotion(action, x, y, downTime, InputDevice.SOURCE_TOUCHSCREEN, 0f, 0f, 0)
    if (action == MotionEvent.ACTION_UP || action == MotionEvent.ACTION_CANCEL) downTime = 0L
    return ok
  }

  private fun globalViaKeys(sh: IInputService, action: Int): Boolean {
    val key = when (action) {
      1 -> KeyEvent.KEYCODE_BACK
      2 -> KeyEvent.KEYCODE_HOME
      3 -> KeyEvent.KEYCODE_APP_SWITCH
      4 -> KeyEvent.KEYCODE_NOTIFICATION
      8 -> KeyEvent.KEYCODE_SLEEP
      9 -> KeyEvent.KEYCODE_SYSRQ
      else -> return false
    }
    return sh.injectKey(KeyEvent.ACTION_DOWN, key, 0) && sh.injectKey(KeyEvent.ACTION_UP, key, 0)
  }
}
