// Carico rappresentativo di virgola mobile e SIMD (M4, JIT per Android: ART,
// bionic, Skia e SwiftShader ne fanno largo uso): conversioni, prodotti di
// matrici float e double, riduzioni, memcpy/strlen/memchr NEON, TBL, EXT,
// ADDV/UMAXV, FCMP/FCSEL, FPCR/FPSR. Ogni sezione stampa un riassunto
// esatto (bit dei risultati in esadecimale): Vetro, col JIT e senza, deve
// stampare quello che stampa QEMU (tests/linux/tests/fpsimd.rs).
//
// Uso: fpsimd [ripetizioni]  (default 1; più ripetizioni per le misure)
#include <arm_neon.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static uint64_t mix(uint64_t h, uint64_t v) {
    h ^= v + 0x9e3779b97f4a7c15ull + (h << 6) + (h >> 2);
    return h;
}

static uint64_t bits_d(double d) {
    uint64_t u;
    memcpy(&u, &d, 8);
    return u;
}

static uint32_t bits_f(float f) {
    uint32_t u;
    memcpy(&u, &f, 4);
    return u;
}

static uint64_t fpsr(void) {
    uint64_t v;
    __asm__ volatile("mrs %0, fpsr" : "=r"(v));
    return v;
}

static void set_fpsr(uint64_t v) { __asm__ volatile("msr fpsr, %0" ::"r"(v)); }

static void set_fpcr(uint64_t v) { __asm__ volatile("msr fpcr, %0" ::"r"(v)); }

// Pseudo-casuale deterministico.
static uint64_t rng_state = 0x5eed1234abcdull;
static uint64_t rnd(void) {
    rng_state ^= rng_state << 13;
    rng_state ^= rng_state >> 7;
    rng_state ^= rng_state << 17;
    return rng_state;
}

// Valori interessanti: normali, piccoli, grandi, denormali, zeri, infiniti, NaN.
static double special_d(int i) {
    static const uint64_t s[] = {
        0x0000000000000000ull, 0x8000000000000000ull, 0x3ff0000000000000ull, 0xbff0000000000000ull,
        0x7ff0000000000000ull, 0xfff0000000000000ull, 0x7ff8000000000000ull, 0x7ff4000000000001ull,
        0x0000000000000001ull, 0x000fffffffffffffull, 0x0010000000000000ull, 0x7fefffffffffffffull,
        0x3fe0000000000000ull, 0x4330000000000000ull, 0x43e0000000000000ull, 0xc3e0000000000001ull,
        0x41dfffffffc00000ull, 0x3ff8000000000000ull, 0x4004000000000000ull, 0xc00c000000000000ull,
    };
    double d;
    memcpy(&d, &s[i % (sizeof s / sizeof s[0])], 8);
    return d;
}

#define N_SPECIAL 20

// --- conversioni ------------------------------------------------------
static uint64_t conversions(int reps) {
    uint64_t h = 0;
    for (int r = 0; r < reps; r++) {
        for (int i = 0; i < 2000; i++) {
            double d = (i < N_SPECIAL) ? special_d(i) : (double)(int64_t)rnd() / (double)(1ull << (rnd() % 60));
            float f = (float)d;
            h = mix(h, bits_f(f));
            h = mix(h, bits_d((double)f));
            h = mix(h, (uint64_t)(int64_t)d);
            h = mix(h, (uint64_t)(int32_t)f);
            h = mix(h, (uint64_t)d);
            h = mix(h, (uint32_t)f);
            h = mix(h, bits_d(floor(d)));
            h = mix(h, bits_d(ceil(d)));
            h = mix(h, bits_d(trunc(d)));
            h = mix(h, bits_d(round(d)));
            h = mix(h, bits_d(rint(d)));
            h = mix(h, (uint64_t)lround(d));
            h = mix(h, bits_f(floorf(f)));
            int64_t k = (int64_t)rnd() >> (rnd() % 64);
            h = mix(h, bits_d((double)k));
            h = mix(h, bits_f((float)k));
            h = mix(h, bits_d((double)(uint64_t)k));
            h = mix(h, bits_f((float)(int32_t)k));
            h = mix(h, bits_d(sqrt(fabs(d))));
            h = mix(h, bits_f(sqrtf(fabsf(f))));
        }
    }
    return h;
}

// --- aritmetica scalare, FCMP/FCSEL, fma --------------------------------
static uint64_t scalar_arith(int reps) {
    uint64_t h = 0;
    double acc = 1.0;
    float accf = 1.0f;
    for (int r = 0; r < reps; r++) {
        for (int i = 0; i < 400; i++) {
            for (int j = 0; j < N_SPECIAL; j += 3) {
                double a = special_d(i % N_SPECIAL), b = special_d(j);
                float fa = (float)a, fb = (float)b;
                h = mix(h, bits_d(a + b));
                h = mix(h, bits_d(a - b));
                h = mix(h, bits_d(a * b));
                h = mix(h, bits_d(a / b));
                h = mix(h, bits_d(fma(a, b, acc)));
                h = mix(h, bits_f(fa + fb));
                h = mix(h, bits_f(fa * fb));
                h = mix(h, bits_f(fa / fb));
                h = mix(h, bits_f(fmaf(fa, fb, accf)));
                h = mix(h, bits_d(fmax(a, b)));
                h = mix(h, bits_d(fmin(a, b)));
                h = mix(h, (a < b) | (a == b) << 1 | (a > b) << 2 | (a != a) << 3);
                h = mix(h, bits_d(a < b ? a : b));
                h = mix(h, bits_f(fa > fb ? fa : fb));
            }
            double x = (double)(rnd() >> 11) * 0x1p-53 + 0.5;
            acc = acc * x + 0.25 / (x + 1.0);
            accf = accf * (float)x - 0.125f / ((float)x + 2.0f);
            h = mix(h, bits_d(acc));
            h = mix(h, bits_f(accf));
        }
    }
    return h;
}

// --- prodotti di matrici ---------------------------------------------
#define M 24
static uint64_t matmul(int reps) {
    static float af[M][M], bf[M][M], cf[M][M];
    static double ad[M][M], bd[M][M], cd[M][M];
    for (int i = 0; i < M; i++) {
        for (int j = 0; j < M; j++) {
            af[i][j] = (float)((int)(rnd() % 2001) - 1000) / 37.0f;
            bf[i][j] = (float)((int)(rnd() % 2001) - 1000) / 91.0f;
            ad[i][j] = (double)((int64_t)(rnd() % 200001) - 100000) / 3.0;
            bd[i][j] = (double)((int64_t)(rnd() % 200001) - 100000) / 7.0;
        }
    }
    uint64_t h = 0;
    for (int r = 0; r < reps * 4; r++) {
        for (int i = 0; i < M; i++) {
            for (int j = 0; j < M; j++) {
                float s = 0;
                double t = 0;
                for (int k = 0; k < M; k++) {
                    s += af[i][k] * bf[k][j];
                    t += ad[i][k] * bd[k][j];
                }
                cf[i][j] = s;
                cd[i][j] = t;
            }
        }
        // Versione NEON (FMLA vettoriale, elemento indicizzato).
        for (int i = 0; i < M; i++) {
            for (int j = 0; j < M; j += 4) {
                float32x4_t acc = vdupq_n_f32(0);
                for (int k = 0; k < M; k++) acc = vfmaq_n_f32(acc, vld1q_f32(&bf[k][j]), af[i][k]);
                float32x4_t prev = vld1q_f32(&cf[i][j]);
                vst1q_f32(&cf[i][j], vaddq_f32(prev, vmulq_f32(acc, vdupq_n_f32(0.5f))));
            }
            for (int j = 0; j < M; j += 2) {
                float64x2_t acc = vdupq_n_f64(0);
                for (int k = 0; k < M; k++) acc = vfmaq_laneq_f64(acc, vld1q_f64(&bd[k][j]), vdupq_n_f64(ad[i][k]), 0);
                vst1q_f64(&cd[i][j], vsubq_f64(vld1q_f64(&cd[i][j]), acc));
            }
        }
        for (int i = 0; i < M; i++) {
            for (int j = 0; j < M; j++) {
                h = mix(h, bits_f(cf[i][j]));
                h = mix(h, bits_d(cd[i][j]));
            }
        }
        // Le matrici cambiano un po' a ogni giro.
        af[r % M][(r * 7) % M] += 1.0f;
        bd[(r * 5) % M][r % M] *= -0.5;
    }
    return h;
}

// --- SIMD intero: memcpy, strlen, memchr NEON, TBL, EXT, ADDV, UMAXV ---
static void neon_memcpy(uint8_t *d, const uint8_t *s, size_t n) {
    size_t i = 0;
    for (; i + 64 <= n; i += 64) {
        uint8x16x4_t v = vld1q_u8_x4(s + i);
        vst1q_u8_x4(d + i, v);
    }
    for (; i + 16 <= n; i += 16) vst1q_u8(d + i, vld1q_u8(s + i));
    for (; i < n; i++) d[i] = s[i];
}

static size_t neon_strlen(const char *s) {
    size_t i = 0;
    // Allineato a 16: le letture non escono dalla pagina.
    while (((uintptr_t)(s + i) & 15) != 0) {
        if (s[i] == 0) return i;
        i++;
    }
    for (;;) {
        uint8x16_t v = vld1q_u8((const uint8_t *)s + i);
        uint8x16_t z = vceqq_u8(v, vdupq_n_u8(0));
        if (vmaxvq_u8(z) != 0) {
            // Primo zero: indice minimo fra le corsie a zero.
            static const uint8_t idx[16] = {0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15};
            uint8x16_t pos = vorrq_u8(vld1q_u8(idx), vmvnq_u8(z));
            return i + vminvq_u8(pos);
        }
        i += 16;
    }
}

static const uint8_t *neon_memchr(const uint8_t *s, uint8_t c, size_t n) {
    size_t i = 0;
    uint8x16_t cc = vdupq_n_u8(c);
    for (; i + 16 <= n; i += 16) {
        uint8x16_t eq = vceqq_u8(vld1q_u8(s + i), cc);
        uint64x2_t w = vreinterpretq_u64_u8(eq);
        if (vgetq_lane_u64(w, 0) | vgetq_lane_u64(w, 1)) {
            for (size_t k = 0; k < 16; k++)
                if (s[i + k] == c) return s + i + k;
        }
    }
    for (; i < n; i++)
        if (s[i] == c) return s + i;
    return 0;
}

static uint64_t simd_int(int reps) {
    enum { LEN = 8192 };
    static uint8_t a[LEN + 64], b[LEN + 64];
    static char str[LEN + 64];
    for (int i = 0; i < LEN + 64; i++) {
        a[i] = (uint8_t)rnd();
        str[i] = (char)('a' + rnd() % 26);
    }
    uint64_t h = 0;
    for (int r = 0; r < reps * 8; r++) {
        size_t off = r % 13, n = LEN - (r * 97) % 1024;
        neon_memcpy(b + (r % 7), a + off, n);
        uint32x4_t sum = vdupq_n_u32(0);
        for (size_t i = 0; i + 16 <= n; i += 16) {
            uint8x16_t v = vld1q_u8(b + (r % 7) + i);
            sum = vpadalq_u16(sum, vpaddlq_u8(v));
        }
        h = mix(h, vaddvq_u32(sum));
        str[LEN - (r * 131) % 4000] = 0;
        h = mix(h, neon_strlen(str + (r % 5)));
        h = mix(h, (uint64_t)strlen(str + (r % 3)));
        str[LEN - (r * 131) % 4000] = 'x';
        const uint8_t *p = neon_memchr(a, (uint8_t)r, LEN);
        h = mix(h, p ? (uint64_t)(p - a) : 999999);
        // TBL/TBX, EXT, ADDV, UMAXV, SMINV, CNT, REV, ZIP/UZP.
        uint8x16_t t = vld1q_u8(a + (r % 32));
        uint8x16_t ix = vandq_u8(vld1q_u8(a + 100 + (r % 16)), vdupq_n_u8(0x1f));
        uint8x16x2_t tab = {{t, vld1q_u8(a + 200)}};
        uint8x16_t tb = vqtbl2q_u8(tab, ix);
        uint8x16_t tx = vqtbx1q_u8(t, t, ix);
        uint8x16_t ex = (r & 1) ? vextq_u8(tb, tx, 3) : vextq_u8(tb, tx, 7);
        h = mix(h, vaddvq_u8(ex));
        h = mix(h, vmaxvq_u8(tb));
        h = mix(h, (uint64_t)(int64_t)vminvq_s8(vreinterpretq_s8_u8(tx)));
        h = mix(h, vaddvq_u8(vcntq_u8(ex)));
        uint8x16_t z = vzip1q_u8(tb, vrev64q_u8(tx));
        uint16x8_t w = vmull_u8(vget_low_u8(z), vget_high_u8(ex));
        h = mix(h, vaddvq_u16(w));
        int16x8_t s16 = vqaddq_s16(vreinterpretq_s16_u16(w), vdupq_n_s16(30000));
        h = mix(h, vgetq_lane_u64(vreinterpretq_u64_s16(s16), 1));
        uint32x4_t sh = vshlq_n_u32(vreinterpretq_u32_u8(ex), 3);
        h = mix(h, vgetq_lane_u32(vshrq_n_u32(vaddq_u32(sh, vreinterpretq_u32_u8(tb)), 5), 2));
        h = mix(h, vgetq_lane_u64(vreinterpretq_u64_u8(vuzp2q_u8(tb, ex)), 0));
        uint8x16_t c = vbslq_u8(vcgtq_u8(tb, tx), tb, vsubq_u8(tx, tb));
        h = mix(h, vgetq_lane_u64(vreinterpretq_u64_u8(c), 1));
    }
    return h;
}

// --- vettoriale in virgola mobile: riduzioni, conversioni, confronti ---
static uint64_t simd_fp(int reps) {
    enum { LEN = 1024 };
    static float x[LEN], y[LEN];
    static double dx[LEN];
    for (int i = 0; i < LEN; i++) {
        x[i] = (float)((int)(rnd() % 20001) - 10000) / 17.0f;
        y[i] = (float)((int)(rnd() % 20001) - 10000) / 3.0f;
        dx[i] = i < N_SPECIAL ? special_d(i) : (double)x[i] * 1.0000001;
    }
    uint64_t h = 0;
    for (int r = 0; r < reps * 8; r++) {
        float32x4_t acc = vdupq_n_f32(0), mx = vdupq_n_f32(-INFINITY);
        int32x4_t ci = vdupq_n_s32(0);
        for (int i = 0; i < LEN; i += 4) {
            float32x4_t a = vld1q_f32(x + i), b = vld1q_f32(y + i);
            acc = vmlaq_f32(acc, a, b);
            mx = vmaxq_f32(mx, vabsq_f32(vsubq_f32(a, b)));
            uint32x4_t gt = vcgtq_f32(a, b);
            ci = vaddq_s32(ci, vcvtq_s32_f32(vbslq_f32(gt, a, b)));
            float32x4_t q = vdivq_f32(a, vaddq_f32(vabsq_f32(b), vdupq_n_f32(1.0f)));
            vst1q_f32(y + i, vaddq_f32(b, vmulq_n_f32(q, 0.001f)));
        }
        h = mix(h, bits_f(vaddvq_f32(acc)));
        h = mix(h, bits_f(vmaxvq_f32(mx)));
        h = mix(h, (uint64_t)vaddvq_s32(ci));
        float64x2_t dacc = vdupq_n_f64(0);
        for (int i = 0; i < LEN; i += 2) {
            float64x2_t d = vld1q_f64(dx + i);
            dacc = vaddq_f64(dacc, vsqrtq_f64(vabsq_f64(d)));
            float32x2_t n = vcvt_f32_f64(d);
            h = mix(h, vget_lane_u64(vreinterpret_u64_f32(n), 0));
            float64x2_t w = vcvt_f64_f32(vget_low_f32(vld1q_f32(x + i)));
            dacc = vfmaq_f64(dacc, w, vdupq_n_f64(0.25));
            h = mix(h, (uint64_t)vgetq_lane_s64(vcvtq_s64_f64(d), 1));
        }
        h = mix(h, bits_d(vgetq_lane_f64(dacc, 0)) ^ bits_d(vgetq_lane_f64(dacc, 1)));
        h = mix(h, bits_f(vminnmvq_f32(vld1q_f32(x + (r % 64) * 4))));
    }
    return h;
}

// --- FPCR: arrotondamenti, FZ, DN; FPSR: flag cumulativi ---------------
static uint64_t fpcr_modes(int reps) {
    uint64_t h = 0;
    for (int r = 0; r < reps; r++) {
        for (int mode = 0; mode < 8; mode++) {
            // RMode (bit 23:22), FZ (24), DN (25).
            uint64_t fpcr = (uint64_t)(mode & 3) << 22 | (uint64_t)((mode >> 2) & 1) << 24 |
                            (uint64_t)((mode >> 2) & 1) << 25;
            set_fpsr(0);
            set_fpcr(fpcr);
            volatile double acc = 0;
            volatile float accf = 0;
            for (int i = 0; i < 300; i++) {
                double a = special_d(i), b = special_d(i * 7 + 3);
                double s = a / (b + 3.0) + a * 1e-300 * 1e-10;
                acc = acc + s;
                float fa = (float)a * 1e-30f;
                accf = accf + fa * fa;
                h = mix(h, bits_d(s));
                h = mix(h, bits_f(fa * 1e-10f));
                h = mix(h, (uint64_t)(int64_t)(s * 1e10));
            }
            h = mix(h, bits_d(acc));
            h = mix(h, bits_f(accf));
            h = mix(h, fpsr());
            set_fpcr(0);
        }
    }
    set_fpsr(0);
    return h;
}

int main(int argc, char **argv) {
    int reps = argc > 1 ? atoi(argv[1]) : 1;
    printf("conversioni %016llx\n", (unsigned long long)conversions(reps));
    printf("scalare %016llx fpsr %llx\n", (unsigned long long)scalar_arith(reps), (unsigned long long)fpsr());
    set_fpsr(0);
    printf("matrici %016llx fpsr %llx\n", (unsigned long long)matmul(reps), (unsigned long long)fpsr());
    printf("simd intero %016llx\n", (unsigned long long)simd_int(reps));
    set_fpsr(0);
    printf("simd fp %016llx fpsr %llx\n", (unsigned long long)simd_fp(reps), (unsigned long long)fpsr());
    printf("fpcr %016llx\n", (unsigned long long)fpcr_modes(reps));
    return 0;
}
