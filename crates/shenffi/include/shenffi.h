/* shenffi.h — C ABI for embedding shen-rust (Shen language) in a host app. */
#ifndef SHENFFI_H
#define SHENFFI_H

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque handle to a booted Shen image. */
typedef struct ShenCtx ShenCtx;

/* Boot a Shen kernel. Pass NULL for kernel_dir to auto-locate the vendored
 * kernel, or an absolute path to a directory of .kl files. Returns NULL on
 * failure. Free with shen_free(). */
ShenCtx *shen_boot(const char *kernel_dir);

/* Boot the full kernel from sources embedded in the binary — no filesystem
 * access. Recommended on iOS. Returns NULL on failure; free with shen_free(). */
ShenCtx *shen_boot_embedded(void);

/* Evaluate a Shen expression; returns a heap-allocated C string (the rendered
 * result, or "error: <message>"). Release it with shen_string_free(). */
char *shen_eval(ShenCtx *ctx, const char *src);

/* Boot any Ratatoskr-shaken program from a shaken kernel.kl slice + optional
 * program .kl (pass NULL prog_kl for kernel-only). The program is loaded with
 * *hush* set, so its load-time stdout chatter is suppressed. Free with
 * shen_free(). */
ShenCtx *shen_boot_shaken(const char *kernel_kl, const char *prog_kl);

/* --- shen-cas: embedded, tree-shaken computer algebra system --- */

/* Boot the embedded shen-cas slice. Free with shen_free(). */
ShenCtx *shen_cas_boot(void);

/* Parse + reduce + pretty-print a CAS expression, e.g. "D[Sin[x],x]" -> "[Cos x]".
 * Returns a heap string; release with shen_string_free(). */
char *shen_cas_reduce(ShenCtx *ctx, const char *src);

/* Free a string returned by shen_eval() / shen_cas_reduce(). */
void shen_string_free(char *s);

/* Free a handle returned by shen_boot(). */
void shen_free(ShenCtx *ctx);

#ifdef __cplusplus
}
#endif

#endif /* SHENFFI_H */
