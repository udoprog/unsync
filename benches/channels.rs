use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use tokio::task;

pub fn criterion_benchmark(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    for size in [16, 32, 64, 128, 256, 1024] {
        c.bench_with_input(BenchmarkId::new("unsync", size), &size, |b, size| {
            b.iter(|| rt.block_on(test_unsync(*size)).unwrap())
        });

        c.bench_with_input(BenchmarkId::new("tokio", size), &size, |b, size| {
            b.iter(|| rt.block_on(test_tokio(*size)).unwrap())
        });
    }
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);

async fn test_unsync(size: u32) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
    use unsync::{oneshot, spsc};

    let local = task::LocalSet::new();

    let (mut tx, mut rx) = spsc::channel::<oneshot::Sender<u32>>(1);

    let (a, b) = local
        .run_until(async move {
            let a = task::spawn_local(async move {
                let mut n = 0;

                while let Some(oneshot_tx) = rx.recv().await {
                    let _ = oneshot_tx.send(n);

                    if n % 7 == 0 {
                        task::yield_now().await;
                    }

                    n += 1;
                }
            });

            let b = task::spawn_local(async move {
                let mut out = Vec::new();

                for n in 0..size {
                    let (oneshot_tx, oneshot_rx) = oneshot::channel();
                    let _ = tx.send(oneshot_tx).await;

                    if n % 5 == 0 {
                        task::yield_now().await;
                    }

                    out.extend(oneshot_rx.await);

                    if n % 3 == 0 {
                        task::yield_now().await;
                    }
                }

                out
            });

            tokio::join!(a, b)
        })
        .await;

    a?;
    let actual = b?;

    let expected = (0..size).collect::<Vec<_>>();
    assert_eq!(actual, expected);

    Ok(actual)
}

async fn test_tokio(size: u32) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
    use tokio::sync::{mpsc, oneshot};

    let local = task::LocalSet::new();

    let (tx, mut rx) = mpsc::channel::<oneshot::Sender<u32>>(1);

    let (a, b) = local
        .run_until(async move {
            let a = task::spawn_local(async move {
                let mut n = 0;

                while let Some(oneshot_tx) = rx.recv().await {
                    let _ = oneshot_tx.send(n);

                    if n % 7 == 0 {
                        task::yield_now().await;
                    }

                    n += 1;
                }
            });

            let b = task::spawn_local(async move {
                let mut out = Vec::new();

                for n in 0..size {
                    let (oneshot_tx, oneshot_rx) = oneshot::channel();
                    let _ = tx.send(oneshot_tx).await;

                    if n % 5 == 0 {
                        task::yield_now().await;
                    }

                    out.extend(oneshot_rx.await);

                    if n % 3 == 0 {
                        task::yield_now().await;
                    }
                }

                out
            });

            tokio::join!(a, b)
        })
        .await;

    a?;
    let actual = b?;

    let expected = (0..size).collect::<Vec<_>>();
    assert_eq!(actual, expected);

    Ok(actual)
}
