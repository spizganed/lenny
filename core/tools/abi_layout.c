/* Prints sizeof/offsetof for every struct in lenny.h. tools/abi_check.sh compares the output of this file
 * compiled against the hand-written header and against the cbindgen-generated one. */
#include <stddef.h>
#include <stdio.h>
#include LENNY_HEADER

#define S(t) printf("%s size=%zu align=%zu\n", #t, sizeof(t), _Alignof(t))
#define O(t, f) printf("  %s.%s @%zu\n", #t, #f, offsetof(t, f))

int main(void) {
    S(lenny_mode); O(lenny_mode, width); O(lenny_mode, height); O(lenny_mode, fps_num); O(lenny_mode, fps_den);
    S(lenny_stream_settings); O(lenny_stream_settings, codec); O(lenny_stream_settings, mode);
    O(lenny_stream_settings, bitrate_kbps); O(lenny_stream_settings, has_lens); O(lenny_stream_settings, lens_id);
    S(lenny_lens); O(lenny_lens, lens_id); O(lenny_lens, facing); O(lenny_lens, label);
    S(lenny_control); O(lenny_control, req_id); O(lenny_control, cmd); O(lenny_control, x); O(lenny_control, y);
    O(lenny_control, value);
    S(lenny_control_state); O(lenny_control_state, af_mode); O(lenny_control_state, exposure_comp);
    O(lenny_control_state, exposure_lock); O(lenny_control_state, wb_lock); O(lenny_control_state, torch);
    O(lenny_control_state, lens_id); O(lenny_control_state, zoom); O(lenny_control_state, battery);
    O(lenny_control_state, charging); O(lenny_control_state, pan_x); O(lenny_control_state, pan_y);
    S(lenny_video_frame); O(lenny_video_frame, frame_seq); O(lenny_video_frame, pts_us);
    O(lenny_video_frame, local_pts_us); O(lenny_video_frame, orientation); O(lenny_video_frame, flags);
    O(lenny_video_frame, data); O(lenny_video_frame, size);
    S(lenny_stats); O(lenny_stats, rtt_us); O(lenny_stats, clock_offset_us); O(lenny_stats, frames);
    O(lenny_stats, bytes); O(lenny_stats, bad_messages); O(lenny_stats, reconnects); O(lenny_stats, latency_us);
    O(lenny_stats, dropped_frames); O(lenny_stats, bitrate_kbps);
    S(lenny_identity); O(lenny_identity, device_id); O(lenny_identity, device_name); O(lenny_identity, app_version);
    O(lenny_identity, platform);
    S(lenny_sender_config); O(lenny_sender_config, identity); O(lenny_sender_config, modes);
    O(lenny_sender_config, mode_count); O(lenny_sender_config, max_bitrate_kbps); O(lenny_sender_config, controls);
    O(lenny_sender_config, lenses); O(lenny_sender_config, lens_count); O(lenny_sender_config, exposure_comp_min);
    O(lenny_sender_config, exposure_comp_max); O(lenny_sender_config, exposure_comp_step_milli);
    O(lenny_sender_config, lens_caps);
    S(lenny_lens_caps); O(lenny_lens_caps, modes); O(lenny_lens_caps, mode_count); O(lenny_lens_caps, zoom_min);
    O(lenny_lens_caps, zoom_max); O(lenny_lens_caps, zoom_base);
    S(lenny_sender_callbacks); O(lenny_sender_callbacks, user); O(lenny_sender_callbacks, on_state);
    O(lenny_sender_callbacks, on_stream_config); O(lenny_sender_callbacks, on_control);
    O(lenny_sender_callbacks, on_bitrate);
    S(lenny_receiver_config); O(lenny_receiver_config, identity); O(lenny_receiver_config, port);
    O(lenny_receiver_config, preferred);
    S(lenny_receiver_callbacks); O(lenny_receiver_callbacks, user); O(lenny_receiver_callbacks, on_state);
    O(lenny_receiver_callbacks, on_approval_needed); O(lenny_receiver_callbacks, on_stream_start);
    O(lenny_receiver_callbacks, on_stream_status); O(lenny_receiver_callbacks, on_video_config);
    O(lenny_receiver_callbacks, on_video_frame); O(lenny_receiver_callbacks, on_control_state);
    O(lenny_receiver_callbacks, on_control_ack);
    S(lenny_peer_info); O(lenny_peer_info, device_id); O(lenny_peer_info, name); O(lenny_peer_info, platform);
    O(lenny_peer_info, controls); O(lenny_peer_info, lens_count); O(lenny_peer_info, lens_ids);
    O(lenny_peer_info, lens_facing); O(lenny_peer_info, lens_labels); O(lenny_peer_info, exposure_min);
    O(lenny_peer_info, exposure_max); O(lenny_peer_info, exposure_step_milli); O(lenny_peer_info, mode_count);
    O(lenny_peer_info, modes);
    return 0;
}
