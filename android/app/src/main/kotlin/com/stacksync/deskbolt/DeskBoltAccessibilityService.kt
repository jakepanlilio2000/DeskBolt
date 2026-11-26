package com.stacksync.deskbolt

import android.accessibilityservice.AccessibilityService
import android.content.ClipData
import android.content.ClipboardManager
import android.accessibilityservice.GestureDescription
import android.graphics.Path
import android.graphics.PointF
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import kotlin.math.abs

class DeskBoltAccessibilityService : AccessibilityService() {
    private val clipboard by lazy { getSystemService(ClipboardManager::class.java) }
    private var lastClipboard: Pair<String, String?>? = null
    private val clipboardListener = ClipboardManager.OnPrimaryClipChangedListener {
        if (DeskBoltHostBridge.captureWidth == 0) return@OnPrimaryClipChangedListener
        val clip = clipboard.primaryClip ?: return@OnPrimaryClipChangedListener
        if (clip.itemCount != 1 ||
            (Build.VERSION.SDK_INT >= 24 &&
                clip.description.extras?.getBoolean("android.content.extra.IS_SENSITIVE") == true)
        ) return@OnPrimaryClipChangedListener
        val item = clip.getItemAt(0)
        val text = item.text?.toString() ?: return@OnPrimaryClipChangedListener
        val content = text to item.htmlText
        if (content != lastClipboard) {
            lastClipboard = content
            DeskBoltHostBridge.nativeOnClipboardText(text, item.htmlText)
        }
    }
    private val main = Handler(Looper.getMainLooper())
    private var pointer = PointF()
    private var lastPoint = PointF()
    private var pressed = false
    private var inFlight = false
    private var continuation: GestureDescription.StrokeDescription? = null
    private val pendingPoints = ArrayList<PointF>()
    private var shift = false
    private var ctrl = false
    private var alt = false

    override fun onServiceConnected() {
        super.onServiceConnected()
        clipboard.removePrimaryClipChangedListener(clipboardListener)
        clipboard.addPrimaryClipChangedListener(clipboardListener)
        DeskBoltHostBridge.refreshClipboard = clipboardListener::onPrimaryClipChanged
        DeskBoltHostBridge.nativeRegisterAccessibility(this)
    }

    override fun onDestroy() {
        clipboard.removePrimaryClipChangedListener(clipboardListener)
        DeskBoltHostBridge.refreshClipboard = null
        onRemoteCancel()
        DeskBoltHostBridge.nativeRegisterAccessibility(null)
        super.onDestroy()
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) = Unit
    override fun onInterrupt() = onRemoteCancel()

    @Suppress("unused")
    fun onRemotePointer(action: Int, x: Float, y: Float) {
        if (!x.isFinite() || !y.isFinite()) return
        main.post {
            val width = DeskBoltHostBridge.captureWidth
            val height = DeskBoltHostBridge.captureHeight
            if (width <= 0 || height <= 0) return@post
            pointer = PointF(x.coerceIn(0f, 1f) * (width - 1), y.coerceIn(0f, 1f) * (height - 1))
            when (action) {
                0 -> if (pressed) appendPoint()
                1 -> if (!pressed && !inFlight) {
                    pressed = true
                    lastPoint = pointer
                    appendPoint()
                    dispatchDrag()
                }
                2 -> if (pressed) {
                    appendPoint()
                    pressed = false
                    if (!inFlight) dispatchDrag()
                }
                3 -> if (!pressed && !inFlight) dispatchOneShot(listOf(Path().apply {
                    moveTo(pointer.x, pointer.y)
                }), 650)
            }
        }
    }

    private fun appendPoint() {
        if (pendingPoints.size == 128) pendingPoints.removeAt(0)
        pendingPoints.add(pointer)
    }

    private fun dispatchDrag() {
        val current = continuation
        if (DeskBoltHostBridge.captureWidth == 0) {
            pressed = false
            pendingPoints.clear()
        }
        if (current == null && !pressed) return
        val path = Path().apply {
            moveTo(lastPoint.x, lastPoint.y)
            pendingPoints.forEach { lineTo(it.x, it.y) }
        }
        lastPoint = pendingPoints.lastOrNull() ?: lastPoint
        pendingPoints.clear()
        val continues = pressed
        val stroke = current?.continueStroke(path, 0, 45, continues)
            ?: GestureDescription.StrokeDescription(path, 0, 45, continues)
        continuation = if (continues) stroke else null
        inFlight = true
        if (!dispatchGesture(GestureDescription.Builder().addStroke(stroke).build(), object : GestureResultCallback() {
            override fun onCompleted(gestureDescription: GestureDescription?) {
                inFlight = false
                dispatchDrag()
            }
            override fun onCancelled(gestureDescription: GestureDescription?) = resetGesture()
        }, main)) resetGesture()
    }

    private fun resetGesture() {
        inFlight = false
        pressed = false
        continuation = null
        pendingPoints.clear()
    }

    @Suppress("unused")
    fun onRemoteScroll(dx: Int, dy: Int) {
        main.post {
            if (pressed || inFlight || dx == 0 && dy == 0) return@post
            val dm = resources.displayMetrics
            val width = DeskBoltHostBridge.captureWidth.toFloat()
            val height = DeskBoltHostBridge.captureHeight.toFloat()
            if (width <= 0 || height <= 0) return@post
            val separation = 24 * dm.density
            val x = pointer.x.coerceIn(width * 0.2f, width * 0.8f)
            val y = pointer.y.coerceIn(height * 0.2f, height * 0.8f)
            val factor = 0.65f * dm.density
            val distance = maxOf(abs(dx), abs(dy)).coerceAtMost(600)
            val paths = listOf(-separation, separation).map { offset ->
                val startX = (x + offset).coerceIn(0f, width - 1)
                Path().apply {
                    moveTo(startX, y)
                    lineTo((startX - dx * factor).coerceIn(0f, width - 1), (y + dy * factor).coerceIn(0f, height - 1))
                }
            }
            dispatchOneShot(paths, 85L + distance / 3L)
        }
    }

    private fun dispatchOneShot(paths: List<Path>, duration: Long) {
        val gesture = GestureDescription.Builder()
        for (path in paths) gesture.addStroke(GestureDescription.StrokeDescription(path, 0, duration))
        inFlight = dispatchGesture(gesture.build(), object : GestureResultCallback() {
            override fun onCompleted(gestureDescription: GestureDescription?) = resetGesture()
            override fun onCancelled(gestureDescription: GestureDescription?) = resetGesture()
        }, main)
    }

    @Suppress("unused")
    fun onRemoteAction(action: Int) {
        main.post {
            if (DeskBoltHostBridge.captureWidth == 0) return@post
            val id = when (action) {
                0 -> GLOBAL_ACTION_HOME
                1 -> GLOBAL_ACTION_BACK
                2 -> GLOBAL_ACTION_RECENTS
                3 -> GLOBAL_ACTION_NOTIFICATIONS
                4 -> GLOBAL_ACTION_QUICK_SETTINGS
                5 -> if (Build.VERSION.SDK_INT >= 28) GLOBAL_ACTION_LOCK_SCREEN else return@post
                else -> return@post
            }
            performGlobalAction(id)
        }
    }

    @Suppress("unused")
    fun onRemoteCancel() {
        main.post {
            shift = false
            ctrl = false
            alt = false
            pressed = false
            if (!inFlight) dispatchDrag()
        }
    }

    fun onRemoteClipboard(kind: Int, value: String) {
        main.post {
            if (DeskBoltHostBridge.captureWidth == 0) return@post
            when (kind) {
                0 -> {
                    lastClipboard = value to null
                    clipboard.setPrimaryClip(ClipData.newPlainText("DeskBolt", value))
                }
                1 -> {
                    val text = android.text.Html.fromHtml(value, android.text.Html.FROM_HTML_MODE_LEGACY).toString()
                    lastClipboard = text to value
                    clipboard.setPrimaryClip(ClipData.newHtmlText("DeskBolt", text, value))
                }
                2 -> {
                    lastClipboard = null
                    if (Build.VERSION.SDK_INT >= 28) clipboard.clearPrimaryClip()
                    else {
                        lastClipboard = "" to null
                        clipboard.setPrimaryClip(ClipData.newPlainText("DeskBolt", ""))
                    }
                }
            }
        }
    }

    @Suppress("unused")
    fun onRemoteKey(code: Int, down: Boolean) {
        main.post {
            if (DeskBoltHostBridge.captureWidth == 0) return@post
            when (code) {
                0x10, 0xa0, 0xa1 -> shift = down
                0x11, 0xa2, 0xa3 -> ctrl = down
                0x12, 0xa4, 0xa5 -> alt = down
                else -> if (down && !ctrl && !alt) {
                    when (code) {
                        0x1b -> performGlobalAction(GLOBAL_ACTION_BACK)
                        0x08 -> editFocus("", delete = true)
                        0x0d -> editFocus("\n")
                        0x09 -> editFocus("\t")
                        else -> keyText(code, shift)?.let { editFocus(it) }
                    }
                }
            }
        }
    }

    @Suppress("unused")
    fun onRemoteText(text: String) {
        main.post {
            if (DeskBoltHostBridge.captureWidth > 0) editFocus(text)
        }
    }

    private fun editFocus(text: String, delete: Boolean = false) {
        val target = rootInActiveWindow?.findFocus(AccessibilityNodeInfo.FOCUS_INPUT) ?: return
        if (!target.isEditable) return
        val old = target.text?.toString().orEmpty()
        var start = target.textSelectionStart.takeIf { it in 0..old.length } ?: old.length
        val end = target.textSelectionEnd.coerceIn(start, old.length)
        if (delete && start == end && start > 0) start = old.offsetByCodePoints(start, -1)
        val newText = old.replaceRange(start, end, text)
        val args = Bundle().apply { putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, newText) }
        if (target.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)) {
            val caret = start + text.length
            target.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, Bundle().apply {
                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, caret)
                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, caret)
            })
        }
    }

    private fun keyText(code: Int, shifted: Boolean): String? {
        if (code in 0x41..0x5a) return code.toChar().let { if (shifted) it else it.lowercaseChar() }.toString()
        val codes = intArrayOf(0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x20,
            0xba, 0xbb, 0xbc, 0xbd, 0xbe, 0xbf, 0xc0, 0xdb, 0xdc, 0xdd, 0xde)
        val plain = "0123456789 ;=,-./`[\\]'"
        val shiftChars = ")!@#$%^&*( :+<_>?~{|}\""
        return codes.indexOf(code).takeIf { it >= 0 }?.let { (if (shifted) shiftChars else plain)[it].toString() }
    }
}
