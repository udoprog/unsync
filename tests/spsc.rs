use std::time::Instant;

use tokio::task;
use unsync::spsc;

const END: u32 = 1_000_000;

#[tokio::test]
async fn test_spsc() -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();

    let local = task::LocalSet::new();

    let (mut tx, mut rx) = spsc::channel();

    let (a, b) = local
        .run_until(async move {
            let a = task::spawn_local(async move {
                let mut out = Vec::new();

                while let Some(value) = rx.recv().await {
                    out.push(value);

                    if value % 3 == 0 {
                        task::yield_now().await;
                    }
                }

                out
            });

            let b = task::spawn_local(async move {
                for n in 0..END {
                    let _ = tx.send(n).await;

                    if n % 5 == 0 {
                        task::yield_now().await;
                    }
                }
            });

            tokio::join!(a, b)
        })
        .await;

    let actual = a?;
    let () = b?;

    let expected = (0..END).collect::<Vec<_>>();

    assert_eq!(actual, expected);
    dbg!(Instant::now().duration_since(start));
    Ok(())
}
