/*
 * The x264 settings come from voxl (https://github.com/tembo/voxl),
 * Copyright (c) 2026 Tembo, under the MIT license: see LICENSE-voxl.
 *
 * A small C side keeps x264_param_t and x264_picture_t, whose layout
 * changes between x264 builds, out of Rust.
 */

#include <stdint.h>
#include <stdlib.h>
#include <x264.h>

struct screen_enc {
    x264_t *h;
    x264_param_t param;
    x264_picture_t pic;
};

struct screen_enc *screen_enc_open(int width, int height, int fps, int threads, float crf,
                                   int max_kbps)
{
    struct screen_enc *e = calloc(1, sizeof(*e));
    if (!e)
        return NULL;
    x264_param_t *p = &e->param;
    if (x264_param_default_preset(p, "ultrafast", "zerolatency") < 0) {
        free(e);
        return NULL;
    }
    p->i_width = width;
    p->i_height = height;
    p->i_csp = X264_CSP_I420;
    p->i_fps_num = fps;
    p->i_fps_den = 1;
    p->i_timebase_num = 1;
    p->i_timebase_den = 1000;
    /* VFR input costs a frame of encoder delay. */
    p->b_vfr_input = 0;
    p->i_threads = threads;
    p->b_sliced_threads = threads > 1;
    p->i_slice_count = threads;
    /* Keyframes only when a viewer needs one. */
    p->i_keyint_max = X264_KEYINT_MAX_INFINITE;
    p->i_keyint_min = 1;
    p->i_scenecut_threshold = 0;
    p->b_intra_refresh = 0;
    p->i_bframe = 0;
    p->b_annexb = 1;
    p->b_repeat_headers = 1;
    p->b_aud = 0;
    p->i_log_level = X264_LOG_WARNING;
    p->rc.i_rc_method = X264_RC_CRF;
    p->rc.f_rf_constant = crf;
    p->rc.i_lookahead = 0;
    p->rc.b_mb_tree = 0;
    p->i_sync_lookahead = 0;
    /* The diamond search of ultrafast only follows motion of a few pixels;
     * UMH finds a dragged window or a scroll for about the same CPU. */
    p->analyse.i_me_method = X264_ME_UMH;
    p->analyse.i_me_range = 64;
    /* Unchanged macroblocks, from X damage, skip analysis. */
    p->analyse.b_mb_info = 1;
    if (max_kbps > 0) {
        p->rc.i_vbv_max_bitrate = max_kbps;
        p->rc.i_vbv_buffer_size = max_kbps / (fps > 0 ? fps : 1) * 5 / 4;
    }
    /* BT.601 limited range, as libyuv ARGBToI420 writes it. */
    p->vui.i_colorprim = 1;
    p->vui.i_transfer = 1;
    p->vui.i_colmatrix = 6;
    p->vui.b_fullrange = 0;
    x264_param_apply_profile(p, "high");
    e->h = x264_encoder_open(p);
    if (!e->h) {
        free(e);
        return NULL;
    }
    x264_picture_init(&e->pic);
    e->pic.img.i_csp = X264_CSP_I420;
    e->pic.img.i_plane = 3;
    return e;
}

/* Profile, constraint flags and level from the SPS, for the codec string. */
int screen_enc_avc(struct screen_enc *e, uint8_t out[3])
{
    x264_nal_t *nals;
    int n;
    if (x264_encoder_headers(e->h, &nals, &n) < 0)
        return -1;
    for (int i = 0; i < n; i++) {
        if (nals[i].i_type != NAL_SPS)
            continue;
        const uint8_t *b = nals[i].p_payload;
        int len = nals[i].i_payload;
        int at = 0;
        while (at + 3 <= len && !(b[at] == 0 && b[at + 1] == 0 && b[at + 2] == 1))
            at++;
        at += 4;
        if (at + 3 > len)
            return -1;
        out[0] = b[at];
        out[1] = b[at + 1];
        out[2] = b[at + 2];
        return 0;
    }
    return -1;
}

void screen_enc_set_rate(struct screen_enc *e, float crf, int max_kbps, int buf_kbit)
{
    e->param.rc.f_rf_constant = crf;
    if (max_kbps > 0) {
        e->param.rc.i_vbv_max_bitrate = max_kbps;
        e->param.rc.i_vbv_buffer_size = buf_kbit > 0 ? buf_kbit : 1;
    }
    x264_encoder_reconfig(e->h, &e->param);
}

/* Encodes one I420 picture. On success *out points at the access unit,
 * Annex B, valid until the next call, and the result is its size. */
int screen_enc_encode(struct screen_enc *e, uint8_t *y, uint8_t *u, uint8_t *v, int stride_y,
                      int stride_uv, int64_t pts, int idr, uint8_t *mb_info,
                      const uint8_t **out, int *keyframe)
{
    e->pic.img.plane[0] = y;
    e->pic.img.plane[1] = u;
    e->pic.img.plane[2] = v;
    e->pic.img.i_stride[0] = stride_y;
    e->pic.img.i_stride[1] = stride_uv;
    e->pic.img.i_stride[2] = stride_uv;
    e->pic.i_type = idr ? X264_TYPE_IDR : X264_TYPE_AUTO;
    e->pic.i_qpplus1 = 0;
    e->pic.prop.mb_info = mb_info;
    e->pic.prop.mb_info_free = NULL;
    e->pic.i_pts = pts;
    x264_nal_t *nals = NULL;
    int n = 0;
    x264_picture_t pic_out;
    int size = x264_encoder_encode(e->h, &nals, &n, &e->pic, &pic_out);
    *out = NULL;
    *keyframe = 0;
    if (size < 0)
        return size;
    if (size > 0 && nals) {
        /* The payloads of one call are contiguous. */
        *out = nals[0].p_payload;
        *keyframe = pic_out.b_keyframe;
    }
    return size;
}

void screen_enc_close(struct screen_enc *e)
{
    x264_encoder_close(e->h);
    free(e);
}
