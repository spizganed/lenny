/* Lenny core — stable C ABI.
 *
 * Called from Dart (dart:ffi, control only), Kotlin (JNI) and native desktop plugins (media path).
 * Rules: opaque handles, plain structs, no exceptions across the boundary, additive changes only
 * within an ABI major version (see lenny_abi_version).
 *
 * Threading: every callback runs on the session's I/O thread. Don't block in it; copy what you need
 * and return. All functions are safe to call from any thread, except lenny_session_destroy, which
 * must not be called from inside a callback.
 */
#ifndef LENNY_H
#define LENNY_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32)
#  if defined(LENNY_BUILD_SHARED)
#    define LENNY_API __declspec(dllexport)
#  elif defined(LENNY_USE_SHARED)
#    define LENNY_API __declspec(dllimport)
#  else
#    define LENNY_API
#  endif
#else
#  define LENNY_API __attribute__((visibility("default")))
#endif

#define LENNY_ABI_VERSION_MAJOR 1
#define LENNY_ABI_VERSION_MINOR 3
#define LENNY_DEFAULT_PORT 47474
#define LENNY_DEVICE_ID_SIZE 16
#define LENNY_PAIR_TOKEN_SIZE 16

/* ---- Results ---------------------------------------------------------- */
enum {
    LENNY_OK = 0,
    LENNY_E_INVALID_ARG = -1,
    LENNY_E_STATE = -2,    /* not allowed in the current session state (e.g. frame while not streaming) */
    LENNY_E_IO = -3,       /* socket failed */
    LENNY_E_INTERNAL = -4  /* bug or allocation failure; logged, never thrown */
};

/* ---- Session state (reported via on_state) ----------------------------
 * Enum types never cross the ABI (their size is compiler-defined); functions and callbacks use int32_t. */
typedef enum {
    LENNY_STATE_IDLE = 0,
    LENNY_STATE_CONNECTING = 1,        /* sender: dialing. receiver: listening */
    LENNY_STATE_HANDSHAKE = 2,         /* HELLO / PAIR / CAPS in progress */
    LENNY_STATE_AWAITING_APPROVAL = 3, /* receiver asked the user to accept an unknown phone */
    LENNY_STATE_STREAMING = 4,
    LENNY_STATE_RECONNECTING = 5,      /* sender: link lost, retrying with backoff */
    LENNY_STATE_CLOSED = 6             /* stopped for good; see reason */
} lenny_state;

/* GOODBYE reasons (protocol.md §6.2), also used as on_state reason. */
enum {
    LENNY_REASON_NORMAL = 0,
    LENNY_REASON_VERSION = 1,
    LENNY_REASON_ROLE = 2,
    LENNY_REASON_PAIR_DENIED = 3,
    LENNY_REASON_BUSY = 4,
    LENNY_REASON_TIMEOUT = 5,
    LENNY_REASON_PROTOCOL_ERROR = 6,
    LENNY_REASON_USER = 7,
    LENNY_REASON_LINK_LOST = 100 /* local only, never sent: socket error / keepalive silence */
};

/* PAIR_RESULT codes (protocol.md §6.4). */
enum {
    LENNY_PAIR_OK = 0,
    LENNY_PAIR_UNKNOWN_TOKEN = 1,
    LENNY_PAIR_EXPIRED = 2,
    LENNY_PAIR_ALREADY_USED = 3,
    LENNY_PAIR_DENIED = 4
};

enum { LENNY_PLATFORM_ANDROID = 1, LENNY_PLATFORM_IOS = 2, LENNY_PLATFORM_WINDOWS = 3, LENNY_PLATFORM_LINUX = 5 };
enum { LENNY_CODEC_H264 = 1 };

/* ---- Stream settings (CAPS / CAPS_SELECT / STREAM_START) --------------- */
typedef struct {
    uint16_t width;
    uint16_t height;
    uint16_t fps_num;
    uint16_t fps_den;
} lenny_mode;

typedef struct {
    uint8_t codec;         /* LENNY_CODEC_* */
    lenny_mode mode;
    uint32_t bitrate_kbps;
    uint8_t has_lens;      /* lens_id valid */
    uint8_t lens_id;
} lenny_stream_settings;

typedef struct {
    uint8_t lens_id;
    uint8_t facing;        /* 0 back, 1 front, 2 external */
    const char* label;     /* UTF-8 */
} lenny_lens;

/* ---- Camera controls (protocol.md §6.9) ------------------------------ */
typedef enum {
    LENNY_CTL_KEYFRAME_REQUEST = 10,
    LENNY_CTL_FOCUS_AT = 11,       /* x, y */
    LENNY_CTL_FOCUS_LOCK = 12,     /* value bool */
    LENNY_CTL_FOCUS_AUTO = 13,
    LENNY_CTL_EXPOSURE_COMP = 14,  /* value EV*1000 */
    LENNY_CTL_EXPOSURE_LOCK = 15,  /* value bool */
    LENNY_CTL_WB_LOCK = 16,        /* value bool */
    LENNY_CTL_TORCH = 17,          /* value bool */
    LENNY_CTL_SELECT_LENS = 18,    /* value lens_id */
    LENNY_CTL_ZOOM = 19,           /* value ratio*100 */
    LENNY_CTL_RESET_AUTO = 20,
    LENNY_CTL_PAN = 21             /* ABI 1.3, protocol 1.1: x, y = crop centre, 0..65535 across the pannable range */
} lenny_control_cmd;

/* CAPS controls bitmask bits. */
enum {
    LENNY_CAP_FOCUS = 1u << 0,
    LENNY_CAP_FOCUS_LOCK = 1u << 1,
    LENNY_CAP_EXPOSURE_COMP = 1u << 2,
    LENNY_CAP_EXPOSURE_LOCK = 1u << 3,
    LENNY_CAP_WB_LOCK = 1u << 4,
    LENNY_CAP_TORCH = 1u << 5,
    LENNY_CAP_LENS = 1u << 6,
    LENNY_CAP_ZOOM = 1u << 7,
    LENNY_CAP_PAN = 1u << 8        /* ABI 1.3 */
};

enum { LENNY_ACK_OK = 0, LENNY_ACK_UNSUPPORTED = 1, LENNY_ACK_FAILED = 2, LENNY_ACK_BUSY = 3 };

typedef struct {
    uint32_t req_id;  /* filled in by the core when sending */
    uint16_t cmd;     /* lenny_control_cmd */
    uint16_t x, y;    /* FOCUS_AT: normalized 0..65535, upright image coords */
    int32_t value;    /* see lenny_control_cmd */
} lenny_control;

typedef struct {
    uint8_t af_mode;        /* 0 continuous, 1 locked, 2 focusing */
    int32_t exposure_comp;  /* EV*1000 */
    uint8_t exposure_lock;
    uint8_t wb_lock;
    uint8_t torch;
    uint8_t lens_id;
    uint16_t zoom;          /* ratio*100 */
    uint8_t battery;        /* phone battery percent 0..100, 255 unknown (ABI 1.2) */
    uint8_t charging;       /* bool (ABI 1.2) */
    uint16_t pan_x, pan_y;  /* crop centre, 0..65535 across the pannable range, 32768 = centred (ABI 1.3) */
} lenny_control_state;

/* STREAM_STATUS states. */
enum { LENNY_STREAM_LIVE = 0, LENNY_STREAM_PAUSED = 1, LENNY_STREAM_CAMERA_LOST = 2, LENNY_STREAM_THERMAL = 3 };

/* VIDEO_FRAME flags. */
enum { LENNY_FRAME_KEYFRAME = 1u << 0, LENNY_FRAME_MIRROR = 1u << 1 };

typedef struct {
    uint32_t frame_seq;
    int64_t pts_us;          /* sender monotonic clock */
    int64_t local_pts_us;    /* receiver: pts_us mapped to local monotonic clock via clock sync (0 if unknown) */
    uint8_t orientation;     /* 0..3 = 0/90/180/270 deg clockwise to upright */
    uint8_t flags;           /* LENNY_FRAME_* */
    const uint8_t* data;     /* Annex-B access unit; valid only during the callback */
    size_t size;
} lenny_video_frame;

typedef struct {
    int64_t rtt_us;            /* best recent RTT, -1 if unknown */
    int64_t clock_offset_us;   /* remote_clock - local_clock */
    uint64_t frames;           /* video frames sent or received */
    uint64_t bytes;            /* video payload bytes sent or received */
    uint32_t bad_messages;     /* malformed control messages ignored */
    uint32_t reconnects;
    int64_t latency_us;        /* receiver: capture -> received, smoothed (needs clock sync); -1 if unknown */
    uint32_t dropped_frames;   /* sender: frames dropped because the network fell behind */
    uint32_t bitrate_kbps;     /* sender: current encoder target after congestion control */
} lenny_stats;

/* Local identity, sent in HELLO. */
typedef struct {
    uint8_t device_id[LENNY_DEVICE_ID_SIZE]; /* random, persisted per install by the app */
    const char* device_name;                 /* UTF-8 */
    const char* app_version;                 /* UTF-8, may be NULL */
    uint8_t platform;                        /* LENNY_PLATFORM_* */
} lenny_identity;

typedef struct lenny_session lenny_session;

LENNY_API uint32_t lenny_abi_version(void); /* (major << 16) | minor */
/* The core's monotonic clock in µs. Frame pts_us must be in this clock. Camera timestamps often use another
 * clock (Android SENSOR_TIMESTAMP may be BOOTTIME), so convert: pts = cam_ts - (cam_clock_now - lenny_now_us()). */
LENNY_API int64_t lenny_now_us(void);

/* ---- Sender (phone) --------------------------------------------------- */
/* ABI 1.3 (protocol 1.1): what one lens can do, sent in CAPS. A receiver only offers modes the selected lens has. */
typedef struct {
    const lenny_mode* modes;   /* modes this lens can stream; mode_count 0 = the config-level modes */
    size_t mode_count;
    uint16_t zoom_min, zoom_max; /* LENNY_CTL_ZOOM range for this lens, ratio*100 relative to it; 0 = unknown */
    uint16_t zoom_base;          /* this lens's zoom ratio*100 on its camera (e.g. 200 for a 2x tele), 0 = 100 */
} lenny_lens_caps;

typedef struct {
    lenny_identity identity;
    const lenny_mode* modes;   /* supported modes, copied */
    size_t mode_count;
    uint32_t max_bitrate_kbps;
    uint32_t controls;         /* LENNY_CAP_* */
    const lenny_lens* lenses;  /* copied */
    size_t lens_count;
    int32_t exposure_comp_min, exposure_comp_max; /* EV*1000 */
    uint32_t exposure_comp_step_milli;
    /* ABI 1.3: per-lens capabilities, parallel to `lenses` (lens_count entries, copied), or NULL. */
    const lenny_lens_caps* lens_caps;
} lenny_sender_config;

typedef struct {
    void* user;
    void (*on_state)(void* user, int32_t state /* lenny_state */, int32_t reason);
    /* Receiver asked for settings. `effective` is prefilled with `requested`; the platform clamps it to what the
     * camera/encoder can do and reconfigures. Core then sends STREAM_START with `effective`. */
    void (*on_stream_config)(void* user, const lenny_stream_settings* requested, lenny_stream_settings* effective);
    /* Camera control from the receiver (also KEYFRAME_REQUEST, which the core itself issues on every new stream).
     * Return LENNY_ACK_*. */
    int32_t (*on_control)(void* user, const lenny_control* control);
    /* Congestion control changed the target bitrate (protocol.md §9). Apply it to the encoder. May be NULL. */
    void (*on_bitrate)(void* user, uint32_t kbps);
} lenny_sender_callbacks;

LENNY_API lenny_session* lenny_sender_create(const lenny_sender_config* config, const lenny_sender_callbacks* callbacks);
/* Connects and keeps reconnecting with backoff until lenny_session_disconnect. `pair_token` (16 bytes, from the QR code)
 * may be NULL; it's used for the first successful handshake only, since tokens are single-use. */
LENNY_API int32_t lenny_sender_connect(lenny_session* s, const char* host, uint16_t port, const uint8_t* pair_token);
LENNY_API int32_t lenny_sender_send_video_config(lenny_session* s, const uint8_t* data, size_t size);
/* Never blocks on the network: the frame is copied into a queue and written by a core thread. When the queue holds
 * more than ~250 ms of video, the backlog is dropped, the core asks for a keyframe (on_control KEYFRAME_REQUEST) and
 * lowers the bitrate (on_bitrate). Frames are also dropped until that keyframe arrives. Returns LENNY_OK either way. */
LENNY_API int32_t lenny_sender_send_video_frame(lenny_session* s, const uint8_t* data, size_t size, int64_t pts_us,
                                                uint8_t orientation, uint8_t flags);
/* The camera settled on different settings than announced (e.g. another resolution): re-sends STREAM_START. */
LENNY_API int32_t lenny_sender_update_stream(lenny_session* s, const lenny_stream_settings* effective);
LENNY_API int32_t lenny_sender_send_control_state(lenny_session* s, const lenny_control_state* state);
LENNY_API int32_t lenny_sender_send_stream_status(lenny_session* s, uint8_t state, const char* reason);

/* ---- Receiver (desktop) ----------------------------------------------- */
typedef struct {
    lenny_identity identity;
    uint16_t port;                    /* 0 = ephemeral (tests); see lenny_receiver_port */
    lenny_stream_settings preferred;  /* closest supported mode is picked from the sender's CAPS */
} lenny_receiver_config;

/* All receiver callbacks return void so Dart can use NativeCallable.listener for the UI ones. */
typedef struct {
    void* user;
    void (*on_state)(void* user, int32_t state /* lenny_state */, int32_t reason);
    /* Unknown phone connected without a QR token. Answer with lenny_receiver_approve within 30 s. */
    void (*on_approval_needed)(void* user, const uint8_t device_id[LENNY_DEVICE_ID_SIZE], const char* device_name);
    void (*on_stream_start)(void* user, const lenny_stream_settings* effective);
    void (*on_stream_status)(void* user, uint8_t state, const char* reason);
    void (*on_video_config)(void* user, const uint8_t* data, size_t size);
    void (*on_video_frame)(void* user, const lenny_video_frame* frame);
    void (*on_control_state)(void* user, const lenny_control_state* state);
    void (*on_control_ack)(void* user, uint32_t req_id, uint8_t result);
} lenny_receiver_callbacks;

LENNY_API lenny_session* lenny_receiver_create(const lenny_receiver_config* config, const lenny_receiver_callbacks* callbacks);
/* Starts listening (all interfaces). One streaming phone at a time; others get GOODBYE(BUSY). */
LENNY_API int32_t lenny_receiver_start(lenny_session* s);
LENNY_API uint16_t lenny_receiver_port(lenny_session* s); /* bound port, 0 if not listening */
/* Issue a single-use QR pairing token. ttl_ms 0 = default 90 s. Previous tokens stay valid until they expire. */
LENNY_API int32_t lenny_receiver_new_pair_token(lenny_session* s, uint32_t ttl_ms, uint8_t out[LENNY_PAIR_TOKEN_SIZE]);
/* Pre-trust a device (remembered by the app); it skips the approval prompt. */
LENNY_API int32_t lenny_receiver_trust_device(lenny_session* s, const uint8_t device_id[LENNY_DEVICE_ID_SIZE]);
/* Answer on_approval_needed. accept != 0 also trusts the device for the lifetime of this session object. */
LENNY_API int32_t lenny_receiver_approve(lenny_session* s, int32_t accept);
/* ABI 1.1. Change the stream settings: the phone mode closest to `preferred` (area, then fps) is requested right away
 * when streaming (CAPS_SELECT mid-stream) and on every later connect. LENNY_OK also when not streaming yet. */
LENNY_API int32_t lenny_receiver_select_stream(lenny_session* s, const lenny_stream_settings* preferred);
/* Send a camera control; req_id is assigned by the core and written back into *control. */
LENNY_API int32_t lenny_receiver_send_control(lenny_session* s, lenny_control* control);

/* ---- Common ----------------------------------------------------------- */
/* UI events, for Dart (NativeCallable.listener) or any other UI layer. Events carry plain integers only, because
 * async listeners run after the callback returned, when pointers would already dangle. Fetch details with the
 * getters below. Runs on the I/O thread, in addition to the native callbacks. After set_event_listener(s, NULL, NULL)
 * returns, the old listener is never called again. The listener must not call lenny_session_set_event_listener. */
typedef enum {
    LENNY_EVENT_STATE = 1,             /* a = lenny_state, b = reason */
    LENNY_EVENT_APPROVAL_NEEDED = 2,   /* receiver; details: lenny_session_peer */
    LENNY_EVENT_STREAM_START = 3,      /* details: lenny_session_stream_settings */
    LENNY_EVENT_STREAM_STATUS = 4,     /* receiver; a = LENNY_STREAM_* */
    LENNY_EVENT_CONTROL_STATE = 5,     /* receiver; details: lenny_session_control_state */
    LENNY_EVENT_CONTROL_ACK = 6        /* receiver; a = req_id, b = LENNY_ACK_* */
} lenny_event;

#define LENNY_MAX_PEER_LENSES 8
#define LENNY_MAX_PEER_MODES 32

typedef struct {
    uint8_t device_id[LENNY_DEVICE_ID_SIZE];
    char name[64];     /* UTF-8, NUL-terminated, truncated */
    uint8_t platform;  /* LENNY_PLATFORM_*, 0 if unknown */
    /* Receiver only, from the phone's CAPS (zero until it arrives): what the remote camera controls can do. */
    uint32_t controls;                                   /* LENNY_CAP_* */
    uint8_t lens_count;                                  /* <= LENNY_MAX_PEER_LENSES */
    uint8_t lens_ids[LENNY_MAX_PEER_LENSES];
    uint8_t lens_facing[LENNY_MAX_PEER_LENSES];          /* 0 back, 1 front, 2 external */
    char lens_labels[LENNY_MAX_PEER_LENSES][32];         /* UTF-8, NUL-terminated, truncated */
    int32_t exposure_min, exposure_max;                  /* EV*1000; both 0 = no exposure compensation */
    uint32_t exposure_step_milli;
    /* ABI 1.1: the phone's modes from CAPS, for a resolution / frame-rate picker. */
    uint8_t mode_count;                                  /* <= LENNY_MAX_PEER_MODES */
    lenny_mode modes[LENNY_MAX_PEER_MODES];
} lenny_peer_info;

LENNY_API int32_t lenny_session_set_event_listener(lenny_session* s, void (*fn)(void* user, int32_t event, int32_t a,
                                                                                 int32_t b), void* user);
/* Details of the current / most recent peer, stream and camera state. LENNY_E_STATE if not known yet. */
LENNY_API int32_t lenny_session_peer(lenny_session* s, lenny_peer_info* out);
LENNY_API int32_t lenny_session_stream_settings(lenny_session* s, lenny_stream_settings* out);
LENNY_API int32_t lenny_session_control_state(lenny_session* s, lenny_control_state* out);
LENNY_API int32_t lenny_session_state(lenny_session* s); /* lenny_state */
LENNY_API int32_t lenny_session_get_stats(lenny_session* s, lenny_stats* out);
/* Sends GOODBYE(USER), stops reconnecting / listening. Idempotent. A phone that receives USER stops retrying. */
LENNY_API int32_t lenny_session_disconnect(lenny_session* s);
/* Stops, joins threads, frees. NULL is fine. Not from inside a callback. Sends GOODBYE(NORMAL), not USER: when the
 * desktop app quits or restarts, the phone keeps retrying and resumes by itself once the app is back. */
LENNY_API void lenny_session_destroy(lenny_session* s);

#ifdef __cplusplus
}
#endif
#endif /* LENNY_H */
