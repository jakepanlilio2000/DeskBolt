package com.stacksync.deskbolt

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.graphics.Path
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.media.ImageReader
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.IBinder
import android.util.DisplayMetrics
import android.view.WindowManager
import android.view.accessibility.AccessibilityEvent

object DeskBoltHostBridge {
    init {
        System.loadLibrary("deskbolt_server_lib")
    }

    external fun nativeOnFrameRgba(rgba: ByteArray, width: Int, height: Int)
    external fun nativeRegisterAccessibility(serviceObj: Any)
    external fun nativeStartHost(dataDir: String, password: String): Int
    external fun nativeStopHost()
}

class DeskBoltScreenShareService : Service() {
    private var projection: MediaProjection? = null
    private var reader: ImageReader? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        ensureNotificationChannel()
        val notif = Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("DeskBolt Host Active")
            .setContentText("Sharing device display over encrypted LAN connection")
            .build()
        startForeground(1, notif)

        val resultCode = intent?.getIntExtra("resultCode", 0) ?: 0
        val data = if (Build.VERSION.SDK_INT >= 33) {
            intent?.getParcelableExtra("data", Intent::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent?.getParcelableExtra("data")
        }
        val pw = intent?.getStringExtra("password") ?: ""

        if (resultCode != 0 && data != null) {
            val mgr = getSystemService(MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
            projection = mgr.getMediaProjection(resultCode, data)
            val metrics = DisplayMetrics()
            @Suppress("DEPRECATION")
            (getSystemService(WINDOW_SERVICE) as WindowManager).defaultDisplay.getRealMetrics(metrics)
            val w = metrics.widthPixels.coerceAtLeast(720)
            val h = metrics.heightPixels.coerceAtLeast(1280)
            reader = ImageReader.newInstance(w, h, PixelFormat.RGBA_8888, 2)
            reader?.setOnImageAvailableListener({ ir ->
                val img = ir.acquireLatestImage() ?: return@setOnImageAvailableListener
                val plane = img.planes[0]
                val buf = plane.buffer
                val rowStride = plane.rowStride
                val pixelStride = plane.pixelStride
                val out = ByteArray(w * h * 4)
                if (rowStride == w * 4 && pixelStride == 4) {
                    buf.get(out)
                } else {
                    val row = ByteArray(rowStride)
                    for (y in 0 until h) {
                        buf.get(row, 0, rowStride.coerceAtMost(buf.remaining()))
                        System.arraycopy(row, 0, out, y * w * 4, w * 4)
                    }
                }
                img.close()
                DeskBoltHostBridge.nativeOnFrameRgba(out, w, h)
            }, null)
            projection?.createVirtualDisplay(
                "DeskBoltDisplay",
                w,
                h,
                metrics.densityDpi,
                DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
                reader?.surface,
                null,
                null
            )
            DeskBoltHostBridge.nativeStartHost(filesDir.absolutePath, pw)
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        DeskBoltHostBridge.nativeStopHost()
        reader?.close()
        projection?.stop()
        super.onDestroy()
    }

    private fun ensureNotificationChannel() {
        val mgr = getSystemService(NOTIFICATION_SERVICE) as NotificationManager
        val ch = NotificationChannel(CHANNEL_ID, "DeskBolt Screen Share", NotificationManager.IMPORTANCE_LOW)
        mgr.createNotificationChannel(ch)
    }

    companion object {
        private const val CHANNEL_ID = "deskbolt_host"
    }
}

class DeskBoltAccessibilityService : AccessibilityService() {
    override fun onServiceConnected() {
        super.onServiceConnected()
        DeskBoltHostBridge.nativeRegisterAccessibility(this)
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {}
    override fun onInterrupt() {}

    @Suppress("unused")
    fun onRemoteTap(normX: Float, normY: Float) {
        val dm = resources.displayMetrics
        val px = (normX.coerceIn(0f, 1f) * dm.widthPixels)
        val py = (normY.coerceIn(0f, 1f) * dm.heightPixels)
        val path = Path().apply { moveTo(px, py) }
        val stroke = GestureDescription.StrokeDescription(path, 0, 50)
        dispatchGesture(GestureDescription.Builder().addStroke(stroke).build(), null, null)
    }

    @Suppress("unused")
    fun onRemoteKey(_vkCode: Int, _down: Boolean) {
        // Basic global action fallback when remote user presses Escape / Home / Back
        if (_down && _vkCode == 0x1B) {
            performGlobalAction(GLOBAL_ACTION_BACK)
        }
    }
}
