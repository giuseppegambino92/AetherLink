package dev.aetherlink.mobile

import android.os.SystemClock
import android.view.InputDevice
import android.view.InputEvent
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.MotionEvent
import androidx.annotation.Keep
import java.lang.reflect.Method
import kotlin.system.exitProcess

/**
 * Shizuku UserService: started by Shizuku in its own process with the **shell uid**, which
 * holds `INJECT_EVENTS`. Injects real `MotionEvent`/`KeyEvent`s through the hidden
 * `InputManager(Global).injectInputEvent(event, mode)`.
 */
@Keep
class InputUserService : IInputService.Stub() {
  private val inject: Pair<Any, Method>? = resolveInjector()

  override fun destroy() {
    exitProcess(0)
  }

  override fun injectMotion(
    action: Int,
    x: Float,
    y: Float,
    downTime: Long,
    source: Int,
    vscroll: Float,
    hscroll: Float,
    buttonState: Int,
  ): Boolean {
    val now = SystemClock.uptimeMillis()
    val props = MotionEvent.PointerProperties().apply {
      id = 0
      toolType = if (source == InputDevice.SOURCE_MOUSE) MotionEvent.TOOL_TYPE_MOUSE else MotionEvent.TOOL_TYPE_FINGER
    }
    val coords = MotionEvent.PointerCoords().apply {
      this.x = x
      this.y = y
      pressure = 1f
      size = 1f
      setAxisValue(MotionEvent.AXIS_VSCROLL, vscroll)
      setAxisValue(MotionEvent.AXIS_HSCROLL, hscroll)
    }
    val ev = MotionEvent.obtain(
      if (downTime > 0) downTime else now, now, action, 1, arrayOf(props), arrayOf(coords),
      0, buttonState, 1f, 1f, 0, 0, source, 0,
    )
    return try {
      send(ev)
    } finally {
      ev.recycle()
    }
  }

  override fun injectKey(action: Int, keyCode: Int, metaState: Int): Boolean {
    val now = SystemClock.uptimeMillis()
    val ev = KeyEvent(now, now, action, keyCode, 0, metaState, KeyCharacterMap.VIRTUAL_KEYBOARD, 0, 0, InputDevice.SOURCE_KEYBOARD)
    return send(ev)
  }

  override fun injectText(text: String): Boolean {
    val events = KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD).getEvents(text.toCharArray()) ?: return false
    var ok = true
    for (e in events) ok = send(e) && ok
    return ok
  }

  private fun send(ev: InputEvent): Boolean {
    val (target, method) = inject ?: return false
    return try {
      method.invoke(target, ev, 0 /* INJECT_INPUT_EVENT_MODE_ASYNC */) as? Boolean ?: true
    } catch (_: Exception) {
      false
    }
  }

  companion object {
    /** `InputManagerGlobal` (Android 14+) or `InputManager` (older), both hidden APIs. */
    private fun resolveInjector(): Pair<Any, Method>? {
      for (cls in listOf("android.hardware.input.InputManagerGlobal", "android.hardware.input.InputManager")) {
        try {
          val c = Class.forName(cls)
          val instance = c.getDeclaredMethod("getInstance").invoke(null) ?: continue
          val m = c.getDeclaredMethod("injectInputEvent", InputEvent::class.java, Int::class.javaPrimitiveType)
          m.isAccessible = true
          return instance to m
        } catch (_: Throwable) {
        }
      }
      return null
    }
  }
}
