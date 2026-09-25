/*
 * vetro-dev: esercita dal guest i dispositivi virtio di M5 (virtio-gpu,
 * virtio-input, virtio-vsock) con le interfacce del kernel, senza librerie.
 * Stampa solo valori deterministici (niente puntatori né tempi), così lo
 * stesso comando dà lo stesso log sotto QEMU e sotto Vetro.
 *
 *   vetro-dev drm                 connettore, modi, modeset di un dumb buffer
 *                                 con un motivo noto, cursore
 *   vetro-dev drm-hold            come drm, poi aspetta una riga su stdin
 *                                 prima di chiudere (l'host guarda lo scanout)
 *   vetro-dev input               capacità di ogni /dev/input/event*
 *   vetro-dev input-read DEV N    stampa i prossimi N eventi di DEV
 *   vetro-dev led DEV CODICE V    scrive un evento EV_LED su DEV
 *   vetro-dev vsock-cid           CID locale
 *   vetro-dev vsock-connect PORTA MSG
 *                                 si collega all'host (CID 2), manda MSG,
 *                                 chiude in scrittura e stampa la risposta
 *   vetro-dev vsock-listen PORTA  accetta una connessione, rimanda in
 *                                 maiuscolo quello che riceve
 *   vetro-dev xattr-set FILE NOME VALORE
 *                                 imposta un xattr (M8: prove del gestore
 *                                 dei file; BusyBox non ha setfattr)
 *   vetro-dev xattr-get FILE NOME stampa il valore di un xattr
 *
 * Compilato da tools/guest-kernel/build.sh con gli header UAPI del kernel
 * guest (drm/ non c'è negli header di Alpine).
 */
#include <ctype.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/xattr.h>
#include <unistd.h>
#include <linux/input.h>
#include <linux/vm_sockets.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>

#define P(...) printf("vetro-dev: " __VA_ARGS__)

static int die(const char *what)
{
	P("ERRORE %s: %s\n", what, strerror(errno));
	return 1;
}

/* ---- DRM ------------------------------------------------------------- */

/* Colore del pixel (x, y) del motivo di prova, in XRGB8888. */
static uint32_t pattern(uint32_t x, uint32_t y)
{
	return ((x & 0xff) << 16) | ((y & 0xff) << 8) | ((x ^ y) & 0xff);
}

static int drm(int hold)
{
	int fd = open("/dev/dri/card0", O_RDWR | O_CLOEXEC);
	if (fd < 0)
		return die("open /dev/dri/card0");

	char name[64] = {0};
	struct drm_version ver = {.name = name, .name_len = sizeof(name) - 1};
	if (ioctl(fd, DRM_IOCTL_VERSION, &ver))
		return die("DRM_IOCTL_VERSION");
	P("drm driver %s %d.%d.%d\n", name, ver.version_major, ver.version_minor,
	  ver.version_patchlevel);

	struct drm_mode_card_res res = {0};
	if (ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res))
		return die("GETRESOURCES");
	uint32_t crtcs[8], conns[8], encs[8], fbs[8];
	if (res.count_crtcs > 8 || res.count_connectors > 8 || res.count_encoders > 8 || res.count_fbs > 8)
		return die("troppi oggetti DRM");
	res.crtc_id_ptr = (uintptr_t)crtcs;
	res.connector_id_ptr = (uintptr_t)conns;
	res.encoder_id_ptr = (uintptr_t)encs;
	res.fb_id_ptr = (uintptr_t)fbs;
	if (ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res))
		return die("GETRESOURCES");
	P("drm crtc %u connettori %u encoder %u, fb da %ux%u a %ux%u\n", res.count_crtcs,
	  res.count_connectors, res.count_encoders, res.min_width, res.min_height, res.max_width,
	  res.max_height);
	if (res.count_connectors < 1 || res.count_crtcs < 1)
		return die("nessun connettore");

	struct drm_mode_get_connector conn = {.connector_id = conns[0]};
	if (ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &conn))
		return die("GETCONNECTOR");
	struct drm_mode_modeinfo modes[64];
	uint32_t conn_encs[8];
	if (conn.count_modes > 64)
		conn.count_modes = 64;
	if (conn.count_encoders > 8)
		conn.count_encoders = 8;
	conn.modes_ptr = (uintptr_t)modes;
	conn.encoders_ptr = (uintptr_t)conn_encs;
	conn.count_props = 0;
	if (ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &conn))
		return die("GETCONNECTOR");
	P("drm connettore tipo %u stato %u, %u modi, %ux%u mm\n", conn.connector_type,
	  conn.connection, conn.count_modes, conn.mm_width, conn.mm_height);
	int pref = -1;
	for (uint32_t i = 0; i < conn.count_modes; i++) {
		struct drm_mode_modeinfo *m = &modes[i];
		P("drm modo %s %u Hz clock %u tipo %#x%s\n", m->name, m->vrefresh, m->clock, m->type,
		  (m->type & DRM_MODE_TYPE_PREFERRED) ? " preferito" : "");
		if (pref < 0 && (m->type & DRM_MODE_TYPE_PREFERRED))
			pref = (int)i;
	}
	if (pref < 0)
		return die("nessun modo preferito");
	struct drm_mode_modeinfo mode = modes[pref];
	uint32_t w = mode.hdisplay, h = mode.vdisplay;

	/* Dumb buffer con il motivo, poi modeset. */
	struct drm_mode_create_dumb cd = {.width = w, .height = h, .bpp = 32};
	if (ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &cd))
		return die("CREATE_DUMB");
	P("drm dumb %ux%u pitch %u size %llu\n", w, h, cd.pitch, (unsigned long long)cd.size);
	struct drm_mode_fb_cmd fb = {.width = w, .height = h, .pitch = cd.pitch, .bpp = 32, .depth = 24,
				     .handle = cd.handle};
	if (ioctl(fd, DRM_IOCTL_MODE_ADDFB, &fb))
		return die("ADDFB");
	struct drm_mode_map_dumb md = {.handle = cd.handle};
	if (ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &md))
		return die("MAP_DUMB");
	uint8_t *px = mmap(0, cd.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, md.offset);
	if (px == MAP_FAILED)
		return die("mmap del dumb buffer");
	for (uint32_t y = 0; y < h; y++)
		for (uint32_t x = 0; x < w; x++)
			*(uint32_t *)(px + y * cd.pitch + 4 * x) = pattern(x, y);

	struct drm_mode_get_encoder enc = {.encoder_id = conn.encoder_id ? conn.encoder_id : conn_encs[0]};
	if (ioctl(fd, DRM_IOCTL_MODE_GETENCODER, &enc))
		return die("GETENCODER");
	uint32_t crtc_id = crtcs[0];
	struct drm_mode_crtc crtc = {.set_connectors_ptr = (uintptr_t)&conns[0], .count_connectors = 1,
				     .crtc_id = crtc_id, .fb_id = fb.fb_id, .mode_valid = 1, .mode = mode};
	if (ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &crtc))
		return die("SETCRTC");
	P("drm modeset %ux%u ok\n", w, h);

	/* Un rettangolo ridisegnato e segnalato con DIRTYFB (TRANSFER + FLUSH
	 * di una parte sola). */
	for (uint32_t y = 16; y < 48; y++)
		for (uint32_t x = 32; x < 96; x++)
			*(uint32_t *)(px + y * cd.pitch + 4 * x) = 0x00ffffff;
	struct drm_clip_rect clip = {.x1 = 32, .y1 = 16, .x2 = 96, .y2 = 48};
	struct drm_mode_fb_dirty_cmd dirty = {.fb_id = fb.fb_id, .num_clips = 1,
					      .clips_ptr = (uintptr_t)&clip};
	if (ioctl(fd, DRM_IOCTL_MODE_DIRTYFB, &dirty))
		return die("DIRTYFB");
	P("drm dirtyfb ok\n");

	/* Cursore 64x64 (coda cursor di virtio-gpu): definito, poi spostato. */
	struct drm_mode_create_dumb cc = {.width = 64, .height = 64, .bpp = 32};
	if (ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &cc))
		return die("CREATE_DUMB cursore");
	struct drm_mode_map_dumb cm = {.handle = cc.handle};
	if (ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &cm))
		return die("MAP_DUMB cursore");
	uint8_t *cp = mmap(0, cc.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, cm.offset);
	if (cp == MAP_FAILED)
		return die("mmap del cursore");
	for (uint32_t i = 0; i < 64 * 64; i++)
		((uint32_t *)cp)[i] = 0xff000000u | i;
	struct drm_mode_cursor cur = {.flags = DRM_MODE_CURSOR_BO, .crtc_id = crtc_id, .width = 64,
				      .height = 64, .handle = cc.handle};
	if (ioctl(fd, DRM_IOCTL_MODE_CURSOR, &cur))
		return die("CURSOR BO");
	struct drm_mode_cursor mv = {.flags = DRM_MODE_CURSOR_MOVE, .crtc_id = crtc_id, .x = 100, .y = 50};
	if (ioctl(fd, DRM_IOCTL_MODE_CURSOR, &mv))
		return die("CURSOR MOVE");
	P("drm cursore ok\n");

	if (hold) {
		P("VETRO-DRM-PRONTO\n");
		fflush(stdout);
		char line[16];
		if (!fgets(line, sizeof(line), stdin))
			return die("stdin");
	}
	munmap(cp, cc.size);
	munmap(px, cd.size);
	close(fd);
	P("drm chiuso\n");
	return 0;
}

/* ---- input ----------------------------------------------------------- */

static int test_bit(const uint8_t *bits, int n)
{
	return (bits[n / 8] >> (n % 8)) & 1;
}

/* Stampa i bit accesi come lista di numeri esadecimali. */
static void print_bits(const char *what, const uint8_t *bits, int max)
{
	printf("vetro-dev:   %s:", what);
	for (int i = 0; i < max; i++)
		if (test_bit(bits, i))
			printf(" %x", i);
	printf("\n");
}

static int input_caps(const char *path)
{
	int fd = open(path, O_RDONLY | O_CLOEXEC);
	if (fd < 0)
		return die(path);
	char s[256];
	struct input_id id;
	int v;
	memset(s, 0, sizeof(s));
	ioctl(fd, EVIOCGNAME(sizeof(s) - 1), s);
	P("%s nome \"%s\"\n", path, s);
	memset(s, 0, sizeof(s));
	ioctl(fd, EVIOCGPHYS(sizeof(s) - 1), s);
	P("  phys \"%s\"\n", s);
	memset(s, 0, sizeof(s));
	ioctl(fd, EVIOCGUNIQ(sizeof(s) - 1), s);
	P("  uniq \"%s\"\n", s);
	if (ioctl(fd, EVIOCGID, &id))
		return die("EVIOCGID");
	if (ioctl(fd, EVIOCGVERSION, &v))
		return die("EVIOCGVERSION");
	P("  id bus %04x vendor %04x product %04x version %04x, evdev %x\n", id.bustype, id.vendor,
	  id.product, id.version, v);
	uint8_t bits[KEY_MAX / 8 + 1];
	memset(bits, 0, sizeof(bits));
	ioctl(fd, EVIOCGPROP(sizeof(bits)), bits);
	print_bits("prop", bits, INPUT_PROP_MAX);
	uint8_t ev[EV_MAX / 8 + 1];
	memset(ev, 0, sizeof(ev));
	ioctl(fd, EVIOCGBIT(0, sizeof(ev)), ev);
	print_bits("ev", ev, EV_MAX);
	static const struct {
		int type, max;
		const char *name;
	} types[] = {{EV_KEY, KEY_MAX, "key"}, {EV_REL, REL_MAX, "rel"}, {EV_ABS, ABS_MAX, "abs"},
		     {EV_MSC, MSC_MAX, "msc"}, {EV_SW, SW_MAX, "sw"},     {EV_LED, LED_MAX, "led"},
		     {EV_SND, SND_MAX, "snd"}, {EV_FF, FF_MAX, "ff"}};
	for (unsigned t = 0; t < sizeof(types) / sizeof(types[0]); t++) {
		if (!test_bit(ev, types[t].type))
			continue;
		memset(bits, 0, sizeof(bits));
		ioctl(fd, EVIOCGBIT(types[t].type, sizeof(bits)), bits);
		print_bits(types[t].name, bits, types[t].max);
		if (types[t].type != EV_ABS)
			continue;
		for (int a = 0; a <= ABS_MAX; a++) {
			struct input_absinfo ai;
			if (!test_bit(bits, a) || ioctl(fd, EVIOCGABS(a), &ai))
				continue;
			P("  abs %x min %d max %d fuzz %d flat %d res %d\n", a, ai.minimum, ai.maximum,
			  ai.fuzz, ai.flat, ai.resolution);
		}
	}
	if (test_bit(ev, EV_REP)) {
		unsigned rep[2];
		if (!ioctl(fd, EVIOCGREP, rep))
			P("  rep ritardo %u periodo %u\n", rep[0], rep[1]);
	}
	close(fd);
	return 0;
}

static int input_all(void)
{
	int rc = 0;
	for (int i = 0; i < 16; i++) {
		char path[32];
		snprintf(path, sizeof(path), "/dev/input/event%d", i);
		if (access(path, F_OK))
			break;
		rc |= input_caps(path);
	}
	return rc;
}

static int input_read(const char *path, int n)
{
	int fd = open(path, O_RDONLY | O_CLOEXEC);
	if (fd < 0)
		return die(path);
	P("VETRO-INPUT-PRONTO %s\n", path);
	fflush(stdout);
	for (int i = 0; i < n; i++) {
		struct input_event e;
		if (read(fd, &e, sizeof(e)) != sizeof(e))
			return die("read evento");
		P("evento %u %u %d\n", e.type, e.code, e.value);
	}
	close(fd);
	return 0;
}

static int led(const char *path, int code, int value)
{
	int fd = open(path, O_WRONLY | O_CLOEXEC);
	if (fd < 0)
		return die(path);
	struct input_event e[2] = {{.type = EV_LED, .code = code, .value = value},
				   {.type = EV_SYN, .code = SYN_REPORT}};
	if (write(fd, e, sizeof(e)) != sizeof(e))
		return die("write EV_LED");
	close(fd);
	P("led %d = %d\n", code, value);
	return 0;
}

/* ---- vsock ----------------------------------------------------------- */

static int vsock_cid(void)
{
	int fd = open("/dev/vsock", O_RDONLY | O_CLOEXEC);
	if (fd < 0)
		return die("/dev/vsock");
	unsigned cid = 0;
	if (ioctl(fd, IOCTL_VM_SOCKETS_GET_LOCAL_CID, &cid))
		return die("GET_LOCAL_CID");
	P("vsock cid %u\n", cid);
	close(fd);
	return 0;
}

/* Legge fino alla fine del flusso. */
static ssize_t read_all(int s, char *buf, size_t cap)
{
	size_t n = 0;
	for (;;) {
		ssize_t r = read(s, buf + n, cap - n);
		if (r < 0)
			return -1;
		if (r == 0)
			return (ssize_t)n;
		n += (size_t)r;
		if (n == cap)
			return (ssize_t)n;
	}
}

static int vsock_connect(unsigned port, const char *msg)
{
	int s = socket(AF_VSOCK, SOCK_STREAM, 0);
	if (s < 0)
		return die("socket AF_VSOCK");
	struct sockaddr_vm a = {.svm_family = AF_VSOCK, .svm_cid = VMADDR_CID_HOST, .svm_port = port};
	if (connect(s, (struct sockaddr *)&a, sizeof(a)))
		return die("connect");
	P("vsock connesso a 2:%u\n", port);
	size_t len = strlen(msg);
	if (write(s, msg, len) != (ssize_t)len)
		return die("write");
	shutdown(s, SHUT_WR);
	static char buf[1 << 20];
	ssize_t n = read_all(s, buf, sizeof(buf));
	if (n < 0)
		return die("read");
	unsigned sum = 0;
	for (ssize_t i = 0; i < n; i++)
		sum = sum * 31 + (unsigned char)buf[i];
	if (n <= 64)
		P("vsock risposta %zd byte \"%.*s\"\n", n, (int)n, buf);
	else
		P("vsock risposta %zd byte, somma %08x\n", n, sum);
	close(s);
	return 0;
}

static int vsock_listen(unsigned port)
{
	int s = socket(AF_VSOCK, SOCK_STREAM, 0);
	if (s < 0)
		return die("socket AF_VSOCK");
	struct sockaddr_vm a = {.svm_family = AF_VSOCK, .svm_cid = VMADDR_CID_ANY, .svm_port = port};
	if (bind(s, (struct sockaddr *)&a, sizeof(a)) || listen(s, 1))
		return die("bind/listen");
	P("VETRO-VSOCK-ASCOLTO %u\n", port);
	fflush(stdout);
	struct sockaddr_vm peer;
	socklen_t pl = sizeof(peer);
	int c = accept(s, (struct sockaddr *)&peer, &pl);
	if (c < 0)
		return die("accept");
	P("vsock accettato da %u:%u\n", peer.svm_cid, peer.svm_port);
	static char buf[1 << 20];
	ssize_t n = read_all(c, buf, sizeof(buf));
	if (n < 0)
		return die("read");
	for (ssize_t i = 0; i < n; i++)
		buf[i] = (char)toupper((unsigned char)buf[i]);
	for (ssize_t off = 0; off < n;) {
		ssize_t w = write(c, buf + off, (size_t)(n - off));
		if (w <= 0)
			return die("write");
		off += w;
	}
	P("vsock rimandati %zd byte\n", n);
	close(c);
	close(s);
	return 0;
}

/* ---- xattr (M8) --------------------------------------------------- */

static int xattr_set(const char *path, const char *name, const char *value)
{
	if (lsetxattr(path, name, value, strlen(value), 0))
		return die("lsetxattr");
	return 0;
}

static int xattr_get(const char *path, const char *name)
{
	char buf[1024];
	ssize_t n = lgetxattr(path, name, buf, sizeof(buf));
	if (n < 0)
		return die("lgetxattr");
	P("xattr %s=%.*s\n", name, (int)n, buf);
	return 0;
}

int main(int argc, char **argv)
{
	setvbuf(stdout, NULL, _IOLBF, 0);
	const char *cmd = argc > 1 ? argv[1] : "";
	if (!strcmp(cmd, "drm"))
		return drm(0);
	if (!strcmp(cmd, "drm-hold"))
		return drm(1);
	if (!strcmp(cmd, "input"))
		return input_all();
	if (!strcmp(cmd, "input-read") && argc == 4)
		return input_read(argv[2], atoi(argv[3]));
	if (!strcmp(cmd, "led") && argc == 5)
		return led(argv[2], atoi(argv[3]), atoi(argv[4]));
	if (!strcmp(cmd, "vsock-cid"))
		return vsock_cid();
	if (!strcmp(cmd, "vsock-connect") && argc == 4)
		return vsock_connect((unsigned)atoi(argv[2]), argv[3]);
	if (!strcmp(cmd, "vsock-listen") && argc == 3)
		return vsock_listen((unsigned)atoi(argv[2]));
	if (!strcmp(cmd, "xattr-set") && argc == 5)
		return xattr_set(argv[2], argv[3], argv[4]);
	if (!strcmp(cmd, "xattr-get") && argc == 4)
		return xattr_get(argv[2], argv[3]);
	fprintf(stderr, "uso: vetro-dev drm|drm-hold|input|input-read DEV N|led DEV CODICE V|"
			"vsock-cid|vsock-connect PORTA MSG|vsock-listen PORTA|xattr-set FILE NOME VALORE|"
			"xattr-get FILE NOME\n");
	return 2;
}
