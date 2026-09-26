// JNI bridge: Kotlin (LennyNative) <-> lenny_core C ABI. Encoded frames go straight from MediaCodec's direct
// ByteBuffer into the core; nothing is copied into the JVM heap.
#include <android/log.h>
#include <jni.h>

#include <vector>

#include "lenny/lenny.h"

#define LOGW(...) __android_log_print(ANDROID_LOG_WARN, "lenny", __VA_ARGS__)

namespace {

JavaVM* g_vm = nullptr;

// Core callbacks arrive on the core's I/O thread, which the JVM doesn't know. Attach it once and detach when the
// thread exits (a thread_local destructor), otherwise the thread leaks a JNIEnv or aborts on exit.
JNIEnv* env_for_thread() {
    JNIEnv* env = nullptr;
    if (g_vm->GetEnv(reinterpret_cast<void**>(&env), JNI_VERSION_1_6) == JNI_OK) return env;
    thread_local struct Detacher {
        bool attached = false;
        ~Detacher() {
            if (attached) g_vm->DetachCurrentThread();
        }
    } detacher;
    if (g_vm->AttachCurrentThread(&env, nullptr) != JNI_OK) return nullptr;
    detacher.attached = true;
    return env;
}

// A Kotlin exception must never unwind into the core; log and fall back to a safe answer.
bool clear_exception(JNIEnv* env, const char* where) {
    if (!env->ExceptionCheck()) return false;
    LOGW("exception in %s", where);
    env->ExceptionDescribe();
    env->ExceptionClear();
    return true;
}

struct Ctx {
    lenny_session* session = nullptr;
    jobject listener = nullptr;  // global ref to SenderListener
    jmethodID on_stream_config = nullptr;
    jmethodID on_control = nullptr;
    jmethodID on_bitrate = nullptr;
};

void on_stream_config(void* user, const lenny_stream_settings* req, lenny_stream_settings* eff) {
    auto* ctx = static_cast<Ctx*>(user);
    JNIEnv* env = env_for_thread();
    if (!env) return;
    auto arr = static_cast<jintArray>(env->CallObjectMethod(ctx->listener, ctx->on_stream_config, jint(req->mode.width),
                                                            jint(req->mode.height), jint(req->mode.fps_num),
                                                            jint(req->mode.fps_den), jint(req->bitrate_kbps)));
    if (clear_exception(env, "onStreamConfig") || !arr) return;
    if (env->GetArrayLength(arr) == 5) {
        jint v[5];
        env->GetIntArrayRegion(arr, 0, 5, v);
        eff->mode = {uint16_t(v[0]), uint16_t(v[1]), uint16_t(v[2]), uint16_t(v[3])};
        eff->bitrate_kbps = uint32_t(v[4]);
    }
    env->DeleteLocalRef(arr);
}

int32_t on_control(void* user, const lenny_control* c) {
    auto* ctx = static_cast<Ctx*>(user);
    JNIEnv* env = env_for_thread();
    if (!env) return LENNY_ACK_FAILED;
    jint r = env->CallIntMethod(ctx->listener, ctx->on_control, jint(c->cmd), jint(c->x), jint(c->y), jint(c->value));
    return clear_exception(env, "onControl") ? LENNY_ACK_FAILED : r;
}

void on_bitrate(void* user, uint32_t kbps) {
    auto* ctx = static_cast<Ctx*>(user);
    JNIEnv* env = env_for_thread();
    if (!env) return;
    env->CallVoidMethod(ctx->listener, ctx->on_bitrate, jint(kbps));
    clear_exception(env, "onBitrate");
}

Ctx* ctx_of(jlong h) { return reinterpret_cast<Ctx*>(h); }

const uint8_t* direct(JNIEnv* env, jobject buf, jint offset) {
    auto* p = static_cast<uint8_t*>(env->GetDirectBufferAddress(buf));
    return p ? p + offset : nullptr;
}

}  // namespace

extern "C" {

JNIEXPORT jint JNI_OnLoad(JavaVM* vm, void*) {
    g_vm = vm;
    return JNI_VERSION_1_6;
}

// lenses = flat [id, facing, ...] with labels[i] for each; exposure = [min, max, step_milli] (EV*1000) or empty;
// lens_caps = per lens [zoom_min, zoom_max, zoom_base, mode_count, w, h, fps_num, fps_den, ...] (lenny_lens_caps).
JNIEXPORT jlong JNICALL Java_com_spizganed_android_1camera_LennyNative_create(
    JNIEnv* env, jclass, jbyteArray device_id, jstring name, jintArray modes, jint max_bitrate, jint controls,
    jintArray lenses, jobjectArray lens_labels, jintArray exposure, jintArray lens_caps, jobject listener) {
    if (!device_id || env->GetArrayLength(device_id) != LENNY_DEVICE_ID_SIZE || !name || !modes || !listener ||
        !lenses || !lens_labels || !exposure || !lens_caps)
        return 0;
    auto* ctx = new Ctx;
    jclass cls = env->GetObjectClass(listener);
    ctx->on_stream_config = env->GetMethodID(cls, "onStreamConfig", "(IIIII)[I");
    ctx->on_control = env->GetMethodID(cls, "onControl", "(IIII)I");
    ctx->on_bitrate = env->GetMethodID(cls, "onBitrate", "(I)V");
    if (!ctx->on_stream_config || !ctx->on_control || !ctx->on_bitrate) {
        clear_exception(env, "create");
        delete ctx;
        return 0;
    }
    ctx->listener = env->NewGlobalRef(listener);

    lenny_sender_config cfg{};
    env->GetByteArrayRegion(device_id, 0, LENNY_DEVICE_ID_SIZE, reinterpret_cast<jbyte*>(cfg.identity.device_id));
    const char* cname = env->GetStringUTFChars(name, nullptr);
    cfg.identity.device_name = cname;
    cfg.identity.platform = LENNY_PLATFORM_ANDROID;
    std::vector<lenny_mode> m;
    const jsize n = env->GetArrayLength(modes) / 4;
    jint* mv = env->GetIntArrayElements(modes, nullptr);
    for (jsize i = 0; i < n; ++i)
        m.push_back({uint16_t(mv[4 * i]), uint16_t(mv[4 * i + 1]), uint16_t(mv[4 * i + 2]), uint16_t(mv[4 * i + 3])});
    env->ReleaseIntArrayElements(modes, mv, JNI_ABORT);
    cfg.modes = m.data();
    cfg.mode_count = m.size();
    cfg.max_bitrate_kbps = uint32_t(max_bitrate);
    cfg.controls = uint32_t(controls);

    // Lens labels: keep the UTF-8 copies alive until lenny_sender_create has copied them.
    std::vector<lenny_lens> lens_list;
    std::vector<std::pair<jstring, const char*>> label_refs;
    const jsize nl = env->GetArrayLength(lenses) / 2;
    jint* lv = env->GetIntArrayElements(lenses, nullptr);
    for (jsize i = 0; i < nl && i < env->GetArrayLength(lens_labels); ++i) {
        auto js = static_cast<jstring>(env->GetObjectArrayElement(lens_labels, i));
        const char* label = js ? env->GetStringUTFChars(js, nullptr) : "";
        label_refs.emplace_back(js, label);
        lens_list.push_back({uint8_t(lv[2 * i]), uint8_t(lv[2 * i + 1]), label});
    }
    env->ReleaseIntArrayElements(lenses, lv, JNI_ABORT);
    cfg.lenses = lens_list.data();
    cfg.lens_count = lens_list.size();

    // Per-lens caps. Mode storage is sized up front so the pointers in lc stay valid.
    std::vector<jint> cv(size_t(env->GetArrayLength(lens_caps)));
    env->GetIntArrayRegion(lens_caps, 0, jsize(cv.size()), cv.data());
    std::vector<lenny_mode> lens_modes(cv.size() / 4 + 1);
    std::vector<lenny_lens_caps> lc;
    size_t at = 0, used = 0;
    for (size_t i = 0; i < lens_list.size() && at + 4 <= cv.size(); ++i) {
        const size_t count = size_t(cv[at + 3]);
        if (count > (cv.size() - at - 4) / 4) break;  // malformed: stop, and send no per-lens caps at all
        lenny_lens_caps c{lens_modes.data() + used, count, uint16_t(cv[at]), uint16_t(cv[at + 1]), uint16_t(cv[at + 2])};
        for (size_t k = 0; k < count; ++k) {
            const jint* m = &cv[at + 4 + 4 * k];
            lens_modes[used++] = {uint16_t(m[0]), uint16_t(m[1]), uint16_t(m[2]), uint16_t(m[3])};
        }
        lc.push_back(c);
        at += 4 + 4 * count;
    }
    if (lc.size() == lens_list.size()) cfg.lens_caps = lc.data();
    if (env->GetArrayLength(exposure) == 3) {
        jint ev[3];
        env->GetIntArrayRegion(exposure, 0, 3, ev);
        cfg.exposure_comp_min = ev[0];
        cfg.exposure_comp_max = ev[1];
        cfg.exposure_comp_step_milli = uint32_t(ev[2]);
    }

    lenny_sender_callbacks cb{};
    cb.user = ctx;
    cb.on_stream_config = on_stream_config;
    cb.on_control = on_control;
    cb.on_bitrate = on_bitrate;
    ctx->session = lenny_sender_create(&cfg, &cb);  // copies everything it needs
    env->ReleaseStringUTFChars(name, cname);
    for (auto& [js, label] : label_refs) {
        if (!js) continue;
        env->ReleaseStringUTFChars(js, label);
        env->DeleteLocalRef(js);
    }
    if (!ctx->session) {
        env->DeleteGlobalRef(ctx->listener);
        delete ctx;
        return 0;
    }
    return reinterpret_cast<jlong>(ctx);
}

JNIEXPORT jlong JNICALL Java_com_spizganed_android_1camera_LennyNative_sessionPtr(JNIEnv*, jclass, jlong h) {
    return h ? reinterpret_cast<jlong>(ctx_of(h)->session) : 0;
}

JNIEXPORT jint JNICALL Java_com_spizganed_android_1camera_LennyNative_connect(JNIEnv* env, jclass, jlong h, jstring host,
                                                                             jint port, jbyteArray token) {
    if (!h || !host) return LENNY_E_INVALID_ARG;
    uint8_t tok[LENNY_PAIR_TOKEN_SIZE];
    const bool has_token = token && env->GetArrayLength(token) == LENNY_PAIR_TOKEN_SIZE;
    if (has_token) env->GetByteArrayRegion(token, 0, LENNY_PAIR_TOKEN_SIZE, reinterpret_cast<jbyte*>(tok));
    const char* chost = env->GetStringUTFChars(host, nullptr);
    jint r = lenny_sender_connect(ctx_of(h)->session, chost, uint16_t(port), has_token ? tok : nullptr);
    env->ReleaseStringUTFChars(host, chost);
    return r;
}

JNIEXPORT jint JNICALL Java_com_spizganed_android_1camera_LennyNative_sendConfig(JNIEnv* env, jclass, jlong h,
                                                                                jobject buf, jint offset, jint size) {
    const uint8_t* p = h && buf ? direct(env, buf, offset) : nullptr;
    return p ? lenny_sender_send_video_config(ctx_of(h)->session, p, size_t(size)) : LENNY_E_INVALID_ARG;
}

JNIEXPORT jint JNICALL Java_com_spizganed_android_1camera_LennyNative_sendFrame(JNIEnv* env, jclass, jlong h, jobject buf,
                                                                               jint offset, jint size, jlong pts_us,
                                                                               jint orientation, jint flags) {
    const uint8_t* p = h && buf ? direct(env, buf, offset) : nullptr;
    return p ? lenny_sender_send_video_frame(ctx_of(h)->session, p, size_t(size), pts_us, uint8_t(orientation),
                                             uint8_t(flags))
             : LENNY_E_INVALID_ARG;
}

JNIEXPORT jint JNICALL Java_com_spizganed_android_1camera_LennyNative_updateStream(JNIEnv*, jclass, jlong h, jint w,
                                                                                  jint height, jint fps,
                                                                                  jint bitrate) {
    if (!h) return LENNY_E_INVALID_ARG;
    lenny_stream_settings s{LENNY_CODEC_H264, {uint16_t(w), uint16_t(height), uint16_t(fps), 1}, uint32_t(bitrate), 0,
                            0};
    return lenny_sender_update_stream(ctx_of(h)->session, &s);
}

// state = [afMode, exposureComp, exposureLock, wbLock, torch, lensId, zoom, battery, charging, panX, panY]
JNIEXPORT jint JNICALL Java_com_spizganed_android_1camera_LennyNative_sendControlState(JNIEnv* env, jclass, jlong h,
                                                                                      jintArray state) {
    if (!h || !state || env->GetArrayLength(state) != 11) return LENNY_E_INVALID_ARG;
    jint v[11];
    env->GetIntArrayRegion(state, 0, 11, v);
    lenny_control_state s{uint8_t(v[0]), v[1],           uint8_t(v[2]),  uint8_t(v[3]),   uint8_t(v[4]),   uint8_t(v[5]),
                          uint16_t(v[6]), uint8_t(v[7]), uint8_t(v[8]), uint16_t(v[9]), uint16_t(v[10])};
    return lenny_sender_send_control_state(ctx_of(h)->session, &s);
}

JNIEXPORT jint JNICALL Java_com_spizganed_android_1camera_LennyNative_disconnect(JNIEnv*, jclass, jlong h) {
    return h ? lenny_session_disconnect(ctx_of(h)->session) : LENNY_E_INVALID_ARG;
}

JNIEXPORT void JNICALL Java_com_spizganed_android_1camera_LennyNative_destroy(JNIEnv* env, jclass, jlong h) {
    if (!h) return;
    Ctx* ctx = ctx_of(h);
    lenny_session_destroy(ctx->session);  // joins the I/O thread, so no callback can use ctx after this
    env->DeleteGlobalRef(ctx->listener);
    delete ctx;
}

JNIEXPORT jlong JNICALL Java_com_spizganed_android_1camera_LennyNative_nowUs(JNIEnv*, jclass) { return lenny_now_us(); }

}  // extern "C"
