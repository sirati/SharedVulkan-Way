/* wc-pw-node: the sound ring's devices as native PipeWire nodes (§106.3).
 *
 * wc-proxy (sound.rs) starts this once per session when the session's sound
 * server is PipeWire, and talks to it over stdin/stdout in the sound ring's
 * own frames (sv-transport soundring: an 8-byte header, type u8, flags u8,
 * stream u16 LE, len u32 LE, then the payload). Nothing here crosses the VM
 * boundary: both ends are on Linux; wc-proxy carries the frames on the ring.
 *
 *   in  0x10 Device   u8 dir (0 out, 1 in), u8 channels, u8 format (1 s16le),
 *                     u8 0, u32 rate, "node.name\0node.description"
 *   in  0x11 Gone
 *   in  0x20 Data     an input's samples, as the device side recorded them
 *   in  0x21 Credit   bytes an output may hand over (the device side's credit)
 *   out 0x20 Data     an output's samples, never beyond its credit
 *   out 0x21 Credit   bytes of an input consumed by the graph
 *   out 0x22 Start / 0x23 Stop   an input became / stopped being recorded from
 *
 * WHY A DRIVER. A pipe sink in PipeWire is a follower: the graph's clock (the
 * dummy driver) runs it every quantum whether or not anybody reads its FIFO,
 * so when the device side holds its credit the samples have nowhere to go and
 * are dropped (measured: 44 % of a tone lost for a 1 s hold). Here every node
 * is the DRIVER of its graph (PW_STREAM_FLAG_DRIVER, as module-pipe-tunnel's
 * source mode is): a timer at the quantum's period starts a cycle with
 * pw_stream_trigger_process only when the output's credit covers one, or the
 * input has a cycle's worth of the microphone. While credit is held no cycle
 * runs, so the application's stream waits; nothing is dropped and nothing is
 * invented.
 *
 * One thread: the main loop runs stdin, the timers and the streams' events
 * (no PW_STREAM_FLAG_RT_PROCESS), so there are no locks.
 */
#include <errno.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <pipewire/pipewire.h>
#include <spa/param/audio/format-utils.h>
#include <spa/utils/result.h>

#define T_DEVICE 0x10
#define T_GONE 0x11
#define T_DATA 0x20
#define T_CREDIT 0x21
#define T_START 0x22
#define T_STOP 0x23
#define MAX_PAYLOAD (64 * 1024)
/* An input buffers at most this much of the microphone; the session grants
 * the device side 32 KiB at a time, so this is never reached by a peer that
 * keeps to its credit. */
#define IN_CAP (1024 * 1024)

struct node {
	struct node *next;
	uint16_t id;
	int dir; /* 0 output (Audio/Sink), 1 input (Audio/Source) */
	uint32_t rate, channels, frame;
	struct pw_stream *stream;
	struct spa_hook listener;
	struct spa_io_position *position;
	struct spa_source *timer;
	bool streaming, driving;
	uint64_t next_time;
	/* output: credit from the device side, and samples the graph produced
	 * that the credit did not yet cover. */
	uint64_t credit;
	uint8_t *buf;
	size_t len, cap;
	uint64_t cycles, skipped;
};

static struct pw_main_loop *mainloop;
static struct pw_loop *loop;
static struct pw_core *core;
static struct node *nodes;
static uint8_t inbuf[2 * (MAX_PAYLOAD + 8)];
static size_t inlen;

static void die(const char *m)
{
	fprintf(stderr, "wc-pw-node: %s\n", m);
	exit(1);
}

static void put_u16(uint8_t *p, uint16_t v) { p[0] = v; p[1] = v >> 8; }
static void put_u32(uint8_t *p, uint32_t v) { p[0] = v; p[1] = v >> 8; p[2] = v >> 16; p[3] = v >> 24; }
static uint16_t get_u16(const uint8_t *p) { return p[0] | (p[1] << 8); }
static uint32_t get_u32(const uint8_t *p) { return p[0] | (p[1] << 8) | (p[2] << 16) | ((uint32_t)p[3] << 24); }

static void write_all(const void *p, size_t n)
{
	const uint8_t *b = p;
	while (n > 0) {
		ssize_t w = write(1, b, n);
		if (w < 0 && errno == EINTR)
			continue;
		if (w <= 0)
			die("stdout closed");
		b += w;
		n -= (size_t)w;
	}
}

static void send_frame(uint8_t type, uint16_t stream, const void *payload, uint32_t len)
{
	uint8_t h[8];
	h[0] = type;
	h[1] = 0;
	put_u16(h + 2, stream);
	put_u32(h + 4, len);
	write_all(h, 8);
	if (len)
		write_all(payload, len);
}

static uint64_t now_ns(void)
{
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return (uint64_t)t.tv_sec * SPA_NSEC_PER_SEC + (uint64_t)t.tv_nsec;
}

static struct node *find(uint16_t id)
{
	for (struct node *n = nodes; n; n = n->next)
		if (n->id == id)
			return n;
	return NULL;
}

static void reserve(struct node *n, size_t more)
{
	if (n->len + more <= n->cap)
		return;
	size_t c = n->cap ? n->cap : 65536;
	while (c < n->len + more)
		c *= 2;
	n->buf = realloc(n->buf, c);
	if (!n->buf)
		die("out of memory");
	n->cap = c;
}

static void consume(struct node *n, size_t k)
{
	memmove(n->buf, n->buf + k, n->len - k);
	n->len -= k;
}

/* Output: hand over what the credit covers, in whole frames, at most a
 * frame's payload at a time. */
static void flush_out(struct node *n)
{
	while (n->len > 0) {
		uint64_t k = n->len < n->credit ? n->len : n->credit;
		if (k > MAX_PAYLOAD)
			k = MAX_PAYLOAD;
		k -= k % n->frame;
		if (k == 0)
			return;
		send_frame(T_DATA, n->id, n->buf, (uint32_t)k);
		n->credit -= k;
		consume(n, (size_t)k);
	}
}

/* Bytes of this node's format one cycle of the graph needs, with room for a
 * graph rate that is not the node's. */
static size_t cycle_bytes(struct node *n)
{
	uint64_t dur = 1024, rate = n->rate;
	if (n->position) {
		dur = n->position->clock.target_duration;
		rate = n->position->clock.target_rate.denom;
	}
	if (rate == 0)
		rate = n->rate;
	return (size_t)((dur * n->rate + rate - 1) / rate + 8) * n->frame;
}

static void set_timer(struct node *n, uint64_t at)
{
	struct timespec t = { .tv_sec = at / SPA_NSEC_PER_SEC, .tv_nsec = at % SPA_NSEC_PER_SEC };
	struct timespec zero = { 0, 0 };
	pw_loop_update_timer(loop, n->timer, &t, &zero, true);
}

static void on_timer(void *data, uint64_t expirations)
{
	struct node *n = data;
	(void)expirations;
	if (!n->streaming)
		return;
	uint64_t dur = 1024, rate = 48000;
	struct spa_io_position *pos = n->position;
	if (pos) {
		dur = pos->clock.target_duration;
		rate = pos->clock.target_rate.denom;
	}
	uint64_t period = dur * SPA_NSEC_PER_SEC / (rate ? rate : 48000);
	uint64_t now = now_ns();
	bool go;
	if (n->dir == 0)
		go = n->len == 0 && n->credit >= cycle_bytes(n);
	else
		go = n->len >= cycle_bytes(n);
	if (go && n->driving) {
		if (pos) {
			pos->clock.nsec = n->next_time;
			pos->clock.rate = pos->clock.target_rate;
			pos->clock.position += pos->clock.duration;
			pos->clock.duration = pos->clock.target_duration;
			pos->clock.delay = 0;
			pos->clock.rate_diff = 1.0;
			pos->clock.next_nsec = n->next_time + period;
		}
		n->cycles++;
		pw_stream_trigger_process(n->stream);
	} else {
		n->skipped++;
	}
	n->next_time += period;
	/* After a hold, start again from now rather than catching up. */
	if (n->next_time + period < now)
		n->next_time = now;
	set_timer(n, n->next_time);
}

static void on_process(void *data)
{
	struct node *n = data;
	struct pw_buffer *b = pw_stream_dequeue_buffer(n->stream);
	if (!b)
		return;
	struct spa_data *d = &b->buffer->datas[0];
	if (n->dir == 0) {
		uint32_t offs = SPA_MIN(d->chunk->offset, d->maxsize);
		uint32_t size = SPA_MIN(d->chunk->size, d->maxsize - offs);
		if (d->data && size > 0) {
			reserve(n, size);
			memcpy(n->buf + n->len, SPA_PTROFF(d->data, offs, void), size);
			n->len += size;
		}
		pw_stream_queue_buffer(n->stream, b);
		flush_out(n);
	} else {
		size_t want = b->requested ? b->requested * n->frame : cycle_bytes(n);
		if (want > d->maxsize)
			want = d->maxsize;
		want -= want % n->frame;
		size_t got = n->len < want ? n->len : want;
		got -= got % n->frame;
		memcpy(d->data, n->buf, got);
		if (got < want)
			memset((uint8_t *)d->data + got, 0, want - got);
		d->chunk->offset = 0;
		d->chunk->size = (uint32_t)want;
		d->chunk->stride = (int32_t)n->frame;
		pw_stream_queue_buffer(n->stream, b);
		if (got > 0) {
			consume(n, got);
			uint8_t c[4];
			put_u32(c, (uint32_t)got);
			send_frame(T_CREDIT, n->id, c, 4);
		}
	}
}

static void on_state(void *data, enum pw_stream_state old, enum pw_stream_state state, const char *error)
{
	struct node *n = data;
	(void)old;
	switch (state) {
	case PW_STREAM_STATE_STREAMING:
		n->streaming = true;
		n->driving = pw_stream_is_driving(n->stream);
		fprintf(stderr, "wc-pw-node: stream %u streaming, %s\n", n->id,
			n->driving ? "driving its graph" : "NOT DRIVING: the graph's clock runs it");
		n->next_time = now_ns();
		set_timer(n, n->next_time);
		if (n->dir == 1)
			send_frame(T_START, n->id, NULL, 0);
		break;
	case PW_STREAM_STATE_PAUSED:
		if (n->streaming && n->dir == 1)
			send_frame(T_STOP, n->id, NULL, 0);
		n->streaming = false;
		set_timer(n, 0);
		break;
	case PW_STREAM_STATE_ERROR:
		fprintf(stderr, "wc-pw-node: stream %u: %s\n", n->id, error ? error : "error");
		break;
	default:
		break;
	}
}

static void on_io_changed(void *data, uint32_t id, void *area, uint32_t size)
{
	struct node *n = data;
	(void)size;
	if (id == SPA_IO_Position)
		n->position = area;
}

static const struct pw_stream_events stream_events = {
	PW_VERSION_STREAM_EVENTS,
	.state_changed = on_state,
	.io_changed = on_io_changed,
	.process = on_process,
};

static void add_node(uint16_t id, const uint8_t *p, uint32_t len)
{
	if (len < 8 || find(id))
		return;
	int dir = p[0];
	uint32_t channels = p[1], rate = get_u32(p + 4);
	if (p[2] != 1 || channels == 0 || channels > 8 || rate == 0) {
		fprintf(stderr, "wc-pw-node: stream %u: unsupported format\n", id);
		return;
	}
	char names[1024];
	uint32_t nl = len - 8 < sizeof(names) - 1 ? len - 8 : sizeof(names) - 1;
	memcpy(names, p + 8, nl);
	names[nl] = 0;
	const char *name = names;
	const char *desc = memchr(names, 0, nl) ? names + strlen(names) + 1 : names;

	struct node *n = calloc(1, sizeof(*n));
	if (!n)
		die("out of memory");
	n->id = id;
	n->dir = dir ? 1 : 0;
	n->rate = rate;
	n->channels = channels;
	n->frame = channels * 2;

	struct pw_properties *props = pw_properties_new(
		PW_KEY_MEDIA_TYPE, "Audio",
		PW_KEY_MEDIA_CLASS, n->dir == 0 ? "Audio/Sink" : "Audio/Source",
		PW_KEY_NODE_NAME, name,
		PW_KEY_NODE_DESCRIPTION, desc,
		PW_KEY_PRIORITY_DRIVER, "250000",
		PW_KEY_PRIORITY_SESSION, "1000",
		NULL);
	pw_properties_setf(props, "wc.sound.stream", "%u", id);
	n->stream = pw_stream_new(core, name, props);
	if (!n->stream)
		die("pw_stream_new failed");
	pw_stream_add_listener(n->stream, &n->listener, &stream_events, n);
	n->timer = pw_loop_add_timer(loop, on_timer, n);

	uint8_t pb[1024];
	struct spa_pod_builder b = SPA_POD_BUILDER_INIT(pb, sizeof(pb));
	struct spa_audio_info_raw info = { .format = SPA_AUDIO_FORMAT_S16_LE, .rate = rate, .channels = channels };
	if (channels == 1) {
		info.position[0] = SPA_AUDIO_CHANNEL_MONO;
	} else if (channels == 2) {
		info.position[0] = SPA_AUDIO_CHANNEL_FL;
		info.position[1] = SPA_AUDIO_CHANNEL_FR;
	} else {
		info.flags = SPA_AUDIO_FLAG_UNPOSITIONED;
	}
	const struct spa_pod *params[1] = { spa_format_audio_raw_build(&b, SPA_PARAM_EnumFormat, &info) };
	int res = pw_stream_connect(n->stream, n->dir == 0 ? PW_DIRECTION_INPUT : PW_DIRECTION_OUTPUT, PW_ID_ANY,
				    PW_STREAM_FLAG_DRIVER | PW_STREAM_FLAG_MAP_BUFFERS, params, 1);
	if (res < 0) {
		fprintf(stderr, "wc-pw-node: stream %u: connect: %s\n", id, spa_strerror(res));
		pw_stream_destroy(n->stream);
		pw_loop_destroy_source(loop, n->timer);
		free(n);
		return;
	}
	n->next = nodes;
	nodes = n;
	fprintf(stderr, "wc-pw-node: stream %u is %s %s (%s, %u Hz, %u ch)\n", id,
		n->dir == 0 ? "sink" : "source", name, desc, rate, channels);
}

static void remove_node(uint16_t id)
{
	for (struct node **pp = &nodes; *pp; pp = &(*pp)->next) {
		struct node *n = *pp;
		if (n->id != id)
			continue;
		*pp = n->next;
		fprintf(stderr, "wc-pw-node: stream %u gone (%llu cycles, %llu held)\n", id,
			(unsigned long long)n->cycles, (unsigned long long)n->skipped);
		pw_loop_destroy_source(loop, n->timer);
		pw_stream_destroy(n->stream);
		free(n->buf);
		free(n);
		return;
	}
}

static void on_frame(uint8_t type, uint16_t id, const uint8_t *p, uint32_t len)
{
	struct node *n;
	switch (type) {
	case T_DEVICE:
		add_node(id, p, len);
		break;
	case T_GONE:
		remove_node(id);
		break;
	case T_CREDIT:
		if (len >= 4 && (n = find(id)) && n->dir == 0) {
			n->credit += get_u32(p);
			flush_out(n);
		}
		break;
	case T_DATA:
		if ((n = find(id)) && n->dir == 1) {
			if (n->len + len > IN_CAP) {
				fprintf(stderr, "wc-pw-node: stream %u: more than its credit; dropped\n", id);
				break;
			}
			reserve(n, len);
			memcpy(n->buf + n->len, p, len);
			n->len += len;
		}
		break;
	default:
		break; /* unknown types are skipped by their length */
	}
}

static void on_stdin(void *data, int fd, uint32_t mask)
{
	(void)data;
	if (mask & SPA_IO_IN) {
		ssize_t r = read(fd, inbuf + inlen, sizeof(inbuf) - inlen);
		if (r <= 0) {
			pw_main_loop_quit(mainloop);
			return;
		}
		inlen += (size_t)r;
		size_t off = 0;
		while (inlen - off >= 8) {
			uint32_t len = get_u32(inbuf + off + 4);
			if (len > MAX_PAYLOAD)
				die("oversized frame");
			if (inlen - off < 8 + (size_t)len)
				break;
			on_frame(inbuf[off], get_u16(inbuf + off + 2), inbuf + off + 8, len);
			off += 8 + len;
		}
		memmove(inbuf, inbuf + off, inlen - off);
		inlen -= off;
	} else if (mask & (SPA_IO_HUP | SPA_IO_ERR)) {
		pw_main_loop_quit(mainloop);
	}
}

static void on_core_error(void *data, uint32_t id, int seq, int res, const char *message)
{
	(void)data;
	(void)seq;
	fprintf(stderr, "wc-pw-node: core error on %u: %s (%s)\n", id, message, spa_strerror(res));
	if (id == PW_ID_CORE && res == -EPIPE)
		pw_main_loop_quit(mainloop);
}

static const struct pw_core_events core_events = {
	PW_VERSION_CORE_EVENTS,
	.error = on_core_error,
};

int main(int argc, char **argv)
{
	(void)argc;
	(void)argv;
	signal(SIGPIPE, SIG_IGN);
	pw_init(NULL, NULL);
	mainloop = pw_main_loop_new(NULL);
	if (!mainloop)
		die("no main loop");
	loop = pw_main_loop_get_loop(mainloop);
	struct pw_context *ctx = pw_context_new(loop, NULL, 0);
	if (!ctx)
		die("no context");
	core = pw_context_connect(ctx, NULL, 0);
	if (!core)
		die("cannot connect to PipeWire (PIPEWIRE_REMOTE)");
	struct spa_hook core_listener;
	pw_core_add_listener(core, &core_listener, &core_events, NULL);
	pw_loop_add_io(loop, 0, SPA_IO_IN | SPA_IO_HUP | SPA_IO_ERR, false, on_stdin, NULL);
	fprintf(stderr, "wc-pw-node: connected (PipeWire %s)\n", pw_get_library_version());
	pw_main_loop_run(mainloop);
	while (nodes)
		remove_node(nodes->id);
	pw_core_disconnect(core);
	pw_context_destroy(ctx);
	pw_main_loop_destroy(mainloop);
	pw_deinit();
	return 0;
}
