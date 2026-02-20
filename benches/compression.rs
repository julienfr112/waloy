use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use waloy::compression::{compress, decompress};
use waloy::CompressionAlgorithm;

const SIZES: &[(&str, usize)] = &[("1KB", 1024), ("64KB", 64 * 1024), ("1MB", 1024 * 1024)];

fn make_payload(size: usize) -> Vec<u8> {
    // Semi-compressible: repeating pattern with some variation
    (0..size).map(|i| (i % 251) as u8).collect()
}

fn bench_lz4(c: &mut Criterion) {
    let mut group = c.benchmark_group("lz4");
    for &(label, size) in SIZES {
        let data = make_payload(size);
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(BenchmarkId::new("compress", label), &data, |b, data| {
            b.iter(|| compress(data, &CompressionAlgorithm::Lz4).unwrap());
        });

        let compressed = compress(&data, &CompressionAlgorithm::Lz4).unwrap();
        group.bench_with_input(
            BenchmarkId::new("decompress", label),
            &compressed,
            |b, data| {
                b.iter(|| decompress(data).unwrap());
            },
        );

        group.bench_with_input(BenchmarkId::new("roundtrip", label), &data, |b, data| {
            b.iter(|| {
                let c = compress(data, &CompressionAlgorithm::Lz4).unwrap();
                decompress(&c).unwrap()
            });
        });
    }
    group.finish();
}

fn bench_zstd(c: &mut Criterion) {
    let mut group = c.benchmark_group("zstd");
    for &(label, size) in SIZES {
        let data = make_payload(size);
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(BenchmarkId::new("compress", label), &data, |b, data| {
            b.iter(|| compress(data, &CompressionAlgorithm::Zstd).unwrap());
        });

        let compressed = compress(&data, &CompressionAlgorithm::Zstd).unwrap();
        group.bench_with_input(
            BenchmarkId::new("decompress", label),
            &compressed,
            |b, data| {
                b.iter(|| decompress(data).unwrap());
            },
        );

        group.bench_with_input(BenchmarkId::new("roundtrip", label), &data, |b, data| {
            b.iter(|| {
                let c = compress(data, &CompressionAlgorithm::Zstd).unwrap();
                decompress(&c).unwrap()
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_lz4, bench_zstd);
criterion_main!(benches);
