use tokio::task;
use unsync::spsc;

#[cfg(not(miri))]
const SIZE: u32 = 100_000;

#[cfg(miri)]
const SIZE: u32 = 10;

#[tokio::test]
async fn test_spsc() -> Result<(), Box<dyn std::error::Error>> {
    let local = task::LocalSet::new();

    let (mut tx, mut rx) = spsc::channel(2);

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
                for n in 0..SIZE {
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

    let expected = (0..SIZE).collect::<Vec<_>>();

    assert_eq!(actual, expected);
    Ok(())
}

#[tokio::test]
async fn test_try_send() -> Result<(), task::JoinError> {
    let (mut tx, mut rx) = spsc::channel(3);
    assert!(tx.try_send(1).is_ok());
    assert!(tx.try_send(2).is_ok());
    assert!(tx.try_send(3).is_ok());
    assert!(tx.try_send(4).is_err());

    let first = rx.recv().await;
    assert_eq!(first, Some(1));
    assert!(tx.try_send(5).is_ok());
    assert!(tx.try_send(6).is_err());

    let mut collected = Vec::new();

    drop(tx);

    while let Some(value) = rx.recv().await {
        collected.push(value);
    }

    assert_eq!(collected, vec![2, 3, 5]);
    Ok(())
}
