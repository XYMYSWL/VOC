//! 编译 RNNoise 0.2（xiph/rnnoise v0.2，BSD-3-Clause，源码和模型在 third_party/rnnoise-0.2，第 68 项）。
//! 只编通用 C 版本（不编 x86 目录里按 CPU 挑 AVX2 / SSE4.1 的那套）：一路 48 kHz 麦克风用不着。
fn main() {
    let dir = std::path::Path::new("third_party/rnnoise-0.2");
    let files = [
        "celt_lpc.c",
        "denoise.c",
        "kiss_fft.c",
        "nnet.c",
        "nnet_default.c",
        "parse_lpcnet_weights.c",
        "pitch.c",
        "rnn.c",
        "rnnoise_data.c",
        "rnnoise_tables.c",
    ];
    let mut b = cc::Build::new();
    b.include(dir.join("src")).include(dir.join("include")).define("RNNOISE_EXPORT", "").warnings(false);
    // MSVC 不定义 __SSE2__（gcc 在 x86_64 上默认有）：不定义的话走通用版本，而 0.2 的发布包里缺通用版本要的 os_support.h。
    // 64 位 x86 都有 SSE2，按 gcc 的默认走 SSE2 版本
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64") {
        b.define("__SSE2__", None);
    }
    for f in files {
        let p = dir.join("src").join(f);
        println!("cargo:rerun-if-changed={}", p.display());
        b.file(p);
    }
    b.compile("rnnoise2");
}
