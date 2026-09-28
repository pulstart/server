//! CPU conversion of a scanout readback into encoder input: packed RGB
//! (8-bit, 10-bit, FP16) to BT.709 limited-range NV12 or BGRA8, split into
//! row bands across a small realtime worker pool.

use crossbeam_channel::{bounded, Receiver, Sender};
use std::thread::JoinHandle;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SrcFormat {
    /// XRGB8888/ARGB8888: bytes B, G, R, X.
    Bgrx8,
    /// XBGR8888/ABGR8888: bytes R, G, B, X.
    Rgbx8,
    /// XRGB2101010/ARGB2101010.
    Xrgb10,
    /// XBGR2101010/ABGR2101010.
    Xbgr10,
    /// XBGR16161616F/ABGR16161616F: half floats R, G, B, A, display-referred
    /// 0..1 like the GL path samples them.
    Rgba16f,
}

impl SrcFormat {
    pub fn from_drm(fourcc: u32) -> Option<Self> {
        match &fourcc.to_le_bytes() {
            b"XR24" | b"AR24" => Some(Self::Bgrx8),
            b"XB24" | b"AB24" => Some(Self::Rgbx8),
            b"XR30" | b"AR30" => Some(Self::Xrgb10),
            b"XB30" | b"AB30" => Some(Self::Xbgr10),
            b"XB4H" | b"AB4H" => Some(Self::Rgba16f),
            _ => None,
        }
    }

    pub fn bytes_per_pixel(self) -> usize {
        if self == Self::Rgba16f {
            8
        } else {
            4
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DstFormat {
    Nv12,
    Bgra,
}

impl DstFormat {
    pub fn len(self, width: usize, height: usize) -> usize {
        match self {
            Self::Nv12 => width * height * 3 / 2,
            Self::Bgra => width * height * 4,
        }
    }
}

const LUMA: [f32; 3] = [
    0.2126 * 219.0 / 255.0,
    0.7152 * 219.0 / 255.0,
    0.0722 * 219.0 / 255.0,
];
// Applied to the sum of a 2x2 block, hence the extra quarter.
const CB: [f32; 3] = [
    -0.114572 * 224.0 / 255.0 / 4.0,
    -0.385428 * 224.0 / 255.0 / 4.0,
    0.5 * 224.0 / 255.0 / 4.0,
];
const CR: [f32; 3] = [
    0.5 * 224.0 / 255.0 / 4.0,
    -0.454153 * 224.0 / 255.0 / 4.0,
    -0.045847 * 224.0 / 255.0 / 4.0,
];
const TEN_BIT: f32 = 255.0 / 1023.0;

/// FP16 → 0..255. Exact for normals and subnormals; negatives map to 0 and
/// inf/NaN to large finite values the clamp caps.
#[inline(always)]
fn half_to_255(h: u64) -> f32 {
    let h = h as u32;
    let v = f32::from_bits((h & 0x7fff) << 13) * f32::from_bits(0x7780_0000) * 255.0;
    if h & 0x8000 != 0 {
        0.0
    } else {
        v.min(255.0)
    }
}

#[derive(Default)]
struct Scratch {
    rgb: [Vec<f32>; 6],
}

impl Scratch {
    fn rows(&mut self, width: usize) -> &mut [Vec<f32>; 6] {
        if self.rgb[0].len() != width {
            for row in &mut self.rgb {
                *row = vec![0.0; width];
            }
        }
        &mut self.rgb
    }
}

/// Planar 0..255 floats for `r.len()` pixels of a packed row.
///
/// # Safety
/// `src` must point at `r.len()` pixels, 8-byte aligned for FP16 and 4-byte
/// aligned otherwise.
#[inline(always)]
unsafe fn decode_row(fmt: SrcFormat, src: *const u8, r: &mut [f32], g: &mut [f32], b: &mut [f32]) {
    let n = r.len();
    let (g, b) = (&mut g[..n], &mut b[..n]);
    if fmt == SrcFormat::Rgba16f {
        let px = std::slice::from_raw_parts(src as *const u64, n);
        for i in 0..n {
            let p = px[i];
            r[i] = half_to_255(p);
            g[i] = half_to_255(p >> 16);
            b[i] = half_to_255(p >> 32);
        }
        return;
    }
    let px = std::slice::from_raw_parts(src as *const u32, n);
    let (mask, scale, shifts) = match fmt {
        SrcFormat::Bgrx8 => (0xff, 1.0, [16, 8, 0]),
        SrcFormat::Rgbx8 => (0xff, 1.0, [0, 8, 16]),
        SrcFormat::Xrgb10 => (0x3ff, TEN_BIT, [20, 10, 0]),
        SrcFormat::Xbgr10 => (0x3ff, TEN_BIT, [0, 10, 20]),
        SrcFormat::Rgba16f => unreachable!(),
    };
    for i in 0..n {
        let p = px[i];
        r[i] = ((p >> shifts[0]) & mask) as f32 * scale;
        g[i] = ((p >> shifts[1]) & mask) as f32 * scale;
        b[i] = ((p >> shifts[2]) & mask) as f32 * scale;
    }
}

/// NV12 for pixels `x..width` of one row pair (scalar; the SIMD path covers
/// the rest).
#[inline(always)]
unsafe fn nv12_pair_scalar(job: &Job, scratch: &mut Scratch, pair: usize, x: usize) {
    let (w, h) = (job.width, job.height);
    let n = w - x;
    let bpp = job.src_format.bytes_per_pixel();
    let [r0, g0, b0, r1, g1, b1] = scratch.rows(w);
    let (r0, g0, b0) = (&mut r0[..n], &mut g0[..n], &mut b0[..n]);
    let (r1, g1, b1) = (&mut r1[..n], &mut g1[..n], &mut b1[..n]);
    decode_row(
        job.src_format,
        job.src_row(2 * pair).add(x * bpp),
        r0,
        g0,
        b0,
    );
    decode_row(
        job.src_format,
        job.src_row(2 * pair + 1).add(x * bpp),
        r1,
        g1,
        b1,
    );
    for (row, (r, g, b)) in [
        (2 * pair, (&*r0, &*g0, &*b0)),
        (2 * pair + 1, (&*r1, &*g1, &*b1)),
    ] {
        let y = std::slice::from_raw_parts_mut(job.out.add(row * w + x), n);
        for i in 0..n {
            y[i] = (16.5 + LUMA[0] * r[i] + LUMA[1] * g[i] + LUMA[2] * b[i]) as u8;
        }
    }
    let uv = std::slice::from_raw_parts_mut(job.out.add(w * h + pair * w + x), n);
    for j in 0..n / 2 {
        let (a, c) = (2 * j, 2 * j + 1);
        let r = r0[a] + r0[c] + r1[a] + r1[c];
        let g = g0[a] + g0[c] + g1[a] + g1[c];
        let b = b0[a] + b0[c] + b1[a] + b1[c];
        uv[a] = (128.5 + CB[0] * r + CB[1] * g + CB[2] * b) as u8;
        uv[c] = (128.5 + CR[0] * r + CR[1] * g + CR[2] * b) as u8;
    }
}

#[inline(always)]
unsafe fn bgra_row_scalar(job: &Job, scratch: &mut Scratch, y: usize, x: usize) {
    let n = job.width - x;
    let [r, g, b, ..] = scratch.rows(job.width);
    let (r, g, b) = (&mut r[..n], &mut g[..n], &mut b[..n]);
    let bpp = job.src_format.bytes_per_pixel();
    decode_row(job.src_format, job.src_row(y).add(x * bpp), r, g, b);
    let out = std::slice::from_raw_parts_mut(job.out.add((y * job.width + x) * 4), n * 4);
    for (i, px) in out.chunks_exact_mut(4).enumerate() {
        px[0] = (b[i] + 0.5) as u8;
        px[1] = (g[i] + 0.5) as u8;
        px[2] = (r[i] + 0.5) as u8;
        px[3] = 255;
    }
}

#[cfg(target_arch = "x86_64")]
mod avx2 {
    use super::{SrcFormat, CB, CR, LUMA, TEN_BIT};
    use std::arch::x86_64::*;

    pub fn available() -> bool {
        is_x86_feature_detected!("avx2")
            && is_x86_feature_detected!("fma")
            && is_x86_feature_detected!("f16c")
    }

    #[inline]
    #[target_feature(enable = "avx2,fma,f16c")]
    unsafe fn unit_to_255(v: __m256, order: __m256i) -> __m256 {
        // min() maps NaN to 255 (second operand wins).
        let max = _mm256_set1_ps(255.0);
        let v = _mm256_mul_ps(_mm256_permutevar8x32_ps(v, order), max);
        _mm256_max_ps(_mm256_min_ps(v, max), _mm256_setzero_ps())
    }

    #[inline]
    #[target_feature(enable = "avx2,fma,f16c")]
    unsafe fn channel(p: __m256i, shift: i32, mask: __m256i, scale: __m256) -> __m256 {
        let v = _mm256_and_si256(_mm256_srlv_epi32(p, _mm256_set1_epi32(shift)), mask);
        _mm256_mul_ps(_mm256_cvtepi32_ps(v), scale)
    }

    /// Eight pixels as planar 0..255 floats in pixel order.
    #[inline]
    #[target_feature(enable = "avx2,fma,f16c")]
    unsafe fn load8(fmt: SrcFormat, src: *const u8) -> [__m256; 3] {
        if fmt == SrcFormat::Rgba16f {
            // Four 2-pixel RGBA vectors transpose to [0 2 4 6 | 1 3 5 7].
            let v0 = _mm256_cvtph_ps(_mm_loadu_si128(src as *const __m128i));
            let v1 = _mm256_cvtph_ps(_mm_loadu_si128(src.add(16) as *const __m128i));
            let v2 = _mm256_cvtph_ps(_mm_loadu_si128(src.add(32) as *const __m128i));
            let v3 = _mm256_cvtph_ps(_mm_loadu_si128(src.add(48) as *const __m128i));
            let (t0, t1) = (_mm256_unpacklo_ps(v0, v1), _mm256_unpackhi_ps(v0, v1));
            let (t2, t3) = (_mm256_unpacklo_ps(v2, v3), _mm256_unpackhi_ps(v2, v3));
            let order = _mm256_setr_epi32(0, 4, 1, 5, 2, 6, 3, 7);
            return [
                unit_to_255(_mm256_shuffle_ps::<0x44>(t0, t2), order),
                unit_to_255(_mm256_shuffle_ps::<0xEE>(t0, t2), order),
                unit_to_255(_mm256_shuffle_ps::<0x44>(t1, t3), order),
            ];
        }
        let p = _mm256_loadu_si256(src as *const __m256i);
        let (mask, scale, [r, g, b]) = match fmt {
            SrcFormat::Bgrx8 => (0xff, 1.0, [16, 8, 0]),
            SrcFormat::Rgbx8 => (0xff, 1.0, [0, 8, 16]),
            SrcFormat::Xrgb10 => (0x3ff, TEN_BIT, [20, 10, 0]),
            SrcFormat::Xbgr10 => (0x3ff, TEN_BIT, [0, 10, 20]),
            SrcFormat::Rgba16f => unreachable!(),
        };
        let (mask, scale) = (_mm256_set1_epi32(mask), _mm256_set1_ps(scale));
        [
            channel(p, r, mask, scale),
            channel(p, g, mask, scale),
            channel(p, b, mask, scale),
        ]
    }

    #[inline]
    #[target_feature(enable = "avx2,fma,f16c")]
    unsafe fn dot(rgb: [__m256; 3], k: [f32; 3], bias: f32) -> __m256i {
        let acc = _mm256_fmadd_ps(rgb[2], _mm256_set1_ps(k[2]), _mm256_set1_ps(bias));
        let acc = _mm256_fmadd_ps(rgb[1], _mm256_set1_ps(k[1]), acc);
        _mm256_cvtps_epi32(_mm256_fmadd_ps(rgb[0], _mm256_set1_ps(k[0]), acc))
    }

    /// Sixteen i32 (0..255) → sixteen bytes, `a` then `b`.
    #[inline]
    #[target_feature(enable = "avx2,fma,f16c")]
    unsafe fn pack16(a: __m256i, b: __m256i) -> __m128i {
        let words = _mm256_permute4x64_epi64::<0xD8>(_mm256_packus_epi32(a, b));
        let bytes = _mm256_packus_epi16(words, words);
        _mm256_castsi256_si128(_mm256_permute4x64_epi64::<0x08>(bytes))
    }

    /// Horizontal pair sums of the row-pair sums, in pixel order.
    #[inline]
    #[target_feature(enable = "avx2,fma,f16c")]
    unsafe fn quad_sums(a0: __m256, a1: __m256, b0: __m256, b1: __m256) -> __m256 {
        let sums = _mm256_hadd_ps(_mm256_add_ps(a0, a1), _mm256_add_ps(b0, b1));
        _mm256_permutevar8x32_ps(sums, _mm256_setr_epi32(0, 1, 4, 5, 2, 3, 6, 7))
    }

    /// NV12 for a row pair, 16 pixels at a time. Returns pixels done.
    #[target_feature(enable = "avx2,fma,f16c")]
    pub unsafe fn nv12_pair(
        fmt: SrcFormat,
        src0: *const u8,
        src1: *const u8,
        y0: *mut u8,
        y1: *mut u8,
        uv: *mut u8,
        width: usize,
    ) -> usize {
        let bpp = fmt.bytes_per_pixel();
        let done = width & !15;
        for x in (0..done).step_by(16) {
            let a0 = load8(fmt, src0.add(x * bpp));
            let b0 = load8(fmt, src0.add((x + 8) * bpp));
            let a1 = load8(fmt, src1.add(x * bpp));
            let b1 = load8(fmt, src1.add((x + 8) * bpp));
            let luma0 = pack16(dot(a0, LUMA, 16.0), dot(b0, LUMA, 16.0));
            _mm_storeu_si128(y0.add(x) as *mut __m128i, luma0);
            let luma1 = pack16(dot(a1, LUMA, 16.0), dot(b1, LUMA, 16.0));
            _mm_storeu_si128(y1.add(x) as *mut __m128i, luma1);
            let sums = [
                quad_sums(a0[0], a1[0], b0[0], b1[0]),
                quad_sums(a0[1], a1[1], b0[1], b1[1]),
                quad_sums(a0[2], a1[2], b0[2], b1[2]),
            ];
            let (cb, cr) = (dot(sums, CB, 128.0), dot(sums, CR, 128.0));
            let (lo, hi) = (_mm256_unpacklo_epi32(cb, cr), _mm256_unpackhi_epi32(cb, cr));
            let chroma = pack16(
                _mm256_permute2x128_si256::<0x20>(lo, hi),
                _mm256_permute2x128_si256::<0x31>(lo, hi),
            );
            _mm_storeu_si128(uv.add(x) as *mut __m128i, chroma);
        }
        done
    }

    /// BGRA8 for one row, 8 pixels at a time. Returns pixels done.
    #[target_feature(enable = "avx2,fma,f16c")]
    pub unsafe fn bgra_row(fmt: SrcFormat, src: *const u8, out: *mut u8, width: usize) -> usize {
        let bpp = fmt.bytes_per_pixel();
        let done = width & !7;
        let half = _mm256_set1_ps(0.5);
        let alpha = _mm256_set1_epi32(0xff00_0000u32 as i32);
        for x in (0..done).step_by(8) {
            let [r, g, b] = load8(fmt, src.add(x * bpp));
            let r = _mm256_slli_epi32::<16>(_mm256_cvttps_epi32(_mm256_add_ps(r, half)));
            let g = _mm256_slli_epi32::<8>(_mm256_cvttps_epi32(_mm256_add_ps(g, half)));
            let b = _mm256_cvttps_epi32(_mm256_add_ps(b, half));
            let px = _mm256_or_si256(_mm256_or_si256(b, g), _mm256_or_si256(r, alpha));
            _mm256_storeu_si256(out.add(x * 4) as *mut __m256i, px);
        }
        done
    }
}

#[derive(Clone, Copy)]
struct Job {
    src_format: SrcFormat,
    dst_format: DstFormat,
    src: *const u8,
    src_stride: usize,
    out: *mut u8,
    width: usize,
    height: usize,
    simd: bool,
}

// SAFETY: `Converter::convert` keeps both buffers alive and unaliased until
// every band it dispatched has reported back.
unsafe impl Send for Job {}

impl Job {
    unsafe fn src_row(&self, y: usize) -> *const u8 {
        self.src.add(y * self.src_stride)
    }
}

/// Converts units `[start, end)` of `job`.
unsafe fn convert_units(job: &Job, scratch: &mut Scratch, start: usize, end: usize) {
    let (w, h) = (job.width, job.height);
    for unit in start..end {
        let mut x = 0;
        match job.dst_format {
            DstFormat::Nv12 => {
                #[cfg(target_arch = "x86_64")]
                if job.simd {
                    x = avx2::nv12_pair(
                        job.src_format,
                        job.src_row(2 * unit),
                        job.src_row(2 * unit + 1),
                        job.out.add(2 * unit * w),
                        job.out.add((2 * unit + 1) * w),
                        job.out.add(w * h + unit * w),
                        w,
                    );
                }
                if x < w {
                    nv12_pair_scalar(job, scratch, unit, x);
                }
            }
            DstFormat::Bgra if job.src_format == SrcFormat::Bgrx8 => {
                std::ptr::copy_nonoverlapping(job.src_row(unit), job.out.add(unit * w * 4), w * 4);
            }
            DstFormat::Bgra => {
                #[cfg(target_arch = "x86_64")]
                if job.simd {
                    x = avx2::bgra_row(
                        job.src_format,
                        job.src_row(unit),
                        job.out.add(unit * w * 4),
                        w,
                    );
                }
                if x < w {
                    bgra_row_scalar(job, scratch, unit, x);
                }
            }
        }
    }
}

struct Worker {
    tx: Sender<(Job, usize, usize)>,
    handle: JoinHandle<()>,
}

pub struct Converter {
    workers: Vec<Worker>,
    done: Receiver<()>,
    scratch: Scratch,
    simd: bool,
}

impl Converter {
    /// `threads` bands per frame, the calling thread converting one of them.
    pub fn new(threads: usize) -> Self {
        let (done_tx, done) = bounded(threads.max(1));
        let workers = (1..threads.max(1))
            .map(|i| {
                let (tx, rx) = bounded::<(Job, usize, usize)>(1);
                let done_tx = done_tx.clone();
                let handle = std::thread::Builder::new()
                    .name(format!("st-csc-{i}"))
                    .spawn(move || {
                        st_protocol::thread_priority::promote_current_thread(
                            st_protocol::thread_priority::ThreadRole::Capture,
                        );
                        let mut scratch = Scratch::default();
                        while let Ok((job, start, end)) = rx.recv() {
                            unsafe { convert_units(&job, &mut scratch, start, end) };
                            if done_tx.send(()).is_err() {
                                break;
                            }
                        }
                    })
                    .expect("spawn csc worker");
                Worker { tx, handle }
            })
            .collect();
        Self {
            workers,
            done,
            scratch: Scratch::default(),
            #[cfg(target_arch = "x86_64")]
            simd: avx2::available() && std::env::var_os("ST_CSC_SCALAR").is_none(),
            #[cfg(not(target_arch = "x86_64"))]
            simd: false,
        }
    }

    /// Bands per frame for this machine: enough to hide the conversion behind
    /// the copy without taking many cores from the game.
    pub fn default_threads() -> usize {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        (cores / 4).clamp(1, 4)
    }

    /// Convert `height` rows of `src` (row pitch `src_stride`, 8-byte aligned
    /// rows) into `out`, sized by `DstFormat::len`. NV12 needs even
    /// dimensions.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn convert(
        &mut self,
        src_format: SrcFormat,
        dst_format: DstFormat,
        src: &[u8],
        src_stride: usize,
        out: &mut [u8],
        width: usize,
        height: usize,
    ) {
        self.convert_rows(
            src_format,
            dst_format,
            src,
            src_stride,
            out,
            width,
            height,
            0..height,
        );
    }

    /// `convert` restricted to source `rows` (even bounds for NV12), so a
    /// band can be converted while the next one is still being copied.
    #[allow(clippy::too_many_arguments)]
    pub fn convert_rows(
        &mut self,
        src_format: SrcFormat,
        dst_format: DstFormat,
        src: &[u8],
        src_stride: usize,
        out: &mut [u8],
        width: usize,
        height: usize,
        rows: std::ops::Range<usize>,
    ) {
        assert!(src_stride >= width * src_format.bytes_per_pixel());
        assert!(src.len() >= src_stride * (height - 1) + width * src_format.bytes_per_pixel());
        assert!((src.as_ptr() as usize).is_multiple_of(8) && src_stride.is_multiple_of(8));
        assert_eq!(out.len(), dst_format.len(width, height));
        assert!(rows.start <= rows.end && rows.end <= height);
        let units = match dst_format {
            DstFormat::Nv12 => {
                assert!(width.is_multiple_of(2) && height.is_multiple_of(2));
                assert!(rows.start.is_multiple_of(2) && rows.end.is_multiple_of(2));
                rows.start / 2..rows.end / 2
            }
            DstFormat::Bgra => rows,
        };
        let job = Job {
            src_format,
            dst_format,
            src: src.as_ptr(),
            src_stride,
            out: out.as_mut_ptr(),
            width,
            height,
            simd: self.simd,
        };
        let chunk = units.len().div_ceil(self.workers.len() + 1);
        let mut sent = 0;
        for (i, worker) in self.workers.iter().enumerate() {
            let start = units.start + (i + 1) * chunk;
            let end = (start + chunk).min(units.end);
            if start < end && worker.tx.send((job, start, end)).is_ok() {
                sent += 1;
            }
        }
        let own_end = (units.start + chunk).min(units.end);
        unsafe { convert_units(&job, &mut self.scratch, units.start, own_end) };
        for _ in 0..sent {
            let _ = self.done.recv();
        }
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        for worker in self.workers.drain(..) {
            drop(worker.tx);
            let _ = worker.handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED_709: [u8; 3] = [63, 102, 240];
    const BLUE_709: [u8; 3] = [32, 240, 118];

    fn half(v: f32) -> u16 {
        // Test values are exact in FP16.
        let bits = v.to_bits();
        if v == 0.0 {
            return 0;
        }
        let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
        ((exp as u16) << 10) | ((bits >> 13) & 0x3ff) as u16
    }

    /// Red top half, blue bottom half, in each source format.
    fn source(fmt: SrcFormat, w: usize, h: usize) -> Vec<u64> {
        let px = |red: bool| -> u64 {
            let (r, b) = if red { (1.0, 0.0) } else { (0.0, 1.0) };
            match fmt {
                SrcFormat::Bgrx8 => ((r as u64 * 255) << 16) | (b as u64 * 255),
                SrcFormat::Rgbx8 => (r as u64 * 255) | ((b as u64 * 255) << 16),
                SrcFormat::Xrgb10 => ((r as u64 * 1023) << 20) | (b as u64 * 1023),
                SrcFormat::Xbgr10 => (r as u64 * 1023) | ((b as u64 * 1023) << 20),
                SrcFormat::Rgba16f => {
                    half(r) as u64 | ((half(b) as u64) << 32) | ((half(1.0) as u64) << 48)
                }
            }
        };
        let bpp = fmt.bytes_per_pixel();
        let mut bytes = vec![0u8; w * h * bpp];
        for y in 0..h {
            for x in 0..w {
                let v = px(y < h / 2).to_le_bytes();
                bytes[(y * w + x) * bpp..(y * w + x + 1) * bpp].copy_from_slice(&v[..bpp]);
            }
        }
        bytes
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    fn as_bytes(v: &[u64]) -> &[u8] {
        unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 8) }
    }

    fn near(a: u8, b: u8) -> bool {
        (a as i32 - b as i32).abs() <= 1
    }

    #[test]
    fn every_format_converts_to_bt709_nv12_and_bgra_upright() {
        let (w, h) = (64usize, 32usize);
        for threads in [1, 3] {
            let mut conv = Converter::new(threads);
            for fmt in [
                SrcFormat::Bgrx8,
                SrcFormat::Rgbx8,
                SrcFormat::Xrgb10,
                SrcFormat::Xbgr10,
                SrcFormat::Rgba16f,
            ] {
                let src = source(fmt, w, h);
                let stride = w * fmt.bytes_per_pixel();
                let mut nv12 = vec![0u8; DstFormat::Nv12.len(w, h)];
                conv.convert(
                    fmt,
                    DstFormat::Nv12,
                    as_bytes(&src),
                    stride,
                    &mut nv12,
                    w,
                    h,
                );
                let (top, bottom) = (nv12[0], nv12[w * h - 1]);
                assert!(
                    near(top, RED_709[0]) && near(bottom, BLUE_709[0]),
                    "{fmt:?} Y"
                );
                let uv_top = &nv12[w * h..w * h + 2];
                let uv_bottom = &nv12[nv12.len() - 2..];
                assert!(
                    near(uv_top[0], RED_709[1]) && near(uv_top[1], RED_709[2]),
                    "{fmt:?} top CbCr {uv_top:?}"
                );
                assert!(
                    near(uv_bottom[0], BLUE_709[1]) && near(uv_bottom[1], BLUE_709[2]),
                    "{fmt:?} bottom CbCr {uv_bottom:?}"
                );
                let mut bgra = vec![0u8; DstFormat::Bgra.len(w, h)];
                conv.convert(
                    fmt,
                    DstFormat::Bgra,
                    as_bytes(&src),
                    stride,
                    &mut bgra,
                    w,
                    h,
                );
                assert_eq!(&bgra[..3], &[0, 0, 255], "{fmt:?} top must be red");
                assert_eq!(
                    &bgra[bgra.len() - 4..bgra.len() - 1],
                    &[255, 0, 0],
                    "{fmt:?}"
                );
            }
        }
    }

    #[test]
    fn half_decode_covers_edge_cases() {
        assert_eq!(half_to_255(0), 0.0);
        assert_eq!(half_to_255(half(1.0) as u64), 255.0);
        assert_eq!(half_to_255(half(0.5) as u64), 127.5);
        assert_eq!(half_to_255(0xbc00), 0.0, "negative clamps to 0");
        assert_eq!(half_to_255(0x7c00), 255.0, "inf clamps");
        assert_eq!(half_to_255(0x7e00), 255.0, "NaN clamps");
        assert!(half_to_255(0x0001) > 0.0 && half_to_255(0x0001) < 0.001);
    }

    /// `cargo test --release csc_bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn csc_bench() {
        let (w, h) = (2560usize, 1440usize);
        for fmt in [SrcFormat::Rgba16f, SrcFormat::Bgrx8] {
            let src = source(fmt, w, h);
            let stride = w * fmt.bytes_per_pixel();
            let mut out = vec![0u8; DstFormat::Nv12.len(w, h)];
            for threads in [1, 2, 3, 4, 6] {
                let mut conv = Converter::new(threads);
                let mut times = Vec::new();
                for _ in 0..200 {
                    let t = std::time::Instant::now();
                    conv.convert(fmt, DstFormat::Nv12, as_bytes(&src), stride, &mut out, w, h);
                    times.push(t.elapsed());
                }
                times.sort();
                eprintln!(
                    "[csc] {fmt:?} {w}x{h} threads={threads} p50={:.2?} p95={:.2?}",
                    times[100], times[190]
                );
            }
        }
    }
}
