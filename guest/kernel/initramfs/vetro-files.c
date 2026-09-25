/*
 * vetro-files: il demone del gestore dei file di Vetro nel guest (M8,
 * ADR 0020, protocollo in docs/specs/files.md).
 *
 * Ascolta su virtio-vsock (porta 5200, da qualsiasi CID) e serve all'host
 * letture e scritture sui file del guest passando dal kernel del guest:
 * niente accesso diretto all'immagine del disco, che con il guest acceso
 * corromperebbe il file system.
 *
 *   list, stat (tipo, dimensione, mtime, modo, uid/gid, destinazione dei
 *   collegamenti, contesto SELinux dall'xattr security.selinux se c'è),
 *   lettura a pezzi, scrittura atomica (file temporaneo nella stessa
 *   cartella, poi rename: proprietario, modo e xattr del file che si
 *   sostituisce si conservano; i file nuovi prendono proprietario e
 *   contesto SELinux della cartella), create, mkdir, delete (anche
 *   ricorsivo), rename, watch con inotify (eventi dal vivo).
 *
 * Un solo processo, un ciclo poll(): fino a MAX_CLIENTS connessioni, ognuna
 * con il suo inotify e un buffer d'uscita (scritture non bloccanti: un host
 * che non legge non ferma gli altri). Stampa solo in caso di errore fatale.
 *
 * Compilato statico con musl da tools/guest-kernel/build.sh (come
 * vetro-dev) e avviato da /init se c'è un dispositivo virtio-vsock. In
 * futuro va nell'immagine Android (userdebug, root), con bionic: usa solo
 * POSIX e header UAPI di Linux.
 *
 *   vetro-files [-p PORTA]
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <ftw.h>
#include <limits.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/xattr.h>
#include <unistd.h>
#include <linux/vm_sockets.h>

#define DEFAULT_PORT 5200
#define MAGIC 0x46525456u /* "VTRF" in little endian */
#define VERSION 1
/* Byte al più per READ e WDATA. */
#define MAX_CHUNK (1u << 20)
/* Lunghezza al più di una richiesta (WDATA più il resto). */
#define MAX_REQUEST (MAX_CHUNK + 16384u)
#define MAX_CLIENTS 8
#define MAX_HANDLES 16
/* Oltre questi byte in uscita non si leggono altre richieste né eventi. */
#define OUT_HIGH (8u << 20)
#define TMP_PREFIX ".vetro-tmp."
#define SELINUX_XATTR "security.selinux"

enum {
	T_STAT = 1,
	T_LIST = 2,
	T_READ = 3,
	T_WOPEN = 4,
	T_WDATA = 5,
	T_WCOMMIT = 6,
	T_WABORT = 7,
	T_MKDIR = 8,
	T_CREATE = 9,
	T_DELETE = 10,
	T_RENAME = 11,
	T_WATCH = 12,
	T_UNWATCH = 13,
	T_HELLO = 0x80,
	T_REPLY = 0x81,
	T_EVENT = 0x82,
};

enum { K_OTHER, K_FILE, K_DIR, K_SYMLINK, K_CHAR, K_BLOCK, K_FIFO, K_SOCKET };

#define WATCH_MASK                                                                                 \
	(IN_CREATE | IN_DELETE | IN_MODIFY | IN_CLOSE_WRITE | IN_MOVED_FROM | IN_MOVED_TO | IN_ATTRIB | \
	 IN_DELETE_SELF | IN_MOVE_SELF)

struct buf {
	uint8_t *p;
	size_t len, cap;
};

struct handle {
	int used;
	uint32_t id;
	int fd;
	int err; /* primo errore di una WDATA: la WCOMMIT fallisce */
	int existed;
	struct stat st; /* del file sostituito */
	uint32_t mode;  /* per un file nuovo */
	char *tmp, *target;
};

struct client {
	int fd, ino;
	struct buf in, out;
	size_t out_off;
	struct handle h[MAX_HANDLES];
};

static struct client clients[MAX_CLIENTS];
static unsigned long tmp_counter;

/* ---- buffer ------------------------------------------------------------ */

static void need(struct buf *b, size_t n)
{
	if (b->len + n <= b->cap)
		return;
	size_t cap = b->cap ? b->cap : 4096;
	while (cap < b->len + n)
		cap *= 2;
	uint8_t *p = realloc(b->p, cap);
	if (!p) {
		fprintf(stderr, "vetro-files: memoria esaurita\n");
		exit(1);
	}
	b->p = p;
	b->cap = cap;
}

static void put(struct buf *b, const void *d, size_t n)
{
	need(b, n);
	memcpy(b->p + b->len, d, n);
	b->len += n;
}

static void put_u8(struct buf *b, uint8_t v) { put(b, &v, 1); }

static void put_u16(struct buf *b, uint16_t v)
{
	uint8_t x[2] = {(uint8_t)v, (uint8_t)(v >> 8)};
	put(b, x, 2);
}

static void put_u32(struct buf *b, uint32_t v)
{
	uint8_t x[4] = {(uint8_t)v, (uint8_t)(v >> 8), (uint8_t)(v >> 16), (uint8_t)(v >> 24)};
	put(b, x, 4);
}

static void put_u64(struct buf *b, uint64_t v)
{
	put_u32(b, (uint32_t)v);
	put_u32(b, (uint32_t)(v >> 32));
}

static void put_str(struct buf *b, const char *s, size_t n)
{
	if (n > 0xffff)
		n = 0xffff;
	put_u16(b, (uint16_t)n);
	put(b, s, n);
}

static void put_cstr(struct buf *b, const char *s) { put_str(b, s, strlen(s)); }

static void set_u32(struct buf *b, size_t at, uint32_t v)
{
	b->p[at] = (uint8_t)v;
	b->p[at + 1] = (uint8_t)(v >> 8);
	b->p[at + 2] = (uint8_t)(v >> 16);
	b->p[at + 3] = (uint8_t)(v >> 24);
}

/* Inizio di un frame: lunghezza (da riempire), tipo, id. */
static size_t frame_begin(struct buf *b, uint8_t type, uint32_t id)
{
	size_t at = b->len;
	put_u32(b, 0);
	put_u8(b, type);
	put_u32(b, id);
	return at;
}

static void frame_end(struct buf *b, size_t at) { set_u32(b, at, (uint32_t)(b->len - at - 4)); }

/* ---- lettura delle richieste ---------------------------------------------- */

struct rd {
	const uint8_t *p;
	size_t len, pos;
	int bad;
};

static const uint8_t *take(struct rd *r, size_t n)
{
	if (r->bad || r->len - r->pos < n) {
		r->bad = 1;
		return NULL;
	}
	const uint8_t *p = r->p + r->pos;
	r->pos += n;
	return p;
}

static uint8_t get_u8(struct rd *r)
{
	const uint8_t *p = take(r, 1);
	return p ? p[0] : 0;
}

static uint16_t get_u16(struct rd *r)
{
	const uint8_t *p = take(r, 2);
	return p ? (uint16_t)(p[0] | p[1] << 8) : 0;
}

static uint32_t get_u32(struct rd *r)
{
	const uint8_t *p = take(r, 4);
	return p ? (uint32_t)p[0] | (uint32_t)p[1] << 8 | (uint32_t)p[2] << 16 | (uint32_t)p[3] << 24 : 0;
}

static uint64_t get_u64(struct rd *r)
{
	uint64_t lo = get_u32(r);
	return lo | (uint64_t)get_u32(r) << 32;
}

/* Un percorso: u16 lunghezza, byte senza NUL. Restituisce una copia con NUL. */
static char *get_path(struct rd *r)
{
	uint16_t n = get_u16(r);
	const uint8_t *p = take(r, n);
	if (!p || n == 0 || memchr(p, 0, n)) {
		r->bad = 1;
		return NULL;
	}
	char *s = malloc((size_t)n + 1);
	if (!s)
		exit(1);
	memcpy(s, p, n);
	s[n] = 0;
	return s;
}

/* ---- stat --------------------------------------------------------------- */

static uint8_t kind_of(mode_t m)
{
	if (S_ISREG(m))
		return K_FILE;
	if (S_ISDIR(m))
		return K_DIR;
	if (S_ISLNK(m))
		return K_SYMLINK;
	if (S_ISCHR(m))
		return K_CHAR;
	if (S_ISBLK(m))
		return K_BLOCK;
	if (S_ISFIFO(m))
		return K_FIFO;
	if (S_ISSOCK(m))
		return K_SOCKET;
	return K_OTHER;
}

/* Contesto SELinux del file (senza seguire i collegamenti); 0 se non c'è. */
static ssize_t selinux_of(const char *path, char *out, size_t cap)
{
	ssize_t n = lgetxattr(path, SELINUX_XATTR, out, cap);
	if (n < 0)
		return 0;
	while (n > 0 && out[n - 1] == 0)
		n--;
	return n;
}

/* kind, mode, uid, gid, size, mtime (s, ns), nlink, destinazione, contesto. */
static void put_stat(struct buf *b, const char *path, const struct stat *st)
{
	put_u8(b, kind_of(st->st_mode));
	put_u32(b, (uint32_t)st->st_mode);
	put_u32(b, (uint32_t)st->st_uid);
	put_u32(b, (uint32_t)st->st_gid);
	put_u64(b, (uint64_t)st->st_size);
	put_u64(b, (uint64_t)(int64_t)st->st_mtim.tv_sec);
	put_u32(b, (uint32_t)st->st_mtim.tv_nsec);
	put_u32(b, (uint32_t)st->st_nlink);
	char link[PATH_MAX];
	ssize_t n = S_ISLNK(st->st_mode) ? readlink(path, link, sizeof(link)) : 0;
	put_str(b, link, n > 0 ? (size_t)n : 0);
	char ctx[256];
	put_str(b, ctx, (size_t)selinux_of(path, ctx, sizeof(ctx)));
}

/* ---- operazioni ---------------------------------------------------------- */

static int cmp_names(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }

static int do_list(struct buf *o, const char *path)
{
	DIR *d = opendir(path);
	if (!d)
		return errno;
	size_t n = 0, cap = 64;
	char **names = malloc(cap * sizeof(char *));
	struct dirent *e;
	while ((e = readdir(d))) {
		if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, ".."))
			continue;
		if (n == cap)
			names = realloc(names, (cap *= 2) * sizeof(char *));
		names[n++] = strdup(e->d_name);
	}
	closedir(d);
	/* Ordine fisso (strcmp), qualunque sia quello del file system. */
	qsort(names, n, sizeof(char *), cmp_names);
	size_t count_at = o->len;
	put_u32(o, 0);
	uint32_t count = 0;
	size_t plen = strlen(path);
	for (size_t i = 0; i < n; i++) {
		size_t len = plen + 1 + strlen(names[i]) + 1;
		char *full = malloc(len);
		snprintf(full, len, "%s%s%s", path, plen && path[plen - 1] == '/' ? "" : "/", names[i]);
		struct stat st;
		if (lstat(full, &st) == 0) {
			put_cstr(o, names[i]);
			put_stat(o, full, &st);
			count++;
		}
		free(full);
		free(names[i]);
	}
	free(names);
	set_u32(o, count_at, count);
	return 0;
}

static int do_read(struct buf *o, const char *path, uint64_t off, uint32_t len)
{
	if (len > MAX_CHUNK)
		len = MAX_CHUNK;
	int fd = open(path, O_RDONLY | O_CLOEXEC);
	if (fd < 0)
		return errno;
	struct stat st;
	if (fstat(fd, &st)) {
		int e = errno;
		close(fd);
		return e;
	}
	if (S_ISDIR(st.st_mode)) {
		close(fd);
		return EISDIR;
	}
	put_u64(o, (uint64_t)st.st_size);
	size_t len_at = o->len;
	put_u32(o, 0);
	need(o, len);
	uint32_t got = 0;
	while (got < len) {
		ssize_t r = pread(fd, o->p + o->len, len - got, (off_t)(off + got));
		if (r < 0) {
			if (errno == EINTR)
				continue;
			int e = errno;
			close(fd);
			return e;
		}
		if (r == 0)
			break;
		o->len += (size_t)r;
		got += (uint32_t)r;
	}
	close(fd);
	set_u32(o, len_at, got);
	return 0;
}

/* Cartella e nome di un percorso (copie). */
static void split(const char *path, char **dir, char **base)
{
	const char *s = strrchr(path, '/');
	if (!s) {
		*dir = strdup(".");
		*base = strdup(path);
	} else if (s == path) {
		*dir = strdup("/");
		*base = strdup(s + 1);
	} else {
		*dir = strndup(path, (size_t)(s - path));
		*base = strdup(s + 1);
	}
}

/*
 * Proprietario e contesto SELinux della cartella che contiene `path` su un
 * file nuovo (`fd` >= 0) o su `path` stesso: come fa Android per i file di
 * un'app, che hanno uid, gid e contesto della sua cartella dei dati.
 */
static int inherit_parent(const char *path, int fd)
{
	char *dir, *base;
	split(path, &dir, &base);
	struct stat ps;
	int e = 0;
	if (stat(dir, &ps))
		e = errno;
	else if ((fd >= 0 ? fchown(fd, ps.st_uid, ps.st_gid) : lchown(path, ps.st_uid, ps.st_gid)) && errno != EPERM)
		e = errno;
	if (!e) {
		char ctx[256];
		ssize_t n = lgetxattr(dir, SELINUX_XATTR, ctx, sizeof(ctx));
		if (n > 0 && (fd >= 0 ? fsetxattr(fd, SELINUX_XATTR, ctx, (size_t)n, 0)
				      : lsetxattr(path, SELINUX_XATTR, ctx, (size_t)n, 0)))
			e = errno;
	}
	free(dir);
	free(base);
	return e;
}

/* Copia gli xattr di `src` su `fd`. security.selinux è obbligatorio. */
static int copy_xattrs(const char *src, int fd)
{
	ssize_t n = llistxattr(src, NULL, 0);
	if (n <= 0)
		return 0; /* nessuno, o file system senza xattr */
	char *names = malloc((size_t)n);
	n = llistxattr(src, names, (size_t)n);
	int e = 0;
	for (ssize_t i = 0; n > 0 && i < n && !e; i += (ssize_t)strlen(names + i) + 1) {
		const char *name = names + i;
		ssize_t vn = lgetxattr(src, name, NULL, 0);
		if (vn < 0)
			continue;
		char *v = malloc(vn ? (size_t)vn : 1);
		vn = lgetxattr(src, name, v, (size_t)vn);
		if (vn >= 0 && fsetxattr(fd, name, v, (size_t)vn, 0) && !strcmp(name, SELINUX_XATTR))
			e = errno;
		free(v);
	}
	free(names);
	return e;
}

static struct handle *find_handle(struct client *c, uint32_t id)
{
	for (int i = 0; i < MAX_HANDLES; i++)
		if (c->h[i].used && c->h[i].id == id)
			return &c->h[i];
	return NULL;
}

static void drop_handle(struct handle *h, int unlink_tmp)
{
	if (h->fd >= 0)
		close(h->fd);
	if (unlink_tmp && h->tmp)
		unlink(h->tmp);
	free(h->tmp);
	free(h->target);
	memset(h, 0, sizeof(*h));
	h->fd = -1;
}

static int do_wopen(struct client *c, uint32_t id, const char *path, uint32_t mode, uint8_t flags)
{
	if (find_handle(c, id))
		return EBUSY;
	struct handle *h = NULL;
	for (int i = 0; i < MAX_HANDLES && !h; i++)
		if (!c->h[i].used)
			h = &c->h[i];
	if (!h)
		return EMFILE;
	/* Un collegamento simbolico resta: si sostituisce il file a cui punta. */
	struct stat ls;
	char *target = NULL;
	if (lstat(path, &ls) == 0 && S_ISLNK(ls.st_mode)) {
		target = realpath(path, NULL);
		if (!target)
			return errno;
	} else {
		target = strdup(path);
	}
	struct stat st;
	int existed = stat(target, &st) == 0;
	int e = 0;
	if (!existed && errno != ENOENT)
		e = errno;
	else if (existed && S_ISDIR(st.st_mode))
		e = EISDIR;
	else if (existed && !S_ISREG(st.st_mode))
		e = EINVAL;
	else if (existed && (flags & 1))
		e = EEXIST;
	if (e) {
		free(target);
		return e;
	}
	char *dir, *base;
	split(target, &dir, &base);
	size_t len = strlen(dir) + strlen(base) + 64;
	char *tmp = malloc(len);
	/* Nomi lunghi: senza il nome originale (NAME_MAX). */
	snprintf(tmp, len, "%s/%s%lu.%s", dir, TMP_PREFIX, ++tmp_counter, strlen(base) > 200 ? "x" : base);
	free(dir);
	free(base);
	int fd = open(tmp, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
	if (fd < 0) {
		e = errno;
		free(tmp);
		free(target);
		return e;
	}
	memset(h, 0, sizeof(*h));
	h->used = 1;
	h->id = id;
	h->fd = fd;
	h->existed = existed;
	if (existed)
		h->st = st;
	h->mode = mode & 07777;
	h->tmp = tmp;
	h->target = target;
	return 0;
}

static int do_wdata(struct client *c, uint32_t id, uint64_t off, const uint8_t *data, uint32_t len)
{
	struct handle *h = find_handle(c, id);
	if (!h)
		return EBADF;
	if (h->err)
		return h->err;
	for (uint32_t done = 0; done < len;) {
		ssize_t w = pwrite(h->fd, data + done, len - done, (off_t)(off + done));
		if (w < 0 && errno == EINTR)
			continue;
		if (w <= 0) {
			h->err = w < 0 ? errno : EIO;
			return h->err;
		}
		done += (uint32_t)w;
	}
	return 0;
}

static int do_wcommit(struct client *c, struct buf *o, uint32_t id)
{
	struct handle *h = find_handle(c, id);
	if (!h)
		return EBADF;
	int e = h->err;
	if (!e && h->existed) {
		/* chown prima di chmod: chown toglie setuid e setgid. */
		if (fchown(h->fd, h->st.st_uid, h->st.st_gid) || fchmod(h->fd, h->st.st_mode & 07777))
			e = errno;
		else
			e = copy_xattrs(h->target, h->fd);
	} else if (!e) {
		e = inherit_parent(h->target, h->fd);
		if (!e && fchmod(h->fd, h->mode))
			e = errno;
	}
	if (!e && fsync(h->fd))
		e = errno;
	if (!e) {
		close(h->fd);
		h->fd = -1;
		if (rename(h->tmp, h->target))
			e = errno;
	}
	if (e) {
		drop_handle(h, 1);
		return e;
	}
	char *dir, *base;
	split(h->target, &dir, &base);
	int dfd = open(dir, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
	if (dfd >= 0) {
		fsync(dfd);
		close(dfd);
	}
	free(dir);
	free(base);
	struct stat st;
	if (lstat(h->target, &st))
		e = errno;
	else
		put_stat(o, h->target, &st);
	drop_handle(h, 0);
	return e;
}

static int do_mkdir(const char *path, uint32_t mode)
{
	if (mkdir(path, mode & 07777))
		return errno;
	int e = inherit_parent(path, -1);
	if (!e && chmod(path, mode & 07777))
		e = errno;
	return e;
}

static int do_create(const char *path, uint32_t mode)
{
	int fd = open(path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, mode & 07777);
	if (fd < 0)
		return errno;
	int e = inherit_parent(path, fd);
	if (!e && fchmod(fd, mode & 07777))
		e = errno;
	close(fd);
	return e;
}

static int rm_one(const char *path, const struct stat *st, int flag, struct FTW *f)
{
	(void)st;
	(void)f;
	return (flag == FTW_DP ? rmdir(path) : unlink(path)) ? errno : 0;
}

static int do_delete(const char *path, uint8_t flags)
{
	struct stat st;
	if (lstat(path, &st))
		return errno;
	if (!S_ISDIR(st.st_mode))
		return unlink(path) ? errno : 0;
	if (flags & 1) {
		int r = nftw(path, rm_one, 16, FTW_DEPTH | FTW_PHYS);
		return r < 0 ? errno : r;
	}
	return rmdir(path) ? errno : 0;
}

/* ---- connessioni ---------------------------------------------------------- */

static void reply_hello(struct client *c)
{
	size_t at = frame_begin(&c->out, T_HELLO, 0);
	put_u32(&c->out, MAGIC);
	put_u16(&c->out, VERSION);
	put_u16(&c->out, access("/sys/fs/selinux", F_OK) == 0 ? 1 : 0);
	put_u32(&c->out, MAX_CHUNK);
	frame_end(&c->out, at);
}

static void handle_request(struct client *c, const uint8_t *p, size_t len)
{
	struct rd r = {p, len, 0, 0};
	uint8_t type = get_u8(&r);
	uint32_t id = get_u32(&r);
	struct buf *o = &c->out;
	size_t at = frame_begin(o, T_REPLY, id);
	size_t status_at = o->len;
	put_u32(o, 0);
	size_t body = o->len;
	int e = EPROTO;
	char *path = NULL, *path2 = NULL;
	switch (type) {
	case T_STAT: {
		path = get_path(&r);
		struct stat st;
		if (r.bad)
			break;
		e = lstat(path, &st) ? errno : 0;
		if (!e)
			put_stat(o, path, &st);
		break;
	}
	case T_LIST:
		path = get_path(&r);
		if (!r.bad)
			e = do_list(o, path);
		break;
	case T_READ: {
		path = get_path(&r);
		uint64_t off = get_u64(&r);
		uint32_t n = get_u32(&r);
		if (!r.bad)
			e = do_read(o, path, off, n);
		break;
	}
	case T_WOPEN: {
		uint32_t h = get_u32(&r);
		path = get_path(&r);
		uint32_t mode = get_u32(&r);
		uint8_t flags = get_u8(&r);
		if (!r.bad)
			e = do_wopen(c, h, path, mode, flags);
		break;
	}
	case T_WDATA: {
		uint32_t h = get_u32(&r);
		uint64_t off = get_u64(&r);
		uint32_t n = get_u32(&r);
		const uint8_t *d = take(&r, n);
		if (!r.bad)
			e = n > MAX_CHUNK ? EMSGSIZE : do_wdata(c, h, off, d, n);
		break;
	}
	case T_WCOMMIT: {
		uint32_t h = get_u32(&r);
		if (!r.bad)
			e = do_wcommit(c, o, h);
		break;
	}
	case T_WABORT: {
		uint32_t h = get_u32(&r);
		struct handle *hd = r.bad ? NULL : find_handle(c, h);
		e = r.bad ? EPROTO : hd ? 0 : EBADF;
		if (hd)
			drop_handle(hd, 1);
		break;
	}
	case T_MKDIR: {
		path = get_path(&r);
		uint32_t mode = get_u32(&r);
		if (!r.bad)
			e = do_mkdir(path, mode);
		break;
	}
	case T_CREATE: {
		path = get_path(&r);
		uint32_t mode = get_u32(&r);
		if (!r.bad)
			e = do_create(path, mode);
		break;
	}
	case T_DELETE: {
		path = get_path(&r);
		uint8_t flags = get_u8(&r);
		if (!r.bad)
			e = do_delete(path, flags);
		break;
	}
	case T_RENAME:
		path = get_path(&r);
		path2 = get_path(&r);
		if (!r.bad)
			e = rename(path, path2) ? errno : 0;
		break;
	case T_WATCH: {
		path = get_path(&r);
		if (r.bad)
			break;
		int wd = c->ino < 0 ? (errno = ENOSYS, -1) : inotify_add_watch(c->ino, path, WATCH_MASK);
		e = wd < 0 ? errno : 0;
		if (!e)
			put_u32(o, (uint32_t)wd);
		break;
	}
	case T_UNWATCH: {
		uint32_t wd = get_u32(&r);
		if (!r.bad)
			e = inotify_rm_watch(c->ino, (int)wd) ? errno : 0;
		break;
	}
	default:
		e = ENOSYS;
		break;
	}
	free(path);
	free(path2);
	if (e)
		o->len = body; /* niente corpo con un errore */
	set_u32(o, status_at, (uint32_t)e);
	frame_end(o, at);
}

static void drop_client(struct client *c)
{
	for (int i = 0; i < MAX_HANDLES; i++)
		if (c->h[i].used)
			drop_handle(&c->h[i], 1);
	close(c->fd);
	if (c->ino >= 0)
		close(c->ino);
	free(c->in.p);
	free(c->out.p);
	memset(c, 0, sizeof(*c));
	c->fd = -1;
}

/* Scrive quanto il socket accetta. -1 se la connessione è persa. */
static int flush_out(struct client *c)
{
	while (c->out_off < c->out.len) {
		ssize_t w = send(c->fd, c->out.p + c->out_off, c->out.len - c->out_off, MSG_NOSIGNAL);
		if (w < 0 && errno == EINTR)
			continue;
		if (w < 0 && (errno == EAGAIN || errno == EWOULDBLOCK))
			return 0;
		if (w <= 0)
			return -1;
		c->out_off += (size_t)w;
	}
	c->out.len = 0;
	c->out_off = 0;
	return 0;
}

/* Legge dal socket e serve le richieste complete. -1 se la connessione è finita. */
static int serve_input(struct client *c)
{
	uint8_t tmp[65536];
	ssize_t n = recv(c->fd, tmp, sizeof(tmp), 0);
	if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR))
		return 0;
	if (n <= 0)
		return -1;
	put(&c->in, tmp, (size_t)n);
	size_t pos = 0;
	while (c->in.len - pos >= 4) {
		const uint8_t *p = c->in.p + pos;
		uint32_t len = (uint32_t)p[0] | (uint32_t)p[1] << 8 | (uint32_t)p[2] << 16 | (uint32_t)p[3] << 24;
		if (len < 5 || len > MAX_REQUEST)
			return -1;
		if (c->in.len - pos - 4 < len)
			break;
		handle_request(c, p + 4, len);
		pos += 4 + (size_t)len;
	}
	memmove(c->in.p, c->in.p + pos, c->in.len - pos);
	c->in.len -= pos;
	return 0;
}

/* Eventi di inotify verso l'host, tranne quelli dei nostri file temporanei. */
static int serve_events(struct client *c)
{
	char evbuf[65536] __attribute__((aligned(__alignof__(struct inotify_event))));
	ssize_t n = read(c->ino, evbuf, sizeof(evbuf));
	if (n <= 0)
		return 0;
	for (char *p = evbuf; p < evbuf + n;) {
		struct inotify_event *ev = (struct inotify_event *)p;
		const char *name = ev->len ? ev->name : "";
		if (strncmp(name, TMP_PREFIX, strlen(TMP_PREFIX))) {
			size_t at = frame_begin(&c->out, T_EVENT, 0);
			put_u32(&c->out, (uint32_t)ev->wd);
			put_u32(&c->out, ev->mask);
			put_u32(&c->out, ev->cookie);
			put_cstr(&c->out, name);
			frame_end(&c->out, at);
		}
		p += sizeof(struct inotify_event) + ev->len;
	}
	return 0;
}

int main(int argc, char **argv)
{
	unsigned port = DEFAULT_PORT;
	if (argc == 3 && !strcmp(argv[1], "-p"))
		port = (unsigned)strtoul(argv[2], NULL, 10);
	else if (argc != 1) {
		fprintf(stderr, "uso: vetro-files [-p PORTA]\n");
		return 2;
	}
	signal(SIGPIPE, SIG_IGN);
	/* I modi chiesti dall'host valgono così come sono. */
	umask(0);
	int ls = socket(AF_VSOCK, SOCK_STREAM | SOCK_CLOEXEC, 0);
	struct sockaddr_vm a = {.svm_family = AF_VSOCK, .svm_cid = VMADDR_CID_ANY, .svm_port = port};
	if (ls < 0 || bind(ls, (struct sockaddr *)&a, sizeof(a)) || listen(ls, MAX_CLIENTS)) {
		fprintf(stderr, "vetro-files: vsock porta %u: %s\n", port, strerror(errno));
		return 1;
	}
	for (int i = 0; i < MAX_CLIENTS; i++)
		clients[i].fd = -1;
	for (;;) {
		struct pollfd pf[1 + 2 * MAX_CLIENTS];
		int who[1 + 2 * MAX_CLIENTS];
		int n = 0;
		pf[n] = (struct pollfd){.fd = ls, .events = POLLIN};
		who[n++] = -1;
		for (int i = 0; i < MAX_CLIENTS; i++) {
			struct client *c = &clients[i];
			if (c->fd < 0)
				continue;
			int busy = c->out.len - c->out_off > OUT_HIGH;
			pf[n] = (struct pollfd){.fd = c->fd,
						.events = (short)((busy ? 0 : POLLIN) | (c->out.len > c->out_off ? POLLOUT : 0))};
			who[n++] = i;
			pf[n] = (struct pollfd){.fd = busy || c->ino < 0 ? -1 : c->ino, .events = POLLIN};
			who[n++] = i;
		}
		if (poll(pf, (nfds_t)n, -1) < 0) {
			if (errno == EINTR)
				continue;
			fprintf(stderr, "vetro-files: poll: %s\n", strerror(errno));
			return 1;
		}
		for (int k = 0; k < n; k++) {
			if (!pf[k].revents)
				continue;
			if (who[k] < 0) {
				int fd = accept4(ls, NULL, NULL, SOCK_NONBLOCK | SOCK_CLOEXEC);
				if (fd < 0)
					continue;
				struct client *c = NULL;
				for (int i = 0; i < MAX_CLIENTS && !c; i++)
					if (clients[i].fd < 0)
						c = &clients[i];
				if (!c) {
					close(fd);
					continue;
				}
				/* Senza inotify nel kernel (-1) WATCH risponde con l'errore. */
				int ino = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
				memset(c, 0, sizeof(*c));
				c->fd = fd;
				c->ino = ino;
				for (int h = 0; h < MAX_HANDLES; h++)
					c->h[h].fd = -1;
				reply_hello(c);
				if (flush_out(c))
					drop_client(c);
				continue;
			}
			struct client *c = &clients[who[k]];
			if (c->fd < 0)
				continue;
			int bad = 0;
			if (pf[k].fd >= 0 && pf[k].fd == c->ino) {
				serve_events(c);
			} else {
				if (pf[k].revents & (POLLIN | POLLHUP | POLLERR))
					bad = serve_input(c);
			}
			if (!bad)
				bad = flush_out(c);
			if (bad)
				drop_client(c);
		}
	}
}
