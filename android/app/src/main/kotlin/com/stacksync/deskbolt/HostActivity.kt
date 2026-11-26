package com.stacksync.deskbolt

import android.app.Activity
import android.content.Intent
import android.media.projection.MediaProjectionConfig
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import android.text.InputType
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView

class HostActivity : Activity() {
    private lateinit var password: EditText
    private lateinit var status: TextView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        password = EditText(this).apply {
            hint = "Host password"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
            isSingleLine = true
            isSaveEnabled = false
        }
        status = TextView(this).apply { text = "Allow screen sharing to accept remote connections." }
        val layout = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            val padding = (24 * resources.displayMetrics.density).toInt()
            setPadding(padding, padding, padding, padding)
            addView(status)
            addView(password)
            addView(Button(this@HostActivity).apply {
                text = "Accessibility Settings"
                setOnClickListener { startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS)) }
            })
            addView(Button(this@HostActivity).apply {
                text = "Start Sharing"
                setOnClickListener { requestSharing() }
            })
            addView(Button(this@HostActivity).apply {
                text = "Stop Sharing"
                setOnClickListener {
                    stopService(Intent(this@HostActivity, DeskBoltScreenShareService::class.java))
                    status.text = "Screen sharing stopped."
                }
            })
        }
        setContentView(layout)
    }

    @Suppress("DEPRECATION")
    private fun requestSharing() {
        if (password.text.isEmpty()) {
            password.error = "Enter a host password."
            return
        }
        val manager = getSystemService(MediaProjectionManager::class.java)
        val intent = if (Build.VERSION.SDK_INT >= 34) {
            manager.createScreenCaptureIntent(MediaProjectionConfig.createConfigForDefaultDisplay())
        } else {
            manager.createScreenCaptureIntent()
        }
        startActivityForResult(intent, SHARE_SCREEN)
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) DeskBoltHostBridge.refreshClipboard?.invoke()
    }

    @Deprecated("Uses the platform's activity result callback.")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != SHARE_SCREEN || resultCode != RESULT_OK || data == null) return
        val secret = password.text.toString()
        password.text.clear()
        if (secret.isEmpty()) return
        startForegroundService(Intent(this, DeskBoltScreenShareService::class.java).apply {
            putExtra("resultCode", resultCode)
            putExtra("data", data)
            putExtra("password", secret)
        })
        status.text = "Screen sharing requested. Use the host notification to stop sharing."
    }

    companion object {
        private const val SHARE_SCREEN = 1
    }
}
