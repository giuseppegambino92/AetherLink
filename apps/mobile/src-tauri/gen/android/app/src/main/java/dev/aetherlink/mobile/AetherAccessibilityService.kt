package dev.aetherlink.mobile

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.content.Intent
import android.graphics.Path
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import android.view.KeyEvent
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import kotlin.math.hypot
import kotlin.math.max
import kotlin.math.min

/**
 * Remote-control backend without root/Shizuku: replays the remote pointer as
 * `dispatchGesture` strokes and maps system actions to `performGlobalAction`.
 *
 * * tap / drag / swipe: the stroke is recorded between pointer-down and pointer-up and
 *   dispatched with its real timing (taps ≤ 50 ms, long presses keep their duration);
 * * scroll: coalesced wheel deltas become a short swipe;
 * * pinch: two symmetric strokes around the cursor;
 * * multi-touch: contacts are collected until all lift, then dispatched as one gesture;
 * * text: `ACTION_SET_TEXT` on the focused editable node.
 */
class AetherAccessibilityService : AccessibilityService() {
  companion object {
    private const val TAG = "AetherLink"

    @Volatile
    var instance: AetherAccessibilityService? = null
      private set
  }

  private data class Pt(val t: Long, val x: Float, val y: Float)

  private val main = Handler(Looper.getMainLooper())
  private val stroke = ArrayList<Pt>()
  private val contacts = HashMap<Int, ArrayList<Pt>>()
  private val liftedContacts = HashSet<Int>()
  private var scrollAcc = floatArrayOf(0f, 0f, 0f, 0f) // x, y, dx, dy
  private var scrollPending = false

  override fun onServiceConnected() {
    super.onServiceConnected()
    instance = this
  }

  override fun onUnbind(intent: Intent?): Boolean {
    instance = null
    return super.onUnbind(intent)
  }

  override fun onDestroy() {
    instance = null
    super.onDestroy()
  }

  override fun onAccessibilityEvent(event: AccessibilityEvent?) {}

  override fun onInterrupt() {}

  private fun now() = SystemClock.uptimeMillis()

  private fun screen(): Pair<Float, Float> {
    val m = resources.displayMetrics
    return m.widthPixels.toFloat() to m.heightPixels.toFloat()
  }

  private fun clampX(x: Float) = x.coerceIn(0f, screen().first - 1)
  private fun clampY(y: Float) = y.coerceIn(0f, screen().second - 1)

  // ------------------------------------------------------------ primary pointer

  @Synchronized
  fun pointerDown(x: Float, y: Float): Boolean {
    stroke.clear()
    stroke.add(Pt(now(), x, y))
    return true
  }

  @Synchronized
  fun pointerMove(x: Float, y: Float): Boolean {
    if (stroke.isEmpty()) return true
    val last = stroke.last()
    if (now() - last.t >= 8 || hypot(x - last.x, y - last.y) > 24) stroke.add(Pt(now(), x, y))
    return true
  }

  @Synchronized
  fun pointerUp(x: Float, y: Float): Boolean {
    if (stroke.isEmpty()) return true
    stroke.add(Pt(now(), x, y))
    val pts = ArrayList(stroke)
    stroke.clear()
    return dispatch(listOf(pts))
  }

  fun longPress(x: Float, y: Float): Boolean {
    val t = now()
    return dispatch(listOf(listOf(Pt(t, x, y), Pt(t + 700, x, y))))
  }

  @Synchronized
  fun releaseAll() {
    stroke.clear()
    contacts.clear()
    liftedContacts.clear()
  }

  // ------------------------------------------------------------ scroll / pinch

  @Synchronized
  fun scroll(x: Float, y: Float, dx: Float, dy: Float): Boolean {
    scrollAcc[0] = x
    scrollAcc[1] = y
    scrollAcc[2] += dx
    scrollAcc[3] += dy
    if (!scrollPending) {
      scrollPending = true
      main.postDelayed({ flushScroll() }, 60)
    }
    return true
  }

  @Synchronized
  private fun flushScroll() {
    scrollPending = false
    val (x, y, dx, dy) = scrollAcc.toList()
    scrollAcc = floatArrayOf(0f, 0f, 0f, 0f)
    if (dx == 0f && dy == 0f) return
    // Wheel down (dy > 0) shows content further down: the finger moves up.
    val t = now()
    dispatch(listOf(listOf(Pt(t, clampX(x), clampY(y)), Pt(t + 220, clampX(x - dx), clampY(y - dy)))))
  }

  fun pinch(x: Float, y: Float, scale: Float): Boolean {
    val d0 = 120f
    val d1 = (d0 * scale).coerceIn(10f, 900f)
    val t = now()
    val a = listOf(Pt(t, clampX(x - d0), y), Pt(t + 260, clampX(x - d1), y))
    val b = listOf(Pt(t, clampX(x + d0), y), Pt(t + 260, clampX(x + d1), y))
    return dispatch(listOf(a, b))
  }

  // ------------------------------------------------------------ direct multi-touch

  /** state: 0 down, 1 move, 2 up, 3 cancel. */
  @Synchronized
  fun touch(id: Int, x: Float, y: Float, state: Int): Boolean {
    when (state) {
      0 -> {
        contacts[id] = arrayListOf(Pt(now(), x, y))
        liftedContacts.remove(id)
      }
      1 -> contacts[id]?.let { pts ->
        val last = pts.last()
        if (now() - last.t >= 8) pts.add(Pt(now(), x, y))
      }
      2 -> {
        contacts[id]?.add(Pt(now(), x, y))
        liftedContacts.add(id)
      }
      else -> {
        contacts.remove(id)
        liftedContacts.remove(id)
      }
    }
    if (contacts.isNotEmpty() && liftedContacts.containsAll(contacts.keys)) {
      val all = contacts.values.map { ArrayList(it) }
      contacts.clear()
      liftedContacts.clear()
      return dispatch(all)
    }
    return true
  }

  // ------------------------------------------------------------ keys / text

  fun key(keyCode: Int, down: Boolean): Boolean {
    if (!down) return true
    return when (keyCode) {
      KeyEvent.KEYCODE_ESCAPE, KeyEvent.KEYCODE_BACK -> performGlobalAction(GLOBAL_ACTION_BACK)
      KeyEvent.KEYCODE_HOME, KeyEvent.KEYCODE_MOVE_HOME -> performGlobalAction(GLOBAL_ACTION_HOME)
      KeyEvent.KEYCODE_DEL -> editFocused { it.dropLast(1) }
      KeyEvent.KEYCODE_ENTER -> {
        val node = focusedEditable() ?: return false
        if (Build.VERSION.SDK_INT >= 30) {
          node.performAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_IME_ENTER.id)
        } else editFocused { it + "\n" }
      }
      KeyEvent.KEYCODE_TAB -> editFocused { it + "\t" }
      KeyEvent.KEYCODE_SPACE -> editFocused { it + " " }
      else -> true // not representable through accessibility; ignored
    }
  }

  fun typeText(text: String): Boolean = editFocused { it + text }

  private fun focusedEditable(): AccessibilityNodeInfo? {
    val node = rootInActiveWindow?.findFocus(AccessibilityNodeInfo.FOCUS_INPUT) ?: return null
    return if (node.isEditable) node else null
  }

  private fun editFocused(change: (String) -> String): Boolean {
    val node = focusedEditable() ?: return false
    val current = if (Build.VERSION.SDK_INT >= 26 && node.isShowingHintText) "" else node.text?.toString() ?: ""
    val args = Bundle().apply {
      putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, change(current))
    }
    return node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)
  }

  // ------------------------------------------------------------ dispatch

  private fun dispatch(strokes: List<List<Pt>>): Boolean {
    val usable = strokes.filter { it.isNotEmpty() }.take(GestureDescription.getMaxStrokeCount())
    if (usable.isEmpty()) return true
    val t0 = usable.minOf { it.first().t }
    val maxDur = GestureDescription.getMaxGestureDuration()
    val builder = GestureDescription.Builder()
    for (pts in usable) {
      val path = Path()
      path.moveTo(clampX(pts.first().x), clampY(pts.first().y))
      val moved = pts.any { hypot(it.x - pts.first().x, it.y - pts.first().y) > 12 }
      if (moved) for (p in pts.drop(1)) path.lineTo(clampX(p.x), clampY(p.y))
      val start = min(pts.first().t - t0, maxDur - 1)
      val raw = pts.last().t - pts.first().t
      // A short stationary stroke is a tap.
      val dur = if (!moved && raw < 300) 40L else max(1L, min(raw, maxDur - start))
      builder.addStroke(GestureDescription.StrokeDescription(path, start, dur))
    }
    val n = usable.size
    return try {
      val ok = dispatchGesture(builder.build(), object : GestureResultCallback() {
        override fun onCompleted(gestureDescription: GestureDescription?) {
          Log.d(TAG, "gesture completed ($n stroke(s))")
        }

        override fun onCancelled(gestureDescription: GestureDescription?) {
          Log.w(TAG, "gesture cancelled ($n stroke(s))")
        }
      }, null)
      if (!ok) Log.w(TAG, "dispatchGesture refused")
      ok
    } catch (e: IllegalArgumentException) {
      Log.w(TAG, "invalid gesture", e)
      false
    }
  }
}
