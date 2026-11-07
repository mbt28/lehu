/*
 * The V4L2 ioctls the camera needs, as plain calls. Written in C so the
 * kernel's own header lays out the structures: v4l2_buffer carries a
 * timeval whose size and alignment differ between 32-bit ABIs with 32- and
 * 64-bit time, and the ioctl numbers encode that size.
 *
 * Every call returns 0 (or a count, or a descriptor) on success and -errno
 * on failure, after retrying EINTR.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <linux/videodev2.h>

struct lehu_ctrl_info {
	uint32_t id;
	int32_t min, max, step, def;
	uint32_t type;
	uint32_t flags;
	char name[32];
};

static int xioctl(int fd, unsigned long req, void *arg)
{
	int r;
	do
		r = ioctl(fd, req, arg);
	while (r < 0 && errno == EINTR);
	return r < 0 ? -errno : 0;
}

/* Open and check for a streaming capture device that is not a codec
 * (memory-to-memory). -ENODEV when it is something else. */
int lehu_cam_open(const char *path)
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
	if (!(caps & V4L2_CAP_VIDEO_CAPTURE) || !(caps & V4L2_CAP_STREAMING) ||
	    (caps & (V4L2_CAP_VIDEO_M2M | V4L2_CAP_VIDEO_M2M_MPLANE))) {
		close(fd);
		return -ENODEV;
	}
	return fd;
}

int lehu_cam_enum_input(int fd, uint32_t index, char *name, size_t len, int *has_std,
			int *no_signal)
{
	struct v4l2_input in;
	memset(&in, 0, sizeof(in));
	in.index = index;
	int r = xioctl(fd, VIDIOC_ENUMINPUT, &in);
	if (r < 0)
		return r;
	if (name && len) {
		strncpy(name, (const char *)in.name, len - 1);
		name[len - 1] = 0;
	}
	if (has_std)
		*has_std = (in.capabilities & V4L2_IN_CAP_STD) != 0;
	if (no_signal)
		*no_signal = (in.status & V4L2_IN_ST_NO_SIGNAL) != 0;
	return 0;
}

int lehu_cam_set_input(int fd, int index)
{
	return xioctl(fd, VIDIOC_S_INPUT, &index);
}

int lehu_cam_query_std(int fd, uint64_t *std)
{
	v4l2_std_id id = 0;
	int r = xioctl(fd, VIDIOC_QUERYSTD, &id);
	*std = id;
	return r;
}

int lehu_cam_set_std(int fd, uint64_t std)
{
	v4l2_std_id id = std;
	return xioctl(fd, VIDIOC_S_STD, &id);
}

int lehu_cam_get_std(int fd, uint64_t *std)
{
	v4l2_std_id id = 0;
	int r = xioctl(fd, VIDIOC_G_STD, &id);
	*std = id;
	return r;
}

int lehu_cam_query_ctrl(int fd, uint32_t id, struct lehu_ctrl_info *out)
{
	struct v4l2_queryctrl q;
	memset(&q, 0, sizeof(q));
	q.id = id;
	int r = xioctl(fd, VIDIOC_QUERYCTRL, &q);
	if (r < 0)
		return r;
	out->id = q.id;
	out->min = q.minimum;
	out->max = q.maximum;
	out->step = q.step;
	out->def = q.default_value;
	out->type = q.type;
	out->flags = q.flags;
	strncpy(out->name, (const char *)q.name, sizeof(out->name) - 1);
	out->name[sizeof(out->name) - 1] = 0;
	return 0;
}

/* The next control after `id` (0 for the first), for walking them all. */
int lehu_cam_next_ctrl(int fd, uint32_t id, struct lehu_ctrl_info *out)
{
	return lehu_cam_query_ctrl(fd, id | V4L2_CTRL_FLAG_NEXT_CTRL, out);
}

int lehu_cam_query_menu(int fd, uint32_t id, uint32_t index, char *name, size_t len)
{
	struct v4l2_querymenu m;
	memset(&m, 0, sizeof(m));
	m.id = id;
	m.index = index;
	int r = xioctl(fd, VIDIOC_QUERYMENU, &m);
	if (r < 0)
		return r;
	strncpy(name, (const char *)m.name, len - 1);
	name[len - 1] = 0;
	return 0;
}

int lehu_cam_get_ctrl(int fd, uint32_t id, int32_t *value)
{
	struct v4l2_control c;
	memset(&c, 0, sizeof(c));
	c.id = id;
	int r = xioctl(fd, VIDIOC_G_CTRL, &c);
	*value = c.value;
	return r;
}

int lehu_cam_set_ctrl(int fd, uint32_t id, int32_t value)
{
	struct v4l2_control c;
	memset(&c, 0, sizeof(c));
	c.id = id;
	c.value = value;
	return xioctl(fd, VIDIOC_S_CTRL, &c);
}

/* The format the standard gives, asked for as NV12; -EINVAL when the
 * driver has no NV12. */
int lehu_cam_set_nv12(int fd, uint32_t *width, uint32_t *height, uint32_t *pitch)
{
	struct v4l2_format fmt;
	memset(&fmt, 0, sizeof(fmt));
	fmt.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
	int r = xioctl(fd, VIDIOC_G_FMT, &fmt);
	if (r < 0)
		return r;
	fmt.fmt.pix.pixelformat = V4L2_PIX_FMT_NV12;
	r = xioctl(fd, VIDIOC_S_FMT, &fmt);
	if (r < 0)
		return r;
	if (fmt.fmt.pix.pixelformat != V4L2_PIX_FMT_NV12)
		return -EINVAL;
	*width = fmt.fmt.pix.width;
	*height = fmt.fmt.pix.height;
	*pitch = fmt.fmt.pix.bytesperline;
	return 0;
}

/* Asks for `count` MMAP buffers; returns how many the driver gave (0 frees
 * them all). */
int lehu_cam_reqbufs(int fd, uint32_t count)
{
	struct v4l2_requestbuffers req;
	memset(&req, 0, sizeof(req));
	req.count = count;
	req.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
	req.memory = V4L2_MEMORY_MMAP;
	int r = xioctl(fd, VIDIOC_REQBUFS, &req);
	return r < 0 ? r : (int)req.count;
}

/* The buffer as a dma-buf descriptor. */
int lehu_cam_expbuf(int fd, uint32_t index)
{
	struct v4l2_exportbuffer eb;
	memset(&eb, 0, sizeof(eb));
	eb.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
	eb.index = index;
	eb.flags = O_RDWR | O_CLOEXEC;
	int r = xioctl(fd, VIDIOC_EXPBUF, &eb);
	return r < 0 ? r : eb.fd;
}

int lehu_cam_qbuf(int fd, uint32_t index)
{
	struct v4l2_buffer b;
	memset(&b, 0, sizeof(b));
	b.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
	b.memory = V4L2_MEMORY_MMAP;
	b.index = index;
	return xioctl(fd, VIDIOC_QBUF, &b);
}

/* -EAGAIN when no frame is ready (the descriptor is non-blocking). */
int lehu_cam_dqbuf(int fd, uint32_t *index)
{
	struct v4l2_buffer b;
	memset(&b, 0, sizeof(b));
	b.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
	b.memory = V4L2_MEMORY_MMAP;
	int r = xioctl(fd, VIDIOC_DQBUF, &b);
	if (r == 0)
		*index = b.index;
	return r;
}

int lehu_cam_stream(int fd, int on)
{
	int type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
	return xioctl(fd, on ? VIDIOC_STREAMON : VIDIOC_STREAMOFF, &type);
}
