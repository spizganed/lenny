package com.spizganed.android_camera

import java.nio.ByteBuffer

/** Called on the core's I/O thread. Keep it quick; never throw (the JNI side treats exceptions as failures). */
interface SenderListener {
    /** Receiver asked for these settings. Return the effective [width, height, fpsNum, fpsDen, bitrateKbps]. */
    fun onStreamConfig(width: Int, height: Int, fpsNum: Int, fpsDen: Int, bitrateKbps: Int): IntArray

    /** Camera control (LENNY_CTL_*). Return LENNY_ACK_* (0 ok, 1 unsupported, 2 failed). */
    fun onControl(cmd: Int, x: Int, y: Int, value: Int): Int

    /** Congestion control picked a new encoder bitrate. */
    fun onBitrate(kbps: Int)
}

/** Thin JNI surface over core/include/lenny/lenny.h (see src/main/cpp/lenny_jni.cpp). */
object LennyNative {
    init {
        System.loadLibrary("lenny_jni") // pulls in liblenny_core.so, the same copy Dart FFI opens
    }

    // Mirrors of core/include/lenny/lenny.h
    const val CTL_KEYFRAME_REQUEST = 10
    const val CTL_FOCUS_AT = 11
    const val CTL_FOCUS_LOCK = 12
    const val CTL_FOCUS_AUTO = 13
    const val CTL_EXPOSURE_COMP = 14
    const val CTL_EXPOSURE_LOCK = 15
    const val CTL_WB_LOCK = 16
    const val CTL_TORCH = 17
    const val CTL_SELECT_LENS = 18
    const val CTL_ZOOM = 19
    const val CTL_RESET_AUTO = 20
    const val CTL_PAN = 21
    const val CAP_FOCUS = 1 shl 0
    const val CAP_FOCUS_LOCK = 1 shl 1
    const val CAP_EXPOSURE_COMP = 1 shl 2
    const val CAP_EXPOSURE_LOCK = 1 shl 3
    const val CAP_WB_LOCK = 1 shl 4
    const val CAP_TORCH = 1 shl 5
    const val CAP_LENS = 1 shl 6
    const val CAP_ZOOM = 1 shl 7
    const val CAP_PAN = 1 shl 8
    const val ACK_OK = 0
    const val ACK_UNSUPPORTED = 1
    const val ACK_FAILED = 2
    const val FRAME_KEYFRAME = 1

    /**
     * modes = flat [w, h, fpsNum, fpsDen, ...]; lenses = flat [id, facing, ...] with one label each;
     * exposure = [minEvMilli, maxEvMilli, stepMilli] or empty; lensCaps = per lens [zoomMin, zoomMax, zoomBase, modeCount,
     * modes (w, h, fps, 1)...] (lenny_lens_caps). Returns 0 on failure.
     */
    @JvmStatic external fun create(
        deviceId: ByteArray, name: String, modes: IntArray, maxBitrateKbps: Int, controls: Int,
        lenses: IntArray, lensLabels: Array<String>, exposure: IntArray, lensCaps: IntArray, listener: SenderListener,
    ): Long

    /** The lenny_session* inside a handle, for Dart FFI. */
    @JvmStatic external fun sessionPtr(handle: Long): Long
    @JvmStatic external fun connect(handle: Long, host: String, port: Int, token: ByteArray?): Int
    @JvmStatic external fun sendConfig(handle: Long, buf: ByteBuffer, offset: Int, size: Int): Int
    @JvmStatic external fun sendFrame(
        handle: Long, buf: ByteBuffer, offset: Int, size: Int, ptsUs: Long, orientation: Int, flags: Int,
    ): Int
    @JvmStatic external fun updateStream(handle: Long, width: Int, height: Int, fps: Int, bitrateKbps: Int): Int

    /** state = [afMode, exposureCompEvMilli, exposureLock, wbLock, torch, lensId, zoomX100, battery, charging, panX, panY] */
    @JvmStatic external fun sendControlState(handle: Long, state: IntArray): Int
    @JvmStatic external fun disconnect(handle: Long): Int
    @JvmStatic external fun destroy(handle: Long)
    @JvmStatic external fun nowUs(): Long
}
