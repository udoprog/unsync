use tokio::task;
use unsync::broadcast;

const END: u32 = 16;

#[tokio::test]
async fn test_broadcast() -> Result<(), Box<dyn std::error::Error>> {
    let local = task::LocalSet::new();

    let mut tx = broadcast::channel();

    let (receivers, b) = local
        .run_until(async move {
            let mut receivers = Vec::new();

            for _ in 0..16 {
                let mut rx = tx.subscribe();

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

                receivers.push(a);
            }

            let b = task::spawn_local(async move {
                for n in 0..END {
                    let _ = tx.send(n).await;

                    if n % 5 == 0 {
                        task::yield_now().await;
                    }
                }
            });

            let mut received = Vec::new();

            for receiver in receivers {
                received.push(receiver.await.unwrap());
            }

            (received, b.await)
        })
        .await;

    let () = b?;

    let expected = (0..END).collect::<Vec<_>>();

    for actual in receivers {
        assert_eq!(actual, expected);
    }

    Ok(())
}

#[tokio::test]
async fn test_broadcast_receiver_drop() {
    let mut tx = broadcast::channel();
    let mut sub1 = tx.subscribe();
    let mut sub2 = tx.subscribe();

    let (_, s1, s2) = tokio::join!(tx.send(1), sub1.recv(), sub2.recv());
    drop(sub2);
    assert_eq!((s1, s2), (Some(1), Some(1)));

    let (_, s1) = tokio::join!(tx.send(2), sub1.recv());
    assert_eq!(s1, Some(2));
    drop(sub1);

    let (send,) = tokio::join!(tx.send(2));
    assert!(send.is_err());
}
