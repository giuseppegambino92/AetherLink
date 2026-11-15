// Shizuku UserService (runs as the shell uid) that injects real input events.
package dev.aetherlink.mobile;

interface IInputService {
    /** Reserved by Shizuku for UserService teardown. */
    void destroy() = 16777114;

    /** action: MotionEvent.ACTION_*; source: InputDevice.SOURCE_*. Returns false if injection failed. */
    boolean injectMotion(int action, float x, float y, long downTime, int source, float vscroll, float hscroll, int buttonState) = 1;

    /** action: KeyEvent.ACTION_DOWN / ACTION_UP. */
    boolean injectKey(int action, int keyCode, int metaState) = 2;

    /** Types text through the virtual keyboard character map. */
    boolean injectText(String text) = 3;
}
