package com.stacksync.deskbolt

import android.media.MediaCodecList
import android.os.Build

internal object DeskBoltVideoCodecs {
    fun hardwareBuffers(): String {
        if (Build.VERSION.SDK_INT < 29) return ""
        val types = setOf("video/avc", "video/hevc", "video/av01")
        return runCatching {
            MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
                .filter { it.isHardwareAccelerated && !it.isAlias }
                .flatMap { codec ->
                    val direction = if (codec.isEncoder) "e" else "d"
                    codec.supportedTypes.filter { it in types }.flatMap { mime ->
                        val formats = runCatching { codec.getCapabilitiesForType(mime).colorFormats }
                            .getOrDefault(intArrayOf())
                        // Only advertise layouts the native byte-buffer path can copy.
                        formats.filter { it == 19 || it == 21 }.distinct()
                            .map { color -> "$mime|$direction|$color|${codec.name}" }
                    }
                }.take(128).joinToString("\n")
        }.getOrDefault("")
    }
}
