/*
 * AAC-LC access units to S16 PCM through libavcodec's fixed-point decoder.
 *
 * The ARM926 has no FPU: ffmpeg's default AAC decoder runs on soft-float
 * and took most of the core (61% measured on FastCarPlay in October 2026);
 * "aac_fixed" emits S32P, of which the top 16 bits are taken here, in C, so
 * the per-sample loop stays out of the Rust hot path's way and float
 * formats (a host's float decoder) are still handled. Apple sends raw AAC-LC
 * units without ADTS, so the decoder is given the 2-byte
 * AudioSpecificConfig the stream's rate and channels describe.
 */
#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

#include <libavcodec/avcodec.h>
#include <libavutil/channel_layout.h>
#include <libavutil/error.h>
#include <libavutil/mem.h>
#include <libavutil/samplefmt.h>

/* The Rust side keeps this many readable bytes after every unit. */
#define LEHU_PADDING 64
_Static_assert(AV_INPUT_BUFFER_PADDING_SIZE <= LEHU_PADDING,
	       "the unit padding is smaller than ffmpeg's");

struct lehu_aac {
	AVCodecContext *ctx;
	AVPacket *pkt;
	AVFrame *frame;
	const char *name;
};

/* MPEG-4 sampling frequency index for the AudioSpecificConfig. */
static int freq_index(int rate)
{
	switch (rate) {
	case 96000: return 0;
	case 88200: return 1;
	case 64000: return 2;
	case 48000: return 3;
	case 44100: return 4;
	case 32000: return 5;
	case 24000: return 6;
	case 22050: return 7;
	case 16000: return 8;
	default: return 4;
	}
}

static void fail(char *err, size_t len, const char *msg)
{
	if (err && len) {
		strncpy(err, msg, len - 1);
		err[len - 1] = 0;
	}
}

void lehu_aac_close(struct lehu_aac *d);

struct lehu_aac *lehu_aac_open(int rate, int channels, char *err, size_t errlen)
{
	if (channels < 1 || channels > 2) {
		fail(err, errlen, "AAC: 1 or 2 channels");
		return NULL;
	}
	const AVCodec *dec = avcodec_find_decoder_by_name("aac_fixed");
	if (!dec)
		dec = avcodec_find_decoder(AV_CODEC_ID_AAC);
	if (!dec) {
		fail(err, errlen, "no AAC decoder in libavcodec");
		return NULL;
	}
	struct lehu_aac *d = av_mallocz(sizeof(*d));
	if (!d) {
		fail(err, errlen, "out of memory");
		return NULL;
	}
	d->name = dec->name;
	d->ctx = avcodec_alloc_context3(dec);
	d->pkt = av_packet_alloc();
	d->frame = av_frame_alloc();
	if (!d->ctx || !d->pkt || !d->frame) {
		fail(err, errlen, "out of memory");
		lehu_aac_close(d);
		return NULL;
	}
	d->ctx->sample_rate = rate;
	av_channel_layout_default(&d->ctx->ch_layout, channels);
	/* objectType(5)=2 AAC-LC | freqIndex(4) | channels(4) */
	const int asc = ((2 << 11) | (freq_index(rate) << 7) | (channels << 3)) & 0xffff;
	d->ctx->extradata = av_mallocz(2 + AV_INPUT_BUFFER_PADDING_SIZE);
	if (!d->ctx->extradata) {
		fail(err, errlen, "out of memory");
		lehu_aac_close(d);
		return NULL;
	}
	d->ctx->extradata[0] = (uint8_t)(asc >> 8);
	d->ctx->extradata[1] = (uint8_t)(asc & 0xff);
	d->ctx->extradata_size = 2;
	int ret = avcodec_open2(d->ctx, dec, NULL);
	if (ret < 0) {
		char msg[96];
		av_strerror(ret, msg, sizeof msg);
		fail(err, errlen, msg);
		lehu_aac_close(d);
		return NULL;
	}
	return d;
}

const char *lehu_aac_name(const struct lehu_aac *d)
{
	return d ? d->name : "";
}

/*
 * One decoded frame into `out` (interleaved S16, `cap` samples free), mono
 * duplicated to stereo when `upmix`. Samples written, or -1 when it does not
 * fit.
 */
static long write_frame(const AVFrame *f, int16_t *out, size_t cap, int upmix)
{
	const int n = f->nb_samples;
	const int ch = f->ch_layout.nb_channels;
	const int dup = upmix && ch == 1;
	const size_t need = (size_t)n * (size_t)(dup ? 2 : ch);
	if (n <= 0 || ch <= 0 || ch > 8 || need > cap)
		return -1;
	int16_t *w = out;
	switch (f->format) {
	case AV_SAMPLE_FMT_S32P:
		for (int i = 0; i < n; i++) {
			for (int c = 0; c < ch; c++)
				*w++ = (int16_t)(((const int32_t *)f->data[c])[i] >> 16);
			if (dup) { w[0] = w[-1]; w++; }
		}
		break;
	case AV_SAMPLE_FMT_S32:
		for (int i = 0; i < n; i++) {
			for (int c = 0; c < ch; c++)
				*w++ = (int16_t)(((const int32_t *)f->data[0])[i * ch + c] >> 16);
			if (dup) { w[0] = w[-1]; w++; }
		}
		break;
	case AV_SAMPLE_FMT_S16P:
		for (int i = 0; i < n; i++) {
			for (int c = 0; c < ch; c++)
				*w++ = ((const int16_t *)f->data[c])[i];
			if (dup) { w[0] = w[-1]; w++; }
		}
		break;
	case AV_SAMPLE_FMT_S16:
		for (int i = 0; i < n; i++) {
			for (int c = 0; c < ch; c++)
				*w++ = ((const int16_t *)f->data[0])[i * ch + c];
			if (dup) { w[0] = w[-1]; w++; }
		}
		break;
	case AV_SAMPLE_FMT_FLTP:
	case AV_SAMPLE_FMT_FLT:
		/* A host's float decoder; never the board's (aac_fixed). */
		for (int i = 0; i < n; i++) {
			for (int c = 0; c < ch; c++) {
				float s = f->format == AV_SAMPLE_FMT_FLTP
					  ? ((const float *)f->data[c])[i]
					  : ((const float *)f->data[0])[i * ch + c];
				int v = (int)(s * 32767.0f + (s >= 0 ? 0.5f : -0.5f));
				*w++ = (int16_t)(v > 32767 ? 32767 : v < -32768 ? -32768 : v);
			}
			if (dup) { w[0] = w[-1]; w++; }
		}
		break;
	default:
		return -1;
	}
	return (long)need;
}

/*
 * Decode one access unit (`LEHU_PADDING` readable bytes follow `data`) and
 * write every frame it yields to `out`, `cap` samples free. Samples
 * written (0 when the decoder kept the unit), or a negative AVERROR;
 * AVERROR(ENOSPC) when a frame did not fit.
 */
long lehu_aac_decode(struct lehu_aac *d, const uint8_t *data, size_t len,
		     int16_t *out, size_t cap, int upmix)
{
	if (!d || !data || len == 0 || len > INT32_MAX)
		return AVERROR(EINVAL);
	d->pkt->data = (uint8_t *)data;
	d->pkt->size = (int)len;
	int ret = avcodec_send_packet(d->ctx, d->pkt);
	if (ret < 0)
		return ret;
	long written = 0;
	for (;;) {
		ret = avcodec_receive_frame(d->ctx, d->frame);
		if (ret < 0)
			break;
		long n = write_frame(d->frame, out + written, cap - (size_t)written, upmix);
		av_frame_unref(d->frame);
		if (n < 0)
			return AVERROR(ENOSPC);
		written += n;
	}
	return written;
}

void lehu_aac_close(struct lehu_aac *d)
{
	if (!d)
		return;
	if (d->frame)
		av_frame_free(&d->frame);
	if (d->pkt)
		av_packet_free(&d->pkt);
	if (d->ctx)
		avcodec_free_context(&d->ctx);
	av_free(d);
}
