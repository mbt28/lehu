/* SPDX-License-Identifier: GPL-3.0-or-later */
/*
 * H.264 and HEVC on a V4L2 stateless decoder (cedrus on the Allwinner
 * F1C200s) through ffmpeg's v4l2-request hwaccel. Pictures come back as DRM
 * PRIME dma-bufs (tiled NV12) and go straight onto the video plane: no copy
 * and no colour conversion on the CPU.
 *
 * Adapted from LIVI's decoder.c (GPL-3.0), itself ported from FastCarPlay's
 * V4l2DrmDecoder. What is kept, and why:
 *  - libavcodec's parser sits in front of the decoder. The stateless hwaccel
 *    takes the slice parameters from the parser; raw Annex B fed straight to
 *    avcodec_send_packet fails with "Invalid data".
 *  - The parser runs with PARSER_FLAG_COMPLETE_FRAMES: every unit the
 *    protocol hands over is one access unit. Without the flag the parser
 *    could only end a frame at the start of the next, every frame waited a
 *    frame interval, and the last frame of a burst (the final state of a
 *    tap or a scroll) stayed stuck until the phone sent another.
 *  - avcodec_flush_buffers is never called. On v4l2-request it re-initialises
 *    the decoder and allocates a second cedrus buffer pool next to the live
 *    one; the 24 MB of CMA fit one, and ffmpeg then falls back to software
 *    decode without a word. An IDR resets the references by itself.
 *  - Decoded pictures stay referenced in fixed slots (the three held frames
 *    and the pending one) that the Rust side names; nothing is allocated per
 *    picture.
 */
#include <stddef.h>
#include <stdint.h>
#include <string.h>

#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_drm.h>

/* The three held frames and the pending one. */
#define LEHU_SLOTS 4
/* The Rust side keeps this many readable bytes after every unit. */
#define LEHU_PADDING 64
_Static_assert(AV_INPUT_BUFFER_PADDING_SIZE <= LEHU_PADDING,
	       "the access unit padding is smaller than ffmpeg's");

struct lehu_picture {
	uint32_t fourcc;
	int32_t width;
	int32_t height;
	int32_t planes;
	int32_t fds[4];
	uint32_t pitches[4];
	uint32_t offsets[4];
	uint64_t modifier;
};

typedef void (*lehu_picture_cb)(void *ctx, const struct lehu_picture *p);

struct lehu_dec {
	AVCodecContext *ctx;
	AVBufferRef *hwdev;
	AVCodecParserContext *parser;
	AVPacket *pkt;
	/* The picture just decoded, for the length of the callback. */
	AVFrame *frame;
	AVFrame *slots[LEHU_SLOTS];
};

static enum AVPixelFormat get_drm_format(AVCodecContext *ctx, const enum AVPixelFormat *fmts)
{
	(void)ctx;
	for (const enum AVPixelFormat *p = fmts; *p != AV_PIX_FMT_NONE; p++)
		if (*p == AV_PIX_FMT_DRM_PRIME)
			return *p;
	return fmts[0];
}

/* What this ffmpeg offers for a coded format (0 H.264, 1 HEVC, 2 VP9,
 * 3 AV1): bit 0 a decoder, bit 1 a parser for it, bit 2 a DRM hwaccel
 * (v4l2-request) on that decoder. The kernel's word (the V4L2 probe) is moot
 * without all three. Tables only: nothing is opened or allocated. */
int lehu_dec_support(int which)
{
	static const enum AVCodecID ids[] = {
		AV_CODEC_ID_H264,
		AV_CODEC_ID_HEVC,
		AV_CODEC_ID_VP9,
		AV_CODEC_ID_AV1,
	};
	if (which < 0 || which >= (int)(sizeof(ids) / sizeof(ids[0])))
		return 0;
	enum AVCodecID id = ids[which];
	int bits = 0;
	const AVCodec *c = avcodec_find_decoder(id);
	if (c) {
		bits |= 1;
		for (int i = 0;; i++) {
			const AVCodecHWConfig *cfg = avcodec_get_hw_config(c, i);
			if (!cfg)
				break;
			if (cfg->pix_fmt == AV_PIX_FMT_DRM_PRIME ||
			    cfg->device_type == AV_HWDEVICE_TYPE_DRM) {
				bits |= 4;
				break;
			}
		}
	}
	void *it = NULL;
	const AVCodecParser *p;
	while (!(bits & 2) && (p = av_parser_iterate(&it))) {
		size_t n = sizeof(p->codec_ids) / sizeof(p->codec_ids[0]);
		for (size_t k = 0; k < n && p->codec_ids[k] != AV_CODEC_ID_NONE; k++)
			if (p->codec_ids[k] == (int)id)
				bits |= 2;
	}
	return bits;
}

void lehu_dec_close(struct lehu_dec *d)
{
	if (!d)
		return;
	for (int i = 0; i < LEHU_SLOTS; i++)
		av_frame_free(&d->slots[i]);
	av_frame_free(&d->frame);
	av_packet_free(&d->pkt);
	if (d->parser)
		av_parser_close(d->parser);
	avcodec_free_context(&d->ctx);
	av_buffer_unref(&d->hwdev);
	av_free(d);
}

/* Opens the decoder; `err` (at least 64 bytes) says why when it fails. */
struct lehu_dec *lehu_dec_open(int hevc, int low_delay, int fast, char *err, size_t err_len)
{
	const char *why = "out of memory";
	struct lehu_dec *d = av_mallocz(sizeof(*d));
	if (!d)
		goto fail;
	enum AVCodecID id = hevc ? AV_CODEC_ID_HEVC : AV_CODEC_ID_H264;
	const AVCodec *c = avcodec_find_decoder(id);
	if (!c) {
		why = "no decoder for the codec";
		goto fail;
	}
	d->ctx = avcodec_alloc_context3(c);
	if (!d->ctx)
		goto fail;
	/* v4l2-request's hwdevice: DRM-typed in Kwiboo's 6.x series, its own
	 * type ("v4l2request") from 7.x on. get_format then picks DRM_PRIME
	 * either way, so the in-kernel decoder is used and pictures come back
	 * as dma-bufs. */
	enum AVHWDeviceType type = av_hwdevice_find_type_by_name("v4l2request");
	if (type == AV_HWDEVICE_TYPE_NONE)
		type = AV_HWDEVICE_TYPE_DRM;
	if (av_hwdevice_ctx_create(&d->hwdev, type, NULL, NULL, 0) < 0) {
		why = "no v4l2-request hwdevice";
		goto fail;
	}
	d->ctx->hw_device_ctx = av_buffer_ref(d->hwdev);
	if (!d->ctx->hw_device_ctx)
		goto fail;
	d->ctx->get_format = get_drm_format;
	if (low_delay)
		d->ctx->flags |= AV_CODEC_FLAG_LOW_DELAY;
	if (fast)
		d->ctx->flags2 |= AV_CODEC_FLAG2_FAST;
	int ret = avcodec_open2(d->ctx, c, NULL);
	if (ret < 0) {
		av_strerror(ret, err, err_len);
		lehu_dec_close(d);
		return NULL;
	}
	d->parser = av_parser_init(id);
	if (!d->parser) {
		why = "no parser for the codec";
		goto fail;
	}
	d->parser->flags |= PARSER_FLAG_COMPLETE_FRAMES;
	d->pkt = av_packet_alloc();
	d->frame = av_frame_alloc();
	if (!d->pkt || !d->frame)
		goto fail;
	for (int i = 0; i < LEHU_SLOTS; i++) {
		d->slots[i] = av_frame_alloc();
		if (!d->slots[i])
			goto fail;
	}
	return d;
fail:
	if (err_len) {
		strncpy(err, why, err_len - 1);
		err[err_len - 1] = 0;
	}
	lehu_dec_close(d);
	return NULL;
}

const char *lehu_dec_name(const struct lehu_dec *d)
{
	return d && d->ctx && d->ctx->codec ? d->ctx->codec->name : "";
}

static void hand_out(struct lehu_dec *d, lehu_picture_cb cb, void *cb_ctx)
{
	if (d->frame->format != AV_PIX_FMT_DRM_PRIME)
		return;
	const AVDRMFrameDescriptor *desc = (const AVDRMFrameDescriptor *)d->frame->data[0];
	if (!desc || desc->nb_objects < 1 || desc->nb_layers < 1)
		return;
	const AVDRMLayerDescriptor *layer = &desc->layers[0];
	struct lehu_picture p;
	memset(&p, 0, sizeof(p));
	p.fourcc = layer->format;
	p.width = d->frame->width;
	p.height = d->frame->height;
	p.planes = layer->nb_planes < 4 ? layer->nb_planes : 4;
	if (p.planes < 1)
		return;
	for (int i = 0; i < p.planes; i++) {
		int object = layer->planes[i].object_index;
		if (object < 0 || object >= desc->nb_objects)
			return;
		p.fds[i] = desc->objects[object].fd;
		p.pitches[i] = (uint32_t)layer->planes[i].pitch;
		p.offsets[i] = (uint32_t)layer->planes[i].offset;
	}
	p.modifier = desc->objects[layer->planes[0].object_index].format_modifier;
	cb(cb_ctx, &p);
}

/*
 * Decodes one access unit (`len` bytes, with LEHU_PADDING readable zero
 * bytes after them: the parser reads past the end). Every picture goes to
 * `cb` while it is in the scratch frame; the callback keeps it with
 * lehu_dec_hold or it is released when the callback returns. Returns the
 * number of pictures, or a negative AVERROR when the decoder refused a
 * packet and no picture came.
 */
int lehu_dec_push(struct lehu_dec *d, const uint8_t *data, size_t len, lehu_picture_cb cb,
		  void *cb_ctx)
{
	const uint8_t *p = data;
	int left = len > (size_t)INT32_MAX ? INT32_MAX : (int)len;
	int pictures = 0;
	int err = 0;
	while (left > 0) {
		uint8_t *out;
		int out_len;
		int used = av_parser_parse2(d->parser, d->ctx, &out, &out_len, p, left,
					    AV_NOPTS_VALUE, AV_NOPTS_VALUE, 0);
		if (used < 0)
			break;
		p += used;
		left -= used;
		if (out_len <= 0)
			continue;
		av_packet_unref(d->pkt);
		d->pkt->data = out;
		d->pkt->size = out_len;
		int ret = avcodec_send_packet(d->ctx, d->pkt); /* copies: not refcounted */
		if (ret < 0) {
			err = ret;
			continue;
		}
		while (avcodec_receive_frame(d->ctx, d->frame) == 0) {
			pictures++;
			hand_out(d, cb, cb_ctx);
			av_frame_unref(d->frame); /* nothing left when it was held */
		}
	}
	return pictures ? pictures : err;
}

/* Keep the picture of the current callback in `slot`, releasing what the
 * slot held. Outside a callback the scratch frame is empty: a no-op. */
void lehu_dec_hold(struct lehu_dec *d, int slot)
{
	if (slot < 0 || slot >= LEHU_SLOTS)
		return;
	av_frame_unref(d->slots[slot]);
	av_frame_move_ref(d->slots[slot], d->frame);
}

/* Give the slot's buffer back to the pool. */
void lehu_dec_release(struct lehu_dec *d, int slot)
{
	if (slot < 0 || slot >= LEHU_SLOTS)
		return;
	av_frame_unref(d->slots[slot]);
}

void lehu_dec_strerror(int err, char *buf, size_t len)
{
	if (av_strerror(err, buf, len) < 0 && len)
		buf[0] = 0;
}
