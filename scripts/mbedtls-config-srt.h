/*
 * Minimal Mbed TLS build configuration for libsrt's AES encryption.
 *
 * libsrt is the ONLY consumer of this library in the restream build (FFmpeg is
 * compiled --disable-network; the Rust side uses rustls+ring). SRT's mbedTLS
 * CRYSPR backend (srt/haicrypt/cryspr-mbedtls.c) calls exactly:
 *
 *   AES ECB/CTR              -> MBEDTLS_AES_C + MBEDTLS_CIPHER_MODE_CTR
 *   CTR-DRBG RNG             -> MBEDTLS_CTR_DRBG_C   (requires AES_C)
 *   platform entropy source  -> MBEDTLS_ENTROPY_C    (requires SHA-256/512)
 *   message digest (HMAC)    -> MBEDTLS_MD_C
 *   PBKDF2 passphrase KDF     -> MBEDTLS_PKCS5_C + SHA-1  (SRT KDF is PBKDF2-HMAC-SHA1)
 *
 * Everything else Mbed TLS can build — TLS/DTLS, X.509, all public-key crypto
 * (RSA/ECDH/ECDSA/DHM), the PSA layer, and every other cipher and hash — is
 * left out. This file is the WHOLE configuration: it is passed via
 * -DMBEDTLS_CONFIG_FILE, which replaces (not extends) the stock config, so it
 * must be self-contained. mbedtls/build_info.h pulls in the config_adjust_*.h
 * helpers and check_config.h after this header, so do not include them here.
 *
 * Hardware acceleration: MBEDTLS_AESNI_C + MBEDTLS_HAVE_ASM enable the x86-64
 * AES-NI / CLMUL code path, selected at runtime via CPUID. This is independent
 * of the compiler's -march level (the AES-NI translation units carry their own
 * target attributes), so AES stays hardware-accelerated regardless of the
 * baseline microarchitecture the rest of the build targets.
 */
#ifndef MBEDTLS_CONFIG_H
#define MBEDTLS_CONFIG_H

/* Toolchain + hardware AES (AES-NI / CLMUL, runtime CPUID-detected). */
#define MBEDTLS_HAVE_ASM
#define MBEDTLS_AESNI_C

/* Symmetric primitives invoked by SRT's CRYSPR. */
#define MBEDTLS_AES_C
#define MBEDTLS_CIPHER_MODE_CTR
#define MBEDTLS_MD_C
#define MBEDTLS_SHA1_C
#define MBEDTLS_SHA256_C
#define MBEDTLS_PKCS5_C

/* RNG for session-key / IV material: CTR-DRBG seeded from platform entropy. */
#define MBEDTLS_ENTROPY_C
#define MBEDTLS_CTR_DRBG_C

/* Exposes mbedtls_version_get_string_full() for runtime version reporting
 * (the SBOM / status endpoint reads the actually-linked library version). */
#define MBEDTLS_VERSION_C

#endif /* MBEDTLS_CONFIG_H */
