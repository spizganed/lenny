package com.spizganed.android_camera

import android.content.Context
import android.graphics.Rect
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CameraMetadata
import android.hardware.camera2.CaptureRequest
import android.hardware.camera2.CaptureResult
import android.hardware.camera2.TotalCaptureResult
import android.hardware.camera2.params.MeteringRectangle
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.BatteryManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.os.Looper
import android.os.SystemClock
import android.util.Base64
import android.util.Log
import android.util.Range
import android.util.Size
import android.view.OrientationEventListener
import android.view.Surface
import android.view.WindowManager
import java.security.SecureRandom
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.math.abs
import kotlin.math.hypot
import kotlin.math.roundToInt

/**
 * Camera -> H.264 -> core, plus camera controls. Camera2 renders straight into the encoder's input surface, so frames
 * stay on the GPU until MediaCodec hands us the encoded bytes.
 *
 * Camera2, not CameraX: CameraX only sees CameraManager.cameraIdList, and many phones leave the ultrawide and tele
 * out of it while still letting apps open them. A logical multi-camera (one id over several sensors, switched by
 * zoom ratio) becomes one lens per sensor: 0.6x, 1x, 2x. Switching between those is a new request, not a restart.
 *
 * Threads: the core calls onStreamConfig/onControl/onBitrate on its I/O thread; everything that touches the camera
 * (device, session, requests, [state]) runs on the main thread; encoder output is handled on "lenny-encoder".
 */
class Pipeline(private val context: Context) : SenderListener {
    private val main = Handler(Looper.getMainLooper())
    private val codecThread = HandlerThread("lenny-encoder").apply { start() }
    private val codecHandler = Handler(codecThread.looper)
    private val manager = context.getSystemService(CameraManager::class.java)

    @Volatile private var handle = 0L
    private lateinit var lenses: List<Lens>
    private var device: CameraDevice? = null
    private var opening: String? = null // camera id being opened
    private var session: CameraCaptureSession? = null
    private var target: Surface? = null
    private var sessionGen = 0
    private var bound: Settings? = null
    private var outSize = Size(1, 1)
    private var stopped = false
    private var focusRegion: MeteringRectangle? = null
    @Volatile private var encoder: MediaCodec? = null
    @Volatile private var orientation = 0 // 0..3, quarter turns clockwise to upright
    private var deviceDegrees = 0 // how far the phone is turned clockwise from portrait
    @Volatile private var ptsOffsetUs = 0L // camera clock -> core clock (CLOCK_MONOTONIC)
    @Volatile private var ptsCalibrated = false
    /** LENNY_CAP_* union over all lenses, lens labels, exposure range [min, max, step] in EV*1000 (after start). */
    @Volatile var caps = 0
        private set
    var lensLabels: List<String> = emptyList()
        private set
    var exposure: IntArray = IntArray(0)
        private set

    /** Current camera control state. Main thread only. */
    val state = ControlState()

    /** Called with [state] after every change, on the main thread (the Flutter side shows it). */
    var onStateChanged: ((ControlState) -> Unit)? = null

    private data class Settings(val width: Int, val height: Int, val fps: Int, val bitrateKbps: Int, val lens: Int)

    /** One selectable lens: a camera id, plus the zoom ratio that picks its sensor on a logical camera. */
    private class Lens(val id: String, val chars: CameraCharacteristics, val facing: Int, val zoom: Float) {
        var label = ""
        /** What this lens can stream, flat [w, h, fps, 1, ...] (CAPS per-lens modes, protocol 1.1). */
        var modes = IntArray(0)
        fun <T> get(key: CameraCharacteristics.Key<T>): T? = chars.get(key)
    }

    private val lens get() = lenses[(bound?.lens ?: state.lens).coerceIn(lenses.indices)]

    private val orientationListener = object : OrientationEventListener(context) {
        override fun onOrientationChanged(deg: Int) {
            if (deg == ORIENTATION_UNKNOWN) return // flat on a table: keep the last one
            deviceDegrees = (deg + 45) / 90 % 4 * 90
            updateOrientation()
        }
    }

    /** Any thread except main (it waits for camera callbacks there). Returns the lenny_session* for Dart FFI. */
    fun start(host: String, port: Int, token: ByteArray?): Long {
        // Discovery runs at every start (every Connect), so a camera that appeared since (USB) is picked up.
        lenses = discoverLenses()
        check(lenses.isNotEmpty()) { "no camera" }
        lenses.forEach { it.modes = supportedModes(it) }
        state.lens = lenses.indexOfFirst { it.facing == FACING_BACK && it.zoom == 1f }.coerceAtLeast(0)
        caps = capabilities()
        lensLabels = lenses.map { it.label }
        exposure = exposureRange()
        val flat = lenses.flatMapIndexed { i, l -> listOf(i, l.facing) }.toIntArray()
        // Per lens: [zoomMin, zoomMax, zoomBase, modeCount, modes...]; min/max as ratio*100 relative to the lens
        // (CTL_ZOOM units), base = the lens's own ratio*100 on its camera (what pan is relative to).
        val lensCaps = lenses.flatMap { l ->
            val (lo, hi) = zoomRange(l)
            listOf((lo / l.zoom * 100).roundToInt(), (hi / l.zoom * 100).roundToInt(), (l.zoom * 100).roundToInt(),
                l.modes.size / 4) + l.modes.toList()
        }.toIntArray()
        handle = LennyNative.create(
            deviceId(context), Build.MODEL, lenses[state.lens].modes, MAX_BITRATE_KBPS, caps, flat,
            lensLabels.toTypedArray(), exposure, lensCaps,
            this,
        )
        check(handle != 0L) { "lenny_sender_create failed" }
        @Suppress("DEPRECATION")
        val rotation = context.getSystemService(WindowManager::class.java).defaultDisplay.rotation
        deviceDegrees = intArrayOf(0, 270, 180, 90)[rotation]
        main.post { orientationListener.enable() }
        LennyNative.connect(handle, host, port, token)
        return LennyNative.sessionPtr(handle)
    }

    /** Main thread. */
    fun stop() {
        stopped = true
        orientationListener.disable()
        main.removeCallbacks(batteryTick)
        closeSession() // encoder released when the session reports closed
        device?.close()
        device = null
        // Destroy on the encoder thread: every sendFrame runs there, so none can race the free.
        codecHandler.post {
            val h = handle
            handle = 0L
            LennyNative.disconnect(h)
            LennyNative.destroy(h)
            codecThread.quitSafely()
        }
    }

    // ---- core callbacks (I/O thread) --------------------------------------------------------------------------

    override fun onStreamConfig(width: Int, height: Int, fpsNum: Int, fpsDen: Int, bitrateKbps: Int): IntArray {
        val bitrate = bitrateKbps.coerceIn(1000, MAX_BITRATE_KBPS)
        // Only ever stream a mode the lens really has: the receiver was told about them in CAPS, and the effective
        // mode goes back in STREAM_START, so nothing silently falls back.
        val m = nearestMode(lenses[state.lens.coerceIn(lenses.indices)].modes, width, height, fpsNum / maxOf(fpsDen, 1))
        main.post {
            val s = Settings(m[0], m[1], m[2], bitrate, state.lens)
            // Same settings again = a new stream (first connect or reconnect): back to Auto (protocol.md §6.9).
            // Different settings on a running camera = the desktop switched mode mid-stream: keep the controls.
            // ponytail: a reconnect that also changes the mode keeps the controls; pass the core's link state
            // through JNI if that ever matters.
            val modeSwitch = bound != null && session != null && s.copy(lens = 0) != bound!!.copy(lens = 0)
            if (!modeSwitch) resetAll()
            bindCamera(s)
        }
        return intArrayOf(m[0], m[1], m[2], 1, bitrate)
    }

    override fun onControl(cmd: Int, x: Int, y: Int, value: Int): Int {
        if (cmd == LennyNative.CTL_KEYFRAME_REQUEST) {
            encoder?.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) })
            // The core asks for a keyframe at every stream (re)start: also a good moment to tell the receiver
            // what the camera is doing.
            main.post {
                publishState()
                main.removeCallbacks(batteryTick)
                main.postDelayed(batteryTick, BATTERY_PERIOD_MS)
            }
            return LennyNative.ACK_OK // no encoder yet = the first frame will be a keyframe anyway
        }
        if (!supported(cmd, value)) return LennyNative.ACK_UNSUPPORTED
        main.post { apply(cmd, x, y, value) }
        return LennyNative.ACK_OK
    }

    override fun onBitrate(kbps: Int) {
        encoder?.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_VIDEO_BITRATE, kbps * 1000) })
    }

    // ---- controls (main thread) -----------------------------------------------------------------------------

    /** The phone's own UI. Same code path as remote controls. */
    fun control(cmd: Int, x: Int, y: Int, value: Int): Int {
        if (!supported(cmd, value)) return LennyNative.ACK_UNSUPPORTED
        apply(cmd, x, y, value)
        return LennyNative.ACK_OK
    }

    private fun supported(cmd: Int, value: Int): Boolean = when (cmd) {
        LennyNative.CTL_FOCUS_AT, LennyNative.CTL_FOCUS_AUTO -> caps and LennyNative.CAP_FOCUS != 0
        LennyNative.CTL_FOCUS_LOCK -> caps and LennyNative.CAP_FOCUS_LOCK != 0
        LennyNative.CTL_EXPOSURE_COMP -> caps and LennyNative.CAP_EXPOSURE_COMP != 0
        LennyNative.CTL_EXPOSURE_LOCK -> caps and LennyNative.CAP_EXPOSURE_LOCK != 0
        LennyNative.CTL_WB_LOCK -> caps and LennyNative.CAP_WB_LOCK != 0
        LennyNative.CTL_TORCH -> caps and LennyNative.CAP_TORCH != 0
        LennyNative.CTL_SELECT_LENS -> caps and LennyNative.CAP_LENS != 0 && value in lenses.indices
        LennyNative.CTL_ZOOM -> caps and LennyNative.CAP_ZOOM != 0
        LennyNative.CTL_PAN -> caps and LennyNative.CAP_PAN != 0
        LennyNative.CTL_RESET_AUTO -> true
        else -> false
    }

    private fun apply(cmd: Int, x: Int, y: Int, value: Int) {
        when (cmd) {
            LennyNative.CTL_FOCUS_AT -> focusAt(x / 65535f, y / 65535f)
            LennyNative.CTL_FOCUS_LOCK -> {
                state.focusLocked = value != 0
                if (state.focusLocked) focusAt(state.focusX, state.focusY) else cancelFocus()
            }
            LennyNative.CTL_FOCUS_AUTO -> {
                state.focusLocked = false
                cancelFocus()
            }
            LennyNative.CTL_EXPOSURE_COMP -> {
                val step = lens.get(CameraCharacteristics.CONTROL_AE_COMPENSATION_STEP)?.toFloat() ?: 0f
                val range = lens.get(CameraCharacteristics.CONTROL_AE_COMPENSATION_RANGE)
                if (step > 0f && range != null) {
                    val index = (value / 1000f / step).roundToInt().coerceIn(range.lower, range.upper)
                    state.exposureEvMilli = (index * step * 1000).roundToInt()
                }
            }
            LennyNative.CTL_EXPOSURE_LOCK -> state.aeLock = value != 0
            LennyNative.CTL_WB_LOCK -> state.awbLock = value != 0
            LennyNative.CTL_TORCH -> state.torch = value != 0 && hasFlash(lens)
            LennyNative.CTL_SELECT_LENS -> if (value != state.lens) {
                state.lens = value
                state.zoom100 = 100 // zoom is relative to the lens
                state.panX = PAN_CENTER
                state.panY = PAN_CENTER
                if (!hasFlash(lenses[value])) state.torch = false
                focusRegion = null
                bound?.let { b ->
                    // The new lens may not have the current mode: move to its nearest one and tell the receiver.
                    val m = nearestMode(lenses[value].modes, b.width, b.height, b.fps)
                    val s = b.copy(width = m[0], height = m[1], fps = m[2], lens = value)
                    if (s.copy(lens = 0) != b.copy(lens = 0)) {
                        LennyNative.updateStream(handle, s.width, s.height, s.fps, s.bitrateKbps)
                    }
                    bindCamera(s)
                }
            }
            LennyNative.CTL_ZOOM -> {
                val (lo, hi) = zoomRange(lens)
                val ratio = (lens.zoom * value / 100f).coerceIn(lo, hi)
                state.zoom100 = (ratio / lens.zoom * 100).roundToInt()
            }
            LennyNative.CTL_PAN -> {
                state.panX = x
                state.panY = y
            }
            LennyNative.CTL_RESET_AUTO -> resetAuto()
        }
        applyRequest()
        publishState()
    }

    /** upright (u, v) in 0..1 -> buffer coordinates, then tap-to-focus. The focus holds there (Manual mode) until
     *  Auto or focus_auto: tapping a point is the lock. */
    private fun focusAt(u: Float, v: Float) {
        state.focusX = u
        state.focusY = v
        if (lens.get(CameraCharacteristics.CONTROL_AF_AVAILABLE_MODES)?.contains(CameraMetadata.CONTROL_AF_MODE_AUTO)
            != true
        ) return // fixed-focus lens
        val (bx, by) = toBuffer(u, v)
        focusRegion = meteringRegion(bx, by)
        state.focusLocked = true
        state.afMode = ControlState.AF_FOCUSING
        applyRequest(trigger = true)
    }

    private fun cancelFocus() {
        focusRegion = null
        state.afMode = if (state.focusLocked) ControlState.AF_LOCKED else ControlState.AF_CONTINUOUS
        applyRequest()
        publishState()
    }

    /** Upright point (0..1, what the receiver shows) -> buffer point: inverse of the receiver's upright rotation
     *  (quarter turns clockwise). The buffer is sensor-native, like the active array. */
    private fun toBuffer(u: Float, v: Float): Pair<Float, Float> = when (orientation) {
        1 -> v to 1f - u
        2 -> 1f - u to 1f - v
        3 -> 1f - v to u
        else -> u to v
    }

    /** Buffer point (0..1) -> 3A region, in the coordinates regions use: the active array, or with
     *  CONTROL_ZOOM_RATIO the zoomed view scaled to it. Within that, what's visible is the crop region (pan / pre-30
     *  zoom), further cropped by the output's aspect ratio. */
    private fun meteringRegion(bx: Float, by: Float): MeteringRectangle? {
        val ar = lens.get(CameraCharacteristics.SENSOR_INFO_ACTIVE_ARRAY_SIZE) ?: return null
        val vis = readout(lens).crop ?: Rect(0, 0, ar.width(), ar.height())
        var w = vis.width().toFloat()
        var h = vis.height().toFloat()
        val aspect = outSize.width.toFloat() / outSize.height
        if (w / h > aspect) w = h * aspect else h = w / aspect
        val cx = vis.exactCenterX() + (bx - 0.5f) * w
        val cy = vis.exactCenterY() + (by - 0.5f) * h
        val half = maxOf(ar.width(), ar.height()) * 0.05f
        val r = Rect(
            (cx - half).toInt().coerceIn(0, ar.width() - 1), (cy - half).toInt().coerceIn(0, ar.height() - 1),
            (cx + half).toInt().coerceIn(1, ar.width()), (cy + half).toInt().coerceIn(1, ar.height()),
        )
        return MeteringRectangle(r, MeteringRectangle.METERING_WEIGHT_MAX)
    }

    /** Auto mode: continuous AF, auto exposure/ISO at EV 0, auto WB, no locks. Nothing manual is left over.
     *  Framing and light (lens, zoom, pan, torch) aren't auto/manual things and stay. */
    private fun resetAuto() {
        focusRegion = null
        state.focusLocked = false
        state.afMode = ControlState.AF_CONTINUOUS
        state.exposureEvMilli = 0
        state.aeLock = false
        state.awbLock = false
    }

    /** A new stream (connect or reconnect): Auto, and default framing: no torch, 1x, centred. Lens stays. */
    private fun resetAll() {
        resetAuto()
        state.torch = false
        state.zoom100 = 100
        state.panX = PAN_CENTER
        state.panY = PAN_CENTER
    }

    private fun publishState() {
        onStateChanged?.invoke(state)
        if (handle == 0L) return
        val bm = context.getSystemService(BatteryManager::class.java)
        val pct = bm.getIntProperty(BatteryManager.BATTERY_PROPERTY_CAPACITY)
        val battery = intArrayOf(if (pct in 0..100) pct else 255, if (bm.isCharging) 1 else 0)
        LennyNative.sendControlState(handle, state.toArray() + battery + intArrayOf(state.panX, state.panY)) // not streaming -> ignored
    }

    /** Battery rides on CONTROL_STATE; resend it now and then so the PC's number stays fresh. */
    private val batteryTick = object : Runnable {
        override fun run() {
            publishState()
            main.postDelayed(this, BATTERY_PERIOD_MS)
        }
    }

    private fun updateOrientation() {
        if (!::lenses.isInitialized) return
        val sensor = lens.get(CameraCharacteristics.SENSOR_ORIENTATION) ?: 0
        val deg = if (lens.facing == FACING_FRONT) sensor - deviceDegrees + 360 else sensor + deviceDegrees
        orientation = deg % 360 / 90
    }

    // ---- camera + encoder -------------------------------------------------------------------------------------

    private fun bindCamera(s: Settings) {
        if (stopped) return
        val old = bound
        if (s == old && session != null) return // reconnect with the same settings: keep the running camera
        val l = lenses[s.lens.coerceIn(lenses.indices)]
        bound = s
        updateOrientation()
        if (old != null && session != null && s.copy(lens = 0) == old.copy(lens = 0) && lenses[old.lens].id == l.id) {
            applyRequest() // another sensor of the same logical camera: just a new zoom ratio
            return
        }
        closeSession()
        if (device?.id == l.id) return createSession()
        device?.close()
        device = null
        if (opening == l.id) return // onOpened creates the session
        opening = l.id
        try {
            manager.openCamera(l.id, object : CameraDevice.StateCallback() {
                override fun onOpened(d: CameraDevice) {
                    if (opening == d.id) opening = null
                    if (stopped || device != null || lens.id != d.id) return d.close() // superseded meanwhile
                    device = d
                    createSession()
                }

                override fun onDisconnected(d: CameraDevice) {
                    if (opening == d.id) opening = null
                    d.close()
                    if (device === d) device = null
                }

                override fun onError(d: CameraDevice, error: Int) {
                    Log.w(TAG, "camera ${d.id} error $error")
                    if (opening == d.id) opening = null
                    d.close()
                    if (device === d) device = null
                }
            }, main)
        } catch (e: Exception) {
            Log.w(TAG, "open camera ${l.id} failed", e)
            opening = null
        }
    }

    private fun createSession() {
        val d = device ?: return
        val s = bound ?: return
        val size = pickSize(lens, s.width, s.height)
        val codec = try {
            createEncoder(size.width, size.height, s.fps, s.bitrateKbps)
        } catch (e: Exception) {
            Log.w(TAG, "encoder setup failed", e)
            return
        }
        val surface = codec.createInputSurface()
        codec.start()
        encoder = codec
        outSize = size
        ptsCalibrated = false // new camera: its timestamp clock may differ (see calibratePts)
        // Tell the receiver when the camera can't do the size it asked for.
        if (size.width != s.width || size.height != s.height) {
            LennyNative.updateStream(handle, size.width, size.height, s.fps, s.bitrateKbps)
        }
        val released = AtomicBoolean(false)
        val release = Runnable {
            if (!released.compareAndSet(false, true)) return@Runnable
            if (encoder === codec) encoder = null
            runCatching { codec.stop() }
            codec.release()
            surface.release()
        }
        val releaseOnCodecThread = { if (!codecHandler.post(release)) release.run() } // thread gone after stop()
        val gen = ++sessionGen
        try {
            @Suppress("DEPRECATION") // the SessionConfiguration overload is API 28+
            d.createCaptureSession(listOf(surface), object : CameraCaptureSession.StateCallback() {
                override fun onConfigured(cs: CameraCaptureSession) {
                    if (gen != sessionGen || stopped) return cs.close()
                    session = cs
                    target = surface
                    applyRequest()
                }

                override fun onConfigureFailed(cs: CameraCaptureSession) {
                    Log.w(TAG, "capture session failed for ${d.id} ${size.width}x${size.height}")
                    releaseOnCodecThread()
                }

                override fun onClosed(cs: CameraCaptureSession) = releaseOnCodecThread()
            }, main)
        } catch (e: Exception) {
            Log.w(TAG, "capture session setup failed", e)
            releaseOnCodecThread()
        }
    }

    private fun closeSession() {
        sessionGen++
        session?.close()
        session = null
        target = null
    }

    /** The one repeating request, rebuilt from [state] after every change. */
    private fun applyRequest(trigger: Boolean = false) {
        val cs = session ?: return
        val out = target ?: return
        val l = lens
        try {
            val b = cs.device.createCaptureRequest(CameraDevice.TEMPLATE_RECORD)
            b.addTarget(out)
            // "Auto, always" defaults (prompt's quality bar): continuous video AF, auto exposure/ISO, auto WB,
            // anti-banding for indoor lights, and a fixed AE fps range so low light doesn't drop the frame rate.
            b.set(CaptureRequest.CONTROL_MODE, CameraMetadata.CONTROL_MODE_AUTO)
            b.set(CaptureRequest.CONTROL_AE_MODE, CameraMetadata.CONTROL_AE_MODE_ON)
            b.set(CaptureRequest.CONTROL_AWB_MODE, CameraMetadata.CONTROL_AWB_MODE_AUTO)
            b.set(CaptureRequest.CONTROL_AE_ANTIBANDING_MODE, CameraMetadata.CONTROL_AE_ANTIBANDING_MODE_AUTO)
            // Stabilisation looks frames ahead: latency we don't want in a webcam.
            b.set(CaptureRequest.CONTROL_VIDEO_STABILIZATION_MODE, CameraMetadata.CONTROL_VIDEO_STABILIZATION_MODE_OFF)
            fpsRange(l.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES), bound?.fps ?: 30)
                ?.let { b.set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, it) }
            b.set(CaptureRequest.CONTROL_AE_LOCK, state.aeLock)
            b.set(CaptureRequest.CONTROL_AWB_LOCK, state.awbLock)
            val step = l.get(CameraCharacteristics.CONTROL_AE_COMPENSATION_STEP)?.toFloat() ?: 0f
            if (step > 0f) b.set(CaptureRequest.CONTROL_AE_EXPOSURE_COMPENSATION, (state.exposureEvMilli / 1000f / step).roundToInt())
            b.set(CaptureRequest.FLASH_MODE, if (state.torch) CameraMetadata.FLASH_MODE_TORCH else CameraMetadata.FLASH_MODE_OFF)
            // Zoom and pan on the sensor readout, never on finished frames (protocol.md §6.9 "Pan").
            val r = readout(l)
            if (r.ratio != null && Build.VERSION.SDK_INT >= 30) b.set(CaptureRequest.CONTROL_ZOOM_RATIO, r.ratio)
            r.crop?.let { b.set(CaptureRequest.SCALER_CROP_REGION, it) }
            val region = focusRegion
            val afModes = l.get(CameraCharacteristics.CONTROL_AF_AVAILABLE_MODES) ?: IntArray(0)
            if (region != null) {
                b.set(CaptureRequest.CONTROL_AF_MODE, CameraMetadata.CONTROL_AF_MODE_AUTO)
                b.set(CaptureRequest.CONTROL_AF_REGIONS, arrayOf(region))
                b.set(CaptureRequest.CONTROL_AE_REGIONS, arrayOf(region))
            } else if (CameraMetadata.CONTROL_AF_MODE_CONTINUOUS_VIDEO in afModes) {
                b.set(CaptureRequest.CONTROL_AF_MODE, CameraMetadata.CONTROL_AF_MODE_CONTINUOUS_VIDEO)
            }
            cs.setRepeatingRequest(b.build(), afWatcher, main)
            if (trigger) {
                b.set(CaptureRequest.CONTROL_AF_TRIGGER, CameraMetadata.CONTROL_AF_TRIGGER_START)
                cs.capture(b.build(), null, main)
            }
        } catch (e: Exception) { // session closed under us: the next session applies [state] anyway
            Log.w(TAG, "capture request failed", e)
        }
    }

    /** Ends the "focusing" state once a tap-to-focus scan has settled. */
    private val afWatcher = object : CameraCaptureSession.CaptureCallback() {
        override fun onCaptureCompleted(cs: CameraCaptureSession, request: CaptureRequest, result: TotalCaptureResult) {
            if (state.afMode != ControlState.AF_FOCUSING) return
            val af = result.get(CaptureResult.CONTROL_AF_STATE)
            if (af == CaptureResult.CONTROL_AF_STATE_FOCUSED_LOCKED || af == CaptureResult.CONTROL_AF_STATE_NOT_FOCUSED_LOCKED) {
                state.afMode = if (state.focusLocked) ControlState.AF_LOCKED else ControlState.AF_CONTINUOUS
                publishState()
            }
        }
    }

    private fun effectiveZoom(l: Lens = lens): Float {
        val (lo, hi) = zoomRange(l)
        return (l.zoom * state.zoom100 / 100f).coerceIn(lo, hi)
    }

    /** CONTROL_ZOOM_RATIO (null = don't set) and SCALER_CROP_REGION (null = whole view) for the current zoom + pan. */
    private class Readout(val ratio: Float?, val crop: Rect?)

    /**
     * Zoom Z (1 = the camera's full active array) shows a W/Z x H/Z window; pan slides that window over the full
     * array. With CONTROL_ZOOM_RATIO (API 30+) the platform picks the physical sensor from the ratio, and the crop
     * region is relative to the zoomed view. So: ratio = Z while centred (pure platform zoom, sensor switching
     * included); when panned towards an edge, the ratio drops just enough that the zoomed view still contains the
     * window, and the crop selects the window inside it (a tele sensor can't see the array's edge, so near the edge
     * the platform may go back to the wider sensor). Before API 30 there's no ratio: the crop does everything,
     * digital zoom on one sensor, up to SCALER_AVAILABLE_MAX_DIGITAL_ZOOM.
     */
    private fun readout(l: Lens): Readout {
        val z = effectiveZoom(l)
        val ratioApi = usesZoomRatio(l)
        val ar = l.get(CameraCharacteristics.SENSOR_INFO_ACTIVE_ARRAY_SIZE) ?: return Readout(if (ratioApi) z else null, null)
        val (bx, by) = toBuffer(state.panX / 65535f, state.panY / 65535f)
        val (ratio, crop) = panCrop(ar.width(), ar.height(), z, bx, by, ratioApi)
        return Readout(ratio, crop?.let { Rect(it[0], it[1], it[2], it[3]) })
    }

    private fun createEncoder(width: Int, height: Int, fps: Int, bitrateKbps: Int): MediaCodec {
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, bitrateKbps * 1000)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 1) // short GOP: a lost decoder recovers within 1 s
            setInteger(MediaFormat.KEY_PRIORITY, 0) // realtime
            if (Build.VERSION.SDK_INT >= 29) setInteger(MediaFormat.KEY_MAX_B_FRAMES, 0)
            if (Build.VERSION.SDK_INT >= 30) setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        }
        val codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        codec.setCallback(object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(codec: MediaCodec, index: Int) = Unit // surface input

            override fun onOutputBufferAvailable(codec: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
                val buf = try { codec.getOutputBuffer(index) } catch (e: IllegalStateException) { return }
                if (buf != null && info.size > 0 && handle != 0L) {
                    if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
                        LennyNative.sendConfig(handle, buf, info.offset, info.size)
                    } else {
                        if (!ptsCalibrated) calibratePts(info.presentationTimeUs)
                        // A capture time in the future is impossible, so this camera's clock is off (the emulator's
                        // front camera). Checked per frame, not once: the first frame after a switch encodes slowest.
                        val now = LennyNative.nowUs()
                        if (info.presentationTimeUs - ptsOffsetUs > now) ptsOffsetUs = info.presentationTimeUs - now
                        val key = if (info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0) LennyNative.FRAME_KEYFRAME else 0
                        // Copied into the core's queue; never waits on the network. Not streaming -> dropped.
                        LennyNative.sendFrame(
                            handle, buf, info.offset, info.size, info.presentationTimeUs - ptsOffsetUs, orientation, key,
                        )
                    }
                }
                runCatching { codec.releaseOutputBuffer(index, false) }
            }

            override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
                Log.w(TAG, "encoder error", e)
            }
            override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) = Unit
        }, codecHandler)
        codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        return codec
    }

    /**
     * Camera timestamps are either MONOTONIC (the core's clock) or BOOTTIME, and SENSOR_INFO_TIMESTAMP_SOURCE isn't
     * reliable on every device (the emulator's front camera gets it wrong). So check the first frame: it was captured
     * a few ms ago, so whichever clock puts it closest to "now" is the right one.
     */
    private fun calibratePts(ptsUs: Long) {
        val now = LennyNative.nowUs()
        val bootOffset = (SystemClock.elapsedRealtimeNanos() - System.nanoTime()) / 1000
        val ageMono = now - ptsUs
        val ageBoot = now - (ptsUs - bootOffset)
        ptsOffsetUs = when {
            abs(ageMono) <= abs(ageBoot) && abs(ageMono) < 2_000_000 -> 0L
            abs(ageBoot) < 2_000_000 -> bootOffset
            else -> {
                // Some other time base entirely. Count from encoder output instead: latency then excludes capture and
                // encode time, but stays meaningful.
                Log.w(TAG, "unknown camera clock: pts=$ptsUs now=$now boot=$bootOffset")
                ptsUs - now
            }
        }
        ptsCalibrated = true
    }

    // ---- lenses + capabilities (CAPS) -------------------------------------------------------------------------

    /**
     * Every camera an app may open: cameraIdList plus the ids some phones hide from it (their ultrawide and tele),
     * each kept only if it really opens. A logical multi-camera turns into one lens per sensor, at the zoom ratio that
     * selects it; its sensors aren't listed again on their own. Order: back by zoom, then front, then external.
     */
    private fun discoverLenses(): List<Lens> {
        val listed = manager.cameraIdList.toSet()
        // ponytail: hidden ids probed as "0".."15", the usual numbering; widen if a phone uses other ids.
        val chars = (listed + (0..15).map { "$it" }).mapNotNull { id ->
            runCatching { id to manager.getCameraCharacteristics(id) }.getOrNull()
        }.filter { (id, c) ->
            c.get(CameraCharacteristics.REQUEST_AVAILABLE_CAPABILITIES)
                ?.contains(CameraMetadata.REQUEST_AVAILABLE_CAPABILITIES_BACKWARD_COMPATIBLE) == true &&
                (id in listed || canOpen(id))
        }.toMap()

        val out = mutableListOf<Lens>()
        val used = mutableSetOf<String>()
        if (Build.VERSION.SDK_INT >= 30) { // zoom ratios below 1x (the ultrawide) need CONTROL_ZOOM_RATIO
            val logical = chars.filter { (_, c) -> c.physicalCameraIds.size > 1 && zoomRatioRange(c) != null }
                .entries.sortedByDescending { it.value.physicalCameraIds.size }
            for ((id, c) in logical) {
                if (id in used || c.physicalCameraIds.any { it in used }) continue
                val range = zoomRatioRange(c)!!
                val base = equivalentFocal(c) ?: continue
                val stops = c.physicalCameraIds.mapNotNull { p ->
                    val pc = chars[p] ?: runCatching { manager.getCameraCharacteristics(p) }.getOrNull()
                    pc?.let(::equivalentFocal)?.let { it / base }
                }.map {
                    // The widest sensor is what the minimum zoom ratio shows; one decimal is what camera apps label.
                    val z = if (it < 0.95f) range.lower else it.coerceAtMost(range.upper)
                    (z * 10).roundToInt() / 10f
                }.distinct().sorted()
                if (stops.size < 2) continue
                stops.forEach { out += Lens(id, c, facingOf(c), it).apply { label = zoomLabel(it) } }
                used += id
                used += c.physicalCameraIds
            }
        }
        val rest = chars.filterKeys { it !in used }.map { (id, c) -> Lens(id, c, facingOf(c), 1f) }
            .sortedWith(compareBy({ it.facing }, { it.id !in listed }, { it.id }))
        val backs = out.size + rest.count { it.facing == FACING_BACK }
        val counts = IntArray(3)
        for (l in rest) {
            val n = ++counts[l.facing]
            l.label = when (l.facing) {
                FACING_BACK -> if (backs == 1) "Back" else "Back $n"
                FACING_FRONT -> if (n == 1) "Front" else "Front $n"
                else -> if (n == 1) "External" else "External $n"
            }
        }
        return (out + rest).sortedBy { it.facing }
    }

    /** Hidden cameras can be readable but not openable; find out now rather than when the user picks one. */
    private fun canOpen(id: String): Boolean {
        val done = CountDownLatch(1)
        var ok = false
        try {
            manager.openCamera(id, object : CameraDevice.StateCallback() {
                override fun onOpened(d: CameraDevice) { ok = true; d.close(); done.countDown() }
                override fun onDisconnected(d: CameraDevice) { d.close(); done.countDown() }
                override fun onError(d: CameraDevice, error: Int) { d.close(); done.countDown() }
            }, codecHandler)
        } catch (e: Exception) {
            return false
        }
        done.await(2, TimeUnit.SECONDS)
        return ok
    }

    /** Union over all lenses; per-lens gaps are handled when a control is applied. */
    private fun capabilities(): Int {
        var c = 0
        for (l in lenses) {
            if (l.get(CameraCharacteristics.CONTROL_AF_AVAILABLE_MODES)?.contains(CameraMetadata.CONTROL_AF_MODE_AUTO) == true) {
                c = c or LennyNative.CAP_FOCUS or LennyNative.CAP_FOCUS_LOCK
            }
            val range = l.get(CameraCharacteristics.CONTROL_AE_COMPENSATION_RANGE)
            if (range != null && range.upper > range.lower) c = c or LennyNative.CAP_EXPOSURE_COMP
            if (l.get(CameraCharacteristics.CONTROL_AE_LOCK_AVAILABLE) == true) c = c or LennyNative.CAP_EXPOSURE_LOCK
            if (l.get(CameraCharacteristics.CONTROL_AWB_LOCK_AVAILABLE) == true) c = c or LennyNative.CAP_WB_LOCK
            if (hasFlash(l)) c = c or LennyNative.CAP_TORCH
            if (zoomRange(l).second > 1f) c = c or LennyNative.CAP_ZOOM or LennyNative.CAP_PAN
        }
        if (lenses.size > 1) c = c or LennyNative.CAP_LENS
        return c
    }

    private fun exposureRange(): IntArray {
        val l = lenses[state.lens]
        val range = l.get(CameraCharacteristics.CONTROL_AE_COMPENSATION_RANGE) ?: return IntArray(0)
        val step = l.get(CameraCharacteristics.CONTROL_AE_COMPENSATION_STEP)?.toFloat() ?: return IntArray(0)
        if (range.upper <= range.lower) return IntArray(0)
        return intArrayOf(
            (range.lower * step * 1000).roundToInt(), (range.upper * step * 1000).roundToInt(), (step * 1000).roundToInt(),
        )
    }

    companion object {
        private const val TAG = "lenny"
        private const val BATTERY_PERIOD_MS = 60_000L
        private const val MAX_BITRATE_KBPS = 20000
        private const val PAN_CENTER = 32768
        private val FALLBACK_MODES = intArrayOf(1280, 720, 30, 1)
        private const val MAX_MODES = 32 // what a receiver keeps per lens (LENNY_MAX_PEER_MODES)

        /**
         * What [l] can stream (w, h, fps, 1 flattened), straight from the camera and the encoder, nothing assumed:
         * every size the camera outputs to MediaCodec (>= 240 lines), at every frame rate the camera can hold there
         * (an AE target range topping out exactly at it, 15..60, and a short enough minimum frame duration) that an
         * AVC encoder accepts. Per aspect ratio the 4 largest sizes, 4 aspect ratios at most (by largest size), so
         * the list fits a receiver's picker; biggest first.
         */
        private fun supportedModes(l: Lens): IntArray {
            val map = l.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP) ?: return FALLBACK_MODES
            val aeFps = l.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES).orEmpty()
                .map { it.upper }.filter { it in 15..60 }.distinct().sorted()
            val encoders = android.media.MediaCodecList(android.media.MediaCodecList.REGULAR_CODECS).codecInfos
                .filter { it.isEncoder && MediaFormat.MIMETYPE_VIDEO_AVC in it.supportedTypes }
                .mapNotNull { runCatching { it.getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC).videoCapabilities }.getOrNull() }
            val sizes = map.getOutputSizes(MediaCodec::class.java).orEmpty()
                .filter { it.height >= 240 && encoders.any { e -> e.isSizeSupported(it.width, it.height) } }
                .sortedByDescending { it.width * it.height }
                .groupBy { aspectKey(it.width, it.height) }.values.take(4).flatMap { it.take(4) }
                .sortedByDescending { it.width * it.height }
            val modes = mutableListOf<Int>()
            for (size in sizes) {
                val minFrameNs = map.getOutputMinFrameDuration(MediaCodec::class.java, size)
                for (fps in aeFps) {
                    if (minFrameNs > 0 && minFrameNs > 1_000_000_000L / fps) continue
                    if (encoders.none { it.areSizeAndRateSupported(size.width, size.height, fps.toDouble()) }) continue
                    if (modes.size < MAX_MODES * 4) modes += listOf(size.width, size.height, fps, 1)
                }
            }
            Log.i(TAG, "lens ${l.label} (${l.id}) modes: " + modes.chunked(4).joinToString { "${it[0]}x${it[1]}@${it[2]}" })
            return if (modes.isEmpty()) FALLBACK_MODES else modes.toIntArray()
        }

        /**
         * [readout] without Android types: array W x H, zoom [z] (1 = whole array), window centre at (bx, by) in 0..1
         * of the pannable range, buffer axes. Returns (CONTROL_ZOOM_RATIO or null, crop [l, t, r, b] or null).
         */
        internal fun panCrop(W: Int, H: Int, z: Float, bx: Float, by: Float, ratioApi: Boolean): Pair<Float?, IntArray?> {
            if (z <= 1f) return (if (ratioApi) z else null) to null
            val (fw, fh) = W.toFloat() to H.toFloat()
            val (w, h) = fw / z to fh / z
            val cx = fw / 2 + (bx - 0.5f) * (fw - w)
            val cy = fh / 2 + (by - 0.5f) * (fh - h)
            if (!ratioApi) {
                return null to intArrayOf((cx - w / 2).roundToInt(), (cy - h / 2).roundToInt(), (cx + w / 2).roundToInt(), (cy + h / 2).roundToInt())
            }
            val zr = minOf(z, fw / (2 * abs(cx - fw / 2) + w), fh / (2 * abs(cy - fh / 2) + h)).coerceAtLeast(1f)
            if (zr >= z * 0.999f) return z to null // centred: pure platform zoom
            // Active-array point -> post-zoom coordinates (the zoomed view scaled back to the array's size).
            fun mx(x: Float) = ((x - (fw - fw / zr) / 2) * zr).roundToInt().coerceIn(0, W)
            fun my(y: Float) = ((y - (fh - fh / zr) / 2) * zr).roundToInt().coerceIn(0, H)
            return zr to intArrayOf(mx(cx - w / 2), my(cy - h / 2), mx(cx + w / 2), my(cy + h / 2))
        }

        /** Width:height reduced, so 1920x1080 and 1280x720 group together. */
        internal fun aspectKey(w: Int, h: Int): Pair<Int, Int> {
            tailrec fun gcd(a: Int, b: Int): Int = if (b == 0) a else gcd(b, a % b)
            val g = gcd(w, h)
            return w / g to h / g
        }

        /** The mode in [modes] (flat w, h, fps, 1) nearest the request: same aspect first, then area, then fps. */
        internal fun nearestMode(modes: IntArray, w: Int, h: Int, fps: Int): IntArray {
            val list = modes.toList().chunked(4)
            val best = list.minByOrNull { m ->
                val aspect = if (m[0].toLong() * h == m[1].toLong() * w) 0L else 1L
                // Lexicographic: aspect, then area difference, then fps difference.
                aspect * 1_000_000_000_000L + abs(m[0].toLong() * m[1] - w.toLong() * h) * 1000 + abs(m[2] - fps)
            } ?: return intArrayOf(w, h, fps, 1)
            return best.toIntArray()
        }
        private const val FACING_BACK = 0
        private const val FACING_FRONT = 1

        private fun facingOf(c: CameraCharacteristics) = when (c.get(CameraCharacteristics.LENS_FACING)) {
            CameraMetadata.LENS_FACING_BACK -> FACING_BACK
            CameraMetadata.LENS_FACING_FRONT -> FACING_FRONT
            else -> 2
        }

        private fun hasFlash(l: Lens) = l.get(CameraCharacteristics.FLASH_INFO_AVAILABLE) == true

        private fun zoomRatioRange(c: CameraCharacteristics): Range<Float>? =
            if (Build.VERSION.SDK_INT >= 30) c.get(CameraCharacteristics.CONTROL_ZOOM_RATIO_RANGE) else null

        private fun usesZoomRatio(l: Lens) = zoomRatioRange(l.chars) != null

        private fun zoomRange(l: Lens): Pair<Float, Float> = zoomRatioRange(l.chars)?.let { it.lower to it.upper }
            ?: (1f to (l.get(CameraCharacteristics.SCALER_AVAILABLE_MAX_DIGITAL_ZOOM) ?: 1f))

        /** 35 mm-equivalent focal length: comparable across sensors of different sizes. */
        private fun equivalentFocal(c: CameraCharacteristics): Float? {
            val f = c.get(CameraCharacteristics.LENS_INFO_AVAILABLE_FOCAL_LENGTHS)?.firstOrNull() ?: return null
            val s = c.get(CameraCharacteristics.SENSOR_INFO_PHYSICAL_SIZE) ?: return null
            return f * 43.27f / hypot(s.width, s.height)
        }

        private fun zoomLabel(z: Float) = if (z == z.toInt().toFloat()) "${z.toInt()}×" else "$z×"

        /** The camera's size nearest the request: exact, else same aspect ratio, else nearest area. */
        private fun pickSize(l: Lens, w: Int, h: Int): Size {
            val sizes = l.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
                ?.getOutputSizes(MediaCodec::class.java).orEmpty()
            return sizes.firstOrNull { it.width == w && it.height == h }
                ?: sizes.filter { it.width * h == it.height * w }.minByOrNull { abs(it.width * it.height - w * h) }
                ?: sizes.minByOrNull { abs(it.width * it.height - w * h) }
                ?: Size(w, h)
        }

        /** Exact [fps, fps] if the camera has it, else the narrowest range that still reaches fps. */
        private fun fpsRange(ranges: Array<Range<Int>>?, fps: Int): Range<Int>? =
            ranges?.firstOrNull { it.lower == fps && it.upper == fps }
                ?: ranges?.filter { it.upper >= fps }?.minByOrNull { it.upper - it.lower }

        /** Random 16 bytes, persisted per install (HELLO device_id; how the desktop remembers this phone). */
        fun deviceId(context: Context): ByteArray {
            val prefs = context.getSharedPreferences("lenny", Context.MODE_PRIVATE)
            prefs.getString("device_id", null)?.let { return Base64.decode(it, Base64.NO_WRAP) }
            val id = ByteArray(16).also { SecureRandom().nextBytes(it) }
            prefs.edit().putString("device_id", Base64.encodeToString(id, Base64.NO_WRAP)).apply()
            return id
        }
    }
}

/** Mirrors lenny_control_state, plus what the phone needs to remember. Main thread only. */
data class ControlState(
    var afMode: Int = AF_CONTINUOUS,
    var focusLocked: Boolean = false,
    var focusX: Float = 0.5f,
    var focusY: Float = 0.5f,
    var exposureEvMilli: Int = 0,
    var aeLock: Boolean = false,
    var awbLock: Boolean = false,
    var torch: Boolean = false,
    var lens: Int = 0,
    var zoom100: Int = 100,
    /** Where the zoomed crop sits, 0..65535 per axis, 32768 = centred (protocol 1.1 pan). */
    var panX: Int = 32768,
    var panY: Int = 32768,
) {
    /** lenny_control_state order, minus battery/charging (added by the caller), plus pan at the end. */
    fun toArray() = intArrayOf(
        afMode, exposureEvMilli, if (aeLock) 1 else 0, if (awbLock) 1 else 0, if (torch) 1 else 0, lens, zoom100,
    )

    fun toMap(): Map<String, Any> = mapOf(
        "afMode" to afMode, "focusLocked" to focusLocked, "exposureEvMilli" to exposureEvMilli, "aeLock" to aeLock,
        "awbLock" to awbLock, "torch" to torch, "lens" to lens, "zoom100" to zoom100, "panX" to panX, "panY" to panY,
    )

    companion object {
        const val AF_CONTINUOUS = 0
        const val AF_LOCKED = 1
        const val AF_FOCUSING = 2
    }
}
