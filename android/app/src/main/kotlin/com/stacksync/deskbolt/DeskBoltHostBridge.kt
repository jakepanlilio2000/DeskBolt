package com.stacksync.deskbolt

import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.res.Configuration
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.ImageReader
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.DisplayMetrics
import android.view.WindowManager

object DeskBoltHostBridge {
    @Volatile var captureWidth = 0
    @Volatile var captureHeight = 0
    var refreshClipboard: (() -> Unit)? = null
    init {
        System.loadLibrary("deskbolt_server_lib")
    }

    external fun nativeOnFrameRgba(rgba: ByteArray, width: Int, height: Int)
    external fun nativeOnClipboardText(text: String, html: String?)
    external fun nativeRegisterAccessibility(serviceObj: Any?)
    external fun nativeStartHost(dataDir: String, password: String): Int
    external fun nativeStopHost()
}

class DeskBoltScreenShareService : Service() {
    private var projection: MediaProjection? = null
    private var reader: ImageReader? = null
    private var display: VirtualDisplay? = null
    private val main = Handler(Looper.getMainLooper())
    private val projectionCallback = object : MediaProjection.Callback() {
        override fun onStop() { stopSelf() }

        override fun onCapturedContentResize(width: Int, height: Int) {
            resizeCapture(width, height, displayMetrics().densityDpi)
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == STOP_SHARING) {
            stopSelf()
            return START_NOT_STICKY
        }
        if (projection != null) return START_NOT_STICKY
        ensureNotificationChannel()
        val stop = PendingIntent.getService(this, 0,
            Intent(this, DeskBoltScreenShareService::class.java).setAction(STOP_SHARING),
            PendingIntent.FLAG_IMMUTABLE)
        val notif = Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.ic_menu_view)
            .setContentTitle("DeskBolt is sharing your screen")
            .addAction(Notification.Action.Builder(null, "Stop Sharing", stop).build())
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

        if (resultCode != Activity.RESULT_OK || data == null || pw.isEmpty()) {
            stopSelf()
            return START_NOT_STICKY
        }
        try {
            val mgr = getSystemService(MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
            val activeProjection = checkNotNull(mgr.getMediaProjection(resultCode, data))
            projection = activeProjection
            activeProjection.registerCallback(projectionCallback, main)
            val metrics = displayMetrics()
            val w = metrics.widthPixels
            val h = metrics.heightPixels
            DeskBoltHostBridge.captureWidth = w
            DeskBoltHostBridge.captureHeight = h
            val imageReader = newReader(w, h)
            reader = imageReader
            display = activeProjection.createVirtualDisplay(
                "DeskBolt Screen Sharing",
                w,
                h,
                metrics.densityDpi,
                DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
                imageReader.surface,
                null,
                null
            )
            if (display == null || DeskBoltHostBridge.nativeStartHost(filesDir.absolutePath, pw) != 0) {
                stopSelf()
            }
        } catch (_: IllegalStateException) {
            stopSelf()
        } catch (_: SecurityException) {
            stopSelf()
        }
        return START_NOT_STICKY
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        if (Build.VERSION.SDK_INT < 34) {
            val metrics = displayMetrics()
            resizeCapture(metrics.widthPixels, metrics.heightPixels, metrics.densityDpi)
        }
    }

    @Suppress("DEPRECATION")
    private fun displayMetrics() = DisplayMetrics().also {
        (getSystemService(WINDOW_SERVICE) as WindowManager).defaultDisplay.getRealMetrics(it)
    }

    private fun resizeCapture(width: Int, height: Int, density: Int) {
        val activeDisplay = display ?: return
        if (width <= 0 || height <= 0 || (width == reader?.width && height == reader?.height)) return
        val previous = reader
        val replacement = newReader(width, height)
        reader = replacement
        try {
            activeDisplay.resize(width, height, density)
            activeDisplay.surface = replacement.surface
            DeskBoltHostBridge.captureWidth = width
            DeskBoltHostBridge.captureHeight = height
        } catch (_: IllegalStateException) {
            stopSelf()
        } finally {
            previous?.close()
        }
    }

    private fun newReader(width: Int, height: Int): ImageReader {
        return ImageReader.newInstance(width, height, PixelFormat.RGBA_8888, 2).apply {
            setOnImageAvailableListener({ source ->
                if (source !== reader) return@setOnImageAvailableListener
                source.acquireLatestImage()?.use { image ->
                    val plane = image.planes[0]
                    val buf = plane.buffer
                    val rowBytes = width * 4
                    if (plane.pixelStride != 4 ||
                        (height - 1L) * plane.rowStride + rowBytes > buf.limit()) return@use
                    val out = ByteArray(rowBytes * height)
                    for (y in 0 until height) {
                        buf.position(y * plane.rowStride)
                        buf.get(out, y * rowBytes, rowBytes)
                    }
                    DeskBoltHostBridge.nativeOnFrameRgba(out, width, height)
                }
            }, main)
        }
    }

    override fun onDestroy() {
        DeskBoltHostBridge.nativeStopHost()
        DeskBoltHostBridge.captureWidth = 0
        DeskBoltHostBridge.captureHeight = 0
        display?.release()
        reader?.close()
        projection?.unregisterCallback(projectionCallback)
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
        private const val STOP_SHARING = "com.stacksync.deskbolt.STOP_SHARING"
    }
}
