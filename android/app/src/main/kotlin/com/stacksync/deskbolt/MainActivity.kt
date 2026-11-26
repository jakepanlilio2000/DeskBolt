package com.stacksync.deskbolt

import com.google.androidgamesdk.GameActivity

class MainActivity : GameActivity() {
    companion object {
        init {
            System.loadLibrary("deskbolt_viewer_lib")
        }
    }
}
