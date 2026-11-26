package com.stacksync.deskbolt

import android.os.Bundle
import com.google.androidgamesdk.GameActivity

class MainActivity : GameActivity() {
    private external fun nativeSetVideoCodecs(codecs: String)

    override fun onCreate(savedInstanceState: Bundle?) {
        nativeSetVideoCodecs(DeskBoltVideoCodecs.hardwareBuffers())
        super.onCreate(savedInstanceState)
    }

    companion object {
        init {
            System.loadLibrary("deskbolt_viewer_lib")
        }
    }
}
