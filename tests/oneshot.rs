use tokio::task;
use unsync::{oneshot, spsc};

const END: u32 = 1_000_000;

#[tokio::test]
async fn test_oneshot() -> Result<(), Box<dyn std::error::Error>> {
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

                for n in 0..END {
                    let (oneshot_tx, oneshot_rx) = oneshot::channel();
                    let _ = tx.send(oneshot_tx).await;

                    if n % 5 == 0 {
                        task::yield_now().await;
                    }

                    let value = oneshot_rx.await;
                    out.extend(value);

                    if n % 3 == 0 {
                        task::yield_now().await;
                    }
                }

                out
            });

            tokio::join!(a, b)
        })
        .await;

    let () = a?;
    let actual = b?;

    let expected = (0..END).collect::<Vec<_>>();

    assert_eq!(actual, expected);
    Ok(())
}
