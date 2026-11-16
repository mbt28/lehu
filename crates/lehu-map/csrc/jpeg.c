/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (C) 2026 Mehmet Tekbas */
/*
 * The video engine's JPEG decoder (cedrus with its JPEG patches) as three
 * calls: open a session for pictures of one size, decode one, close.
 *
 * A memory-to-memory device, single-planar: the whole file is queued on
 * OUTPUT (no media request: the kernel parses the headers itself), the
 * picture comes back on CAPTURE as NV12 in 32 x 32 tiles (ST12). One buffer
 * each way, one picture at a time. Baseline 8-bit 4:2:0 only: anything
 * else comes back with the error flag, and the caller decodes it itself.
 *
 * Written in C so the kernel's own header lays out the structures (struct
 * v4l2_buffer holds a timeval, whose size the ioctl numbers encode).
 * Every call returns 0 on success and -errno on failure, after retrying
 * EINTR.
 */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

#include <linux/videodev2.h>

#ifndef V4L2_PIX_FMT_NV12_32L32
#define V4L2_PIX_FMT_NV12_32L32 v4l2_fourcc('S', 'T', '1', '2')
#endif

/* How long a picture may take: a tile takes about a millisecond. */
#define DECODE_TIMEOUT_MS 500

struct lehu_jpeg {
	int fd;
	void *in;
	size_t in_len;
	void *out;
	size_t out_len;
	uint32_t width, height, luma_pitch, luma_size;
};

static int xioctl(int fd, unsigned long req, void *arg)
{
	int r;
	do
		r = ioctl(fd, req, arg);
	while (r < 0 && errno == EINTR);
	return r < 0 ? -errno : 0;
}

/* The device takes JPEG on its OUTPUT queue. */
static int takes_jpeg(int fd)
{
	struct v4l2_fmtdesc d;
	for (uint32_t i = 0; i < 64; i++) {
		memset(&d, 0, sizeof(d));
		d.index = i;
		d.type = V4L2_BUF_TYPE_VIDEO_OUTPUT;
		if (xioctl(fd, VIDIOC_ENUM_FMT, &d) < 0)
			return 0;
		if (d.pixelformat == V4L2_PIX_FMT_JPEG)
			return 1;
	}
	return 0;
}

/* A single-planar memory-to-memory device that takes JPEG: its descriptor,
 * else -ENODEV. */
static int open_device(const char *path)
{
	int fd = open(path, O_RDWR | O_NONBLOCK | O_CLOEXEC);
	if (fd < 0)
		return -errno;
	struct v4l2_capability cap;
	memset(&cap, 0, sizeof(cap));
	if (xioctl(fd, VIDIOC_QUERYCAP, &cap) < 0) {
		close(fd);
		return -ENODEV;
	}
	uint32_t caps = (cap.capabilities & V4L2_CAP_DEVICE_CAPS) ? cap.device_caps : cap.capabilities;
	if (!(caps & V4L2_CAP_VIDEO_M2M) || !(caps & V4L2_CAP_STREAMING) || !takes_jpeg(fd)) {
		close(fd);
		return -ENODEV;
	}
	return fd;
}

static int set_formats(struct lehu_jpeg *j, uint32_t max_input)
{
	struct v4l2_format f;
	memset(&f, 0, sizeof(f));
	f.type = V4L2_BUF_TYPE_VIDEO_OUTPUT;
	f.fmt.pix.pixelformat = V4L2_PIX_FMT_JPEG;
	f.fmt.pix.width = j->width;
	f.fmt.pix.height = j->height;
	f.fmt.pix.sizeimage = max_input;
	int r = xioctl(j->fd, VIDIOC_S_FMT, &f);
	if (r < 0)
		return r;
	memset(&f, 0, sizeof(f));
	f.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
	f.fmt.pix.pixelformat = V4L2_PIX_FMT_NV12_32L32;
	f.fmt.pix.width = j->width;
	f.fmt.pix.height = j->height;
	r = xioctl(j->fd, VIDIOC_S_FMT, &f);
	if (r < 0)
		return r;
	if (f.fmt.pix.pixelformat != V4L2_PIX_FMT_NV12_32L32 || f.fmt.pix.width != j->width ||
	    f.fmt.pix.height != j->height)
		return -EINVAL;
	/* Tiles of 32 x 32: the luma's whole tiles, then the chroma's. */
	j->luma_pitch = (j->width + 31) & ~31u;
	j->luma_size = j->luma_pitch * ((j->height + 31) & ~31u);
	return 0;
}

/* One MMAP buffer on queue `type`, mapped. */
static int map_buffer(int fd, uint32_t type, void **ptr, size_t *len)
{
	struct v4l2_requestbuffers rb;
	memset(&rb, 0, sizeof(rb));
	rb.count = 1;
	rb.type = type;
	rb.memory = V4L2_MEMORY_MMAP;
	int r = xioctl(fd, VIDIOC_REQBUFS, &rb);
	if (r < 0)
		return r;
	if (rb.count < 1)
		return -ENOMEM;
	struct v4l2_buffer b;
	memset(&b, 0, sizeof(b));
	b.type = type;
	b.memory = V4L2_MEMORY_MMAP;
	b.index = 0;
	r = xioctl(fd, VIDIOC_QUERYBUF, &b);
	if (r < 0)
		return r;
	void *p = mmap(NULL, b.length, PROT_READ | PROT_WRITE, MAP_SHARED, fd, b.m.offset);
	if (p == MAP_FAILED)
		return -errno;
	*ptr = p;
	*len = b.length;
	return 0;
}

void lehu_jpeg_close(struct lehu_jpeg *j)
{
	if (!j)
		return;
	if (j->fd >= 0) {
		int t = V4L2_BUF_TYPE_VIDEO_OUTPUT;
		xioctl(j->fd, VIDIOC_STREAMOFF, &t);
		t = V4L2_BUF_TYPE_VIDEO_CAPTURE;
		xioctl(j->fd, VIDIOC_STREAMOFF, &t);
	}
	if (j->in)
		munmap(j->in, j->in_len);
	if (j->out)
		munmap(j->out, j->out_len);
	if (j->fd >= 0)
		close(j->fd);
	free(j);
}

/*
 * A session for `width` x `height` pictures of up to `max_input` bytes, on
 * the first /dev/videoN that decodes JPEG. Its device's path goes to
 * `path` (`path_len` bytes). NULL with -errno in `*err` when there is
 * none (-ENODEV) or it would not start.
 */
struct lehu_jpeg *lehu_jpeg_open(uint32_t width, uint32_t height, uint32_t max_input, char *path,
				 size_t path_len, int *err)
{
	*err = -ENODEV;
	if (path_len)
		path[0] = 0;
	int fd = -ENODEV;
	char name[32];
	for (int i = 0; i < 16 && fd < 0; i++) {
		snprintf(name, sizeof(name), "/dev/video%d", i);
		fd = open_device(name);
	}
	if (fd < 0)
		return NULL;
	if (path_len)
		snprintf(path, path_len, "%s", name);
	struct lehu_jpeg *j = calloc(1, sizeof(*j));
	if (!j) {
		close(fd);
		*err = -ENOMEM;
		return NULL;
	}
	j->fd = fd;
	j->width = width;
	j->height = height;
	int r = set_formats(j, max_input);
	if (r == 0)
		r = map_buffer(fd, V4L2_BUF_TYPE_VIDEO_OUTPUT, &j->in, &j->in_len);
	if (r == 0)
		r = map_buffer(fd, V4L2_BUF_TYPE_VIDEO_CAPTURE, &j->out, &j->out_len);
	if (r == 0 && j->out_len < j->luma_size + j->luma_size / 2)
		r = -EINVAL;
	int t = V4L2_BUF_TYPE_VIDEO_OUTPUT;
	if (r == 0)
		r = xioctl(fd, VIDIOC_STREAMON, &t);
	t = V4L2_BUF_TYPE_VIDEO_CAPTURE;
	if (r == 0)
		r = xioctl(fd, VIDIOC_STREAMON, &t);
	if (r < 0) {
		lehu_jpeg_close(j);
		*err = r;
		return NULL;
	}
	*err = 0;
	return j;
}

/* Bytes of luma (32-tiled) at the start of a picture; the chroma follows. */
uint32_t lehu_jpeg_luma_size(const struct lehu_jpeg *j)
{
	return j->luma_size;
}

static int dequeue(int fd, uint32_t type, uint32_t *flags)
{
	struct v4l2_buffer b;
	memset(&b, 0, sizeof(b));
	b.type = type;
	b.memory = V4L2_MEMORY_MMAP;
	int r = xioctl(fd, VIDIOC_DQBUF, &b);
	if (r == 0)
		*flags |= b.flags;
	return r;
}

/*
 * Decode the `len` bytes at `data`. On success `*out` points at the
 * picture (ST12, `*out_len` bytes, valid until the next call), and the
 * mapping is uncached: copy it out whole before reading it. -EBADMSG when
 * the engine would not decode this file (a progressive or 4:4:4 one, or
 * a corrupt one): decode it some other way.
 */
int lehu_jpeg_decode(struct lehu_jpeg *j, const uint8_t *data, size_t len, const uint8_t **out,
		     size_t *out_len)
{
	if (len == 0 || len > j->in_len)
		return -EFBIG;
	memcpy(j->in, data, len);
	struct v4l2_buffer b;
	memset(&b, 0, sizeof(b));
	b.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
	b.memory = V4L2_MEMORY_MMAP;
	b.index = 0;
	int r = xioctl(j->fd, VIDIOC_QBUF, &b);
	if (r < 0)
		return r;
	memset(&b, 0, sizeof(b));
	b.type = V4L2_BUF_TYPE_VIDEO_OUTPUT;
	b.memory = V4L2_MEMORY_MMAP;
	b.index = 0;
	b.bytesused = (uint32_t)len;
	r = xioctl(j->fd, VIDIOC_QBUF, &b);
	if (r < 0) {
		/* Take the capture buffer back so the next call can queue it. */
		int t = V4L2_BUF_TYPE_VIDEO_CAPTURE;
		xioctl(j->fd, VIDIOC_STREAMOFF, &t);
		xioctl(j->fd, VIDIOC_STREAMON, &t);
		return r;
	}
	uint32_t flags = 0;
	int got_in = 0, got_out = 0;
	while (!got_in || !got_out) {
		struct pollfd p = { .fd = j->fd, .events = POLLIN | POLLOUT };
		int n = poll(&p, 1, DECODE_TIMEOUT_MS);
		if (n < 0 && errno == EINTR)
			continue;
		if (n <= 0 || (p.revents & POLLERR)) {
			/* Stuck: both queues back to empty, for the next call. */
			int t = V4L2_BUF_TYPE_VIDEO_OUTPUT;
			xioctl(j->fd, VIDIOC_STREAMOFF, &t);
			xioctl(j->fd, VIDIOC_STREAMON, &t);
			t = V4L2_BUF_TYPE_VIDEO_CAPTURE;
			xioctl(j->fd, VIDIOC_STREAMOFF, &t);
			xioctl(j->fd, VIDIOC_STREAMON, &t);
			return n < 0 ? -errno : -ETIMEDOUT;
		}
		if (!got_in && (p.revents & POLLOUT) &&
		    dequeue(j->fd, V4L2_BUF_TYPE_VIDEO_OUTPUT, &flags) == 0)
			got_in = 1;
		if (!got_out && (p.revents & POLLIN) &&
		    dequeue(j->fd, V4L2_BUF_TYPE_VIDEO_CAPTURE, &flags) == 0)
			got_out = 1;
	}
	if (flags & V4L2_BUF_FLAG_ERROR)
		return -EBADMSG;
	*out = j->out;
	*out_len = j->luma_size + j->luma_size / 2;
	return 0;
}
