use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use waloy::encryption::{decrypt, encrypt};

const SIZES: &[(&str, usize)] = &[("1KB", 1024), ("64KB", 64 * 1024), ("1MB", 1024 * 1024)];
const PASSPHRASE: &str = "bench-passphrase-waloy";

fn make_payload(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 251) as u8).collect()
}

fn bench_encryption(c: &mut Criterion) {
    let mut group = c.benchmark_group("encryption");
    // Argon2 KDF is intentionally slow — reduce sample size
    group.sample_size(10);

    for &(label, size) in SIZES {
        let data = make_payload(size);
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(BenchmarkId::new("encrypt", label), &data, |b, data| {
            b.iter(|| encrypt(data, PASSPHRASE).unwrap());
        });

        let encrypted = encrypt(&data, PASSPHRASE).unwrap();
        group.bench_with_input(
            BenchmarkId::new("decrypt", label),
            &encrypted,
            |b, data| {
                b.iter(|| decrypt(data, PASSPHRASE).unwrap());
            },
        );

        group.bench_with_input(BenchmarkId::new("roundtrip", label), &data, |b, data| {
            b.iter(|| {
                let enc = encrypt(data, PASSPHRASE).unwrap();
                decrypt(&enc, PASSPHRASE).unwrap()
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_encryption);
criterion_main!(benches);
